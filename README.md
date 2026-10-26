# Notary
Notary runs three coding agents on a backlog at the same time without letting them collide. Each task gets its own git worktree. A task counts as done only when Notary proves it: the task's locked acceptance test **fails** on the code the agent started from, **passes** on the agent's commit, and the full regression suite plus every previously merged task's acceptance test stays green after merging. Every claim in the final report can be re-checked with one command.

All coordination code (task state, scheduling, isolation, budget, verification, merging, crash recovery) is written from scratch in this repository. No multi-agent orchestration framework is used.

## How it works

`notary run` executes this pipeline:

1. **Preflight**: the target repo is clean, `npm test` is green, the test lock is intact.
2. **Probe**: every `acceptance/Txx.test.js` runs on the base commit. A task whose tests already pass is marked AlreadySatisfied. Ticket text such as "already shipped" is never trusted.
3. **Analyze**: acceptance files are scanned for the endpoints they call, and tickets for the endpoints they add. This finds dependencies without an LLM (for example, a test that calls `POST /cashouts` depends on the ticket that adds it).
4. **Plan**: one LLM call groups tasks by the code area they change, adds dependencies the scan can't see, and flags pairs of tasks whose acceptance tests cannot both pass. For each contradiction the operator chooses which task to keep (with a timed default). The other task is blocked with evidence and no tokens are spent on it.
5. **Build**: three worker slots run in parallel. Each task attempt gets a fresh worktree branched from the current integration tip. Tasks in the same area never run at the same time.
6. **Verify** (per task, in its worktree): something changed; only `src/**` or `docs/**` changed; no `TODO`, `.skip` or `.only` added; the acceptance test passes; `npm test` passes; `npm run verify:tests` passes.
7. **Merge gate** (one task at a time): rebase onto integration, fast-forward, then run the regression suite, the lock check, and the acceptance tests of **every merged task**. Red means the merge is reverted. If an earlier task's test broke, the new task is blocked as a contradiction, with the failing assertions as evidence. After each merge, the remaining tasks' acceptance tests are rerun, so work satisfied as a side effect is never paid for twice.
8. **Final**: `npm run grade` runs on the integration branch, its output is cross-checked against Notary's claims, `main` is fast-forwarded, and the report is written.

## Agents and tools

| Slot | Tool | Model | Role |
|---|---|---|---|
| 1 | Notary patch agent (this repo, `src/workers/patch.rs`) | Gemini, cheap tier | First attempts |
| 2 | Notary patch agent | A second model (OpenRouter or Gemini) | First attempts |
| 3 | Claude Code (`claude -p`), single-agent CLI | Claude | Harder tasks and all retries |

That is three agents across two tools and at least two model families. A retry always goes to a different worker than the first attempt. Workers never commit, run git, or touch locked files. Notary writes only the paths it allows, and verifies the diff afterwards.

## Dependencies

- **Rust** (stable) and these crates (see `Cargo.toml`): `anyhow`, `clap`, `serde`, `serde_json`, `toml`, `ureq`, `regex`, `sha2`, `hex`, `dotenvy`, `wait-timeout`, `globset`, `chrono`
- **Node.js 20+**, to run the target repository's tests
- **Git**
- **Claude Code** (`@anthropic-ai/claude-code`), used only as a single-agent worker
- **Model APIs**: Google Gemini API (OpenAI-compatible endpoint), optionally OpenRouter

No multi-agent orchestration frameworks (CrewAI, AutoGen, LangGraph or similar), and no Agent HQ code, binaries or gateway, are used or installed.

## Setup

```bash
# 1. Tools
rustc --version && node --version && git --version
npm install -g @anthropic-ai/claude-code && claude        # log in once

# 2. Build
git clone <this-repo-url> notary && cd notary
cargo build --release

# 3. Secrets (never committed)
cat > .env <<'EOF'
GEMINI_API_KEY=...
OPENROUTER_KEY_SLOT2=...   # only if slot 2 uses OpenRouter
EOF

# 4. Configure
#    edit notary.toml: model IDs, prices per 1M tokens, and run_cap_usd
```

## Run (one command)

```bash
./target/release/notary run --repo /path/to/collision-course-ledger
```

Watch progress from a second terminal:

```bash
watch -n 2 ./target/release/notary status --repo /path/to/collision-course-ledger
```

## Commands

| Command | What it does |
|---|---|
| `notary init` | Creates `.notary/`, excludes it from git, creates the integration worktree |
| `notary probe` | Runs every acceptance file on the base commit |
| `notary plan` | Probe + analyzer + planner; asks about contradictions; writes `plan.json` |
| `notary run` | The full pipeline (reuses an existing `plan.json`) |
| `notary resume` | Continues after a crash or interruption |
| `notary status` | Task table, active slots, spend against the cap |
| `notary verify` | Re-checks the before-and-after proof of every merged task and runs the grader |
| `notary clean` | Removes Notary's worktrees and branches |

All commands take `--repo <path>` and `--config <file>` (default `notary.toml`).

## Configuration

`notary.toml` holds the repository commands, the writable and locked paths, the budget, the policies, the providers and prices, and the three worker slots. The budget cap is a setting: set `run_cap_usd` to the cap announced by the organizers. Keys are read from `.env` by the variable names in `[providers.*].key_env` and are never logged.

## Budget

- **Hard run cap and per-task ceiling.** Before any worker starts, Notary reserves its maximum spend. If the reservation would exceed the cap, the work doesn't start, and the task is reported as skipped for budget.
- **Workers can't overspend their reservation.** The patch agent checks before every model call, and Claude Code gets `--max-budget-usd`.
- **Every cost is labelled by how it was measured**:
  - *computed*: Gemini token counts × configured prices
  - *reported*: OpenRouter's `usage.cost`
  - *estimated*: Claude Code's `total_cost_usd`, because it runs on a subscription

## Crash recovery

`state.json` is written atomically (temporary file, then rename) after every state change, and every change is also appended to `events.jsonl`. `notary resume` handles leftovers as follows:

- Settles any open reservations at their full amount.
- Re-verifies anything that was in progress.
- For an interrupted merge, checks whether the commit landed, then re-gates it or resets integration to its last good commit.

Nothing from before the crash is trusted without re-running its tests.

## Output and evidence

Everything is written to `.notary/` in the target repository:

| Path | Contents |
|---|---|
| `report.md`, `report.json` | Status of every task with its reason, attempts, workers, cost, collision counters and the grader's output |
| `events.jsonl` | Timestamped log of every decision and state change |
| `runs/Txx/` | Prompts, agent responses, before/after test output (TAP), diff, merge-gate log |
| `plan.json` | Dependencies (marked analyzer or planner), areas, contradiction decisions |
| `final-grade.json` | `grade-report.json` from the final integration commit |

Task statuses:

| Status | Meaning |
|---|---|
| **Merged** | Proven and merged; a confidence label notes whether advisory shadow tests also passed |
| **AlreadySatisfied** | Its acceptance test passed before any work: at base, or after another task merged |
| **Blocked** | Contradiction, or failed after two attempts by different workers. The reason is given |
| **Skipped** | Never attempted (budget cap, or a dependency was blocked). The reason is given |

Any merged task that the grader reports as failing is flagged as a `DISCREPANCY` in the report rather than hidden.

## Known limitations

- The organizers' hidden tests are not available to Notary. Passing the local acceptance tests is evidence, not a guarantee.
- Contradictions are flagged by the planner and confirmed by running tests. A contradiction the planner misses is caught by the merge gate, but only after one implementation has been paid for.
- Claude Code's cost is an estimate, because it runs on a subscription.

## License

MIT
