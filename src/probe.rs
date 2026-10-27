use std::{fs, io::Write, path::Path};

use crate::{
    config::Config, events, git,
    state::{self, State, Task, TaskState},
    testrun,
};

const EXCLUDES: [&str; 3] = [".notary/", "notary-shadow/", "grade-report.json"];

/// Creates .notary/, the git excludes, the integration worktree and state.json.
pub fn init(cfg: &Config, repo: &Path) -> anyhow::Result<()> {
    if state::state_path(repo).exists() {
        println!("init: already initialized ({}); `notary clean` and delete .notary to start over",
            state::state_path(repo).display());
        return Ok(());
    }
    ensure_init(cfg, repo).map(|_| ())
}

/// Loads state.json, or initializes the repo first if there is none.
pub fn ensure_init(cfg: &Config, repo: &Path) -> anyhow::Result<State> {
    let sp = state::state_path(repo);
    if sp.exists() {
        return State::load(&sp);
    }
    anyhow::ensure!(git::is_clean(repo)?, "{} has uncommitted changes; init needs a clean repo", repo.display());

    let nd = state::notary_dir(repo);
    for d in ["runs", "trees"] { fs::create_dir_all(nd.join(d))?; }

    let exclude = repo.join(".git").join("info").join("exclude");
    fs::create_dir_all(exclude.parent().unwrap())?;
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&exclude)?;
    for line in EXCLUDES {
        if !existing.lines().any(|l| l.trim() == line) { writeln!(f, "{line}")?; }
    }

    let base_sha = git::head(repo)?;
    git::worktree_add(repo, &state::integration_dir(repo), "notary/integration", &base_sha)?;

    let backlog: serde_json::Value = serde_json::from_str(&fs::read_to_string(repo.join(&cfg.repo.backlog))?)?;
    let mut st = State { base_sha: base_sha.clone(), ..Default::default() };
    st.ledger.cap_usd = cfg.budget.run_cap_usd;
    st.ledger.task_ceiling_usd = cfg.budget.task_ceiling_usd;
    for t in backlog["tasks"].as_array().ok_or_else(|| anyhow::anyhow!("backlog has no tasks array"))? {
        let s = |k: &str| t[k].as_str().unwrap_or("").to_string();
        let id = s("id");
        anyhow::ensure!(!id.is_empty(), "backlog task without an id: {t}");
        st.order.push(id.clone());
        st.tasks.insert(id.clone(), Task::new(&id, &s("title"), &s("body")));
    }
    st.save(&sp)?;
    events::log(&nd, "init", None, serde_json::json!({ "base_sha": base_sha, "tasks": st.order }))?;
    println!("init: {} tasks, base {}, integration worktree at {}",
        st.order.len(), &base_sha[..8], state::integration_dir(repo).display());
    Ok(st)
}

/// Refuses to start unless the repo, tests, lock and budget are all in order.
pub fn preflight(cfg: &Config, repo: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(git::is_clean(repo)?, "preflight: {} is not clean", repo.display());
    let integ = state::integration_dir(repo);
    anyhow::ensure!(integ.exists(), "preflight: integration worktree missing; run `notary init`");
    let nd = state::notary_dir(repo);
    anyhow::ensure!(testrun::regression(cfg, &integ, &nd.join("preflight-test.log"))?,
        "preflight: `{}` is red on integration (see .notary/preflight-test.log)", cfg.repo.test_all);
    anyhow::ensure!(testrun::integrity(cfg, &integ, &nd.join("preflight-integrity.log"))?,
        "preflight: `{}` failed (see .notary/preflight-integrity.log)", cfg.repo.integrity);
    anyhow::ensure!(cfg.budget.run_cap_usd > 0.0 && cfg.budget.task_ceiling_usd > 0.0, "preflight: budget cap must be > 0");
    let mut providers: Vec<&str> = cfg.slots.iter().filter_map(|s| s.provider.as_deref()).collect();
    providers.push(&cfg.planner.provider);
    if let Some(p) = &cfg.planner.fallback_provider { providers.push(p); }
    for p in providers {
        let pc = cfg.providers.get(p).ok_or_else(|| anyhow::anyhow!("preflight: unknown provider {p}"))?;
        anyhow::ensure!(std::env::var(&pc.key_env).is_ok_and(|k| !k.is_empty()),
            "preflight: {} is not set (provider {p})", pc.key_env);
    }
    println!("preflight: repo clean, regression green, lock intact, budget ${:.2}", cfg.budget.run_cap_usd);
    Ok(())
}

/// Runs every Pending task's acceptance test in the integration worktree (at base_sha).
/// `force: false` skips tasks that already have a probe.tap (plan and run after a manual probe).
pub fn probe(cfg: &Config, repo: &Path, st: &mut State, force: bool) -> anyhow::Result<()> {
    let integ = state::integration_dir(repo);
    let nd = state::notary_dir(repo);
    let sp = state::state_path(repo);
    let tip = git::head(&integ)?;
    anyhow::ensure!(tip == st.base_sha, "probe: integration is at {} but base is {}; probe only runs before work starts",
        &tip[..8], &st.base_sha[..8]);
    for id in st.order.clone() {
        if st.tasks[&id].state != TaskState::Pending { continue; }
        let out = state::run_dir(repo, &id)?.join("probe.tap");
        if !force && out.exists() { continue; }
        let r = testrun::task(cfg, &integ, &id, &out)?;
        let label = if r.passed { "pass" } else if r.timed_out { "TIMEOUT" } else { "FAIL" };
        println!("{id:<6} {label} ({} tests, {} failed)", r.tests, r.failed);
        events::log(&nd, "probe", Some(&id),
            serde_json::json!({ "passed": r.passed, "tests": r.tests, "failed": r.failed, "tap": out }))?;
        if r.passed {
            let t = st.tasks.get_mut(&id).unwrap();
            t.transition(TaskState::AlreadySatisfied { by: None })?;
            t.notes.push(format!("acceptance already passes on base {} (runs/{id}/probe.tap)", &st.base_sha[..8]));
            st.save(&sp)?;
            events::log(&nd, "already_satisfied", Some(&id), serde_json::json!({ "by": null }))?;
        }
    }
    st.save(&sp)
}

pub fn probe_cmd(cfg: &Config, repo: &Path) -> anyhow::Result<()> {
    let mut st = ensure_init(cfg, repo)?;
    probe(cfg, repo, &mut st, true)
}
