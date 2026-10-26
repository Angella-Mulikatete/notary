use regex::Regex;
use serde::Serialize;
use std::{fs, path::Path};

use crate::{config::Config, state::State};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Dep { pub task: String, pub on: String, pub why: String, pub source: String }

/// "/transfers/${sent.id}/reverse?x=1" and "/transfers/:id/reverse" -> segments with :p
pub fn normalize(path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    Regex::new(r"\$\{[^}]*\}|:[A-Za-z_]+").unwrap().replace_all(path, ":p").trim_end_matches('/').to_string()
}

/// A pattern segment ":p" matches any segment of a concrete path.
pub fn route_matches(pattern: &str, path: &str) -> bool {
    let (a, b): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
    a.len() == b.len() && a.iter().zip(&b).all(|(p, s)| *p == ":p" || *s == ":p" || p == s)
}

fn endpoints(re: &str, text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Regex::new(re).unwrap().captures_iter(text)
        .map(|c| (c[1].to_string(), normalize(&c[2]))).collect();
    out.sort();
    out.dedup();
    out
}

pub fn existing(routes_js: &str) -> Vec<(String, String)> {
    endpoints(r#"method:\s*'(\w+)',\s*path:\s*'([^']+)'"#, routes_js)
}

pub fn added_by_ticket(body: &str) -> Vec<(String, String)> {
    endpoints(r"\bAdd\s+(GET|POST|PUT|PATCH|DELETE)\s+(/[\w/:.\-]*\w)", body)
}

pub fn called_by_test(src: &str) -> Vec<(String, String)> {
    endpoints(r#"call\(\s*'(GET|POST|PUT|PATCH|DELETE)'\s*,\s*['"`]([^'"`]+)['"`]"#, src)
}

/// tickets and tests are (task id, text) pairs.
pub fn mechanical_deps(routes_js: &str, tickets: &[(String, String)], tests: &[(String, String)]) -> Vec<Dep> {
    let have = existing(routes_js);
    let is_existing = |(m, p): &(String, String)| have.iter().any(|(hm, hp)| hm == m && route_matches(hp, p));
    let adds: Vec<(String, (String, String))> = tickets.iter()
        .flat_map(|(id, body)| added_by_ticket(body).into_iter()
            .filter(|e| !is_existing(e)).map(move |e| (id.clone(), e)))
        .collect();
    let mut deps: Vec<Dep> = Vec::new();
    for (task, src) in tests {
        for (m, p) in called_by_test(src) {
            for (owner, (am, ap)) in &adds {
                if owner == task || *am != m || !route_matches(ap, &p) { continue; }
                if deps.iter().any(|d| &d.task == task && &d.on == owner) { continue; }
                deps.push(Dep { task: task.clone(), on: owner.clone(),
                    why: format!("acceptance/{task} calls {m} {p}, which {owner} adds"), source: "analyzer".into() });
            }
        }
    }
    deps
}

/// Reads routes, tickets and acceptance files from `dir` and returns the dependencies.
pub fn analyze(cfg: &Config, dir: &Path, st: &State) -> anyhow::Result<Vec<Dep>> {
    let routes = fs::read_to_string(dir.join(&cfg.repo.routes_file)).unwrap_or_else(|e| {
        eprintln!("analyzer: cannot read {}: {e}; treating every added endpoint as new", cfg.repo.routes_file);
        String::new()
    });
    let tickets: Vec<(String, String)> = st.order.iter().map(|id| (id.clone(), st.tasks[id].body.clone())).collect();
    let tests: Vec<(String, String)> = st.order.iter()
        .filter_map(|id| fs::read_to_string(dir.join("acceptance").join(format!("{id}.test.js"))).ok().map(|s| (id.clone(), s)))
        .collect();
    Ok(mechanical_deps(&routes, &tickets, &tests))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: &str = "  {\n    method: 'POST',\n    path: '/fees/quote',\n  },\n  {\n    method: 'POST',\n    path: '/transfers',\n  },";
    const T02: &str = "Customers need to withdraw cash at an agent. Add POST /cashouts with body { \"account\": string, \"amount\": integer }. POST /fees/quote must accept type \"cashout\" and answer from the same schedule.";
    const T07: &str = "Financial inclusion commitment from the board: any send of 1,000 shillings or less is free. The fee is 0 for those amounts in the transfer itself, in the ledger entry (no fee line at all) and in POST /fees/quote.";
    const T12_TEST: &str = "const result = await app.call('POST', '/cashouts', { account: 'duty-a', amount: 100_000 });\nconst quote = await app.call('POST', '/fees/quote', { type: 'cashout', amount: 10_000 });";
    const T01_TEST: &str = "await app.call('POST', '/fees/quote', { type: 'send', amount });";

    fn s(a: &str, b: &str) -> (String, String) { (a.to_string(), b.to_string()) }

    #[test]
    fn t12_depends_on_t02() {
        let deps = mechanical_deps(ROUTES, &[s("T02", T02), s("T07", T07)], &[s("T12", T12_TEST), s("T01", T01_TEST)]);
        assert_eq!(deps.len(), 1, "{deps:?}");
        assert_eq!((deps[0].task.as_str(), deps[0].on.as_str()), ("T12", "T02"));
    }

    #[test]
    fn mentioning_an_endpoint_without_add_adds_nothing() {
        assert!(added_by_ticket(T07).is_empty());
        assert_eq!(added_by_ticket(T02), vec![s("POST", "/cashouts")]);
    }

    #[test]
    fn backtick_path_normalizes() {
        assert_eq!(normalize("/transfers/${sent.id}/reverse"), "/transfers/:p/reverse");
        assert_eq!(called_by_test("app.call('POST', `/transfers/${sent.id}/reverse`);"), vec![s("POST", "/transfers/:p/reverse")]);
        assert!(route_matches("/transfers/:p", "/transfers/tr_999999"));
        assert!(!route_matches("/transfers/:p", "/transfers/:p/reverse"));
    }

    #[test]
    fn never_depends_on_itself_or_on_existing_routes() {
        let t10 = "Add POST /transfers/:id/reverse with no body.";
        let t10_test = "app.call('POST', `/transfers/${sent.id}/reverse`);";
        let fake = "Add POST /transfers to fix things.";
        let deps = mechanical_deps(ROUTES, &[s("T10", t10), s("T99", fake)], &[s("T10", t10_test), s("T01", "app.call('POST', '/transfers', {})")]);
        assert!(deps.is_empty(), "{deps:?}");
    }
}
