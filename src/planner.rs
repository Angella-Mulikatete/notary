use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path, sync::mpsc, time::Duration};

use crate::{
    analyzer::{self, Dep}, config::Config, events, llm, probe, prompt,
    state::{self, Contradiction, State},
};

const PLAN_USD: f64 = 0.10;
const PLAN_BUDGET_ID: &str = "_plan";

#[derive(Deserialize, Default, Debug)]
struct PlanTask { #[serde(default)] area: Option<String>, #[serde(default)] hard: bool }
#[derive(Deserialize, Debug)]
struct PlanDep { task: String, on: String, #[serde(default)] why: String }
#[derive(Deserialize, Debug)]
struct PlanContra { a: String, b: String, #[serde(default)] why: String, #[serde(default)] suggest_keep: Option<String> }
#[derive(Deserialize, Default, Debug)]
struct PlanOut {
    #[serde(default)] tasks: BTreeMap<String, PlanTask>,
    #[serde(default)] deps: Vec<PlanDep>,
    #[serde(default)] contradictions: Vec<PlanContra>,
}

const SYSTEM: &str = "You are a senior engineer planning a backlog for parallel coding agents. Reply with one JSON object only.";

const RULES: &str = r#"Plan this backlog. Reply with JSON only, in exactly this shape:
{
  "tasks": { "T01": { "area": "fees", "files": ["src/fees/calculate.js"], "hard": false } },
  "deps": [ { "task": "T10", "on": "T04", "why": "reversal looks up a transfer by id, which T04 adds" } ],
  "contradictions": [ { "a": "T07", "b": "T11", "why": "T07 test expects fee 0 for 1,000; T11 test expects 200", "suggest_keep": "T07" } ]
}
Rules:
- Every task gets an entry in "tasks".
- Tasks that change the same function or the same business rule share an area. Adding separate entries to the route table does NOT count as sharing.
- A dependency means task cannot pass its acceptance test until "on" is merged (it uses something "on" adds). Only list real ones.
- A contradiction is two acceptance tests that cannot both pass on the same code. Quote the conflicting assertions in "why". Do not list mere overlaps.
- "hard" means it needs changes across 3 or more files, or new stored state.
- suggest_keep: the task whose test should win, or the one listed first if unsure."#;

fn src_outline(dir: &Path) -> String {
    let re = regex::Regex::new(r"(?m)^\s*(?:export\s+)?(?:async\s+)?(?:function\s+(\w+)|const\s+(\w+)\s*=)").unwrap();
    prompt::list_files(dir).into_iter().filter(|f| f.starts_with("src/")).map(|f| {
        let text = fs::read_to_string(dir.join(&f)).unwrap_or_default();
        let names: Vec<&str> = re.captures_iter(&text).filter_map(|c| c.get(1).or(c.get(2)).map(|m| m.as_str())).collect();
        format!("{f}: {}", names.join(", "))
    }).collect::<Vec<_>>().join("\n")
}

fn planner_prompt(dir: &Path, acc: &str, st: &State) -> String {
    let mut p = format!("{RULES}\n\n# Backlog\n");
    for id in &st.order {
        let t = &st.tasks[id];
        p.push_str(&format!("\n## {id}: {}\n{}\n", t.title, t.body));
    }
    p.push_str("\n# Acceptance tests (locked)\n");
    for id in &st.order {
        let src = fs::read_to_string(dir.join(acc).join(format!("{id}.test.js"))).unwrap_or_else(|_| "(missing)".into());
        p.push_str(&format!("\n--- {acc}/{id}.test.js ---\n{}\n", src.replace("\r\n", "\n")));
    }
    if let Ok(api) = fs::read_to_string(dir.join("docs").join("api.md")) {
        p.push_str(&format!("\n# docs/api.md\n{}\n", api.replace("\r\n", "\n")));
    }
    p.push_str(&format!("\n# Source files and their functions\n{}\n", src_outline(dir)));
    p
}

fn parse_plan(text: &str) -> anyhow::Result<PlanOut> {
    let start = text.find('{').ok_or_else(|| anyhow::anyhow!("no JSON object in reply"))?;
    let end = text.rfind('}').ok_or_else(|| anyhow::anyhow!("no closing brace in reply"))?;
    Ok(serde_json::from_str(&text[start..=end])?)
}

/// One planner call (plus one retry on a parse error). None if it fails; the analyzer's deps still stand.
fn call_planner(cfg: &Config, repo: &Path, st: &mut State) -> anyhow::Result<Option<PlanOut>> {
    let nd = state::notary_dir(repo);
    let mut models = vec![(cfg.planner.provider.clone(), cfg.planner.model.clone())];
    if let (Some(p), Some(m)) = (&cfg.planner.fallback_provider, &cfg.planner.fallback_model) {
        models.push((p.clone(), m.clone()));
    }
    let amount = st.ledger.available_for(PLAN_BUDGET_ID).min(PLAN_USD);
    if amount <= 0.0 || st.ledger.reserve(PLAN_BUDGET_ID, PLAN_BUDGET_ID, amount).is_err() {
        events::log(&nd, "budget_refused", None, serde_json::json!({ "for": "planner" }))?;
        return Ok(None);
    }
    let user = planner_prompt(&state::integration_dir(repo), &cfg.repo.acceptance_dir, st);
    fs::write(nd.join("planner-prompt.md"), &user)?;
    let mut spent = 0.0;
    let mut basis = crate::state::CostBasis::Computed;
    let mut result = None;
    // an unreachable model falls through to the fallback; an unparseable reply gets one retry
    'models: for (pname, model) in &models {
        let provider = llm::provider(cfg, pname)?;
        let mut ask = user.clone();
        for round in 1..=2 {
            if spent >= amount { break 'models; }
            let reply = match llm::chat(&provider, &cfg.prices, model, SYSTEM, &ask, 240) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("planner: {pname}/{model}: {e:#}");
                    events::log(&nd, "plan", None, serde_json::json!({ "model": model, "error": format!("{e:#}") }))?;
                    continue 'models;
                }
            };
            spent += reply.cost_usd;
            basis = reply.basis;
            fs::write(nd.join(format!("planner-response-{round}.txt")), &reply.text)?;
            match parse_plan(&reply.text) {
                Ok(p) => {
                    println!("plan: planner {pname}/{model}");
                    result = Some(p);
                    break 'models;
                }
                Err(e) => {
                    eprintln!("planner: {model} reply {round} did not parse: {e}");
                    ask = format!("{user}\n\nYour previous reply could not be parsed ({e}). Reply with the JSON object only.");
                }
            }
        }
    }
    st.ledger.settle(PLAN_BUDGET_ID, PLAN_BUDGET_ID, spent, &format!("{basis:?}").to_lowercase());
    events::log(&nd, "settle", None, serde_json::json!({ "res": PLAN_BUDGET_ID, "cost": spent }))?;
    if result.is_none() { println!("plan: planner failed; continuing with the analyzer's dependencies only"); }
    Ok(result)
}

/// Would adding task -> on create a cycle? (Is `task` reachable from `on`?)
fn creates_cycle(st: &State, task: &str, on: &str) -> bool {
    let mut stack = vec![on.to_string()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(n) = stack.pop() {
        if n == task { return true; }
        if !seen.insert(n.clone()) { continue; }
        if let Some(t) = st.tasks.get(&n) { stack.extend(t.deps.iter().cloned()); }
    }
    false
}

fn add_edge(st: &mut State, nd: &Path, d: Dep) -> anyhow::Result<()> {
    if d.task == d.on || !st.tasks.contains_key(&d.task) || !st.tasks.contains_key(&d.on) { return Ok(()); }
    if st.tasks[&d.task].deps.contains(&d.on) { return Ok(()); }
    if creates_cycle(st, &d.task, &d.on) {
        println!("dep dropped (cycle): {} -> {}", d.task, d.on);
        return events::log(nd, "deps_found", Some(&d.task), serde_json::json!({ "dropped": d, "reason": "cycle" }));
    }
    println!("dep: {} -> {}  [{}] {}", d.task, d.on, d.source, d.why);
    st.tasks.get_mut(&d.task).unwrap().deps.push(d.on.clone());
    st.dep_edges.push(d);
    Ok(())
}

/// Returns (answer, timed_out).
fn ask_with_timeout(question: &str, default: &str, secs: u64) -> (String, bool) {
    println!("{question}  [default: {default} in {secs}s]");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        let _ = tx.send(s);
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(s) if !s.trim().is_empty() => (s.trim().to_lowercase(), false),
        _ => (default.to_string(), true),
    }
}

fn decide(cfg: &Config, st: &mut State, nd: &Path, c: PlanContra) -> anyhow::Result<()> {
    if c.a == c.b || !st.tasks.contains_key(&c.a) || !st.tasks.contains_key(&c.b) { return Ok(()); }
    let default = if c.suggest_keep.as_deref() == Some(c.b.as_str()) { "b" } else { "a" };
    let (choice, decided_by) = if cfg.policy.contradiction == "first" {
        (default.to_string(), "policy (first)".to_string())
    } else {
        let q = format!("\nCONTRADICTION {} vs {}: {}\n  a = keep {}, b = keep {}, c = not a contradiction (try both)",
            c.a, c.b, c.why, c.a, c.b);
        let (ans, timed_out) = ask_with_timeout(&q, default, cfg.policy.ask_timeout_secs);
        let ans = if ["a", "b", "c"].contains(&ans.as_str()) { ans } else { default.to_string() };
        (ans, if timed_out { "default (timeout)".into() } else { "operator".into() })
    };
    let keep = match choice.as_str() { "a" => Some(c.a.clone()), "b" => Some(c.b.clone()), _ => None };
    println!("contradiction {} vs {}: {} (decided by {decided_by})", c.a, c.b,
        keep.as_ref().map(|k| format!("keep {k}")).unwrap_or("try both".into()));
    if let Some(winner) = &keep {
        let loser = if *winner == c.a { c.b.clone() } else { c.a.clone() };
        let t = st.tasks.get_mut(&loser).unwrap();
        t.contradicts = Some(winner.clone());
        t.contradiction_why = Some(c.why.clone());
        add_edge(st, nd, Dep { task: loser, on: winner.clone(), why: format!("contradiction: gives way to {winner}"), source: "contradiction".into() })?;
    }
    events::log(nd, "contradiction_decided", None, serde_json::json!({ "a": c.a, "b": c.b, "why": c.why, "keep": keep, "decided_by": decided_by }))?;
    st.contradictions.push(Contradiction { a: c.a, b: c.b, why: c.why, keep, decided_by });
    Ok(())
}

/// Analyzer + planner + contradiction decisions, applied to `st` once. A later `run` reuses them.
pub fn plan(cfg: &Config, repo: &Path, st: &mut State) -> anyhow::Result<()> {
    let nd = state::notary_dir(repo);
    let sp = state::state_path(repo);
    if st.planned {
        println!("plan: reusing the saved plan ({} deps, {} contradictions)", st.dep_edges.len(), st.contradictions.len());
        return Ok(());
    }
    let mechanical = analyzer::analyze(cfg, &state::integration_dir(repo), st)?;
    events::log(&nd, "deps_found", None, serde_json::json!({ "analyzer": mechanical }))?;
    for d in mechanical { add_edge(st, &nd, d)?; }

    let planned = call_planner(cfg, repo, st)?;
    if let Some(p) = &planned {
        for (id, pt) in &p.tasks {
            if let Some(t) = st.tasks.get_mut(id) {
                t.area = pt.area.clone().filter(|a| !a.trim().is_empty());
                t.hard = pt.hard;
            }
        }
        events::log(&nd, "plan", None, serde_json::json!({
            "areas": p.tasks.iter().map(|(k, v)| (k.clone(), v.area.clone())).collect::<BTreeMap<_, _>>(),
            "hard": p.tasks.iter().filter(|(_, v)| v.hard).map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            "deps": p.deps.len(), "contradictions": p.contradictions.len() }))?;
    }
    if let Some(p) = planned {
        for d in p.deps { add_edge(st, &nd, Dep { task: d.task, on: d.on, why: d.why, source: "planner".into() })?; }
        for c in p.contradictions { decide(cfg, st, &nd, c)?; }
    }
    st.planned = true;
    st.save(&sp)?;
    let plan = serde_json::json!({
        "deps": st.dep_edges,
        "areas": st.order.iter().map(|id| (id.clone(), st.tasks[id].area.clone())).collect::<BTreeMap<_, _>>(),
        "hard": st.order.iter().filter(|id| st.tasks[*id].hard).collect::<Vec<_>>(),
        "contradictions": st.contradictions,
    });
    fs::write(nd.join("plan.json"), serde_json::to_vec_pretty(&plan)?)?;
    println!("plan: wrote {}", nd.join("plan.json").display());
    Ok(())
}

pub fn plan_cmd(cfg: &Config, repo: &Path) -> anyhow::Result<()> {
    let mut st = probe::ensure_init(cfg, repo)?;
    if cfg.policy.shadow_tests && !st.planned {
        crate::shadow::shadow_cmd(cfg, repo, &[])?;
        st = State::load(&state::state_path(repo))?;
    }
    if !st.planned { probe::probe(cfg, repo, &mut st, false)?; }
    plan(cfg, repo, &mut st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Task;

    #[test]
    fn parses_fenced_json() {
        let p = parse_plan("```json\n{\"tasks\":{\"T01\":{\"area\":\"fees\",\"hard\":true}},\"deps\":[],\"contradictions\":[{\"a\":\"T07\",\"b\":\"T11\",\"why\":\"x\"}]}\n```").unwrap();
        assert!(p.tasks["T01"].hard);
        assert_eq!(p.contradictions[0].b, "T11");
    }

    #[test]
    fn cycles_are_detected() {
        let mut st = State::default();
        for id in ["T01", "T02", "T03"] { st.tasks.insert(id.into(), Task::new(id, "", "")); }
        st.tasks.get_mut("T02").unwrap().deps.push("T01".into());
        st.tasks.get_mut("T03").unwrap().deps.push("T02".into());
        assert!(creates_cycle(&st, "T01", "T03"));
        assert!(!creates_cycle(&st, "T03", "T01"));
    }
}
