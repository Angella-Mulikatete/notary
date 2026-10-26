use std::{fs, path::Path};

use crate::{config::Config, state::Task};

const RULES: &str = "\
You are fixing one ticket in a small Node.js codebase (ES modules, no dependencies).
Rules:
- You may edit only files under src/ and docs/.
- Never touch test/, acceptance/, scripts/, backlog/, package.json or tests.lock.json.
- Do not add dependencies. Node standard library only.
- Do not run git.
- Keep `npm test` (the regression suite) green.";

const PATCH_FORMAT: &str = "\
Return ONLY changed files, each complete, in this exact format:
=== FILE: src/path/file.js ===
<entire new file content>
=== END FILE ===
Then one line: === SUMMARY: <what you changed> ===";

const SKIP_DIRS: [&str; 4] = [".git", "node_modules", ".notary", "notary-shadow"];

/// Relative paths (with `/`) of every file under `root`, sorted.
pub fn list_files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) { walk(root, &p, out); }
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn context(cfg: &Config, worktree: &Path) -> anyhow::Result<String> {
    let files = list_files(worktree);
    let mut seen = std::collections::BTreeSet::new();
    let mut out = String::new();
    for pat in &cfg.repo.context_files {
        let m = globset::Glob::new(pat)?.compile_matcher();
        for f in files.iter().filter(|f| m.is_match(f.as_str())) {
            if !seen.insert(f.clone()) { continue; }
            let text = fs::read_to_string(worktree.join(f)).unwrap_or_default();
            out.push_str(&format!("--- {f} ---\n{}\n", text.replace("\r\n", "\n").trim_end()));
        }
    }
    Ok(out)
}

pub fn task_prompt(cfg: &Config, worktree: &Path, task: &Task, feedback: Option<&str>, for_patch_agent: bool) -> anyhow::Result<String> {
    let test_file = format!("acceptance/{}.test.js", task.id);
    let test_src = fs::read_to_string(worktree.join(&test_file)).unwrap_or_else(|_| "(missing)".into());
    let mut p = format!("{RULES}\n\n# Repository\n\n{}\n# Ticket {}: {}\n\n{}\n\n# Acceptance test\n\nThis locked test must pass: {test_file}\n\n```js\n{}\n```\n",
        context(cfg, worktree)?, task.id, task.title, task.body, test_src.replace("\r\n", "\n").trim_end());
    if let Some(fb) = feedback.filter(|f| !f.trim().is_empty()) {
        p.push_str(&format!("\n# Feedback from the last try\n\nThe previous attempt failed. Fix this:\n\n```\n{}\n```\n", fb.trim()));
    }
    p.push_str("\n# Output\n\n");
    if for_patch_agent {
        p.push_str(PATCH_FORMAT);
    } else {
        p.push_str(&format!("Edit the files directly. Run `node --test {test_file}` and `npm test` before finishing. \
Finish with one line saying what you changed."));
    }
    p.push('\n');
    Ok(p)
}
