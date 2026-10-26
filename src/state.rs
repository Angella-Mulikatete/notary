use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

/// Where Notary keeps everything inside the target repo.
pub fn notary_dir(repo: &Path) -> PathBuf { repo.join(".notary") }
pub fn state_path(repo: &Path) -> PathBuf { notary_dir(repo).join("state.json") }
pub fn integration_dir(repo: &Path) -> PathBuf { notary_dir(repo).join("trees").join("integration") }
pub fn run_dir(repo: &Path, task: &str) -> anyhow::Result<PathBuf> {
    let d = notary_dir(repo).join("runs").join(task);
    fs::create_dir_all(&d)?;
    Ok(d)
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CostBasis { #[default] Computed, Reported, Estimated }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "state")]
pub enum TaskState {
    Pending,
    Running { slot: usize, attempt: u8 },
    Implemented,
    Verified,
    Merging { prev_integration: String },
    Merged,
    AlreadySatisfied { by: Option<String> },
    Blocked { reason: String },
    Skipped { reason: String },
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskState::Merged | TaskState::AlreadySatisfied { .. }
            | TaskState::Blocked { .. } | TaskState::Skipped { .. })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Attempt {
    pub n: u8,
    pub worker: String,        // "patch:gemini/<model>" or "claude-code"
    pub cost_usd: f64,
    pub basis: CostBasis,
    pub outcome: String,       // "verified", "after-proof failed", "timeout", ...
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub body: String,
    pub deps: Vec<String>,
    pub area: Option<String>,
    pub hard: bool,
    pub state: TaskState,
    pub attempts: Vec<Attempt>,
    pub start_sha: Option<String>,   
    pub impl_sha: Option<String>,    
    pub merged_sha: Option<String>,
    pub confidence: Option<String>, 
    pub notes: Vec<String>,         
    #[serde(default)]
    pub contradicts: Option<String>,
    #[serde(default)]
    pub contradiction_why: Option<String>,
    #[serde(default)]
    pub bonus_attempts: u8,          // granted when a merge collision, not the agent, sank an attempt
}

impl Task {
    pub fn new(id: &str, title: &str, body: &str) -> Self {
        Task {
            id: id.to_string(), title: title.to_string(), body: body.to_string(),
            deps: Vec::new(), area: None, hard: false, state: TaskState::Pending,
            attempts: Vec::new(), start_sha: None, impl_sha: None, merged_sha: None,
            confidence: None, notes: Vec::new(), contradicts: None, contradiction_why: None, bonus_attempts: 0,
        }
    }

    pub fn max_attempts(&self, base: u8) -> usize { (base + self.bonus_attempts) as usize }

    pub fn transition(&mut self, to: TaskState) -> anyhow::Result<()> {
        use TaskState::*;
        let ok = match (&self.state, &to) {
            (from, Blocked { .. } | Skipped { .. }) => !from.is_terminal(),
            (Pending, Running { .. } | AlreadySatisfied { .. }) => true,
            (Running { .. }, Implemented | Pending) => true,
            (Implemented, Verified | Pending) => true,
            (Verified, Merging { .. } | Pending) => true,
            (Merging { .. }, Merged | Pending) => true,
            _ => false,
        };
        anyhow::ensure!(ok, "illegal transition for {}: {:?} -> {:?}", self.id, self.state, to);
        self.state = to;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    pub base_sha: String,
    #[serde(default)]
    pub order: Vec<String>,         
    #[serde(default)]
    pub last_merged: Option<String>,
    #[serde(default)]
    pub planned: bool,
    #[serde(default)]
    pub dep_edges: Vec<crate::analyzer::Dep>,
    #[serde(default)]
    pub contradictions: Vec<Contradiction>,
    #[serde(default)]
    pub elapsed_secs: u64,
    pub tasks: BTreeMap<String, Task>,
    pub ledger: crate::budget::Ledger,
    pub collisions: Collisions,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Contradiction { pub a: String, pub b: String, pub why: String, pub keep: Option<String>, pub decided_by: String }

#[derive(Serialize, Deserialize, Default)]
pub struct Collisions { pub rebase_conflicts: u32, pub repairs_ok: u32, pub reverts: u32, pub wasted_attempts: u32 }

impl State {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, path)?; // atomic: a crash never leaves half a file
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskState::*;

    #[test]
    fn legal_path_to_merged() {
        let mut t = Task::new("T01", "t", "b");
        t.transition(Running { slot: 0, attempt: 1 }).unwrap();
        t.transition(Implemented).unwrap();
        t.transition(Verified).unwrap();
        t.transition(Merging { prev_integration: "abc".into() }).unwrap();
        t.transition(Merged).unwrap();
        assert_eq!(t.state, Merged);
    }

    #[test]
    fn pending_cannot_jump_to_merged() {
        let mut t = Task::new("T01", "t", "b");
        assert!(t.transition(Merged).is_err());
        assert_eq!(t.state, Pending);
    }

    #[test]
    fn merged_cannot_go_back_to_pending() {
        let mut t = Task::new("T01", "t", "b");
        t.state = Merged;
        assert!(t.transition(Pending).is_err());
        assert!(t.transition(Blocked { reason: "x".into() }).is_err());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("notary-state-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let mut s = State { base_sha: "abc".into(), ..Default::default() };
        s.tasks.insert("T01".into(), Task::new("T01", "t", "b"));
        s.save(&path).unwrap();
        s.save(&path).unwrap();
        let back = State::load(&path).unwrap();
        assert_eq!(back.base_sha, "abc");
        assert_eq!(back.tasks["T01"].state, Pending);
        fs::remove_dir_all(&dir).ok();
    }
}
