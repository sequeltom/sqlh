# Drain policy experiment (PLAN.md 2.6)

Measured 2026-10-11, on a working tree based on `f12b041`; the prototype is `f4a9c78` on branch `worktree-wf_3b570b22-04a-2`. The question: what does it cost to keep reading and counting rows after `--max-rows` (DESIGN.md §4.4), and what would a bounded drain look like? This is the evidence for owner decision 6 (PLAN.md §6.6). It does not make the decision.

## Summary

- **The drain costs the whole result, not the 100 rows.** `TOP (100)` makes these 0.2 to 0.7 s calls. With today's exact count they take 1 to 4 s for 0.3 to 10 million narrow rows, 21 s for a 1.3 GB table and 119 s for the 6.8 GB one (so the default `-t 60` ends that one with exit 4, after the rows were delivered), and 36 s for a text search that `TOP (100)` answers in 0.7 s.
- **The server and the link dominate; building rows on the client does not.** Over this Wi-Fi the link gives 57 to 65 MB/s. Counting without building rows cut the client's CPU by 22 to 41% and changed no wall time.
- **A bounded drain costs `TOP (100)` plus its budget.** Cutting at the first hidden row took 0.23 to 0.52 s for every query but the text search (0.97 s); a 250 ms budget took 0.5 to 1.15 s. The server acknowledged the attention in 3 to 420 ms, stopped scanning at once (checked on the text search), and the session kept its transaction and temp tables.
- **The price is meaning, not speed.** Statements after the cut result don't run. With `--allow-write`, a skipped `COMMIT` leaves a reported write to be rolled back at disconnect. `SET XACT_ABORT ON` makes the cut roll back the read-only wrapper. And the lower bound from a time budget differs from run to run.
- **Recommended** (the owner decides): a bounded drain by time for read-only runs, the exact drain kept for `--allow-write` and on request, a coarse floor for the reported bound, and a fix for the exit-4 case either way. See "Options for decision 6".

## Setup

- **Client:** the dev workstation (Windows 11 Pro; Core i7-8850H, 6 cores; 31 GB), on Wi-Fi 802.11ac (link rate 867 Mbit/s). Over this link the server delivered about 57 to 65 MB/s of result data (roughly 500 Mbit/s).
- **Server:** the owner's dev/test SQL Server 2025 (17.0.1000.7, Standard Developer) on Windows Server 2025 in a VM: 15 GB of RAM, 4 CPUs. About 8.4 GB of the StackOverflow2010 database was in the buffer pool during the runs, so scans read memory, not disk, apart from the first scan of `Votes` (9.3 s, against 3.6 to 4.3 s afterwards).
- **Connection:** SQL login through the `test` profile, encryption mandatory with `-C`, database `StackOverflow2010`, read-only (the default). Every query is a plain `SELECT`.
- **Builds:** the baseline is the release build of `f12b041` (2,285,568 bytes). The prototype build is the same tree plus the prototype commit, built optimized but with debug assertions on, because the switch exists only in debug builds (2.5 MB; see "Reproduce"). The timing tables below come from a build made before the `trace` setting was added to the same commit (2,534,400 bytes); the acknowledgement times, the output shapes and the caveat runs come from the final one (2,535,424 bytes). Run without the switch it matches the baseline: the fastest runs agree (`Votes` 3.50 s against 3.65 s, `Posts` integers 1.85 s against 1.91 s, `Users` 1.04 s against 1.03 s) and the medians differ by less than the spread of either.
- **Not a quiet machine.** Other agents built, tested and queried on the same workstation and server during the runs (another session's `SELECT`s showed up in `sys.dm_exec_requests` during several). Single runs vary by up to 3 times. So the tables give medians of interleaved rounds (every variant of one query runs in turn; three to seven rounds, fewer for the slow queries) with the minimum and maximum, and conclusions rest on differences far larger than that noise.

## Method

Part (a), the cost of the exact drain. The biggest tables came from `sys.dm_db_partition_stats` (summed over the indexes of each table):

| Table | Rows | Size | Columns |
|---|---|---|---|
| `Posts` | 3,729,195 | 6.8 GB (0.5 GB of it LOB) | 20, with `Body nvarchar(max)`, `Title nvarchar(250)`, `Tags nvarchar(150)` |
| `Comments` | 3,875,183 | 1.3 GB | 6, with `Text nvarchar(700)` |
| `Votes` | 10,143,364 | 364 MB | 6, all narrow |
| `Users` | 299,398 | 58 MB | 14, with `AboutMe nvarchar(max)` |
| `Badges` | 1,102,019 | 51 MB | 4 |

For each query, two forms: `SELECT <cols> FROM t` (sqlh's default: `--max-rows 100`, `-t 60`, text format; it counts the rest) and `SELECT TOP (100) <cols> FROM t` (what an agent that knew the table was big would write). The difference is what the drain costs. Each run is one process: the harness records the wall time from start to exit, sqlh's own `ms` from its status line, and the client's CPU time. Wall time includes about 0.2 s of connecting (PLAN.md 2.1) and 30 ms of start-up, in both forms.

To say what dominates, each query was also looked at from the server's side:
- `SET STATISTICS TIME ON` in the same batch gives the statement's server CPU and elapsed time.
- `sys.dm_exec_session_wait_stats` for the session, read by a trailing statement in the same batch (it runs only because the drain reaches the end), gives `ASYNC_NETWORK_IO`: the time the server waited for the client to take data.
- `sys.dm_exec_connections.num_writes` times the packet size (8,058 bytes) gives the megabytes sent.
- For queries too long to finish, a second connection sampled `sys.dm_exec_requests` and the same wait stats while the query streamed.
- A `discard` variant of the prototype (below) counts hidden rows with `next_row_into` and a discarding writer instead of `next_row()`, which separates the cost of building rows from the cost of receiving them.

## Results: what the exact drain costs

Medians over interleaved rounds, with the range in parentheses. "Exact drain" is the behavior today.

| Query | Rows | TOP (100) | Exact drain | Client CPU in the drain | Bound by |
|---|---|---|---|---|---|
| `SELECT * FROM dbo.Users` | 299,398 | 0.22 s (0.20 to 0.41) | 1.1 s (1.0 to 3.7) | 0.42 s | the link (52 MB sent) and the server |
| `SELECT * FROM dbo.Votes` | 10,143,364 | 0.22 s (0.20 to 0.27) | 3.8 s (3.7 to 4.3) | 3.0 s | server CPU 2.7 s; the client keeps pace |
| `SELECT Id, Score, ViewCount FROM dbo.Posts` | 3,729,195 | 0.21 s (0.20 to 0.28) | 2.1 s (1.9 to 2.2) | 1.2 s | server CPU 1.7 s |
| `SELECT * FROM dbo.Comments` | 3,875,183 | 0.26 s (0.25 to 0.28) | 20.6 s (20.0 to 21.8) | 3.9 s | the link: 65 MB/s |
| `SELECT Id, Body FROM dbo.Posts` | 3,729,195 | 0.45 s (0.24 to 0.60) | not done in 60 s: **exit 4** (3 of 3) | 5.9 s per 60 s | the link: 61 MB/s |
| `SELECT * FROM dbo.Posts` | 3,729,195 | 0.28 s (0.21 to 0.44) | not done in 60 s: **exit 4** (3 of 3); 118.6 s with `-t 0` | 7.2 s per 60 s | the link: 57 MB/s |
| `SELECT Id, Title FROM dbo.Posts WHERE Body LIKE '%jquery%'` | 115,893 matches | 0.73 s (0.64 to 0.82) | 35.7 s (32.7 to 41.2) | 0.2 s | server CPU: a scan of 6.8 GB of text |

Also measured, in a first non-interleaved pass: `SELECT Id FROM dbo.Comments` 0.25 s against 1.3 s (1.1 to 2.2); `SELECT * FROM dbo.Badges` 0.44 s against 1.6 s; `SELECT Id, Title, Tags FROM dbo.Posts` 0.25 s against 3.9 s. Numeric predicates on `Posts` (`Score >= 200`, `>= 1000`, `>= 2500`: 15,665, 1,508 and 291 matches) cost only 0.3 s more than `TOP (100)`: there is no index on `Score` (only the clustered key), but the server scans the 3.7 million rows from memory in about a third of a second.

Server side, for the queries that finished (statement CPU and elapsed time from `SET STATISTICS TIME`, `ASYNC_NETWORK_IO` from the session's wait stats):

| Query | Server CPU | Server elapsed | Waiting on the client | Sent |
|---|---|---|---|---|
| `Users *` | 0.33 s | 0.62 s | 0.20 s | 52 MB |
| `Votes *` | 2.67 s | 3.52 s | 0.49 s | 217 MB |
| `Comments Id` | 0.81 s | 0.85 s | none | 18 MB |
| `Posts Id, Score, ViewCount` | 1.67 s | 1.76 s | none | 46 MB |
| `Posts Id, Title, Tags` | 2.03 s | 2.84 s | 0.23 s | 189 MB |

For the two that never finish, sampled while they streamed: `Comments *` was suspended on `ASYNC_NETWORK_IO` for 11.1 s of its first 15.1 s (74%) with 3.3 s of server CPU, and had sent 949 MB; `Posts Id, Body` for 13.6 s of its first 18.2 s (75%) with 4.4 s of CPU, 1,098 MB sent (61 MB/s).

### Findings

1. **The drain costs the whole result, not the 100 rows.** A careless `SELECT *` from a big table is a 0.2 s call that becomes 1 to 4 s for 0.3 to 10 million narrow rows, 21 s for a 1.3 GB table, and 2 minutes for the 6.8 GB one. For comparison, agent trial 1's eleven calls took 0.2 to 0.27 s each ([agent-trial-1.md](agent-trial-1.md)). The cost has nothing to do with `--max-rows`: it is paid for every row the query returns.
2. **The server and the link dominate; decoding does not.**
   - Narrow rows (`Votes`, `Comments Id`, three `Posts` integers): the server needs 0.2 to 0.5 microseconds of CPU per row to put rows on the wire (2.7 s for 10.1 million), and the client, at 80% of a core, keeps pace.
   - Wide rows (`Comments *`, `Posts` with `Body`): the link is the limit, at 57 to 65 MB/s here. The server waits on `ASYNC_NETWORK_IO` three quarters of the time and the client is 80 to 90% idle.
   - A scan for rare text matches (`LIKE '%jquery%'`): 34 s of server CPU for 115,893 rows and almost no bytes. Neither the link nor the client matters.
   - Counting without building the rows (`discard`) cut the client's CPU by a quarter to two fifths (`Votes` 3.0 to 2.25 s, `Comments *` 3.9 to 2.75 s, `Posts` integers 1.2 to 0.94 s, `Users` 0.42 to 0.25 s) and **changed no wall time** (`Votes` 4.1 s against 3.8 s, `Comments *` 21.4 s against 20.6 s, `Posts` integers 2.0 s against 2.1 s, `LIKE` 36.6 s against 35.7 s). So making the count cheaper on the client, which PLAN.md 2.7 would do for memory's sake, cannot make an exact drain fast. Only not reading the rows can.
3. **A drain that outlives `-t` is reported as a failure, after the rows were delivered.** The default `-t 60` ends `Posts *` and `Posts Id, Body` with exit 4. What the agent receives is its 100 rows, no footer, and the error (`--max-rows 3`, `-t 5` here):

   ```
   -- result 1
   Id:int	Body:nvarchar(max)
   4	<p>I want to use a track-bar to change a form's opacity.</p>…[+447]
   6	<p>I have an absolutely positioned <code>div</code> containi…[+694]
   7	<p>An explicit cast to double like this isn't necessary:</p>…[+282]
   -- error: query timeout after 5 s in batch 1; raise -t or narrow the query
   -- failed (exit 4): timeout, 5607 ms
   ```

   The rows arrived in the first fraction of a second, so the failure comes from counting the rest. `SELECT * FROM dbo.Posts` with `-t 0` ran to the end only after 118.6 s (client CPU 12.6 s; 57 MB/s), and ended with `-- 100 of 3729195 rows shown (--max-rows 100); 47 values cut (--max-cell 500)`.
4. **How expensive the scan is depends on the predicate.** A numeric filter costs 0.3 s more than `TOP (100)`; a text pattern (`Body LIKE '%jquery%'`, or anything the server can't serve from the key) makes the server read all 6.8 GB after the first 100 matches, which `TOP (100)` would not have done: 0.73 s against 35.7 s. This is the case where the exact count is expensive for no transfer at all, and where only cancelling the statement saves the server's work.
5. **An exact total can be had on demand, cheaper, when the cost is the transfer.** `SELECT COUNT_BIG(*)` over a whole table took 0.37 s (`Users`), 0.55 to 0.63 s (`Comments`), 0.70 s (`Votes`) and 0.76 s (`Posts`), wall time including the connect, because the server counts without sending anything. For the wide tables that is about 35 to 150 times cheaper than the drain. It does not help when the cost is the scan (the `LIKE` case: the count needs the same 34 s).

## The bounded-drain prototype

The prototype is the commit `f4a9c78` ("Prototype a bounded drain behind SQLH_DEBUG_DRAIN") on this branch. It stays there until the owner decides (PLAN.md §6.6). It changes no default and no release build: the switch is read only when `debug_assertions` are on.

### The switch

`SQLH_DEBUG_DRAIN`, read in `Session::connect` under `cfg(debug_assertions)`, like `SQLH_DEBUG_PANIC`. Comma-separated settings:

| Setting | Meaning |
|---|---|
| `rows=N` | Stop once N rows beyond the shown ones have been counted. `rows=0` cuts at the first hidden row, so the footer says `≥101` for `--max-rows 100`: there are more rows than shown. |
| `ms=N` | Stop N milliseconds after the first hidden row. |
| `discard` | Count hidden rows without building them. The count stays exact and nothing is cut: a measurement aid. |
| `trace` | Write `sqlh: drain cut: N rows counted, attention acknowledged M ms after the budget ran out` to stderr when a count is cut. `N` counts hidden rows only. |

When both budgets are given the first to run out wins. A value that isn't understood fails the run before it connects (`-- error: SQLH_DEBUG_DRAIN "rows=x": expected a whole number`, exit 2), so a typo can't pass for the default in a measurement.

### How it works

- **Hidden rows** are the rows `BatchSink::wants_values()` refuses: those past `--max-rows`, those of a result hidden by `--max-results`, and those after `--max-output` is spent. All three are counted today, and all three are subject to the budget.
- **One cancel mechanism for both budgets.** `Session::run` hands the driver's `execute` a handle that is a child of a child of the run's cancel handle (`Drain` in `db/mod.rs`). Cancelling the middle one cancels this statement and nothing else, while the run's own cancel (Ctrl+C) still reaches the statement through the chain. The row budget cancels it when the Nth hidden row has been counted. The time budget is a task on the runtime that cancels it after `ms`, started with the first hidden row of a result and stopped when the result ends, so it can't touch the statements that follow.
- **The cancel is the one Ctrl+C already uses.** The driver's read loop sees the token (`await_read_or_interrupt` in `network_transport.rs`; the buffered-row fast path checks it too, so the row budget is exact: shown plus N), `read_to_attention_boundary` sends the TDS attention, and `drain_to_attention_ack` reads and discards what is in flight (rows are skipped with `ColumnPolicy::SkipAll`, never built) up to the DONE token that carries the ATTN flag, within `ATTENTION_TIMEOUT_SECONDS` (5 s). It then settles its state: `consume_attention_settlement` applies the ENVCHANGE tokens it read, which carry the transaction state, and `normalize_after_attention` closes the batch and clears the timeout and the cancel handle. `next_row()` returns `OperationCancelledError`.
- **sqlh recognizes the cancel as its own** (the budget fired, the run wasn't cancelled, the connection isn't marked dead), calls `BatchSink::counting_stopped()`, closes the result with `end_rows()` and returns true. `run` then stops walking the batch: nothing after that result runs.
- **A timer, not a clock check between rows,** because a check between rows can't fire while the server is looking for the next row. A live test pins this: the first branch of a `UNION ALL` returns 20,000 rows at once, the second finds nothing for 6.5 s (27 million combinations through a filter). With `ms=150` the run ended after 0.7 s, with `≥19177` rows (the other 823 were probably in a packet the server had not yet sent). A check between rows would have waited the 6.5 s.
- If the driver gets no acknowledgement within 5 s it marks the connection dead, and the run ends as it does for Ctrl+C in that case: exit 6, `the server didn't confirm the cancel, so the connection was closed`. That never happened in any run here.
- **Output layer** (`output/*`): the three sinks forward `counting_stopped` to `Tally::stop_counting`. The footer says the total is a lower bound, one note at the end of the run names the batches (up to five, then a count), and JSON adds `row_count_exact:false` to the result. The status line, the exit code and `--help` are untouched (the prototype may not touch `main.rs`), so a run with a cut still ends `-- ok: ...`; see the options.

### Output shapes

Exact output from the prototype build, for a two-batch input with `--max-rows 3`:

```sql
CREATE TABLE #t (i int);
INSERT #t VALUES (1),(2);
SELECT TOP (50000) a.object_id AS id, a.name FROM sys.all_objects AS a CROSS JOIN sys.all_objects AS b;
PRINT 'after the big result';
GO
SELECT 1 AS next_batch
```

**text**, today (the exact drain):

```
-- batch 1 (line 1)
-- 2 rows affected
-- result 1
id:int	name:nvarchar(128)
-103	all_columns
-103	all_columns
-103	all_columns
-- 3 of 50000 rows shown (--max-rows 3)
-- msg: after the big result
-- batch 2 (line 6)
-- result 2
next_batch:int
1
-- 1 row
-- ok: 2 batches, 351 ms
```

**text**, with `SQLH_DEBUG_DRAIN=rows=1000`:

```
-- batch 1 (line 1)
-- 2 rows affected
-- result 1
id:int	name:nvarchar(128)
-103	all_columns
-103	all_columns
-103	all_columns
-- 3 of ≥1003 rows shown (--max-rows 3; counting stopped)
-- batch 2 (line 6)
-- result 2
next_batch:int
1
-- 1 row
-- note: counting stopped early in batch 1 (at 1003 rows): the rest of the batch was cancelled, so any later statements in it did not run
-- ok: 2 batches, 235 ms
```

The `PRINT 'after the big result'` did not run, batch 2 did, and the status line is `ok` in both.

**json**, the cut run (today's document differs only in `row_count` being 50000, having no `row_count_exact`, and a `msg` event for the `PRINT` after the result):

```json
{"batches":[{"batch":1,"source":"-Q","line":1,"events":[{"type":"rows_affected","count":2},{"type":"result","columns":[{"name":"id","type":"int","nullable":false},{"name":"name","type":"nvarchar(128)","nullable":false}],"rows":[[-103,"all_columns"],[-103,"all_columns"],[-103,"all_columns"]],"row_count":1003,"row_count_exact":false,"truncated":true,"cells_cut":0}]},{"batch":2,"source":"-Q","line":6,"events":[{"type":"result","columns":[{"name":"next_batch","type":"int","nullable":false}],"rows":[[1]],"row_count":1,"truncated":false,"cells_cut":0}]}],"notes":["counting stopped early in batch 1 (at 1003 rows): the rest of the batch was cancelled, so any later statements in it did not run"],"ok":true,"exit_code":0,"summary":"ok: 2 batches, 287 ms","elapsed_ms":287}
```

**csv**, the cut run (stdout, then stderr; today's differs in the same places as text):

```
id,name
-103,all_columns
-103,all_columns
-103,all_columns

next_batch
1
```

```
-- batch 1 (line 1)
-- 2 rows affected
-- result 1: id:int, name:nvarchar(128)
-- 3 of ≥1003 rows shown (--max-rows 3; counting stopped)
-- batch 2 (line 6)
-- result 2: next_batch:int
-- 1 row
-- note: counting stopped early in batch 1 (at 1003 rows): the rest of the batch was cancelled, so any later statements in it did not run
-- ok: 2 batches, 238 ms
```

Rules these shapes follow:
- The footer is `N of ≥M rows shown (<limit>; counting stopped)`: `≥` before the total, and `; counting stopped` inside the parentheses, which still name the limit that hid the rows (`--max-rows 3`, or `--max-output`). `; 8 values cut (--max-cell 500)` and the like follow as before.
- The count is exact whenever the result ends inside the budget; then nothing differs from today.
- JSON gets `row_count_exact:false` on the cut result only, so today's documents are unchanged. `row_count` is the shown rows plus the hidden rows counted, a lower bound.
- The note is one for the whole run and comes with the closing notes, before the status line. It lists up to five batches (`counting stopped early in batches 2, 3, 4, 5, 6 (+3 more): the rest of each was cancelled, so any later statements in them did not run`), then counts.
- Not in the prototype: a change to the status line or exit code, a per-batch machine-readable flag (the JSON has only the free-text note for "later statements did not run"), and tests of the `--max-output` reserve with a cut (the widest ending grows by about 24 bytes in JSON and 21 in text and csv; the widest ending is about 420 bytes of the 512 that `STATUS_RESERVE` holds, so it should fit, but the tests that pin it don't include a cut).

### Measured

Wall seconds, median of interleaved rounds (range). The first two columns are part (a)'s.

| Query | TOP (100) | Exact | `rows=0` | `rows=10000` | `ms=250` | `ms=1000` |
|---|---|---|---|---|---|---|
| `Users *` | 0.22 (0.20 to 0.41) | 1.10 (1.0 to 3.7) | 0.52 (0.22 to 0.64) | 0.90 (0.46 to 1.8) | 0.94 (0.59 to 1.8) | |
| `Votes *` | 0.22 (0.20 to 0.27) | 3.76 (3.65 to 4.3) | 0.25 (0.21 to 0.53) | 0.25 (0.23 to 0.73) | 0.63 (0.48 to 0.81) | |
| `Posts Id, Score, ViewCount` | 0.21 (0.20 to 0.28) | 2.14 (1.9 to 2.2) | 0.23 (0.20 to 0.38) | 0.22 (0.21 to 0.29) | 0.51 (0.45 to 1.7) | |
| `Comments *` | 0.26 (0.25 to 0.28) | 20.6 (20.0 to 21.8) | 0.33 (0.26 to 0.37) | 0.49 (0.45 to 0.56) | 0.83 (0.76 to 1.22) | 1.50 (1.42 to 1.54) |
| `Posts Id, Body` | 0.45 (0.24 to 0.60) | not done in 60 s | 0.44 (0.25 to 0.53) | 0.83 (0.72 to 0.99) | 1.01 (0.85 to 1.14) | 1.76 (1.52 to 1.86) |
| `Posts ... LIKE '%jquery%'` | 0.73 (0.64 to 0.82) | 35.7 (32.7 to 41.2) | 0.97 (0.77 to 1.27) | 5.4 (5.0 to 6.7) | 1.15 (1.04 to 1.20) | 1.83 (1.77 to 2.13) |

What the cut leaves in the footer with `ms=250` (range over the runs; the same budget counts different numbers because the link and the server are not steady):

| Query | Counted by `ms=250` |
|---|---|
| `Users *` | ≥2,722 to ≥57,678 |
| `Votes *` | ≥556,885 to ≥1,071,809 |
| `Posts Id, Score, ViewCount` | ≥457,383 to ≥951,043 |
| `Comments *` | ≥38,926 to ≥53,731 (≥133,601 to ≥228,717 with `ms=1000`) |
| `Posts Id, Body` | ≥10,861 to ≥12,166 (≥19,231 to ≥53,769 with `ms=1000`) |
| `Posts ... LIKE` | ≥548 to ≥570 (≥1,514 to ≥2,102 with `ms=1000`) |

How long the attention took to be acknowledged (`trace`; milliseconds, median with the range over eight runs, measured from the moment the budget ran out until `next_row()` reported the cancel):

| Query | `rows=0` | `rows=10000` | `ms=250` |
|---|---|---|---|
| `Users *` | 17 (12 to 26) | 127 (57 to 251) | 249 (81 to 344) |
| `Votes *` | 11 (7 to 21) | 17 (10 to 26) | 68 (23 to 129) |
| `Posts Id, Score, ViewCount` | 6 (3 to 11) | 7 (4 to 10) | 8 (5 to 12) |
| `Comments *` | 24 (17 to 33) | 130 (99 to 348) | 254 (195 to 349) |
| `Posts Id, Body` | 31 (15 to 57) | 261 (219 to 416) | 300 (189 to 395) |
| `Posts ... LIKE` | 28 (13 to 43) | 18 (9 to 46) | 35 (30 to 50) |

Findings:
1. **A bounded drain is `TOP (100)` plus its budget.** `rows=0` stayed within 10 to 70 ms of `TOP (100)` on `Votes`, `Posts` integers, `Comments *` and `Posts Id, Body`; on `Users` its median was 0.3 s above (the first four rounds ran slow, and its fastest run, 0.22 s, equals `TOP (100)`'s 0.20 s); on the text search it was 0.24 s above, the time to tell the server to stop scanning. `ms=250` took 0.5 to 1.15 s whatever the table: the budget, the cancel, and a connect. Exact took 1.1 to 119 s.
2. **A row budget doesn't bound time; a time budget does.** `rows=10000` is as cheap as `rows=0` for narrow rows and 0.15 to 0.4 s more for wide rows (it has to receive 2 to 20 MB first), but on the text search it took 4.4 s more, because finding 10,000 matches is the cost. `ms=250` held 1.15 s there.
3. **The cancel takes as long as the data in flight.** The server acknowledged within 3 to 57 ms right after the first hidden row, and up to 250 to 420 ms when the link was the limit and the server had run ahead by about one TCP window (at 60 MB/s, 250 ms is about 15 MB). It never took more than 420 ms in 144 cut runs, and always acknowledged. On the text search the server stopped scanning within 50 ms.
4. **The server's work stops.** After a `ms=250` cut of the 34-second text scan, `sys.dm_exec_requests` showed no request of ours, right away or 2 s later.
5. **Counts from a time budget aren't reproducible,** by a factor of 2 for `Votes` and up to 20 for `Users` (when the link was loaded), though the cut itself is on time. A row budget is exact (shown plus N).
6. **Exactness is kept for small results:** a 500-row result under `rows=1000`, `ms=250` and both printed `3 of 500 rows shown (--max-rows 3)`, and the `PRINT` after it ran, as today.

## Caveats of cutting

Observed on the prototype build (text output; "exact" means no switch) except where marked as reasoning.

- **Later statements in the batch don't run,** and nothing but the note says so. A batch `CREATE TABLE #t ...; INSERT #t (3 rows); PRINT 'before'; <big SELECT>; INSERT #t VALUES (4); PRINT 'after'` prints `-- 3 rows affected`, `-- msg: before` and the cut result; the second `INSERT` and `PRINT 'after'` never ran (`SELECT COUNT(*) FROM #t` in the next batch says 3). The same holds for a second result set. Later `GO` batches are separate requests and run on the same connection.
- **The read-only wrapper transaction survives,** and so do temp tables: the next batch saw `@@TRANCOUNT` = 1 and its `#t`. XACT_ABORT stays OFF in sqlh's session setup.
- **`SET XACT_ABORT ON` in the input changes that.** The server rolls the transaction back on the attention, the driver's settlement says so, and sqlh's own invariant stops the run: `-- error: read-only guard: an error rolled back the wrapper transaction; stopping so nothing runs outside it`, then the note and `-- failed (exit 1): SQL error`. The same input with the exact drain finishes with exit 0. So a script that sets it gets a different outcome when it is cut, in a read-only run too.
- **With `--allow-write`, a skipped statement can be a write or a `COMMIT`.** `BEGIN TRAN; INSERT <probe table> VALUES (1); <big SELECT>; COMMIT;` then `GO` and a second batch: cut, it reports `-- 1 row affected`, `-- ok`, exit 0, and the next batch finds `@@TRANCOUNT` = 1 with the row visible to its own session. After the run a new session sees 0 rows, because the open transaction was rolled back when sqlh closed the connection. With the exact drain the `COMMIT` ran and the row was there. Nothing but the note says so, and (reasoning, not run) a later batch's writes would join the open transaction and be lost the same way. This is the strongest argument for keeping the exact drain wherever writes are allowed.
- **Row counts.** The statements before the cut report as usual (`-- 3 rows affected`). The cut result's count is a lower bound in the footer and in `row_count`. The cut statement's own `DONE` row count never arrives (by the nature of an attention), and neither does anything the later statements would have reported. Rows the server had not yet sent are not counted: `≥19177` of 20,000 in the slow-tail test.
- **Output limits.** `--max-rows 0` shows every row, so there is nothing hidden to count and the budget never applies (a 2,000-row result printed `-- 2000 rows` and the statement after it ran). But rows the `--max-output` budget has no room for are hidden too, so the drain budget applies to them: `--max-rows 0 --max-output 4096` on 200,000 rows printed `-- 610 of ≥1611 rows shown (--max-output; counting stopped)` with both notes. A result hidden by `--max-results` is counted and cut the same way, with no footer (reasoning from the code, not run).
- **Driver.** After the attention the connection was reusable in every run (the next batch always ran), and `has_active_transaction` followed the server's state (the XACT_ABORT case above). That is evidence for the open question in [mcp-mode.md](../mcp-mode.md) §10, "Cancellation and reuse", for a cut the driver saw acknowledged: the session, its transaction and its temp tables were usable at once. It proves nothing for other states (an unacknowledged attention marks the connection dead). The driver bounds the wait for the acknowledgement at 5 s and marks the connection dead if it doesn't come (exit 6 here). A statement the server can't interrupt promptly, or a link that is down, would take that path; PLAN.md 2.5 is to measure harness kills for CPU-bound and lock-blocked queries, and the same cases apply to a cut.
- **Not covered by the prototype:** the status line and exit code of a run that has a cut, `--help`, the DESIGN.md contract, and `main.rs`. With the prototype a cut run ends `-- ok: 2 batches, 235 ms` and exits 0, so a reader of the status line alone doesn't learn that anything was cancelled.

## Options for decision 6

PLAN.md §6.6: "Exact counts as today, or a bounded drain with `≥N`?" The numbers allow more than two answers.

| Option | Cost of a careless `SELECT *` (measured) | Count | Later statements | What it takes |
|---|---|---|---|---|
| **A. Exact, as today** | 1.1 to 4 s, 21 s, 119 s (exit 4 at `-t 60`), 36 s for the text scan | exact | run | nothing |
| **B. Bounded by time** (`ms`, such as 250) | 0.5 to 1.15 s | exact when it finishes in time, else `≥N` | cancelled on a cut | the prototype's mechanism plus status, exit, JSON, docs |
| **C. Cut at the first hidden row** (`rows=0`) | 0.23 to 0.52 s (0.97 s on the scan) | exact up to `--max-rows`, else `≥101` | cancelled on a cut | the same, less: no timer, deterministic output |
| **D. Exact on request** | A's cost, when asked for | exact | run | a flag, with B or C as the default |
| **E. Cut only where nothing is lost** | as B or C where it applies, A elsewhere | as B or C | cancelled only in read-only runs, or single-statement batches | B or C plus a rule for when |

What each is worth:
- **A** is the simplest contract, and the exact total sometimes matters (115,893 posts mention `jquery`: that tells an agent to narrow the query). But its cost is the size of the result, and past `-t` it turns delivered rows into an exit-4 failure. It also holds a pooled connection busy for the whole drain, which Phase 5 (connection reuse) would pay for on every call.
- **B** keeps the exact total wherever the result is small (anything that ends inside the budget), and says "at least" with a magnitude otherwise: `≥852,359` after 250 ms on a 10-million-row table. Its output isn't reproducible, which hurts golden tests and an agent comparing two runs. Flooring the reported bound to one significant digit (`≥800000`) would make runs agree and keep the magnitude; the prototype doesn't do that.
- **C** says only that more rows exist. That is the one bit an agent needs to decide to narrow the query, at the cost of a `TOP (100)`, with output that is the same every time. It loses the magnitude.
- **D** is the way back to an exact total, which the agent can also get for 0.4 to 0.8 s with `SELECT COUNT_BIG(*)` when the cost is the transfer.
- **E** removes the worst of the caveats. The cut loses a write or a `COMMIT` only where writes are allowed (`--allow-write`, or a profile whose policy is `allow`), and only a later statement can be lost, so keeping the exact drain for writes and for multi-statement batches makes the cut safe by construction. Telling one statement from several is a job for the guard's tokenizer and must fall back to the exact drain when unsure. The one read-only case that still changes an outcome is `SET XACT_ABORT ON`; the guard can see the word, as it sees `NOEXEC`, and fall back to the exact drain.

**Recommendation** (the owner decides):
1. **Make the default B for read-only runs, with C as its zero-budget limit,** starting at about 250 ms, plus the floored bound. Keep the exact drain behind a flag (D) and for `--allow-write` and `XACT_ABORT` input (E). A large result then costs 0.5 to 1.15 s instead of 1.1 to 119 s, a result that ends inside the budget is exact as before, and in a read-only run nothing a cut skips could have outlived the run, because the wrapper transaction is rolled back at the end (later batches of the same run can still depend on a skipped temp-table write, which the note covers).
2. **Whatever is decided about counting, don't let the drain turn delivered rows into exit 4.** If the shown rows are complete when `-t` expires during the count, end the count (the same attention) and report `≥N` with exit 0, or at least say in the error that the rows above are complete. Today the agent has to guess.
3. **Decide the machine-readable form together with the shapes:** the status line should say a batch was cut (`ok: 2 batches, counting stopped in 1`, say) and JSON should carry a per-batch flag, not only a note.
4. **Do not spend PLAN.md 2.7 on the drain.** It is still worth doing for memory (a huge cell), but an exact drain stays link- and server-bound.

What the real change needs, if B or C is chosen: the flag or default for the budget; the status-line and exit-code wording; DESIGN.md §4.4 (the "Rows" bullet and "Limits never change what runs"), §5.1 to §5.3 (the footer and `row_count_exact`) and §6; the `--help` FORMAT paragraph (the help is at its 62-line limit); the README "Built for agents" entry; exact-output tests for the cut footer and note in the three formats and for the longer endings in the `--max-output` reserve; live tests for the interaction cases above (`XACT_ABORT`, `--allow-write`, `GO n`, `--max-output`); and a check of Ctrl+C during the cancel, which takes the existing path but wasn't tested here.

## Not measured

- A wired link, a faster or slower network, another server, a cold buffer pool, Windows authentication, strict encryption.
- A server under load, or several clients at once. The measurements share the dev server with other agents' sessions.
- Cutting inside a procedure, a trigger or a cursor; a batch with `SET ROWCOUNT`; `GO 1000` loops where every batch is cut.
- The 5 s unacknowledged-attention path.
- Ctrl+C during the cancel of a cut.
- Row sizes outside 21 bytes (`Votes`) to 1.8 KB (`Posts`) per row. In between, the cost follows the bytes sent.

## Reproduce

Build the baseline and the prototype (the second needs debug assertions for the switch to exist):

```powershell
cargo build --release                                                  # baseline: target\release\sqlh.exe
$env:CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS = 'true'
cargo build --release --target-dir C:\scratch\target-drain             # prototype with the switch
$env:SQLH_DEBUG_DRAIN = 'ms=250,trace'
C:\scratch\target-drain\release\sqlh.exe --profile test -Q "SELECT * FROM dbo.Comments"
```

The harness, one process per run, with the wall time and the client CPU:

```powershell
function Invoke-Sqlh($Exe, $Sql, $Env = @{}, $ExtraArgs = @()) {
    $psi = [Diagnostics.ProcessStartInfo]::new($Exe)
    $psi.UseShellExecute = $false; $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
    foreach ($a in @('--profile', 'test') + $ExtraArgs + @('-Q', $Sql)) { $psi.ArgumentList.Add($a) }
    foreach ($k in $Env.Keys) { $psi.Environment[$k] = $Env[$k] }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = [Diagnostics.Process]::Start($psi)
    $err = $p.StandardError.ReadToEndAsync(); $out = $p.StandardOutput.ReadToEnd(); $p.WaitForExit()
    [pscustomobject]@{ WallMs = $sw.Elapsed.TotalMilliseconds; CpuMs = $p.TotalProcessorTime.TotalMilliseconds
                       Exit = $p.ExitCode; Out = $out; Err = $err.Result }
}
# Interleave the variants of one query, several rounds, and take medians:
#   Invoke-Sqlh $base  'SELECT TOP (100) * FROM dbo.Votes'
#   Invoke-Sqlh $base  'SELECT * FROM dbo.Votes'
#   Invoke-Sqlh $proto 'SELECT * FROM dbo.Votes' @{ SQLH_DEBUG_DRAIN = 'rows=0' }
#   Invoke-Sqlh $proto 'SELECT * FROM dbo.Votes' @{ SQLH_DEBUG_DRAIN = 'ms=250,trace' }
```

The tests that came with the prototype: unit tests for the switch grammar, the budget and the timer (`db/mod.rs`), exact-output tests for the footer and note in text, json and csv and across batches (`output/*`), a process-level test that a bad switch fails before connecting, and three live tests (`SQLH_TEST_PROFILE=test cargo test --test cli -- --ignored live`, debug builds only): a row budget gives a lower bound, cancels the batch and leaves the session usable; the same in json; a time budget ends the count while the server is still working.

## What was built (PLAN.md 2.15, owner decision 6)

Built 2026-10-11 on the 2.7 read path, not merged from the prototype. The contract is DESIGN.md §4.4, §5.1 to §5.3 and §6; this section says how it differs from the prototype and what was learned doing it.

**Differences from the prototype**
- **Not a debug switch.** The budget is a fixed 250 ms in every build (`db::COUNT_BUDGET`), chosen per run by `Counting` (`BOUNDED`, `EXACT`) from `main`: exact for `--allow-write` (a profile whose `write_policy` is `allow` too), for input whose tokens include `XACT_ABORT` (`guard::mentions_xact_abort`, found with the tokenizer like `NOEXEC`), and for `--exact-count` or a profile's `exact_count`. There is no row budget and no `discard`/`trace` mode.
- **The read is the 2.7 one.** Hidden rows are skipped by `next_row_into` with the driver's `DiscardRowWriter` in result sets without a PLP column, and by the cursor (`try_next_row_cursor` and the next row's column skip) in those with one. `drain_rows` is not used: it allocates every PLP value whole.
- **The clock** starts at the first hidden row of a result. A timer task fires the cancel while the server is still looking for the next row, and the check on each hidden row fires it on time when rows keep coming (a timer cannot run while the read never yields). Until a hidden row arrives nothing runs the 250 ms: a server that is slow to produce the first one is waited for, up to `-t`. Measured on a query that sent 7 rows and then stalled: `--max-rows 7 -t 4` took 4.2 s and read `7 of ≥7 rows shown`, while `--max-rows 6` stopped in 0.43 s. That follows decision 6 (hidden rows are counted for about 250 ms) and was kept; DESIGN.md §4.4 says it: `-t` bounds a stall before the first hidden row.
- **The cancel** is the prototype's: a child of a child of the run's handle, so the user's Ctrl+C still reaches the statement. If the budget fires as a result's last row arrives, the cancel meets the next step instead of a read; `run` treats that as a stopped batch too (the count was complete, but the rest of the batch is gone, and the output says so). If the server does not acknowledge sqlh's own attention within the driver's 5 s, the driver marks the connection dead and the run ends with exit 6 and a message of its own, `stopped counting the rows that are not shown, but the server didn't confirm the cancel, so the connection was closed...`, so that nobody mistakes it for a cancel from outside. That path was never reached in any run (as in the prototype's), so only the classification is tested, by a unit test.
- **`-t`** during the count: the driver reports a timeout (settled exactly like a cancel: `settle_interrupted_read`), and when only rows that are not shown were being read, in a run that loses nothing by a stop, the count stops as a budget stop would. Otherwise it is exit 4 as before. A stop loses something with writes allowed (it skips a later `COMMIT`) and with `XACT_ABORT` in the input (the attention rolls the read-only wrapper back, which ended a hand-run reproduction with exit 1 and no mention of the timeout when a batch followed, and with exit 0 and the wrapper gone when the stalling batch was the last). Decision 6 words the `-t` rule without these two exceptions; they are a reading of it that the owner has yet to confirm. **Learned (and fixed by R15, see "R14 and R15" at the end):** the driver's `-t` is the time spent waiting for the server, summed over reads (`update_remaining_timeout`), not wall time, and its buffered-row path doesn't count at all. A client that reads slower than the server sends (a debug build on narrow rows, a fast link) never uses its `-t`: `--allow-write -t 1` on 10 million narrow rows ran 7.4 s to the end. A wide-row result (a PLP column) did time out. The test for this case makes the server stall (20,000 rows at once, then a search for 15 billion combinations); that stall tests the wait path, and the fast-result tests at the end test the rest.
- **Output.** The footer is `N of ≥M rows shown (<limit>; counting stopped)` with M floored to one significant digit. Flooring would flatten a small count to "nothing hidden" (154 rows counted with 100 shown floor to 100), so once a hidden row was counted M is at least N + 1, and JSON's `truncated` reads the raw count. When `-t` ended the wait for the first hidden row M is N. The note and the status line (`counting stopped in N`, also in every failed run) say that the rest of the batch was cancelled, and the note's advice follows what stopped the count (`--exact-count` for the budget, `raise -t` for `-t`). JSON has `row_count_exact` on every result (false also for a result a failure cut short) and `counting_stopped` on every batch (the prototype had the first only when false), and its contract version went from 1.0 to 1.1 for them. The prototype's per-statement `at N rows` in the note is gone: the floored bound is in the footer.
- **Reserve.** The longer endings took JSON's widest ending to 561 bytes (text and csv: 399), so `STATUS_RESERVE` went from 512 to 640 (`OUTPUT_RESERVE` stays 1,024).
- **Not seen.** `XACT_ABORT` is found among the input's tokens. A server whose `user options` setting turns it on for every session was not seen, nor is dynamic SQL in a string (a read-only run refuses dynamic SQL; a run that can write counts exactly); there a stopped count rolled the wrapper back and the run ended with exit 1 and sqlh's own message about the wrapper. **Closed by R14:** the session setup now says `SET XACT_ABORT OFF` (see the end).

**Checked live** (`tests/cli.rs`, module `live`, StackOverflow2010 read-only): a 5-million-row result is cut in about a second (debug build; the footer read a few hundred thousand rows counted, a bound that changes from run to run, as the notes above say); a 5,000-row result is exact and the `PRINT` after it runs; `--exact-count` and `XACT_ABORT` input count 3 million rows exactly; `--allow-write` counts exactly and its `COMMIT` runs (the row is there for a new session); `GO 3` stops in each repeat; `--max-rows 0 --max-output 4096` cuts and stays within 4096 bytes; `-t 1` while the server stalls gives `≥N` and exit 0 (note: `raise -t`), exit 4 under `--allow-write`, and exit 4 with `--max-rows 0` or `XACT_ABORT` input (no stopped count); a stop after a few hidden rows (154 rows of 400 characters, then a stall, with `--max-rows 100`) reads `100 of ≥101` and is `truncated`, where flooring alone gave `≥100` and not truncated; Ctrl+Break during the cancel gives exit 6 (a debug-only hold, `SQLH_DEBUG_STOP_HOLD_MS`, makes that gap certain: the break lands in the middle of the hold and the hold ends inside the 2 s the break gives, so the run ends through the real decision of whose cancel it was, and the test fails if that decision ignores the user; a release build has no hold and no test). After a stop the next batch runs on the same session in the wrapper transaction (`@@TRANCOUNT` = 1).

## R14 and R15: `XACT_ABORT` OFF, and sqlh's own `-t` (the Phase 2 completion review)

Two gaps the completion review found in the 2.15 work, both closed 2026-10-11. The contract is DESIGN.md §4.3, §4.4, §6 and §7.

**R14, `XACT_ABORT`.** The bounded count looked for the word in the input, but a new session inherits the server's `user options` default, so a server that turns `XACT_ABORT` on for every session gave sqlh an ON session without the word anywhere. The attention that stops a count then rolled the wrapper transaction back: the next batch failed the wrapper check, and the last batch ended in a success with the wrapper gone. The session setup now sends `SET XACT_ABORT OFF` in the first batch, ahead of `BEGIN TRANSACTION` (no extra round trip, read-only and `--allow-write` alike), and after the last batch the run checks that the wrapper is still there (exit 1 if it is gone and no batch failed). Input that mentions `XACT_ABORT` is counted exactly, as before.
- **Test seam.** Debug builds send `SQLH_DEBUG_SESSION_PREFIX` (a SQL string) ahead of the setup, in the same batch, so a live test starts a session with `XACT_ABORT` ON with no word in the user's SQL and no change to the server. Tests: `the_session_setup_turns_xact_abort_off` (`@@OPTIONS & 16384` is 0 after the setup, in both kinds of run), `a_stopped_count_keeps_the_wrapper_in_a_session_that_started_with_xact_abort_on` (a bounded stop, then a batch that sees `@@TRANCOUNT` = 1; the stopped count as the last batch; JSON), `xact_abort_in_the_input_still_counts_exactly_in_such_a_session`, and the setup text pinned exactly by a unit test.
- **They fail without the fix.** With `SET XACT_ABORT OFF` taken out of the setup, the first two fail: the setup test reads 1, and the stop followed by a `GO` batch ends with exit 1 and `read-only guard: an error rolled back the wrapper transaction; stopping so nothing runs outside it`. The last-batch run, which before this change would have ended with exit 0 and the wrapper gone, now ends with exit 1 and `read-only guard: the wrapper transaction was gone after the last batch ...`.
- **The wrapper check after the last batch** is `wrapper_lost_unreported` in `src/run.rs` (`src/main.rs` before PLAN.md 3.1), a function of three booleans with a unit test (`a_wrapper_lost_without_a_failed_batch_is_a_failure`); the debug seam cannot reach it now that the setup turns the option OFF.

**R15, `-t`.** The driver's `timeout_secs` adds up the time its transport reads wait (`update_remaining_timeout`), and `await_read_or_interrupt` polls a read once before it builds the timeout, so a result that is already buffered, a chunk that is already there, and everything sqlh does between reads (decoding, formatting, waiting for the reader) never use it. Measured on the unfixed code, debug build, live tests with `-t 1`: `--allow-write` on `SELECT Id FROM dbo.Votes` (10 million narrow rows) ran 8.0 s (7.4 s in the first measurement above), and the loop of 20 million one-row results with `--allow-write` ran 91.8 s (the count budget starts again with each result, so `--max-results` hiding them bounds nothing). The single large value (a wait-bound result) did time out, as before.

What was built (`db::Pace`, `db::Drain`, `wait_for_output`):
- **A deadline of sqlh's own** per batch: `Instant::now() + -t` when the batch is sent (`-t 0` is none). The driver gets `timeout_secs(0)` for batches; the short internal statements keep the driver's accounting.
- **Three ways to keep it.** A timer task (`tokio::time::sleep_until`) fires the cancel while sqlh waits. `Pace::tick` counts work in ticks (a hidden row 1, a shown row 4, a chunk of a large value 16) and looks at the clock every 256, and every pass through the statement loop looks too (`Pace::slice`); a look fires the deadline if it has passed and hands the runtime back with `yield_now` if the loop has held it for 10 ms. Without the yield the timer, the count budget and the console listeners cannot run while the data is ready, on the current-thread runtime. `wait_for_output` has a deadline arm, so a slow reader ends at it too (the cancel arm stays first).
- **A cancel of its own.** The handles are nested: the run's, then the deadline's root, then the budget's root, then the driver's. Firing the budget cancels the statement only; firing the deadline cancels it and the budget's; Ctrl+C cancels all of them and leaves no cause behind. The first of the two to fire is recorded (`AtomicU8`), so a budget stop is never reported as `-t`, nor the reverse. A user's cancel wins over both (exit 6).
- **Outcomes** are the rules of owner decision 6: exit 4 and the existing message where rows to show are produced or awaited, with writes allowed, with `XACT_ABORT` in the input, and between statements or results; `Stop::Timeout` and the run's own exit code where only hidden rows were being read or awaited in a read-only run. A deadline whose attention is not acknowledged (the driver marks the connection dead after 5 s) is exit 4 too.

- **Tests through the real loops.** `SQLH_DEBUG_ROW_WORK_US` (debug builds) spins that many microseconds per row in `count_rows` and per chunk in `large_value`, after the read, so the stream is ready but the batch outlasts `-t`. `the_row_loop_keeps_the_deadline_when_the_work_is_sqlhs_own` (20,000 rows of 400 us, `-t 1`; shown, writes allowed and exact-count cases) fails when the `pace.tick` calls come out of `count_rows`: the run lasts its 8 s of work, 8206 ms measured. `the_chunk_loop_keeps_the_deadline_when_the_work_is_sqlhs_own` still passes with the tick out of `large_value`: each 32 KB chunk takes several socket reads, and tokio's own budget for them hands the runtime back often enough for the timer to run, so the chunk tick is a second guard and its mechanism is pinned only by the `Pace` unit tests.
- **A stalled reader mid-batch** ends the batch at the deadline (exit 4) before the writer's stall clock reaches `-t`; the closing lines then wait a stall limit, so the run takes about 2x `-t` and ends exit 7 (measured: `-t 3`, an unread pipe, release build, exit 4 at 3221 ms, exit 7 after). DESIGN.md §4.4 and §6 say so.

**Cost.** One tick is 3.5 ns in a release build (`pacing_costs_next_to_nothing`, 200 million ticks, ignored test), against 100 to 500 ns for a narrow row read. Interleaved medians of 6 runs on `SELECT Id FROM dbo.Votes`, release, batch time from the JSON `elapsed_ms`, before and after (the server and link vary by more than the difference): `--exact-count -t 0` 1889/1932 ms and 1893/1923 ms (the middle two of six); `--allow-write -t 0` 2051/2102 and 2003/2141; `--allow-write --max-rows 0 -t 0` 2035/2226 and 1858/1868. No difference that the noise doesn't explain.

**After the deadline** (release build, wall time of the process including about 0.15 to 0.25 s to connect, `-t 1`, 4 runs each): `--allow-write` on the 10 million rows 1.17 to 1.22 s (exit 4); `--exact-count` 1.18 to 1.23 s (exit 0, `≥N`, note `raise -t`); `--max-rows 0 --max-output 0` to a file, 12 to 14 MB written, 1.2 to 1.35 s (exit 4); the default bounded run 0.43 to 0.51 s (the 250 ms budget, exit 0); the 20 million one-row loop 1.18 to 1.35 s with `--allow-write` and 1.21 to 1.61 s read-only (exit 4 in all eight; the exit-0 stopped count is possible and the test accepts either); a 256 MB value built in steps and shown with `--max-cell 10` 1.24 to 1.25 s (exit 4). Between the deadline and the end of the batch the attention takes 8 to 82 ms in a release build. In a debug build, which cannot keep up with the stream, the server's data in flight takes longer to read: the same `--allow-write` run read 0.75 to 1.8 s after the deadline fired (total 1.9 to 3.1 s). That is the reason the tests allow `-t` plus 4 s. One more thing the tests taught: a single `REPLICATE` call for 300 MB cannot be interrupted, so the server answered the attention only when it was done and the driver waited its 5 s; the large-value test builds its value in steps (`SET @v += @v`) for that reason, and DESIGN.md §4.4 gives 5 s as the worst case of the grace.

**Tests.** Unit (`db/mod.rs`): `the_deadline_fires_in_a_loop_that_never_waits` (the deterministic regression: every row ready at once and 100 µs of work of ours each, 200,000 rows against a 100 ms deadline; the loop ends within 250 ms of it, where waiting-time accounting would never fire), `the_clock_check_fires_the_deadline_without_the_timer`, `a_busy_loop_lets_other_tasks_run`, `no_timeout_means_no_deadline_and_the_first_stop_wins`, `a_wait_for_the_reader_ends_at_the_deadline_and_a_cancel_wins`. Live (`tests/cli.rs`): `a_fast_narrow_result_is_bounded_by_t_in_every_mode` (`--allow-write`, `--exact-count`, `--max-rows 0 --max-output 0` in text and JSON, `--max-rows 0`, the default), `a_large_visible_value_is_bounded_by_t`, `many_small_results_are_bounded_by_t`. On the unfixed code the narrow-result and many-results tests fail (8.0 s and 91.8 s against a bound of 5 s); the large-value test passes there too, since a value streamed at the server's pace is a wait.
