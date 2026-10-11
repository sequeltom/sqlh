# sqlh

`sqlh.exe` is a SQL Server query tool for AI agents (Claude Code and other harnesses) working through a person's DBA or developer questions. It does the job of `sqlcmd`, as a single Windows executable with no runtime dependencies: no ODBC, no OLE DB, no .NET.

What makes it agent-friendly, in brief: compact typed output with hard limits, errors that point at the input line, documented exit codes, no prompts, and read-only by default. The full running list is [Built for agents](#built-for-agents).

Status: pre-release. It is built and tested on Windows 11 x64 against SQL Server 2025. See [docs/PLAN.md](docs/PLAN.md) for what's being worked on.

## Safety: read this first

Read-only mode **guards against accidents; it is not a security boundary.** Before anything runs, sqlh refuses SQL that writes, and it runs everything inside a transaction that it rolls back. That stops a confused agent from changing data. It can't stop a determined one with the same login, and some effects survive a rollback.

**Give agents a login with only the permissions they need**, for example `db_datareader` and `VIEW DEFINITION`. The database is the enforcement boundary. Writes need `--allow-write`, and a profile can refuse even that (`--write-policy deny`).

## Build

You need Rust (via `rustup`; the toolchain is pinned in `rust-toolchain.toml`) and the Visual Studio C++ build tools with a Windows SDK.

```powershell
cargo build --release      # target\release\sqlh.exe, about 2 MB
cargo test                 # no server needed
```

## Quick start

Save a connection once. The password is asked for with echo off and stored in Windows Credential Manager:

```powershell
sqlh profile add dev -S myserver -d AppDb -U agent_reader
```

Then query:

```powershell
sqlh --profile dev -Q "SELECT TOP 5 name, create_date FROM sys.tables ORDER BY create_date DESC"
```

```
-- result 1
name:nvarchar(128)	create_date:datetime
Orders	2026-09-30T14:02:11.483
...
-- 5 rows
-- ok: 1 batch, 210 ms
```

`--format json` and `--format csv` are also available. `sqlh --help` is the full reference; it's written to be read by models.

## Built for agents

What `sqlh` does that `sqlcmd` doesn't, because the reader is a model with a limited context, working through a person's questions across several turns. This list is kept current: a feature is added here in the same change as its code ([AGENTS.md](AGENTS.md)). The tags say when it landed (see [docs/PLAN.md](docs/PLAN.md)); the spec is [docs/DESIGN.md](docs/DESIGN.md).

**Tally: 31 built, 7 planned.**

### Built

**Output an agent can afford to read**
- **Compact, typed output.** `name:type` headers, tab-separated rows, ISO dates, exact decimals, and `NULL` kept apart from the string `'NULL'` (written `\NULL`). *(MVP)*
- **Hard limits by default.** 100 rows, 500 characters per value, 20 result sets, and 1 MB for the whole response, errors, notes and diagnostics included (`--max-rows`, `--max-cell`, `--max-results`, `--max-output`), and 10 MB of input. Text that doesn't fit is cut with a marker; the status line never is. *(MVP; `--max-output` in Phase 2.2)*
- **Hidden rows are counted; cut values say how much.** `-- 100 of 48213 rows shown (--max-rows 100)` and `…[+N]`, so the agent knows what it didn't see and can narrow the query. *(MVP)*
- **A status line that proves the output is complete.** The last line is `-- ok: ...` or `-- failed (exit N): ...`. Without it (exit 7 or 10), the output can't be trusted. *(MVP)*
- **Loops don't flood.** Consecutive row counts collapse into `-- 300 statements, 300 rows affected`. *(Phase 2.2)*
- **Data can't pass for metadata.** Control characters and Unicode line breaks are escaped, and a leading `--` in a value is too, so nothing stored in a database can forge an `-- ok:` line. *(Phase 1)*
- **Three formats.** Text (the default, cheapest in tokens), json (one streamed document of ordered events, valid even when cut short) and csv (data only on stdout, metadata on stderr). *(MVP)*
- **A message never lands in the middle of a result.** A PRINT or warning that arrives while a result streams is written after that result, never among its rows, and the output refuses an event out of order instead of writing a broken document. *(Phase 2.12)*
- **JSON says where it ran and how long it took.** A json document starts with its contract version, then the server, database, auth method and encryption it connected with and the connect time; each batch ends with its duration. `sqlh --version` gives the contract version. *(Phase 2.11, json part)*

**Errors an agent can act on**
- **Errors point at the input.** File and line, mapped back through `GO` batches and `$(var)` substitution; inside a procedure, the procedure and its own line. *(MVP)*
- **Exit codes with a meaning.** 0 ok, 1 SQL error, 2 connect, 3 usage, 4 timeout, 5 blocked, 6 cancelled, 7 output not delivered, 10 internal. Exit 7 means "don't trust what you received", and it is no reason to retry a write blindly. *(MVP; exit 7 in Phase 1)*
- **Refusals say how to fix them.** A blocked write names the statement and the better spelling (`#temp` instead of `##t`, `sys.sp_who`) before it suggests `--allow-write`. *(Phase 1)*
- **Connection errors name what was tried** (server, port, auth, encryption) and hint at the usual cause: SQL Browser, certificate trust, TLS version. *(MVP)*

**Safe to hand to a model**
- **Read-only by default.** A guard refuses SQL that writes before anything runs, and everything runs in a transaction that is rolled back. This guards against accidents; the database's permissions are the real boundary. *(MVP)*
- **`--check`.** Compile only: syntax errors and unknown columns, with nothing executed, and the input can't switch the check off. *(MVP; hardened in Phase 1)*
- **A write policy per profile.** `deny` (never), `flag` (needs `--allow-write`) or `allow`. *(MVP)*
- **No secrets in the transcript.** `-P` is rejected. Passwords live in Windows Credential Manager (or `SQLCMDPASSWORD`), and a stored password is only ever sent to its own server and login, so a profile can't be repointed to harvest it. *(MVP)*
- **`-o` can't hurt the input.** It refuses to write onto an input file, however it is spelled, and replaces the destination whole or not at all. *(Phase 1)*

**Never stuck**
- **Never interactive.** No prompts: with nothing to run on a console, it prints usage and exits 3. The one exception is human setup, `sqlh profile add`. *(MVP)*
- **Timeouts by default.** 60 s per batch and 15 s to log in; sqlcmd's default of none is wrong for agents. *(MVP)*
- **A reader that stops reading can't hang it.** Output is written from its own thread, so a harness that stalls gets exit 7 after `-t` seconds, and timers and Ctrl+C keep working. A slow reader isn't cut off as long as it takes each 4 KB within `-t`, and an unread stderr can't hold up the response or hang sqlh's own diagnostics. *(Phase 2.3)*
- **Cancelling works, at any point.** Ctrl+C, Ctrl+Break or closing the console stops sqlh while it connects, runs a query, waits on a slow reader or delivers its last lines. It exits 6 and says what became of the statement. A harness that kills sqlh also stops the server's work: measured for waiting, CPU-bound and lock-blocked queries, the server ends the request within about 0.1 s. *(MVP; every console event in Phase 2.4)*
- **Connect failures come back fast, and the same way every time.** One attempt per address: a refused one fails in about 2 s, and one that doesn't answer after `-l` seconds. sqlh never makes the slow, uncancellable Named Pipes attempt to another machine: remote names go over TCP, and an explicit remote pipe is refused with the TCP spelling to use. *(Phase 2.4)*

**Fits how models already work**
- **sqlcmd-compatible.** `-S -d -U -Q -i -v -t`, `GO`, `GO n` and `$(var)`; sqlcmd's formatting flags are accepted and ignored with one note, so a model's habits don't break. *(MVP)*
- **`--help` written for models.** Example-first, about 60 lines, with the discovery tips agent trial 1 showed were missing (`SELECT TOP 0 * FROM x` for a table's columns). *(Phase 2.9)*
- **Multi-line SQL from any shell.** `-i file.sql` works everywhere, and stdin (`-i -`, heredocs) where the shell has it; UTF-8 and UTF-16 input are decoded. *(MVP)*
- **Works the same in cmd.exe, PowerShell and Git Bash.** The `--help` examples run unchanged in each, and a test runs them. A Git Bash path (`/c/dir/f.sql`) that cmd.exe or PowerShell can't find is answered with the Windows spelling to use, and sqlh leaves the console's code page as it found it. *(Phase 2.13)*
- **Piped text in the wrong encoding is refused, with the fix.** Input that isn't valid UTF-8 (cmd.exe at an OEM code page, for example) ends in a usage error naming `chcp 65001` and PowerShell's `$OutputEncoding`; `--help` says how to keep non-ASCII text intact. *(Phase 2.13)*
- **The same answers as SSMS.** Session `SET` options match SSMS, so plans and filtered-index behavior match what the DBA sees. *(MVP)*
- **One exe, no dependencies.** About 2 MB, with no ODBC, .NET or redistributable to install first. *(MVP)*
- **Flat memory for huge values.** A 1 GB `varchar(max)` shown with `--max-cell 500` costs sqlh about 10 MB instead of 2 GB, rows past `--max-rows` aren't kept, and a runaway `$(var)` expansion is refused at 10 MB before it is built, so one careless query can't exhaust the machine the agent runs on. *(Phase 2.7)*

### Planned

- **Schema discovery commands.** `tables`, `describe`, `find`, `def`, `dbs` and `info`: focused answers instead of catalog queries. *(Phase 4)*
- **Profile descriptions and search**, so an agent can pick the right server in one call. *(Phase 4)*
- **Early stop for huge results.** A bounded drain that reports `≥N rows` instead of reading every hidden row, if the owner chooses it. *(Phase 2.6)*
- **Agent session defaults.** `LOCK_TIMEOUT` and `DEADLOCK_PRIORITY LOW`, and perhaps `READ UNCOMMITTED` for read-only runs, so agent queries give way to production work. *(Phase 2.8)*
- **Connection reuse.** An MCP server or a local daemon, so repeated calls skip the connect, with native tools and read/write permissions the harness can gate. *(Phase 5)*
- **A wider read-only allowlist** for DBA workflows (`xp_readerrorlog`, `xp_instance_regread`, showplans) from a categorization of commands, not a longer blocklist. *(Phase 6)*
- **A skill and a plugin** that teach agents the investigation workflow and install it in one step. *(Phase 8)*

## Documents

This README is the human-facing overview. The other documents, [AGENTS.md](AGENTS.md) included, are written mainly for the AI agents that work on the project, so they are denser and more exact than this page. Claude Code imports AGENTS.md through CLAUDE.md.

- [docs/VISION.md](docs/VISION.md): what the product is for, and its priorities
- [docs/PLAN.md](docs/PLAN.md): the current plan of attack
- [docs/DESIGN.md](docs/DESIGN.md): the technical spec (output contract, exit codes, guard, profiles)
- [code-review.md](code-review.md): the architecture review the plan responds to
- [code-review-phase1.md](code-review-phase1.md): the follow-up review of Phase 1

## Local housekeeping

Builds go in the ignored `target/` directory; `cargo clean` frees the disk space, and the next build is slower. `scratch/` and `artifacts/` are ignored too, for experiments and local results. Real server data and connection profiles stay local, and committed examples use synthetic data. Executables will be distributed as GitHub release assets when releases are ready. The other contributor rules are in [AGENTS.md](AGENTS.md).
