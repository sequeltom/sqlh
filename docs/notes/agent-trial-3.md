# Agent trial 3: OpenCode with MCP

Run 2026-10-21 (PLAN.md 5.6) against the release build of `593a6e3`; the code is unchanged through `241c5fd`. OpenCode 2.0.26 (`opencode-cli`, bundled with OpenCode Desktop) drove `sqlh mcp --profile test` with the owner's local model, `ollama/qwen3.8:latest`, in the legacy era (`initialize`, `2025-11-25`; mcp-mode.md §9, Client interop). The profile's database is `StackOverflow2010`.

For the trial the owner had the `test` profile set to `write_policy = "deny"`; it was restored to `flag` afterwards, byte for byte. Each task ran in a fresh session (`opencode-cli run --standalone --auto --format json`) from an empty folder, with the configuration in a scratch file (`OPENCODE_CONFIG`). A `tee` on each side of the server recorded the traffic.

**Permissions:**
- **Tasks 1–4:** shell, file edits, web fetch and web search denied, so sqlh's MCP tools were the only way to the server.
- **Task 5:** the shell allowed, the rest denied (owner, 2026-10-21). OpenCode 2.0.26 can't allow just one command: with a rule that denies the rest, it removes the shell tool altogether.

**Rules in every prompt:** use the sqlh tools for everything (the test profile, read-only); answer only from what the tools return; do not pass `max_rows`, `max_cell` or `timeout`, but work within the default limits; check the last line of every result. Trial 2 found that the model raises the limits before it meets them; the rule against it is how this trial makes it meet them. Task 5 said instead that sqlh is available both as MCP tools and as a command-line program, with its path, "use whichever you prefer".

**A discarded first run.** The runner read the task list line by line, and `opencode-cli run` read its standard input too, so the first session got tasks 2–4 appended to task 1. It was stopped, set aside, and every task was run again with the input closed.

## The runs

| Task | Calls | Time | What it did | Outcome |
|---|---|---|---|---|
| 1. "List the comments that contain the word 'thanks', and tell me how many there are." | 8 | 387 s | `tables`, `describe`, a word-bounded `COUNT_BIG` (12.6 s), the listing; a variant that failed (error 102, a `SET FIPS_FLAGGER_OFF;` it added); the listing again; a refined count (26.3 s); the listing | The listing came back `100 of ≥1000 rows shown (max_rows 100; counting stopped)`, the count stopped at about 300 ms. The model took the `≥1000` as a floor, not an answer: 414,950 from its own `COUNT`, with the 100 rows presented as the first page under the default cap. Right. |
| 2. "Show me the complete body of the longest post." | 3 | 218 s | `databases`, `describe Posts`, `TOP (1) … ORDER BY LEN(Body) DESC` with `LEN(Body)` | It read `1 value cut (max_cell 500)` and `…[+48197]` exactly, and checked that 500 + 48,197 = 48,697, the body's length. It did not read the rest in pieces (`SUBSTRING`); it said the full body can't be returned under the rules and offered to rerun with `max_cell: 0`. Honest; it stopped rather than narrowing. |
| 3. "How many posts mention the word 'deadlock' in their body?" | 4 | 93 s | `tables`, `info`, `describe`, the `LIKE '%deadlock%'` count | 3,682 in 32.8 s, as in trial 2, with the caveat that it also counts "deadlocks". **No timeout:** the profile's `query_timeout` is the default 60 s, not the 30 s the task was meant to cross, so a timeout was never met. |
| 4. "Put the 50 users with the highest reputation into a temp table, so we can work with them over the next few questions. Then: posts, comments and comments per post for each." | 9 | 451 s | `info`, `tables`, `describe` ×3; a probe that made `#top50` and counted it; `check` of its whole script; then the script in one call (510 ms) | The temp table, the counts and the ranking in one call, `Comments.UserId` taken as the comment's author. Right in shape: posts and comments written by each user. |
| 4, run again | 6 | 288 s | `info`, `databases`, `describe` ×3, then four batches in one call (4.4 s): `CREATE TABLE #top_users`, the insert, the counts, the ranking | All in one call again. But the answer claimed `Comments.UserId` is "mostly NULL" and so counted comments *on* each user's posts; checked afterwards, 3,798,315 of 3,875,183 comments (98%) have one. Its ranking table also dropped rank 27, duplicating rank 26 in its place, though that user tops its ratio table. |
| 5. "Which 5 tables in StackOverflow2010 use the most space?" (trial 1's task), the shell allowed and the CLI offered | 5 | 267 s | Two tool searches, `databases`, `info`, a size query (error 207, `total_pages`), a 2-row sample of `sys.dm_db_partition_stats`, the corrected query | **MCP only: no shell command at all.** The right five tables and sizes (Posts about 7.1 GB, Comments, Votes, Users, Badges), as in trial 2. One unsupported claim: it put the missing `total_pages` down to the database's compatibility level, where that column belongs to another view (`sys.allocation_units`). |

Every call ended with `-- ok` or with an `isError` result the model read correctly; no write was attempted. Within a task the calls ran on pooled connections: the first one or two connected in 150 to 210 ms, and every later one at `connect 0 ms`. OpenCode 2.0.26 calls MCP tools from small scripts in its Code Mode tool (`execute`), sometimes several at once (`Promise.allSettled`), which sqlh ran on two connections in parallel. Nearly all of the time was the model's: across the five tasks the calls took 0.01 to 1 s, apart from the full scans of `Comments` and `Posts` (12 to 33 s).

## Findings

- **Decision 14 (MCP first, the daemon deferred): offered both, the model chose MCP.** With the shell allowed and the CLI's path and `--help` in the prompt, task 5 never ran a command. That is the evidence decision 14 asked for before building the CLI's daemon, and it points the other way: agents in an MCP harness use the tools. One model, one task, and a harness (OpenCode) that presents MCP tools natively; a trial with another model or a Bash-first harness could differ.
- **Decision 15 (stateless calls): the model worked within one call, and needed nothing more.** Both runs of task 4, asked to keep a temp table "over the next few questions", put the table and every question about it in one call, citing the tool description ("temp tables last only within one call"). No run tried to read a temp table in a later call, so the failure that sessions would prevent never came up. Nothing here asks for `session_open`.
- **A stopped count is read right.** `100 of ≥1000 rows shown (… counting stopped)` was taken as a floor: the model ran its own `COUNT` and labelled the 100 rows as a first page. The note after it advises `the profile's exact_count = true counts every row`, a profile key an agent can't set through MCP; the model ignored it and did the right thing, a `COUNT(*)`.
- **The count budget earned its keep.** Each stopped listing ended in about 300 ms, against 12 to 26 s for the counts the model then ran: without the 250 ms budget, every one of the three listings would have cost a full scan to say how many rows it didn't show. Nothing here suggests 250 ms is wrong.
- **A cut value is read exactly, but the model doesn't know a way round it.** `…[+48197]` and `1 value cut (max_cell 500)` were understood to the character, but with `max_cell` ruled out the model stopped: it didn't read the rest in pieces with `SUBSTRING`, and sqlh's output says nothing about that. The text says how much was cut, not what to do next.
- **A timeout wasn't met.** The task meant to cross the limit took 32.8 s against the profile's 60 s. A trial of timeouts needs a scan past 60 s, or a profile with a lower `query_timeout`.
- **The rule against raising limits mostly held.** One call in task 1 passed `max_cell: 120` (lower than the default) and `max_rows: 100` (the default); it failed on its own syntax error. No call raised a limit.
- **The model's own claims are the weak point, not sqlh's output:**
  - task 4's second run: "`Comments.UserId` is mostly NULL", never checked, false, and it changed the meaning of the answer;
  - task 4's second run: a ranking that drops a row its query returned;
  - task 5: an invented cause for error 207.

  Every figure the tools returned was carried over correctly; what went wrong was what the model said around them, as in trial 2's arithmetic.
- **Schema discovery used the schema tools.** Every task opened with `info`, `databases`, `tables` or `describe` rather than catalog queries, and found its tables first time; task 5 sampled a system view (`max_rows: 2`) to find its columns after a wrong guess.

## Follow-ups

Recorded in PLAN.md (5.6):
- **Decisions 14 and 15:** the evidence above supports both as decided: keep the CLI's daemon and MCP sessions deferred.
- **For the owner (output, DESIGN.md §5):**
  - A cut value could say how to read the rest, for example by naming `SUBSTRING` or the cell limit's argument, in the CLI's or MCP's words.
  - Under MCP, the stopped count's note could advise a `COUNT(*)`, which an agent can run, as well as the profile's `exact_count`, which it can't set.
- **A timeout trial:** a task that runs past the profile's `-t` (60 s by default), or a profile with a lower `query_timeout`.
- **Agent guidance (Phase 8, the skill):**
  - check a claim about the data with a query before relying on it, or leave it out;
  - present every row a result returned, or say what was left out;
  - read a cut value in pieces when the whole of it is needed.
