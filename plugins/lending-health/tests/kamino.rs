use lending_health::health::Protocol;
use lending_health::kamino::{iso_to_epoch, parse_portfolio, portfolio_url};

const ACTIVE: &str = include_str!("fixtures/kamino_portfolio_active.json");
const EMPTY: &str = include_str!("fixtures/kamino_portfolio_empty.json");

#[test]
fn url_is_built_from_base_and_wallet() {
    assert_eq!(
        portfolio_url("https://api.kamino.finance", "abc"),
        "https://api.kamino.finance/portfolio/abc"
    );
}

#[test]
fn active_fixture_yields_lending_and_multiply_positions() {
    let positions = parse_portfolio(ACTIVE, "main").expect("live fixture");
    assert_eq!(positions.len(), 3, "2 lending + 1 multiply obligations");
    assert!(positions.iter().all(|p| p.protocol == Protocol::Kamino));
    assert!(positions.iter().all(|p| p.wallet_label == "main"));

    let vanilla = &positions[0];
    assert_eq!(vanilla.market, "Vanilla@47tf");
    assert!((vanilla.deposit_usd - 200_638.24).abs() < 0.01);
    assert!((vanilla.borrow_usd - 125_169.05).abs() < 0.01);
    let vanilla_liq = vanilla.liquidation.expect("liquidation basis");
    assert!((vanilla_liq.ltv - 0.623854).abs() < 1e-4);
    assert!((vanilla_liq.liquidation_ltv - 0.75).abs() < 1e-9);

    let tight = positions[1].liquidation.expect("liquidation basis");
    assert!((tight.ltv - 0.753300).abs() < 1e-4);
    assert!((tight.liquidation_ltv - 0.799089).abs() < 1e-4);

    let multiply = &positions[2];
    assert_eq!(multiply.market, "Multiply@47tf");
    let multiply_liq = multiply.liquidation.expect("liquidation basis");
    assert!((multiply_liq.ltv - 0.654767).abs() < 1e-4);
}

#[test]
fn active_fixture_echoes_each_obligation_address() {
    let positions = parse_portfolio(ACTIVE, "main").unwrap();
    // Shortened heads and tails of the obligation addresses in the capture.
    assert_eq!(positions[0].account, "6FJt..SSLy");
    assert_eq!(positions[1].account, "HcrU..iS4J");
    assert_eq!(positions[2].account, "FWjx..Vq67");
}

#[test]
fn row_without_an_obligation_address_reports_an_unknown_identity() {
    let body = r#"{"lending":[{"market":"47tfyEG9SsdEnUm9cw5kY9BXngQGqu3LBoop9j5uTAv8",
        "tag":"Vanilla","totalDepositValue":"100","totalBorrowValue":"50",
        "ltv":"0.5","liquidationLtv":"0.75"}]}"#;
    let positions = parse_portfolio(body, "main").unwrap();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].account, "?");
}

#[test]
fn active_fixture_flags_stale_positions() {
    let positions = parse_portfolio(ACTIVE, "main").unwrap();
    // In the live capture the lending indexer lagged the price feed by 39 h
    // and the multiply indexer by 61 h.
    assert_eq!(
        positions[0].stale_hint.as_deref(),
        Some("positions stale 39 h")
    );
    assert_eq!(
        positions[2].stale_hint.as_deref(),
        Some("positions stale 61 h")
    );
}

#[test]
fn empty_wallet_fixture_yields_no_positions() {
    let positions = parse_portfolio(EMPTY, "main").unwrap();
    assert!(positions.is_empty());
}

#[test]
fn plain_text_error_body_is_an_error() {
    let err = parse_portfolio("Loan abc not found", "main").unwrap_err();
    assert!(err.contains("not JSON"), "err: {err}");
}

#[test]
fn iso_parser_matches_known_epochs() {
    assert_eq!(iso_to_epoch("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(iso_to_epoch("2000-01-01T00:00:00.000Z"), Some(946_684_800));
    let a = iso_to_epoch("2026-07-17T01:56:09.892Z").unwrap();
    let b = iso_to_epoch("2026-07-18T17:05:40.206Z").unwrap();
    assert_eq!(b - a, 140_971);
    assert_eq!(iso_to_epoch("garbage"), None);
}
