use std::{fs::File, path::Path, process::{Command, Stdio}, time::Duration};
use wait_timeout::ChildExt;

pub struct RunResult { pub code: Option<i32>, pub timed_out: bool }

impl RunResult {
    pub fn ok(&self) -> bool { self.code == Some(0) && !self.timed_out }
}

pub fn sh(cmdline: &str, cwd: &Path) -> Command {
    let sh = sh_path();
    let mut c = Command::new(sh);
    c.arg("-c").arg(cmdline).current_dir(cwd);
    // so sleep/cat/grep resolve inside sh even when only Git\cmd is on PATH
    if let Some(dir) = sh.parent().filter(|d| !d.as_os_str().is_empty()) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&path)));
        if let Ok(p) = joined { c.env("PATH", p); }
    }
    c
}

fn sh_path() -> &'static std::path::PathBuf {
    static SH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    SH.get_or_init(|| {
        if let Ok(p) = std::env::var("NOTARY_SH") { return p.into(); }
        if cfg!(windows) {
            let exec = Command::new("git").arg("--exec-path").output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            if let Some(root) = exec.as_deref().map(Path::new).and_then(|p| p.ancestors().nth(3)) {
                let sh = root.join("usr").join("bin").join("sh.exe");
                if sh.exists() { return sh; }
            }
        }
        "sh".into()
    })
}

pub fn run(cmd: Command, out: &Path, err: &Path, timeout: Duration) -> anyhow::Result<RunResult> {
    run_with_input(cmd, None, out, err, timeout)
}

pub fn run_with_input(mut cmd: Command, input: Option<&Path>, out: &Path, err: &Path, timeout: Duration) -> anyhow::Result<RunResult> {
    let stdin = match input { Some(p) => Stdio::from(File::open(p)?), None => Stdio::null() };
    cmd.stdin(stdin).stdout(File::create(out)?).stderr(File::create(err)?);
    let mut child = cmd.spawn()?;
    match child.wait_timeout(timeout)? {
        Some(status) => Ok(RunResult { code: status.code(), timed_out: false }),
        None => {
            kill_tree(child.id());
            let _ = child.kill();
            child.wait()?;
            Ok(RunResult { code: None, timed_out: true })
        }
    }
}

fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("notary-proc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn timeout_kills_the_process() {
        let d = tmp("timeout");
        let start = Instant::now();
        let r = run(sh("sleep 30", &d), &d.join("o"), &d.join("e"), Duration::from_secs(1)).unwrap();
        assert!(r.timed_out);
        assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
    }

    #[test]
    fn output_goes_to_files_and_exit_code_is_kept() {
        let d = tmp("out");
        let r = run(sh("echo hello; echo oops >&2; exit 3", &d), &d.join("o"), &d.join("e"), Duration::from_secs(10)).unwrap();
        assert_eq!(r.code, Some(3));
        assert_eq!(std::fs::read_to_string(d.join("o")).unwrap().trim(), "hello");
        assert_eq!(std::fs::read_to_string(d.join("e")).unwrap().trim(), "oops");
    }
}
