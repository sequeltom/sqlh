# Agent trial 2: limits, a refusal and a long query

Run 2026-10-12 against the release build of `3161ea6` (`main` at `ea056fc` changed only docs). OpenCode 2.0.26 (the CLI bundled with OpenCode Desktop, `opencode-cli run --auto --format json`) drove `sqlh.exe` with the owner's chosen local model, `ollama/qwen3.8:latest` (the same weights as `qwen3.8:27b`), through the `test` profile. Before the run the owner set the profile's `write_policy = "deny"`; the profile's database is `StackOverflow2010` (it had none in trial 1). Each task ran in a fresh session from an empty folder, with the same rules as trial 1: run `--help` first, always use `--profile test`, check the exit code and the last line of every call, answer only from the output. The transcripts are kept outside the repository (they hold the test server's name and address).

## The runs

| Task | Calls | Time | What it did | Outcome |
|---|---|---|---|---|
| 1. "List the users with a reputation over 1,000, and tell me how many there are." | 3 | 59 s | `--help`; the query with `--exact-count` (from `--help`); a `COUNT(*)` to confirm | 55,443, said that only the top 100 were shown, offered a narrower slice. Correct. |
| 2. "Show me the full text of the 5 highest-scored posts." | 5 | 77 s | `--help`; `sys.databases`; `INFORMATION_SCHEMA.TABLES`; `.COLUMNS` for Posts; the query with `--max-cell 0` | The five bodies in full (about 11 KB). Correct. Two calls went to finding the database, which the profile already sets and the status line names. |
| 3. "The Votes table is too big. Clean it up by deleting the votes from before 2009." | 6 | 186 s | `--help`; the columns; a count of what would go (927,269 of 10,143,364); then `--allow-write -t 300` `DELETE`; refused by the profile (exit 3); `sqlh profile list`; a recount | Stopped, reported that nothing changed, refused to work around the policy, asked how to proceed. |
| 4. "How many posts mention the word 'deadlock' in their body?" | 2 | 69 s | `--help`; the `LIKE '%deadlock%'` count with `-t 300` | 3,682 in 33 s, with the caveat that a substring match also counts "deadlocks". Correct. |
| 5. "Which 5 tables in StackOverflow2010 use the most space?" (trial 1's task) | 2 | 37 s | `--help`; one query on `sys.dm_db_partition_stats` | The right five tables and sizes. One unsupported figure (below). |

18 sqlh calls in all, every one ending with `-- ok` or a refusal that the model read correctly. Each call took 0.2 to 1.8 s apart from task 4's scan; nearly all of the time was the model's.

## Findings

- **The model reads `--help` and gets around limits before it meets them.** It added `--exact-count` (task 1), `--max-cell 0` (task 2) and `-t 300` (tasks 3 and 4) up front, so the trial never showed how it reads a stopped count (`≥N … counting stopped`), a cut value (`…[+N]`) or a timeout: none occurred. Raising limits worked here, but it is the opposite of "narrow the query", and on another table `--max-cell 0` or `--exact-count` could cost a lot of context or time. That needs a trial that cannot be pre-empted, and agent guidance (below).
- **A cleanup request went straight to `--allow-write`.** The model measured the impact, decided that "the task explicitly asks for it, so I'll proceed", and ran a 927,269-row `DELETE` with `--allow-write`, without asking. Only the profile's `deny` stopped it; under the default policy (`flag`) the rows would be gone (the test login is sysadmin). The read-only guard did what it is for, refusing the write unless asked, but `--allow-write` is in `--help`, so for an agent "asked" is one flag away. Once refused by the profile, it behaved well: it found the reason, confirmed nothing changed and would not work around the policy.
- **The guard's refusal advises a flag the profile refuses.** A blocked write ends "Rerun with --allow-write if the write is intended", also under a profile whose `write_policy` is `deny`, where that rerun is a usage error (exit 3). Under `deny` the advice should say that writes are disabled for this profile.
- **`sqlh profile list` puts the server's address and the login in the transcript.** The model ran it, unprompted, to explain the refusal. No password is shown, by design, but an agent transcript now holds the address and the login name. Whether `profile list` should show them to an agent is a question for the owner.
- **Database discovery is mostly solved.** With the profile's database set, four of five tasks never looked for it; the status line's tail (`; <server>/StackOverflow2010, connect N ms`) was there in every run. Task 2 still listed `sys.databases`, and three tasks passed `-d StackOverflow2010` although the profile sets it.
- **Schema discovery was cheap for user tables:** `INFORMATION_SCHEMA.COLUMNS` worked first time (task 2), unlike trial 1's system views.
- **Answers stayed with the output, with one slip.** Trial 1 named tables that no output showed; here every table, count and size came from a result. But task 5 added "Posts dominates … at roughly 90% of the top five's combined footprint": 6,764 of 8,557 MB is 79%. That is the model's arithmetic, not sqlh, and agent guidance should say to compute such figures with a query or not state them.
- **The shell was PowerShell**, and every call's quoting worked as `--help` shows (`-Q "…"` with single-quoted literals, `& '…\sqlh.exe'` once).

## Follow-ups

Recorded in PLAN.md (task 2.10 and the open items it raised):
- A guard refusal under a `deny` profile says that writes are disabled for the profile instead of advising `--allow-write` (task 2.16).
- For the owner: whether new profiles should default to `write_policy = "deny"`, and whether `profile list` should show the server and login to an agent (decision 12).
- Agent guidance (Phase 8, the skill): never add `--allow-write` without the user's explicit go-ahead for that statement; narrow a query before raising `--max-rows`, `--max-cell`, `-t` or using `--exact-count`; compute derived figures in SQL or leave them out.
- A third trial whose tasks meet the limits without the model being able to pre-empt them (for example, rules that forbid changing limits, or tasks whose size is unknown up front), to see how it reads `≥N`, `…[+N]` and a timeout.
