use std::collections::HashMap;

use lending_health::health::{
    classify, render_report, validate_pubkey, Config, Position, Protocol, Risk, REPORT_CHAR_CAP,
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
    let cfg = Config::from_section(&base_section()).expect("valid section must parse");
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
    let cfg = Config::from_section(&s).expect("kamino-only section must parse");
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
        deposit_usd: 1000.0,
        borrow_usd: 400.0,
        ltv,
        liquidation_ltv: 0.85,
        stale_hint: None,
    }
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
fn empty_positions_render_calm_message() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let report = render_report(&[], &cfg);
    assert!(report.contains("No open lending positions"));
}
