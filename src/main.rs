use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod analyzer; mod budget; mod config; mod events; mod git; mod llm; mod merge;
mod planner; mod probe; mod proc; mod prompt; mod report; mod scheduler;
mod shadow; mod state; mod testrun; mod verify; mod workers;

#[derive(Parser)]
#[command(name = "notary", about = "Multi-agent harness for Collision Course")]
struct Cli {
    #[arg(long, global = true, default_value = "notary.toml")]
    config: PathBuf,
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Init, Probe, Plan, Run, Resume, Status, Verify, Clean,
    /// Writes a test per ticket when the backlog has none (policy.shadow_tests); run after init, before plan
    Shadow { #[arg(long, value_delimiter = ',')] only: Vec<String> },
    /// Debug: one LLM call, prints text and cost
    LlmPing { #[arg(long)] provider: String, #[arg(long)] model: String },
}

/// canonicalize() on Windows returns `\\?\C:\...`, which git and node mishandle.
fn canon(p: &std::path::Path) -> anyhow::Result<PathBuf> {
    let c = p.canonicalize()?;
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") { Some(rest) => PathBuf::from(rest), None => c })
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = config::load(&cli.config)?;
    let repo = canon(&cli.repo)?;
    match cli.cmd {
        Cmd::Init => probe::init(&cfg, &repo),
        Cmd::Probe => probe::probe_cmd(&cfg, &repo),
        Cmd::Shadow { only } => shadow::shadow_cmd(&cfg, &repo, &only),
        Cmd::Plan => planner::plan_cmd(&cfg, &repo),
        Cmd::Run => scheduler::run(&cfg, &repo, false),
        Cmd::Resume => scheduler::run(&cfg, &repo, true),
        Cmd::Status => report::status(&repo),
        Cmd::Verify => report::verify(&cfg, &repo),
        Cmd::Clean => git::clean(&repo),
        Cmd::LlmPing { provider, model } => llm::ping(&cfg, &provider, &model),
    }
}
