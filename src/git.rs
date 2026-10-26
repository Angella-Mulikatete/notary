use std::{path::Path, process::Command};

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = git_raw(dir, args)?;
    anyhow::ensure!(out.status.success(), "git {} failed in {}: {}",
        args.join(" "), dir.display(), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn git_raw(dir: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(Command::new("git").args(args).current_dir(dir)
        .env("GIT_EDITOR", "true")          // rebase --continue must never open an editor
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?)
}

pub fn head(dir: &Path) -> anyhow::Result<String> { git(dir, &["rev-parse", "HEAD"]) }

pub fn is_clean(dir: &Path) -> anyhow::Result<bool> { Ok(git(dir, &["status", "--porcelain"])?.is_empty()) }

pub fn worktree_add(repo: &Path, path: &Path, branch: &str, base: &str) -> anyhow::Result<()> {
    git(repo, &["worktree", "add", "-B", branch, &path.to_string_lossy(), base]).map(|_| ())
}

pub fn worktree_add_detached(repo: &Path, path: &Path, sha: &str) -> anyhow::Result<()> {
    git(repo, &["worktree", "add", "--detach", "-f", &path.to_string_lossy(), sha]).map(|_| ())
}

pub fn prune(repo: &Path) -> anyhow::Result<()> { git(repo, &["worktree", "prune"]).map(|_| ()) }

pub fn worktree_remove(repo: &Path, path: &Path) -> anyhow::Result<()> {
    git(repo, &["worktree", "remove", "--force", &path.to_string_lossy()]).map(|_| ())
}

/// Stages everything and commits; None if nothing changed.
pub fn commit_all(dir: &Path, msg: &str) -> anyhow::Result<Option<String>> {
    git(dir, &["add", "-A"])?;
    if git(dir, &["diff", "--cached", "--quiet"]).is_ok() { return Ok(None); }
    git(dir, &["commit", "-q", "-m", msg])?;
    Ok(Some(head(dir)?))
}

pub fn changed_files(dir: &Path, a: &str, b: &str) -> anyhow::Result<Vec<String>> {
    Ok(git(dir, &["diff", "--name-only", a, b])?.lines().map(str::to_string).collect())
}

/// Added lines (without the leading `+`), excluding the `+++` file headers.
pub fn added_lines(dir: &Path, a: &str, b: &str) -> anyhow::Result<Vec<String>> {
    Ok(git(dir, &["diff", "-U0", a, b])?.lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .map(|l| l[1..].to_string()).collect())
}

pub fn diff(dir: &Path, a: &str, b: &str) -> anyhow::Result<String> { git(dir, &["diff", a, b]) }

pub enum RebaseResult { Clean, Conflict(Vec<String>) }

pub fn rebase(dir: &Path, onto: &str) -> anyhow::Result<RebaseResult> {
    if git_raw(dir, &["rebase", onto])?.status.success() { return Ok(RebaseResult::Clean); }
    let files: Vec<String> = git(dir, &["diff", "--name-only", "--diff-filter=U"])?
        .lines().map(str::to_string).collect();
    anyhow::ensure!(!files.is_empty(), "git rebase {onto} failed without conflicted files in {}", dir.display());
    Ok(RebaseResult::Conflict(files))
}

pub fn rebase_continue(dir: &Path) -> anyhow::Result<()> { git(dir, &["rebase", "--continue"]).map(|_| ()) }

pub fn rebase_abort(dir: &Path) -> anyhow::Result<()> { git(dir, &["rebase", "--abort"]).map(|_| ()) }

pub fn add(dir: &Path, path: &str) -> anyhow::Result<()> { git(dir, &["add", "--", path]).map(|_| ()) }

pub fn ff_merge(dir: &Path, branch: &str) -> anyhow::Result<()> { git(dir, &["merge", "--ff-only", "-q", branch]).map(|_| ()) }

pub fn reset_hard(dir: &Path, sha: &str) -> anyhow::Result<()> { git(dir, &["reset", "--hard", "-q", sha]).map(|_| ()) }

/// `git merge-base --is-ancestor a b`: exit 0 means a is an ancestor of b.
pub fn is_ancestor(dir: &Path, a: &str, b: &str) -> anyhow::Result<bool> {
    Ok(git_raw(dir, &["merge-base", "--is-ancestor", a, b])?.status.success())
}

/// Removes all .notary/trees/* worktrees and deletes notary/* branches.
pub fn clean(repo: &Path) -> anyhow::Result<()> {
    let trees = repo.join(".notary").join("trees");
    if let Ok(entries) = std::fs::read_dir(&trees) {
        for e in entries.flatten() {
            if let Err(err) = worktree_remove(repo, &e.path()) { eprintln!("warning: {err}"); }
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    git(repo, &["worktree", "prune"])?;
    let branches = git(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads/notary/"])?;
    for b in branches.lines().filter(|b| !b.is_empty()) {
        git(repo, &["branch", "-D", b])?;
        println!("deleted branch {b}");
    }
    println!("clean: worktrees removed, notary/* branches deleted");
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::fs;

    /// A throwaway repo with one commit, for tests here and in other modules.
    pub fn temp_repo(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("notary-git-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        git(&d, &["init", "-q", "-b", "main"]).unwrap();
        git(&d, &["config", "user.name", "notary-test"]).unwrap();
        git(&d, &["config", "user.email", "notary@test"]).unwrap();
        git(&d, &["config", "core.autocrlf", "false"]).unwrap();
        fs::write(d.join("a.txt"), "one\n").unwrap();
        git(&d, &["add", "-A"]).unwrap();
        git(&d, &["commit", "-q", "-m", "base"]).unwrap();
        d
    }

    #[test]
    fn worktree_commit_diff_and_ff_merge() {
        let repo = temp_repo("flow");
        let base = head(&repo).unwrap();
        let wt = repo.join("wt");
        worktree_add(&repo, &wt, "notary/T01-a1", &base).unwrap();
        assert_eq!(commit_all(&wt, "nothing").unwrap(), None);
        fs::write(wt.join("a.txt"), "one\ntwo\n").unwrap();
        let sha = commit_all(&wt, "T01").unwrap().expect("a commit");
        assert_eq!(changed_files(&wt, &base, &sha).unwrap(), vec!["a.txt"]);
        assert_eq!(added_lines(&wt, &base, &sha).unwrap(), vec!["two"]);
        assert!(is_ancestor(&repo, &base, &sha).unwrap());
        assert!(!is_ancestor(&repo, &sha, &base).unwrap());
        worktree_remove(&repo, &wt).unwrap();
        ff_merge(&repo, "notary/T01-a1").unwrap();
        assert_eq!(head(&repo).unwrap(), sha);
        reset_hard(&repo, &base).unwrap();
        assert_eq!(head(&repo).unwrap(), base);
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn rebase_reports_conflicted_files() {
        let repo = temp_repo("rebase");
        let base = head(&repo).unwrap();
        let (wa, wb) = (repo.join("wa"), repo.join("wb"));
        worktree_add(&repo, &wa, "notary/A", &base).unwrap();
        worktree_add(&repo, &wb, "notary/B", &base).unwrap();
        fs::write(wa.join("a.txt"), "from A\n").unwrap();
        commit_all(&wa, "A").unwrap();
        fs::write(wb.join("a.txt"), "from B\n").unwrap();
        commit_all(&wb, "B").unwrap();
        match rebase(&wb, "notary/A").unwrap() {
            RebaseResult::Conflict(files) => assert_eq!(files, vec!["a.txt"]),
            RebaseResult::Clean => panic!("expected a conflict"),
        }
        rebase_abort(&wb).unwrap();
        assert!(is_clean(&wb).unwrap());
        fs::remove_dir_all(&repo).ok();
    }
}
