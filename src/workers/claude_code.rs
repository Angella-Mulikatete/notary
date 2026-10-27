use std::{fs, path::PathBuf, process::Command, time::Duration};

use super::{Job, Outcome, Worker};
use crate::{proc, state::CostBasis};

/// The default system prompt (which states the working directory) is replaced to save ~$0.18 a call,
/// so the directory must be stated here: without it Claude once edited the main checkout instead.
fn system_prompt(worktree: &std::path::Path) -> String {
    let win = worktree.display().to_string();
    let posix = win.replace('\\', "/");
    format!("You are a coding agent working in a git worktree of a small Node.js project. \
Your working directory is {win} (in Bash: {posix}). Read and edit files ONLY under that directory; \
use absolute paths that start with it. Never touch any other copy of the repository. \
Complete the task in the user message by editing files directly. Never run git. \
Never edit test/, acceptance/, scripts/, backlog/, package.json or tests.lock.json. \
When done, reply with one line saying what you changed.")
}

pub const MIN_USD: f64 = 0.10;

pub struct ClaudeCodeWorker {
    pub model: String,
    pub exe: PathBuf,
}

impl ClaudeCodeWorker {
    pub fn new(model: &str, command: Option<&str>) -> Self {
        let exe = match command {
            Some(c) => PathBuf::from(c),
            None => std::env::var("APPDATA").ok()
                .map(|a| PathBuf::from(a).join(r"npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe"))
                .filter(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("claude")),
        };
        ClaudeCodeWorker { model: model.to_string(), exe }
    }
}

impl Worker for ClaudeCodeWorker {
    fn name(&self) -> String { format!("claude-code/{}", self.model) }
    fn is_claude(&self) -> bool { true }

    fn run(&self, job: &Job) -> anyhow::Result<Outcome> {
        let budget = (job.max_usd * 0.8).max(0.05); // headroom: the CLI can overshoot its cap
        let input = job.run_dir.join(format!("claude-a{}.prompt.md", job.attempt));
        fs::write(&input, &job.prompt)?;
        let system = system_prompt(&job.worktree);
        let mut cmd = Command::new(&self.exe);
        cmd.current_dir(&job.worktree).args([
            "-p",
            "--model", &self.model,
            "--output-format", "json",
            "--max-budget-usd", &format!("{budget:.2}"),
            "--max-turns", "30",
            "--system-prompt", &system,
            "--setting-sources", "project",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--permission-mode", "acceptEdits",
            "--tools", "Read,Edit,Write,Glob,Grep,Bash",
            "--allowedTools", "Read,Edit,Write,Glob,Grep,Bash(node --test:*),Bash(npm test:*)",
        ]);
        let out = job.run_dir.join(format!("claude-a{}.json", job.attempt));
        let err = job.run_dir.join(format!("claude-a{}.err", job.attempt));
        let r = proc::run_with_input(cmd, Some(&input), &out, &err, Duration::from_secs(job.timeout_secs))?;
        let fail = |msg: String, cost: f64| Ok(Outcome { worker: self.name(), cost_usd: cost, basis: CostBasis::Estimated, summary: String::new(), error: Some(msg) });
        if r.timed_out {
            return fail(format!("claude-code timed out after {}s", job.timeout_secs), budget);
        }
        let v: serde_json::Value = match serde_json::from_str(&fs::read_to_string(&out)?) {
            Ok(v) => v,
            Err(e) => return fail(format!("claude-code output was not JSON ({e}); see {}", err.display()), budget),
        };
        let cost = v["total_cost_usd"].as_f64().unwrap_or(budget);
        let subtype = v["subtype"].as_str().unwrap_or("");
        let summary = format!("[{subtype}] {}", v["result"].as_str().unwrap_or("").trim());
        Ok(Outcome { worker: self.name(), cost_usd: cost, basis: CostBasis::Estimated, summary, error: None })
    }
}
