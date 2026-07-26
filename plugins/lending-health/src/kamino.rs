//! Kamino data path: request URL construction and portfolio parsing.
//!
//! Contract verified live against `api.kamino.finance` on 2026-07-18; the
//! JSON fixtures under `tests/fixtures/` are raw captures of that API.
//! Numeric values in the portfolio response arrive as decimal strings, and
//! every LTV field is a fraction of one, not a percentage.

use serde_json::Value;

use crate::health::{short_account, Liquidation, Position, Protocol};

/// Products in the portfolio response that carry lending-style obligations.
/// Multiply and leverage obligations do not appear in the `lending` array,
/// so all three must be walked for a complete health picture.
const OBLIGATION_PRODUCTS: [&str; 3] = ["lending", "multiply", "leverage"];

/// Positions-vs-prices skew above this many hours earns a stale hint.
const STALE_SKEW_HOURS: i64 = 6;

pub fn portfolio_url(api_base: &str, wallet_pubkey: &str) -> String {
    format!("{api_base}/portfolio/{wallet_pubkey}")
}

/// Parses a `GET /portfolio/{wallet}` body into normalized positions.
/// A wallet with no positions yields an empty vector, not an error.
pub fn parse_portfolio(body: &str, wallet_label: &str) -> Result<Vec<Position>, String> {
    let root: Value =
        serde_json::from_str(body).map_err(|e| format!("kamino portfolio is not JSON: {e}"))?;

    let mut out = Vec::new();
    for product in OBLIGATION_PRODUCTS {
        let stale_hint = staleness_hint(&root, product);
        let Some(rows) = root.get(product).and_then(Value::as_array) else {
            continue;
        };
        for row in rows {
            if let Some(p) = parse_row(row, product, wallet_label, stale_hint.clone()) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

fn parse_row(
    row: &Value,
    product: &str,
    wallet_label: &str,
    stale_hint: Option<String>,
) -> Option<Position> {
    let deposit_usd = str_num(row, "totalDepositValue")?;
    let borrow_usd = str_num(row, "totalBorrowValue")?;
    if deposit_usd == 0.0 && borrow_usd == 0.0 {
        return None;
    }
    let ltv = str_num(row, "ltv")?;
    let liquidation_ltv = str_num(row, "liquidationLtv")?;

    let tag = row.get("tag").and_then(Value::as_str).unwrap_or(product);
    let market = row
        .get("market")
        .and_then(Value::as_str)
        .map(short_pubkey)
        .unwrap_or_else(|| "?".to_string());
    // The obligation address is the identity of the position itself; a wallet
    // can hold several in one market, so the report names the one it read.
    let account = row
        .get("obligation")
        .and_then(Value::as_str)
        .map(short_account)
        .unwrap_or_else(|| "?".to_string());

    Some(Position {
        wallet_label: wallet_label.to_string(),
        protocol: Protocol::Kamino,
        market: format!("{tag}@{market}"),
        account,
        deposit_usd,
        borrow_usd,
        liquidation: Some(Liquidation {
            ltv,
            liquidation_ltv,
        }),
        // The portfolio response carries no protocol-side liquidatable flag;
        // the ratio it does carry is the whole verdict here.
        flagged_unhealthy: false,
        stale_hint,
    })
}

/// Portfolio numbers are decimal strings like `"0.62385441527566678867"`.
fn str_num(row: &Value, key: &str) -> Option<f64> {
    row.get(key)?.as_str()?.trim().parse::<f64>().ok()
}

fn short_pubkey(pk: &str) -> String {
    pk.chars().take(4).collect()
}

/// Compares `positionsRefreshedOn` with `pricesRefreshedOn` for a product
/// section. The indexer can lag the price feed by hours; the report says so
/// instead of presenting stale positions as current.
fn staleness_hint(root: &Value, product: &str) -> Option<String> {
    let section = root.get("sections")?.get(product)?;
    let positions = iso_to_epoch(section.get("positionsRefreshedOn")?.as_str()?)?;
    let prices = iso_to_epoch(section.get("pricesRefreshedOn")?.as_str()?)?;
    let skew_hours = (prices - positions) / 3600;
    if skew_hours >= STALE_SKEW_HOURS {
        Some(format!("positions stale {skew_hours} h"))
    } else {
        None
    }
}

/// Minimal parser for the fixed API format `YYYY-MM-DDTHH:MM:SS.mmmZ`.
/// Returns unix seconds. Days-from-civil per Howard Hinnant's algorithm.
pub fn iso_to_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}
