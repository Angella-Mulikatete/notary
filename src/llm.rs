use std::{collections::BTreeMap, time::Duration};

use crate::{config::{Config, Price}, state::CostBasis};

pub struct Provider { pub base_url: String, pub key: String, pub basis: CostBasis }
pub struct Reply { pub text: String, pub cost_usd: f64, pub basis: CostBasis, pub usage: serde_json::Value }

/// Builds a provider from notary.toml; the key comes from the environment and is never printed.
pub fn provider(cfg: &Config, name: &str) -> anyhow::Result<Provider> {
    let pc = cfg.providers.get(name).ok_or_else(|| anyhow::anyhow!("unknown provider {name}"))?;
    let key = std::env::var(&pc.key_env).map_err(|_| anyhow::anyhow!("{} is not set", pc.key_env))?;
    Ok(Provider { base_url: pc.base_url.trim_end_matches('/').to_string(), key, basis: pc.cost })
}

pub fn computed_cost(price: &Price, usage: &serde_json::Value) -> f64 {
    let input = usage["prompt_tokens"].as_f64().unwrap_or(0.0);
    let completion = usage["completion_tokens"].as_f64().unwrap_or(0.0);
    let total = usage["total_tokens"].as_f64().unwrap_or(input + completion);
    let output = (total - input).max(completion); // catches thinking tokens left out of completion
    (input * price.input + output * price.output) / 1_000_000.0
}

pub fn chat(p: &Provider, prices: &BTreeMap<String, Price>,
            model: &str, system: &str, user: &str, timeout_secs: u64) -> anyhow::Result<Reply> {
    let mut body = serde_json::json!({ "model": model, "messages": [
        { "role": "system", "content": system }, { "role": "user", "content": user } ] });
    if p.basis == CostBasis::Reported {
        body["usage"] = serde_json::json!({ "include": true }); // OpenRouter: put cost in usage
    }
    let url = format!("{}/chat/completions", p.base_url);
    // Gemini's 503 "high demand" spikes last tens of seconds; one 5s retry lost T01 in the first practice run
    const BACKOFF_SECS: [u64; 3] = [5, 15, 30];
    let mut tries = 0;
    let resp: serde_json::Value = loop {
        let r = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", p.key))   // header, never the URL
            .timeout(Duration::from_secs(timeout_secs))
            .send_json(body.clone());
        let wait = BACKOFF_SECS.get(tries).copied();
        tries += 1;
        match r {
            Ok(resp) => break resp.into_json()?,
            Err(ureq::Error::Status(code, resp)) => {
                let text = resp.into_string().unwrap_or_default();
                if let (true, Some(w)) = (code == 429 || code >= 500, wait) {
                    eprintln!("llm: {model} returned {code}; retry {tries} in {w}s");
                    std::thread::sleep(Duration::from_secs(w));
                    continue;
                }
                anyhow::bail!("llm: {model} returned {code}: {}", text.chars().take(400).collect::<String>());
            }
            Err(e) if tries == 1 => {
                eprintln!("llm: {model} request failed ({e}); retrying once in 5s");
                std::thread::sleep(Duration::from_secs(5));
            }
            Err(e) => anyhow::bail!("llm: {model} request failed: {e}"),
        }
    };
    let text = resp["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string();
    let usage = resp["usage"].clone();
    let (cost_usd, basis) = match (p.basis, usage["cost"].as_f64()) {
        (CostBasis::Reported, Some(c)) => (c, CostBasis::Reported),
        _ => {
            let price = prices.get(model).ok_or_else(|| anyhow::anyhow!("no price for {model} in notary.toml"))?;
            // a provider that should report cost but didn't gets our calculation, labelled as an estimate
            let basis = if p.basis == CostBasis::Reported { CostBasis::Estimated } else { p.basis };
            (computed_cost(price, &usage), basis)
        }
    };
    Ok(Reply { text, cost_usd, basis, usage })
}

pub fn ping(cfg: &Config, provider_name: &str, model: &str) -> anyhow::Result<()> {
    let p = provider(cfg, provider_name)?;
    let r = chat(&p, &cfg.prices, model, "You are a terse assistant.", "Reply with the word ok", 60)?;
    println!("text:  {}", r.text.trim());
    println!("cost:  ${:.8} ({:?})", r.cost_usd, r.basis);
    println!("usage: {}", r.usage);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_tokens_are_billed_as_output() {
        let price = Price { input: 1.0, output: 10.0 };
        // gemini-3.6-flash on 2026-10-26: completion 1, total 99 (95 thinking tokens)
        let usage = serde_json::json!({ "prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 99 });
        let cost = computed_cost(&price, &usage);
        assert!((cost - (3.0 * 1.0 + 96.0 * 10.0) / 1e6).abs() < 1e-12);
    }
}
