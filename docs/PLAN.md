# Plan of attack after the architecture review

Status: proposed (2026-10-10). This doc covers four things:
- a triage of [code-review.md](../code-review.md), with each finding re-checked against the code and most reproduced
- a re-review of the updated [VISION.md](VISION.md) and design docs
- new evidence from the test server
- the resulting order of work

VISION.md sets the priorities. This doc turns them into tasks with acceptance criteria, and lists the decisions only the owner can make (§6).

## 1. Review triage

Every finding holds up. Reproductions used `target/release/sqlh.exe`, built from the code as of `4e9b411` (`ee51e1c` changed only docs). They were local probes, or harmless read-only queries against the test server.

| ID | Review | Verdict | Evidence from this re-check | Plan priority |
|---|---|---|---|---|
| F01 | P1 | Confirmed, reproduced | `-i f.sql -o f.sql`: the SQL file was replaced with `-- note: no SQL to run`, exit 0 | P1 |
| F02 | P1 | Confirmed, reproduced | `sqlh -Q "" --format json \| true` exited 0, three times out of three, with no output delivered | P1 |
| F03 | P1 | Confirmed, reproduced | The guard passed `NEXT VALUE FOR`, `EXEC sys.sp_spaceused N'dbo.T', N'true'`, `EXEC Lnk.master.sys.sp_who`, `EXEC sales.sp_who` and `INSERT ##shared`. `##` writes are still rolled back by the wrapper; their risk is blocking other sessions. | P1, with `##` as a policy decision (§6) |
| F04 | P1 | Confirmed live, **and worse** | `--check -Q "SET NOEXEC OFF; SELECT 1"` ran the SELECT within the same batch, then printed "--check compiled the SQL without running it". Combined with `--allow-write`, input can execute writes. | **P0: first fix** |
| F05 | P1 | Confirmed, reproduced | `-Q "SELECT [é"` panics at `guard.rs:508` and the process ends with 0xC0000409, outside the exit-code contract and with no status line | P1 |
| F06 | P1 | Confirmed live | 300 inserts into a temp table produced 300 `-- 1 row affected` lines. Huge `(max)` values are materialized before cutting. | P1 (context budget) |
| F07 | P2 | Agree (inspection) | A harness that keeps reading stdout is unaffected, and a harness kill still works (§3). It matters for MCP. | P2 |
| F08 | P2 | Agree (inspection) | Only Ctrl+C is wired. Internal statements don't get the cancel handle. | P2 |
| F09 | P2 | Confirmed, reproduced | A Windows-auth profile plus `SQLCMDUSER` resolved to SQL login `environment_login`. Duplicate `server =` keys are accepted, last one wins. | P2 |
| F10 | P2 | Confirmed live | `-v sep=GO` produced a single batch and `Incorrect syntax near 'GO'`. A UTF-16 file's dangling byte was dropped silently. | P2 |
| F11 | P2 | Confirmed live | `SELECT 1 AS [-- forged]` printed the header `-- forged:int`, which reads as metadata | P2 |
| F12 | P2 | Agree (inspection) | Real, but profile edits are rare and human-run, so the gap is narrow. Surfacing credential-deletion errors is the part worth doing early. | P3, deletion errors P2 |
| F13 | P2 | Agree; **likely real in Git Bash** | Rust's `IsTerminal` treats mintty/msys ptys as terminals, but `GetConsoleMode` fails on them, so echo would stay on. Needs a manual check with a dummy password. | P2 |
| F14 | P2 | Agree (inspection) | Fix the doc claim now. Real zeroization can reuse the `zeroize` crate, which mssql-tds already pulls in, at no size cost. | P3, doc fix P1 |

The review's architecture section also holds up: extract an execution core, keep the driver boundary, and test the bespoke parsers more seriously. Its corrections to the schema and MCP plans are folded into Phases 4 and 5 below.

## 2. Docs re-review

The vision update is coherent, and AGENTS.md, DESIGN.md and the two plans now agree with it. What remains is mostly stale text and claims the code doesn't meet:

| # | Where | Problem | Resolution |
|---|---|---|---|
| D1 | VISION.md, DESIGN.md §2 | "SQL Server 2025 Standard on Windows Server 2025 Standard" didn't say where `sqlh.exe` runs. | **Resolved** (§6.1): workstations only, server usually remote. VISION.md, DESIGN.md and AGENTS.md updated. |
| D2 | DESIGN.md §4.3 | Says `$(var)` is substituted before splitting; the code splits first (F10) | **Decided** (§6.3): keep the code, correct the doc in Phase 1 |
| D3 | DESIGN.md §4.3, §6 | Ctrl+Break and console close are promised but not handled (F08) | Phase 2 |
| D4 | DESIGN.md §8 | "zeroed after use" is not true (F14) | Phase 1: correct the claim |
| D5 | `--help`, no README | No warning that the guard isn't a security boundary (F03). It matters more now that the test login is **sysadmin** (§3). | Phase 1 |
| D6 | `docs/notes/m0-driver-spike.md` | "Still to verify" lists Server 2016, SQL 2012/2014 and Azure | Phase 1: replace with the first-release checklist (§7) |
| D7 | schema-commands.md §4–§5 | Unlimited `describe`/`def` conflicts with a whole-response budget; fallback name resolution can discard an explicit schema | Phase 4 |
| D8 | mcp-mode.md §5 | Hour-long read-only session transaction; collected responses have no byte cap | Phase 5 |
| D9 | The test server | Its edition is *Standard Developer*, while first-release validation names *Standard* | Release checklist §7 |

## 3. New evidence from the test server

- **Connecting dominates every call.**
  - `SELECT 1` takes 171–265 ms inside sqlh, of which **162–254 ms is connect plus login**. The query itself, with setup and rollback, is about 10 ms. Ping to the server is 2.3 ms.
  - `-No` didn't help, so it isn't data-path TLS.
  - About 70 round trips' worth of time for a handshake that needs perhaps 5 points at something in the connect path: name lookups, SChannel credential setup, certificate parsing, or driver retries.
  - This is the largest lever on VISION's "time to the next useful answer". It also decides how urgent pooling is.
  - **Explained by 2.1** ([notes/connect-latency.md](notes/connect-latency.md)): about 150 ms of each connect is the server hashing the SQL login's password (PBKDF2, new in SQL Server 2025), by design. sqlh and the driver add about 5 ms.
- **A harness kill cancels server work promptly, for waits at least.** sqlh was killed with TerminateProcess during a 20 s `WAITFOR`. Within about 200 ms (the observer's own connect time) the server request and session were gone. CPU-bound and lock-blocked queries aren't measured yet.
- **The test login is sysadmin.** The over-privileged connection VISION.md worries about is the default situation here, and the read-only guard is the only thing standing between an agent and writes.
- **Draining has a cost.** Showing 100 of 299,398 `Users` rows took 1.3 s, all of it spent counting rows nobody sees. Bigger tables scale linearly.

## 4. Plan of attack

Sizes: **S** is up to half a day, **M** is 1–2 days, **L** is longer. Each phase ends green: fmt, clippy, unit tests, and the new CLI tests.

### Phase 1: correctness and trustworthy outcomes (VISION priority 1)

**Status (2026-10-10): built and tested, and so are the fixes the follow-up review asked for (below).** 1.9 was checked by hand in Git Bash on the dev machine: the password stayed hidden. That Git Bash runs native programs in a real Windows console (pseudo-console mode), so echo-off works there. The refusal path for a console-less mintty is verified by code review only.
- The harness is `tests/cli.rs`: 13 process-level tests, 3 of them live against the scratch database `sqlh_test`. A negative control confirmed the live tests really run.
- Every task below has its regression tests; 61 unit tests in all.
- Also done along the way:
  - credential-deletion errors now surface (the urgent part of F12)
  - `profile list` checks that a password exists without reading it (part of F14)
- **Found while testing:** an unreachable server ends as exit 2 (TCP connect timed out) or exit 4 (login deadline), depending on which timer fires first. Fixed in 2.4: an address that refuses or doesn't answer is exit 2, a server that doesn't finish the login exit 4.

Start with a **CLI test harness**, because most of these fixes are process-level: exit codes, files, crashes. Add `tests/cli.rs`, which spawns `CARGO_BIN_EXE_sqlh` and asserts exit code, stdout and stderr. Most cases need no server. Live cases skip themselves unless `SQLH_TEST_PROFILE` is set. (M)

| # | Task | Acceptance | Size |
|---|---|---|---|
| 1.1 | **F04.** In `--check` mode, refuse input that mentions `NOEXEC`, whether or not writes are allowed. (`PARSEONLY`, `FMTONLY` and `SHOWPLAN_*` only restrict execution further, so they can stay.) Re-send `SET NOEXEC ON` before every batch. Always use the rollback wrapper in check mode. Make the closing note true. | Live test: `SET NOEXEC OFF; SELECT 1` and the two-batch form are refused (exit 5) and nothing runs. `--check --allow-write` with an `INSERT` compiles and inserts nothing. | S |
| 1.2 | **F05 and a panic net.** Fix the unterminated-quote slice. Add property tests ("any UTF-8 input never panics") for both lexers, using a tiny in-repo generator rather than a new crate. Add a **panic hook** that writes `-- error: internal error ...` and `-- failed (exit 10)` and exits 10, so any future panic still honors the contract. | `SELECT [é` no longer crashes: it passes the guard and the server reports the syntax error. An injected test panic exits 10 with a status line. | S |
| 1.3 | **F01.** Load and preflight all input before opening `-o`. Refuse when output and input are the same file, comparing file identity via `GetFileInformationByHandle`, not path strings. Write to `<path>.partial` and rename at the end. | CLI tests: same path, `.\` alias and hard link are refused with exit 3, and the SQL file is intact. | S |
| 1.4 | **F02.** Errors from finishing or flushing output decide the exit code. Propose a **new exit code 7**, "output could not be delivered" (§6.2), with a best-effort stderr line. | A writer failing on its first write, mid-write and at flush gives exit 7, including for profile commands. | S |
| 1.5 | **F03 guard gaps.** Block `NEXT VALUE FOR`. Only accept procedure names qualified as `[master.]sys\|dbo.` with at most 3 parts, so linked servers and user schemas are refused. Drop `sp_spaceused` from the allowlist, or allow it with at most one positional argument and no `@updateusage`. Apply the `##` policy (§6.4). | Table tests for each case above, plus aliases and positional arguments. | S |
| 1.6 | **F11.** Escape a header name that starts with `--`, and pass every interpolated metadata field (procedure names, block reasons, notes) through the escaper, in csv's stderr stream too. | Hostile-name tests for headers, errors and blocked lines. | S |
| 1.7 | **F10.** Reject odd-length UTF-16. Implement and document the chosen substitution order (§6.3). Map line numbers correctly across multi-line values, or document that they're approximate. | Tests for odd bytes, a variable expanding to `GO`, and a multi-line value. | S |
| 1.8 | **F09 and the config parser.** Make auth explicit (`auth` decides it and `user` must agree). Resolve the auth method before credentials. Reject duplicate keys and contradictions. Let `-C` be turned off with `--trust-server-certificate=false`. | Precedence tests with an injected environment and credential store. | M |
| 1.9 | **F13.** Refuse to read a password unless echo was actually turned off, and point Git Bash users to PowerShell or `winpty`. | Manual: in Git Bash, `profile add` with a dummy password neither echoes nor proceeds. | S |
| 1.10 | **Docs truth-up** (D3–D6). Correct the claims about zeroization and Ctrl+Break, add the security-boundary sentence to `--help` and a short README, and replace the spike checklist with §7. | The doc review finds no claim that is ahead of the code. | S |

#### Phase 1 follow-up ([code-review-phase1.md](../code-review-phase1.md))

The follow-up review asked for changes before Phase 1 counts as complete. Each finding was reproduced, fixed and given a regression test. As a negative control, the new tests were run against `9482722`: they failed exactly where the review said, and the existing tests passed.

| # | Finding | Fix | Tests |
|---|---|---|---|
| R01 (P1) | Every run shared `<path>.partial`, so a run could write into output that another run had already published, and it took over any existing `.partial` file. | Each run creates its own `<path>.<pid>-<n>.partial` (CREATE_NEW, marked for deletion until published; no other process can change it, or open it while it's marked), closes it, then renames it. A failed run, or one killed while writing, leaves nothing (see R07 for when the deletion mark can't be set). An existing target that can't be written is refused before anything runs. Overlapping runs: the last complete file wins. | `files.rs` units; CLI: concurrent runs, a held destination, an existing `.partial` file |
| R02 | `eprintln!` and the panic hook could panic on a closed stream: exit 10 or an abort instead of 7. | Every diagnostic is a best-effort write. `--help` and `--version` write fallibly (exit 7). The password prompt returns write errors, so echo is restored, and the panic hook restores it too. stderr says when a failure mid-run leaves the outcome unknown. | CLI: stdout, stderr or both closed, across text, json, csv, help, version, usage errors and the panic path |
| R03 | The panic test failed in release builds. | Built only in debug builds, like `SQLH_DEBUG_PANIC`, and aimed at the unreachable address. | `cargo test --release --test cli` |
| R04 | The NOEXEC refusal advised `--allow-write`, which can't help. | Each refusal carries its own advice; `--allow-write` is suggested only where it helps. Specific advice for NOEXEC, `##` tables, `sp_spaceused`, qualified procedure names (`sys.sp_who`) and typos. | guard units; CLI: all three formats |
| R05 | CLR UDT type names reached text and csv metadata unescaped. | Type names are escaped like names, and `:` is written `\x3A`. | text and csv exact-output tests |

Also from the review:
- **Live suite:** the live tests are `#[ignore]`d, so skipping shows, and they fail when asked for without a profile. They write only to a table of their own, in a database with the `dbo.sqlh_scratch` marker (`tests/fixtures/sqlh_test.sql`), and drop it.
- **Docs:**
  - The README's "never interactive" names the `profile add` exception.
  - DESIGN.md and `--help` separate completed output from best-effort fatal diagnostics.
  - Exit 7 is documented as no reason to retry a write.
  - The `dbo.` rule is described as a policy, with the `sys.` spelling suggested.
- **Found while fixing:** Phase 1's rename broke `-o NUL` (exit 7). Devices and pipes are written directly again.
- **Still open, as planned:** the response budget (2.2), stalled-reader cancellation (2.3), Windows cancellation coverage (2.4), and F12's lock and config/credential ordering and F14's wiping (3.2).

Second round: the reviewer's verification of `817ab03` and `e3556bf` accepted R01–R05 and found two follow-ups for before the first release. Both are fixed. As a negative control, four unit tests and the CLI test fail against the old escaper.

| # | Finding | Fix | Tests |
|---|---|---|---|
| R06 (P2) | U+0085, U+2028 and U+2029 are line breaks to some consumers (Python's `splitlines`), and the escaper let them through, so a blocked statement's text could appear to start a `-- ok:` line. | `push_escaped` writes every control character as `\xNN`: C0, DEL and the whole C1 range, not only U+0085, so the rule is simply "every control character" and U+009B (a control sequence introducer on some 8-bit terminals) is covered. U+2028 and U+2029 become `\u2028` and `\u2029`. Other Unicode text is untouched. csv data rows (csv quoting) and json strings (valid as they are) are left alone. DESIGN.md §5.1 specifies the escapes and how a reader decodes them. | `output/mod.rs` units, including a check of every `char` against the break characters of Python's `splitlines`; text and csv exact output; CLI: blocked text holding each separator, in text and csv |
| R07 (P3) | The cleanup promise for the `.partial` file left out the case where the deletion mark can't be set. | Wording only. The mark is what deletes the file, even after a kill. Without it (not expected), `Drop` removes the file by name when the run ends normally, which is best effort, and a kill can leave it. `files.rs` and DESIGN.md §4.2 say so. | none: the failure can't be forced |

### Phase 2: fast, bounded, cancellable turns (VISION priority 2)

| # | Task | Acceptance | Size |
|---|---|---|---|
| 2.1 | **Connect-latency investigation** (§3), time-boxed. Time each phase (TCP, prelogin, TLS, login, setup) behind `--verbose`, find where the roughly 150 ms goes, then fix it in our code or file it upstream. An upstream investigation must not hold up 2.2–2.4, which are release requirements (follow-up review, answer 5). **Done 2026-10-10** ([notes/connect-latency.md](notes/connect-latency.md)):<br>• Of a 195 ms connect, about 150 ms is the server's PBKDF2 check of the SQL login's password (SQL Server 2025, by design), about 26 ms is TLS over Wi-Fi, and about 5 ms is sqlh and the driver.<br>• `--verbose` and `profile test` report the connect and setup times. A build with `--features timing` also splits the connect into steps. That isn't in default builds because it costs 270 KB.<br>• Found on the way: the driver's debug logging dumps packets in clear text, including the login packet (§6.9). | A breakdown is recorded in `docs/notes/`. Target: under 40 ms connect on this LAN, or a documented reason why not. **Met by the documented reason:** with SQL authentication on SQL Server 2025, the login alone exceeds 40 ms. | M |
| 2.2 | **F06, the response budget.** Collapse consecutive `rows affected` lines into one (`-- 300 statements, 300 rows affected`). Cap `GO n` at 1,000. Cap input at 10 MB. Add a total output budget (proposed default 1 MB, `--max-output`) that keeps room for the closing lines and reports what was withheld. **Done 2026-10-10:**<br>• `--max-output N` (default 1,000,000 bytes) covers every format and both streams, and keeps 1 KB for the closing lines. Past it, nothing more of the body is written. The cut result set's footer says `(--max-output)`, and a closing note lists what wasn't shown. JSON stays valid.<br>• Like the other limits it only stops printing: the run finishes and the counts stay exact. Stopping early instead is the drain question (2.6).<br>• Input is capped at 10 MB before and after substitution, and `GO n` at 1000.<br>• Huge values are still read whole before being cut; that's 2.7. | The temp-table loop prints one line. A run over the budget ends with a valid status (text and JSON) and a note. **Met:** live tests for both, plus unit tests per format. | M |
| 2.3 | **F07, separate output from the database.** A writer thread fed by a bounded channel, so the runtime keeps handling timers and cancellation while a pipe is stalled. **Done 2026-10-10** (`output/delivery.rs`):<br>• Sinks hand 64 KB chunks to an output thread and never block.<br>• Past 256 KB queued, the batch loop awaits room (or a cancel) instead of reading rows.<br>• A reader that makes no progress for `-t` seconds means the output couldn't be delivered: exit 7, from the batch or from the last wait for delivery.<br>• No measurable cost: 200,000 rows to a file took 383–414 ms before and after. | Test: with a stalled reader, `-t 2` still fires. **Met:** a live test holds stdout open unread; both paths end with exit 7 after about 2 s. Unit tests cover order, a stall, recovery and a broken pipe. | M |
| 2.4 | **F08, cancellation everywhere.** Ctrl+Break, close, logoff and shutdown handlers. Pass the cancel handle through connect, setup, NOEXEC and rollback. Report "attention acknowledged" vs "connection dropped". Make an unreachable server end the same way every time: today the TCP connect timeout (exit 2) and the login deadline (exit 4) race. **Fail fast on a refused connection:** today sqlh retries until the login deadline, taking 14.2 s with the default `-l 15`, although one refused attempt takes about 2 s on Windows (measured 2026-10-10). Restore console echo when Ctrl+C or a close arrives at the `profile add` password prompt. **Done 2026-10-10** ([notes/connect-failures.md](notes/connect-failures.md)):<br>• Ctrl+C, Ctrl+Break, close, logoff and shutdown cancel the run (`src/signals.rs`). The cancel reaches the connect, which is dropped at once, the setup, NOEXEC, every batch and the wait for a slow reader, and no later batch starts. The error says whether the server confirmed the cancel within 2 s (1 s after close, logoff or shutdown) or the connection was closed.<br>• One connect attempt: a refused connection fails in about 2 s. TCP gets `-l` and the login deadline `-l` + 1, so an address that refuses or doesn't answer is exit 2, and a server that doesn't finish the login is exit 4, every time. `-l 0` means no limit, as in sqlcmd; before, it timed out the TCP connect at once.<br>• Found on the way: for a remote name, the driver's fallback to Named Pipes blocked the whole runtime for about 21 s when the host didn't answer, past `-l` and Ctrl+C. sqlh now reaches remote names over TCP only (DESIGN.md §4.2). That and the driver ignoring a cancel during a connect attempt are upstream issues worth filing (see the note).<br>• Also fixed: after a cancel, a stalled reader got 1 s and then the full `-t` again, when the output was dropped. And, found while preparing the review: time with nothing to write counted toward a stall, so a run that had been quiet for longer than the limit could end with exit 7 although its output was delivered. That happened after any cancel that came over 1 s after the last output, and could happen in a race at the end of a run whose quiet stretch reached `-t`. The stall clock now starts when the writer is handed work.<br>• The `profile add` prompt restores echo on Ctrl+C, Ctrl+Break or close, then lets Windows end the process.<br>• Tests: a refused connection, a server that never answers, and Ctrl+Break during a connect (process-level); Ctrl+Break during a running query and while waiting for a stalled reader (live). Not tested automatically: close (it needs a console window to close), logoff and shutdown (Windows sends them only to services), cancelling during setup, NOEXEC or rollback (the same path as a batch, but too quick to catch), and the prompt's echo. | Tests for each signal and for cancelling during connect and setup. An unreachable server always gives the same exit code. A refused connection fails after one attempt. **Met, except as listed:** Ctrl+C shares Ctrl+Break's path. | M |
| 2.5 | **Harness-kill behavior.** Extend the §3 experiment to a CPU-bound query, a lock-blocked query and a large scratch rollback. | Results in `docs/notes/`. Any gap gets a mitigation (a server-side `-t`, a session setting). | S |
| 2.6 | **Drain policy experiment.** Measure the cost of draining on the biggest StackOverflow2010 tables. Prototype a bounded drain: stop counting after a time or row budget, send an attention, and report `≥N rows`, with an explicit note that later statements in that batch didn't run. | Numbers recorded, and the owner decides the default (§6.6). | M |
| 2.7 | **Memory.** Use the driver's streaming path (`next_row_into` with a cutting `RowWriter`, or PLP chunks) so hidden rows and cut cells aren't fully materialized. | Peak memory stays flat on a 1 GB `varchar(max)` value shown with `--max-cell 500`. | M |
| 2.8 | **Agent session defaults** (§6.7), proposed: `SET LOCK_TIMEOUT 10000` and `SET DEADLOCK_PRIORITY LOW`, so agent queries give way to production work instead of waiting behind it. **Also consider `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED` as the default for read-only runs** (owner, 2026-10-10: "worth considering"). The owner's earlier sql-query-runner skill sets it on every query, so agent reads never take shared locks or block writers. Points to settle with a live test: dirty reads can return uncommitted, duplicated or missing rows, so the docs must say so; it doesn't help against schema-modification locks, so the lock timeout is still needed; apply it only when read-only, never under `--allow-write`, where a write could be based on a dirty read; and decide whether a profile can override it. | Documented in DESIGN.md §4.3; a live test confirms the settings, including a read that would otherwise block behind an open write transaction. | S |
| 2.9 | **Help tips from agent trial 1** ([notes/agent-trial-1.md](notes/agent-trial-1.md)). In `--help`: how to see a table's or view's columns (`SELECT TOP 0 * FROM x`, or `sys.all_columns` for system views, since `sys.columns` and `information_schema` show none), and that without `-d` the profile's database applies (`master` if it has none). Consider giving the `test` profile a database. **Done 2026-10-10:** an example shows `SELECT TOP 0 * FROM sys.dm_exec_requests` (columns and types only), and the `-d` line says the default is the profile's database, else the login's (often `master`); `--help` is still 62 lines. Giving the `test` profile a database is left to the owner. | `--help` stays under about 60 lines; a repeat of trial 1 needs fewer discovery calls (to check in trial 2, 2.10). | S |
| 2.10 | **Agent trial 2.** Same setup, with tasks that hit the row and value limits, a guard refusal (a cleanup request), and a long query, to see whether the agent narrows queries, reads the advice, and avoids `--allow-write`. | Findings recorded in `docs/notes/`, follow-ups planned. | S |
| 2.11 | **Provenance, timings and a contract version in the output** (from the comparison with Microsoft's [`mssql-sqlcmd`](https://github.com/microsoft/mssql-rs/tree/main/mssql-sqlcmd) crate, 2026-10-10; the `--format json` document it renders carries all three). Today a run's output never says which server or database it ran against, and the connect time shows only under `--verbose`. Agent trial 1 spent a call finding out it was in `master`, and [notes/connect-latency.md](notes/connect-latency.md) found that connecting dominates a call.<br>• **Where it ran:** the server, the database in effect, the auth method, whether the connection is encrypted and the server version: a `connection` object in JSON, and a short tail on the status line in text and csv (for example `-- ok: 3 rows, 12 ms; connect 195 ms; devsql01 / master`; the exact shape comes from the golden tests and the token cost). `Session::describe` already has these from the login, so there is no extra round trip. Check that the driver's `database()` follows a `USE`; if it doesn't, report the database at connect. Never a connection string or anything from a credential. Left out when nothing connected (usage error, guard block, failed connect).<br>• **How long:** the connect time in the summary and in JSON (`connect_ms`), and `elapsed_ms` on each JSON batch, so a slow connect and a slow batch can be told apart. Once connections are reused (Phase 5) it shows about 0.<br>• **Contract version:** the JSON document starts with `contract_version` (`major.minor`; the major changes only for an incompatible change, and a minor may add optional fields, event types or enum values). Unlike the outcome fields it needs no buffering, because it's known before anything runs. Text and csv carry none, since it would cost tokens on every call; `--version` reports it instead.<br>• **Constraints:** every variable field goes through the escaper (§5.1), and a bracket-quoted database name may hold control characters; the closing lines must still fit the `--max-output` reserve with the longest possible names; `--help` is 62 lines now, so its FORMAT paragraph has to change without growing. | DESIGN.md §5 (the version rule, and each format's new fields) and §6 updated in the same change. Exact-output tests for text, json and csv, with and without a connection, with hostile server and database names, and for a run cut by `--max-output`. A repeat of trial 1 shows the database in the first response. How much goes in the text status line is owner decision 10 (§6). | M |
| 2.12 | **Write down where messages land, and pin it** (same comparison: Microsoft's JSON docs state what happens to a message that arrives while a result's rows are still streaming; ours says only "wire order"). Today the driver buffers informational messages, and `Session::run` delivers them at statement boundaries, and after the result is closed when its rows end in an error. So a message that arrives mid-result lands after that result, in every format. Nothing states or enforces that: the `BatchSink` docs say only "in wire order", and if a sink's `message`, `error` or `rows_affected` were ever called between `columns` and `end_rows`, the JSON sink would write the event inside the open result's `rows` (`message` doesn't close it) and the document would be invalid.<br>• Replace "wire order" with the rule in DESIGN.md §5.1 and §5.2 and in the `BatchSink` docs.<br>• Have each sink refuse the out-of-order call in tests (a debug assertion or a returned error; release builds must not panic on server-derived input), so a driver upgrade or the Runner refactor (3.1) can't break it silently.<br>• If a query can be found that makes the server send a message between rows, add it as a live test; otherwise the unit tests stand. | The rule is in DESIGN.md and the trait docs. Each of the three sinks has a test that fails if a message arrives mid-result. | S |

### Phase 3: execution core, CI and fixtures (the enabler for Phases 4 and 5)

| # | Task | Acceptance | Size |
|---|---|---|---|
| 3.1 | **Extract a library and a `Runner`** with `RunPolicy` (guard, wrapper, check mode), `QueryOrigin` (user SQL vs generated SQL; generated SQL skips `$(var)` and the classifier but not the wrapper) and `RunOutcome` (exit code and counts, independent of the sink). Profile commands stop importing from `main.rs`. | No behavior change: all existing tests and the smoke run pass. | L |
| 3.2 | **Injectable stores** for config, credentials and output. Then the F12 work: a lock around profile edits, unique temp files (`files::Partial` from the R01 fix fits), and credential/config coordination; deletion errors already surface. And F14: wipe sqlh's own password copies with `zeroize`, already in the dependency tree, and document what the driver keeps. | Failure-injection tests. | M |
| 3.3 | **`scripts/check.ps1`**, a local release check: fmt, clippy, tests, `--locked` release build, size budget, and `dumpbin` imports. It becomes CI once there's a remote. | One command, non-zero on any failure. | S |
| 3.4 | **Asserted live tests.** Turn `tests/smoke.sql` into golden-output tests with volatile values normalized. Add disposable fixtures (sequence, procedure, trigger, FK, filtered index) in the scratch database `sqlh_test` (§6.5). | `SQLH_TEST_PROFILE=test cargo test --test cli -- --ignored live` checks real server behavior, including the F03 and F04 regressions. | M |
| 3.5 | **GitHub Actions CI** (`.github/workflows/ci.yml`), added 2026-10-10 at the owner's request and modeled on ssms-executor's `maintenance-ci.yml`:<br>• Runs on pushes to `main` and on pull requests, on `windows-latest`, with `permissions: contents: read`.<br>• Checks out and runs `scripts/check.ps1` (3.3), so CI and the local check can't drift. The Rust toolchain comes from `rust-toolchain.toml`.<br>• Runs no live tests (there is no server in CI) and holds no secrets, so forks and PRs can run it.<br>• Pins action versions, as ssms-executor does. | A push or PR runs the same checks as `scripts/check.ps1` and fails when any of them does. | S |
| 3.6 | **Signed release workflow** (`.github/workflows/release.yml`), added 2026-10-10 at the owner's request and modeled on ssms-executor's `dotnetfx.yml` ([workflows](https://github.com/tkwj/ssms-executor/tree/master/.github/workflows)). The pattern to copy:<br>• Triggers on pushes to `main` and `workflow_dispatch`, with `permissions: id-token: write` and `contents: write`.<br>• Gates every signing and publishing step on `github.ref == 'refs/heads/main' && github.repository_owner == 'tkwj'` and a push or dispatch event, so pull requests and forks never reach the signing identity.<br>• Takes the version from the run number (`0.1.${{ github.run_number }}`; ssms-executor uses `3.1.`) and puts it in the exe, so `sqlh --version` matches the release. Stamping `Cargo.toml` would break `--locked`, because the lockfile records the root package's version. Pass the version at build time instead, for example an environment variable read with `option_env!`.<br>• Signs in to Azure with `azure/login` through an OIDC federated credential (secrets `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID`; no client secret). Then it runs `dotnet tool install --global sign --prerelease` and `sign code artifact-signing sqlh.exe` against the owner's existing Artifact Signing account and certificate profile (ssms-executor uses account `tkwj`, profile `tkwj-github`, endpoint `https://eus.codesigning.azure.net`).<br>• Zips the exe, uploads the workflow artifact, and runs `gh release create v<version> ... --draft --generate-notes`, so the owner reviews each release and publishes it by hand.<br>Where sqlh differs: there is no gallery step, because the GitHub release is the publication (AGENTS.md). The build is `cargo build --release --locked`, and the 3.3 checks (size budget, `dumpbin /dependents` imports in-box only) run before signing. Signing changes the exe, so the signature is verified (`signtool verify /pa /v`) and a SHA-256 file is computed after it, and both ship in the release.<br>**Owner-only setup:** the GitHub repository and the remote; the three secrets; a federated credential for the new repository on the Azure app registration (its subject names the repository and branch); and a check that the existing certificate profile covers signing an exe.<br>**To settle when building it:** whether every push to `main` should make a draft release, as ssms-executor does, or only `workflow_dispatch` and version tags, since pre-release `main` will move often. | A push to `main` produces a draft release holding a signed `sqlh.exe` in a zip. `signtool verify /pa /v` passes on it, and `sqlh --version` shows the release version. A pull-request run signs and publishes nothing. | M |

### Phase 4: focused schema discovery (VISION priority 3a)

Build `docs/schema-commands.md` on top of the Phase 3 `Runner`, with the review's corrections:
- fallback resolution never discards an explicit schema, and is defined for case-sensitive databases
- alias types resolved to their base type
- `#temp` only within sessions
- an explicit marker for metadata that is unsupported or hidden, rather than a bare NULL
- `describe` and `def` get a high default limit under the response budget, not unlimited
- framing that survives control characters in definitions

Validate on SQL Server 2025, and keep 2016 portability cheap but untested. (L)

**Evidence from agent trial 1** ([notes/agent-trial-1.md](notes/agent-trial-1.md)): five of eleven calls went to finding one system view's columns, including an invented view name. `describe` should cover system views and DMVs (`sys.dm_db_partition_stats`), not only user tables, so this costs one call.

**Also in this phase: profile descriptions and search** (owner, 2026-10-10, after comparing sqlh with the earlier sql-query-runner skill, which has both and uses them to pick a server). Finding out *which server* is the first step of many turns, and it costs context like any other discovery. It is independent of the schema commands, so it can ship ahead of them. (S)

| # | Task | Acceptance |
|---|---|---|
| 4.1 | **`description` profile key.** Optional, one line, up to 300 characters (environment, purpose, sensitivity: "prod reporting replica, safe for ad-hoc"). Set with `sqlh profile add NAME --description "..."`; a re-run that leaves it out keeps the saved value, like other options. Optional in the file so existing profiles keep working. Human-set only, because `profile add` is the only prompt. | Parser tests (known key, over-length, repeated key, escapes); `profile add` keeps and replaces it |
| 4.2 | **`profile list` shows it, and `profile list --search TEXT` filters.** A case-insensitive substring match over the name and the description. It never reads a password, as today. Every variable field goes through the text escaper like any metadata field (DESIGN.md §5.1), including in json. | Exact-output tests for text and json, including a hostile description; a search that matches only the description; no match gives an empty list, not an error |
| 4.3 | **Docs.** DESIGN.md §8 gains the key, the flag and the output shape; `--help` stays under about 60 lines. | DESIGN.md §8 and `--help` match the code |

### Phase 5: connection reuse (VISION priority 3b)

Decide the mechanism using the 2.1 results:
- **2.1's answer: connecting doesn't get fast** with SQL authentication on SQL Server 2025. Each login spends about 150 ms in the server's password hashing, by design, so reuse is needed rather than an optimization. Windows authentication skips that hash and may be fast enough without reuse; it wasn't measured.
- **Choose:**
  - **(a) MCP** as designed in `docs/mcp-mode.md`, with the review's prerequisites: a response byte cap, bounded request queues, reset-hygiene tests, and discarding any connection whose cancellation can't be proven clean.
  - **(b) A local per-profile daemon** that the CLI talks to over a named pipe, which gives reuse to Bash-driven agents without MCP.
- **Either way:** replace the hour-long read-only session transaction with per-call wrappers, unless 2.5 and 2.6 show that a long transaction is cheap. (L)

### Phase 6: command categorization (VISION priority 4)

This is design work with the owner before any code:
1. List concrete DBA investigation workflows: blocking chains, wait stats, Query Store, plan cache and showplans, index usage, statistics, `sp_who2`, and common community procedures.
2. List their risks.
3. Derive categories (data read, metadata read, server-state read, session scratch, maintenance, data write, DDL, security, external or remote).
4. Map each category to a policy (allow, deny, require `--allow-write` or the `execute` tool).

Classifier v2 implements the result. Showplan support (`SET SHOWPLAN_XML`, summarized) probably belongs here as a first-class DBA read. (L, mostly design)

**Interim step, not blocked on the taxonomy: extend the EXEC allowlist** (owner, 2026-10-10: "we will need to allow `xp_readerrorlog` and `xp_instance_regread`, and maybe more if the login permissions allow"). VISION.md keeps selecting individual permitted commands separate from designing the categories, so this can ship first. Today `xp_*` is blocked wholesale (DESIGN.md §7), which rules out reading the SQL Server error log, a basic investigation. The earlier sql-query-runner skill already allows these two, so they come from real use. (S)
- **Principle:** the allowlist says which commands are read-only. Whether a login may run one is the server's call, and database permissions stay the enforcement boundary (VISION.md). So a command belongs on the list because it only reads, not because this login happens to have the rights, and a login without the permission just gets the server's error.
- **First additions:** `xp_readerrorlog` and its wrapper `sp_readerrorlog`, and `xp_instance_regread`. Name them exactly: a prefix match on `xp_*` would also admit `xp_cmdshell` and `xp_instance_regwrite`, which stay blocked. `xp_instance_regread` reads registry values, which can disclose information; record that in the category notes.
- **Candidates to evaluate in the same pass:** `xp_fixeddrives`, `xp_msver`, and `sp_configure` in its listing form only (with arguments it changes settings, so it needs the argument check `sp_spaceused` gets).
- **To verify on SQL Server 2025 Standard:** which permission each one needs for a non-sysadmin login (newer versions have finer-grained server permissions for the error log), so the blocked/allowed advice and the docs can say what to grant.
- **Acceptance:** table-driven guard tests (allowed bare and qualified as `sys.`/`master.`; `xp_instance_regwrite` and `xp_cmdshell` still blocked; stacked statements still refused); a live read of the error log; DESIGN.md §7 updated in the same change.

### Phase 7: evidence-based evaluations (VISION priority 5)

Exact row counts (from 2.6 and real sessions), the Windows/SQL Server 2016 floor, the md format, and what SmartScreen and antivirus make of the signed exe on locked-down workstations (the signing itself is task 3.6).

### Phase 8: delivery to agents (skill, connector, plugin)

Raised 2026-10-10. Not a VISION priority: the exe is the product, and these are wrappers that get it to agents. Nothing is decided or started; the owner deferred drafting the skill. In order:

1. **Skill** (S, no code). A `SKILL.md` for Bash-driven agents. `--help` already carries the syntax, so the skill covers workflow: describe before querying, how to read the output format, the limits and exit codes, and when to reach for sqlh. It can be drafted at any time, and is more useful once Phase 4's schema commands exist. From agent trial 1: tell the agent to state only what the output shows (the trial's answer named tables it never queried), to pass `-d` or check `DB_NAME()` first, and to look up column names before writing a query.
2. **Connector = local stdio MCP server** (built in Phase 5(a), not here). It is not a hosted, remote connector: credentials come from Credential Manager and SSPI on the workstation, and the workstation reaches the SQL Server, and neither survives a hosted deployment. Packaging it as a Claude Desktop extension (`.mcpb`) is a possible later step and is unresearched.
3. **Plugin** (S, after 2). Bundles the skill, the MCP config and the exe location so installing is one step. Open: whether the plugin carries the exe or points at an installed path, which ties into the code-signing decision in Phase 7.

If Phase 5 chooses the local daemon (b) over MCP, step 2 drops out and the skill carries more of the load.

## 5. Sequencing rationale

- **1.1 (F04) first.** It is the only finding where the tool states something false while possibly executing writes.
- **The test harness comes before the other fixes,** so every fix lands with a process-level regression test.
- **2.1 (connect latency) early, but time-boxed.** It is cheap to measure and could change Phase 5 entirely. Building pooling to hide a fixable 150 ms would be the wrong order. Bounded output and cancellation under a stalled reader (2.2–2.4) are release requirements, so they come before pooling or more discovery features, whatever 2.1 finds.
- **The Runner refactor (3.1) after the correctness fixes and before schema/MCP work.** Fixing defects first keeps the refactor behavior-neutral and easy to verify; schema commands and MCP both need its policies.

## 6. Owner decisions

Decided 2026-10-10:

1. **Where sqlh runs: workstations only.** The SQL Server is assumed to be remote in about 90% of use cases. The first release is validated on Windows 11 x64 workstations against a remote SQL Server 2025 Standard. Running on the database server is not a target.
2. **Exit code 7: output could not be delivered.** DESIGN.md §6 defines exactly what "undeliverable" means.
3. **Substitution order: split first, then substitute per batch** (today's behavior). Variables can't create batch separators. DESIGN.md §4.3 is corrected to match.
4. **Global temp tables (`##`) are blocked in read-only mode.**
5. **A scratch database and a low-privilege test login on the dev/test server are approved.** The scratch database is `sqlh_test`. The test login waits until its password can be stored without a console (planned with 3.4), so it never passes through a command line or a file.

Still open:

6. **The drain default after 2.6.** Exact counts as today, or a bounded drain with `≥N`?
7. **Agent session defaults.** `LOCK_TIMEOUT` and `DEADLOCK_PRIORITY LOW`: yes or no, and which values? Also `READ UNCOMMITTED` for read-only runs (owner, 2026-10-10: worth considering as a default); the trade-offs are in 2.8.
8. **A Git remote and CI: GitHub, with Actions (owner direction, 2026-10-10).** 3.3 becomes CI (3.5), and 3.6 adds a signed release workflow like ssms-executor's. Still open: the repository's name and visibility (the history needs the private-data review in AGENTS.md before any public push), and 3.6's trigger and version scheme.
9. **Ask upstream to redact the password in packet dumps?** At debug level, mssql-tds hex-dumps every TDS packet before TLS encrypts it (`io/packet_writer.rs`), including the login packet ([notes/connect-latency.md](notes/connect-latency.md)). The password in it carries only the scramble the TDS protocol requires (nibble swap and XOR 0xA5), which anyone can reverse. It's a logging-hygiene issue (CWE-532), not a remote vulnerability: nothing leaks unless an application turns on debug logging for the driver. Other drivers' traces usually mask the password. The channel is the owner's choice: a GitHub issue, or Microsoft's security process. Check `main` first; this was read in 0.2.0.
10. **How much provenance goes in the text output (2.11).** Proposed: the server, database and connect time on the status line of every run, which costs a few tokens per call. Alternatives: JSON only; or text only when the database wasn't named by `-d` or the profile, which is the case that bit trial 1.

## 7. First-release checklist (replaces the spike's "still to verify")

- [ ] Clean Windows 11 x64 workstation (§6.1): runs with no installs, `dumpbin` imports are in-box only, and the size is within budget
- [ ] Against a remote SQL Server 2025 over a real network: connect latency measured (2.1 measured the dev LAN; the release environment is still to do), encryption modes mandatory and strict
- [ ] SQL Server 2025 **Standard** (not only Standard Developer): SQL auth, and Windows auth if it's in scope
- [ ] Every finding in §1 fixed or explicitly accepted, each with a regression test
- [ ] Cancellation: Ctrl+C, Ctrl+Break, console close, harness kill, `-t`, during connect and during execution
- [ ] The response budget holds for text, json and csv, and the status line is always present
- [ ] `scripts/check.ps1` is green, CI (3.5) passes on the remote, live tests are green with `SQLH_TEST_PROFILE`, and the docs have no claims ahead of the code
- [ ] Code signing: the release workflow (3.6) produces a signed `sqlh.exe`, and SmartScreen and antivirus behavior is checked on a clean, locked-down workstation
