//! Pure core of the `lending-health` tool: config parsing, request planning,
//! risk classification, and report rendering. No wasm and no I/O in here, so
//! the whole module runs under a plain host `cargo test`.

use std::collections::HashMap;

/// Hard cap for the rendered report, in characters. Keeps the tool output
/// around 200 tokens so a scheduled briefing never floods the agent context.
pub const REPORT_CHAR_CAP: usize = 900;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Ok,
    Warn,
    Critical,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Ok => "OK",
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

/// One lending position, already normalized from a protocol response.
#[derive(Debug, Clone)]
pub struct Position {
    pub wallet_label: String,
    pub protocol: Protocol,
    pub market: String,
    pub deposit_usd: f64,
    pub borrow_usd: f64,
    pub ltv: f64,
    pub liquidation_ltv: f64,
    pub stale_hint: Option<String>,
}

/// Renders the final chat-facing report. One line per position, worst risk
/// first, capped at [`REPORT_CHAR_CAP`] characters.
pub fn render_report(positions: &[Position], cfg: &Config) -> String {
    if positions.is_empty() {
        return "No open lending positions found for the configured wallets.".to_string();
    }

    let mut sorted: Vec<&Position> = positions.iter().collect();
    sorted.sort_by(|a, b| {
        let ra = classify(a.ltv, cfg) as u8;
        let rb = classify(b.ltv, cfg) as u8;
        rb.cmp(&ra).then(
            b.ltv
                .partial_cmp(&a.ltv)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    let worst = classify(sorted[0].ltv, cfg);
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
        lines.push(format!(
            "[{}] {} {} {}: deposit ${:.0}, borrow ${:.0}, LTV {:.1}% of {:.1}% liq{}",
            classify(p.ltv, cfg).as_str(),
            p.wallet_label,
            p.protocol.as_str(),
            p.market,
            p.deposit_usd,
            p.borrow_usd,
            p.ltv * 100.0,
            p.liquidation_ltv * 100.0,
            stale
        ));
    }

    let mut report = lines.join("\n");
    if report.len() > REPORT_CHAR_CAP {
        let shown = lines.len();
        // Keep whole lines only; drop from the tail until the cap fits, then
        // say how many lines were omitted.
        let mut kept = Vec::new();
        let mut used = 0usize;
        for line in &lines {
            if used + line.len() + 1 > REPORT_CHAR_CAP.saturating_sub(40) {
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
