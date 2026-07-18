# stake-monitor

A ZeroClaw **WIT component** tool plugin. It implements the `tool-plugin` world
from `wit/v0` and compiles to a `wasm32-wasip2` component. It exposes one tool,
`stake_monitor`, that reads the on-chain status of the operator's own stake
accounts over Solana JSON-RPC and shapes the result for a chat agent. Read-only:
the plugin holds no keys and signs nothing.

## What it does

Point the tool at an allowlist of stake accounts the operator already controls
and at a Solana RPC endpoint. It returns a short briefing, one line per account,
sized to fit an agent turn without flooding the context.

Each account line carries the delegation lifecycle status, how much SOL is
delegated, the validator's health, and whether the account earned a reward in
the previous epoch. The lifecycle status is one of `activating`, `active`,
`deactivating`, or `inactive`; an account with no delegation is reported as such.
A header line counts the accounts, sums the delegated stake, names the current
epoch with a rough "hours left" hint, and raises a `DELINQUENT` flag when any
validator is behind.

The reading is assembled from a few narrow RPC calls. `getEpochInfo` gives the
epoch and the time-left hint. `getAccountInfo` with `jsonParsed` yields the
delegation. `getVoteAccounts` is filtered by `votePubkey` so the reply is a
single validator record instead of the whole roster. `getInflationReward` for
the prior epoch supplies the last reward. The optional `account` argument selects
one allowlisted entry by label or pubkey; omit it to report every configured
account.

## Custody tier

Read-only. The plugin holds no private keys and signs no transactions; it only
reads public chain state through the operator's RPC endpoint. Nothing it exposes
can move funds, redelegate, deactivate a stake account, or change an authority.
The worst a malicious argument can achieve is to read the status of an account
that the operator already placed on the allowlist.

## Config keys

The operator configures the plugin by name; the host resolves that one section
and hands the plugin a flat `string -> string` map, injected as `__config`. This
only happens because the manifest requests the `config_read` permission. The
plugin refuses to run without a configured allowlist.

| Key | Required | Default | Meaning |
|---|---|---|---|
| `stake_accounts` | yes | — | Comma-separated allowlist. Each entry is `label:pubkey`, or a bare pubkey that is auto-labelled `stake1`, `stake2`, and so on. At least one valid base58 pubkey is required. |
| `rpc_url` | yes | — | The operator's own Solana JSON-RPC endpoint. Must be `https://`. A trailing slash is trimmed. |
| `timeout_secs` | no | `10` | Per-request connect timeout in seconds, bounded to 1 through 60. |

## Threat model

- **Allowlist only.** The `account` argument can select an entry that is already
  configured; it can never introduce a fresh address. `resolve_account` rejects
  anything outside the list.
- **No on-chain discovery.** There is deliberately no `getProgramAccounts` scan
  to enumerate stake accounts. That call is heavy on public RPC and would widen
  what the tool can read, so an explicit allowlist is both cheaper and tighter.
- **Fail-closed config.** An unknown config key is a hard error rather than a
  silently ignored typo, which surfaces a misspelled key immediately. `rpc_url`
  must be `https://`, and `timeout_secs` is bounded.
- **Authoritative commission.** Commission is read from `commissionBps`, the
  authoritative field. The legacy `commission` percentage can be null even when a
  reward exists, so it is used only as a fallback.
- **Bounded output.** The report is capped near 900 characters, roughly 200
  tokens, so a scheduled briefing can never flood the agent's context.
- **Narrow egress.** The `http_client` permission reaches only the configured
  `rpc_url`. No other host is contacted, and the pure core in `src/stake.rs` does
  no I/O at all.

## A worked example

A single active account, reported live against mainnet:

```
Stake: 1 account(s), 500 SOL delegated, epoch 1004 (~47 h left).
[active] main: 500 SOL, validator GHVi.. ok, fee 100.0%, no reward last epoch
```

The header sums the position and dates it to an epoch. The account line is where
the tool earns its keep: it read the validator's fee as `100.0%` and, in the same
breath, showed `no reward last epoch`. Those two facts explain each other. A
validator taking full commission leaves the staker with nothing, and the tool
surfaces that from one line without anyone opening an explorer. An operator
reading this briefing knows to redelegate.

## Prompt-injection test

Suppose the surrounding data stream tries to steer the agent into checking an
address the operator never configured. The tool is asked for a pubkey outside the
allowlist:

```
tool:   stake_monitor
args:   { "account": "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM" }

result: success=false, error: stake account `9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM` is not in the configured allowlist; known labels: main
```

The tool fails closed. It resolves the argument against the allowlist before any
RPC call goes out, so an unrecognized address returns `success=false` and no
network request is made on its behalf. The only names the tool will act on are
the ones the operator configured, and the error names the allowlist labels so a
legitimate typo is easy to correct.

## Layout (the reference format)

```
src/stake.rs    # pure logic, no wasm deps — host-testable with `cargo test`
src/lib.rs      # thin #[cfg(target_family = "wasm")] component shim
tests/stake.rs  # host-run integration tests over the pure core
manifest.toml   # name, version, wasm_path, capabilities, permissions
```

## Build and test

```bash
cargo test                                        # host tests, no wasm needed
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release      # the component
cp target/wasm32-wasip2/release/stake_monitor.wasm stake_monitor.wasm
```

The pure core in `src/stake.rs` carries no wasm dependency, so config parsing,
response parsing, status derivation, and report rendering all run under a plain
host `cargo test`. Field shapes in those tests mirror live mainnet replies
captured during verification.

## Install

```bash
zeroclaw plugin install stake-monitor
```

or copy this directory (the `.wasm` next to its `manifest.toml`) into your
configured plugins dir, then enable plugins:

```toml
[plugins]
enabled = true
```

Configuration is required here, since the plugin refuses to run without an
allowlist. Supply the allowlist and endpoint in the plugin's own config record,
which is the section the `config_read` permission unlocks:

```toml
[[plugins.entries]]
name = "stake-monitor"

[plugins.entries.config]
stake_accounts = "main:6ySLTQWEpCFKPYKfPaKYnhKzEccuqKafFEzfJVQ4Gifp"
rpc_url = "https://your-own-rpc.example.com"
timeout_secs = "10"
```

Run the agent with a build that includes a compiler backend, e.g.
`--features plugins-wasm,plugins-wasm-cranelift`. For runtime-only hosts
(`--features plugins-wasm`), precompile with a matching wasmtime:
`wasmtime compile --target <triple> stake_monitor.wasm -o stake_monitor.cwasm`
and point `wasm_path` at the `.cwasm`.

## License

Dual-licensed under MIT or Apache-2.0, matching the repository convention.
