use std::{
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

use crate::{
    analyzer, config::Config, events, git, llm, merge, probe, prompt, report,
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
        probe::probe(cfg, repo, &mut st)?;
        for d in analyzer::analyze(cfg, &state::integration_dir(repo), &st)? {
            let t = st.tasks.get_mut(&d.task).unwrap();
            if !t.deps.contains(&d.on) { t.deps.push(d.on.clone()); }
        }
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
        ctx.set(&id, TaskState::Skipped { reason: format!("depends on {dep}, which is {}", short(&ctx.st.tasks[&dep].state)) })?;
    }

    let w = &workers[slot];
    let max = ctx.cfg.policy.max_attempts as usize;
    let has_claude = workers.iter().any(|w| w.is_claude());
    let running_areas: Vec<String> = busy.iter().flatten().filter_map(|id| ctx.st.tasks[id].area.clone()).collect();
    let eligible: Vec<String> = ctx.st.order.iter().filter(|id| {
        let t = &ctx.st.tasks[*id];
        t.state == TaskState::Pending
            && t.attempts.len() < max
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
        eligible.into_iter().filter(|id| {
            let t = &ctx.st.tasks[id];
            match t.attempts.last() {
                None => !t.hard || !has_claude,
                Some(a) => a.worker.starts_with("claude-code") || !has_claude,
            }
        }).collect()
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
    let tap = [format!("after-a{prev}.tap"), format!("gate-a{prev}.tap")].iter()
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

    if let Some(e) = error { return fail(ctx, id, n, e); }
    let wt = ctx.wt(id, n);
    let title = ctx.st.tasks[id].title.clone();
    let Some(imp) = git::commit_all(&wt, &format!("{id}: {title} (notary attempt {n})"))? else {
        return fail(ctx, id, n, "agent changed nothing".into());
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
    let max = ctx.cfg.policy.max_attempts as usize;
    let t = ctx.st.tasks.get_mut(id).unwrap();
    let worker = t.attempts.last().map(|a| a.worker.clone()).unwrap_or_default();
    if let Some(a) = t.attempts.last_mut() { a.outcome = reason.clone(); }
    t.notes.push(format!("a{n} ({worker}): {reason}"));
    let used = t.attempts.len();
    let wt = ctx.wt(id, n);
    if wt.exists() && git::worktree_remove(&ctx.repo, &wt).is_err() { let _ = fs::remove_dir_all(&wt); }
    if used >= max {
        ctx.set(id, TaskState::Blocked { reason: format!("failed after {used} attempts: {reason}") })
    } else {
        ctx.set(id, TaskState::Pending)
    }
}


fn recover(ctx: &mut Ctx) -> anyhow::Result<()> {
    let integ = state::integration_dir(&ctx.repo);
    let mut changed = Vec::new();
    for (res, amount) in std::mem::take(&mut ctx.st.ledger.reserved) {
        let task = res.split('-').next().unwrap_or(&res).to_string();
        ctx.st.ledger.settle(&res, &task, amount, "estimated");
        changed.push(format!("settled {res} at ${amount:.3} (estimated)"));
    }
    for id in ctx.st.order.clone() {
        let state = ctx.st.tasks[&id].state.clone();
        let n = match &state {
            TaskState::Running { attempt, .. } => {
                let t = ctx.st.tasks.get_mut(&id).unwrap();
                t.attempts.push(Attempt { n: *attempt, worker: "unknown (crashed)".into(), cost_usd: 0.0, basis: CostBasis::Estimated, outcome: "interrupted by crash".into() });
                *attempt
            }
            TaskState::Implemented | TaskState::Verified => ctx.st.tasks[&id].attempts.len() as u8,
            TaskState::Merging { prev_integration } => {
                if git::head(&integ)? != *prev_integration { git::reset_hard(&integ, prev_integration)?; }
                ctx.st.tasks[&id].attempts.len() as u8
            }
            _ => continue,
        };
        let wt = ctx.wt(&id, n);
        if wt.exists() && git::worktree_remove(&ctx.repo, &wt).is_err() { let _ = fs::remove_dir_all(&wt); }
        let t = ctx.st.tasks.get_mut(&id).unwrap();
        t.notes.push(format!("a{n} interrupted in {state:?}; requeued by resume"));
        t.state = TaskState::Pending; // recovery is the one place allowed to reset in-flight work
        changed.push(format!("{id}: {state:?} -> Pending"));
    }
    git::prune(&ctx.repo)?;
    ctx.log("crash_recovered", None, serde_json::json!({ "changes": changed }))?;
    for c in &changed { println!("resume: {c}"); }
    Ok(())
}

fn finalize(ctx: &mut Ctx, started: Instant, out_of_budget: bool) -> anyhow::Result<()> {
    let max = ctx.cfg.policy.max_attempts as usize;
    for id in ctx.st.order.clone() {
        let t = &ctx.st.tasks[&id];
        if t.state != TaskState::Pending { continue; }
        let waiting = t.deps.iter().find(|d| dep_ok(&ctx.st, d) != Some(true)).cloned();
        let reason = if t.attempts.len() >= max { "no attempts left".to_string() }
            else if out_of_budget { "budget cap reached".to_string() }
            else if let Some(d) = waiting { format!("waiting on {d}, which never merged") }
            else if ctx.st.ledger.available_for(&id) < PATCH_MIN_USD { "per-task budget ceiling reached".to_string() }
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
    let report = serde_json::json!({
        "elapsed_secs": started.elapsed().as_secs(),
        "ledger": ctx.st.ledger.clone(),
        "collisions": serde_json::to_value(&ctx.st.collisions)?,
        "tasks": ctx.st.order.iter().map(|id| serde_json::to_value(&ctx.st.tasks[id])).collect::<Result<Vec<_>, _>>()?,
        "grade": grade, "discrepancies": discrepancies,
    });
    fs::write(ctx.nd.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    report::status(&ctx.repo)
}
