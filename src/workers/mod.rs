use std::path::PathBuf;
use crate::state::{CostBasis, Task};

pub mod claude_code;
pub mod patch;

pub struct Job {
    pub task_id: String,
    pub task: Task,
    pub attempt: u8,
    pub prompt: String,         
    pub worktree: PathBuf,
    pub run_dir: PathBuf,      
    pub test_cmd: String,       
    pub max_usd: f64,          
    pub timeout_secs: u64,
}

pub struct Outcome {
    pub worker: String,
    pub cost_usd: f64,
    pub basis: CostBasis,
    pub summary: String,       
    pub error: Option<String>,  
}

pub trait Worker: Send + Sync {
    fn name(&self) -> String;
    fn is_claude(&self) -> bool { false }
    fn run(&self, job: &Job) -> anyhow::Result<Outcome>;
}

pub enum Msg {
    Done { slot: usize, task: String, attempt: u8, result: anyhow::Result<Outcome> },
}
