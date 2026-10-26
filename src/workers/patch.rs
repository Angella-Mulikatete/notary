use std::{fs, io::Write, path::Path, time::Duration};

use super::{Job, Outcome, Worker};
use crate::{config::Config, llm, proc, prompt, state::CostBasis, testrun};

const SYSTEM: &str = "You are a careful senior JavaScript engineer. Make the smallest correct change. Follow the output format exactly.";
const ROUNDS: u32 = 3;
const EXPECTED_OUTPUT_TOKENS: f64 = 8000.0;

pub fn parse_files(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("=== FILE: ") {
            let path = rest.trim_end_matches("===").trim().trim_start_matches("./").to_string();
            cur = Some((path, Vec::new()));
        } else if line.trim() == "=== END FILE ===" {
            if let Some((path, mut lines)) = cur.take() {
                if lines.first().is_some_and(|l| l.trim_start().starts_with("```"))
                    && lines.last().is_some_and(|l| l.trim() == "```") {
                    lines.remove(0);
                    lines.pop();
                }
                out.push((path, lines.join("\n") + "\n"));
            }
        } else if let Some((_, lines)) = cur.as_mut() {
            lines.push(line);
        }
    }
    out
}

pub fn globs(patterns: &[String]) -> anyhow::Result<globset::GlobSet> {
    let mut b = globset::GlobSetBuilder::new();
    for p in patterns { b.add(globset::Glob::new(p)?); }
    Ok(b.build()?)
}

pub fn allowed(path: &str, writable: &globset::GlobSet, locked: &globset::GlobSet) -> bool {
    let p = Path::new(path);
    !path.contains('\\')
        && !p.is_absolute()
        && p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
        && writable.is_match(path)
        && !locked.is_match(path)
}

pub struct PatchWorker {
    pub cfg: Config,
    pub provider_name: String,
    pub provider: llm::Provider,
    pub model: String,
}

impl PatchWorker {
    fn estimate(&self, prompt: &str) -> f64 {
        match self.cfg.prices.get(&self.model) {
            Some(p) => ((prompt.len() as f64 / 4.0) * p.input + EXPECTED_OUTPUT_TOKENS * p.output) / 1e6,
            None => 0.0,
        }
    }
}

impl Worker for PatchWorker {
    fn name(&self) -> String { format!("patch:{}/{}", self.provider_name, self.model) }

    fn run(&self, job: &Job) -> anyhow::Result<Outcome> {
        let writable = globs(&self.cfg.repo.writable)?;
        let locked = globs(&self.cfg.repo.locked)?;
        let mut log = fs::OpenOptions::new().create(true).append(true)
            .open(job.run_dir.join(format!("worker-a{}.log", job.attempt)))?;
        let mut spent = 0.0;
        let mut basis = self.provider.basis;
        let mut prompt = job.prompt.clone();
        let mut summary = String::new();
        let mut last_problem = String::from("no rounds ran");
        let timeout = job.timeout_secs.min(300);

        for round in 1..=ROUNDS {
            let est = self.estimate(&prompt);
            if spent + est > job.max_usd {
                last_problem = format!("budget: round {round} would need ~${est:.4}, ${:.4} of ${:.4} left", job.max_usd - spent, job.max_usd);
                break;
            }
            writeln!(log, "\n===== round {round} request ({} chars) =====\n{prompt}", prompt.len())?;
            let reply = match llm::chat(&self.provider, &self.cfg.prices, &self.model, SYSTEM, &prompt, timeout) {
                Ok(r) => r,
                Err(e) => {
                    writeln!(log, "===== round {round} error =====\n{e:#}")?;
                    return Ok(Outcome { worker: self.name(), cost_usd: spent, basis, summary, error: Some(format!("{e:#}")) });
                }
            };
            spent += reply.cost_usd;
            if reply.basis == CostBasis::Estimated { basis = CostBasis::Estimated; }
            writeln!(log, "===== round {round} response (${:.5}, usage {}) =====\n{}", reply.cost_usd, reply.usage, reply.text)?;
            if let Some(s) = reply.text.lines().find_map(|l| l.trim().strip_prefix("=== SUMMARY:")) {
                summary = s.trim_end_matches("===").trim().to_string();
            }

            let files = parse_files(&reply.text);
            let forbidden: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).filter(|p| !allowed(p, &writable, &locked)).collect();
            let feedback = if files.is_empty() {
                "You returned no files. Use the exact === FILE: path === ... === END FILE === format.".to_string()
            } else if !forbidden.is_empty() {
                format!("These paths are forbidden, nothing was written: {}. Edit only src/** and docs/**.", forbidden.join(", "))
            } else {
                for (path, content) in &files {
                    let dest = job.worktree.join(path);
                    if let Some(parent) = dest.parent() { fs::create_dir_all(parent)?; }
                    fs::write(dest, content)?;
                }
                let tap = job.run_dir.join(format!("patch-a{}-r{round}.tap", job.attempt));
                let r = proc::run(proc::sh(&job.test_cmd, &job.worktree), &tap, &tap.with_extension("err"), Duration::from_secs(150))?;
                let res = testrun::parse(&fs::read_to_string(&tap)?, r.code, r.timed_out);
                writeln!(log, "===== round {round} acceptance: {} ({} tests, {} failed) =====", if res.passed { "pass" } else { "FAIL" }, res.tests, res.failed)?;
                if res.passed {
                    return Ok(Outcome { worker: self.name(), cost_usd: spent, basis, summary, error: None });
                }
                format!("The acceptance test still fails after your change. Failing output:\n{}", testrun::failures(&fs::read_to_string(&tap)?, 60))
            };
            last_problem = feedback.lines().next().unwrap_or("").to_string();
            prompt = prompt::task_prompt(&self.cfg, &job.worktree, &job.task, Some(&feedback), true)?;
        }
        Ok(Outcome { worker: self.name(), cost_usd: spent, basis, summary, error: Some(last_problem) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_files_and_a_summary() {
        let text = "Sure.\n=== FILE: src/a.js ===\nconst a = 1;\n=== END FILE ===\n=== FILE: src/b/c.js ===\n```js\nexport {};\n```\n=== END FILE ===\n=== SUMMARY: two files ===";
        let files = parse_files(text);
        assert_eq!(files, vec![("src/a.js".into(), "const a = 1;\n".into()), ("src/b/c.js".into(), "export {};\n".into())]);
    }

    #[test]
    fn path_guard() {
        let w = globs(&["src/**".into(), "docs/**".into()]).unwrap();
        let l = globs(&["test/**".into(), "acceptance/**".into(), "package.json".into()]).unwrap();
        assert!(!allowed("src/../acceptance/T01.test.js", &w, &l));
        assert!(!allowed("/etc/passwd", &w, &l));
        assert!(!allowed("C:/Windows/x.js", &w, &l));
        assert!(!allowed("src\\..\\package.json", &w, &l));
        assert!(!allowed("package.json", &w, &l));
        assert!(allowed("src/fees/calculate.js", &w, &l));
    }
}
