# lending-health

A ZeroClaw **WIT component** tool plugin that reports how close an operator's
DeFi borrow positions sit to liquidation. It implements the `tool-plugin` world
from `wit/v0` and compiles to a `wasm32-wasip2` component. The tool is named
`lending_health`.

## What it does

The tool answers one question for a set of operator-owned wallets: are any
borrow positions drifting toward liquidation? A scheduled briefing or a chat
query can then surface a margin problem while there is still time to act on it.

Two protocols are covered by different data paths. Kamino positions come from
the public Kamino REST API (`GET /portfolio/{wallet}`). MarginFi positions come
straight from on-chain account state, decoded from a `getProgramAccounts` read
over the operator's own Solana JSON-RPC endpoint, with the maintenance-weighted
asset and liability values read at fixed byte offsets. For each position the
tool computes current LTV against that market's liquidation LTV.

Two thresholds split the risk scale. Below `warn_ltv` a position reads `OK`. At
or above `warn_ltv` it reads `WARN`, and at or above `critical_ltv` it escalates
to `CRITICAL`. The report lists one line per position, worst risk first, and the
whole thing is capped near 200 tokens so a recurring briefing never floods the
agent context. Positions that the Kamino indexer has not refreshed against the
current price feed carry a staleness hint, so an old snapshot is never presented
as live.

Drift is deliberately out of scope. Its API does not expose a current health or
liquidation figure for an open position, so the tool would have to reconstruct
one and risk reporting a number that is simply wrong. Reporting nothing beats
reporting a guess about someone's liquidation distance.

## Config keys

The operator configures the plugin by name in `config.toml`; the host resolves
that one section and hands the plugin a flat `string -> string` map, injected as
`__config`. The plugin can never read the global config or another plugin's
section.

| Key | Default | Meaning |
|---|---|---|
| `wallets` | (required) | Comma-separated allowlist. Each entry is `label:pubkey` or a bare pubkey. The tool refuses to run with no wallet configured. |
| `rpc_url` | (none) | Solana JSON-RPC endpoint used for the MarginFi read. Required whenever `marginfi` is enabled. Must be `https://`. |
| `kamino_api_base` | `https://api.kamino.finance` | Base URL for the Kamino REST API. Must be `https://`. |
| `protocols` | `kamino,marginfi` | Which protocols to query. |
| `warn_ltv` | `0.65` | LTV at or above which a position is flagged `WARN`. |
| `critical_ltv` | `0.80` | LTV at or above which a position is flagged `CRITICAL`. Must exceed `warn_ltv`. |
| `timeout_secs` | `10` | Per-request connect timeout in seconds, from 1 to 60. |

## Layout (the reference format)

```
src/health.rs     # pure core: config parsing, request planning, risk classification, report rendering
src/kamino.rs     # Kamino REST path: URL building and portfolio parsing
src/marginfi.rs   # MarginFi path: getProgramAccounts body and raw account decoding
src/lib.rs        # thin #[cfg(target_family = "wasm")] component shim over the core
tests/            # host-run tests over the pure core, with captured live API fixtures
manifest.toml     # name, version, wasm_path, capabilities, permissions
```

Every pure-core module above carries no wasm dependency, so the whole core runs
under a plain host `cargo test`: it parses config, plans the requests, classifies
risk, and renders the report. The wasm component reuses that same logic through
the shim in `src/lib.rs`.

## Build and test

```bash
cargo test                                        # host tests, no wasm needed
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release      # the component
cp target/wasm32-wasip2/release/lending_health.wasm lending_health.wasm
```

## Install

```bash
zeroclaw plugin install lending-health
```

or copy this directory (the `.wasm` next to its `manifest.toml`) into your
configured plugins dir, then enable plugins and configure the wallet allowlist:

```toml
[plugins]
enabled = true
```

Run the agent with a build that includes a compiler backend, e.g.
`--features plugins-wasm,plugins-wasm-cranelift`. For runtime-only hosts
(`--features plugins-wasm`), precompile with a matching wasmtime:
`wasmtime compile --target <triple> lending_health.wasm -o lending_health.cwasm`
and point `wasm_path` at the `.cwasm`.

## Custody tier

The tool only reads. It builds nothing, signs nothing, holds no key material,
and moves no funds. Every call it makes is an HTTPS `GET` to the Kamino API or a
read-only JSON-RPC query to the configured endpoint. There is no code path in
the plugin that constructs a transaction, signs it, submits it, or otherwise
writes to the network, so even a fully hijacked prompt cannot cause a transfer
or a liquidation. The worst a bad instruction can do here is ask for a report
the allowlist will not produce.

## Threat model

The tool runs untrusted model output against real wallet data, so the trust
boundary sits between what the model asks for and what the plugin will actually
do.

**Address substitution.** Wallets come only from the config allowlist. The
`wallet` argument is resolved against that list by label or pubkey; anything not
on it is refused before a single request goes out. The model can narrow the
report to one configured wallet, never widen it to an arbitrary address.

**Endpoint substitution.** The RPC endpoint and the Kamino base URL live in the
operator's config, not in the tool arguments, and both are required to be
`https://`. The model cannot point the tool at an attacker-controlled host to
exfiltrate the query or receive forged position data.

**Fail-closed config.** Any unrecognized config key is a hard error, not a
silent fallback. A typo like `warn_ltw` surfaces on the first call instead of
quietly leaving the position on a default threshold, and a smuggled key never
slips through as a no-op.

**Bounded, non-leaking errors.** A failed upstream call is reported as a short
status string such as `HTTP 500`. Raw upstream response bodies from a failed
call are never appended to the report, so one wallet's broken response cannot
drag another payload into the agent context. When at least one source succeeds
the report still renders, with the failures listed as short data issues; when
every source fails the tool returns an error rather than an empty all-clear.

**No custody.** As above, the plugin holds no keys and issues no writes, so a
prompt-injection ceiling is a wrong or refused report, not a lost position.

## A worked example

A run over one demo wallet with three open Kamino positions:

```
Lending health: 3 position(s), worst risk WARN.
[WARN] demo kamino Vanilla@7u3H: deposit $53930, borrow $40471, LTV 75.0% of 79.9% liq (positions stale 40 h)
[WARN] demo kamino Multiply@47tf: deposit $65030, borrow $42580, LTV 65.5% of 75.0% liq (positions stale 63 h)
[OK] demo kamino Vanilla@47tf: deposit $200638, borrow $125170, LTV 62.4% of 75.0% liq (positions stale 40 h)
```

Read the first data line as: the `demo` wallet holds a Kamino `Vanilla` position
in the market whose pubkey starts `7u3H`, with $53,930 deposited against $40,471
borrowed. Its LTV of 75.0% is close to the 79.9% liquidation LTV, so it is
flagged `WARN`. The trailing hint says the Kamino indexer's position snapshot
lags the price feed by 40 hours, so the figure is a recent read rather than a
live one. The header names the count and the worst status up front, which is the
part a scheduled briefing surfaces first.

## Prompt-injection test

Suppose the model is talked into ignoring the operator and querying a wallet
that was never configured. The allowlist stops it cold:

```
args:   {"wallet":"9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"}
        config allowlist has only demo:AcNSmd5C...

result: success=false
error:  wallet `9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM` is not in the
        configured allowlist; known labels: demo
```

The refusal is not a policy prompt or a soft warning. The wallet allowlist is
resolved inside the plugin before any network call, so an address that is not in
the operator's config has no path to a request. Even if the model fully complied
with the injection and passed the attacker's pubkey, the tool physically cannot
query it. The failure is closed, and the error names only the labels the
operator actually configured.

## License

Dual-licensed under MIT or Apache-2.0, matching the repository convention.
