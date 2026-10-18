# sqlh

`sqlh.exe` is a SQL Server query tool for AI agents (Claude Code and other harnesses) working through a person's DBA or developer questions. It does the job of `sqlcmd`, as a single Windows executable with no runtime dependencies: no ODBC, no OLE DB, no .NET.

What makes it agent-friendly, in brief: compact typed output with hard limits, errors that point at the input line, documented exit codes, no prompts, and read-only by default. The full running list is [Built for agents](#built-for-agents).

Status: pre-release. It is built and tested on Windows 11 x64 against SQL Server 2025. See [docs/PLAN.md](docs/PLAN.md) for what's being worked on.

## Safety: read this first

Read-only mode **guards against accidents; it is not a security boundary.** Before anything runs, sqlh refuses SQL that writes, and it runs everything inside a transaction that it rolls back. That stops a confused agent from changing data. It can't stop a determined one with the same login, and some effects survive a rollback.

**Give agents a login with only the permissions they need**, for example `db_datareader` and `VIEW DEFINITION`. The database is the enforcement boundary. A new profile refuses writes altogether; a profile changed to allow them (`--write-policy flag`) still needs `--allow-write` on each run.

## Build

You need Rust (via `rustup`; the toolchain is pinned in `rust-toolchain.toml`) and the Visual Studio C++ build tools with a Windows SDK.

```powershell
cargo build --release      # target\release\sqlh.exe, under 3 MB
cargo test                 # no server needed
pwsh scripts/check.ps1     # everything a change must pass; CI runs the same script
```

Releases will be built by GitHub Actions: a signed `sqlh.exe` in a zip with this licence and the third-party notices, plus SHA-256 sums.

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
-- ok: 1 batch, 210 ms; myserver/AppDb, connect 160 ms
```

`--format json` and `--format csv` are also available. `sqlh --help` is the full reference; it's written to be read by models.

To give Claude Code sqlh's commands as tools, with connections kept open between calls, add it as an MCP server (list each profile it may use):

```powershell
claude mcp add sqlh -- sqlh mcp --profile dev
```

The tools only read: writes stay on the command line. [docs/mcp-mode.md](docs/mcp-mode.md) has the details.

## Built for agents

What `sqlh` does that `sqlcmd` doesn't, because the reader is a model with a limited context, working through a person's questions across several turns. This list is kept current: a feature is added here in the same change as its code ([AGENTS.md](AGENTS.md)). The tags say when it landed (see [docs/PLAN.md](docs/PLAN.md)); the spec is [docs/DESIGN.md](docs/DESIGN.md).

**Tally: 39 built, 2 planned.**

### Built

**Output an agent can afford to read**
- **Compact, typed output.** `name:type` headers, tab-separated rows, ISO dates, exact decimals, and `NULL` kept apart from the string `'NULL'` (written `\NULL`). *(MVP)*
- **Hard limits by default.** 100 rows, 500 characters per value, 20 result sets, and 1 MB for the whole response, errors, notes and diagnostics included (`--max-rows`, `--max-cell`, `--max-results`, `--max-output`), and 10 MB of input. Text that doesn't fit is cut with a marker; the status line never is. *(MVP; `--max-output` in Phase 2.2)*
- **Hidden rows are counted; cut values say how much.** `-- 100 of 48213 rows shown (--max-rows 100)` and `…[+N]`, so the agent knows what it didn't see and can narrow the query. *(MVP)*
- **A huge result doesn't make the agent wait.** Past `--max-rows`, a read-only run counts the hidden rows for about a quarter of a second, then reports a lower bound (`100 of ≥800000 rows shown`) and says that the rest of that batch was skipped. Counting every row of a big table took seconds to minutes; `--exact-count` still does, and runs that can write always do, so a `COMMIT` is never skipped. *(Phase 2.15)*
- **A status line that proves the output is complete.** The last line is `-- ok: ...` or `-- failed (exit N): ...`. Without it (exit 7 or 10), the output can't be trusted. *(MVP)*
- **Loops don't flood.** Consecutive row counts collapse into `-- 300 statements, 300 rows affected`. *(Phase 2.2)*
- **Data can't pass for metadata.** Control characters and Unicode line breaks are escaped, and a leading `--` in a value is too, so nothing stored in a database can forge an `-- ok:` line. *(Phase 1)*
- **Three formats.** Text (the default, cheapest in tokens), json (one streamed document of ordered events, valid even when cut short) and csv (data only on stdout, metadata on stderr). *(MVP)*
- **A message never lands in the middle of a result.** A PRINT or warning that arrives while a result streams is written after that result, never among its rows, and the output refuses an event out of order instead of writing a broken document. *(Phase 2.12)*
- **Every response says where it ran.** The status line ends with the server, the database and the connect time (`-- ok: 1 batch, 241 ms; db01/master, connect 195 ms`), so an agent sees at once whether it is in the right database. A json document also has the auth method, the encryption and each batch's duration, and starts with its contract version, which `sqlh --version` gives too. *(Phase 2.11)*

**Errors an agent can act on**
- **Errors point at the input.** File and line, mapped back through `GO` batches and `$(var)` substitution; inside a procedure, the procedure and its own line. *(MVP)*
- **Exit codes with a meaning.** 0 ok, 1 SQL error, 2 connect, 3 usage, 4 timeout, 5 blocked, 6 cancelled, 7 output not delivered, 10 internal. Exit 7 means "don't trust what you received", and it is no reason to retry a write blindly. *(MVP; exit 7 in Phase 1)*
- **A result an error cut short says so.** The rows before a SQL error are marked incomplete in every format (`-- 2 rows so far (interrupted)`), so an agent never takes them for the whole answer. *(Phase 2 review)*
- **Refusals say how to fix them.** A blocked write names the statement and the better spelling (`#temp` instead of `##t`, `sys.sp_who`) before it suggests `--allow-write`. *(Phase 1)*
- **Connection errors name what was tried** (server, port, auth, encryption) and hint at the usual cause: SQL Browser, certificate trust, TLS version. *(MVP)*

**Safe to hand to a model**
- **Read-only by default.** A guard refuses SQL that writes before anything runs, and everything runs in a transaction that is rolled back. This guards against accidents; the database's permissions are the real boundary. *(MVP)*
- **`--check`.** Compile only: syntax errors and unknown columns, with nothing executed, and the input can't switch the check off. *(MVP; hardened in Phase 1)*
- **A write policy per profile, and new profiles refuse writes.** `deny` (never), `flag` (needs `--allow-write`) or `allow`; `profile add` makes new profiles `deny`, because an agent in trial 2 added `--allow-write` on its own. Under `deny`, a refusal says the profile doesn't allow writes rather than suggesting a flag that would fail. *(MVP; deny by default and its refusals since Phase 2.16)*
- **No secrets in the transcript.** `-P` is rejected. Passwords live in Windows Credential Manager (or `SQLCMDPASSWORD`), and a stored password is only ever sent to its own server and login, so a profile can't be repointed to harvest it. Each stored password remembers the server, login and encryption it was saved for, so a run that meets a profile mid-edit, or one an interrupted edit left behind, refuses rather than send it elsewhere. *(MVP; bound to its profile since Phase 3)*
- **`-o` can't hurt the input.** It refuses to write onto an input file, however it is spelled, and replaces the destination whole or not at all. *(Phase 1)*
- **Dirty reads only on request.** `--read-uncommitted` lets a read-only run read past writers' locks without waiting, and the output says the rows may be uncommitted. It is refused when writes are allowed, so no write is ever based on a dirty read. *(Phase 2.8)*

**Never stuck**
- **Never interactive.** No prompts: with nothing to run on a console, it prints usage and exits 3. The one exception is human setup, `sqlh profile add`. *(MVP)*
- **Timeouts by default, and real ones.** 60 s per batch and 15 s to log in; sqlcmd's default of none is wrong for agents. sqlh keeps the batch timeout itself, as elapsed time, so a result that streams faster than it is read, a huge value or thousands of tiny results can't outrun it. *(MVP; elapsed time since the Phase 2 review)*
- **The read-only check can't hang.** Deeply nested or machine-generated SQL is classified in milliseconds, or refused before connecting with what to do (simplify it, split it with `GO`, or `--allow-write` if writes are meant). *(Phase 2 review)*
- **A blocked query fails fast, and production work comes first.** Every session gives up on a lock after 10 s (error 1222; `--lock-timeout` changes it) and is the one chosen when it deadlocks with ordinary work, so an agent gets a clear error instead of waiting out the query timeout, and a deadlock costs the agent's query, not the application's. *(Phase 2.8)*
- **A reader that stops reading can't hang it.** Output is written from its own thread, so a harness that stalls gets exit 7 after `-t` seconds, and timers and Ctrl+C keep working. A slow reader isn't cut off as long as it takes each 4 KB within `-t`, and an unread stderr can't hold up the response or hang sqlh's own diagnostics. *(Phase 2.3)*
- **Cancelling works, at any point.** Ctrl+C, Ctrl+Break or closing the console stops sqlh while it connects, runs a query, waits on a slow reader or delivers its last lines. It exits 6 and says what became of the statement. A harness that kills sqlh also stops the server's work: measured for waiting, CPU-bound and lock-blocked queries, the server ends the request within about 0.1 s. *(MVP; every console event in Phase 2.4)*
- **Connect failures come back fast, and the same way every time.** One attempt per address: a refused one fails in about 2 s, and one that doesn't answer after `-l` seconds. sqlh never makes the slow, uncancellable Named Pipes attempt to another machine: remote names go over TCP, and an explicit remote pipe is refused with the TCP spelling to use. *(Phase 2.4)*

**Fits how models already work**
- **sqlcmd-compatible.** `-S -d -U -Q -i -v -t`, `GO`, `GO n` and `$(var)`; sqlcmd's formatting flags are accepted and ignored with one note, so a model's habits don't break. *(MVP)*
- **`--help` written for models.** Example-first, about 60 lines, with the discovery tips agent trial 1 showed were missing (`describe` for a table's or view's columns). *(Phase 2.9; `describe` in Phase 4)*
- **The schema in one call.** `describe dbo.Orders` gives a table's columns, keys, indexes, foreign keys both ways, checks and triggers (or a procedure's parameters, or a system view's columns); `def` prints a view's or procedure's source; `deps` lists what an object uses and what uses it, including foreign keys, synonyms and calls resolved only at run time; `tables`, `find` (names, or with `--text` module bodies), `dbs` and `info` answer the rest. Each is a few labeled sections with empty ones folded into one line, where hand-written catalog queries took agent trial 1 five calls and an invented view name. Any name it prints can be passed straight back (odd names come bracketed), and a cut definition says what was left out and which limit cut it. *(Phase 4)*
- **Open connections, as native tools.** `sqlh mcp --profile NAME` is an MCP server: a harness such as Claude Code gets sqlh's commands as read-only tools, and an agent's repeated calls reuse an open connection instead of paying the 150–250 ms connect and login each time (on the dev LAN, a `SELECT 1` took 223 ms the first time and 9–11 ms on each call after it). Every call starts clean, so nothing one call does, not even a changed isolation level, leaks into the next. If a kept connection died while idle, the call runs again on a new one instead of failing, and the time that took is reported as connect time, not as the query's. A call with a wrong argument gets an ordinary result the model can read and correct, and where the command line's advice would name a flag, the advice in results names the call's own arguments or the profile's settings instead, or says that a limit is set where the server is started. It is part of the same exe, and its code runs only when a harness starts it: using sqlh only as a command-line tool costs nothing for it at run time (no startup time, no memory, no open connections). *(Phase 5.3)*
- **Multi-line SQL from any shell.** `-i file.sql` works everywhere, and stdin (`-i -`, heredocs) where the shell has it; UTF-8 and UTF-16 input are decoded. *(MVP)*
- **Works the same in cmd.exe, PowerShell and Git Bash.** The `--help` examples run unchanged in each, and a test runs them. A Git Bash path (`/c/dir/f.sql`) that cmd.exe or PowerShell can't find is answered with the Windows spelling to use, and sqlh leaves the console's code page as it found it. A path from Explorer's "Copy as path" works as is after `-i` or `-o`. *(Phase 2.13)*
- **Piped text in the wrong encoding is refused, with the fix.** Input that isn't valid UTF-8 (cmd.exe at an OEM code page, for example) ends in a usage error naming `chcp 65001` and PowerShell's `$OutputEncoding`; `--help` says how to keep non-ASCII text intact. *(Phase 2.13)*
- **The same answers as SSMS.** Session `SET` options match SSMS, so plans and filtered-index behavior match what the DBA sees. *(MVP)*
- **Profiles say what they're for, and can be searched.** Each profile can carry a one-line description (`sqlh profile add dev --description "prod reporting replica, read-only"`), and `sqlh profile list --search reporting` lists the profiles whose name or description matches, so an agent picks the right server in one call. *(Phase 4.1–4.2)*
- **One exe, no dependencies.** Under 3 MB, with no ODBC, .NET or redistributable to install first. *(MVP)*
- **Flat memory for huge values.** A 1 GB `varchar(max)` shown with `--max-cell 500` costs sqlh about 10 MB instead of 2 GB, rows past `--max-rows` aren't kept, a runaway `$(var)` expansion is refused at 10 MB before it is built, and the read-only guard checks a 10 MB script in under 60 MB whatever it holds, so one careless query can't exhaust the machine the agent runs on. The old `text`, `ntext` and `image` types are the exception: those values are still read whole. *(Phase 2.7; the guard in 2.14)*

### Planned

- **A wider read-only allowlist** for DBA workflows (`xp_readerrorlog`, `xp_instance_regread`, showplans) from a categorization of commands, not a longer blocklist. *(Phase 6)*
- **A skill and a plugin** that teach agents the investigation workflow and install it in one step. *(Phase 8)*

## Documents

This README is the human-facing overview. The other documents, [AGENTS.md](AGENTS.md) included, are written mainly for the AI agents that work on the project, so they are denser and more exact than this page. Claude Code imports AGENTS.md through CLAUDE.md.

- [docs/VISION.md](docs/VISION.md): what the product is for, and its priorities
- [docs/PLAN.md](docs/PLAN.md): the current plan of attack
- [docs/DESIGN.md](docs/DESIGN.md): the technical spec (output contract, exit codes, guard, profiles)

## License

[MIT](LICENSE).

## Local housekeeping

Builds go in the ignored `target/` directory; `cargo clean` frees the disk space, and the next build is slower. `scratch/` and `artifacts/` are ignored too, for experiments and local results. Real server data and connection profiles stay local, and committed examples use synthetic data. Executables will be distributed as GitHub release assets when releases are ready. The other contributor rules are in [AGENTS.md](AGENTS.md).
