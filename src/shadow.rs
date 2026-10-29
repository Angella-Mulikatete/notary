use std::{fs, path::Path};

use crate::{
    config::Config, events, git, llm, probe, prompt,
    state::{self, State, TaskState},
    testrun,
};

const SHADOW_USD: f64 = 0.05;
const BUDGET_ID: &str = "_shadow";
const ROUNDS: usize = 3;

const REVIEW: &str = "\
Review this acceptance test for the ticket above. It decides whether another agent's work is accepted, so a wrong assertion rejects correct work.
Check, and fix in place:
1. Recompute every expected number step by step from the ticket and docs (fees, rounding, totals, counts). Fix any that are wrong.
2. Every setup step must succeed once this ticket and the other tickets are implemented: valid ASCII ids, enough money including fees, amounts under every limit any ticket mentions.
3. Go through the other tickets one by one. If one of them changes a value an assertion depends on (a fee, a duty, a balance, a limit, a count of entries), rewrite the assertion to check only what this ticket owns: the field or the change this ticket introduces, not a total that other tickets also move. For example, assert the amount credited to the receiver or the field this ticket adds, rather than the sender's whole balance.
4. Remove any assertion the ticket does not clearly require, or that you are not sure of. Keep at least 3 tests, and keep testing what the ticket adds.
Reply with the complete corrected file in one ```js block and nothing else.";

const SYSTEM: &str = "You are a careful QA engineer. You turn one ticket into a black-box acceptance test. Reply with one ```js code block only.";

const RULES: &str = "\
Write the acceptance test for the ticket below, as one Node.js test file.
Rules:
- Use node:test and node:assert/strict, and the helpers in test/helpers.js (import from '../test/helpers.js'). Import NOTHING else: no files under src/. Test through the HTTP API only.
- Follow the style of the example tests: before/after hooks that start and close the app, one behaviour per test.
- Assert only what the ticket states explicitly. Compute expected numbers from the ticket and the docs. Do not assert error message text, field names or status codes the ticket does not give; for a rejection the ticket does not specify, assert result.status >= 400.
- Other tickets in the backlog may change shared rules (fees, duties, limits, ids). Do not depend on a value another ticket changes unless this ticket states it; pick amounts and inputs that other tickets leave alone, and assert what this ticket owns rather than totals other tickets also move.
- Use fresh, unique account ids in every test so tests do not share state. Ids are plain ASCII: lowercase letters, digits and '-' only.
- Setup must succeed once the ticket is done: open accounts with far more money than the test moves, keep amounts small and well under every limit any ticket mentions, and check setup responses before asserting.
- Above every expected number, write the arithmetic in a comment, step by step from the ticket and docs.
- Write 3 to 6 tests. At least one must fail on the current code, because the ticket is not implemented yet.
- Reply with the complete file in one ```js block and nothing else.";

fn test_path(cfg: &Config, id: &str) -> String { format!("{}/{id}.test.js", cfg.repo.acceptance_dir) }

fn gen_prompt(cfg: &Config, integ: &Path, st: &State, id: &str) -> anyhow::Result<String> {
    let t = &st.tasks[id];
    let mut p = format!("{RULES}\n\n# Repository\n\n{}", prompt::context(cfg, integ)?);
    let helpers = fs::read_to_string(integ.join("test").join("helpers.js")).unwrap_or_default();
    p.push_str(&format!("\n--- test/helpers.js ---\n{}\n", helpers.replace("\r\n", "\n").trim_end()));
    // examples come from the regression suite, never from acceptance/, so no ticket's answer leaks in
    for ex in example_tests(integ).iter().take(2) {
        let src = fs::read_to_string(integ.join(ex)).unwrap_or_default();
        p.push_str(&format!("\n# Example test: {ex}\n```js\n{}\n```\n", src.replace("\r\n", "\n").trim_end()));
    }
    p.push_str("\n# The other tickets (other agents implement them in parallel; your test must still pass after they merge)\n");
    for o in &st.order {
        if o != id { p.push_str(&format!("\n## {o}: {}\n{}\n", st.tasks[o].title, st.tasks[o].body)); }
    }
    p.push_str(&format!("\n# Ticket {id}: {}\n\n{}\n\n# Output\n\nThe file is saved as {}.\n", t.title, t.body, test_path(cfg, id)));
    Ok(p)
}

/// Black-box regression tests (start the app, no src/ imports), largest first.
fn example_tests(integ: &Path) -> Vec<String> {
    let mut v: Vec<(usize, String)> = prompt::list_files(integ).into_iter()
        .filter(|f| f.starts_with("test/") && f.ends_with(".test.js"))
        .filter_map(|f| fs::read_to_string(integ.join(&f)).ok()
            .filter(|s| s.contains("startApp") && !s.contains("../src/")).map(|s| (s.len(), f)))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|(_, f)| f).collect()
}

fn extract_js(text: &str) -> Option<String> {
    let start = text.find("```")?;
    let after = &text[start + 3..];
    let body = &after[after.find('\n')? + 1..];
    let end = body.find("```")?;
    Some(ascii_punct(body[..end].trim_end()) + "\n")
}

/// Models write U+2011 and friends inside ids and strings; the API accepts only ASCII ids.
fn ascii_punct(s: &str) -> String {
    s.chars().map(|c| match c {
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00a0}' | '\u{202f}' => ' ',
        c => c,
    }).collect()
}

/// Imports other than node builtins and the test helpers make a test white-box; reject them.
fn bad_imports(src: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"(?m)(?:^\s*import\b[^'"]*|\bimport\s*\(\s*|\brequire\s*\(\s*)['"]([^'"]+)['"]"#).unwrap();
    re.captures_iter(src).map(|c| c[1].to_string())
        .filter(|m| !m.starts_with("node:") && m != "../test/helpers.js")
        .collect()
}

/// Ok(summary) if the test is usable, Err(feedback for the model) if not.
fn check(cfg: &Config, integ: &Path, nd: &Path, id: &str, src: &str, round: usize) -> anyhow::Result<Result<String, String>> {
    let bad = bad_imports(src);
    if !bad.is_empty() {
        return Ok(Err(format!("It imports {}. Import only node: builtins and '../test/helpers.js', and test through the HTTP API.", bad.join(", "))));
    }
    let path = integ.join(test_path(cfg, id));
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, src)?;
    let syn = nd.join("runs").join(id).join(format!("shadow-check-{round}.log"));
    let r = crate::proc::run(crate::proc::sh(&format!("node --check {}", test_path(cfg, id)), integ),
        &syn, &syn.with_extension("err"), std::time::Duration::from_secs(30))?;
    if !r.ok() {
        let err = fs::read_to_string(syn.with_extension("err")).unwrap_or_default();
        return Ok(Err(format!("It does not parse:\n{}", err.lines().take(15).collect::<Vec<_>>().join("\n"))));
    }
    let tap = nd.join("runs").join(id).join(format!("shadow-base-{round}.tap"));
    let res = testrun::task(cfg, integ, id, &tap)?;
    let text = fs::read_to_string(&tap).unwrap_or_default();
    if res.timed_out { return Ok(Err("It timed out on the current code. Close the app in after() and avoid waiting on timers.".into())); }
    if res.tests == 0 { return Ok(Err("No tests ran. Check the file loads and defines test(...) calls.".into())); }
    if res.passed {
        return Ok(Err("Every test passes on the current code, so it checks nothing the ticket adds. Test the new behaviour the ticket asks for.".into()));
    }
    if text.contains("ReferenceError") || text.contains("SyntaxError") || (res.tests == 1 && text.contains("test failed")) {
        return Ok(Err(format!("It crashes instead of testing:\n{}", testrun::failures(&text, 25))));
    }
    Ok(Ok(format!("{} tests, {} fail on base", res.tests, res.failed)))
}

/// One budgeted model call; moves on to the fallback model when the primary is unreachable.
fn call(cfg: &Config, st: &mut State, nd: &Path, id: &str, tag: &str, ask: &str, models: &[(String, String)], mi: &mut usize)
    -> anyhow::Result<Result<(String, String), String>> {
    while *mi < models.len() {
        let res_id = format!("{BUDGET_ID}-{id}-{tag}");
        let amount = st.ledger.available_for(BUDGET_ID).min(SHADOW_USD);
        if amount <= 0.0 || st.ledger.reserve(&res_id, BUDGET_ID, amount).is_err() {
            events::log(nd, "budget_refused", Some(id), serde_json::json!({ "for": "shadow" }))?;
            return Ok(Err("budget refused the shadow test call".into()));
        }
        let (pname, model) = &models[*mi];
        match llm::provider(cfg, pname).and_then(|p| llm::chat(&p, &cfg.prices, model, SYSTEM, ask, 240)) {
            Ok(r) => {
                st.ledger.settle(&res_id, BUDGET_ID, r.cost_usd, &format!("{:?}", r.basis).to_lowercase());
                events::log(nd, "settle", Some(id), serde_json::json!({ "res": res_id, "cost": r.cost_usd, "model": model }))?;
                fs::write(nd.join("runs").join(id).join(format!("shadow-response-{tag}.txt")), &r.text)?;
                return Ok(Ok((r.text, model.clone())));
            }
            Err(e) => {
                st.ledger.settle(&res_id, BUDGET_ID, 0.0, "computed");
                eprintln!("shadow: {id}: {pname}/{model}: {e:#}");
                *mi += 1;
            }
        }
    }
    Ok(Err("every shadow model failed".into()))
}

fn generate(cfg: &Config, repo: &Path, st: &mut State, id: &str) -> anyhow::Result<Result<String, String>> {
    let nd = state::notary_dir(repo);
    let integ = state::integration_dir(repo);
    let mref = cfg.shadow.as_ref().unwrap_or(&cfg.planner);
    let mut models = vec![(mref.provider.clone(), mref.model.clone())];
    if let (Some(p), Some(m)) = (&mref.fallback_provider, &mref.fallback_model) { models.push((p.clone(), m.clone())); }
    let base = gen_prompt(cfg, &integ, st, id)?;
    fs::create_dir_all(nd.join("runs").join(id))?;
    fs::write(nd.join("runs").join(id).join("shadow-prompt.md"), &base)?;
    let mut ask = base.clone();
    let mut last = String::from("no reply");
    let mut mi = 0;
    for round in 1..=ROUNDS {
        let (text, model) = match call(cfg, st, &nd, id, &round.to_string(), &ask, &models, &mut mi)? {
            Ok(r) => r,
            Err(e) => { last = e; break; }
        };
        let draft = extract_js(&text);
        let verdict = match &draft {
            Some(src) => check(cfg, &integ, &nd, id, src, round)?,
            None => Err("Reply with the complete file in one ```js block.".to_string()),
        };
        events::log(&nd, "shadow", Some(id), serde_json::json!({ "round": round, "model": model,
            "ok": verdict.is_ok(), "detail": verdict.as_ref().map_or_else(|e| e.clone(), |s| s.clone()) }))?;
        let (Ok(summary), Some(src)) = (verdict.clone(), draft) else {
            let fb = verdict.unwrap_err();
            println!("shadow: {id} round {round} rejected: {}", fb.lines().next().unwrap_or(""));
            last = fb.clone();
            ask = format!("{base}\n\n# Your previous file was rejected\n\n{fb}\n\nWrite the whole file again.");
            continue;
        };
        // second pass re-derives expected numbers and drops doubtful assertions; the draft stays if the review breaks it
        let review = format!("{base}\n\n# Test to review\n\n```js\n{src}```\n\n{REVIEW}");
        if let Ok((rtext, rmodel)) = call(cfg, st, &nd, id, &format!("{round}-review"), &review, &models, &mut mi)? {
            if let Some(rsrc) = extract_js(&rtext) {
                let rv = check(cfg, &integ, &nd, id, &rsrc, round + 10)?;
                events::log(&nd, "shadow_review", Some(id), serde_json::json!({ "round": round, "model": rmodel,
                    "ok": rv.is_ok(), "detail": rv.as_ref().map_or_else(|e| e.clone(), |s| s.clone()) }))?;
                if let Ok(rsum) = rv { return Ok(Ok(format!("{rsum} ({model}, round {round}, reviewed)"))); }
            }
        }
        fs::write(integ.join(test_path(cfg, id)), &src)?;
        return Ok(Ok(format!("{summary} ({model}, round {round}, review rejected)")));
    }
    let _ = fs::remove_file(integ.join(test_path(cfg, id)));
    Ok(Err(last))
}

pub fn shadow_cmd(cfg: &Config, repo: &Path, only: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(cfg.policy.shadow_tests, "shadow: policy.shadow_tests is off in the config");
    let mut st = probe::ensure_init(cfg, repo)?;
    anyhow::ensure!(!st.planned, "shadow: the plan already exists; shadow tests must be written before `plan`");
    let nd = state::notary_dir(repo);
    let sp = state::state_path(repo);
    let integ = state::integration_dir(repo);
    anyhow::ensure!(git::head(&integ)? == st.base_sha, "shadow: work has already started on the integration branch");

    let mut written = Vec::new();
    for id in st.order.clone() {
        if st.tasks[&id].state != TaskState::Pending { continue; }
        let exists = integ.join(test_path(cfg, &id)).exists();
        if if only.is_empty() { exists } else { !only.contains(&id) } { continue; }
        match generate(cfg, repo, &mut st, &id)? {
            Ok(summary) => {
                println!("shadow: {id} ok: {summary}");
                st.tasks.get_mut(&id).unwrap().notes.push(format!("shadow test {}: {summary}", test_path(cfg, &id)));
                written.push(id);
            }
            Err(why) => {
                let reason = format!("no usable shadow test: {}", why.lines().next().unwrap_or(""));
                println!("shadow: {id} SKIPPED: {reason}");
                let t = st.tasks.get_mut(&id).unwrap();
                t.transition(TaskState::Skipped { reason })?;
            }
        }
        st.save(&sp)?;
    }
    // stale probe results were taken without the test; plan re-probes these tasks
    for id in &written { let _ = fs::remove_file(nd.join("runs").join(id).join("probe.tap")); }
    if let Some(sha) = git::commit_all(&integ, &format!("notary: shadow tests for {}", written.join(", ")))? {
        st.base_sha = sha.clone();
        events::log(&nd, "shadow_commit", None, serde_json::json!({ "sha": sha, "tasks": written }))?;
        println!("shadow: committed {} test(s) at {}; new base", written.len(), &sha[..8]);
    }
    st.save(&sp)?;
    println!("shadow: spent ${:.4} of ${:.2} cap. Review the tests in {} before `plan`.",
        st.ledger.spent_usd, st.ledger.cap_usd, integ.join(&cfg.repo.acceptance_dir).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_code_block() {
        let s = extract_js("Here:\n```js\nimport x from 'node:test';\n```\nbye").unwrap();
        assert_eq!(s, "import x from 'node:test';\n");
    }

    #[test]
    fn only_builtins_and_helpers_may_be_imported() {
        let ok = "import assert from 'node:assert/strict';\nimport { startApp } from '../test/helpers.js';";
        assert!(bad_imports(ok).is_empty());
        let bad = "import { calculateFee } from '../src/fees/calculate.js';\nconst m = await import('../src/x.js');";
        assert_eq!(bad_imports(bad), vec!["../src/fees/calculate.js", "../src/x.js"]);
    }
}
