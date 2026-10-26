use std::{path::Path, time::Duration};

use crate::{config::Config, proc};

/// The grader kills a suite after 120 s; give ours a little more before calling it a timeout.
const TEST_TIMEOUT: Duration = Duration::from_secs(150);

pub struct TestResult { pub passed: bool, pub tests: u32, pub failed: u32, pub timed_out: bool }

fn count(tap: &str, label: &str) -> u32 {
    let re = regex::Regex::new(&format!(r"(?m)^# {label} (\d+)")).unwrap();
    re.captures(tap).and_then(|c| c[1].parse().ok()).unwrap_or(0)
}

/// Same rule as scripts/grade.js: exit 0 and at least one test.
pub fn parse(tap: &str, code: Option<i32>, timed_out: bool) -> TestResult {
    let tests = count(tap, "tests");
    TestResult { passed: code == Some(0) && tests > 0, tests, failed: count(tap, "fail"), timed_out }
}

/// Runs `test_task` for one id in `dir`, writes TAP to `out` (stderr next to it as .err).
pub fn task(cfg: &Config, dir: &Path, id: &str, out: &Path) -> anyhow::Result<TestResult> {
    let cmd = cfg.repo.test_task.replace("{id}", id);
    let r = proc::run(proc::sh(&cmd, dir), out, &out.with_extension("err"), TEST_TIMEOUT)?;
    Ok(parse(&std::fs::read_to_string(out)?, r.code, r.timed_out))
}

/// Runs `test_all` (the regression suite); passes on exit 0. Output goes to `log`.
pub fn regression(cfg: &Config, dir: &Path, log: &Path) -> anyhow::Result<bool> {
    Ok(proc::run(proc::sh(&cfg.repo.test_all, dir), log, &log.with_extension("err"), TEST_TIMEOUT)?.ok())
}

/// Runs `integrity` (the test-lock check); passes on exit 0. Output goes to `log`.
pub fn integrity(cfg: &Config, dir: &Path, log: &Path) -> anyhow::Result<bool> {
    Ok(proc::run(proc::sh(&cfg.repo.integrity, dir), log, &log.with_extension("err"), TEST_TIMEOUT)?.ok())
}

/// The `not ok` lines plus their assertion details, from the last `max_lines` of the TAP.
pub fn failures(tap: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = tap.lines().collect();
    let tail = &lines[lines.len().saturating_sub(max_lines)..];
    tail.iter().filter(|l| !l.trim().is_empty()).copied().collect::<Vec<_>>().join("\n")
}

/// Test names from `not ok N - name` lines, for "contradicts" evidence.
pub fn failing_names(tap: &str) -> Vec<String> {
    let re = regex::Regex::new(r"(?m)^\s*not ok \d+ - (.+)$").unwrap();
    re.captures_iter(tap).map(|c| c[1].trim().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAP: &str = "TAP version 13\n# Subtest: rounds half up\nnot ok 1 - rounds half up\n  ---\n  error: 'Expected 1501'\n  ...\nok 2 - quote matches charge\n1..2\n# tests 2\n# suites 0\n# pass 1\n# fail 1\n";

    #[test]
    fn counts_like_the_grader() {
        let r = parse(TAP, Some(1), false);
        assert_eq!((r.tests, r.failed, r.passed), (2, 1, false));
        assert!(parse(TAP, Some(0), false).passed);
        assert!(!parse("# tests 0\n", Some(0), false).passed, "zero tests never counts as a pass");
    }

    #[test]
    fn extracts_failing_test_names() {
        assert_eq!(failing_names(TAP), vec!["rounds half up".to_string()]);
    }
}
