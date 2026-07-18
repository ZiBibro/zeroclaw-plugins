//! MarginFi data path: `getProgramAccounts` request construction and raw
//! account decoding at fixed byte offsets.
//!
//! Offsets derive from the marginfi-v2 type crate (commit d4c70c8) and were
//! sanity-checked on 2026-07-18 by decoding a live mainnet account and
//! cross-checking the group and authority fields against a second RPC read.
//! The account fixture under `tests/fixtures/` is that live capture.

use base64::Engine;
use serde_json::Value;

use crate::health::{Position, Protocol};

pub const MARGINFI_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

/// First 8 bytes of every MarginfiAccount: sha256("account:MarginfiAccount")
/// truncated, base58-encoded for the memcmp filter.
pub const ACCOUNT_DISCRIMINATOR_B58: &str = "CKkRR4La3xu";

/// On-chain size of a MarginfiAccount: 8-byte discriminator + 2304 struct.
pub const ACCOUNT_SIZE: u64 = 2312;

const OFFSET_AUTHORITY: usize = 40;
const OFFSET_ASSET_VALUE: usize = 1840;
const OFFSET_LIABILITY_VALUE: usize = 1856;
const OFFSET_ASSET_VALUE_MAINT: usize = 1872;
const OFFSET_LIABILITY_VALUE_MAINT: usize = 1888;
const OFFSET_FLAGS: usize = 1944;

const FLAG_HEALTHY: u32 = 1;
const FLAG_ORACLE_OK: u32 = 4;

/// JSON-RPC body for `getProgramAccounts` filtered down to the marginfi
/// accounts owned by one authority. The filters mirror the live-verified
/// query: exact size, account discriminator, authority at offset 40.
pub fn gpa_request_body(authority_pubkey: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getProgramAccounts",
        "params": [
            MARGINFI_PROGRAM,
            {
                "encoding": "base64",
                "filters": [
                    { "dataSize": ACCOUNT_SIZE },
                    { "memcmp": { "offset": 0, "bytes": ACCOUNT_DISCRIMINATOR_B58 } },
                    { "memcmp": { "offset": OFFSET_AUTHORITY, "bytes": authority_pubkey } }
                ]
            }
        ]
    })
    .to_string()
}

/// Parses a `getProgramAccounts` response into normalized positions, one per
/// marginfi account. Accounts with no value on either side are skipped.
pub fn parse_gpa_response(body: &str, wallet_label: &str) -> Result<Vec<Position>, String> {
    let root: Value =
        serde_json::from_str(body).map_err(|e| format!("marginfi RPC reply is not JSON: {e}"))?;
    if let Some(err) = root.get("error") {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("?");
        return Err(format!("marginfi RPC error: {msg}"));
    }
    let Some(rows) = root.get("result").and_then(Value::as_array) else {
        return Err("marginfi RPC reply has no result array".to_string());
    };

    let mut out = Vec::new();
    for row in rows {
        let pubkey = row.get("pubkey").and_then(Value::as_str).unwrap_or("?");
        let Some(b64) = row
            .get("account")
            .and_then(|a| a.get("data"))
            .and_then(Value::as_array)
            .and_then(|d| d.first())
            .and_then(Value::as_str)
        else {
            continue;
        };
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("marginfi account data is not base64: {e}"))?;
        if let Some(p) = decode_account(&data, pubkey, wallet_label) {
            out.push(p);
        }
    }
    Ok(out)
}

/// Decodes one raw MarginfiAccount using the health cache the program itself
/// maintains: maintenance-weighted asset and liability values plus status
/// flags. No interest math is re-derived on our side.
pub fn decode_account(data: &[u8], pubkey: &str, wallet_label: &str) -> Option<Position> {
    if data.len() < ACCOUNT_SIZE as usize {
        return None;
    }
    let asset = i80f48_at(data, OFFSET_ASSET_VALUE)?;
    let liability = i80f48_at(data, OFFSET_LIABILITY_VALUE)?;
    let asset_maint = i80f48_at(data, OFFSET_ASSET_VALUE_MAINT)?;
    let liability_maint = i80f48_at(data, OFFSET_LIABILITY_VALUE_MAINT)?;
    let flags = u32::from_le_bytes(data[OFFSET_FLAGS..OFFSET_FLAGS + 4].try_into().ok()?);

    if asset == 0.0 && liability == 0.0 && asset_maint == 0.0 && liability_maint == 0.0 {
        return None;
    }

    let healthy = flags & FLAG_HEALTHY != 0;
    let oracle_ok = flags & FLAG_ORACLE_OK != 0;

    // Liquidation begins when maintenance-weighted liabilities reach
    // maintenance-weighted assets, so that ratio maps onto the LTV scale
    // with the liquidation threshold at 1.0. When the oracle flag is unset
    // the maintenance pair can be zeroed by the risk engine; falling back to
    // the initial-weight pair avoids a false liquidation alarm, and the hint
    // says which basis the number is on.
    let maint_usable = asset_maint > 0.0 || (oracle_ok && liability_maint == 0.0);
    let (mut ltv, stale_hint) = if maint_usable {
        let ratio = if asset_maint > 0.0 {
            liability_maint / asset_maint
        } else {
            0.0
        };
        let hint = (!oracle_ok).then(|| "oracle flag unset".to_string());
        (ratio, hint)
    } else {
        let ratio = if asset > 0.0 {
            liability / asset
        } else if liability > 0.0 {
            1.0
        } else {
            0.0
        };
        (
            ratio,
            Some("oracle flag unset; init-weight ratio".to_string()),
        )
    };
    if !healthy {
        ltv = ltv.max(1.0);
    }

    Some(Position {
        wallet_label: wallet_label.to_string(),
        protocol: Protocol::Marginfi,
        market: format!("acct@{}", pubkey.chars().take(4).collect::<String>()),
        deposit_usd: asset,
        borrow_usd: liability,
        ltv,
        liquidation_ltv: 1.0,
        stale_hint,
    })
}

/// Reads a `WrappedI80F48` (16 bytes, little-endian i128 with 48 fractional
/// bits) and converts it to `f64`.
fn i80f48_at(data: &[u8], offset: usize) -> Option<f64> {
    let raw: [u8; 16] = data.get(offset..offset + 16)?.try_into().ok()?;
    let fixed = i128::from_le_bytes(raw);
    Some(fixed as f64 / (1u64 << 48) as f64)
}
