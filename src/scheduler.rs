use std::{
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

use crate::{
    config::Config, events, git, llm, merge, planner, probe, prompt, report,
    state::{self, Attempt, CostBasis, State, TaskState},
    testrun, verify,
    workers::{claude_code::{self, ClaudeCodeWorker}, patch::PatchWorker, Job, Msg, Worker},
};

const PATCH_MIN_USD: f64 = 0.005;

pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub repo: PathBuf,
    pub nd: PathBuf,
    pub sp: PathBuf,
    pub st: State,
}

impl Ctx<'_> {
    pub fn set(&mut self, id: &str, to: TaskState) -> anyhow::Result<()> {
        let t = self.st.tasks.get_mut(id).ok_or_else(|| anyhow::anyhow!("unknown task {id}"))?;
        let from = t.state.clone();
        t.transition(to.clone())?;
        self.st.save(&self.sp)?;
        events::log(&self.nd, "state", Some(id), serde_json::json!({ "from": from, "to": to }))
    }
    pub fn log(&self, kind: &str, task: Option<&str>, detail: serde_json::Value) -> anyhow::Result<()> {
        self.st.save(&self.sp)?;
        events::log(&self.nd, kind, task, detail)
    }
    pub fn wt(&self, id: &str, n: u8) -> PathBuf { self.nd.join("trees").join(format!("{id}-a{n}")) }
}

enum Pick { Task(String, f64), Nothing, BudgetExhausted }

fn build_workers(cfg: &Config) -> anyhow::Result<Vec<Arc<dyn Worker>>> {
    cfg.slots.iter().map(|s| -> anyhow::Result<Arc<dyn Worker>> {
        Ok(match s.kind.as_str() {
            "patch" => {
                let name = s.provider.clone().ok_or_else(|| anyhow::anyhow!("patch slot needs a provider"))?;
                Arc::new(PatchWorker { cfg: cfg.clone(), provider: llm::provider(cfg, &name)?, provider_name: name, model: s.model.clone() })
            }
            "claude-code" => Arc::new(ClaudeCodeWorker::new(&s.model, s.command.as_deref())),
            k => anyhow::bail!("unknown slot kind {k}"),
        })
    }).collect()
}

pub fn run(cfg: &Config, repo: &Path, resume: bool) -> anyhow::Result<()> {
    let started = Instant::now();
    let nd = state::notary_dir(repo);
    let sp = state::state_path(repo);
    let st = if resume {
        anyhow::ensure!(sp.exists(), "nothing to resume: {} not found", sp.display());
        State::load(&sp)?
    } else {
        let mut st = probe::ensure_init(cfg, repo)?;
        anyhow::ensure!(st.tasks.values().all(|t| t.attempts.is_empty()),
            "a run already happened here; use `notary resume`, or start from a fresh repo");
        probe::preflight(cfg, repo)?;
        if !st.planned { probe::probe(cfg, repo, &mut st, false)?; }
        planner::plan(cfg, repo, &mut st)?;
        st
    };
    let mut ctx = Ctx { cfg, repo: repo.to_path_buf(), nd, sp, st };
    ctx.st.ledger.cap_usd = cfg.budget.run_cap_usd;
    ctx.st.ledger.task_ceiling_usd = cfg.budget.task_ceiling_usd;
    if resume { recover(&mut ctx)?; }
    ctx.log("run_started", None, serde_json::json!({ "resume": resume, "cap_usd": cfg.budget.run_cap_usd }))?;

    let workers = build_workers(cfg)?;
    for (i, w) in workers.iter().enumerate() { println!("slot {}: {}", i + 1, w.name()); }
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut busy: Vec<Option<String>> = vec![None; workers.len()];
    let mut claude_used = ctx.st.tasks.values().any(|t| t.attempts.iter().any(|a| a.worker.starts_with("claude-code")));
    let mut out_of_budget = false;

    loop {
        for slot in 0..workers.len() {
            while busy[slot].is_none() && !out_of_budget {
                match pick(&mut ctx, slot, &workers, &busy, claude_used)? {
                    Pick::Nothing => break,
                    Pick::BudgetExhausted => {
                        if busy.iter().all(Option::is_none) {
                            out_of_budget = true;
                            ctx.log("budget_refused", None, serde_json::json!({ "available": ctx.st.ledger.cap_usd - ctx.st.ledger.spent_usd }))?;
                        }
                        break;
                    }
                    Pick::Task(id, amount) => match dispatch(&mut ctx, slot, &workers[slot], &id, amount, &tx) {
                        Ok(true) => {
                            if workers[slot].is_claude() { claude_used = true; }
                            println!("dispatch: {id} -> slot {} ({}), reserved ${amount:.3}", slot + 1, workers[slot].name());
                            busy[slot] = Some(id);
                        }
                        Ok(false) => {}
                        Err(e) => {
                            eprintln!("dispatch {id} failed: {e:#}");
                            ctx.st.ledger.reserved.retain(|k, _| !k.starts_with(&format!("{id}-")));
                            if !ctx.st.tasks[&id].state.is_terminal() {
                                ctx.set(&id, TaskState::Blocked { reason: format!("dispatch failed: {e:#}") })?;
                            }
                        }
                    },
                }
            }
        }
        if busy.iter().all(Option::is_none) { break; }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Msg::Done { slot, task, attempt, result }) => {
                busy[slot] = None;
                let name = workers[slot].name();
                if let Err(e) = handle_done(&mut ctx, &name, &task, attempt, result) {
                    eprintln!("handling {task} failed: {e:#}");
                    if !ctx.st.tasks[&task].state.is_terminal() {
                        ctx.set(&task, TaskState::Blocked { reason: format!("harness error: {e:#}") })?;
                    }
                }
                println!("done: {task} a{attempt} -> {}", short(&ctx.st.tasks[&task].state));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!("scheduler holds a sender"),
        }
    }
    finalize(&mut ctx, started, out_of_budget)
}

fn short(s: &TaskState) -> String {
    match s {
        TaskState::Blocked { reason } | TaskState::Skipped { reason } => format!("{s:?}").split(' ').next().unwrap_or("").to_string() + ": " + reason,
        other => format!("{other:?}"),
    }
}

fn dep_ok(st: &State, dep: &str) -> Option<bool> {
    match st.tasks.get(dep).map(|t| &t.state) {
        None | Some(TaskState::Merged) | Some(TaskState::AlreadySatisfied { .. }) => Some(true),
        Some(TaskState::Blocked { .. }) | Some(TaskState::Skipped { .. }) => None,
        _ => Some(false),
    }
}

fn pick(ctx: &mut Ctx, slot: usize, workers: &[Arc<dyn Worker>], busy: &[Option<String>], claude_used: bool) -> anyhow::Result<Pick> {
    loop {
        let doomed = ctx.st.order.iter().find_map(|id| {
            let t = &ctx.st.tasks[id];
            if t.state != TaskState::Pending { return None; }
            t.deps.iter().find(|d| dep_ok(&ctx.st, d).is_none()).map(|d| (id.clone(), d.clone()))
        });
        let Some((id, dep)) = doomed else { break };
        if ctx.st.tasks[&id].contradicts.as_deref() == Some(dep.as_str()) {
            let t = ctx.st.tasks.get_mut(&id).unwrap();
            t.deps.retain(|d| *d != dep);
            t.contradicts = None;
            t.notes.push(format!("contradiction winner {dep} did not land, so {id} is tried instead"));
            ctx.log("contradiction_released", Some(&id), serde_json::json!({ "winner": dep }))?;
            continue;
        }
        ctx.set(&id, TaskState::Skipped { reason: format!("depends on {dep}, which is {}", short(&ctx.st.tasks[&dep].state)) })?;
    }

    let w = &workers[slot];
    let has_claude = workers.iter().any(|w| w.is_claude());
    let running_areas: Vec<String> = busy.iter().flatten().filter_map(|id| ctx.st.tasks[id].area.clone()).collect();
    let eligible: Vec<String> = ctx.st.order.iter().filter(|id| {
        let t = &ctx.st.tasks[*id];
        t.state == TaskState::Pending
            && t.attempts.len() < t.max_attempts(ctx.cfg.policy.max_attempts)
            && t.deps.iter().all(|d| dep_ok(&ctx.st, d) == Some(true))
            && !t.area.as_ref().is_some_and(|a| running_areas.contains(a))
            && t.attempts.last().is_none_or(|a| a.worker != w.name())
    }).cloned().collect();

    let preferred: Vec<String> = if w.is_claude() {
        let patch_all_busy = workers.iter().zip(busy).filter(|(w, _)| !w.is_claude()).all(|(_, b)| b.is_some());
        let mut v: Vec<String> = eligible.iter().filter(|id| { let t = &ctx.st.tasks[*id]; !t.attempts.is_empty() || t.hard }).cloned().collect();
        if !claude_used || patch_all_busy {
            let rest: Vec<String> = eligible.iter().filter(|id| !v.contains(id)).cloned().collect();
            v.extend(rest);
        }
        v
    } else {
        let claude_busy = workers.iter().zip(busy).any(|(w, b)| w.is_claude() && b.is_some());
        let l = &ctx.st.ledger;
        let claude_unaffordable = l.cap_usd - l.spent_usd - l.reserved.values().sum::<f64>() < claude_code::MIN_USD;
        let mut v: Vec<String> = eligible.into_iter().filter(|id| {
            let t = &ctx.st.tasks[id];
            match t.attempts.last() {
                None => !t.hard || !has_claude || claude_busy || claude_unaffordable,
                // retries prefer Claude Code, but another patch model may take one rather than leave it idle
                Some(a) => a.worker.starts_with("claude-code") || !has_claude || claude_busy || claude_unaffordable,
            }
        }).collect();
        v.sort_by_key(|id| ctx.st.tasks[id].hard);
        v
    };

    let l = &ctx.st.ledger;
    let need = if w.is_claude() { claude_code::MIN_USD } else { PATCH_MIN_USD };
    let ceiling_share = if w.is_claude() { l.task_ceiling_usd } else { l.task_ceiling_usd / 2.0 };
    for id in preferred {
        let amount = l.available_for(&id).min(ceiling_share);
        if amount >= need { return Ok(Pick::Task(id, amount)); }
    }
    let run_left = l.cap_usd - l.spent_usd - l.reserved.values().sum::<f64>();
    Ok(if run_left < PATCH_MIN_USD { Pick::BudgetExhausted } else { Pick::Nothing })
}

fn dispatch(ctx: &mut Ctx, slot: usize, worker: &Arc<dyn Worker>, id: &str, amount: f64, tx: &mpsc::Sender<Msg>) -> anyhow::Result<bool> {
    let n = ctx.st.tasks[id].attempts.len() as u8 + 1;
    let integ = state::integration_dir(&ctx.repo);
    let start = git::head(&integ)?;
    let wt = ctx.wt(id, n);
    if wt.exists() {
        let _ = git::worktree_remove(&ctx.repo, &wt);
        let _ = fs::remove_dir_all(&wt);
    }
    git::worktree_add(&ctx.repo, &wt, &format!("notary/{id}-a{n}"), &start)?;
    let rd = state::run_dir(&ctx.repo, id)?;

    let before_tap = rd.join(format!("before-a{n}.tap"));
    let before = testrun::task(ctx.cfg, &wt, id, &before_tap)?;
    if before.passed {
        let by = ctx.st.last_merged.clone();
        ctx.st.tasks.get_mut(id).unwrap().notes.push(format!("acceptance already passes at {} (runs/{id}/before-a{n}.tap)", &start[..8]));
        ctx.set(id, TaskState::AlreadySatisfied { by: by.clone() })?;
        ctx.log("already_satisfied", Some(id), serde_json::json!({ "by": by, "at": start }))?;
        let _ = git::worktree_remove(&ctx.repo, &wt);
        return Ok(false);
    }

    if let Some(winner) = ctx.st.tasks[id].contradicts.clone() {
        let names = testrun::failing_names(&fs::read_to_string(&before_tap).unwrap_or_default());
        let why = ctx.st.tasks[id].contradiction_why.clone().unwrap_or_default();
        let reason = format!("contradicts {winner}: {why}; failing on top of {winner}: {} (runs/{id}/before-a{n}.tap)", names.join("; "));
        ctx.st.tasks.get_mut(id).unwrap().notes.push("blocked before any spend; no worker was called".into());
        ctx.set(id, TaskState::Blocked { reason })?;
        let _ = git::worktree_remove(&ctx.repo, &wt);
        return Ok(false);
    }

    let res = format!("{id}-a{n}");
    if let Err(e) = ctx.st.ledger.reserve(&res, id, amount) {
        ctx.log("budget_refused", Some(id), serde_json::json!({ "error": e.to_string() }))?;
        let _ = git::worktree_remove(&ctx.repo, &wt);
        return Ok(false);
    }
    ctx.log("reserve", Some(id), serde_json::json!({ "res": res, "amount": amount }))?;
    {
        let t = ctx.st.tasks.get_mut(id).unwrap();
        t.start_sha = Some(start.clone());
        t.impl_sha = None;
    }
    ctx.set(id, TaskState::Running { slot, attempt: n })?;
    ctx.log("dispatch", Some(id), serde_json::json!({ "slot": slot, "worker": worker.name(), "attempt": n, "start": start, "max_usd": amount }))?;

    let feedback = if n > 1 { Some(retry_feedback(ctx, id, n - 1)) } else { None };
    let task = ctx.st.tasks[id].clone();
    let prompt = prompt::task_prompt(ctx.cfg, &wt, &task, feedback.as_deref(), !worker.is_claude())?;
    fs::write(rd.join(format!("prompt-a{n}.md")), &prompt)?;
    let job = Job {
        task_id: id.to_string(), task, attempt: n, prompt, worktree: wt, run_dir: rd,
        test_cmd: ctx.cfg.repo.test_task.replace("{id}", id), max_usd: amount, timeout_secs: ctx.cfg.policy.timeout_secs,
    };
    let (w, tx) = (Arc::clone(worker), tx.clone());
    std::thread::spawn(move || {
        let result = w.run(&job);
        let _ = tx.send(Msg::Done { slot, task: job.task_id.clone(), attempt: job.attempt, result });
    });
    Ok(true)
}

fn retry_feedback(ctx: &Ctx, id: &str, prev: u8) -> String {
    let t = &ctx.st.tasks[id];
    let reason = t.attempts.last().map(|a| format!("Attempt {prev} by {} failed: {}", a.worker, a.outcome)).unwrap_or_default();
    let rd = ctx.nd.join("runs").join(id);
    let tap = [format!("broke-a{prev}.tap"), format!("after-a{prev}.tap"), format!("gate-a{prev}.tap")].iter()
        .map(|f| rd.join(f)).find(|p| p.exists())
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| testrun::failures(&s, 60)).unwrap_or_default();
    format!("{reason}\n{tap}")
}

fn handle_done(ctx: &mut Ctx, worker: &str, id: &str, n: u8, result: anyhow::Result<crate::workers::Outcome>) -> anyhow::Result<()> {
    let res = format!("{id}-a{n}");
    let reserved = ctx.st.ledger.reserved.get(&res).copied().unwrap_or(0.0);
    let (cost, basis, summary, error) = match result {
        Ok(o) => (o.cost_usd, o.basis, o.summary, o.error),
        Err(e) => (reserved, CostBasis::Estimated, String::new(), Some(format!("{e:#}"))),
    };
    let basis_s = format!("{basis:?}").to_lowercase();
    ctx.st.ledger.settle(&res, id, cost, &basis_s);
    ctx.st.tasks.get_mut(id).unwrap().attempts.push(Attempt { n, worker: worker.to_string(), cost_usd: cost, basis, outcome: "returned".into() });
    ctx.log("settle", Some(id), serde_json::json!({ "res": res, "reserved": reserved, "cost": cost, "basis": basis_s, "over": cost > reserved + 1e-9 }))?;
    ctx.log("worker_done", Some(id), serde_json::json!({ "worker": worker, "attempt": n, "summary": summary, "error": error }))?;

    if !git::is_clean(&ctx.repo)? {
        let msg = format!("notary: stray edits from {id}-a{n} ({worker})");
        git::stash_all(&ctx.repo, &msg)?;
        ctx.log("isolation_breach", Some(id), serde_json::json!({ "worker": worker, "stash": msg }))?;
        return fail(ctx, id, n, format!("wrote outside its worktree; the edits were stashed in the main checkout as \"{msg}\""));
    }
    if let Some(e) = error { return fail(ctx, id, n, e); }
    finish_attempt(ctx, id, n)
}

/// Commit, verify and merge attempt `n`. The task must be Running.
fn finish_attempt(ctx: &mut Ctx, id: &str, n: u8) -> anyhow::Result<()> {
    let wt = ctx.wt(id, n);
    let title = ctx.st.tasks[id].title.clone();
    let imp = match git::commit_all(&wt, &format!("{id}: {title} (notary attempt {n})"))? {
        Some(sha) => sha,
        // resume: the commit was made before the crash
        None => match ctx.st.tasks[id].impl_sha.clone().filter(|s| git::head(&wt).ok().as_ref() == Some(s)) {
            Some(sha) => sha,
            None => return fail(ctx, id, n, "agent changed nothing".into()),
        },
    };
    ctx.st.tasks.get_mut(id).unwrap().impl_sha = Some(imp.clone());
    ctx.set(id, TaskState::Implemented)?;
    let start = ctx.st.tasks[id].start_sha.clone().unwrap_or_default();
    let verdict = verify::verify(ctx, id, n, &wt, &start, &imp)?;
    ctx.log("verify", Some(id), serde_json::json!({ "attempt": n, "ok": verdict.is_ok(), "reason": verdict.as_ref().err() }))?;
    match verdict {
        Ok(()) => {
            ctx.st.tasks.get_mut(id).unwrap().attempts.last_mut().unwrap().outcome = "verified".into();
            ctx.set(id, TaskState::Verified)?;
            merge::gate(ctx, id, n)
        }
        Err(reason) => fail(ctx, id, n, reason),
    }
}

/// Records why attempt `n` failed; back to Pending, or Blocked once attempts run out.
pub fn fail(ctx: &mut Ctx, id: &str, n: u8, reason: String) -> anyhow::Result<()> {
    let t = ctx.st.tasks.get_mut(id).unwrap();
    let max = t.max_attempts(ctx.cfg.policy.max_attempts);
    let worker = t.attempts.last().map(|a| a.worker.clone()).unwrap_or_default();
    if let Some(a) = t.attempts.last_mut() { a.outcome = reason.clone(); }
    t.notes.push(format!("a{n} ({worker}): {reason}"));
    let used = t.attempts.len();
    let wt = ctx.wt(id, n);
    if wt.exists() && git::worktree_remove(&ctx.repo, &wt).is_err() { let _ = fs::remove_dir_all(&wt); }
    if used >= max {
        // every attempt broke the same task's tests: say so plainly, the planner missed a contradiction
        let broke = |o: &str| o.strip_prefix("broke ").and_then(|r| r.split('\'').next()).map(str::to_string);
        let victims: Vec<Option<String>> = ctx.st.tasks[id].attempts.iter().map(|a| broke(&a.outcome)).collect();
        let suspect = match victims.first() {
            Some(Some(v)) if victims.iter().all(|x| x.as_ref() == Some(v)) => format!(" (possible contradiction with {v})"),
            _ => String::new(),
        };
        ctx.set(id, TaskState::Blocked { reason: format!("failed after {used} attempts{suspect}: {reason}") })
    } else {
        ctx.set(id, TaskState::Pending)
    }
}


/// Settles leftover reservations in full (money may have been spent), then picks each
/// in-flight task back up: re-verify work that exists, re-gate cut-off merges, requeue the rest.
fn recover(ctx: &mut Ctx) -> anyhow::Result<()> {
    let integ = state::integration_dir(&ctx.repo);
    let mut changed = Vec::new();
    let reserved = std::mem::take(&mut ctx.st.ledger.reserved);
    for (res, amount) in &reserved {
        let task = res.split('-').next().unwrap_or(res).to_string();
        ctx.st.ledger.settle_crashed(res, &task, *amount);
        changed.push(format!("settled {res} at ${amount:.3} (estimated)"));
    }
    git::prune(&ctx.repo)?;
    let mut reverify = Vec::new();
    let mut regate = Vec::new();
    for id in ctx.st.order.clone() {
        let state = ctx.st.tasks[&id].state.clone();
        let (n, has_work) = match &state {
            TaskState::Running { slot, attempt } => {
                let cost = reserved.get(&format!("{id}-a{attempt}")).copied().unwrap_or(0.0);
                ctx.st.tasks.get_mut(&id).unwrap().attempts.push(Attempt { n: *attempt, worker: format!("slot {} (interrupted)", slot + 1),
                    cost_usd: cost, basis: CostBasis::Estimated, outcome: "interrupted by crash".into() });
                let wt = ctx.wt(&id, *attempt);
                (*attempt, wt.exists() && !git::is_clean(&wt).unwrap_or(true))
            }
            TaskState::Implemented | TaskState::Verified | TaskState::Merging { .. } => {
                let n = ctx.st.tasks[&id].attempts.len() as u8;
                (n, ctx.wt(&id, n).exists())
            }
            _ => continue,
        };
        if let TaskState::Merging { prev_integration } = &state {
            if git::head(&integ)? != *prev_integration { git::reset_hard(&integ, prev_integration)?; }
        }
        let t = ctx.st.tasks.get_mut(&id).unwrap();
        // recovery is the one place allowed to set a state directly
        let what = if !has_work {
            t.state = TaskState::Pending;
            "requeued"
        } else if matches!(state, TaskState::Merging { .. }) {
            t.state = TaskState::Verified;
            regate.push((id.clone(), n));
            "merge gate rerun"
        } else {
            t.state = TaskState::Running { slot: 0, attempt: n };
            reverify.push((id.clone(), n));
            "verified again"
        };
        t.notes.push(format!("a{n} interrupted in {state:?}; {what} by resume"));
        if !has_work {
            let wt = ctx.wt(&id, n);
            if wt.exists() && git::worktree_remove(&ctx.repo, &wt).is_err() { let _ = fs::remove_dir_all(&wt); }
        }
        changed.push(format!("{id}: {state:?} -> {what}"));
    }
    ctx.log("crash_recovered", None, serde_json::json!({ "changes": changed }))?;
    for c in &changed { println!("resume: {c}"); }
    for (id, n) in reverify {
        if let Err(e) = finish_attempt(ctx, &id, n) {
            if !ctx.st.tasks[&id].state.is_terminal() { ctx.set(&id, TaskState::Blocked { reason: format!("harness error on resume: {e:#}") })?; }
        }
        println!("resume: {id} -> {}", short(&ctx.st.tasks[&id].state));
    }
    for (id, n) in regate {
        if let Err(e) = merge::gate(ctx, &id, n) {
            if !ctx.st.tasks[&id].state.is_terminal() { ctx.set(&id, TaskState::Blocked { reason: format!("harness error on resume: {e:#}") })?; }
        }
        println!("resume: {id} -> {}", short(&ctx.st.tasks[&id].state));
    }
    Ok(())
}

fn finalize(ctx: &mut Ctx, started: Instant, out_of_budget: bool) -> anyhow::Result<()> {
    let max = ctx.cfg.policy.max_attempts;
    let l = &ctx.st.ledger;
    let run_left = (l.cap_usd - l.spent_usd - l.reserved.values().sum::<f64>()).max(0.0);
    let cap = l.cap_usd;
    for id in ctx.st.order.clone() {
        let t = &ctx.st.tasks[&id];
        if t.state != TaskState::Pending { continue; }
        let waiting = t.deps.iter().find(|d| dep_ok(&ctx.st, d) != Some(true)).cloned();
        let task_left = ctx.st.ledger.available_for(&id);
        let reason = if t.attempts.len() >= t.max_attempts(max) { "no attempts left".to_string() }
            else if let Some(d) = waiting { format!("waiting on {d}, which never merged") }
            else if out_of_budget || run_left < claude_code::MIN_USD {
                format!("budget cap reached: ${run_left:.3} of ${cap:.2} left, too little to start another attempt") }
            else if task_left < PATCH_MIN_USD { "per-task budget ceiling reached".to_string() }
            else { "no free slot could take it (retry needs a different worker)".to_string() };
        ctx.set(&id, TaskState::Skipped { reason })?;
    }

    let integ = state::integration_dir(&ctx.repo);
    let log = ctx.nd.join("final-grade.log");
    crate::proc::run(crate::proc::sh(&ctx.cfg.repo.grade, &integ), &log, &log.with_extension("err"), Duration::from_secs(600))?;
    let grade_src = integ.join(&ctx.cfg.repo.grade_report);
    let grade: serde_json::Value = serde_json::from_str(&fs::read_to_string(&grade_src).unwrap_or_else(|_| "{}".into()))?;
    fs::write(ctx.nd.join("final-grade.json"), serde_json::to_vec_pretty(&grade)?)?;
    let mut discrepancies = Vec::new();
    for id in &ctx.st.order {
        let t = &ctx.st.tasks[id];
        let claimed = matches!(t.state, TaskState::Merged | TaskState::AlreadySatisfied { .. });
        let passed = grade["tasks"][id]["passed"].as_bool().unwrap_or(false);
        if claimed != passed {
            discrepancies.push(format!("{id}: Notary says {} but grader says passed={passed}", short(&t.state)));
        }
    }
    ctx.log("final_grade", None, serde_json::json!({ "summary": grade["summary"], "discrepancies": discrepancies }))?;
    print!("{}", fs::read_to_string(&log).unwrap_or_default());
    for d in &discrepancies { println!("DISCREPANCY {d}"); }

    if ctx.cfg.policy.fast_forward_main && discrepancies.is_empty() {
        match git::ff_merge(&ctx.repo, "notary/integration") {
            Ok(()) => println!("main fast-forwarded to notary/integration"),
            Err(e) => println!("could not fast-forward main: {e:#}"),
        }
    }
    if let Ok(entries) = fs::read_dir(ctx.nd.join("trees")) {
        for e in entries.flatten().filter(|e| e.file_name() != "integration") {
            let _ = git::worktree_remove(&ctx.repo, &e.path());
        }
    }
    ctx.st.elapsed_secs += started.elapsed().as_secs();
    ctx.st.save(&ctx.sp)?;
    report::write(ctx.cfg, &ctx.st, &ctx.nd, &discrepancies)?;
    let report = serde_json::json!({
        "elapsed_secs": ctx.st.elapsed_secs,
        "ledger": ctx.st.ledger.clone(),
        "collisions": serde_json::to_value(&ctx.st.collisions)?,
        "tasks": ctx.st.order.iter().map(|id| serde_json::to_value(&ctx.st.tasks[id])).collect::<Result<Vec<_>, _>>()?,
        "grade": grade, "discrepancies": discrepancies,
    });
    fs::write(ctx.nd.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    report::status(&ctx.repo)
}
