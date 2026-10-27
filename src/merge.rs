use std::{fs, io::Write};

use crate::{
    git::{self, RebaseResult}, llm, scheduler::Ctx,
    state::{self, TaskState}, testrun, workers::patch,
};

const REPAIR_USD: f64 = 0.02;

pub fn gate(ctx: &mut Ctx, id: &str, n: u8) -> anyhow::Result<()> {
    let integ = state::integration_dir(&ctx.repo);
    let wt = ctx.wt(id, n);
    let rd = state::run_dir(&ctx.repo, id)?;
    let mut glog = fs::OpenOptions::new().create(true).append(true).open(rd.join("gate.log"))?;
    let prev = git::head(&integ)?;
    ctx.set(id, TaskState::Merging { prev_integration: prev.clone() })?;

    if let RebaseResult::Conflict(files) = git::rebase(&wt, "notary/integration")? {
        ctx.st.collisions.rebase_conflicts += 1;
        ctx.log("rebase_conflict", Some(id), serde_json::json!({ "files": files }))?;
        writeln!(glog, "a{n}: rebase conflict in {}", files.join(", "))?;
        let repaired = repair(ctx, id, n, &files).unwrap_or_else(|e| { eprintln!("repair {id}: {e:#}"); false });
        ctx.log("repair", Some(id), serde_json::json!({ "ok": repaired, "files": files }))?;
        if repaired {
            ctx.st.collisions.repairs_ok += 1;
        } else {
            let _ = git::rebase_abort(&wt);
            ctx.st.collisions.wasted_attempts += 1;
            return back_to_pending(ctx, id, n, format!("rebase conflict in {} could not be repaired", files.join(", ")));
        }
    }
    let imp = git::head(&wt)?;
    ctx.st.tasks.get_mut(id).unwrap().impl_sha = Some(imp.clone());

    if let Err(e) = git::ff_merge(&integ, &format!("notary/{id}-a{n}")) {
        return back_to_pending(ctx, id, n, format!("fast-forward failed: {e:#}"));
    }

    let mut problems: Vec<String> = Vec::new();
    let mut contradicts: Vec<(String, Vec<String>)> = Vec::new();
    if !testrun::regression(ctx.cfg, &integ, &rd.join(format!("gate-a{n}-regression.log")))? {
        problems.push("regression suite red".into());
    }
    if !testrun::integrity(ctx.cfg, &integ, &rd.join(format!("gate-a{n}-integrity.log")))? {
        problems.push("locked files modified".into());
    }
    if !testrun::task(ctx.cfg, &integ, id, &rd.join(format!("gate-a{n}.tap")))?.passed {
        problems.push("own acceptance test fails after rebase".into());
    }
    let merged: Vec<String> = ctx.st.order.iter().filter(|t| ctx.st.tasks[*t].state == TaskState::Merged).cloned().collect();
    for m in &merged {
        let tap = rd.join(format!("gate-a{n}-{m}.tap"));
        if !testrun::task(ctx.cfg, &integ, m, &tap)?.passed {
            contradicts.push((m.clone(), testrun::failing_names(&fs::read_to_string(&tap).unwrap_or_default())));
        }
    }
    writeln!(glog, "a{n}: gate on {} -> problems {:?}, broke {:?}", &imp[..8], problems, contradicts)?;

    if problems.is_empty() && contradicts.is_empty() {
        let tip = git::head(&integ)?;
        let t = ctx.st.tasks.get_mut(id).unwrap();
        t.merged_sha = Some(tip.clone());
        t.confidence = Some("local".into());
        t.attempts.last_mut().map(|a| a.outcome = "merged".into());
        t.notes.push(format!("merged at {} after gate (runs/{id}/gate.log)", &tip[..8]));
        ctx.st.last_merged = Some(id.to_string());
        ctx.set(id, TaskState::Merged)?;
        ctx.log("merged", Some(id), serde_json::json!({ "sha": tip, "attempt": n }))?;
        remove_tree(ctx, id, n);
        recheck_pending(ctx, id)?;
        return Ok(());
    }

    git::reset_hard(&integ, &prev)?;
    ctx.st.collisions.reverts += 1;
    ctx.log("reverted", Some(id), serde_json::json!({ "to": prev, "problems": problems, "broke": contradicts }))?;
    // A task that merged after this attempt started was never visible to the agent; breaking it
    // is a merge collision (usually a lossy conflict repair), not a contradiction. Requeue from the tip.
    let start = ctx.st.tasks[id].start_sha.clone().unwrap_or_default();
    let (stale, seen): (Vec<_>, Vec<_>) = contradicts.iter().partition(|(other, _)| {
        ctx.st.tasks[other].merged_sha.as_ref().is_some_and(|m| !git::is_ancestor(&integ, m, &start).unwrap_or(false))
    });
    if seen.is_empty() && !stale.is_empty() {
        ctx.st.collisions.wasted_attempts += 1;
        let broke = stale.iter().map(|(o, names)| format!("{o} ({})", names.join("; "))).collect::<Vec<_>>().join(", ");
        let t = ctx.st.tasks.get_mut(id).unwrap();
        if t.bonus_attempts == 0 { t.bonus_attempts = 1; }
        ctx.log("collision", Some(id), serde_json::json!({ "broke": broke, "bonus_attempt": true }))?;
        return back_to_pending(ctx, id, n, format!("merge collision: broke {broke}, which merged after this attempt started; \
            retrying from the current tip, where that code is visible. Keep its behaviour"));
    }
    // Seen and still broken: usually agent error, so retry with feedback. Planned contradictions never
    // get here (the loser is blocked at dispatch); if retries run out the reason names the suspect.
    if let Some((other, names)) = seen.first() {
        return back_to_pending(ctx, id, n, format!("broke {other}'s acceptance at the merge gate ({}); possible contradiction with {other}",
            names.join("; ")));
    }
    back_to_pending(ctx, id, n, format!("merge gate red: {}", problems.join(", ")))
}

fn back_to_pending(ctx: &mut Ctx, id: &str, n: u8, reason: String) -> anyhow::Result<()> {
    remove_tree(ctx, id, n);
    crate::scheduler::fail(ctx, id, n, reason)
}

fn remove_tree(ctx: &Ctx, id: &str, n: u8) {
    let wt = ctx.wt(id, n);
    if wt.exists() && git::worktree_remove(&ctx.repo, &wt).is_err() { let _ = fs::remove_dir_all(&wt); }
}

/// After a merge, any Pending task whose acceptance now passes was done as a side effect.
fn recheck_pending(ctx: &mut Ctx, by: &str) -> anyhow::Result<()> {
    let integ = state::integration_dir(&ctx.repo);
    let pending: Vec<String> = ctx.st.order.iter().filter(|t| ctx.st.tasks[*t].state == TaskState::Pending).cloned().collect();
    for p in pending {
        let tap = state::run_dir(&ctx.repo, &p)?.join(format!("after-{by}.tap"));
        if testrun::task(ctx.cfg, &integ, &p, &tap)?.passed {
            ctx.st.tasks.get_mut(&p).unwrap().notes.push(format!("acceptance passes after {by} merged (runs/{p}/after-{by}.tap)"));
            ctx.set(&p, TaskState::AlreadySatisfied { by: Some(by.to_string()) })?;
            ctx.log("already_satisfied", Some(&p), serde_json::json!({ "by": by }))?;
        }
    }
    Ok(())
}

/// One cheap-model call per conflicted file. Returns Ok(true) once the rebase is complete.
fn repair(ctx: &mut Ctx, id: &str, n: u8, files: &[String]) -> anyhow::Result<bool> {
    let slot = ctx.cfg.slots.iter().find(|s| s.kind == "patch" && s.provider.is_some())
        .ok_or_else(|| anyhow::anyhow!("no patch slot to repair with"))?.clone();
    let provider = llm::provider(ctx.cfg, slot.provider.as_deref().unwrap())?;
    let res = format!("{id}-a{n}-repair");
    let amount = ctx.st.ledger.available_for(id).min(REPAIR_USD);
    if amount <= 0.0 || ctx.st.ledger.reserve(&res, id, amount).is_err() {
        ctx.log("budget_refused", Some(id), serde_json::json!({ "for": "repair" }))?;
        return Ok(false);
    }
    let wt = ctx.wt(id, n);
    let start = ctx.st.tasks[id].start_sha.clone().unwrap_or_default();
    let others: Vec<String> = ctx.st.order.iter()
        .filter(|t| ctx.st.tasks[*t].state == TaskState::Merged)
        .filter(|t| ctx.st.tasks[*t].merged_sha.as_ref().is_some_and(|m| !git::is_ancestor(&wt, m, &start).unwrap_or(true)))
        .map(|t| format!("Ticket {t}: {}\n{}", ctx.st.tasks[t].title, ctx.st.tasks[t].body)).collect();
    let mine = &ctx.st.tasks[id];
    let tickets = format!("Ticket {id}: {}\n{}\n\n{}", mine.title, mine.body, others.join("\n\n"));
    let mut spent = 0.0;
    let mut basis = provider.basis;
    let mut ok = true;
    for f in files {
        let text = fs::read_to_string(wt.join(f))?;
        let user = format!("Two changes to this file conflict during a git rebase. Resolve it keeping BOTH changes.\n\
The part between <<<<<<< and ======= is code ALREADY MERGED by other tickets: keep every rule and special case in it. \
The part between ======= and >>>>>>> is the change for ticket {id}: add it on top, without dropping anything already merged.\n\n{tickets}\n\n=== FILE: {f} ===\n{text}\n=== END FILE ===\n\nReturn the whole resolved file in the same === FILE: ... === / === END FILE === format, with no conflict markers.");
        let reply = llm::chat(&provider, &ctx.cfg.prices, &slot.model, "You resolve git merge conflicts precisely.", &user, 180);
        let reply = match reply { Ok(r) => r, Err(e) => { eprintln!("repair {id} {f}: {e:#}"); ok = false; break; } };
        spent += reply.cost_usd;
        basis = reply.basis;
        fs::write(state::run_dir(&ctx.repo, id)?.join(format!("repair-a{n}.log")), &reply.text)?;
        match patch::parse_files(&reply.text).into_iter().find(|(p, _)| p == f) {
            Some((_, content)) if !content.contains("<<<<<<<") && !content.contains(">>>>>>>") => {
                fs::write(wt.join(f), content)?;
                git::add(&wt, f)?;
            }
            _ => { ok = false; break; }
        }
    }
    ctx.st.ledger.settle(&res, id, spent, &format!("{basis:?}").to_lowercase());
    ctx.log("settle", Some(id), serde_json::json!({ "res": res, "cost": spent }))?;
    Ok(ok && git::rebase_continue(&wt).is_ok())
}
