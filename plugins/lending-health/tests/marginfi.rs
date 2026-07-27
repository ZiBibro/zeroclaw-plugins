use std::collections::HashMap;

use base64::Engine;
use lending_health::health::{classify_position, render_report, Config, Protocol, Risk};
use lending_health::marginfi::{
    decode_account, gpa_request_body, parse_gpa_response, ACCOUNT_DISCRIMINATOR_B58,
    MARGINFI_PROGRAM,
};

/// The authority encoded at offset 40 in both fixtures below, so the filter
/// this test builds is the one that would have returned that capture.
const AUTHORITY: &str = "Dq7wypbedtaqQK9QqEFvfrxc4ppfRGXCeTVd7ee7n2jw";

const GPA_RESPONSE: &str = include_str!("fixtures/marginfi_gpa_response.json");

/// Hand-built from the live capture to reach the maintenance-weighted path the
/// capture itself cannot: same account layout, health cache rewritten to
/// init-weight 1000/700 USD, maintenance pair 800/600 USD, flag word 7 so no
/// status bit is left unset. Not a mainnet capture.
const GPA_MAINT_SYNTHETIC: &str = include_str!("fixtures/marginfi_gpa_maint_synthetic.json");

/// Byte offset of the health-cache flag word inside a decoded account.
const OFFSET_FLAGS: usize = 1944;

fn account_bytes(fixture: &str) -> Vec<u8> {
    let root: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let b64 = root["result"][0]["account"]["data"][0].as_str().unwrap();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap()
}

fn config() -> Config {
    let section: HashMap<String, String> = [
        ("wallets".to_string(), format!("main:{AUTHORITY}")),
        (
            "rpc_url".to_string(),
            "https://example-rpc.test".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    Config::from_section(&section).expect("test config")
}

#[test]
fn request_body_carries_all_three_filters() {
    let body = gpa_request_body(AUTHORITY);
    assert!(body.contains(MARGINFI_PROGRAM));
    assert!(body.contains("\"dataSize\":2312"));
    assert!(body.contains(ACCOUNT_DISCRIMINATOR_B58));
    assert!(body.contains(AUTHORITY));
    assert!(body.contains("\"offset\":40"));
}

#[test]
fn live_fixture_decodes_to_one_position() {
    let positions = parse_gpa_response(GPA_RESPONSE, "main").expect("live fixture");
    assert_eq!(positions.len(), 1);
    let p = &positions[0];
    assert_eq!(p.protocol, Protocol::Marginfi);
    assert_eq!(p.market, "acct");
    // Golden values decoded independently from the same base64 blob in
    // fixtures/marginfi_gpa_response.json: init-weight asset 859.59 USD,
    // liability 667.79 USD.
    assert!(
        (p.deposit_usd - 859.59).abs() < 0.05,
        "got {}",
        p.deposit_usd
    );
    assert!((p.borrow_usd - 667.79).abs() < 0.05, "got {}", p.borrow_usd);
}

#[test]
fn live_fixture_echoes_the_account_it_read() {
    let p = &parse_gpa_response(GPA_RESPONSE, "main").unwrap()[0];
    // Account EN1WSBJmZR1NVdYvPbpwzPnRk7JhbNncS1kNEXqvK7ND, shortened.
    assert_eq!(p.account, "EN1W..K7ND");
}

#[test]
fn zeroed_maintenance_states_no_liquidation_distance() {
    // The live capture carries a zeroed maintenance pair with the oracle bit
    // unset. The initial-weight pair sits on another basis against another
    // line, so no liquidation distance can honestly be stated for it.
    let p = &parse_gpa_response(GPA_RESPONSE, "main").unwrap()[0];
    assert!(p.liquidation.is_none(), "liquidation: {:?}", p.liquidation);
    assert_eq!(p.stale_hint.as_deref(), Some("maint basis unavailable"));
}

#[test]
fn zeroed_maintenance_never_reaches_the_report_as_a_ratio() {
    let positions = parse_gpa_response(GPA_RESPONSE, "main").unwrap();
    let report = render_report(&positions, &config());
    // 667.79 / 859.59 is the init-weight ratio, 77.7%, that must not appear.
    assert!(!report.contains("77.7"), "report: {report}");
    assert!(!report.contains("liq"), "report: {report}");
    assert!(
        report.contains("[UNKNOWN] main marginfi acct #EN1W..K7ND"),
        "report: {report}"
    );
    assert!(
        report.contains("LTV n/a (maint basis unavailable)"),
        "report: {report}"
    );
}

#[test]
fn synthetic_maintenance_fixture_reports_a_distance() {
    let positions = parse_gpa_response(GPA_MAINT_SYNTHETIC, "main").expect("synthetic fixture");
    assert_eq!(positions.len(), 1);
    let p = &positions[0];
    assert_eq!(p.account, "8mmH..WD56");
    assert!(
        (p.deposit_usd - 1000.0).abs() < 1e-6,
        "got {}",
        p.deposit_usd
    );
    assert!((p.borrow_usd - 700.0).abs() < 1e-6, "got {}", p.borrow_usd);
    let liq = p.liquidation.expect("liquidation basis");
    assert!((liq.ltv - 0.75).abs() < 1e-9, "got {}", liq.ltv);
    assert!((liq.liquidation_ltv - 1.0).abs() < 1e-9);
    assert!(p.stale_hint.is_none(), "hint: {:?}", p.stale_hint);
}

#[test]
fn synthetic_maintenance_fixture_renders_a_measured_line() {
    let positions = parse_gpa_response(GPA_MAINT_SYNTHETIC, "main").unwrap();
    let report = render_report(&positions, &config());
    assert!(
        report.contains("[WARN] main marginfi acct #8mmH..WD56: deposit $1000, borrow $700, LTV 75.0% of 100.0% liq"),
        "report: {report}"
    );
}

#[test]
fn maintenance_data_with_the_oracle_flag_unset_keeps_the_distance_and_hints() {
    let mut data = account_bytes(GPA_MAINT_SYNTHETIC);
    data[OFFSET_FLAGS] &= !4u8;
    let p = decode_account(&data, "pubkey", "main").unwrap();
    assert!(p.liquidation.is_some());
    assert_eq!(p.stale_hint.as_deref(), Some("oracle flag unset"));
}

#[test]
fn rpc_error_body_is_an_error() {
    let body = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"invalid params"},"id":1}"#;
    let err = parse_gpa_response(body, "main").unwrap_err();
    assert!(err.contains("invalid params"), "err: {err}");
}

#[test]
fn empty_result_yields_no_positions() {
    let body = r#"{"jsonrpc":"2.0","result":[],"id":1}"#;
    let positions = parse_gpa_response(body, "main").unwrap();
    assert!(positions.is_empty());
}

#[test]
fn zeroed_account_is_skipped() {
    let zeroed = vec![0u8; 2312];
    assert!(decode_account(&zeroed, "pubkey", "main").is_none());
}

#[test]
fn short_account_is_skipped() {
    let short = vec![0u8; 64];
    assert!(decode_account(&short, "pubkey", "main").is_none());
}

#[test]
fn unhealthy_flag_keeps_the_measured_ratio_and_condemns_the_line() {
    // Clear the HEALTHY bit on the maintenance-weighted account, leaving the
    // engine bit set. The printed distance stays the one the maintenance pair
    // measures, 600/800, and the verdict rides the marker beside it.
    let mut data = account_bytes(GPA_MAINT_SYNTHETIC);
    data[OFFSET_FLAGS] &= !1u8;
    let p = decode_account(&data, "pubkey", "main").unwrap();
    let liq = p.liquidation.expect("liquidation basis");
    assert!((liq.ltv - 0.75).abs() < 1e-9, "got {}", liq.ltv);
    assert!(p.flagged_unhealthy);
    assert_eq!(p.stale_hint.as_deref(), Some("flagged unhealthy"));
    assert_eq!(classify_position(&p, &config()), Risk::Critical);

    let report = render_report(&[p], &config());
    assert!(
        report.contains(
            "[CRITICAL] main marginfi acct #pubkey: \
             deposit $1000, borrow $700, LTV 75.0% of 100.0% liq (flagged unhealthy)"
        ),
        "report: {report}"
    );
}

#[test]
fn a_health_cache_the_engine_never_wrote_condemns_nothing() {
    // Flag word all zeros while the account still carries values: the engine
    // has not written this cache, so its clear HEALTHY bit is the absence of
    // a verdict and cannot be rendered as one.
    let mut data = account_bytes(GPA_MAINT_SYNTHETIC);
    data[OFFSET_FLAGS..OFFSET_FLAGS + 4].fill(0);
    let p = decode_account(&data, "pubkey", "main").unwrap();
    assert!(!p.flagged_unhealthy);
    assert!(p.liquidation.is_none(), "liquidation: {:?}", p.liquidation);
    assert_eq!(p.stale_hint.as_deref(), Some("engine status unset"));
    assert_eq!(classify_position(&p, &config()), Risk::Unknown);

    let report = render_report(&[p], &config());
    assert!(!report.contains("CRITICAL"), "report: {report}");
    assert!(
        report.contains("[UNKNOWN] main marginfi acct #pubkey: deposit $1000, borrow $700, LTV n/a (engine status unset)"),
        "report: {report}"
    );
}

#[test]
fn an_engine_written_cache_with_the_healthy_bit_clear_is_critical() {
    // ENGINE_STATUS_OK alone: the engine ran, cleared HEALTHY, and left the
    // oracle bit unset. That verdict counts, and both markers are stated.
    let mut data = account_bytes(GPA_MAINT_SYNTHETIC);
    data[OFFSET_FLAGS] = 2;
    let p = decode_account(&data, "pubkey", "main").unwrap();
    assert!(p.flagged_unhealthy);
    assert_eq!(
        p.stale_hint.as_deref(),
        Some("oracle flag unset; flagged unhealthy")
    );
    assert_eq!(classify_position(&p, &config()), Risk::Critical);
}

#[test]
fn unhealthy_flag_without_maintenance_data_states_no_distance() {
    // Clearing HEALTHY on the zeroed-maintenance capture must not conjure a
    // ratio, and must not soften the verdict either: the program condemned the
    // account, so the line reads CRITICAL with no distance on it.
    let mut data = account_bytes(GPA_RESPONSE);
    data[OFFSET_FLAGS] &= !1u8;
    let p = decode_account(&data, "pubkey", "main").unwrap();
    assert!(p.liquidation.is_none(), "liquidation: {:?}", p.liquidation);
    assert!(p.flagged_unhealthy);
    assert_eq!(classify_position(&p, &config()), Risk::Critical);
    assert_eq!(
        p.stale_hint.as_deref(),
        Some("maint basis unavailable; flagged unhealthy")
    );
}

#[test]
fn condemned_account_without_a_basis_outranks_every_measured_line() {
    let mut data = account_bytes(GPA_RESPONSE);
    data[OFFSET_FLAGS] &= !1u8;
    let condemned = decode_account(&data, "CondemnedAccountPubkey11111", "main").unwrap();

    // Enough measured warnings to overrun the cap on their own, so a report
    // that ranked the condemned line below them would truncate it away.
    let measured = parse_gpa_response(GPA_MAINT_SYNTHETIC, "main")
        .unwrap()
        .remove(0);
    let mut positions = vec![condemned];
    positions.extend((0..41).map(|_| measured.clone()));

    let report = render_report(&positions, &config());
    let first_line = report.lines().nth(1).expect("data line");
    assert!(
        first_line.starts_with("[CRITICAL] main marginfi acct #Cond..1111:"),
        "report: {report}"
    );
    assert!(first_line.ends_with("LTV n/a (maint basis unavailable; flagged unhealthy)"));
    assert!(report.starts_with("Lending health: 42 position(s), worst risk CRITICAL."));
    assert!(report.contains("omitted"), "report: {report}");
}
