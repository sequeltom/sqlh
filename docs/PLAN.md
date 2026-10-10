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

The vision update is coherent, and CLAUDE.md, DESIGN.md and the two plans now agree with it. What remains is mostly stale text and claims the code doesn't meet:

| # | Where | Problem | Resolution |
|---|---|---|---|
| D1 | VISION.md, DESIGN.md §2 | "SQL Server 2025 Standard on Windows Server 2025 Standard" didn't say where `sqlh.exe` runs. | **Resolved** (§6.1): workstations only, server usually remote. VISION.md, DESIGN.md and CLAUDE.md updated. |
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
- **A harness kill cancels server work promptly, for waits at least.** sqlh was killed with TerminateProcess during a 20 s `WAITFOR`. Within about 200 ms (the observer's own connect time) the server request and session were gone. CPU-bound and lock-blocked queries aren't measured yet.
- **The test login is sysadmin.** The over-privileged connection VISION.md worries about is the default situation here, and the read-only guard is the only thing standing between an agent and writes.
- **Draining has a cost.** Showing 100 of 299,398 `Users` rows took 1.3 s, all of it spent counting rows nobody sees. Bigger tables scale linearly.

## 4. Plan of attack

Sizes: **S** is up to half a day, **M** is 1–2 days, **L** is longer. Each phase ends green: fmt, clippy, unit tests, and the new CLI tests.

### Phase 1: correctness and trustworthy outcomes (VISION priority 1)

**Status (2026-10-10): built and tested.** 1.9 was checked by hand in Git Bash on the dev machine: the password stayed hidden. That Git Bash runs native programs in a real Windows console (pseudo-console mode), so echo-off works there. The refusal path for a console-less mintty is verified by code review only; this machine can't produce it.
- The harness is `tests/cli.rs`: 13 process-level tests, 3 of them live against the scratch database `sqlh_test`. A negative control confirmed the live tests really run.
- Every task below has its regression tests; 61 unit tests in all.
- Also done along the way:
  - credential-deletion errors now surface (the urgent part of F12)
  - `profile list` checks that a password exists without reading it (part of F14)
- **Found while testing:** an unreachable server ends as exit 2 (TCP connect timed out) or exit 4 (login deadline), depending on which timer fires first. Folded into 2.4.

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

### Phase 2: fast, bounded, cancellable turns (VISION priority 2)

| # | Task | Acceptance | Size |
|---|---|---|---|
| 2.1 | **Connect-latency investigation** (§3). Time each phase (TCP, prelogin, TLS, login, setup) behind `--verbose`, find where the roughly 150 ms goes, then fix it in our code or file it upstream. | A breakdown is recorded in `docs/notes/`. Target: under 40 ms connect on this LAN, or a documented reason why not. | M |
| 2.2 | **F06, the response budget.** Collapse consecutive `rows affected` lines into one (`-- 300 statements, 300 rows affected`). Cap `GO n` at 1,000. Cap input at 10 MB. Add a total output budget (proposed default 1 MB, `--max-output`) that keeps room for the closing lines and reports what was withheld. | The temp-table loop prints one line. A run over the budget ends with a valid status (text and JSON) and a note. | M |
| 2.3 | **F07, separate output from the database.** A writer thread fed by a bounded channel, so the runtime keeps handling timers and cancellation while a pipe is stalled. | Test: with a stalled reader, `-t 2` still fires. | M |
| 2.4 | **F08, cancellation everywhere.** Ctrl+Break, close, logoff and shutdown handlers. Pass the cancel handle through connect, setup, NOEXEC and rollback. Report "attention acknowledged" vs "connection dropped". Make an unreachable server end the same way every time: today the TCP connect timeout (exit 2) and the login deadline (exit 4) race. **Fail fast on a refused connection:** today sqlh retries until the login deadline, taking 14.2 s with the default `-l 15`, although one refused attempt takes about 2 s on Windows (measured 2026-10-10). | Tests for each signal and for cancelling during connect and setup. An unreachable server always gives the same exit code. A refused connection fails after one attempt. | M |
| 2.5 | **Harness-kill behavior.** Extend the §3 experiment to a CPU-bound query, a lock-blocked query and a large scratch rollback. | Results in `docs/notes/`. Any gap gets a mitigation (a server-side `-t`, a session setting). | S |
| 2.6 | **Drain policy experiment.** Measure the cost of draining on the biggest StackOverflow2010 tables. Prototype a bounded drain: stop counting after a time or row budget, send an attention, and report `≥N rows`, with an explicit note that later statements in that batch didn't run. | Numbers recorded, and the owner decides the default (§6.6). | M |
| 2.7 | **Memory.** Use the driver's streaming path (`next_row_into` with a cutting `RowWriter`, or PLP chunks) so hidden rows and cut cells aren't fully materialized. | Peak memory stays flat on a 1 GB `varchar(max)` value shown with `--max-cell 500`. | M |
| 2.8 | **Agent session defaults** (§6.7), proposed: `SET LOCK_TIMEOUT 10000` and `SET DEADLOCK_PRIORITY LOW`, so agent queries give way to production work instead of waiting behind it. | Documented in DESIGN.md §4.3; a live test confirms the settings. | S |

### Phase 3: execution core, CI and fixtures (the enabler for Phases 4 and 5)

| # | Task | Acceptance | Size |
|---|---|---|---|
| 3.1 | **Extract a library and a `Runner`** with `RunPolicy` (guard, wrapper, check mode), `QueryOrigin` (user SQL vs generated SQL; generated SQL skips `$(var)` and the classifier but not the wrapper) and `RunOutcome` (exit code and counts, independent of the sink). Profile commands stop importing from `main.rs`. | No behavior change: all existing tests and the smoke run pass. | L |
| 3.2 | **Injectable stores** for config, credentials and output. Then the F12 work: a lock around profile edits, unique temp files, credential/config coordination, and surfacing deletion errors. | Failure-injection tests. | M |
| 3.3 | **`scripts/check.ps1`**, a local release check: fmt, clippy, tests, `--locked` release build, size budget, and `dumpbin` imports. It becomes CI once there's a remote. | One command, non-zero on any failure. | S |
| 3.4 | **Asserted live tests.** Turn `tests/smoke.sql` into golden-output tests with volatile values normalized. Add disposable fixtures (sequence, procedure, trigger, FK, filtered index) in the scratch database `sqlh_test` (§6.5). | `SQLH_TEST_PROFILE=test cargo test` checks real server behavior, including the F03 and F04 regressions. | M |

### Phase 4: focused schema discovery (VISION priority 3a)

Build `docs/schema-commands.md` on top of the Phase 3 `Runner`, with the review's corrections:
- fallback resolution never discards an explicit schema, and is defined for case-sensitive databases
- alias types resolved to their base type
- `#temp` only within sessions
- an explicit marker for metadata that is unsupported or hidden, rather than a bare NULL
- `describe` and `def` get a high default limit under the response budget, not unlimited
- framing that survives control characters in definitions

Validate on SQL Server 2025, and keep 2016 portability cheap but untested. (L)

### Phase 5: connection reuse (VISION priority 3b)

Decide the mechanism using the 2.1 results:
- **If connect gets fast** (around 40 ms), one-shot CLI calls may be good enough for much of the workflow, and reuse becomes an optimization.
- **If not, choose:**
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

### Phase 7: evidence-based evaluations (VISION priority 5)

Exact row counts (from 2.6 and real sessions), the Windows/SQL Server 2016 floor, the md format, and code signing.

## 5. Sequencing rationale

- **1.1 (F04) first.** It is the only finding where the tool states something false while possibly executing writes.
- **The test harness comes before the other fixes,** so every fix lands with a process-level regression test.
- **2.1 (connect latency) early.** It is cheap to measure and could change Phase 5 entirely. Building pooling to hide a fixable 150 ms would be the wrong order.
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
7. **Agent session defaults.** `LOCK_TIMEOUT` and `DEADLOCK_PRIORITY LOW`: yes or no, and which values?
8. **A Git remote and CI?** That decides whether 3.3 stays a local script.

## 7. First-release checklist (replaces the spike's "still to verify")

- [ ] Clean Windows 11 x64 workstation (§6.1): runs with no installs, `dumpbin` imports are in-box only, and the size is within budget
- [ ] Against a remote SQL Server 2025 over a real network: connect latency measured (2.1), encryption modes mandatory and strict
- [ ] SQL Server 2025 **Standard** (not only Standard Developer): SQL auth, and Windows auth if it's in scope
- [ ] Every finding in §1 fixed or explicitly accepted, each with a regression test
- [ ] Cancellation: Ctrl+C, Ctrl+Break, console close, harness kill, `-t`, during connect and during execution
- [ ] The response budget holds for text, json and csv, and the status line is always present
- [ ] `scripts/check.ps1` is green, live tests are green with `SQLH_TEST_PROFILE`, and the docs have no claims ahead of the code
- [ ] Decision on code signing (SmartScreen and antivirus on locked-down servers)
