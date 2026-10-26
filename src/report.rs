use std::path::Path;

use crate::{config::Config, state::{self, State, TaskState}};

pub fn status(repo: &Path) -> anyhow::Result<()> {
    let st = State::load(&state::state_path(repo))?;
    println!("{:<5} {:<18} {:<34} {:>8}  detail", "task", "state", "attempts", "cost");
    for id in &st.order {
        let t = &st.tasks[id];
        let (name, detail) = match &t.state {
            TaskState::Running { slot, attempt } => ("Running".to_string(), format!("slot {} attempt {attempt}", slot + 1)),
            TaskState::Merging { .. } => ("Merging".into(), String::new()),
            TaskState::AlreadySatisfied { by } => ("AlreadySatisfied".into(), by.as_ref().map(|b| format!("by {b}")).unwrap_or("on base".into())),
            TaskState::Blocked { reason } => ("Blocked".into(), reason.clone()),
            TaskState::Skipped { reason } => ("Skipped".into(), reason.clone()),
            other => (format!("{other:?}"), if t.deps.is_empty() { String::new() } else { format!("deps {}", t.deps.join(",")) }),
        };
        let attempts = t.attempts.iter().map(|a| format!("a{}:{}", a.n, a.worker.split('/').next_back().unwrap_or(""))).collect::<Vec<_>>().join(" ");
        let cost = st.ledger.per_task.get(id).copied().unwrap_or(0.0);
        let detail: String = detail.chars().take(90).collect();
        println!("{id:<5} {name:<18} {attempts:<34} ${cost:>7.4}  {detail}");
    }
    let l = &st.ledger;
    let reserved: f64 = l.reserved.values().sum::<f64>().max(0.0);
    println!("\nspent ${:.4} of ${:.2} cap | reserved ${reserved:.4} ({}) | by basis {:?}",
        l.spent_usd, l.cap_usd, l.reserved.keys().cloned().collect::<Vec<_>>().join(", "), l.by_basis);
    let c = &st.collisions;
    println!("collisions: rebase conflicts {}, repairs ok {}, reverts {}, wasted attempts {}",
        c.rebase_conflicts, c.repairs_ok, c.reverts, c.wasted_attempts);
    Ok(())
}

pub fn verify(_cfg: &Config, _repo: &Path) -> anyhow::Result<()> {
    anyhow::bail!("`notary verify` arrives in Phase 3")
}
