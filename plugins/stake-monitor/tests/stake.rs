use std::collections::HashMap;

use stake_monitor::stake::{
    derive_status, parse_epoch_info, parse_inflation_rewards, parse_stake_account,
    parse_vote_status, render_payload, render_report, render_total_failure, Config, Delegation,
    Entry, EpochProgress, Reward, StakeState, StakeStatus, ValidatorStatus,
    DEFAULT_VOTE_LAG_WARN_SLOTS, REPORT_CHAR_CAP,
};

const STAKE_A: &str = "6ySLTQWEpCFKPYKfPaKYnhKzEccuqKafFEzfJVQ4Gifp";
const STAKE_B: &str = "CEHKNKfqQhHDWgiPrLNut2K3o5izJ1gpfSZ42CWBAv5n";
const VOTER: &str = "GHViLh5MgQDGDsuwXTHM9r8kQqEnQY6WsyLvGVYbFXAA";

// Field shapes below mirror live mainnet RPC replies captured during
// verification on 2026-07-18 (epoch 1003).

const EPOCH_INFO: &str = r#"{"jsonrpc":"2.0","result":{"absoluteSlot":433721729,"blockHeight":411783502,"epoch":1003,"slotIndex":425729,"slotsInEpoch":432000,"transactionCount":530368329172},"id":1}"#;

const HEAD_SLOT: u64 = 433_721_729;

/// A `getVoteAccounts` record in the `current` list, with `lastVote` supplied
/// verbatim so a test can also drop the field or send a never-voted zero.
fn vote_accounts_json(last_vote_field: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","result":{{"current":[{{"votePubkey":"{VOTER}","nodePubkey":"x","activatedStake":1,"commission":7,"inflationRewardsCommissionBps":700,"epochVoteAccount":true,"epochCredits":[],{last_vote_field}"rootSlot":433721697}}],"delinquent":[]}},"id":1}}"#
    )
}

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

fn cfg() -> Config {
    Config::from_section(&base_section()).expect("base section")
}

#[test]
fn config_parses_valid_section() {
    let cfg = Config::from_section(&base_section()).expect("valid section");
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
fn config_reads_vote_lag_warn_slots() {
    assert_eq!(cfg().vote_lag_warn_slots, DEFAULT_VOTE_LAG_WARN_SLOTS);
    let mut s = base_section();
    s.insert("vote_lag_warn_slots".to_string(), " 8 ".to_string());
    let tightened = Config::from_section(&s).expect("in-range override");
    assert_eq!(tightened.vote_lag_warn_slots, 8);
}

#[test]
fn config_rejects_out_of_range_vote_lag_warn_slots() {
    // Zero would flag every validator that is not exactly at the head, and a
    // value past the delinquency distance could only fire after the verdict.
    for bad in ["0", "129", "-1", "many"] {
        let mut s = base_section();
        s.insert("vote_lag_warn_slots".to_string(), bad.to_string());
        let err = Config::from_section(&s).unwrap_err();
        assert!(err.contains("vote_lag_warn_slots"), "`{bad}` gave: {err}");
    }
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
    let e = parse_epoch_info(EPOCH_INFO).expect("epoch info");
    assert_eq!(e.epoch, 1003);
    assert_eq!(e.absolute_slot, Some(HEAD_SLOT));
    let p = e.progress.expect("epoch progress");
    // 425729 of 432000 slots consumed.
    assert_eq!(p.pct(), 98);
    // 6271 slots left at 0.4 s per slot is well under two hours.
    assert!(p.hours_to_end() <= 2, "hours: {}", p.hours_to_end());
}

#[test]
fn epoch_info_still_requires_the_epoch_number() {
    // The delegation lifecycle is derived from the epoch number, so that one
    // field stays load-bearing while the rest of the reply degrades.
    let body = r#"{"jsonrpc":"2.0","result":{"absoluteSlot":433721729,"slotIndex":1,"slotsInEpoch":432000},"id":1}"#;
    let err = parse_epoch_info(body).unwrap_err();
    assert!(err.contains("epoch missing"), "err: {err}");
}

/// The lines a degraded epoch reply must never cost: delegation state, the
/// amount, the validator identity, and the reward.
fn assert_account_line_intact(report: &str) {
    assert!(
        report.contains("[active] main: 500 SOL"),
        "report: {report}"
    );
    assert!(report.contains("validator GHVi.."), "report: {report}");
    assert!(report.contains("last reward 0.001 SOL"), "report: {report}");
}

#[test]
fn epoch_info_degrades_without_head_slot() {
    let body = r#"{"jsonrpc":"2.0","result":{"epoch":1003,"slotIndex":425729,"slotsInEpoch":432000},"id":1}"#;
    let e = parse_epoch_info(body).expect("epoch info");
    assert_eq!(e.absolute_slot, None);

    // This validator would be flagged against a known head; with none, the
    // lag reads unknown instead of being invented in either direction.
    let entries = vec![entry(
        "main",
        StakeStatus::Active,
        ValidatorStatus::Ok {
            commission_bps: 700,
            last_vote_slot: Some(HEAD_SLOT - 5_000),
        },
    )];
    let report = render_report(&entries, &e, &cfg());
    assert!(report.contains("epoch 1003 at 98%"), "report: {report}");
    assert!(
        report.contains("ok, vote lag unknown, fee 7.0%"),
        "report: {report}"
    );
    assert!(!report.contains("BEHIND"), "report: {report}");
    assert_account_line_intact(&report);
}

#[test]
fn epoch_info_degrades_on_zero_length_epoch() {
    let body = r#"{"jsonrpc":"2.0","result":{"absoluteSlot":433721729,"epoch":1003,"slotIndex":0,"slotsInEpoch":0},"id":1}"#;
    let e = parse_epoch_info(body).expect("epoch info");
    assert!(e.progress.is_none());

    let report = render_report(
        &[entry("main", StakeStatus::Active, healthy_validator())],
        &e,
        &cfg(),
    );
    assert!(
        report.contains("epoch 1003 (progress unknown)"),
        "report: {report}"
    );
    assert!(!report.contains("h left"), "report: {report}");
    // The head slot survived, so the lag reading does too.
    assert!(report.contains("vote lag 2 slot(s)"), "report: {report}");
    assert_account_line_intact(&report);
}

#[test]
fn epoch_info_degrades_when_slot_index_overruns_the_epoch() {
    let body = r#"{"jsonrpc":"2.0","result":{"absoluteSlot":433721729,"epoch":1003,"slotIndex":432001,"slotsInEpoch":432000},"id":1}"#;
    let e = parse_epoch_info(body).expect("epoch info");
    assert!(e.progress.is_none());

    let report = render_report(
        &[entry("main", StakeStatus::Active, healthy_validator())],
        &e,
        &cfg(),
    );
    assert!(
        report.contains("epoch 1003 (progress unknown)"),
        "report: {report}"
    );
    assert!(report.contains("vote lag 2 slot(s)"), "report: {report}");
    assert_account_line_intact(&report);
}

#[test]
fn epoch_progress_rejects_counters_that_cannot_describe_an_epoch() {
    assert!(EpochProgress::new(0, 0).is_none());
    assert!(EpochProgress::new(1, 0).is_none());
    assert!(EpochProgress::new(432_001, 432_000).is_none());

    // The last slot of an epoch is still inside it, and reads as a full 100%.
    let end = EpochProgress::new(432_000, 432_000).expect("end-of-epoch progress");
    assert_eq!(end.pct(), 100);
    assert_eq!(end.hours_to_end(), 0);

    // Counters large enough to overflow a u64 multiplication stay bounded.
    let huge = EpochProgress::new(u64::MAX, u64::MAX).expect("equal-counter progress");
    assert_eq!(huge.pct(), 100);
}

#[test]
fn stake_account_parses_active_delegation() {
    let body = stake_account_json("18446744073709551615");
    let s = parse_stake_account(&body).expect("stake account");
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
    let body = vote_accounts_json(r#""lastVote":433721727,"#);
    assert_eq!(
        parse_vote_status(&body, VOTER).unwrap(),
        ValidatorStatus::Ok {
            commission_bps: 700,
            last_vote_slot: Some(433_721_727),
        }
    );
}

#[test]
fn vote_status_detects_delinquent_and_unknown() {
    let delinquent = format!(
        r#"{{"jsonrpc":"2.0","result":{{"current":[],"delinquent":[{{"votePubkey":"{VOTER}","commission":5,"activatedStake":1,"epochCredits":[],"lastVote":433719000}}]}},"id":1}}"#
    );
    assert_eq!(
        parse_vote_status(&delinquent, VOTER).unwrap(),
        ValidatorStatus::Delinquent {
            commission_bps: 500,
            last_vote_slot: Some(433_719_000),
        }
    );
    let empty = r#"{"jsonrpc":"2.0","result":{"current":[],"delinquent":[]},"id":1}"#;
    assert_eq!(
        parse_vote_status(empty, VOTER).unwrap(),
        ValidatorStatus::Unknown
    );
}

#[test]
fn vote_lag_measures_distance_to_head() {
    let head = Some(HEAD_SLOT);
    let healthy =
        parse_vote_status(&vote_accounts_json(r#""lastVote":433721727,"#), VOTER).unwrap();
    assert_eq!(healthy.vote_lag(head), Some(2));
    assert!(!healthy.is_behind(head, DEFAULT_VOTE_LAG_WARN_SLOTS));

    let lagging =
        parse_vote_status(&vote_accounts_json(r#""lastVote":433721668,"#), VOTER).unwrap();
    assert_eq!(lagging.vote_lag(head), Some(61));
    assert!(lagging.is_behind(head, DEFAULT_VOTE_LAG_WARN_SLOTS));
    // The same lag is quiet under a threshold the operator raised past it.
    assert!(!lagging.is_behind(head, 100));

    // The warn threshold itself is still quiet; only a lag past it speaks up.
    let at_threshold = ValidatorStatus::Ok {
        commission_bps: 700,
        last_vote_slot: Some(HEAD_SLOT - DEFAULT_VOTE_LAG_WARN_SLOTS),
    };
    assert!(!at_threshold.is_behind(head, DEFAULT_VOTE_LAG_WARN_SLOTS));

    // The head is read before the vote account, so a validator can legitimately
    // report a slot ahead of it. That is zero lag, never a wrapped u64.
    let ahead = ValidatorStatus::Ok {
        commission_bps: 700,
        last_vote_slot: Some(HEAD_SLOT + 5),
    };
    assert_eq!(ahead.vote_lag(head), Some(0));
}

#[test]
fn vote_lag_is_unknown_on_degraded_records() {
    // Field absent from the vote record.
    let missing = parse_vote_status(&vote_accounts_json(""), VOTER).unwrap();
    assert_eq!(
        missing,
        ValidatorStatus::Ok {
            commission_bps: 700,
            last_vote_slot: None,
        }
    );
    assert_eq!(missing.vote_lag(Some(HEAD_SLOT)), None);
    assert!(!missing.is_behind(Some(HEAD_SLOT), DEFAULT_VOTE_LAG_WARN_SLOTS));

    // A vote account that has never voted reports slot 0, which is an absent
    // vote rather than a lag of the whole chain history.
    let never_voted = parse_vote_status(&vote_accounts_json(r#""lastVote":0,"#), VOTER).unwrap();
    assert_eq!(never_voted.vote_lag(Some(HEAD_SLOT)), None);

    // A validator missing from both lists carries no lag either.
    assert_eq!(ValidatorStatus::Unknown.vote_lag(Some(HEAD_SLOT)), None);

    // Neither does a healthy record with no head slot to measure against.
    let healthy =
        parse_vote_status(&vote_accounts_json(r#""lastVote":433721727,"#), VOTER).unwrap();
    assert_eq!(healthy.vote_lag(None), None);
    assert!(!healthy.is_behind(None, DEFAULT_VOTE_LAG_WARN_SLOTS));
}

#[test]
fn delinquent_validator_is_not_double_flagged_as_behind() {
    let delinquent = ValidatorStatus::Delinquent {
        commission_bps: 500,
        last_vote_slot: Some(HEAD_SLOT - 2729),
    };
    assert_eq!(delinquent.vote_lag(Some(HEAD_SLOT)), Some(2729));
    assert!(!delinquent.is_behind(Some(HEAD_SLOT), DEFAULT_VOTE_LAG_WARN_SLOTS));
}

#[test]
fn inflation_rewards_parse_live_shape_with_null_commission() {
    let body = r#"{"jsonrpc":"2.0","result":[{"amount":595001,"commission":null,"commissionBps":300,"effectiveSlot":433296296,"epoch":1002,"postBalance":2025175995},null],"id":1}"#;
    let rewards = parse_inflation_rewards(body, 2).expect("inflation rewards");
    let first = rewards[0].expect("first reward");
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

fn healthy_validator() -> ValidatorStatus {
    ValidatorStatus::Ok {
        commission_bps: 700,
        last_vote_slot: Some(HEAD_SLOT - 2),
    }
}

#[test]
fn report_flags_delinquent_in_header() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let entries = vec![
        entry("main", StakeStatus::Active, healthy_validator()),
        entry(
            "backup",
            StakeStatus::Active,
            ValidatorStatus::Delinquent {
                commission_bps: 500,
                last_vote_slot: Some(HEAD_SLOT - 2729),
            },
        ),
    ];
    let report = render_report(&entries, &e, &cfg());
    assert!(
        report.contains("1 validator(s) DELINQUENT"),
        "report: {report}"
    );
    assert!(
        report.contains("[active] main: 500 SOL"),
        "report: {report}"
    );
    assert!(
        report.contains("DELINQUENT, vote lag 2729 slot(s)"),
        "report: {report}"
    );
}

#[test]
fn report_shows_epoch_progress_and_healthy_vote_lag() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let report = render_report(
        &[entry("main", StakeStatus::Active, healthy_validator())],
        &e,
        &cfg(),
    );
    assert!(report.contains("epoch 1003 at 98%"), "report: {report}");
    assert!(
        report.contains("ok, vote lag 2 slot(s), fee 7.0%"),
        "report: {report}"
    );
    assert!(!report.contains("BEHIND"), "report: {report}");
    assert!(!report.contains("DELINQUENT"), "report: {report}");
}

#[test]
fn report_flags_lagging_validator_before_delinquency() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let entries = vec![
        entry("main", StakeStatus::Active, healthy_validator()),
        entry(
            "backup",
            StakeStatus::Active,
            ValidatorStatus::Ok {
                commission_bps: 700,
                last_vote_slot: Some(HEAD_SLOT - 61),
            },
        ),
    ];
    let report = render_report(&entries, &e, &cfg());
    assert!(report.contains("1 validator(s) BEHIND"), "report: {report}");
    assert!(
        report.contains("ok, vote lag 61 slot(s) BEHIND"),
        "report: {report}"
    );
    // The lagging validator is still current, so delinquency stays silent.
    assert!(!report.contains("DELINQUENT"), "report: {report}");
}

#[test]
fn configured_warn_threshold_drives_the_behind_flag() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let lagging = entry(
        "main",
        StakeStatus::Active,
        ValidatorStatus::Ok {
            commission_bps: 700,
            last_vote_slot: Some(HEAD_SLOT - 61),
        },
    );
    let mut s = base_section();
    s.insert("vote_lag_warn_slots".to_string(), "100".to_string());
    let relaxed = Config::from_section(&s).expect("raised threshold");

    // 61 slots trips the 32-slot default and stays quiet at 100.
    let entries = std::slice::from_ref(&lagging);
    assert!(render_report(entries, &e, &cfg()).contains("BEHIND"));
    let quiet = render_report(entries, &e, &relaxed);
    assert!(!quiet.contains("BEHIND"), "report: {quiet}");
    assert!(
        quiet.contains("ok, vote lag 61 slot(s), fee 7.0%"),
        "report: {quiet}"
    );
}

#[test]
fn report_never_invents_a_lag_number() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let entries = vec![
        entry(
            "main",
            StakeStatus::Active,
            ValidatorStatus::Ok {
                commission_bps: 700,
                last_vote_slot: None,
            },
        ),
        entry("backup", StakeStatus::Active, ValidatorStatus::Unknown),
    ];
    let report = render_report(&entries, &e, &cfg());
    assert!(
        report.contains("ok, vote lag unknown, fee 7.0%"),
        "report: {report}"
    );
    assert!(!report.contains("vote lag 0"), "report: {report}");
    // An unresolved validator says so once and claims no lag at all.
    assert!(
        report.contains("validator GHVi.. not found"),
        "report: {report}"
    );
    assert!(!report.contains("not found, vote lag"), "report: {report}");
    assert!(!report.contains("BEHIND"), "report: {report}");
}

#[test]
fn report_stays_under_char_cap() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let report = render_report(&crowded_entries(), &e, &cfg());
    assert!(
        report.len() <= REPORT_CHAR_CAP,
        "report length {} exceeds cap {}",
        report.len(),
        REPORT_CHAR_CAP
    );
    assert!(report.contains("omitted"), "report: {report}");
}

fn crowded_entries() -> Vec<Entry> {
    (0..40)
        .map(|i| {
            entry(
                &format!("account-{i:02}"),
                StakeStatus::Active,
                healthy_validator(),
            )
        })
        .collect()
}

#[test]
fn payload_cap_covers_the_data_issues_line() {
    // The failed-read line is part of what the agent receives, so a long
    // report plus a long pile of RPC errors still has to fit the cap.
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let issues: Vec<String> = (0..12)
        .map(|i| format!("account-{i:02} validator: request failed: connection reset by peer"))
        .collect();
    let payload = render_payload(&crowded_entries(), &e, &cfg(), &issues);
    assert!(
        payload.len() <= REPORT_CHAR_CAP,
        "payload length {} exceeds cap {}",
        payload.len(),
        REPORT_CHAR_CAP
    );
    // Both halves stay readable: the header and its leading rows survive, and
    // the issues that did not fit are counted rather than dropped in silence.
    assert!(
        payload.contains("Stake: 40 account(s)"),
        "payload: {payload}"
    );
    assert!(
        payload.contains("[active] account-00"),
        "payload: {payload}"
    );
    assert!(
        payload.contains("more line(s) omitted"),
        "payload: {payload}"
    );
    assert!(
        payload.contains("Data issues: account-00 validator: request failed"),
        "payload: {payload}"
    );
    assert!(
        !payload.contains("account-11 validator"),
        "payload: {payload}"
    );
    assert!(payload.contains(" more)"), "payload: {payload}");
}

#[test]
fn payload_keeps_a_short_report_and_its_issues_whole() {
    let e = parse_epoch_info(EPOCH_INFO).unwrap();
    let entries = vec![entry("main", StakeStatus::Active, healthy_validator())];
    let report = render_report(&entries, &e, &cfg());
    assert_eq!(render_payload(&entries, &e, &cfg(), &[]), report);

    let issues = vec!["backup: stake account not found on chain".to_string()];
    let payload = render_payload(&entries, &e, &cfg(), &issues);
    assert_eq!(
        payload,
        format!("{report}\nData issues: backup: stake account not found on chain")
    );
    assert!(!payload.contains("omitted"), "payload: {payload}");
}

#[test]
fn total_failure_text_stays_inside_the_issue_budget() {
    // Every stake account read failed and each upstream message is long.
    let issues: Vec<String> = (0..12)
        .map(|i| format!("stake-{i}: rpc error {}", "x".repeat(120)))
        .collect();
    let text = render_total_failure(&issues);

    assert!(text.starts_with("every stake account read failed: "));
    // The failure path is bounded like the delivered payload, so server-controlled
    // error text cannot flood the agent context.
    assert!(
        text.len() <= REPORT_CHAR_CAP,
        "failure text {} chars, cap {REPORT_CHAR_CAP}",
        text.len()
    );
    assert!(
        text.contains("more"),
        "dropped issues are not counted: {text}"
    );
}

#[test]
fn total_failure_text_states_a_single_short_issue_in_full() {
    let issues = vec!["stake-a: http 503".to_string()];
    assert_eq!(
        render_total_failure(&issues),
        "every stake account read failed: stake-a: http 503"
    );
}
