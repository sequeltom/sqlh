# Harness kill (PLAN.md 2.5)

Measured 2026-10-11 on the dev workstation (Windows 11) against the dev/test SQL Server 2025 (17.0.1000.7, Standard Developer, 4 logical CPUs, same LAN), with the release build of `f4a6610` (mssql-tds 0.2.0). The question: when a harness ends sqlh outright, does the server stop the work, and how soon? DESIGN.md §4.3 had one measurement, a 20 s `WAITFOR` that was gone "within about 200 ms", which was the observer's own connect time. This note adds a CPU-bound query, a lock-blocked query and a large rollback, and a finer clock.

## Summary

- **The server cancels the work promptly.** For a waiting, a CPU-bound (serial and parallel) and a lock-blocked query, the request had ended within 0.14 s of the kill in all 46 runs (median about 30 ms). A killed blocker's transaction rolled back at once, its locks were released, and the session waiting behind it went on within 50 ms. Killing a blocked reader did not disturb the blocker (7 of 7 runs).
- **What a kill can't shorten is the rollback of a big open transaction.** Updating 500,000 rows in one transaction logged 237 MB; after a kill the server rolled it back for 4.8 to 14.6 s, as long as an orderly `ROLLBACK` of the same transaction takes (5.2 to 16.1 s), and about as long as the update itself took (11.9 to 18.8 s). A reader of the table waited for all of it. A statement killed while still running keeps its request alive while it undoes (about 2 s at 100 MB of log).
- **The default read-only mode is cheap to kill.** Its only writes are to `#temp` objects: 92 MB of tempdb log rolled back in about 0.6 s.
- **A kill that reaches only the shell that started sqlh ends nothing.** Windows leaves the child running, and the server kept working until sqlh's own `-t` limit ended it (2 of 2 runs).
- No code change follows. The two gaps (rollback of a big write, and a kill that misses sqlh) have mitigations below. DESIGN.md §4.3's sentence is updated.

## Method

- **Kill** is `Child::kill()` from a throwaway Rust harness, which is `TerminateProcess` on Windows. The harness starts every victim and observer as its own sqlh process (the release exe, `--profile test`), reads their stdout line by line with a monotonic arrival time, and stamps the instant the kill call returns.
- **Which session.** Each victim's first batch prints `@@SPID`, the session's `login_time` and its current transaction id, so the harness knows exactly what to watch. Session ids are reused at once (the next login got the victim's id in a manual check), so a watcher matches on id and login time. Each observer also says once what it was given: `program_name` was `sqlh`, and the request's SQL text (`sys.dm_exec_sql_text`) began with a unique comment (`/* hk-cpu-3 */`).
- **Observer.** A second, read-only sqlh runs the same `SELECT` as `GO 1000` batches, back to back. Each batch prints when it ends, so the harness sees every sample as it happens. The sample joins `sys.dm_exec_sessions`, `sys.dm_exec_requests` (status, command, wait type, CPU, blocker, percent complete), `sys.dm_tran_active_transactions` (state) and `sys.dm_tran_database_transactions` (log bytes used and reserved) on the victim's session and transaction. `sys.dm_tran_locks` costs about 30 ms a query, so locks get an observer of their own.
- **Clock.** Every sample carries the server's `SYSUTCDATETIME()`. The analysis places each sample at its server time, related to the harness clock by the median of (arrival minus server time) over the polls before the kill. That keeps a sample that was delivered late (the workstation was busy) where it was taken; the placement is off by the median delivery lag, a few ms. Sampling gaps were 2 to 32 ms at the median (36 to 72 ms for the lock sampler), with a few of 100 to 450 ms.
- **Result** is a bracket: the last sample that still showed the work, and the first that didn't. The tables give the second, in ms after the kill call returned, as an upper bound ("ended by"). It includes Windows closing the socket, the server noticing, and the sampling gap. A bracket that spans a long gap is reported as it is.
- **Permissions.** The dev login is sysadmin, so it holds `VIEW SERVER STATE` and the observer could see every session. A login without it can't observe another session this way (the DMVs show it only its own), and the only evidence of a kill is then its effect on other sessions, such as the reader that was released.
- **Scratch data.** Tables of the harness's own in `sqlh_test` (after checking its `dbo.sqlh_scratch` marker), all dropped afterwards. Nothing else was written, apart from `#temp` tables in the read-only case. No session or transaction of the runs was left. Files grew while the tests ran and were left as they are: the scratch database's log to about 1.35 GB (from 8 MB) and its data file to 136 MB, and tempdb's log from 8 MB to 264 MB (the `#temp` runs).
- **Load.** The server and the workstation were shared with other work throughout (builds, test suites, the server's own jobs), so absolute numbers, the rollbacks most of all, vary 2 to 3 times between series.

The SQL, with `$(v)`, `$(vt)` and `$(tx)` given by `-v` (the victim's session id, login time and transaction id). A victim's first batch:

```sql
SELECT @@SPID AS spid,
       CONVERT(varchar(27), (SELECT login_time FROM sys.dm_exec_sessions WHERE session_id = @@SPID), 126) AS lt,
       ISNULL((SELECT transaction_id FROM sys.dm_tran_current_transaction), 0) AS tx
```

The observer's sample (the harness wraps each in `ISNULL`, so that every column always has a value):

```sql
SELECT CONVERT(varchar(27), SYSUTCDATETIME(), 126) AS t,
       CASE WHEN s.session_id IS NULL THEN 0 ELSE 1 END AS sess,
       CASE WHEN r.session_id IS NULL THEN 0 ELSE 1 END AS req,
       r.status, r.command, r.wait_type, r.cpu_time, r.blocking_session_id, r.percent_complete,
       s.status AS session_status, x.n AS ntran, x.st AS tstate, y.used AS logused, y.res AS logres
FROM (SELECT 1 AS one) AS d
LEFT JOIN sys.dm_exec_requests AS r ON r.session_id = $(v)
LEFT JOIN sys.dm_exec_sessions AS s ON s.session_id = $(v) AND s.login_time = CONVERT(datetime, '$(vt)', 126)
OUTER APPLY (SELECT COUNT(*) AS n, MAX(transaction_state) AS st
             FROM sys.dm_tran_active_transactions WHERE transaction_id = $(tx)) AS x
OUTER APPLY (SELECT SUM(database_transaction_log_bytes_used) AS used,
                    SUM(database_transaction_log_bytes_reserved) AS res
             FROM sys.dm_tran_database_transactions WHERE transaction_id = $(tx)) AS y
```

and the lock observer's: `SELECT CONVERT(varchar(27), SYSUTCDATETIME(), 126) AS t, COUNT(*) AS locks FROM sys.dm_tran_locks WHERE request_session_id = $(v) AND request_status = N'GRANT'`.

## Cases

### Waiting query (control, as in DESIGN.md §4.3)

`WAITFOR DELAY '00:00:40'` under `--allow-write` (the guard refuses `WAITFOR`), killed about 3 s in. 13 runs.

| | min | median | max |
|---|---|---|---|
| request ended by | 5 ms | 21 ms | 137 ms |
| session gone by | 8 ms | 21 ms | 137 ms |

The 137 ms run had one sampling gap of 131 ms around the kill; the next slowest was 81 ms.

### CPU-bound query

A read-only run (so inside sqlh's wrapper transaction) of a query that takes 28.2 s to finish when left alone, one scheduler, no waits, no output:

```sql
SELECT COUNT_BIG(*) AS n, SUM(CONVERT(bigint, a.object_id) * (b.object_id % 7) + c.column_id) AS s
FROM sys.all_objects AS a CROSS JOIN sys.all_objects AS b
CROSS JOIN (SELECT TOP (20) column_id FROM sys.all_columns ORDER BY object_id, column_id) AS c
OPTION (MAXDOP 1)
```

Killed about 5 s in (3.5 to 5.4 s of CPU used), while the request was `running`. 15 runs.

| | min | median | max |
|---|---|---|---|
| request ended by | 7 ms | 34 ms | 97 ms |
| session and wrapper transaction gone by | 7 ms | 34 ms | 123 ms |

The request's `cpu_time` grew by at most 45 ms between the last sample before the kill and the last one after it (the 10 runs of the second series; it is in ms).

A **parallel** variant (an about 299,000 x 299,000 row cross join of `StackOverflow2010.dbo.Users` at `MAXDOP 2`, degree of parallelism 2 confirmed in `sys.dm_exec_requests.dop`, killed about 5 s in at 10.4 to 10.9 s of CPU): request ended by 28, 43, 49 and 58 ms. In between, the request showed the teardown waits (`EXECSYNC`, `CXSYNC_PORT`).

### Lock-blocked query

Table `T` of the harness's own in `sqlh_test`, 1,000 rows. **A** (`--allow-write`): `BEGIN TRANSACTION; UPDATE T SET v = v + 1;` then `WAITFOR DELAY '00:03:00'`, holding 1,017 locks. **B**: a read-only `SELECT SUM(v) FROM T`, waiting on `LCK_M_S` behind A (blocking session confirmed in every run). 7 runs.

**Kill B**, 1.5 s after it blocked:

| | min | median | max |
|---|---|---|---|
| B's request ended by | 27 ms | 41 ms | 74 ms |
| B's session and wrapper transaction gone by | 27 ms | 41 ms | 74 ms |

**A was unaffected** in 7 of 7 runs, over the 4 s that followed (111 to 182 state samples and 47 to 69 lock samples per run): session and request present, waiting on `WAITFOR`, transaction active, 1,017 locks held, every sample.

**Kill A**, with a third session **C** (the same `SELECT`) waiting behind it:

| | min | median | max |
|---|---|---|---|
| A's request ended by | 11 ms | 30 ms | 65 ms |
| A's session gone by | 13 ms | 30 ms | 77 ms |
| A's transaction gone by | 11 ms | 30 ms | 65 ms |
| A's locks released by (sampled every 36 to 72 ms) | 77 ms | 89 ms | 129 ms |
| C's answer reached the harness | 7 ms | 20 ms | 48 ms |

C's answer was the initial total in all 7 runs: A's update was rolled back, nothing was left half-done. The locks were released progressively as the update was undone (one sample caught 593 of the 1,017), over tens of ms.

### Large open transaction

Table of 500,000 rows (`id int identity primary key clustered, pad char(200)`, 103 MB). One transaction: `BEGIN TRANSACTION; UPDATE T SET pad = REPLICATE('b', 200);`. The transaction had logged **237 MB** with a further **188 MB reserved** for its own rollback when the update finished. Rolling back wrote about another 131 MB (the used log grew from 237 to about 368 MB while the reserved part fell to about 54 MB).

**Killed while idle in the open transaction** (a `WAITFOR` after the update), 1.5 s after the update finished. Two series: the first ran earlier; the second alternated with the orderly runs below, in the same period.

| series | work took | request ended by | transaction gone by |
|---|---|---|---|
| 1, warm-up (log growth) | 16.6 s | 5 ms | 12.6 s |
| 1 | 15.2 s | 4 ms | 8.7 s |
| 1 | 11.9 s | 33 ms | 4.8 s |
| 1 | 18.8 s | 9 ms | 7.0 s |
| 2, warm-up | 14.7 s | 30 ms | 11.9 s |
| 2 | 17.1 s | 40 ms | 14.6 s |
| 2 | 18.4 s | 32 ms | 13.3 s |
| 2 | 17.2 s | 6 ms | 14.0 s |
| 2 | 14.1 s | 12 ms | 11.5 s |
| 2 | 14.8 s | 5 ms | 11.8 s |

A reader of the table that started right after the kill (`SELECT COUNT(*) ... FROM T`) was held until the rollback ended and answered 0.1 to 0.3 s after it (its own connect time), with all 500,000 rows back to their old value in every run.

**Orderly `ROLLBACK`** of the same transaction (from the marker to the end of the run): series 1 took 5.5 s (warm-up), 11.1, 5.2 and 6.7 s; series 2, alternating with the kills above, took 16.1, 14.7, 11.1, 13.9 and 12.5 s. In series 2 the kills took 11.5 to 14.6 s and the rollbacks 11.1 to 16.1 s: **a kill costs no more and saves nothing.** The rollback took 0.4 to 0.95 times as long as the work.

**Killed while the UPDATE was running** (a harness timeout during a write), at 100 MB of its 237 MB logged. 4 runs. The request did not end at once. It stayed, as `UPDATE` in `running`, `runnable` and `suspended`, while the used log grew from 100 to 155 MB (the rollback's records), and ended when the statement was undone.

| | runs |
|---|---|
| request ended by | 1.97, 2.15, 2.07 and 1.83 s |
| session and transaction gone by | 1.98, 2.17, 2.08 and 1.84 s |
| the waiting reader answered at | 2.30, 2.35, 2.26 and 2.00 s |

**What the server shows during a rollback after a plain disconnect.** The request is gone from `sys.dm_exec_requests` within a few ms. The session stays, `sleeping` or `dormant`, with no request, so there is no `KILLED/ROLLBACK` command and no `percent_complete`. The progress is in the transaction views: `sys.dm_tran_active_transactions.transaction_state` is 7 (rolling back), and `database_transaction_log_bytes_used` rises while `..._reserved` falls. After a kill mid-statement the request stays instead, as above.

**Left open on exit.** In one run the UPDATE finished before the kill and sqlh ended normally (`-- ok: 2 batches`). Under `--allow-write` there is no wrapper transaction, and sqlh doesn't end one that the SQL left open: it closes the connection. The server rolled the open transaction back after sqlh had exited, which took about 3 s more, and the next call that touches the table would have waited for that.

### The read-only mode's `#temp` writes

The guard allows writes to `#temp` tables, and the wrapper transaction covers them. A read-only run created a `#temp` table, inserted 200,000 rows and updated them (0.7 to 0.8 s; 92 MB of tempdb log used, 73 MB reserved), then ran the CPU-bound query. Killed about 2 s later, 3 runs (one a warm-up): request ended by 50, 62 and 35 ms; session and transaction gone by 517, 585 and 607 ms. The tempdb rollback was fast, and no other session touches a private temp table, so nobody waits for it.

### A kill that misses sqlh

`cmd /c sqlh.exe ... -t 8 -i script.sql` with a 50 s `WAITFOR`; the harness terminated the `cmd.exe`, not sqlh, 2.3 to 2.4 s into the batch. sqlh went on: the request ran 5.7 and 5.8 s longer, until sqlh's own limit fired at 8 s (`-- error: query timeout after 8 s in batch 2`, exit 4), and sqlh closed the connection. Windows does not end child processes when their parent dies. The harness kept reading the pipes; an orphan whose pipe had been closed would notice only when it next wrote (not tried).

## Conclusions

- **A harness kill of sqlh is safe for the server.** Waiting, running and lock-waiting work stops within about a tenth of a second, even for a statement that never touches the network (CPU-bound), and locks and transactions are released as part of the same cleanup. DESIGN.md §4.3 says so, with these numbers.
- **The one real cost is the rollback of what the killed run had already done,** and it is the same cost as an orderly `ROLLBACK`: roughly the time the work took, 16 to 50 MB of the transaction's log a second here, during which everything that needs the changed rows waits. Nothing sqlh does after the kill can shorten it (it is gone), and killing the session on the server doesn't either: a session that is already rolling back can only be asked how far it is (`KILL n WITH STATUSONLY`).
- **In the default read-only mode the exposure is small.** Nothing but `#temp` objects is written, those roll back in well under a second per 100 MB, and no other session waits for them. The big-rollback case needs `--allow-write`.
- **Abandoning an empty transaction is free.** The CPU-bound and blocked-reader victims were inside sqlh's wrapper transaction; it was gone with the session in tens of ms. For PLAN.md Phase 5's question about a long read-only session transaction: abandoning one costs only what was written inside it. Not measured: what an idle, empty one costs while it lives (pinned tempdb log if it wrote temp data, schema-stability locks held by a long statement).

## Mitigations

For the cases where the server keeps working after a kill. None changes session settings (owner decision 7, PLAN.md 2.8) or code.

1. **Rollback of a big write** (`--allow-write` only).
   - **Keep each transaction small.** An agent that has to change many rows should do it in chunks that commit separately (for example `WHILE` over `UPDATE TOP (n)`, or a batch per range), so a kill rolls back at most one chunk and locks are short. A line in the skill (Phase 8) is the place, or in `--help` if there is room (it is at 62 lines); nothing in sqlh needs to change. The 237 MB case is 500,000 rows in one statement.
   - **Make waiters give up.** `SET LOCK_TIMEOUT` (PLAN.md 2.8) turns a wait behind a rollback into error 1222 after the chosen time. It is the session-level server-side bound for lock waits. As far as the documentation goes, SQL Server has no per-session limit on how long a query may run, so `-t` is a client-side bound that works only while sqlh lives.
   - **Database level (a DBA's choice, not sqlh's):** Accelerated Database Recovery makes the rollback of an aborted transaction near-instant, by Microsoft's documentation. Not tested here: `is_accelerated_database_recovery_on` is 0 on the scratch database and on `StackOverflow2010`, and changing it is outside what these runs may touch.
   - Seeing it: use `sys.dm_tran_active_transactions` and `sys.dm_tran_database_transactions` (see above), not `sys.dm_exec_requests`.
2. **A kill that misses sqlh.** Keep `-t`: its default of 60 s bounds an orphan that survives its parent, and `-t 0` removes the bound. A harness should end sqlh itself or its whole process tree (a job object, or `taskkill /T`), not just the shell. A watch inside sqlh on its parent process is possible, but a detached run would then be cancelled by design; not proposed without the owner's say.

## Not measured

- Windows authentication and strict encryption (all runs: SQL login, mandatory TLS).
- A network that drops without a FIN or RST (cable pulled, VPN down): the server then notices only through TCP keepalive. A process kill sends the close at once.
- A kill during the connect or login, and while the server is blocked sending a large result (`ASYNC_NETWORK_IO`).
- Rollbacks of gigabytes, other disks, or a server with Accelerated Database Recovery on.
- A harness that closes the output pipe but leaves sqlh running (the orphan case above kept its pipes open).

## Reproduce

By hand, at the 0.2 s resolution of the earlier measurement (the observer is a fresh connection). `victim.sql` holds `SELECT @@SPID AS spid`, `GO`, then `WAITFOR DELAY '00:00:40'`:

```powershell
$v = Start-Process -PassThru -WindowStyle Hidden target\release\sqlh.exe -ArgumentList '--profile','test','--allow-write','-t','0','-i','victim.sql' -RedirectStandardOutput v.out
Start-Sleep 3
$spid = [int](Get-Content v.out | Where-Object { $_ -match '^\d+$' } | Select-Object -First 1)
$check = "SELECT COUNT(*) AS sessions FROM sys.dm_exec_sessions WHERE session_id = $spid AND session_id <> @@SPID"
target\release\sqlh.exe --profile test -Q $check     # 1
Stop-Process -Id $v.Id -Force                         # TerminateProcess
target\release\sqlh.exe --profile test -Q $check     # 0
```

The `session_id <> @@SPID` is needed: the freed id goes to the next login, which can be the check itself.

The numbers above came from a throwaway std-only Rust harness (about 850 lines, and an analyzer of its logs) that is not part of the repo. It does what the Method section says: victims and observers as sqlh processes, the SQL shown there, and `Child::kill`. The harness checked the scratch marker first and dropped its tables.
