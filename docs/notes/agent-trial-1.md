# Agent trial 1: OpenCode with a local model

Run 2026-10-10 against `fdefab9`. OpenCode drove `sqlh.exe` with a local model (Qwen 3.8 via Ollama), through the `test` profile, read-only. The prompt gave the exe path and a few rules (run `--help` first, use the profile, check exit codes and status lines). The task: "Which 5 tables in StackOverflow2010 use the most space?"

## What happened

11 sqlh calls in about 2.5 minutes, nearly all of it model time; each call took 0.2–0.27 s.

1. Ran `--help` first.
2. Checked `DB_NAME()`: it was `master`, because the `test` profile sets no database. Found StackOverflow2010 through `sys.databases`.
3. Wrote a `.sql` file and ran it with `-i`, adding `USE StackOverflow2010;` instead of passing `-d`.
4. Recovered from four failures, each in one step from the message:
   - error 156 at line 6: `RowCount` is a reserved word;
   - three errors 207: it had joined `sys.partitions`, which has no `reserved_page_count`;
   - exit 3: two `-Q` flags; the usage message told it to quote the whole query as one argument;
   - error 208: it guessed a view that doesn't exist (`sys.system_internals_columns`).
5. Found the columns with `SELECT TOP 1 * FROM sys.partitions`, switched to `sys.dm_db_partition_stats`, and got the answer with `-- ok`, exit 0.

## Findings

- **The output contract works.** The model read typed headers, `-- ok`/`-- failed` and exit codes without trouble, including escaped `\n\t` inside a value.
- **Errors with real line numbers paid off:** every error led to a correct fix on the next call.
- **Schema discovery is the weak spot.** `information_schema.columns` and `sys.columns` return no rows for system views (their columns are in `sys.all_columns` and `sys.system_columns`), and the model invented a view. Five of the eleven calls went to finding one view's columns; a `describe` command would have taken one.
- **The default database surprised it.** Without `-d`, a profile with no database lands in `master`.
- **The final answer contained a hallucination:** it named tables ("Tags", "PostHistory") that appear in no output, and gave an unsupported reason for Posts' size. That is the model, not sqlh, but agent guidance should say to state only what the output shows.
- **Not exercised:** row and value limits, guard refusals, writes, long-running queries.

## Follow-ups

Recorded in PLAN.md: task 2.9 (help tips), Phase 4 (evidence for `describe`), Phase 8 (skill guidance), and a second trial that exercises limits and refusals.
