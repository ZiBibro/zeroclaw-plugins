use std::collections::HashMap;

use lending_health::health::{
    classify, classify_position, liquidation_buffer, render_payload, render_report,
    render_total_failure, short_account, validate_pubkey, Config, Liquidation, Position, Protocol,
    Risk, REPORT_CHAR_CAP,
};

const WALLET_A: &str = "86xCnPeV69n6t3DnyGvkKobf9FdN2H9oiVDdaMpo2MMY";
const WALLET_B: &str = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";

fn section(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn base_section() -> HashMap<String, String> {
    section(&[
        ("wallets", &format!("main:{WALLET_A}")[..]),
        ("rpc_url", "https://example-rpc.test"),
    ])
}

#[test]
fn config_parses_minimal_valid_section() {
    let cfg = Config::from_section(&base_section()).expect("test config");
    assert_eq!(cfg.wallets.len(), 1);
    assert_eq!(cfg.wallets[0].label, "main");
    assert_eq!(cfg.wallets[0].pubkey, WALLET_A);
    assert_eq!(cfg.protocols, vec![Protocol::Kamino, Protocol::Marginfi]);
    assert!(cfg.warn_liquidation_buffer > cfg.critical_liquidation_buffer);
}

#[test]
fn config_rejects_unknown_key() {
    let mut s = base_section();
    s.insert("warm_ltv".to_string(), "0.5".to_string());
    let err = Config::from_section(&s).unwrap_err();
    assert!(err.contains("unknown config key `warm_ltv`"), "err: {err}");
}

#[test]
fn config_rejects_misspelled_threshold_key() {
    let mut s = base_section();
    s.insert("warn_ltw".to_string(), "0.5".to_string());
    assert!(Config::from_section(&s).is_err());
}

#[test]
fn config_requires_wallets() {
    let s = section(&[("rpc_url", "https://example-rpc.test")]);
    let err = Config::from_section(&s).unwrap_err();
    assert!(err.contains("`wallets` is required"), "err: {err}");
}

#[test]
fn config_rejects_invalid_pubkey() {
    let s = section(&[
        ("wallets", "main:not-a-pubkey"),
        ("rpc_url", "https://example-rpc.test"),
    ]);
    assert!(Config::from_section(&s).is_err());
}

#[test]
fn config_rejects_duplicate_labels() {
    let s = section(&[
        ("wallets", &format!("main:{WALLET_A},main:{WALLET_B}")[..]),
        ("rpc_url", "https://example-rpc.test"),
    ]);
    let err = Config::from_section(&s).unwrap_err();
    assert!(err.contains("duplicate wallet label"), "err: {err}");
}

#[test]
fn config_requires_rpc_url_when_marginfi_enabled() {
    let s = section(&[("wallets", &format!("main:{WALLET_A}")[..])]);
    let err = Config::from_section(&s).unwrap_err();
    assert!(err.contains("rpc_url"), "err: {err}");
}

#[test]
fn config_allows_kamino_only_without_rpc_url() {
    let s = section(&[
        ("wallets", &format!("main:{WALLET_A}")[..]),
        ("protocols", "kamino"),
    ]);
    let cfg = Config::from_section(&s).expect("kamino-only config");
    assert_eq!(cfg.protocols, vec![Protocol::Kamino]);
    assert!(cfg.rpc_url.is_none());
}

#[test]
fn config_rejects_http_rpc_url() {
    let s = section(&[
        ("wallets", &format!("main:{WALLET_A}")[..]),
        ("rpc_url", "http://insecure.test"),
    ]);
    let err = Config::from_section(&s).unwrap_err();
    assert!(err.contains("https://"), "err: {err}");
}

#[test]
fn config_rejects_unknown_protocol() {
    let mut s = base_section();
    s.insert("protocols".to_string(), "kamino,drift".to_string());
    let err = Config::from_section(&s).unwrap_err();
    assert!(err.contains("unknown protocol `drift`"), "err: {err}");
}

#[test]
fn config_rejects_inverted_thresholds() {
    let mut s = base_section();
    // Inverted on the buffer basis: a warning at 0.05 would fire later than a
    // critical at 0.20, which is the wrong way round.
    s.insert("warn_liquidation_buffer".to_string(), "0.05".to_string());
    s.insert(
        "critical_liquidation_buffer".to_string(),
        "0.20".to_string(),
    );
    let err = Config::from_section(&s).unwrap_err();
    assert!(err.contains("must be above"), "err: {err}");
}

#[test]
fn config_rejects_out_of_range_ratio() {
    let mut s = base_section();
    s.insert("warn_liquidation_buffer".to_string(), "1.5".to_string());
    assert!(Config::from_section(&s).is_err());
}

#[test]
fn resolve_wallet_rejects_non_allowlisted() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let err = cfg.resolve_wallet(Some(WALLET_B)).unwrap_err();
    assert!(
        err.contains("not in the configured allowlist"),
        "err: {err}"
    );
}

#[test]
fn resolve_wallet_finds_by_label_and_pubkey() {
    let cfg = Config::from_section(&base_section()).unwrap();
    assert_eq!(cfg.resolve_wallet(Some("main")).unwrap().len(), 1);
    assert_eq!(cfg.resolve_wallet(Some(WALLET_A)).unwrap().len(), 1);
    assert_eq!(cfg.resolve_wallet(None).unwrap().len(), 1);
}

#[test]
fn pubkey_validation_rejects_wrong_length() {
    assert!(validate_pubkey("abc", "test key").is_err());
    assert!(validate_pubkey(WALLET_A, "test key").is_ok());
}

fn liq(ltv: f64, liquidation_ltv: f64) -> Liquidation {
    Liquidation {
        ltv,
        liquidation_ltv,
    }
}

#[test]
fn classify_measures_the_liquidation_buffer_kamino_documents() {
    let cfg = Config::from_section(&base_section()).unwrap();
    // Defaults flag at a 0.15 buffer and escalate at 0.05.
    // (0.80 - 0.40) / 0.80 = 0.50 of the collateral value may still be lost.
    assert_eq!(classify(liq(0.40, 0.80), &cfg), Risk::Ok);
    // (0.80 - 0.70) / 0.80 = 0.125 left, inside the warning band.
    assert_eq!(classify(liq(0.70, 0.80), &cfg), Risk::Warn);
    // (0.80 - 0.78) / 0.80 = 0.025 left.
    assert_eq!(classify(liq(0.78, 0.80), &cfg), Risk::Critical);
}

/// The worked example from Kamino's own documentation: 70% current LTV against
/// an 80% liquidation LTV tolerates a 12.5% decline in collateral value. Pinning
/// it here keeps our arithmetic tied to the protocol's published formula rather
/// than to an in-house invention.
#[test]
fn the_buffer_matches_the_documented_kamino_example() {
    let buffer = liquidation_buffer(liq(0.70, 0.80)).expect("measurable");
    assert!((buffer - 0.125).abs() < 1e-9, "buffer: {buffer}");
}

/// The defect this replaces: classification read two flat config numbers and
/// ignored the `liquidation_ltv` sitting in the same struct, so the same LTV got
/// the same verdict in markets whose lines are 12 points apart.
#[test]
fn the_same_ltv_is_judged_against_each_market_own_line() {
    let cfg = Config::from_section(&base_section()).unwrap();
    // 82% with a 95% line leaves a 13.7% buffer: worth a flag, not an alarm.
    assert_eq!(classify(liq(0.82, 0.95), &cfg), Risk::Warn);
    // The same 82% against an 83% line leaves 1.2%.
    assert_eq!(classify(liq(0.82, 0.83), &cfg), Risk::Critical);
}

/// A position the protocol can already seize has no buffer left to spend.
#[test]
fn a_position_at_or_past_its_line_is_critical() {
    let cfg = Config::from_section(&base_section()).unwrap();
    assert_eq!(classify(liq(0.90, 0.90), &cfg), Risk::Critical);
    assert_eq!(classify(liq(0.95, 0.90), &cfg), Risk::Critical);
    let past = liquidation_buffer(liq(0.95, 0.90)).expect("measurable");
    assert!(past < 0.0, "buffer past the line must be negative: {past}");
}

/// A line of zero, or a ratio that is not a number, measures nothing. Reporting
/// either as OK would clear a position on arithmetic that never happened, and
/// reporting it as CRITICAL would condemn one on the same absence.
#[test]
fn an_unmeasurable_basis_stays_unknown() {
    let cfg = Config::from_section(&base_section()).unwrap();
    assert_eq!(classify(liq(0.50, 0.0), &cfg), Risk::Unknown);
    assert_eq!(classify(liq(0.50, -0.10), &cfg), Risk::Unknown);
    assert_eq!(classify(liq(f64::NAN, 0.80), &cfg), Risk::Unknown);
    assert_eq!(classify(liq(f64::INFINITY, 0.80), &cfg), Risk::Unknown);
}

fn position(label: &str, market: &str, ltv: f64) -> Position {
    Position {
        wallet_label: label.to_string(),
        protocol: Protocol::Kamino,
        market: market.to_string(),
        account: "6FJt..SSLy".to_string(),
        deposit_usd: 1000.0,
        borrow_usd: 400.0,
        liquidation: Some(Liquidation {
            ltv,
            liquidation_ltv: 0.85,
        }),
        flagged_unhealthy: false,
        stale_hint: None,
    }
}

/// A position the protocol itself condemned, with no basis left to measure a
/// distance on: the shape MarginFi returns for a zeroed maintenance pair.
fn condemned(label: &str, market: &str) -> Position {
    Position {
        liquidation: None,
        flagged_unhealthy: true,
        stale_hint: Some("maint basis unavailable; flagged unhealthy".to_string()),
        ..position(label, market, 0.0)
    }
}

#[test]
fn short_account_keeps_head_and_tail() {
    assert_eq!(short_account(WALLET_A), "86xC..2MMY");
    assert_eq!(short_account(WALLET_B), "9WzD..AWWM");
    assert_eq!(short_account("?"), "?");
}

#[test]
fn classify_position_marks_missing_basis_unknown() {
    let cfg = Config::from_section(&base_section()).unwrap();
    // 0.75 against the helper's 0.85 line is 88% of the distance, so it warns.
    let measured = position("main", "usdc", 0.75);
    assert_eq!(classify_position(&measured, &cfg), Risk::Warn);
    let mut blind = measured.clone();
    blind.liquidation = None;
    assert_eq!(classify_position(&blind, &cfg), Risk::Unknown);
}

#[test]
fn classify_position_keeps_a_condemned_account_critical_without_a_basis() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let p = condemned("main", "acct");
    assert!(p.liquidation.is_none());
    assert_eq!(classify_position(&p, &cfg), Risk::Critical);
}

#[test]
fn condemned_position_leads_the_report_and_survives_the_cap() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let mut positions = vec![condemned("main", "condemned")];
    positions.extend((0..60).map(|i| position("main", &format!("market-{i:02}"), 0.70)));
    let report = render_report(&positions, &cfg);

    assert!(report.starts_with("Lending health: 61 position(s), worst risk CRITICAL."));
    let first_line = report.lines().nth(1).expect("data line");
    assert!(
        first_line.starts_with("[CRITICAL] main"),
        "line: {first_line}"
    );
    assert!(first_line.contains("condemned"), "line: {first_line}");
    assert!(first_line.contains("LTV n/a"), "line: {first_line}");
    // The cap drops warnings from the tail; the condemned line is never among
    // the casualties, and no number is invented to keep it there.
    assert!(report.contains("omitted"), "report: {report}");
    assert!(report.len() <= REPORT_CHAR_CAP);
}

#[test]
fn condemned_position_outranks_a_measured_critical_below_the_line() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let positions = vec![
        position("main", "burning", 0.90),
        condemned("main", "condemned"),
        position("main", "past-the-line", 1.20),
    ];
    let report = render_report(&positions, &cfg);
    let past = report.find("past-the-line").unwrap();
    let cond = report.find("condemned").unwrap();
    let burning = report.find("burning").unwrap();
    assert!(past < cond, "report: {report}");
    assert!(cond < burning, "report: {report}");
}

#[test]
fn report_echoes_the_obligation_identity() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let mut p = position("main", "Vanilla@47tf", 0.5);
    p.account = "HcrU..iS4J".to_string();
    let report = render_report(&[p], &cfg);
    assert!(
        report.contains("Vanilla@47tf #HcrU..iS4J:"),
        "report: {report}"
    );
}

#[test]
fn report_states_no_distance_without_a_basis() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let mut p = position("main", "acct", 0.0);
    p.liquidation = None;
    p.stale_hint = Some("maint basis unavailable".to_string());
    let report = render_report(&[p], &cfg);
    assert!(report.contains("[UNKNOWN]"), "report: {report}");
    assert!(
        report.contains("LTV n/a (maint basis unavailable)"),
        "report: {report}"
    );
    assert!(!report.contains("liq"), "no line may be stated: {report}");
    // The values that survive the missing basis are still reported.
    assert!(
        report.contains("deposit $1000, borrow $400"),
        "report: {report}"
    );
}

#[test]
fn unknown_basis_outranks_calm_but_not_measured_risk() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let mut blind = position("main", "blind", 0.0);
    blind.liquidation = None;
    let positions = vec![
        position("main", "calm", 0.10),
        blind,
        position("main", "warm", 0.75),
    ];
    let report = render_report(&positions, &cfg);
    let warm = report.find("warm").unwrap();
    let unmeasured = report.find("blind").unwrap();
    let calm = report.find("calm").unwrap();
    assert!(warm < unmeasured, "report: {report}");
    assert!(unmeasured < calm, "report: {report}");
    assert!(report.starts_with("Lending health: 3 position(s), worst risk WARN."));
}

#[test]
fn report_orders_worst_risk_first() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let positions = vec![
        position("main", "calm", 0.10),
        position("main", "burning", 0.82),
        position("main", "warm", 0.70),
    ];
    let report = render_report(&positions, &cfg);
    let burning = report.find("burning").unwrap();
    let warm = report.find("warm").unwrap();
    let calm = report.find("calm").unwrap();
    assert!(burning < warm, "report: {report}");
    assert!(warm < calm, "report: {report}");
    assert!(report.starts_with("Lending health: 3 position(s), worst risk CRITICAL."));
}

#[test]
fn report_mentions_stale_data() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let mut p = position("main", "usdc", 0.5);
    p.stale_hint = Some("stale 18 h".to_string());
    let report = render_report(&[p], &cfg);
    assert!(report.contains("(stale 18 h)"), "report: {report}");
}

#[test]
fn report_stays_under_char_cap() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let positions: Vec<Position> = (0..60)
        .map(|i| position("main", &format!("market-{i:02}"), 0.5))
        .collect();
    let report = render_report(&positions, &cfg);
    assert!(
        report.len() <= REPORT_CHAR_CAP,
        "report length {} exceeds cap {}",
        report.len(),
        REPORT_CHAR_CAP
    );
    assert!(report.contains("omitted"), "report: {report}");
}

#[test]
fn delivered_payload_stays_under_the_cap_with_data_issues() {
    // A report long enough to truncate on its own, plus a run of failures long
    // enough to overrun the cap if it were appended after the truncation.
    let cfg = Config::from_section(&base_section()).unwrap();
    let positions: Vec<Position> = (0..60)
        .map(|i| position("main", &format!("market-{i:02}"), 0.5))
        .collect();
    let issues: Vec<String> = (0..12)
        .map(|i| format!("marginfi wallet-{i:02}: HTTP 500"))
        .collect();
    let payload = render_payload(&positions, &issues, &cfg);
    assert!(
        payload.len() <= REPORT_CHAR_CAP,
        "payload length {} exceeds cap {}",
        payload.len(),
        REPORT_CHAR_CAP
    );
    assert!(payload.contains("omitted"), "payload: {payload}");
    assert!(
        payload.contains("\nData issues: marginfi wallet-00: HTTP 500"),
        "payload: {payload}"
    );
    // The trimmed tail of the failure list is accounted for, never silent.
    assert!(payload.contains("more)"), "payload: {payload}");
}

#[test]
fn payload_without_data_issues_is_the_report_alone() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let positions = vec![position("main", "usdc", 0.5)];
    assert_eq!(
        render_payload(&positions, &[], &cfg),
        render_report(&positions, &cfg)
    );
}

#[test]
fn a_single_oversized_data_issue_collapses_to_a_count() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let issues = vec!["e".repeat(REPORT_CHAR_CAP * 2)];
    let payload = render_payload(&[position("main", "usdc", 0.5)], &issues, &cfg);
    assert!(
        payload.len() <= REPORT_CHAR_CAP,
        "payload length {} exceeds cap {}",
        payload.len(),
        REPORT_CHAR_CAP
    );
    assert!(
        payload.contains("\nData issues: 1 source call(s) failed"),
        "payload: {payload}"
    );
}

#[test]
fn empty_positions_render_calm_message() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let report = render_report(&[], &cfg);
    assert!(report.contains("No open lending positions"));
}

#[test]
fn total_failure_text_stays_inside_the_issue_budget() {
    // Every source failed and each upstream message is long and server-controlled.
    let issues: Vec<String> = (0..12)
        .map(|i| format!("kamino wallet-{i}: rpc error {}", "x".repeat(120)))
        .collect();
    let text = render_total_failure(&issues);

    assert!(text.starts_with("every data source failed: "));
    // The failure path is bounded by the same budget as the delivered report,
    // so a pile of long RPC errors cannot flood the agent context.
    assert!(
        text.len() <= REPORT_CHAR_CAP,
        "failure text {} chars, cap {REPORT_CHAR_CAP}",
        text.len()
    );
    // Whatever the budget pushed out is counted rather than dropped silently.
    assert!(
        text.contains("more"),
        "dropped issues are not counted: {text}"
    );
}

#[test]
fn total_failure_text_states_a_single_short_issue_in_full() {
    let issues = vec!["kamino main: http 503".to_string()];
    assert_eq!(
        render_total_failure(&issues),
        "every data source failed: kamino main: http 503"
    );
}

/// The two protocols measure LTV on different bases and share one column, while
/// the dollar amounts beside them are unweighted in both cases. A MarginFi line
/// showing $5,000 deposit, $10,000 borrow and 75% invites the operator to divide
/// and conclude the tool is broken. The percentage is right on its own basis, so
/// the line names the basis.
#[test]
fn a_marginfi_ltv_says_it_is_maintenance_weighted() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let mfi = Position {
        wallet_label: "main".to_string(),
        protocol: Protocol::Marginfi,
        market: "acct".to_string(),
        account: "AbCd..WxYz".to_string(),
        deposit_usd: 5_000.0,
        borrow_usd: 10_000.0,
        liquidation: Some(Liquidation {
            ltv: 0.75,
            liquidation_ltv: 1.0,
        }),
        flagged_unhealthy: false,
        stale_hint: None,
    };
    let report = render_report(&[mfi], &cfg);
    assert!(report.contains("maint LTV 75.0%"), "report: {report}");

    // Kamino publishes a protocol LTV directly, so its line carries no prefix
    // and stays as short as it was.
    let kam = Position {
        wallet_label: "main".to_string(),
        protocol: Protocol::Kamino,
        market: "main".to_string(),
        account: "AbCd..WxYz".to_string(),
        deposit_usd: 10_000.0,
        borrow_usd: 6_630.0,
        liquidation: Some(Liquidation {
            ltv: 0.663,
            liquidation_ltv: 0.799,
        }),
        flagged_unhealthy: false,
        stale_hint: None,
    };
    let report = render_report(&[kam], &cfg);
    assert!(
        report.contains("LTV 66.3% of 79.9% liq"),
        "report: {report}"
    );
    assert!(!report.contains("maint"), "report: {report}");
}

/// Found on a live wallet during the demo rehearsal, 2026-07-28: a deposit-only
/// Kamino position rendered as `[UNKNOWN] ... LTV 0.0% of 0.0% liq`.
///
/// Kamino reports both LTV and liquidation LTV as zero for a position carrying
/// no debt, because no liquidation line exists to report. Reading that zero as
/// an unmeasurable basis labelled the safest possible position UNKNOWN, and the
/// rendered ratio read as a broken measurement. Liquidation triggers on debt
/// over collateral; with no debt there is nothing to liquidate.
#[test]
fn a_position_with_no_debt_is_ok_not_unknown() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let deposit_only = Position {
        wallet_label: "hedge".to_string(),
        protocol: Protocol::Kamino,
        market: "Vanilla@6WEG".to_string(),
        account: "Cz3p..NQqK".to_string(),
        deposit_usd: 843.0,
        borrow_usd: 0.0,
        liquidation: Some(Liquidation {
            ltv: 0.0,
            liquidation_ltv: 0.0,
        }),
        flagged_unhealthy: false,
        stale_hint: Some("positions stale 1003 h".to_string()),
    };
    assert_eq!(classify_position(&deposit_only, &cfg), Risk::Ok);

    let report = render_report(&[deposit_only], &cfg);
    assert!(report.contains("no debt"), "report: {report}");
    assert!(!report.contains("0.0% of 0.0%"), "report: {report}");
    assert!(!report.contains("UNKNOWN"), "report: {report}");
}

/// The protocol's own unhealthy flag still outranks the no-debt shortcut: if
/// the risk engine condemned the account, believe it rather than the arithmetic.
#[test]
fn a_condemned_account_stays_critical_even_with_no_debt() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let condemned = Position {
        wallet_label: "hedge".to_string(),
        protocol: Protocol::Marginfi,
        market: "acct".to_string(),
        account: "EN1W..K7ND".to_string(),
        deposit_usd: 100.0,
        borrow_usd: 0.0,
        liquidation: None,
        flagged_unhealthy: true,
        stale_hint: None,
    };
    assert_eq!(classify_position(&condemned, &cfg), Risk::Critical);
}
