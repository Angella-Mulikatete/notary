# Notary: write-up

Notary is a multi-agent coding harness written from scratch in Rust: one binary, no agent frameworks. It takes a backlog of tickets, runs three coding agents in parallel on separate git worktrees, merges their work through a gate, and claims a task is done only when its own evidence shows it. Run it with `notary init`, `shadow`, `plan`, `run` and `verify` (see README).

## What it does

- **Three agents, three tools.** Slot 1 is a patch agent on Gemini 3.1 Flash-Lite, slot 2 a patch agent on gpt-oss:120b (Ollama Cloud), slot 3 Claude Code (Sonnet) as a CLI agent. Patch agents return whole files and get up to three rounds with test feedback; Claude Code edits its worktree directly. Hard tasks and retries go to the slot most likely to finish them.
- **Isolation.** Every attempt runs in its own worktree on its own branch, and only `src/` and `docs/` may change. A merge gate rebases onto the integration branch, repairs conflicts with a cheap model, fast-forwards, reruns the regression suite, the lock check and every merged task's test, and reverts on red.
- **Budget guard.** The USD 5 cap and a USD 0.60 per-task ceiling are settings in `notary.toml`. Every model call reserves before it runs and settles after, and the ledger is logged with its cost basis (computed from tokens, or reported by the CLI).
- **Crash recovery.** State is saved atomically on every transition. `notary resume` re-verifies interrupted work, re-gates merges that were cut off, and charges interrupted reservations to the run without eating the task's retry.
- **Verified "done".** A task is Merged only if its test failed at the commit the agent started from, passes after, and the full suite stays green. `notary verify` rechecks every claim from scratch, independently of the run.
- **Dependencies and contradictions.** A static analyzer finds endpoint dependencies, and one planner call adds hidden ones and flags tests that cannot both pass. The operator picks the winner, or after a timeout the first-listed task wins, and the loser is blocked at $0 rather than burned.
- **Shadow tests.** The live backlog came without acceptance tests, so `notary shadow` has a model write one per ticket before work starts. A test is kept only if it parses, imports nothing but the public test helpers (black-box, through the HTTP API), does not crash, and fails on the base commit. A second model pass then re-derives every expected number and drops assertions that other tickets would invalidate. Kept tests are committed as the new base, so every agent, gate and verify step uses them.

## Results

| Run | Merged | Spend | Notes |
|---|---|---|---|
| Live backlog (12 tasks) | 12 / 12 against Notary's own tests | $1.18 of $5.00 | 6 min 20 s. 6 rebase conflicts, all repaired, 0 reverts. One gate catch: H05's first attempt broke H02's test and was retried. `verify`: all checks passed. |
| Practice, real tests | 11 / 12 | $0.88 | The 12th was a true contradiction, blocked at $0. |
| Practice, shadow tests only | 10 claimed, **9 pass the real tests** | $1.58 | The measured honesty rate for the live setup. One false "done" (T09, too-weak test), flagged by Notary's own grader cross-check. |
| Drills | | | Kill mid-run, then resume: all in-flight tasks recovered. $0.05 cap: stopped at $0.046. Contradiction choices a, b and c each behaved as designed. |

Spend is logged at list price. Most of the work ran on free tiers and a Claude subscription, so actual charges were lower.

## What it got wrong

- **Shadow tests are only as good as the model that writes them.** On the practice backlog, about 8 of 11 gpt-oss tests passed on known-correct code. The misses were arithmetic slips and assertions on totals that another ticket legitimately changes. A wrong test blocks a correct task, and a weak test lets an incomplete one through (one in ten claims, measured above). The live claims carry the same risk, and the report says so.
- **No contradictions found in the live backlog.** The planner flagged none. Because each shadow test was written for one ticket, a real conflict between tickets may have been absorbed by one test being too weak. The merge gate caught one cross-ticket break (H05 vs H02), which suggests that some interactions do exist.
- **Claude Code is most of the spend.** 3 of 12 tasks cost about 89% of the live spend ($1.05 of $1.18).
- **`verify` does not refuse to run while a run is still finishing.** In rehearsal, one task read as unmerged because verify ran mid-merge; a rerun after the run ended was clean.

## What I would fix next

1. **A stronger test writer, plus agreement between two writers.** Gemini 3.6 Flash wrote 9 of 9 faithful tests in the same check, but its free quota ran out. Next: two independent writers, keeping only the assertions both agree on.
2. **A cross-ticket check on the shadow tests themselves.** Run every test against every other ticket's expected behaviour before work starts, so that conflicts surface as contradictions and not as late gate failures.
3. **A lock file so `verify` and `run` cannot overlap**, and a cheaper Claude Code profile for easy retries.

Evidence: `live-run/live-logs.tgz` (events with timestamps and spend, prompts, worker logs, before/after TAP, gate logs) and `live-run/report.md`.
