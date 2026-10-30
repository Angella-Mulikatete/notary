# Collision Course: how Notary was graded

Organizer grading for the Collision Course hackathon (Kolaborate Academy, 23 to 30 October 2026). Results were published on the platform on 30 October 2026.

**Total: 0.701 of 1.000, ranked 1 of 3. Winner.**

| Criterion | Weight | Score |
|---|---|---|
| Hidden tests | 0.40 | 0.291 |
| Collision efficiency | 0.20 | 0.13 |
| Cost against cap | 0.15 | 0.13 |
| Honesty of done claims | 0.15 | 0.07 |
| Process evidence | 0.10 | 0.08 |

## What was graded

- Harness: `master@ae7a3b8`.
- Run evidence: `live-run/live-logs.tgz` and `live-run/report.md`, committed on 29 October at 16:25 EAT, before the 09:00 EAT deadline on 30 October.

## Hidden tests: 0.291

The final ledger code your run produced (integration tip `f832067e`) is not in the repository. The organizers rebuilt it from your logs:

1. The 12 task diffs (`runs/Hxx/diff-aN.patch`) were replayed in the order of the `merged` events.
2. Each rebase was redone as a 3-way merge. Where a conflict was logged, the repair output in `repair-a1.log` was used.
3. Your repair log keeps only the last file of a multi-file repair, so some repairs were missing. Each intermediate state was therefore replaced by the full source snapshot in the next worker prompt (`prompt-aN.md`) that started from that commit.
4. Every step matched the blob hashes in your diffs. The rebuilt tree passes all 51 assertions of your own shadow tests, the same result as your `notary verify` run.

The organizer grading script then replaced `test/`, `acceptance/`, `scripts/`, `backlog/`, `package.json` and `tests.lock.json` with the organizers' copies and ran the hidden acceptance tests.

| Task | Result | Task | Result |
|---|---|---|---|
| H01 | fail, 1 of 6 | H07 | pass, 6 of 6 |
| H02 | fail, 4 of 8 | H08 | fail, 6 of 9 |
| H03 | fail, 9 of 11 | H09 | pass, 9 of 9 |
| H04 | pass, 8 of 8 | H10 | pass, 6 of 6 |
| H05 | pass, 5 of 5 | H11 | pass, 8 of 8 |
| H06 | pass, 7 of 7 | H12 | pass, 6 of 6 |

The regression suite passed 33 of 33 and the locked files were unchanged. H02 and H05 contradict each other by design, so they count as one unit. That makes 8 of 11 units, and 0.40 × 8/11 = 0.291.

## Collision efficiency: 0.13 of 0.20

- Real parallel work with three slots: 6 rebase conflicts, all repaired, and 0 merges reverted.
- The merge gate caught H05 breaking H02.
- Points lost on ordering. H02 merged before H10, the task that creates merchant accounts, and the analyzer found only one dependency (H05 needs H02).

## Cost against cap: 0.13 of 0.15

- USD 1.18 of the USD 5 cap, with a reserve-before-call guard and a per-task ceiling.
- The guard was proven in a drill: at a USD 0.05 cap the run stopped at USD 0.046.
- Not full marks, because the Claude Code spend (USD 1.04 of the 1.18) is the CLI's own estimate.

## Honesty of done claims: 0.07 of 0.15

This criterion cost the most points.

- The report claims all 12 tasks merged, but 4 of them fail the hidden tests.
- Both sides of the H02/H05 contradiction were claimed. When H05's first attempt broke H02 at the gate, Notary retried until both of its shadow tests passed, instead of raising the clash as a contradiction.
- In your favour: H07, the ticket that only sounds finished, was really built and passes. H12's `total` field was added. The report labels its confidence as local and states plainly that the shadow tests may be wrong.

## Process evidence: 0.08 of 0.10

- Very full evidence: timestamped events, per-task TAP output, prompts, diffs and gate logs, plus a candid write-up.
- Points lost because the produced code was not committed and the repair log is lossy. Without the worker prompts, the result could not have been reproduced.

## Rule checks

- No banned dependencies: 13 utility crates, and no orchestration framework or Agent HQ code.
- Locked test files unchanged. Your shadow tests went into `notary-tests/`, which the grader ignores.
- Three worker slots across three model families and two tools.
- On time.

## How sensitive is this result?

| Change | Notary total |
|---|---|
| Hidden tests scored by assertion share (75 of 89) | 0.746 |
| Honesty scored 0 | 0.631 |
| Rebuild not accepted, so hidden tests score 0 | 0.390 |

Notary stays first in every case except the last one. The organizer accepted the rebuild.

## What would have scored higher

1. Commit the integration branch, or attach a `git bundle` of it, with the run evidence.
2. Treat a gate failure between two tickets as a possible contradiction, and report it, instead of retrying until both shadow tests pass.
3. Write every repaired file to the repair log, not only the last one.

The full grading report for all entries is shared separately by the organizers. The hidden acceptance test code stays private.

Grader: Calvin Magezi (organizer), with an AI assistant (Claude) running the grading script and the rebuild. Disclosure: you are a member of Kolaborate staff. Staff were permitted to enter, and every entry was graded with the same rubric and script.
