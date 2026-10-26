use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path};

use crate::state::CostBasis;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    pub repo: RepoCfg,
    pub budget: BudgetCfg,
    pub policy: Policy,
    pub providers: BTreeMap<String, ProviderCfg>,
    pub prices: BTreeMap<String, Price>,
    pub planner: ModelRef,
    pub slots: Vec<SlotCfg>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RepoCfg {
    pub backlog: String,
    pub test_all: String,
    pub test_task: String,
    pub integrity: String,
    pub grade: String,
    pub grade_report: String,
    pub routes_file: String,
    pub writable: Vec<String>,
    pub locked: Vec<String>,
    pub context_files: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct BudgetCfg {
    pub run_cap_usd: f64,
    pub task_ceiling_usd: f64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Policy {
    pub contradiction: String,
    pub ask_timeout_secs: u64,
    pub max_attempts: u8,
    pub fast_forward_main: bool,
    pub shadow_tests: bool,
    pub timeout_secs: u64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ProviderCfg {
    pub base_url: String,
    pub key_env: String,
    pub cost: CostBasis,
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct Price {
    pub input: f64,
    pub output: f64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct SlotCfg {
    pub kind: String,
    pub provider: Option<String>,
    pub model: String,
    #[serde(default)]
    pub command: Option<String>,
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    dotenvy::dotenv().ok();
    let text = fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read config {}: {e}", path.display()))?;
    Ok(toml::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_config_parses() {
        let cfg = load(Path::new("notary.toml")).unwrap();
        assert_eq!(cfg.slots.len(), 3);
        assert_eq!(cfg.providers["gemini"].cost, CostBasis::Computed);
        assert_eq!(cfg.providers["openrouter"].cost, CostBasis::Reported);
        assert!(cfg.budget.run_cap_usd > 0.0);
    }
}
