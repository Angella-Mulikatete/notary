# Notary run report

## Summary

- Tasks: Merged 12
- Spend: $1.1786 of $5.00 cap (by cost basis: computed $0.1387, estimated $1.0399)
- Cost per merged task: $0.0982
- Collisions: 6 rebase conflicts, 6 repaired, 0 merges reverted, 0 wasted attempts
- Run time: 6 min 20 s
- Workers: patch:gemini-3.1-flash-lite, patch:gpt-oss:120b, claude-code:sonnet
- Tests: the backlog shipped without acceptance tests, so Notary wrote one per ticket (`notary-tests/`) before work started; the grader has no file for these tasks and cannot cross-check them

## Tasks

| Task | Title | Status | Confidence | Attempts | Cost | Reason / evidence |
|---|---|---|---|---|---|---|
| H01 | New rate for small sends | Merged | local | a1 patch:gemini/gemini-3.1-flash-lite ($0.0022 Computed): merged | $0.0022 | before fails at a86c25e2, after passes at 1d5f1f06, gate green (runs/H01/) |
| H02 | Pay a merchant | Merged | local | a1 patch:ollama/gpt-oss:120b ($0.0029 Computed): merged | $0.0029 | before fails at a86c25e2, after passes at 97d43ae1, gate green (runs/H02/) |
| H03 | Freeze accounts under investigation | Merged | local | a1 claude-code/sonnet ($0.3205 Estimated): merged | $0.3249 | before fails at a86c25e2, after passes at 79519681, gate green (runs/H03/) |
| H04 | Account ids ignore case | Merged | local | a1 patch:gemini/gemini-3.1-flash-lite ($0.0035 Computed): merged | $0.0043 | before fails at 1d5f1f06, after passes at 1363e745, gate green (runs/H04/) |
| H05 | Merchants keep the full payment | Merged | local | a1 patch:ollama/gpt-oss:120b ($0.0030 Computed): broke H02's acceptance (paying a non-merchant account is rejected with NOT_A_MERCHANT); keep H02's behaviour (runs/H05/broke-a1.tap)<br>a2 claude-code/sonnet ($0.4853 Estimated): merged | $0.4882 | before fails at 79519681, after passes at 0e292f24, gate green (runs/H05/) |
| H06 | References on transfers | Merged | local | a1 patch:gemini/gemini-3.1-flash-lite ($0.0048 Computed): merged | $0.0048 | before fails at 1363e745, after passes at d07ac55f, gate green (runs/H06/) |
| H07 | Per-account currency | Merged | local | a1 patch:ollama/gpt-oss:120b ($0.0026 Computed): merged | $0.0070 | before fails at 1363e745, after passes at d49ac114, gate green (runs/H07/) |
| H08 | Account activity summary | Merged | local | a1 patch:gemini/gemini-3.1-flash-lite ($0.0124 Computed): merged | $0.0153 | before fails at d07ac55f, after passes at ebc0e22c, gate green (runs/H08/) |
| H09 | Bulk transfers for payroll | Merged | local | a1 patch:ollama/gpt-oss:120b ($0.0063 Computed): merged | $0.0108 | before fails at d49ac114, after passes at 2ce05076, gate green (runs/H09/) |
| H10 | Merchant accounts | Merged | local | a1 patch:gemini/gemini-3.1-flash-lite ($0.0051 Computed): merged | $0.0063 | before fails at ebc0e22c, after passes at 2d35310b, gate green (runs/H10/) |
| H11 | Filter statements by kind and date | Merged | local | a1 claude-code/sonnet ($0.2342 Estimated): merged | $0.2342 | before fails at 0e292f24, after passes at f832067e, gate green (runs/H11/) |
| H12 | Show the total a sender pays | Merged | local | a1 patch:ollama/gpt-oss:120b ($0.0030 Computed): merged | $0.0030 | before fails at 2ce05076, after passes at 8ede3971, gate green (runs/H12/) |

## Contradictions

None flagged by the planner.

## Dependencies

- H05 → H02 (analyzer): acceptance/H05 calls POST /payments, which H02 adds

## Final grade

```

> collision-course-ledger@1.0.0 grade
> node scripts/grade.js

regression suite   pass (33 tests, 0 failed)
locked files       unchanged
H01                FAIL (no acceptance file)
H02                FAIL (no acceptance file)
H03                FAIL (no acceptance file)
H04                FAIL (no acceptance file)
H05                FAIL (no acceptance file)
H06                FAIL (no acceptance file)
H07                FAIL (no acceptance file)
H08                FAIL (no acceptance file)
H09                FAIL (no acceptance file)
H10                FAIL (no acceptance file)
H11                FAIL (no acceptance file)
H12                FAIL (no acceptance file)

0 of 12 tasks pass. Details written to grade-report.json.
The exit code covers only the regression suite and the locked files, not the tasks.
```

## Known limitations

- "Done" means Notary's own test for the ticket (`notary-tests/<id>.test.js`, written by a model from the ticket text and required to fail on the base commit) failed before and passes after, plus the regression suite and lock check. If a model misread a ticket, its test is wrong too, so a Merged task can still fail the organizers' hidden tests.
- Claude Code costs are the CLI's own `total_cost_usd` (basis: estimated); on a subscription no money is charged.
- Patch-agent costs are computed from token counts and list prices in notary.toml (basis: computed); on a free tier no money is charged.
- Planner areas and contradictions come from one LLM call and can be wrong; the merge gate is the backstop for contradictions it misses.

Evidence: `.notary/events.jsonl`, `.notary/runs/Txx/` (prompts, worker logs, before/after TAP, diffs, gate logs). Recheck with `notary verify`.
