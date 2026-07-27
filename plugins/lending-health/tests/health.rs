use std::collections::HashMap;

use lending_health::health::{
    classify, classify_position, render_payload, render_report, render_total_failure,
    short_account, validate_pubkey, Config, Liquidation, Position, Protocol, Risk, REPORT_CHAR_CAP,
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
    assert!(cfg.warn_ltv < cfg.critical_ltv);
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
    s.insert("warn_ltv".to_string(), "0.9".to_string());
    s.insert("critical_ltv".to_string(), "0.8".to_string());
    assert!(Config::from_section(&s).is_err());
}

#[test]
fn config_rejects_out_of_range_ratio() {
    let mut s = base_section();
    s.insert("warn_ltv".to_string(), "1.5".to_string());
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
    assert!(validate_pubkey("abc").is_err());
    assert!(validate_pubkey(WALLET_A).is_ok());
}

#[test]
fn classify_uses_configured_thresholds() {
    let cfg = Config::from_section(&base_section()).unwrap();
    assert_eq!(classify(0.10, &cfg), Risk::Ok);
    assert_eq!(classify(0.65, &cfg), Risk::Warn);
    assert_eq!(classify(0.80, &cfg), Risk::Critical);
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
    let measured = position("main", "usdc", 0.70);
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
        position("main", "warm", 0.70),
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
