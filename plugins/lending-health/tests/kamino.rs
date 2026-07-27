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

/// The portfolio endpoint returned decimal strings when the fixtures were
/// captured, but the encoding is upstream's to change. A JSON number must read
/// the same way, because the alternative is a position that silently disappears
/// the day Kamino switches.
#[test]
fn a_numeric_ratio_reads_the_same_as_a_decimal_string() {
    let body = ACTIVE.replace(
        "\"ltv\":\"0.62385441527566678867\"",
        "\"ltv\":0.62385441527566678867",
    );
    assert!(
        body.contains("\"ltv\":0.6238"),
        "fixture shape changed, update this test"
    );
    let positions = parse_portfolio(&body, "main").expect("numeric ratio");
    let with_basis = positions
        .iter()
        .find(|p| {
            p.liquidation
                .is_some_and(|l| (l.ltv - 0.623_854).abs() < 1e-5)
        })
        .expect("the numeric ltv row is still parsed with its basis");
    assert!(with_basis.deposit_usd > 0.0);
}

/// Losing the ratio pair costs the liquidation distance for one position. It must
/// never cost the position, because a dropped row makes the report assert the
/// wallet holds nothing while a real borrow sits on chain.
#[test]
fn a_row_without_a_ratio_pair_survives_without_its_basis() {
    let body = ACTIVE
        .replace("\"ltv\":\"0.62385441527566678867\"", "\"ltv\":null")
        .replace(
            "\"liquidationLtv\":\"0.75000000000000000002\"",
            "\"liquidationLtv\":null",
        );
    assert!(
        body.contains("\"ltv\":null"),
        "fixture shape changed, update this test"
    );
    let before = parse_portfolio(ACTIVE, "main").expect("baseline");
    let after = parse_portfolio(&body, "main").expect("ratio-less row");
    assert_eq!(
        after.len(),
        before.len(),
        "the row must still be reported, without a measured distance"
    );
    assert!(
        after.iter().any(|p| p.liquidation.is_none()),
        "the ratio-less row must carry no basis"
    );
    assert!(
        after
            .iter()
            .any(|p| p.deposit_usd > 0.0 && p.liquidation.is_none()),
        "its deposit figure is still known and must survive"
    );
}
