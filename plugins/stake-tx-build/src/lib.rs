//! ZeroClaw WIT tool plugin: `stake_tx_build`.
//!
//! Builds an unsigned legacy Solana transaction that delegates or
//! deactivates one of the operator's allowlisted stake accounts, returned
//! as base64 with a human summary for the approval gate. The genesis hash the
//! endpoint reports is checked against the pinned cluster before anything is
//! built, which catches an honest endpoint on the wrong chain. The
//! plugin holds no keys and cannot sign or submit; a human wallet does both.
//! The pure core lives in [`txbuild`] with no wasm dependency, so it compiles
//! and tests on the host with a plain `cargo test`; the wasm component reuses
//! the same logic through the shim below.
//!
//! Build:  rustup target add wasm32-wasip2
//!         cargo build --target wasm32-wasip2 --release

pub mod txbuild;

#[cfg(target_family = "wasm")]
mod component {
    wit_bindgen::generate!({
        path: "../../wit/v0",
        world: "tool-plugin",
        features: ["plugins-wit-v0"],
    });

    use std::collections::HashMap;
    use std::time::Duration;

    use crate::txbuild::{self, build_transaction, parse_action, validate_vote, Config};
    use exports::zeroclaw::plugin::plugin_info::Guest as PluginInfo;
    use exports::zeroclaw::plugin::tool::{Guest as Tool, ToolResult};
    use zeroclaw::plugin::logging::{
        log_record, LogLevel, PluginAction, PluginEvent, PluginOutcome,
    };

    struct StakeTxBuild;

    const TOOL_NAME: &str = "stake_tx_build";

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ExecuteArgs {
        action: String,
        stake_account: String,
        #[serde(default)]
        vote_account: Option<String>,
        #[serde(rename = "__config", default)]
        config: HashMap<String, String>,
    }

    impl PluginInfo for StakeTxBuild {
        fn plugin_name() -> String {
            env!("CARGO_PKG_NAME").to_string()
        }

        fn plugin_version() -> String {
            env!("CARGO_PKG_VERSION").to_string()
        }
    }

    impl Tool for StakeTxBuild {
        fn name() -> String {
            TOOL_NAME.to_string()
        }

        fn description() -> String {
            "Builds an UNSIGNED Solana stake transaction (delegate or deactivate) for a \
             stake account from the configured allowlist, returned as base64 for the \
             operator to review and sign in their own wallet. Delegation targets come \
             from a second operator allowlist. This component holds no key material and \
             cannot sign or submit anything."
                .to_string()
        }

        fn parameters_schema() -> String {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["delegate", "deactivate"],
                        "description": "Transaction to build: delegate stake to a vote account, or deactivate the stake."
                    },
                    "stake_account": {
                        "type": "string",
                        "description": "Stake account label or pubkey from the configured allowlist."
                    },
                    "vote_account": {
                        "type": "string",
                        "description": "Delegation target vote account pubkey; required for delegate and must be in the configured allowed_vote_accounts allowlist. Omit for deactivate."
                    }
                },
                "required": ["action", "stake_account"]
            })
            .to_string()
        }

        fn execute(args: String) -> Result<ToolResult, String> {
            let parsed: ExecuteArgs = match serde_json::from_str(&args) {
                Ok(a) => a,
                Err(e) => return fail(format!("invalid arguments: {e}")),
            };

            let cfg = match Config::from_section(&parsed.config) {
                Ok(c) => c,
                Err(e) => return fail(format!("config error: {e}")),
            };

            let action = match parse_action(&parsed.action) {
                Ok(a) => a,
                Err(e) => return fail(e),
            };

            let stake = match cfg.resolve_stake(&parsed.stake_account) {
                Ok(s) => s,
                Err(e) => return fail(e),
            };

            let vote = match validate_vote(&cfg, action, parsed.vote_account.as_deref()) {
                Ok(v) => v,
                Err(e) => return fail(e),
            };

            let timeout = Duration::from_secs(cfg.timeout_secs);

            // Cluster identity gate: one getGenesisHash read per invocation,
            // before any transaction bytes exist. An endpoint reporting a
            // genesis other than the pinned cluster aborts the call here,
            // which catches an honest endpoint on the wrong chain. An
            // endpoint that echoes the expected hash still passes; the limits
            // are spelled out in the README threat model.
            let genesis = match post_json(&cfg.rpc_url, &txbuild::genesis_hash_body(), timeout)
                .and_then(|b| txbuild::parse_genesis_hash(&b))
            {
                Ok(g) => g,
                Err(e) => return fail(format!("cluster check failed: {e}")),
            };
            if let Err(e) = txbuild::verify_cluster(cfg.cluster, &genesis) {
                return fail(e);
            }

            // Durable path: the blockhash slot is filled from the nonce
            // account state instead of the recent blockhash queue.
            let blockhash = match &cfg.nonce {
                Some(nonce) => {
                    match post_json(
                        &cfg.rpc_url,
                        &txbuild::nonce_account_body(&nonce.account),
                        timeout,
                    )
                    .and_then(|b| txbuild::parse_nonce_blockhash(&b))
                    {
                        Ok(h) => h,
                        Err(e) => return fail(format!("nonce account read failed: {e}")),
                    }
                }
                None => {
                    match post_json(&cfg.rpc_url, &txbuild::latest_blockhash_body(), timeout)
                        .and_then(|b| txbuild::parse_latest_blockhash(&b))
                    {
                        Ok(h) => h,
                        Err(e) => return fail(format!("blockhash fetch failed: {e}")),
                    }
                }
            };

            let built = match build_transaction(&cfg, action, stake, vote.as_deref(), blockhash) {
                Ok(b) => b,
                Err(e) => return fail(format!("transaction build failed: {e}")),
            };

            emit(
                PluginAction::Complete,
                PluginOutcome::Success,
                &format!(
                    "built unsigned {} transaction for `{}`",
                    action.as_str(),
                    stake.label
                ),
            );

            Ok(ToolResult {
                success: true,
                output: built.output(),
                error: None,
            })
        }
    }

    fn post_json(url: &str, body: &str, timeout: Duration) -> Result<String, String> {
        let response = waki::Client::new()
            .post(url)
            .connect_timeout(timeout)
            .header("Content-Type", "application/json")
            .body(body.as_bytes().to_vec())
            .send()
            .map_err(|e| format!("request failed: {e}"))?;
        let status = response.status_code();
        let bytes = response
            .body()
            .map_err(|e| format!("response read failed: {e}"))?;
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }
        String::from_utf8(bytes).map_err(|_| "response is not UTF-8".to_string())
    }

    fn fail(message: String) -> Result<ToolResult, String> {
        emit(PluginAction::Fail, PluginOutcome::Failure, &message);
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(message),
        })
    }

    fn emit(action: PluginAction, outcome: PluginOutcome, message: &str) {
        log_record(
            LogLevel::Info,
            &PluginEvent {
                function_name: "stake_tx_build::tool::execute".to_string(),
                action,
                outcome: Some(outcome),
                duration_ms: None,
                attrs: None,
                message: message.to_string(),
            },
        );
    }

    export!(StakeTxBuild);
}
