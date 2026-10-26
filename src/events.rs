use std::{fs::OpenOptions, io::Write, path::Path};

pub fn log(dir: &Path, kind: &str, task: Option<&str>, detail: serde_json::Value) -> anyhow::Result<()> {
    let line = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(), "kind": kind, "task": task, "detail": detail });
    let mut f = OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl"))?;
    writeln!(f, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_json_line_per_event() {
        let dir = std::env::temp_dir().join(format!("notary-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        log(&dir, "probe", Some("T01"), serde_json::json!({"passed": false})).unwrap();
        log(&dir, "run_started", None, serde_json::json!({})).unwrap();
        let text = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["task"], "T01");
        assert!(lines[1]["task"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }
}
