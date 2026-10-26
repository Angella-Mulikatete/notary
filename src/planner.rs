use std::{fs, path::Path};

use crate::{analyzer, config::Config, events, probe, state};

pub fn plan_cmd(cfg: &Config, repo: &Path) -> anyhow::Result<()> {
    let mut st = probe::ensure_init(cfg, repo)?;
    probe::probe(cfg, repo, &mut st)?;
    let nd = state::notary_dir(repo);

    let deps = analyzer::analyze(cfg, &state::integration_dir(repo), &st)?;
    for d in &deps {
        println!("dep: {} -> {}  ({})", d.task, d.on, d.why);
        let t = st.tasks.get_mut(&d.task).unwrap();
        if !t.deps.contains(&d.on) { t.deps.push(d.on.clone()); }
    }
    if deps.is_empty() { println!("dep: none found by the analyzer"); }
    st.save(&state::state_path(repo))?;
    events::log(&nd, "deps_found", None, serde_json::json!({ "analyzer": deps }))?;

    let plan = serde_json::json!({ "analyzer_deps": deps });
    fs::write(nd.join("plan.json"), serde_json::to_vec_pretty(&plan)?)?;
    println!("plan: wrote {}", nd.join("plan.json").display());
    Ok(())
}
