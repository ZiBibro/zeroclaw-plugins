use std::collections::HashMap;

use stake_monitor::stake::{
    derive_status, parse_epoch_info, parse_inflation_rewards, parse_stake_account,
    parse_vote_status, render_report, Config, Delegation, Entry, Reward, StakeState, StakeStatus,
    ValidatorStatus, REPORT_CHAR_CAP,
};

const STAKE_A: &str = "6ySLTQWEpCFKPYKfPaKYnhKzEccuqKafFEzfJVQ4Gifp";
const STAKE_B: &str = "CEHKNKfqQhHDWgiPrLNut2K3o5izJ1gpfSZ42CWBAv5n";
const VOTER: &str = "GHViLh5MgQDGDsuwXTHM9r8kQqEnQY6WsyLvGVYbFXAA";

// Field shapes below mirror live mainnet RPC replies captured during
// verification on 2026-07-18 (epoch 1003).

const EPOCH_INFO: &str = r#"{"jsonrpc":"2.0","result":{"absoluteSlot":433721729,"blockHeight":411783502,"epoch":1003,"slotIndex":425729,"slotsInEpoch":432000,"transactionCount":530368329172},"id":1}"#;

fn stake_account_json(deactivation: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","result":{{"context":{{"slot":433721800}},"value":{{"lamports":502285880,"owner":"Stake11111111111111111111111111111111111111","space":200,"data":{{"program":"stake","space":200,"parsed":{{"type":"delegated","info":{{"meta":{{"authorized":{{"staker":"{STAKE_A}","withdrawer":"{STAKE_A}"}},"lockup":{{"custodian":"11111111111111111111111111111111","epoch":0,"unixTimestamp":0}},"rentExemptReserve":"2282880"}},"stake":{{"creditsObserved":123456789,"delegation":{{"activationEpoch":"1003","deactivationEpoch":"{deactivation}","stake":"499997717120","voter":"{VOTER}"}}}}}}}}}}}}}},"id":1}}"#
    )
}

fn section(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn base_section() -> HashMap<String, String> {
    section(&[
        ("stake_accounts", &format!("main:{STAKE_A}")[..]),
        ("rpc_url", "https://example-rpc.test"),
    ])
}

#[test]
fn config_parses_valid_section() {
    let cfg = Config::from_section(&base_section()).expect("valid section must parse");
    assert_eq!(cfg.accounts.len(), 1);
    assert_eq!(cfg.accounts[0].label, "main");
}

#[test]
fn config_rejects_unknown_key() {
    let mut s = base_section();
    s.insert("stake_acounts".to_string(), "x".to_string());
    let err = Config::from_section(&s).unwrap_err();
    assert!(
        err.contains("unknown config key `stake_acounts`"),
        "err: {err}"
    );
}

#[test]
fn config_requires_rpc_url() {
    let s = section(&[("stake_accounts", &format!("main:{STAKE_A}")[..])]);
    assert!(Config::from_section(&s).unwrap_err().contains("rpc_url"));
}

#[test]
fn config_rejects_http_url() {
    let s = section(&[
        ("stake_accounts", &format!("main:{STAKE_A}")[..]),
        ("rpc_url", "http://insecure.test"),
    ]);
    assert!(Config::from_section(&s).is_err());
}

#[test]
fn config_rejects_bad_pubkey() {
    let s = section(&[
        ("stake_accounts", "main:tooshort"),
        ("rpc_url", "https://example-rpc.test"),
    ]);
    assert!(Config::from_section(&s).is_err());
}

#[test]
fn resolve_rejects_non_allowlisted() {
    let cfg = Config::from_section(&base_section()).unwrap();
    let err = cfg.resolve_account(Some(STAKE_B)).unwrap_err();
    assert!(
        err.contains("not in the configured allowlist"),
        "err: {err}"
    );
}

#[test]
fn epoch_info_parses_live_shape() {
    let e = parse_epoch_info(EPOCH_INFO).expect("live shape must parse");
    assert_eq!(e.epoch, 1003);
    assert_eq!(e.slots_in_epoch, 432000);
    // 6271 slots left at 0.4 s per slot is well under two hours.
    assert!(e.hours_to_end() <= 2, "hours: {}", e.hours_to_end());
}

#[test]
fn stake_account_parses_active_delegation() {
    let body = stake_account_json("18446744073709551615");
    let s = parse_stake_account(&body).expect("delegated account must parse");
    let d = s.delegation.expect("delegation present");
    assert_eq!(d.voter, VOTER);
    assert_eq!(d.stake_lamports, 499_997_717_120);
    assert_eq!(d.deactivation_epoch, u64::MAX);
}

#[test]
fn stake_account_not_found_is_error() {
    let body = r#"{"jsonrpc":"2.0","result":{"context":{"slot":1},"value":null},"id":1}"#;
    let err = parse_stake_account(body).unwrap_err();
    assert!(err.contains("not found"), "err: {err}");
}

#[test]
fn non_stake_account_is_error() {
    let body = r#"{"jsonrpc":"2.0","result":{"context":{"slot":1},"value":{"lamports":1,"owner":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA","data":{"program":"spl-token","parsed":{"type":"account","info":{}},"space":165}}},"id":1}"#;
    let err = parse_stake_account(body).unwrap_err();
    assert!(err.contains("expected a stake account"), "err: {err}");
}

#[test]
fn vote_status_prefers_bps_field() {
    let body = format!(
        r#"{{"jsonrpc":"2.0","result":{{"current":[{{"votePubkey":"{VOTER}","nodePubkey":"x","activatedStake":1,"commission":7,"inflationRewardsCommissionBps":700,"epochVoteAccount":true,"epochCredits":[],"lastVote":1,"rootSlot":1}}],"delinquent":[]}},"id":1}}"#
    );
    assert_eq!(
        parse_vote_status(&body, VOTER).unwrap(),
        ValidatorStatus::Ok {
            commission_bps: 700
        }
    );
}

#[test]
fn vote_status_detects_delinquent_and_unknown() {
    let delinquent = format!(
        r#"{{"jsonrpc":"2.0","result":{{"current":[],"delinquent":[{{"votePubkey":"{VOTER}","commission":5,"activatedStake":1,"epochCredits":[],"lastVote":1}}]}},"id":1}}"#
    );
    assert_eq!(
        parse_vote_status(&delinquent, VOTER).unwrap(),
        ValidatorStatus::Delinquent {
            commission_bps: 500
        }
    );
    let empty = r#"{"jsonrpc":"2.0","result":{"current":[],"delinquent":[]},"id":1}"#;
    assert_eq!(
        parse_vote_status(empty, VOTER).unwrap(),
        ValidatorStatus::Unknown
    );
}

#[test]
fn inflation_rewards_parse_live_shape_with_null_commission() {
    let body = r#"{"jsonrpc":"2.0","result":[{"amount":595001,"commission":null,"commissionBps":300,"effectiveSlot":433296296,"epoch":1002,"postBalance":2025175995},null],"id":1}"#;
    let rewards = parse_inflation_rewards(body, 2).expect("live shape must parse");
    let first = rewards[0].expect("first entry has a reward");
    assert_eq!(first.amount_lamports, 595_001);
    assert_eq!(first.commission_bps, Some(300));
    assert!(rewards[1].is_none());
}

#[test]
fn inflation_rewards_length_mismatch_is_error() {
    let body = r#"{"jsonrpc":"2.0","result":[null],"id":1}"#;
    assert!(parse_inflation_rewards(body, 2).is_err());
}

#[test]
fn status_derivation_covers_lifecycle() {
    let mut d = Delegation {
        voter: VOTER.to_string(),
        stake_lamports: 1,
        activation_epoch: 1003,
        deactivation_epoch: u64::MAX,
    };
    assert_eq!(derive_status(Some(&d), 1003), StakeStatus::Activating);
    assert_eq!(derive_status(Some(&d), 1004), StakeStatus::Active);
    d.deactivation_epoch = 1005;
    assert_eq!(derive_status(Some(&d), 1005), StakeStatus::Deactivating);
    assert_eq!(derive_status(Some(&d), 1006), StakeStatus::Inactive);
    assert_eq!(derive_status(None, 1003), StakeStatus::NotDelegated);
}

fn entry(label: &str, status: StakeStatus, validator: ValidatorStatus) -> Entry {
    Entry {
        label: label.to_string(),
        state: StakeState {
            lamports: 502_285_880,
            delegation: Some(Delegation {
                voter: VOTER.to_string(),
                stake_lamports: 499_997_717_120,
                activation_epoch: 1000,
                deactivation_epoch: u64::MAX,
            }),
        },
        status,
        validator: Some(validator),
        reward: Some(Reward {
            amount_lamports: 595_001,
            commission_bps: Some(300),
        }),
    }
}

#[test]
fn report_flags_delinquent_in_header() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let entries = vec![
        entry(
            "main",
            StakeStatus::Active,
            ValidatorStatus::Ok {
                commission_bps: 700,
            },
        ),
        entry(
            "backup",
            StakeStatus::Active,
            ValidatorStatus::Delinquent {
                commission_bps: 500,
            },
        ),
    ];
    let report = render_report(&entries, &e);
    assert!(
        report.contains("1 validator(s) DELINQUENT"),
        "report: {report}"
    );
    assert!(
        report.contains("[active] main: 500 SOL"),
        "report: {report}"
    );
    assert!(report.contains("DELINQUENT"), "report: {report}");
}

#[test]
fn report_stays_under_char_cap() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let entries: Vec<Entry> = (0..40)
        .map(|i| {
            entry(
                &format!("account-{i:02}"),
                StakeStatus::Active,
                ValidatorStatus::Ok {
                    commission_bps: 700,
                },
            )
        })
        .collect();
    let report = render_report(&entries, &e);
    assert!(
        report.len() <= REPORT_CHAR_CAP,
        "report length {} exceeds cap {}",
        report.len(),
        REPORT_CHAR_CAP
    );
    assert!(report.contains("omitted"), "report: {report}");
}
