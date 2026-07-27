# stake-tx-build

A ZeroClaw **WIT component** tool plugin. It implements the `tool-plugin` world
from `wit/v0` and compiles to a `wasm32-wasip2` component. The exported tool is
`stake_tx_build`: it turns an operator's intent to delegate or deactivate stake
into an unsigned transaction that a human still has to sign.

## What it does

A call produces an unsigned legacy Solana transaction, either a `delegate` or a
`deactivate`, for a stake account named in the operator's allowlist. The output
opens with a plain-language summary for the approval gate and closes with the
transaction as base64 on its own labeled line. A person reads the summary and
signs in their own wallet. The plugin signs nothing and submits nothing; no
private key ever reaches it.

Every instruction byte is assembled by hand, without `solana-sdk`: the base58
and base64 codecs, the compact-u16 length prefixes, the legacy message header,
and the bincode discriminants for each program instruction. That keeps the wasm
artifact small and the transaction layout auditable down to the byte.

Before any of that, the plugin asks the configured endpoint for its genesis
hash and compares it against the pinned cluster, which defaults to
mainnet-beta. The check costs one extra read per call, and a mismatch aborts
before a single transaction byte exists. A URL alone says nothing about the
chain behind it, so an endpoint that answers honestly while serving devnet or
testnet is caught by its own reply. The threat model below states the limits of
that check.

A durable nonce is optional. When the config sets `nonce_account` together with
`nonce_authority`, the first instruction becomes `AdvanceNonceAccount` and the
transaction draws its blockhash from the nonce account state, so it does not go
stale while it waits in an approval queue. Without a nonce the tool reads a
fresh blockhash and the summary warns that the signing window is short.

The builder runs no live validator health check before it delegates, and that
gap is deliberate. Target safety is enforced by the vote account allowlist, and
live health belongs to the separate `stake-monitor` tool.

## Config keys

The operator configures the plugin by name; the host resolves that one section
and hands the plugin a flat `string -> string` map, injected as `__config`. This
only happens because the manifest requests the `config_read` permission.

| Key | Default | Meaning |
|---|---|---|
| `stake_accounts` | (required) | Comma-separated allowlist. Each entry is `label:pubkey` or a bare pubkey. The only stake accounts the tool will act on. |
| `authority` | (required) | Fee payer and stake authority **public key**. Never a private key. |
| `rpc_url` | (required) | HTTPS Solana RPC endpoint read for a blockhash. Must start with `https://`. |
| `cluster` | `mainnet-beta` | Cluster the endpoint's reported genesis hash must match. Stays on `mainnet-beta` unless the operator names another public cluster; the alternatives are `devnet` and `testnet`. Any other value is rejected. |
| `allowed_vote_accounts` | (empty) | Comma-separated allowlist of vote accounts eligible as delegation targets. Empty disables `delegate` entirely. |
| `nonce_account` | (unset) | Durable nonce account pubkey. Set with `nonce_authority` to build a transaction that survives an approval queue. |
| `nonce_authority` | (unset) | Authority pubkey for the durable nonce. Must be set together with `nonce_account`. |
| `timeout_secs` | `10` | Connect timeout for the RPC call, between 1 and 60. |

Upgrading from a version without the cluster gate: `cluster` now defaults to
`mainnet-beta`, so a config whose `rpc_url` points at devnet or testnet fails
on every call until the section adds the matching key, `cluster = "devnet"` or
`cluster = "testnet"`. A config already on mainnet needs no change.

The call itself takes an `action` of `delegate` or `deactivate` and a
`stake_account` given as a label or pubkey from the allowlist. A `delegate`
additionally requires a `vote_account`, which must appear in
`allowed_vote_accounts`; passing `vote_account` to a `deactivate` is rejected.

## Layout (the reference format)

```
src/txbuild.rs   # pure logic, no wasm deps; host-testable with cargo test
src/lib.rs       # thin #[cfg(target_family = "wasm")] component shim
tests/           # host-run integration tests over the pure core
manifest.toml    # name, version, wasm_path, capabilities, permissions
```

## Build and test

```bash
cargo test                                        # host tests, no wasm needed
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release      # the component
cp target/wasm32-wasip2/release/stake_tx_build.wasm stake_tx_build.wasm
```

## Custody tier

This tool builds unsigned transactions and holds no keys. Its only outbound
calls are reads against the operator's own RPC endpoint: the cluster genesis
hash, then a blockhash or the nonce account state. Everything it produces is
inert until a human signs it in a wallet the plugin never sees.

The manifest asks for exactly two permissions: `http_client` for those RPC
reads and `config_read` for its own jailed config section. Neither one can sign
or spend, and the plugin requests nothing beyond them.

## Threat model

The tool assumes the agent driving it may be under prompt injection. Its
defenses do not depend on the agent behaving.

- **Stake accounts are allowlisted.** `stake_account` must resolve to an entry
  in `stake_accounts`. An address the operator never configured is refused, and
  the tool builds nothing.
- **Delegation targets are allowlisted, and off by default.** `delegate` stays
  disabled until the operator sets `allowed_vote_accounts`. Even then the target
  must be on that list.
- **The authority is a public key, never a secret.** `authority` names the fee
  payer and stake authority. No config field accepts private-key material, so
  there is nothing to leak.
- **The endpoint has to report its chain, and the report is checked.** Every
  call reads `getGenesisHash` and compares the reply against the pinned
  `cluster`. A mismatch refuses; a reply that is absent or malformed refuses
  the same way. What this catches is an honest endpoint on the wrong chain: an
  `rpc_url` left pointing at devnet or testnet, or a `cluster` typo that would
  otherwise have bytes assembled against a cluster the operator never meant.
  What it does not catch: a hostile proxy answers `getGenesisHash` with the
  mainnet constant and passes, because nothing binds that reply to the
  blockhash that follows, and a chain forked from mainnet inherits mainnet's
  genesis hash, so it answers correctly too. Trust in the endpoint itself stays
  with the operator who configured it.

  Host tests cover the decision logic in the pure core: the reply parse, and
  the refusal on either a mismatch or a malformed reply. That the gate runs
  before any transaction byte is assembled lives in the wasm shim in
  `src/lib.rs`, alongside the blockhash and nonce reads, and is not exercised
  by `cargo test`.
- **Unknown config keys fail closed.** A typo such as `allowed_vote_account`
  does not silently weaken an allowlist; parsing stops with an error. The same
  holds for `cluster`: `mainnet` is not `mainnet-beta`, and the near miss is
  rejected instead of resolved.
- **Unexpected arguments fail closed.** The argument schema rejects any field it
  does not recognize, so a smuggled parameter aborts the call.

The tool does not check whether a validator is healthy or delinquent before it
delegates. That judgment stays with the operator's allowlist and with
`stake-monitor`. This builder's job ends at an unsigned transaction that is
correct and confined to the allowlist.

## A worked example

A `deactivate` call against the stake account labeled `main`, with no durable
nonce configured, returns:

```
Unsigned deactivate transaction for stake `main`; amount not read by this builder; fresh blockhash: sign and submit within roughly 60 to 90 seconds.
unsigned_tx_base64: AQAAAA...BAUAAAA
```

Decoded byte for byte, the deactivation instruction verifies against the Stake
program: program `Stake11111111111111111111111111111111111111`, accounts
`[stake, clock, authority]`, data `05000000`. Nothing about the staked amount
appears anywhere, because the builder never reads it.

The same call against an `rpc_url` that turns out to be devnet returns no
transaction at all:

```
success=false
error: cluster mismatch: rpc_url reports genesis `EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG`, not mainnet-beta `5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d`
```

## Prompt-injection test

An injected instruction tells the agent to redirect the operator's stake to a
validator the attacker controls, and to deactivate a stake account the operator
never configured. Both calls fail closed. The tool builds zero transactions.

The `delegate`, carrying a vote account that is not among the operator's
approved validators:

```
success=false
error: vote account `5btPEka74QyPuY7Yj6wks8oHHLFMqHWFiRraSLzUB5Ev` is not in the configured allowed_vote_accounts allowlist
```

The `deactivate`, naming a stake account the config never mentions:

```
success=false
error: stake account `Eu9abQ8jj3Dj6MrN8oW6wuyosLrMmA8ZwWWnifCKTvmp` is not in the configured allowlist; known labels: main
```

An agent pushed by an injection runs into two independent allowlists at once,
and neither of them takes its contents from the model. The delegation target
has to be a vote account the operator wrote into `allowed_vote_accounts`; on a
config that never set that key, `delegate` refuses a step earlier still, with
`delegate is disabled`. The stake account has to be one the operator named, and
the error names the allowlist labels so a legitimate typo is easy to correct.

## Install

```bash
zeroclaw plugin install stake-tx-build
```

or copy this directory (the `.wasm` next to its `manifest.toml`) into your
configured plugins dir, then enable plugins and configure the keys above:

```toml
[plugins]
enabled = true
```

Run the agent with a build that includes a compiler backend, e.g.
`--features plugins-wasm,plugins-wasm-cranelift`. For runtime-only hosts
(`--features plugins-wasm`), precompile with a matching wasmtime:
`wasmtime compile --target <triple> stake_tx_build.wasm -o stake_tx_build.cwasm`
and point `wasm_path` at the `.cwasm`.
