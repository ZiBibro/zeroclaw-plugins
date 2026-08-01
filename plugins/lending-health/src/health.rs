//! Everything `lending_health` does between reading its config and handing back
//! a report: config parsing, wallet resolution, risk classification against the
//! configured thresholds, and rendering under a character cap. Nothing in here
//! touches the network or the wasm bindings, so `cargo test` on the host
//! exercises all of it.

use std::collections::HashMap;

/// Hard cap for the delivered payload, in characters. Keeps the tool output
/// around 200 tokens so a scheduled briefing never floods the agent context.
pub const REPORT_CHAR_CAP: usize = 900;

/// Share of [`REPORT_CHAR_CAP`] the trailing data-issues line may claim. The
/// position lines are what the operator asked for, so a long run of failed
/// source calls is trimmed before it can crowd them out.
const ISSUES_CHAR_BUDGET: usize = REPORT_CHAR_CAP / 4;

/// Room kept for the closing line of a truncated report.
const OMISSION_LINE_RESERVE: usize = 40;

/// Room kept for the `(+N more)` marker of a trimmed data-issues line.
const OMISSION_MARKER_RESERVE: usize = 16;

pub const DEFAULT_KAMINO_API_BASE: &str = "https://api.kamino.finance";
/// Liquidation buffer, as Kamino defines it: the share of the gap between
/// Current LTV and Liquidation LTV that is still unused, i.e. how far the
/// collateral can fall before the position becomes liquidatable.
///
/// `buffer = (liquidation_ltv - ltv) / liquidation_ltv`
///
/// Kamino's own documentation calls this the tolerable decline and states that
/// the buffer which matters is the gap to Liquidation LTV rather than the gap to
/// Max LTV. Thresholds are expressed on this basis because every market and
/// every obligation carries its own liquidation line: a flat 0.65 on raw LTV
/// would condemn a position with 30 points of headroom and clear one a tick from
/// seizure.
///
/// Defaults follow the buffer ranges Kamino publishes for its markets: major
/// liquid assets carry a 5 to 10 point buffer between LTV and liquidation
/// threshold, long-tail assets 10 to 20.
pub const DEFAULT_WARN_LIQUIDATION_BUFFER: f64 = 0.15;
pub const DEFAULT_CRITICAL_LIQUIDATION_BUFFER: f64 = 0.05;
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

const CONFIG_KEYS: [&str; 7] = [
    "wallets",
    "rpc_url",
    "kamino_api_base",
    "protocols",
    "warn_liquidation_buffer",
    "critical_liquidation_buffer",
    "timeout_secs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallet {
    pub label: String,
    pub pubkey: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Kamino,
    Marginfi,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Kamino => "kamino",
            Protocol::Marginfi => "marginfi",
        }
    }

    /// Prefix naming the basis of the LTV figure in a rendered line.
    ///
    /// Kamino publishes a protocol LTV: risk-adjusted debt over collateral,
    /// against a per-reserve liquidation threshold. MarginFi has no equivalent
    /// figure, so its ratio is maintenance-weighted liabilities over
    /// maintenance-weighted assets, liquidatable at 1.0. Both land in one
    /// column, and the dollar amounts printed beside them are unweighted in
    /// both cases. Without a label the MarginFi line invites an operator to
    /// divide $5,000 by $10,000, get 50%, and read the 75% next to it as a bug.
    /// The percentage is correct on its own basis; the column now says which.
    pub fn ltv_basis_prefix(self) -> &'static str {
        match self {
            Protocol::Kamino => "",
            Protocol::Marginfi => "maint ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub wallets: Vec<Wallet>,
    pub rpc_url: Option<String>,
    pub kamino_api_base: String,
    pub protocols: Vec<Protocol>,
    /// Liquidation buffer at or below which a position is flagged. See
    /// [`DEFAULT_WARN_LIQUIDATION_BUFFER`] for the basis.
    pub warn_liquidation_buffer: f64,
    pub critical_liquidation_buffer: f64,
    pub timeout_secs: u64,
}

impl Config {
    /// Parses the host-injected `__config` section. Fail-closed: any unknown
    /// key is an error, so a typo like `warn_ltw` surfaces immediately
    /// instead of silently falling back to a default threshold.
    pub fn from_section(section: &HashMap<String, String>) -> Result<Self, String> {
        for key in section.keys() {
            if !CONFIG_KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "unknown config key `{key}`; expected one of: {}",
                    CONFIG_KEYS.join(", ")
                ));
            }
        }

        let wallets = parse_wallets(
            section
                .get("wallets")
                .ok_or("config key `wallets` is required: a comma-separated allowlist like `main:<pubkey>` or bare pubkeys")?,
        )?;

        let protocols = match section.get("protocols") {
            Some(raw) => parse_protocols(raw)?,
            None => vec![Protocol::Kamino, Protocol::Marginfi],
        };

        let rpc_url = match section.get("rpc_url") {
            Some(u) => Some(parse_https_url(u, "rpc_url")?),
            None => None,
        };

        if protocols.contains(&Protocol::Marginfi) && rpc_url.is_none() {
            return Err(
                "config key `rpc_url` is required when the `marginfi` protocol is enabled"
                    .to_string(),
            );
        }

        let kamino_api_base = match section.get("kamino_api_base") {
            Some(u) => parse_https_url(u, "kamino_api_base")?,
            None => DEFAULT_KAMINO_API_BASE.to_string(),
        };

        let warn_liquidation_buffer = parse_ratio(
            section.get("warn_liquidation_buffer"),
            "warn_liquidation_buffer",
            DEFAULT_WARN_LIQUIDATION_BUFFER,
        )?;
        let critical_liquidation_buffer = parse_ratio(
            section.get("critical_liquidation_buffer"),
            "critical_liquidation_buffer",
            DEFAULT_CRITICAL_LIQUIDATION_BUFFER,
        )?;
        // A warning must fire before the critical line, so it sits at the wider
        // buffer. The comparison runs the opposite way to a raw-LTV threshold
        // pair, which is why the message spells out which is which.
        if warn_liquidation_buffer <= critical_liquidation_buffer {
            return Err(format!(
                "warn_liquidation_buffer ({warn_liquidation_buffer}) must be above critical_liquidation_buffer ({critical_liquidation_buffer}): a warning fires while more of the buffer remains"
            ));
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
            wallets,
            rpc_url,
            kamino_api_base,
            protocols,
            warn_liquidation_buffer,
            critical_liquidation_buffer,
            timeout_secs,
        })
    }

    /// Resolves the optional `wallet` argument against the allowlist. The
    /// model can only pick a configured wallet, never introduce a new one.
    pub fn resolve_wallet(&self, requested: Option<&str>) -> Result<Vec<&Wallet>, String> {
        match requested {
            None => Ok(self.wallets.iter().collect()),
            Some(query) => {
                let q = query.trim();
                reject_invisible(q, "requested wallet")?;
                let hit: Vec<&Wallet> = self
                    .wallets
                    .iter()
                    .filter(|w| w.label == q || w.pubkey == q)
                    .collect();
                if hit.is_empty() {
                    Err(format!(
                        "wallet `{q}` is not in the configured allowlist; known labels: {}",
                        self.wallets
                            .iter()
                            .map(|w| w.label.as_str())
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

fn parse_wallets(raw: &str) -> Result<Vec<Wallet>, String> {
    let mut out = Vec::new();
    for (i, entry) in raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let (label, pubkey) = match entry.split_once(':') {
            Some((l, p)) => (l.trim().to_string(), p.trim().to_string()),
            None => (format!("wallet{}", i + 1), entry.to_string()),
        };
        reject_invisible(&label, "wallet label")?;
        validate_pubkey(&pubkey, &format!("wallets entry `{label}`"))?;
        if out.iter().any(|w: &Wallet| w.label == label) {
            return Err(format!("duplicate wallet label `{label}`"));
        }
        out.push(Wallet { label, pubkey });
    }
    if out.is_empty() {
        return Err("config key `wallets` must contain at least one entry".to_string());
    }
    Ok(out)
}

/// Rejects values carrying characters that leave no visible trace: control
/// codes, zero-width marks, the soft hyphen and the BOM.
///
/// Without this check `main` and a `main` with a trailing zero-width space
/// render identically, so a refusal reads "`main` is not in the allowlist;
/// known labels: main" and the operator has no way to see the difference
/// between the value they typed and the one that was accepted. The worst case
/// is an invisible byte inside the config itself, where the label can never be
/// typed to match and the plugin is stuck for good. `trim` does not help: NBSP
/// it removes, these it does not.
fn reject_invisible(value: &str, what: &str) -> Result<(), String> {
    for (i, ch) in value.char_indices() {
        let invisible = ch.is_control()
            || matches!(
                ch,
                '\u{00ad}' | '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}'
            );
        if invisible {
            return Err(format!(
                "{what} contains an invisible character (U+{:04X}) at byte {i}, so it would look identical to a clean value; retype it without hidden formatting",
                ch as u32
            ));
        }
    }
    Ok(())
}

pub fn validate_pubkey(candidate: &str, what: &str) -> Result<(), String> {
    // `what` names the config key or entry under inspection. Without it an
    // empty or malformed value produced "`` is not a valid Solana pubkey",
    // leaving the operator to guess which of the several pubkey-bearing keys
    // was the broken one.
    if candidate.is_empty() {
        return Err(format!("{what} is empty; expected a base58 Solana pubkey"));
    }
    let bytes = bs58::decode(candidate)
        .into_vec()
        .map_err(|_| format!("{what}: `{candidate}` is not valid base58"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "{what}: `{candidate}` is not a valid Solana pubkey (decoded {} bytes, expected 32)",
            bytes.len()
        ));
    }
    Ok(())
}

fn parse_protocols(raw: &str) -> Result<Vec<Protocol>, String> {
    let mut out = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let proto = match entry.to_ascii_lowercase().as_str() {
            "kamino" => Protocol::Kamino,
            "marginfi" => Protocol::Marginfi,
            other => {
                return Err(format!(
                    "unknown protocol `{other}`; supported: kamino, marginfi"
                ))
            }
        };
        if !out.contains(&proto) {
            out.push(proto);
        }
    }
    if out.is_empty() {
        return Err("config key `protocols` must name at least one protocol".to_string());
    }
    Ok(out)
}

fn parse_https_url(raw: &str, key: &str) -> Result<String, String> {
    let url = raw.trim().trim_end_matches('/').to_string();
    if !url.starts_with("https://") {
        return Err(format!("{key} must be an https:// URL, got `{raw}`"));
    }
    Ok(url)
}

fn parse_ratio(raw: Option<&String>, key: &str, default: f64) -> Result<f64, String> {
    match raw {
        None => Ok(default),
        Some(s) => {
            let v = s
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("{key} must be a number between 0 and 1, got `{s}`"))?;
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("{key} must be between 0 and 1, got {v}"));
            }
            Ok(v)
        }
    }
}

/// Risk bucket for a single position, judged against configured thresholds.
/// Variant order is the sort order of the report: worst last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Ok,
    /// The source carried no basis to measure a liquidation distance on, so
    /// the position is listed without one instead of under a made-up number.
    Unknown,
    Warn,
    Critical,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Ok => "OK",
            Risk::Unknown => "UNKNOWN",
            Risk::Warn => "WARN",
            Risk::Critical => "CRITICAL",
        }
    }
}

/// Risk for one measured position, judged against the liquidation line that
/// position actually carries rather than against a flat pair of numbers. Kamino
/// reports a different `liquidationLtv` per market and per obligation, so the
/// same 82% LTV is comfortable at a 95% line and past saving at an 80% one.
///
/// A position at or beyond its own line is [`Risk::Critical`] whatever the
/// thresholds say: the protocol can seize it now. A line of zero or a
/// non-finite ratio measures nothing, so the position stays
/// [`Risk::Unknown`] instead of being condemned or cleared on a meaningless
/// number.
pub fn classify(l: Liquidation, cfg: &Config) -> Risk {
    let Some(buffer) = liquidation_buffer(l) else {
        return Risk::Unknown;
    };
    // A position at or past its line has a buffer of zero or less, so it lands
    // in Critical without a special case: the protocol can seize it now.
    if buffer <= cfg.critical_liquidation_buffer {
        Risk::Critical
    } else if buffer <= cfg.warn_liquidation_buffer {
        Risk::Warn
    } else {
        Risk::Ok
    }
}

/// The liquidation buffer of one position, on the basis Kamino documents:
/// `(liquidation_ltv - ltv) / liquidation_ltv`, i.e. the share of collateral
/// value that can still be lost before liquidation becomes possible. Negative
/// once the position is past its line.
///
/// `None` when nothing can be measured: a line of zero or below is not a line,
/// and a non-finite ratio would make every comparison meaningless. Reporting
/// either as a number would put a figure on the operator's screen that no
/// arithmetic produced.
pub fn liquidation_buffer(l: Liquidation) -> Option<f64> {
    // Spelled out rather than written as `!(liquidation_ltv > 0.0)`: NaN must
    // fall out here, and a bare `<= 0.0` would let it through, since every
    // comparison against NaN is false.
    if !l.ltv.is_finite() || !l.liquidation_ltv.is_finite() || l.liquidation_ltv <= 0.0 {
        return None;
    }
    Some((l.liquidation_ltv - l.ltv) / l.liquidation_ltv)
}

/// Risk for a whole position. A protocol that has already condemned the
/// account outranks every measured ratio: that flag is the protocol's own
/// verdict and needs no basis to be believed. Otherwise the configured
/// classification applies, and [`Risk::Unknown`] covers a read that produced
/// no liquidation basis at all.
pub fn classify_position(position: &Position, cfg: &Config) -> Risk {
    if position.flagged_unhealthy {
        return Risk::Critical;
    }
    // A position carrying no debt cannot be liquidated: liquidation triggers on
    // debt over collateral, and that ratio is zero. Kamino reports such a
    // position with a liquidation LTV of zero, since no line exists to report,
    // and reading that as an unmeasurable basis would label the safest possible
    // position UNKNOWN. Found on a live wallet: a deposit-only position rendered
    // as "LTV 0.0% of 0.0% liq" under UNKNOWN.
    if position.borrow_usd <= 0.0 {
        return Risk::Ok;
    }
    match position.liquidation {
        Some(l) => classify(l, cfg),
        None => Risk::Unknown,
    }
}

/// How far one position sits from liquidation: its current LTV and the line
/// that LTV is measured against, both on the same weighting basis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Liquidation {
    pub ltv: f64,
    pub liquidation_ltv: f64,
}

/// One lending position, already normalized from a protocol response.
#[derive(Debug, Clone)]
pub struct Position {
    pub wallet_label: String,
    pub protocol: Protocol,
    pub market: String,
    /// Shortened identity of the on-chain obligation or account this position
    /// was decoded from, so an operator can tell which position a line is
    /// about. See [`short_account`].
    pub account: String,
    pub deposit_usd: f64,
    pub borrow_usd: f64,
    /// `None` when the source carried no usable basis, e.g. a MarginFi health
    /// cache with a zeroed maintenance pair. The report then states no
    /// liquidation distance at all.
    pub liquidation: Option<Liquidation>,
    /// The protocol's own verdict that this account is liquidatable, e.g. a
    /// MarginFi `HEALTHY` bit its risk engine cleared. A boolean the protocol
    /// already decided, so it survives a missing basis and forces
    /// [`Risk::Critical`]. Set only where the source shows the verdict was
    /// actually reached, never inferred from a field nobody wrote.
    pub flagged_unhealthy: bool,
    pub stale_hint: Option<String>,
}

/// Shortens a base58 address to `head..tail` for the report. Enough to match
/// a line back to one obligation without spending tokens on all 44 characters.
pub fn short_account(pubkey: &str) -> String {
    const HEAD: usize = 4;
    const TAIL: usize = 4;
    // The value arrives from a third-party API and lands in a line an LLM reads,
    // so it is narrowed to the base58 alphabet before anything else happens. A
    // real account address survives untouched. A hostile string cannot bring
    // newlines with it, and a newline is the whole attack here: the report is
    // line-structured, so one smuggled break lets an attacker forge a position
    // row that reads exactly like a real one. Everything outside the alphabet
    // becomes a dot, which keeps the length visible instead of vanishing.
    let safe: String = pubkey
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l') {
                c
            } else {
                '.'
            }
        })
        .collect();
    // Nothing survived the alphabet, so the value was never an address. Say so
    // with the same placeholder the callers use for a missing field, rather than
    // printing a row of dots that looks like a redaction.
    if safe.chars().all(|c| c == '.') {
        return "?".to_string();
    }
    let total = safe.chars().count();
    if total <= HEAD + TAIL + 2 {
        return safe;
    }
    let head: String = safe.chars().take(HEAD).collect();
    let tail: String = safe.chars().skip(total - TAIL).collect();
    format!("{head}..{tail}")
}

/// Renders the whole payload the tool delivers: the position report plus, when
/// some source call failed, a trailing line naming the failures. The cap
/// covers the payload rather than the report alone, because what the operator
/// was promised is a bound on everything that lands in the agent context.
pub fn render_payload(positions: &[Position], issues: &[String], cfg: &Config) -> String {
    if issues.is_empty() {
        return render_report(positions, cfg);
    }
    // The suffix is written first and its length taken out of the budget the
    // report renders under, so the two halves cannot overrun the cap together.
    // The suffix claims at most [`ISSUES_CHAR_BUDGET`], which leaves the report
    // three quarters of the cap however badly the sources behaved.
    let suffix = render_issues(issues);
    let report = render_within(positions, cfg, REPORT_CHAR_CAP.saturating_sub(suffix.len()));
    format!("{report}{suffix}")
}

/// Renders the error text for a run in which no source call came back, Kamino
/// REST and MarginFi RPC alike. Upstream failure messages are server-controlled
/// text, so this path renders under the same character budget as a report
/// instead of pasting whatever the endpoints returned into the agent context.
pub fn render_total_failure(issues: &[String]) -> String {
    let listed = render_issues(issues);
    // `render_issues` writes the trailing line of a report: a leading newline,
    // then a `Data issues: ` label. Neither belongs in a one-sentence error, so
    // both come off before the detail behind them is reused.
    let detail = listed
        .trim_start_matches('\n')
        .trim_start_matches("Data issues: ");
    format!("every data source failed: {detail}")
}

/// Renders the trailing data-issues line under [`ISSUES_CHAR_BUDGET`]. Whole
/// entries are dropped from the tail rather than cut mid-message, so a trimmed
/// list never shows half an endpoint or half a status.
fn render_issues(issues: &[String]) -> String {
    const HEAD: &str = "\nData issues: ";
    let mut kept: Vec<&str> = Vec::new();
    let mut used = HEAD.len();
    for issue in issues {
        let cost = issue.len() + if kept.is_empty() { 0 } else { "; ".len() };
        if used + cost + OMISSION_MARKER_RESERVE > ISSUES_CHAR_BUDGET {
            break;
        }
        used += cost;
        kept.push(issue.as_str());
    }
    if kept.is_empty() {
        // Even one entry was too long to state. The count still tells the
        // operator the report is partial, which is the part that matters.
        return format!("{HEAD}{} source call(s) failed", issues.len());
    }
    let mut out = format!("{HEAD}{}", kept.join("; "));
    let omitted = issues.len() - kept.len();
    if omitted > 0 {
        out.push_str(&format!(" (+{omitted} more)"));
    }
    out
}

/// Renders the final chat-facing report. One line per position, worst risk
/// first, capped at [`REPORT_CHAR_CAP`] characters.
pub fn render_report(positions: &[Position], cfg: &Config) -> String {
    render_within(positions, cfg, REPORT_CHAR_CAP)
}

/// [`render_report`] against a caller-chosen budget, so a report sharing the
/// payload with a data-issues line renders under what is left for it.
fn render_within(positions: &[Position], cfg: &Config, cap: usize) -> String {
    if positions.is_empty() {
        return "No open lending positions found for the configured wallets.".to_string();
    }

    // Ordering key inside a risk bucket. A condemned account with no basis is
    // known to sit at its liquidation line, so it orders at the line instead
    // of at the bottom of the bucket. The key stays internal to the sort; no
    // rendered line ever carries it.
    let ltv_of = |p: &Position| match p.liquidation {
        Some(l) => l.ltv,
        None if p.flagged_unhealthy => 1.0,
        None => 0.0,
    };
    let mut sorted: Vec<&Position> = positions.iter().collect();
    sorted.sort_by(|a, b| {
        let ra = classify_position(a, cfg) as u8;
        let rb = classify_position(b, cfg) as u8;
        rb.cmp(&ra).then(
            ltv_of(b)
                .partial_cmp(&ltv_of(a))
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    let worst = classify_position(sorted[0], cfg);
    let mut lines = vec![format!(
        "Lending health: {} position(s), worst risk {}.",
        sorted.len(),
        worst.as_str()
    )];

    for p in &sorted {
        let stale = p
            .stale_hint
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        // A deposit-only position has no ratio worth printing: the protocol
        // reports both its LTV and its liquidation line as zero, and
        // "LTV 0.0% of 0.0% liq" reads as a broken measurement rather than as
        // the safest state a position can be in.
        let distance = if p.borrow_usd <= 0.0 {
            "no debt".to_string()
        } else {
            match p.liquidation {
                Some(l) => format!(
                    "{}LTV {:.1}% of {:.1}% liq",
                    p.protocol.ltv_basis_prefix(),
                    l.ltv * 100.0,
                    l.liquidation_ltv * 100.0
                ),
                None => "LTV n/a".to_string(),
            }
        };
        lines.push(format!(
            "[{}] {} {} {} #{}: deposit ${:.0}, borrow ${:.0}, {}{}",
            classify_position(p, cfg).as_str(),
            p.wallet_label,
            p.protocol.as_str(),
            p.market,
            p.account,
            p.deposit_usd,
            p.borrow_usd,
            distance,
            stale
        ));
    }

    let mut report = lines.join("\n");
    if report.len() > cap {
        let shown = lines.len();
        // Keep whole lines only; drop from the tail until the cap fits, then
        // say how many lines were omitted.
        let mut kept = Vec::new();
        let mut used = 0usize;
        for line in &lines {
            if used + line.len() + 1 > cap.saturating_sub(OMISSION_LINE_RESERVE) {
                break;
            }
            used += line.len() + 1;
            kept.push(line.clone());
        }
        let omitted = shown - kept.len();
        kept.push(format!("(+{omitted} more position line(s) omitted)"));
        report = kept.join("\n");
    }
    report
}
