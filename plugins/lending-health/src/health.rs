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
pub const DEFAULT_WARN_LTV: f64 = 0.65;
pub const DEFAULT_CRITICAL_LTV: f64 = 0.80;
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

const CONFIG_KEYS: [&str; 7] = [
    "wallets",
    "rpc_url",
    "kamino_api_base",
    "protocols",
    "warn_ltv",
    "critical_ltv",
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
}

#[derive(Debug, Clone)]
pub struct Config {
    pub wallets: Vec<Wallet>,
    pub rpc_url: Option<String>,
    pub kamino_api_base: String,
    pub protocols: Vec<Protocol>,
    pub warn_ltv: f64,
    pub critical_ltv: f64,
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

        let warn_ltv = parse_ratio(section.get("warn_ltv"), "warn_ltv", DEFAULT_WARN_LTV)?;
        let critical_ltv = parse_ratio(
            section.get("critical_ltv"),
            "critical_ltv",
            DEFAULT_CRITICAL_LTV,
        )?;
        if warn_ltv >= critical_ltv {
            return Err(format!(
                "warn_ltv ({warn_ltv}) must be below critical_ltv ({critical_ltv})"
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
            warn_ltv,
            critical_ltv,
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
        validate_pubkey(&pubkey)?;
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

pub fn classify(ltv: f64, cfg: &Config) -> Risk {
    if ltv >= cfg.critical_ltv {
        Risk::Critical
    } else if ltv >= cfg.warn_ltv {
        Risk::Warn
    } else {
        Risk::Ok
    }
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
    match position.liquidation {
        Some(l) => classify(l.ltv, cfg),
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
    let total = pubkey.chars().count();
    if total <= HEAD + TAIL + 2 {
        return pubkey.to_string();
    }
    let head: String = pubkey.chars().take(HEAD).collect();
    let tail: String = pubkey.chars().skip(total - TAIL).collect();
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
        let distance = match p.liquidation {
            Some(l) => format!(
                "LTV {:.1}% of {:.1}% liq",
                l.ltv * 100.0,
                l.liquidation_ltv * 100.0
            ),
            None => "LTV n/a".to_string(),
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
