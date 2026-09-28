//! `~/.config/cued/config.toml` (DESIGN.md §10.1) — user-level defaults.
//! Precedence: built-in < config file < job < step. A missing file is not an
//! error; it just means built-ins.

use std::path::Path;

use anyhow::{Context, Result};
use jiff::SignedDuration;
use serde::Deserialize;

use crate::model::{CatchUp, MissedWait, OnInterrupt, Overlap};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub policy: PolicyDefaults,
    pub env: EnvPolicy,
    pub retention: Retention,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PolicyDefaults {
    pub missed_wait: MissedWait,
    pub on_interrupt: OnInterrupt,
    pub catch_up: CatchUp,
    pub overlap: Overlap,
    /// §2.2: TERM → this → KILL. Provisional number (§12).
    pub kill_grace: SignedDuration,
}

impl Default for PolicyDefaults {
    fn default() -> Self {
        Self {
            missed_wait: MissedWait::default(),
            on_interrupt: OnInterrupt::default(),
            catch_up: CatchUp::default(),
            overlap: Overlap::default(),
            kill_grace: SignedDuration::from_secs(10),
        }
    }
}

/// §7.5: full capture, secrets stripped by pattern at submit time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EnvPolicy {
    /// Glob-style patterns; provisional set (§12).
    pub deny: Vec<String>,
}

impl Default for EnvPolicy {
    fn default() -> Self {
        Self {
            deny: ["*_TOKEN", "*_SECRET", "*_KEY", "*PASSWORD*", "*_CREDENTIALS"]
                .map(String::from)
                .to_vec(),
        }
    }
}

/// §10.2: prune terminal runs + logs by age AND per-job count.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Retention {
    pub days: u32,
    pub runs_per_job: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self { days: 30, runs_per_job: 20 }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let config = Config::default();
        assert_eq!(config.policy.missed_wait, MissedWait::RunAsap);
        assert_eq!(config.policy.on_interrupt, OnInterrupt::Hold);
        assert_eq!(config.policy.catch_up, CatchUp::RunOnce);
        assert_eq!(config.policy.overlap, Overlap::Skip);
        assert_eq!(config.policy.kill_grace, SignedDuration::from_secs(10));
        assert_eq!(config.retention.days, 30);
        assert_eq!(config.retention.runs_per_job, 20);
    }

    #[test]
    fn missing_file_is_defaults() {
        let config = Config::load(Path::new("/nonexistent/cued/config.toml")).unwrap();
        assert_eq!(config.retention.days, 30);
    }
}
