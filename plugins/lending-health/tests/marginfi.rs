use base64::Engine;
use lending_health::health::Protocol;
use lending_health::marginfi::{
    decode_account, gpa_request_body, parse_gpa_response, ACCOUNT_DISCRIMINATOR_B58,
    MARGINFI_PROGRAM,
};

const GPA_RESPONSE: &str = include_str!("fixtures/marginfi_gpa_response.json");

#[test]
fn request_body_carries_all_three_filters() {
    let body = gpa_request_body("86xCnPeV69n6t3DnyGvkKobf9FdN2H9oiVDdaMpo2MMY");
    assert!(body.contains(MARGINFI_PROGRAM));
    assert!(body.contains("\"dataSize\":2312"));
    assert!(body.contains(ACCOUNT_DISCRIMINATOR_B58));
    assert!(body.contains("86xCnPeV69n6t3DnyGvkKobf9FdN2H9oiVDdaMpo2MMY"));
    assert!(body.contains("\"offset\":40"));
}

#[test]
fn live_fixture_decodes_to_one_position() {
    let positions = parse_gpa_response(GPA_RESPONSE, "main").expect("live fixture must parse");
    assert_eq!(positions.len(), 1);
    let p = &positions[0];
    assert_eq!(p.protocol, Protocol::Marginfi);
    assert_eq!(p.market, "acct@EN1W");
    // Golden values decoded independently during Gate A pass 2 from the same
    // live account: init-weight asset 859.59 USD, liability 667.79 USD.
    assert!(
        (p.deposit_usd - 859.59).abs() < 0.05,
        "got {}",
        p.deposit_usd
    );
    assert!((p.borrow_usd - 667.79).abs() < 0.05, "got {}", p.borrow_usd);
    // The account was flagged HEALTHY, so the ratio must sit below 1.
    assert!(p.ltv > 0.5 && p.ltv < 1.0, "got ltv {}", p.ltv);
    assert!((p.liquidation_ltv - 1.0).abs() < 1e-9);
    // Live flags were 3: HEALTHY and ENGINE_OK set, ORACLE_OK unset, so the
    // report must carry an oracle hint.
    assert!(
        p.stale_hint.as_deref().unwrap_or("").contains("oracle"),
        "hint: {:?}",
        p.stale_hint
    );
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
fn unhealthy_flag_forces_critical_ratio() {
    // Take the live account bytes and clear the HEALTHY bit at offset 1944.
    let root: serde_json::Value = serde_json::from_str(GPA_RESPONSE).unwrap();
    let b64 = root["result"][0]["account"]["data"][0].as_str().unwrap();
    let mut data = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    data[1944] &= !1u8;
    let p = decode_account(&data, "pubkey", "main").unwrap();
    assert!(
        p.ltv >= 1.0,
        "unhealthy account must render CRITICAL, ltv {}",
        p.ltv
    );
}
