use std::{fs, path::Path};

use crate::{git, scheduler::Ctx, testrun, workers::patch};

const SHORTCUTS: [&str; 5] = ["TODO", "FIXME", ".skip(", ".only(", "todo("];

/// Per-task checks in the task's worktree. Ok(Err(reason)) = the work failed a check.
pub fn verify(ctx: &Ctx, id: &str, n: u8, wt: &Path, start: &str, imp: &str) -> anyhow::Result<Result<(), String>> {
    let cfg = ctx.cfg;
    let rd = crate::state::run_dir(&ctx.repo, id)?;
    fs::write(rd.join(format!("diff-a{n}.patch")), git::diff(wt, start, imp)?)?;

    let writable = patch::globs(&cfg.repo.writable)?;
    let locked = patch::globs(&cfg.repo.locked)?;
    let changed = git::changed_files(wt, start, imp)?;
    let bad: Vec<&String> = changed.iter().filter(|p| !patch::allowed(p, &writable, &locked)).collect();
    if !bad.is_empty() {
        return Ok(Err(format!("changed files outside src/docs or locked: {}", bad.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
    }

    let added = git::added_lines(wt, start, imp)?;
    if let Some((line, s)) = added.iter().find_map(|l| SHORTCUTS.iter().find(|s| l.contains(*s)).map(|s| (l, s))) {
        return Ok(Err(format!("shortcut `{s}` added: {}", line.trim())));
    }

    let tap = rd.join(format!("after-a{n}.tap"));
    let after = testrun::task(cfg, wt, id, &tap)?;
    if !after.passed {
        let names = testrun::failing_names(&fs::read_to_string(&tap).unwrap_or_default());
        return Ok(Err(format!("after-proof failed: {} of {} tests failing{} (runs/{id}/after-a{n}.tap)",
            after.failed, after.tests, if names.is_empty() { String::new() } else { format!(": {}", names.join("; ")) })));
    }

    // other tasks' locked tests: a whole-file rewrite once turned /^\d+$/ into /^\\d+$/ and broke T08
    // only tasks already in this attempt's start commit; later merges are checked by the gate after rebase
    let merged: Vec<&String> = ctx.st.order.iter().filter(|t| {
        let o = &ctx.st.tasks[*t];
        o.state == crate::state::TaskState::Merged
            && o.merged_sha.as_ref().is_some_and(|m| git::is_ancestor(wt, m, start).unwrap_or(false))
    }).collect();
    for m in merged {
        let tap = rd.join(format!("broke-a{n}.tap"));
        if !testrun::task(cfg, wt, m, &tap)?.passed {
            let names = testrun::failing_names(&fs::read_to_string(&tap).unwrap_or_default());
            return Ok(Err(format!("broke {m}'s acceptance ({}); keep {m}'s behaviour (runs/{id}/broke-a{n}.tap)", names.join("; "))));
        }
        let _ = fs::remove_file(&tap);
    }

    if !testrun::regression(cfg, wt, &rd.join(format!("regression-a{n}.log")))? {
        return Ok(Err(format!("regression suite red (runs/{id}/regression-a{n}.log)")));
    }
    if !testrun::integrity(cfg, wt, &rd.join(format!("integrity-a{n}.log")))? {
        return Ok(Err(format!("locked test files modified (runs/{id}/integrity-a{n}.log)")));
    }
    Ok(Ok(()))
}
