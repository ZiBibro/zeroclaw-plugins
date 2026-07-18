//! Pure core of the `stake_monitor` tool: config parsing, JSON-RPC request
//! construction, response parsing, status derivation, and report rendering.
//! No wasm and no I/O in here, so the whole module runs under a plain host
//! `cargo test`.
//!
//! Response shapes were verified against live mainnet RPC calls on
//! 2026-07-18: `getEpochInfo`, `getVoteAccounts` (with the `votePubkey`
//! filter), `getAccountInfo` (jsonParsed), and `getInflationReward`.
//! Numeric delegation fields arrive as decimal strings; an active stake has
//! `deactivationEpoch` equal to u64::MAX rendered as a string.

use std::collections::HashMap;

use serde_json::Value;

/// Hard cap for the rendered report, in characters. Keeps the tool output
/// around 200 tokens so a scheduled briefing never floods the agent context.
pub const REPORT_CHAR_CAP: usize = 900;

pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

const CONFIG_KEYS: [&str; 3] = ["stake_accounts", "rpc_url", "timeout_secs"];

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Average slot time used only for the human "epoch ends in ~N h" hint.
const SECONDS_PER_SLOT: f64 = 0.4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StakeAccountRef {
    pub label: String,
    pub pubkey: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub accounts: Vec<StakeAccountRef>,
    pub rpc_url: String,
    pub timeout_secs: u64,
}

impl Config {
    /// Parses the host-injected `__config` section. Fail-closed: any unknown
    /// key is an error, so a typo surfaces immediately.
    pub fn from_section(section: &HashMap<String, String>) -> Result<Self, String> {
        for key in section.keys() {
            if !CONFIG_KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "unknown config key `{key}`; expected one of: {}",
                    CONFIG_KEYS.join(", ")
                ));
            }
        }

        let accounts = parse_accounts(section.get("stake_accounts").ok_or(
            "config key `stake_accounts` is required: a comma-separated allowlist like `main:<pubkey>` or bare pubkeys",
        )?)?;

        let rpc_url = section
            .get("rpc_url")
            .ok_or("config key `rpc_url` is required")?
            .trim()
            .trim_end_matches('/')
            .to_string();
        if !rpc_url.starts_with("https://") {
            return Err(format!("rpc_url must be an https:// URL, got `{rpc_url}`"));
        }

        let timeout_secs = match section.get("timeout_secs") {
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("timeout_secs must be a positive integer, got `{raw}`"))?,
            None => DEFAULT_TIMEOUT_SECS,
        };
        if timeout_secs == 0 || timeout_secs > 60 {
            return Err(format!(
                "timeout_secs must be between 1 and 60, got {timeout_secs}"
            ));
        }

        Ok(Config {
            accounts,
            rpc_url,
            timeout_secs,
        })
    }

    /// Resolves the optional `account` argument against the allowlist. The
    /// model can only pick a configured account, never introduce a new one.
    pub fn resolve_account(
        &self,
        requested: Option<&str>,
    ) -> Result<Vec<&StakeAccountRef>, String> {
        match requested {
            None => Ok(self.accounts.iter().collect()),
            Some(query) => {
                let q = query.trim();
                let hit: Vec<&StakeAccountRef> = self
                    .accounts
                    .iter()
                    .filter(|a| a.label == q || a.pubkey == q)
                    .collect();
                if hit.is_empty() {
                    Err(format!(
                        "stake account `{q}` is not in the configured allowlist; known labels: {}",
                        self.accounts
                            .iter()
                            .map(|a| a.label.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                } else {
                    Ok(hit)
                }
            }
        }
    }
}

fn parse_accounts(raw: &str) -> Result<Vec<StakeAccountRef>, String> {
    let mut out = Vec::new();
    for (i, entry) in raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let (label, pubkey) = match entry.split_once(':') {
            Some((l, p)) => (l.trim().to_string(), p.trim().to_string()),
            None => (format!("stake{}", i + 1), entry.to_string()),
        };
        validate_pubkey(&pubkey)?;
        if out.iter().any(|a: &StakeAccountRef| a.label == label) {
            return Err(format!("duplicate stake account label `{label}`"));
        }
        out.push(StakeAccountRef { label, pubkey });
    }
    if out.is_empty() {
        return Err("config key `stake_accounts` must contain at least one entry".to_string());
    }
    Ok(out)
}

pub fn validate_pubkey(candidate: &str) -> Result<(), String> {
    let bytes = bs58::decode(candidate)
        .into_vec()
        .map_err(|_| format!("`{candidate}` is not valid base58"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "`{candidate}` is not a valid Solana pubkey (decoded {} bytes, expected 32)",
            bytes.len()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON-RPC request bodies
// ---------------------------------------------------------------------------

pub fn epoch_info_body() -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getEpochInfo", "params": []
    })
    .to_string()
}

pub fn stake_account_body(pubkey: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [pubkey, { "encoding": "jsonParsed" }]
    })
    .to_string()
}

/// One vote account, filtered server-side so the response stays tiny instead
/// of the full 700-validator roster.
pub fn vote_account_body(vote_pubkey: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getVoteAccounts",
        "params": [{ "votePubkey": vote_pubkey }]
    })
    .to_string()
}

pub fn inflation_reward_body(pubkeys: &[String], epoch: u64) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getInflationReward",
        "params": [pubkeys, { "epoch": epoch }]
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

fn rpc_result(body: &str) -> Result<Value, String> {
    let root: Value =
        serde_json::from_str(body).map_err(|e| format!("RPC reply is not JSON: {e}"))?;
    if let Some(err) = root.get("error") {
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("?");
        return Err(format!("RPC error: {msg}"));
    }
    root.get("result")
        .cloned()
        .ok_or_else(|| "RPC reply has no result".to_string())
}

#[derive(Debug, Clone, Copy)]
pub struct EpochInfo {
    pub epoch: u64,
    pub slot_index: u64,
    pub slots_in_epoch: u64,
}

impl EpochInfo {
    pub fn hours_to_end(&self) -> u64 {
        let slots_left = self.slots_in_epoch.saturating_sub(self.slot_index);
        (slots_left as f64 * SECONDS_PER_SLOT / 3600.0).round() as u64
    }
}

pub fn parse_epoch_info(body: &str) -> Result<EpochInfo, String> {
    let r = rpc_result(body)?;
    Ok(EpochInfo {
        epoch: r
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or("epoch missing")?,
        slot_index: r
            .get("slotIndex")
            .and_then(Value::as_u64)
            .ok_or("slotIndex missing")?,
        slots_in_epoch: r
            .get("slotsInEpoch")
            .and_then(Value::as_u64)
            .ok_or("slotsInEpoch missing")?,
    })
}

#[derive(Debug, Clone)]
pub struct Delegation {
    pub voter: String,
    pub stake_lamports: u64,
    pub activation_epoch: u64,
    pub deactivation_epoch: u64,
}

#[derive(Debug, Clone)]
pub struct StakeState {
    pub lamports: u64,
    pub delegation: Option<Delegation>,
}

/// Delegation numbers arrive as decimal strings (u64 as string), with
/// u64::MAX meaning "no deactivation scheduled".
fn str_u64(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => s.trim().parse::<u64>().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

pub fn parse_stake_account(body: &str) -> Result<StakeState, String> {
    let r = rpc_result(body)?;
    let value = r
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or("stake account not found on chain")?;
    let lamports = value
        .get("lamports")
        .and_then(Value::as_u64)
        .ok_or("lamports missing")?;
    let parsed = value
        .get("data")
        .and_then(|d| d.get("parsed"))
        .ok_or("account is not jsonParsed; is this a stake account?")?;
    let program = value
        .get("data")
        .and_then(|d| d.get("program"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if program != "stake" {
        return Err(format!(
            "account is owned by `{program}`; expected a stake account"
        ));
    }

    let delegation = parsed
        .get("info")
        .and_then(|i| i.get("stake"))
        .filter(|s| !s.is_null())
        .and_then(|s| s.get("delegation"))
        .and_then(|d| {
            Some(Delegation {
                voter: d.get("voter")?.as_str()?.to_string(),
                stake_lamports: str_u64(d.get("stake")?)?,
                activation_epoch: str_u64(d.get("activationEpoch")?)?,
                deactivation_epoch: str_u64(d.get("deactivationEpoch")?)?,
            })
        });

    Ok(StakeState {
        lamports,
        delegation,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatorStatus {
    Ok { commission_bps: u64 },
    Delinquent { commission_bps: u64 },
    Unknown,
}

/// Reads a `getVoteAccounts` reply that was filtered by `votePubkey`.
/// Commission is taken from `inflationRewardsCommissionBps` when present,
/// with the legacy percentage `commission` as the fallback, because the
/// modern field is authoritative and the legacy one can lag.
pub fn parse_vote_status(body: &str, voter: &str) -> Result<ValidatorStatus, String> {
    let r = rpc_result(body)?;
    let pick = |list: &str| -> Option<u64> {
        r.get(list)?
            .as_array()?
            .iter()
            .find(|v| v.get("votePubkey").and_then(Value::as_str) == Some(voter))
            .map(|v| {
                v.get("inflationRewardsCommissionBps")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        v.get("commission").and_then(Value::as_u64).unwrap_or(0) * 100
                    })
            })
    };
    if let Some(bps) = pick("current") {
        return Ok(ValidatorStatus::Ok {
            commission_bps: bps,
        });
    }
    if let Some(bps) = pick("delinquent") {
        return Ok(ValidatorStatus::Delinquent {
            commission_bps: bps,
        });
    }
    Ok(ValidatorStatus::Unknown)
}

#[derive(Debug, Clone, Copy)]
pub struct Reward {
    pub amount_lamports: u64,
    pub commission_bps: Option<u64>,
}

/// Reads a `getInflationReward` reply: one entry per requested address,
/// null when the address earned nothing that epoch. The modern field is
/// `commissionBps`; the legacy `commission` can be null even when a reward
/// exists, so it is only a fallback.
pub fn parse_inflation_rewards(body: &str, expected: usize) -> Result<Vec<Option<Reward>>, String> {
    let r = rpc_result(body)?;
    let arr = r
        .as_array()
        .ok_or("getInflationReward result is not an array")?;
    if arr.len() != expected {
        return Err(format!(
            "getInflationReward returned {} entries, expected {expected}",
            arr.len()
        ));
    }
    Ok(arr
        .iter()
        .map(|v| {
            if v.is_null() {
                return None;
            }
            Some(Reward {
                amount_lamports: v.get("amount").and_then(Value::as_u64)?,
                commission_bps: v
                    .get("commissionBps")
                    .and_then(Value::as_u64)
                    .or_else(|| v.get("commission").and_then(Value::as_u64).map(|c| c * 100)),
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Status derivation and rendering
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StakeStatus {
    NotDelegated,
    Activating,
    Active,
    Deactivating,
    Inactive,
}

impl StakeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StakeStatus::NotDelegated => "not delegated",
            StakeStatus::Activating => "activating",
            StakeStatus::Active => "active",
            StakeStatus::Deactivating => "deactivating",
            StakeStatus::Inactive => "inactive",
        }
    }
}

pub fn derive_status(delegation: Option<&Delegation>, current_epoch: u64) -> StakeStatus {
    match delegation {
        None => StakeStatus::NotDelegated,
        Some(d) => {
            if d.deactivation_epoch == u64::MAX {
                if current_epoch <= d.activation_epoch {
                    StakeStatus::Activating
                } else {
                    StakeStatus::Active
                }
            } else if current_epoch <= d.deactivation_epoch {
                StakeStatus::Deactivating
            } else {
                StakeStatus::Inactive
            }
        }
    }
}

/// One fully assembled report row.
#[derive(Debug, Clone)]
pub struct Entry {
    pub label: String,
    pub state: StakeState,
    pub status: StakeStatus,
    pub validator: Option<ValidatorStatus>,
    pub reward: Option<Reward>,
}

fn fmt_sol(lamports: u64) -> String {
    let sol = lamports as f64 / LAMPORTS_PER_SOL;
    if sol >= 100.0 {
        format!("{sol:.0}")
    } else {
        format!("{sol:.3}")
    }
}

pub fn render_report(entries: &[Entry], epoch: &EpochInfo) -> String {
    if entries.is_empty() {
        return "No stake accounts to report.".to_string();
    }

    let total: u64 = entries
        .iter()
        .map(|e| {
            e.state
                .delegation
                .as_ref()
                .map(|d| d.stake_lamports)
                .unwrap_or(0)
        })
        .sum();
    let delinquent = entries
        .iter()
        .filter(|e| matches!(e.validator, Some(ValidatorStatus::Delinquent { .. })))
        .count();

    let mut lines = vec![format!(
        "Stake: {} account(s), {} SOL delegated, epoch {} (~{} h left).{}",
        entries.len(),
        fmt_sol(total),
        epoch.epoch,
        epoch.hours_to_end(),
        if delinquent > 0 {
            format!(" {delinquent} validator(s) DELINQUENT.")
        } else {
            String::new()
        }
    )];

    for e in entries {
        let mut parts = vec![format!(
            "[{}] {}: {} SOL",
            e.status.as_str(),
            e.label,
            fmt_sol(
                e.state
                    .delegation
                    .as_ref()
                    .map(|d| d.stake_lamports)
                    .unwrap_or(e.state.lamports)
            )
        )];
        if let Some(d) = &e.state.delegation {
            let voter_short: String = d.voter.chars().take(4).collect();
            let vstat = match &e.validator {
                Some(ValidatorStatus::Ok { commission_bps }) => {
                    format!(
                        "validator {voter_short}.. ok, fee {:.1}%",
                        *commission_bps as f64 / 100.0
                    )
                }
                Some(ValidatorStatus::Delinquent { .. }) => {
                    format!("validator {voter_short}.. DELINQUENT")
                }
                Some(ValidatorStatus::Unknown) => format!("validator {voter_short}.. not found"),
                None => format!("validator {voter_short}.."),
            };
            parts.push(vstat);
        }
        match &e.reward {
            Some(r) => parts.push(format!("last reward {} SOL", fmt_sol(r.amount_lamports))),
            None => {
                if e.status == StakeStatus::Active {
                    parts.push("no reward last epoch".to_string());
                }
            }
        }
        lines.push(parts.join(", "));
    }

    let mut report = lines.join("\n");
    if report.len() > REPORT_CHAR_CAP {
        let mut kept = Vec::new();
        let mut used = 0usize;
        for line in &lines {
            if used + line.len() + 1 > REPORT_CHAR_CAP.saturating_sub(40) {
                break;
            }
            used += line.len() + 1;
            kept.push(line.clone());
        }
        let omitted = lines.len() - kept.len();
        kept.push(format!("(+{omitted} more line(s) omitted)"));
        report = kept.join("\n");
    }
    report
}
