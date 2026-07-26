//! Pins the worked example in `README.md` to real rendered output. The two
//! captures come from two different wallets, so the example labels both
//! `demo`; every figure on the lines below is decoded, none is written by
//! hand.

use std::collections::HashMap;

use lending_health::health::{render_report, Config};
use lending_health::kamino::parse_portfolio;
use lending_health::marginfi::parse_gpa_response;

const KAMINO: &str = include_str!("fixtures/kamino_portfolio_active.json");
const MARGINFI: &str = include_str!("fixtures/marginfi_gpa_response.json");

const EXPECTED: &str = "\
Lending health: 4 position(s), worst risk WARN.
[WARN] demo kamino Vanilla@7u3H #HcrU..iS4J: deposit $53724, borrow $40471, LTV 75.3% of 79.9% liq (positions stale 39 h)
[WARN] demo kamino Multiply@47tf #FWjx..Vq67: deposit $65030, borrow $42580, LTV 65.5% of 75.0% liq (positions stale 61 h)
[UNKNOWN] demo marginfi acct #EN1W..K7ND: deposit $860, borrow $668, LTV n/a (maint basis unavailable)
[OK] demo kamino Vanilla@47tf #6FJt..SSLy: deposit $200638, borrow $125169, LTV 62.4% of 75.0% liq (positions stale 39 h)";

fn config() -> Config {
    let section: HashMap<String, String> = [
        (
            "wallets".to_string(),
            "demo:AcNSmd5CtVEqL2CMDDKcC4Bp1rHRD9GcRxNJgcSHTxrb".to_string(),
        ),
        (
            "rpc_url".to_string(),
            "https://example-rpc.test".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    Config::from_section(&section).expect("valid section must parse")
}

#[test]
fn readme_worked_example_is_the_rendered_report() {
    let mut positions = parse_portfolio(KAMINO, "demo").expect("kamino capture must parse");
    positions.extend(parse_gpa_response(MARGINFI, "demo").expect("marginfi capture must parse"));
    assert_eq!(render_report(&positions, &config()), EXPECTED);
}
