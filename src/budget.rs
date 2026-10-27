use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Ledger {
    pub cap_usd: f64,
    pub task_ceiling_usd: f64,
    pub spent_usd: f64,
    pub reserved: BTreeMap<String, f64>, 
    pub per_task: BTreeMap<String, f64>,
    pub by_basis: BTreeMap<String, f64>,
    #[serde(default)]
    pub crash_estimates: BTreeMap<String, f64>,  // part of per_task settled in full by resume
}

impl Ledger {
    pub fn available_for(&self, task: &str) -> f64 {
        let reserved: f64 = self.reserved.values().sum();
        let run_left = self.cap_usd - self.spent_usd - reserved;
        let charged = self.per_task.get(task).copied().unwrap_or(0.0) - self.crash_estimates.get(task).copied().unwrap_or(0.0);
        let task_left = self.task_ceiling_usd - charged;
        run_left.min(task_left).max(0.0)
    }
    /// Sets aside `amount` or refuses. The scheduler calls this before dispatching.
    pub fn reserve(&mut self, res_id: &str, task: &str, amount: f64) -> anyhow::Result<()> {
        anyhow::ensure!(amount > 0.0 && amount <= self.available_for(task) + 1e-9,
            "budget: cannot reserve ${amount:.4} for {task} (available ${:.4})", self.available_for(task));
        self.reserved.insert(res_id.to_string(), amount);
        Ok(())
    }
    pub fn settle(&mut self, res_id: &str, task: &str, actual: f64, basis: &str) {
        self.reserved.remove(res_id);
        self.spent_usd += actual;
        *self.per_task.entry(task.to_string()).or_default() += actual;
        *self.by_basis.entry(basis.to_string()).or_default() += actual;
    }
    /// Resume: an interrupted reservation is charged in full to the run (money may have been spent)
    /// but not to the task's ceiling, so the task still gets its retry.
    pub fn settle_crashed(&mut self, res_id: &str, task: &str, amount: f64) {
        self.settle(res_id, task, amount, "estimated");
        *self.crash_estimates.entry(task.to_string()).or_default() += amount;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(cap: f64, ceiling: f64) -> Ledger {
        Ledger { cap_usd: cap, task_ceiling_usd: ceiling, ..Default::default() }
    }

    #[test]
    fn reserve_over_cap_fails() {
        let mut l = ledger(0.10, 1.0);
        assert!(l.reserve("T01-a1", "T01", 0.11).is_err());
        assert!(l.reserve("T01-a1", "T01", 0.10).is_ok());
        assert!(l.reserve("T02-a1", "T02", 0.01).is_err()); // cap fully reserved
    }

    #[test]
    fn settle_lower_frees_the_difference() {
        let mut l = ledger(1.0, 1.0);
        l.reserve("T01-a1", "T01", 0.60).unwrap();
        assert!((l.available_for("T02") - 0.40).abs() < 1e-9);
        l.settle("T01-a1", "T01", 0.25, "computed");
        assert!((l.available_for("T02") - 0.75).abs() < 1e-9);
        assert!((l.spent_usd - 0.25).abs() < 1e-9);
        assert!((l.by_basis["computed"] - 0.25).abs() < 1e-9);
    }

    #[test]
    fn crash_estimate_counts_for_the_run_but_not_the_task_ceiling() {
        let mut l = ledger(2.0, 0.5);
        l.reserve("T02-a1", "T02", 0.5).unwrap();
        l.settle_crashed("T02-a1", "T02", 0.5);
        assert!((l.spent_usd - 0.5).abs() < 1e-9);
        assert!((l.available_for("T02") - 0.5).abs() < 1e-9);
        assert!((l.available_for("T03") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn per_task_ceiling_enforced() {
        let mut l = ledger(5.0, 0.15);
        l.reserve("T01-a1", "T01", 0.10).unwrap();
        l.settle("T01-a1", "T01", 0.10, "estimated");
        assert!(l.reserve("T01-a2", "T01", 0.06).is_err());
        assert!(l.reserve("T01-a2", "T01", 0.05).is_ok());
    }
}
