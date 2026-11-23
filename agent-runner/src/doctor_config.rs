use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::DriverConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DoctorConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_source_dir")]
    pub source_dir: PathBuf,
    #[serde(default = "default_triage_model")]
    pub triage_model: String,
    #[serde(default = "default_triage_thinking")]
    pub triage_thinking: String,
    #[serde(default = "default_analysis_model")]
    pub analysis_model: String,
    #[serde(default = "default_analysis_thinking")]
    pub analysis_thinking: String,
    #[serde(default)]
    pub triage_daily_token_budget: Option<u64>,
    #[serde(default)]
    pub analysis_daily_token_budget: Option<u64>,
    #[serde(default)]
    pub remediation_daily_token_budget: Option<u64>,
    #[serde(default)]
    pub daily_write_round_budget: Option<u64>,
    #[serde(default = "default_triage_unknown_usage_tokens")]
    pub triage_unknown_usage_tokens: u64,
    #[serde(default = "default_analysis_unknown_usage_tokens")]
    pub analysis_unknown_usage_tokens: u64,
    #[serde(default = "default_remediation_unknown_usage_tokens")]
    pub remediation_unknown_usage_tokens: u64,
    #[serde(default = "default_max_remediation_rounds")]
    pub max_remediation_rounds: u32,
    #[serde(default = "default_max_pending")]
    pub max_pending_report_problems: usize,
    #[serde(default = "default_max_pending")]
    pub max_pending_log_signatures: usize,
    #[serde(default = "default_analysis_failure_threshold")]
    pub analysis_failure_threshold: u32,
    #[serde(default = "default_analysis_failure_window_secs")]
    pub analysis_failure_window_secs: u64,
    #[serde(default = "default_analysis_circuit_cooldown_secs")]
    pub analysis_circuit_cooldown_secs: u64,
    #[serde(default = "default_flood_event_threshold")]
    pub flood_event_threshold: u32,
    #[serde(default = "default_flood_window_secs")]
    pub flood_window_secs: u64,
    #[serde(default = "default_flood_cooldown_secs")]
    pub flood_cooldown_secs: u64,
    #[serde(default)]
    pub write_enabled: bool,
    #[serde(default = "default_allowed_write_roots")]
    pub allowed_write_roots: Vec<PathBuf>,
    /// OCI image used only for Doctor's dedicated container. It is deliberately
    /// separate from project run containers and their sidecars/credentials.
    #[serde(default)]
    pub container_image: Option<String>,
}

impl Default for DoctorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source_dir: default_source_dir(),
            triage_model: default_triage_model(),
            triage_thinking: default_triage_thinking(),
            analysis_model: default_analysis_model(),
            analysis_thinking: default_analysis_thinking(),
            triage_daily_token_budget: None,
            analysis_daily_token_budget: None,
            remediation_daily_token_budget: None,
            daily_write_round_budget: None,
            triage_unknown_usage_tokens: default_triage_unknown_usage_tokens(),
            analysis_unknown_usage_tokens: default_analysis_unknown_usage_tokens(),
            remediation_unknown_usage_tokens: default_remediation_unknown_usage_tokens(),
            max_remediation_rounds: default_max_remediation_rounds(),
            max_pending_report_problems: default_max_pending(),
            max_pending_log_signatures: default_max_pending(),
            analysis_failure_threshold: default_analysis_failure_threshold(),
            analysis_failure_window_secs: default_analysis_failure_window_secs(),
            analysis_circuit_cooldown_secs: default_analysis_circuit_cooldown_secs(),
            flood_event_threshold: default_flood_event_threshold(),
            flood_window_secs: default_flood_window_secs(),
            flood_cooldown_secs: default_flood_cooldown_secs(),
            write_enabled: false,
            allowed_write_roots: default_allowed_write_roots(),
            container_image: None,
        }
    }
}

impl DoctorConfig {
    pub fn validate(&self, driver: &DriverConfig) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let missing = [
            ("triage_daily_token_budget", self.triage_daily_token_budget),
            ("analysis_daily_token_budget", self.analysis_daily_token_budget),
            ("remediation_daily_token_budget", self.remediation_daily_token_budget),
            ("daily_write_round_budget", self.daily_write_round_budget),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.is_none().then_some(name))
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            anyhow::bail!(
                "doctor.enabled = true requires explicit daily budgets; missing {} (zero explicitly disables a stage)",
                missing
                    .iter()
                    .map(|name| format!("doctor.{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if driver.kind != "kimi-acp" {
            anyhow::bail!("doctor.enabled = true requires driver.kind = \"kimi-acp\" for configured model aliases");
        }
        let text = std::fs::read_to_string(&driver.kimi_config).map_err(|e| {
            anyhow::anyhow!(
                "cannot read driver.kimi_config {} for Doctor model aliases: {e}",
                driver.kimi_config.display()
            )
        })?;
        let value: toml::Value = toml::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "cannot parse driver.kimi_config {} for Doctor model aliases: {e}",
                driver.kimi_config.display()
            )
        })?;
        for (key, alias) in [
            ("triage_model", &self.triage_model),
            ("analysis_model", &self.analysis_model),
        ] {
            if !contains_alias(&value, alias) {
                anyhow::bail!("doctor.{key} = {alias:?} is not defined in driver.kimi_config");
            }
        }
        for (key, value) in [
            ("triage_unknown_usage_tokens", self.triage_unknown_usage_tokens),
            ("analysis_unknown_usage_tokens", self.analysis_unknown_usage_tokens),
            (
                "remediation_unknown_usage_tokens",
                self.remediation_unknown_usage_tokens,
            ),
        ] {
            if value == 0 {
                anyhow::bail!("doctor.{key} must be greater than zero so missing telemetry cannot bypass budgets");
            }
        }
        if self.write_enabled && self.allowed_write_roots.is_empty() {
            anyhow::bail!("doctor.write_enabled = true requires at least one doctor.allowed_write_roots entry");
        }
        Ok(())
    }

    pub fn budget(&self, bucket: TokenBucket) -> u64 {
        match bucket {
            TokenBucket::Triage => self.triage_daily_token_budget,
            TokenBucket::Analysis => self.analysis_daily_token_budget,
            TokenBucket::Remediation => self.remediation_daily_token_budget,
        }
        .unwrap_or(0)
    }

    pub fn unknown_usage(&self, bucket: TokenBucket) -> u64 {
        match bucket {
            TokenBucket::Triage => self.triage_unknown_usage_tokens,
            TokenBucket::Analysis => self.analysis_unknown_usage_tokens,
            TokenBucket::Remediation => self.remediation_unknown_usage_tokens,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenBucket {
    Triage,
    Analysis,
    Remediation,
}

impl TokenBucket {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Analysis => "analysis",
            Self::Remediation => "remediation",
        }
    }
}

fn contains_alias(value: &toml::Value, alias: &str) -> bool {
    match value {
        toml::Value::String(value) => value == alias,
        toml::Value::Array(values) => values.iter().any(|value| contains_alias(value, alias)),
        toml::Value::Table(values) => values
            .iter()
            .any(|(key, value)| key == alias || contains_alias(value, alias)),
        _ => false,
    }
}

fn default_source_dir() -> PathBuf {
    PathBuf::from("/opt/remoter-agent/src")
}
fn default_triage_model() -> String {
    "kimi-code/kimi-for-coding".to_string()
}
fn default_triage_thinking() -> String {
    "on".to_string()
}
fn default_analysis_model() -> String {
    "kimi-code/k3".to_string()
}
fn default_analysis_thinking() -> String {
    "max".to_string()
}
fn default_triage_unknown_usage_tokens() -> u64 {
    32_768
}
fn default_analysis_unknown_usage_tokens() -> u64 {
    131_072
}
fn default_remediation_unknown_usage_tokens() -> u64 {
    131_072
}
fn default_max_remediation_rounds() -> u32 {
    5
}
fn default_max_pending() -> usize {
    100
}
fn default_analysis_failure_threshold() -> u32 {
    3
}
fn default_analysis_failure_window_secs() -> u64 {
    600
}
fn default_analysis_circuit_cooldown_secs() -> u64 {
    1800
}
fn default_flood_event_threshold() -> u32 {
    200
}
fn default_flood_window_secs() -> u64 {
    60
}
fn default_flood_cooldown_secs() -> u64 {
    300
}
fn default_allowed_write_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/var/lib/remoter-agent/workspace"),
        PathBuf::from("/var/lib/remoter-agent/doctor-scratch"),
    ]
}

pub fn expand_paths(config: &mut DoctorConfig, expand: impl Fn(&Path) -> PathBuf) {
    config.source_dir = expand(&config.source_dir);
    config.allowed_write_roots = config.allowed_write_roots.iter().map(|path| expand(path)).collect();
}
