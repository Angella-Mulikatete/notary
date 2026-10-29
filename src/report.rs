use std::{fmt::Write as _, fs, path::Path, time::Duration};

use crate::{
    config::Config, git, proc,
    state::{self, State, TaskState},
    testrun,
};

fn state_name(s: &TaskState) -> &'static str {
    match s {
        TaskState::Pending => "Pending",
        TaskState::Running { .. } => "Running",
        TaskState::Implemented => "Implemented",
        TaskState::Verified => "Verified",
        TaskState::Merging { .. } => "Merging",
        TaskState::Merged => "Merged",
        TaskState::AlreadySatisfied { .. } => "AlreadySatisfied",
        TaskState::Blocked { .. } => "Blocked",
        TaskState::Skipped { .. } => "Skipped",
    }
}

pub fn status(repo: &Path) -> anyhow::Result<()> {
    let st = State::load(&state::state_path(repo))?;
    println!("{:<5} {:<18} {:<34} {:>8}  detail", "task", "state", "attempts", "cost");
    for id in &st.order {
        let t = &st.tasks[id];
        let detail = match &t.state {
            TaskState::Running { slot, attempt } => format!("slot {} attempt {attempt}", slot + 1),
            TaskState::AlreadySatisfied { by } => by.as_ref().map(|b| format!("by {b}")).unwrap_or("on base".into()),
            TaskState::Blocked { reason } | TaskState::Skipped { reason } => reason.clone(),
            _ => if t.deps.is_empty() { String::new() } else { format!("deps {}", t.deps.join(",")) },
        };
        let attempts = t.attempts.iter().map(|a| format!("a{}:{}", a.n, a.worker.split('/').next_back().unwrap_or(""))).collect::<Vec<_>>().join(" ");
        let cost = st.ledger.per_task.get(id).copied().unwrap_or(0.0);
        let detail: String = detail.chars().take(90).collect();
        println!("{id:<5} {:<18} {attempts:<34} ${cost:>7.4}  {detail}", state_name(&t.state));
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

fn cell(s: &str) -> String { s.replace('|', "\\|").replace('\n', " ") }

pub fn write(cfg: &Config, st: &State, nd: &Path, discrepancies: &[String]) -> anyhow::Result<()> {
    let mut md = String::new();
    let l = &st.ledger;
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for t in st.tasks.values() { *counts.entry(state_name(&t.state)).or_default() += 1; }
    let merged = counts.get("Merged").copied().unwrap_or(0);
    let c = &st.collisions;

    writeln!(md, "# Notary run report\n")?;
    writeln!(md, "## Summary\n")?;
    writeln!(md, "- Tasks: {}", counts.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "))?;
    writeln!(md, "- Spend: ${:.4} of ${:.2} cap (by cost basis: {})", l.spent_usd, l.cap_usd,
        l.by_basis.iter().map(|(k, v)| format!("{k} ${v:.4}")).collect::<Vec<_>>().join(", "))?;
    if merged > 0 { writeln!(md, "- Cost per merged task: ${:.4}", l.spent_usd / merged as f64)?; }
    writeln!(md, "- Collisions: {} rebase conflicts, {} repaired, {} merges reverted, {} wasted attempts",
        c.rebase_conflicts, c.repairs_ok, c.reverts, c.wasted_attempts)?;
    writeln!(md, "- Run time: {} min {} s", st.elapsed_secs / 60, st.elapsed_secs % 60)?;
    writeln!(md, "- Workers: {}", cfg.slots.iter().map(|s| format!("{}:{}", s.kind, s.model)).collect::<Vec<_>>().join(", "))?;
    if cfg.policy.shadow_tests {
        writeln!(md, "- Tests: the backlog shipped without acceptance tests, so Notary wrote one per ticket (`{}/`) before work started; the grader has no file for these tasks and cannot cross-check them", cfg.repo.acceptance_dir)?;
    }
    if !discrepancies.is_empty() {
        for d in discrepancies { writeln!(md, "- **DISCREPANCY** {d}")?; }
    } else if !cfg.policy.shadow_tests {
        writeln!(md, "- Grader cross-check: every Merged / AlreadySatisfied task passes `{}`; nothing else does", cfg.repo.grade)?;
    }

    writeln!(md, "\n## Tasks\n")?;
    writeln!(md, "| Task | Title | Status | Confidence | Attempts | Cost | Reason / evidence |")?;
    writeln!(md, "|---|---|---|---|---|---|---|")?;
    for id in &st.order {
        let t = &st.tasks[id];
        let attempts = t.attempts.iter().map(|a| format!("a{} {} (${:.4} {:?}): {}", a.n, a.worker, a.cost_usd, a.basis, a.outcome))
            .collect::<Vec<_>>().join("<br>");
        let reason = match &t.state {
            TaskState::Blocked { reason } | TaskState::Skipped { reason } => reason.clone(),
            TaskState::AlreadySatisfied { by } => format!("acceptance passes {} (runs/{id}/)", by.as_ref().map(|b| format!("after {b} merged")).unwrap_or("on the base commit".into())),
            TaskState::Merged => format!("before fails at {}, after passes at {}, gate green (runs/{id}/)",
                t.start_sha.as_deref().map(|s| &s[..8]).unwrap_or("?"), t.impl_sha.as_deref().map(|s| &s[..8]).unwrap_or("?")),
            _ => t.notes.last().cloned().unwrap_or_default(),
        };
        writeln!(md, "| {id} | {} | {} | {} | {} | ${:.4} | {} |", cell(&t.title), state_name(&t.state),
            t.confidence.as_deref().unwrap_or("-"), cell(&attempts), l.per_task.get(id).copied().unwrap_or(0.0), cell(&reason))?;
    }

    writeln!(md, "\n## Contradictions\n")?;
    if st.contradictions.is_empty() { writeln!(md, "None flagged by the planner.")?; }
    for x in &st.contradictions {
        let loser = x.keep.as_ref().map(|k| if *k == x.a { &x.b } else { &x.a });
        writeln!(md, "- **{} vs {}**: {}. Decision: {} (decided by {}).{}", x.a, x.b, cell(&x.why),
            x.keep.as_ref().map(|k| format!("keep {k}")).unwrap_or("not a contradiction, try both".into()), x.decided_by,
            loser.map(|lo| format!(" {lo} is now {}.", state_name(&st.tasks[lo].state))).unwrap_or_default())?;
    }

    writeln!(md, "\n## Dependencies\n")?;
    if st.dep_edges.is_empty() { writeln!(md, "None found.")?; }
    for d in &st.dep_edges { writeln!(md, "- {} → {} ({}): {}", d.task, d.on, d.source, cell(&d.why))?; }

    writeln!(md, "\n## Final grade\n\n```")?;
    write!(md, "{}", fs::read_to_string(nd.join("final-grade.log")).unwrap_or_else(|_| "(grader did not run)\n".into()))?;
    writeln!(md, "```")?;

    writeln!(md, "\n## Known limitations\n")?;
    if cfg.policy.shadow_tests {
        writeln!(md, "- \"Done\" means Notary's own test for the ticket (`{}/<id>.test.js`, written by a model from the ticket text and required to fail on the base commit) failed before and passes after, plus the regression suite and lock check. If a model misread a ticket, its test is wrong too, so a Merged task can still fail the organizers' hidden tests.", cfg.repo.acceptance_dir)?;
    } else {
        writeln!(md, "- \"Done\" means the locked acceptance test failed before and passes after, plus the regression suite and lock check. Hidden tests that probe behaviour outside those tests are not checked.")?;
    }
    writeln!(md, "- Claude Code costs are the CLI's own `total_cost_usd` (basis: estimated); on a subscription no money is charged.")?;
    writeln!(md, "- Patch-agent costs are computed from token counts and list prices in notary.toml (basis: computed); on a free tier no money is charged.")?;
    writeln!(md, "- Planner areas and contradictions come from one LLM call and can be wrong; the merge gate is the backstop for contradictions it misses.")?;
    writeln!(md, "\nEvidence: `.notary/events.jsonl`, `.notary/runs/Txx/` (prompts, worker logs, before/after TAP, diffs, gate logs). Recheck with `notary verify`.")?;

    fs::write(nd.join("report.md"), md)?;
    println!("report: wrote {}", nd.join("report.md").display());
    Ok(())
}

pub fn verify(cfg: &Config, repo: &Path) -> anyhow::Result<()> {
    let st = State::load(&state::state_path(repo))?;
    let nd = state::notary_dir(repo);
    let integ = state::integration_dir(repo);
    let out = nd.join("verify");
    fs::create_dir_all(&out)?;
    let tip = git::head(&integ)?;
    println!("verify: integration tip {}", &tip[..8]);
    let mut bad = 0;
    let mut mark = |ok: bool, line: String| { println!("{} {line}", if ok { "✓" } else { "✗" }); if !ok { bad += 1; } };

    for id in &st.order {
        let t = &st.tasks[id];
        match &t.state {
            TaskState::Merged => {
                let start = t.start_sha.clone().unwrap_or_default();
                let wt = nd.join("trees").join(format!("verify-{id}"));
                if wt.exists() { let _ = git::worktree_remove(repo, &wt); }
                git::worktree_add_detached(repo, &wt, &start)?;
                let before = testrun::task(cfg, &wt, id, &out.join(format!("{id}-before.tap")))?;
                let _ = git::worktree_remove(repo, &wt);
                let after = testrun::task(cfg, &integ, id, &out.join(format!("{id}-after.tap")))?;
                mark(!before.passed && after.passed, format!("{id} Merged: fails at start {} ({}/{} failing), passes at tip ({} tests)",
                    &start[..start.len().min(8)], before.failed, before.tests, after.tests));
            }
            TaskState::AlreadySatisfied { by } => {
                let after = testrun::task(cfg, &integ, id, &out.join(format!("{id}-after.tap")))?;
                mark(after.passed, format!("{id} AlreadySatisfied ({}): passes at tip", by.as_deref().unwrap_or("on base")));
            }
            other => {
                let after = testrun::task(cfg, &integ, id, &out.join(format!("{id}-after.tap")))?;
                mark(!after.passed, format!("{id} {}: not claimed, and {} at tip", state_name(other),
                    if after.passed { "yet its test passes (claim missed, or the test is too weak)" } else { "does not pass" }));
            }
        }
    }
    mark(testrun::regression(cfg, &integ, &out.join("regression.log"))?, format!("regression suite (`{}`)", cfg.repo.test_all));
    mark(testrun::integrity(cfg, &integ, &out.join("integrity.log"))?, format!("locked files (`{}`)", cfg.repo.integrity));
    let glog = out.join("grade.log");
    let g = proc::run(proc::sh(&cfg.repo.grade, &integ), &glog, &glog.with_extension("err"), Duration::from_secs(600))?;
    let grade = fs::read_to_string(&glog).unwrap_or_default();
    let summary = grade.lines().find(|l| l.contains("tasks pass")).unwrap_or("(no summary)").to_string();
    mark(g.code == Some(0), format!("grader: {summary}"));
    let _ = git::prune(repo);
    anyhow::ensure!(bad == 0, "verify: {bad} check(s) failed");
    println!("verify: all checks passed");
    Ok(())
}
