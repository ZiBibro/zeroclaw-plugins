//! Pure core of the `stake_tx_build` tool: config parsing and the byte-level
//! assembly of unsigned legacy Solana transactions for stake delegation and
//! stake deactivation. No wasm bindings and no I/O live here, so the whole
//! module runs under a plain host `cargo test`.
//!
//! Every instruction-level byte fact (discriminants, account order, flags,
//! message layout, compact-u16) follows the Gate A verification report
//! `research/gate-a/p2-stake-tx.md`, which was checked against the
//! solana-program sources and a live mainnet delegate transaction. The
//! builder produces transactions only; it never sees a private key and it
//! cannot sign or submit anything.

use std::collections::HashMap;

use base64::Engine;
use serde_json::Value;

pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

const CONFIG_KEYS: [&str; 7] = [
    "stake_accounts",
    "authority",
    "rpc_url",
    "allowed_vote_accounts",
    "nonce_account",
    "nonce_authority",
    "timeout_secs",
];

/// Stake program id, confirmed by the mainnet fixture in p2-stake-tx.md
/// section 3 (accountKeys[4] of the delegate transaction).
pub const STAKE_PROGRAM_ID: &str = "Stake11111111111111111111111111111111111111";

/// System program id; owner of nonce accounts and home of
/// AdvanceNonceAccount (p2-stake-tx.md section 2).
pub const SYSTEM_PROGRAM_ID: &str = "11111111111111111111111111111111";

/// Clock sysvar, account 2 of DelegateStake and account 1 of Deactivate
/// (p2-stake-tx.md section 1).
pub const SYSVAR_CLOCK_ID: &str = "SysvarC1ock11111111111111111111111111111111";

/// Stake history sysvar, account 3 of DelegateStake (p2-stake-tx.md
/// section 1). Deactivate does not take it.
pub const SYSVAR_STAKE_HISTORY_ID: &str = "SysvarStakeHistory1111111111111111111111111";

/// Stake config account, account 4 of DelegateStake. Semantically dead but
/// positionally required for compatibility; address confirmed by
/// p2-stake-tx.md section 1 (declare_deprecated_id in config.rs).
pub const STAKE_CONFIG_ID: &str = "StakeConfig11111111111111111111111111111111";

/// RecentBlockhashes sysvar, account 1 of AdvanceNonceAccount. Deprecated
/// but still mandatory in the instruction (p2-stake-tx.md section 2).
pub const SYSVAR_RECENT_BLOCKHASHES_ID: &str = "SysvarRecentB1ockHashes11111111111111111111";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StakeAccountRef {
    pub label: String,
    pub pubkey: String,
}

#[derive(Debug, Clone)]
pub struct NoncePair {
    pub account: String,
    pub authority: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub accounts: Vec<StakeAccountRef>,
    pub authority: String,
    pub rpc_url: String,
    pub allowed_vote_accounts: Vec<String>,
    pub nonce: Option<NoncePair>,
    pub timeout_secs: u64,
}

impl Config {
    /// Parses the host-injected `__config` section. Fail-closed: any unknown
    /// key is an error, so a typo surfaces immediately instead of silently
    /// weakening an allowlist.
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

        let authority = section
            .get("authority")
            .ok_or("config key `authority` is required: the fee payer and stake authority pubkey (never a private key)")?
            .trim()
            .to_string();
        validate_pubkey(&authority)?;

        let rpc_url = section
            .get("rpc_url")
            .ok_or("config key `rpc_url` is required")?
            .trim()
            .trim_end_matches('/')
            .to_string();
        if !rpc_url.starts_with("https://") {
            return Err(format!("rpc_url must be an https:// URL, got `{rpc_url}`"));
        }

        let allowed_vote_accounts = match section.get("allowed_vote_accounts") {
            Some(raw) => parse_vote_allowlist(raw)?,
            None => Vec::new(),
        };

        let nonce = match (section.get("nonce_account"), section.get("nonce_authority")) {
            (None, None) => None,
            (Some(account), Some(authority)) => {
                let account = account.trim().to_string();
                let authority = authority.trim().to_string();
                validate_pubkey(&account)?;
                validate_pubkey(&authority)?;
                Some(NoncePair { account, authority })
            }
            _ => {
                return Err(
                    "config keys `nonce_account` and `nonce_authority` must be set together or not at all"
                        .to_string(),
                )
            }
        };

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
            authority,
            rpc_url,
            allowed_vote_accounts,
            nonce,
            timeout_secs,
        })
    }

    /// Resolves the `stake_account` argument against the allowlist. The
    /// model can only pick a configured account, never introduce a new one.
    pub fn resolve_stake(&self, requested: &str) -> Result<&StakeAccountRef, String> {
        let q = requested.trim();
        self.accounts
            .iter()
            .find(|a| a.label == q || a.pubkey == q)
            .ok_or_else(|| {
                format!(
                    "stake account `{q}` is not in the configured allowlist; known labels: {}",
                    self.accounts
                        .iter()
                        .map(|a| a.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
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

fn parse_vote_allowlist(raw: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        validate_pubkey(entry)?;
        if out.iter().any(|v| v == entry) {
            return Err(format!(
                "duplicate vote account `{entry}` in allowed_vote_accounts"
            ));
        }
        out.push(entry.to_string());
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

pub fn decode_pubkey(candidate: &str) -> Result<[u8; 32], String> {
    let bytes = bs58::decode(candidate)
        .into_vec()
        .map_err(|_| format!("`{candidate}` is not valid base58"))?;
    bytes
        .try_into()
        .map_err(|_| format!("`{candidate}` is not a valid Solana pubkey"))
}

/// Decodes a well-known base58 constant defined in this module. Only called
/// with the program and sysvar ids above, all of which are covered by tests.
fn known_key(constant: &str) -> [u8; 32] {
    decode_pubkey(constant).expect("static base58 constant must decode")
}

// ---------------------------------------------------------------------------
// Action and argument validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Delegate,
    Deactivate,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Delegate => "delegate",
            Action::Deactivate => "deactivate",
        }
    }
}

pub fn parse_action(raw: &str) -> Result<Action, String> {
    match raw.trim() {
        "delegate" => Ok(Action::Delegate),
        "deactivate" => Ok(Action::Deactivate),
        other => Err(format!(
            "action must be `delegate` or `deactivate`, got `{other}`"
        )),
    }
}

/// Validates the `vote_account` argument against the action and the
/// configured allowlist. Delegate without an allowlist is refused outright:
/// the operator has to opt in to every delegation target.
pub fn validate_vote(
    cfg: &Config,
    action: Action,
    vote_arg: Option<&str>,
) -> Result<Option<String>, String> {
    match action {
        Action::Deactivate => {
            if vote_arg.is_some() {
                return Err("`vote_account` is only valid for the delegate action".to_string());
            }
            Ok(None)
        }
        Action::Delegate => {
            let vote = vote_arg
                .ok_or("delegate requires a `vote_account` argument")?
                .trim()
                .to_string();
            if cfg.allowed_vote_accounts.is_empty() {
                return Err(
                    "delegate is disabled: config key `allowed_vote_accounts` is not set"
                        .to_string(),
                );
            }
            if !cfg.allowed_vote_accounts.iter().any(|v| v == &vote) {
                return Err(format!(
                    "vote account `{vote}` is not in the configured allowed_vote_accounts allowlist"
                ));
            }
            Ok(Some(vote))
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC request bodies and response parsing
// ---------------------------------------------------------------------------

pub fn latest_blockhash_body() -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getLatestBlockhash", "params": []
    })
    .to_string()
}

pub fn nonce_account_body(pubkey: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [pubkey, { "encoding": "base64" }]
    })
    .to_string()
}

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

pub fn parse_latest_blockhash(body: &str) -> Result<[u8; 32], String> {
    let r = rpc_result(body)?;
    let hash = r
        .get("value")
        .and_then(|v| v.get("blockhash"))
        .and_then(Value::as_str)
        .ok_or("getLatestBlockhash reply has no blockhash")?;
    decode_pubkey(hash).map_err(|_| format!("blockhash `{hash}` is not 32 bytes of base58"))
}

/// Extracts the durable blockhash from a nonce account read with
/// `getAccountInfo` (encoding base64). The 32 hash bytes sit at offset
/// 40..72 of the account data; the offset is fixed by the Gate A design
/// document (design.md, durable nonce section) and by the task
/// specification. p2-stake-tx.md covers the AdvanceNonceAccount instruction
/// bytes; the state layout offset comes from that design review.
pub fn parse_nonce_blockhash(body: &str) -> Result<[u8; 32], String> {
    let r = rpc_result(body)?;
    let value = r
        .get("value")
        .filter(|v| !v.is_null())
        .ok_or("nonce account not found on chain")?;
    let owner = value.get("owner").and_then(Value::as_str).unwrap_or("");
    if owner != SYSTEM_PROGRAM_ID {
        return Err(format!(
            "nonce account is owned by `{owner}`; expected the System program"
        ));
    }
    let b64 = value
        .get("data")
        .and_then(Value::as_array)
        .and_then(|d| d.first())
        .and_then(Value::as_str)
        .ok_or("nonce account data is not base64-encoded")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("nonce account data is not valid base64: {e}"))?;
    if bytes.len() < 72 {
        return Err(format!(
            "nonce account data is {} bytes, expected at least 72",
            bytes.len()
        ));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes[40..72]);
    Ok(hash)
}

// ---------------------------------------------------------------------------
// Instructions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountMeta {
    pub pubkey: [u8; 32],
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(Debug, Clone)]
pub struct Instruction {
    pub program_id: [u8; 32],
    pub accounts: Vec<AccountMeta>,
    pub data: Vec<u8>,
}

/// DelegateStake. Discriminant 2 as u32 little-endian, no payload; account
/// order and flags exactly as in p2-stake-tx.md section 1 (delegate_stake in
/// solana-program/stake, confirmed byte-for-byte by the mainnet fixture in
/// section 3). Account 4 is the deprecated stake config: unused by the
/// program but positionally required.
pub fn delegate_stake_instruction(
    stake: [u8; 32],
    authority: [u8; 32],
    vote: [u8; 32],
) -> Instruction {
    Instruction {
        program_id: known_key(STAKE_PROGRAM_ID),
        accounts: vec![
            AccountMeta {
                pubkey: stake,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: vote,
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: known_key(SYSVAR_CLOCK_ID),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: known_key(SYSVAR_STAKE_HISTORY_ID),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: known_key(STAKE_CONFIG_ID),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: authority,
                is_signer: true,
                is_writable: false,
            },
        ],
        data: vec![2, 0, 0, 0],
    }
}

/// Deactivate. Discriminant 5 as u32 little-endian, no payload; account
/// order and flags exactly as in p2-stake-tx.md section 1 (deactivate_stake
/// in solana-program/stake). Takes neither stake history nor stake config.
pub fn deactivate_instruction(stake: [u8; 32], authority: [u8; 32]) -> Instruction {
    Instruction {
        program_id: known_key(STAKE_PROGRAM_ID),
        accounts: vec![
            AccountMeta {
                pubkey: stake,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: known_key(SYSVAR_CLOCK_ID),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: authority,
                is_signer: true,
                is_writable: false,
            },
        ],
        data: vec![5, 0, 0, 0],
    }
}

/// AdvanceNonceAccount. System program discriminant 4 as u32 little-endian,
/// no payload; account order and flags exactly as in p2-stake-tx.md
/// section 2 (advance_nonce_account in anza-xyz/solana-sdk). The deprecated
/// RecentBlockhashes sysvar is still mandatory in the instruction.
pub fn advance_nonce_instruction(nonce: [u8; 32], nonce_authority: [u8; 32]) -> Instruction {
    Instruction {
        program_id: known_key(SYSTEM_PROGRAM_ID),
        accounts: vec![
            AccountMeta {
                pubkey: nonce,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: known_key(SYSVAR_RECENT_BLOCKHASHES_ID),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: nonce_authority,
                is_signer: true,
                is_writable: false,
            },
        ],
        data: vec![4, 0, 0, 0],
    }
}

// ---------------------------------------------------------------------------
// compact-u16
// ---------------------------------------------------------------------------

/// Encodes a value as compact-u16 (1 to 3 bytes). Normative behavior per
/// p2-stake-tx.md section 4, quoting the ShortU16 doc in
/// anza-xyz/solana-sdk short-vec: 7 payload bits per byte, high bit set on
/// every byte that has a continuation.
pub fn encode_compact_u16(value: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    let mut rem = u32::from(value);
    loop {
        let byte = (rem & 0x7f) as u8;
        rem >>= 7;
        if rem == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// Decodes a compact-u16 prefix, returning the value and the number of
/// bytes consumed. The inverse of [`encode_compact_u16`]; tests use it to
/// walk serialized messages.
pub fn decode_compact_u16(bytes: &[u8]) -> Option<(u16, usize)> {
    let mut value: u32 = 0;
    let mut consumed = 0usize;
    loop {
        let byte = u32::from(*bytes.get(consumed)?);
        value |= (byte & 0x7f) << (7 * consumed as u32);
        consumed += 1;
        if byte & 0x80 == 0 {
            break;
        }
        if consumed == 3 {
            return None;
        }
    }
    if value > u32::from(u16::MAX) {
        return None;
    }
    Some((value as u16, consumed))
}

// ---------------------------------------------------------------------------
// Message compilation and serialization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CompiledInstruction {
    pub program_id_index: u8,
    pub account_indices: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CompiledMessage {
    pub num_required_signatures: u8,
    pub num_readonly_signed: u8,
    pub num_readonly_unsigned: u8,
    pub account_keys: Vec<[u8; 32]>,
    pub recent_blockhash: [u8; 32],
    pub instructions: Vec<CompiledInstruction>,
}

struct KeyMeta {
    key: [u8; 32],
    signer: bool,
    writable: bool,
}

fn upsert(metas: &mut Vec<KeyMeta>, key: [u8; 32], signer: bool, writable: bool) {
    match metas.iter_mut().find(|m| m.key == key) {
        Some(m) => {
            m.signer |= signer;
            m.writable |= writable;
        }
        None => metas.push(KeyMeta {
            key,
            signer,
            writable,
        }),
    }
}

/// Deduplicates account keys and partitions them into the four groups fixed
/// by p2-stake-tx.md section 4: signer writable first, then signer
/// read-only, then non-signer writable, then non-signer read-only. Within a
/// group the order is first appearance, with the fee payer always in front
/// and program ids appended after all instruction accounts (an
/// implementation choice; the wire format only fixes the group order, and
/// header bytes partition the array without per-account flags).
pub fn compile_message(
    fee_payer: [u8; 32],
    instructions: &[Instruction],
    recent_blockhash: [u8; 32],
) -> Result<CompiledMessage, String> {
    let mut metas: Vec<KeyMeta> = vec![KeyMeta {
        key: fee_payer,
        signer: true,
        writable: true,
    }];
    for ix in instructions {
        for a in &ix.accounts {
            upsert(&mut metas, a.pubkey, a.is_signer, a.is_writable);
        }
    }
    for ix in instructions {
        upsert(&mut metas, ix.program_id, false, false);
    }

    let mut ordered: Vec<&KeyMeta> = Vec::with_capacity(metas.len());
    for (signer, writable) in [(true, true), (true, false), (false, true), (false, false)] {
        ordered.extend(
            metas
                .iter()
                .filter(|m| m.signer == signer && m.writable == writable),
        );
    }
    if ordered.len() > u8::MAX as usize {
        return Err(format!("too many account keys: {}", ordered.len()));
    }

    let signers = ordered.iter().filter(|m| m.signer).count();
    let readonly_signed = ordered.iter().filter(|m| m.signer && !m.writable).count();
    let readonly_unsigned = ordered.iter().filter(|m| !m.signer && !m.writable).count();
    let account_keys: Vec<[u8; 32]> = ordered.iter().map(|m| m.key).collect();

    let index_of = |key: [u8; 32]| -> Result<u8, String> {
        account_keys
            .iter()
            .position(|k| *k == key)
            .map(|i| i as u8)
            .ok_or_else(|| "internal: account key vanished during compilation".to_string())
    };

    let mut compiled = Vec::with_capacity(instructions.len());
    for ix in instructions {
        let account_indices = ix
            .accounts
            .iter()
            .map(|a| index_of(a.pubkey))
            .collect::<Result<Vec<u8>, String>>()?;
        compiled.push(CompiledInstruction {
            program_id_index: index_of(ix.program_id)?,
            account_indices,
            data: ix.data.clone(),
        });
    }

    Ok(CompiledMessage {
        num_required_signatures: signers as u8,
        num_readonly_signed: readonly_signed as u8,
        num_readonly_unsigned: readonly_unsigned as u8,
        account_keys,
        recent_blockhash,
        instructions: compiled,
    })
}

/// Serializes a legacy message in the wire order fixed by p2-stake-tx.md
/// section 4: three header bytes, compact-u16 key count, the 32-byte keys,
/// the recent blockhash, compact-u16 instruction count, then each compiled
/// instruction as program index, compact-u16 account count, account
/// indices, compact-u16 data length, data.
pub fn serialize_message(msg: &CompiledMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + 32 * msg.account_keys.len() + 64);
    out.push(msg.num_required_signatures);
    out.push(msg.num_readonly_signed);
    out.push(msg.num_readonly_unsigned);
    out.extend_from_slice(&encode_compact_u16(msg.account_keys.len() as u16));
    for key in &msg.account_keys {
        out.extend_from_slice(key);
    }
    out.extend_from_slice(&msg.recent_blockhash);
    out.extend_from_slice(&encode_compact_u16(msg.instructions.len() as u16));
    for ix in &msg.instructions {
        out.push(ix.program_id_index);
        out.extend_from_slice(&encode_compact_u16(ix.account_indices.len() as u16));
        out.extend_from_slice(&ix.account_indices);
        out.extend_from_slice(&encode_compact_u16(ix.data.len() as u16));
        out.extend_from_slice(&ix.data);
    }
    out
}

/// Serializes the full wire transaction: compact-u16 signature count, then
/// the signatures, then the message (p2-stake-tx.md section 4). For an
/// unsigned transaction the signature count still equals
/// num_required_signatures and each slot holds 64 zero bytes; the wallet
/// replaces the placeholders when it signs. The report describes only this
/// whole-transaction wire format, so the tool returns the full transaction
/// and no separate message blob.
pub fn serialize_transaction(num_required_signatures: u8, message: &[u8]) -> Vec<u8> {
    let sig_bytes = 64 * num_required_signatures as usize;
    let mut out = Vec::with_capacity(3 + sig_bytes + message.len());
    out.extend_from_slice(&encode_compact_u16(u16::from(num_required_signatures)));
    out.resize(out.len() + sig_bytes, 0);
    out.extend_from_slice(message);
    out
}

// ---------------------------------------------------------------------------
// Top-level build
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Built {
    pub summary: String,
    pub tx_base64: String,
}

impl Built {
    /// Two-line tool output: a human summary for the approval gate, then
    /// the base64 payload on its own labeled line.
    pub fn output(&self) -> String {
        format!("{}\nunsigned_tx_base64: {}", self.summary, self.tx_base64)
    }
}

/// Assembles the unsigned transaction. When the config carries a nonce
/// pair, an AdvanceNonceAccount instruction goes first and `blockhash` must
/// be the durable value read from the nonce account, which keeps the
/// transaction valid while it waits for operator approval. Without a nonce
/// the caller passes a fresh blockhash and the summary warns about the
/// short validity window. The staked amount is deliberately absent from
/// the summary: this builder never reads it and must not guess.
pub fn build_transaction(
    cfg: &Config,
    action: Action,
    stake: &StakeAccountRef,
    vote: Option<&str>,
    blockhash: [u8; 32],
) -> Result<Built, String> {
    let authority = decode_pubkey(&cfg.authority)?;
    let stake_key = decode_pubkey(&stake.pubkey)?;

    let mut instructions = Vec::with_capacity(2);
    if let Some(nonce) = &cfg.nonce {
        instructions.push(advance_nonce_instruction(
            decode_pubkey(&nonce.account)?,
            decode_pubkey(&nonce.authority)?,
        ));
    }
    let voter = match (action, vote) {
        (Action::Delegate, Some(v)) => {
            let vote_key = decode_pubkey(v)?;
            instructions.push(delegate_stake_instruction(stake_key, authority, vote_key));
            Some(v.to_string())
        }
        (Action::Deactivate, None) => {
            instructions.push(deactivate_instruction(stake_key, authority));
            None
        }
        _ => return Err("internal: action and vote_account mismatch".to_string()),
    };

    let message = compile_message(authority, &instructions, blockhash)?;
    let message_bytes = serialize_message(&message);
    let tx_bytes = serialize_transaction(message.num_required_signatures, &message_bytes);
    let tx_base64 = base64::engine::general_purpose::STANDARD.encode(tx_bytes);

    let target = match &voter {
        Some(v) => format!(" to vote account {v}"),
        None => String::new(),
    };
    let lifetime = if cfg.nonce.is_some() {
        "durable nonce: stays valid until the nonce advances, so it can wait in an approval queue"
    } else {
        "fresh blockhash: sign and submit within roughly 60 to 90 seconds"
    };
    let summary = format!(
        "Unsigned {} transaction for stake `{}`{}; amount not read by this builder; {}.",
        action.as_str(),
        stake.label,
        target,
        lifetime
    );

    Ok(Built { summary, tx_base64 })
}
