# sqlh

`sqlh.exe` (sqlh: SQL for harnesses) is a SQL Server query tool for AI agents working through a person's DBA or developer questions, in Claude Code and other agent harnesses. It does the job of `sqlcmd`, as a single Windows executable with no runtime dependencies: no ODBC, no OLE DB, no .NET.

What makes it agent-friendly, in brief: compact typed output with hard limits, errors that point at the input line, documented exit codes, no prompts, and read-only by default. The full running list is [Built for agents](#built-for-agents).

Status: pre-release. It is built and tested on Windows 11 x64 against SQL Server 2025, and against SQL Server 2016 SP3 for reading. See [docs/PLAN.md](docs/PLAN.md) for what's being worked on.

## Safety: read this first

Read-only mode **guards against accidents; it is not a security boundary.** Before anything runs, sqlh refuses the SQL it knows writes, and it runs everything inside a transaction that it rolls back. That stops a confused agent from changing data. It can't stop a determined one with the same login: in the middle of a batch, a statement sqlh has no rule for still reaches the server, and some effects survive a rollback.

**Give agents a login with only the permissions they need**, for example `db_datareader` and `VIEW DEFINITION`. The database is the enforcement boundary. A new profile refuses writes altogether; a profile changed to allow them (`--write-policy flag`) still needs `--allow-write` on each run.

For vulnerability reports, see [SECURITY.md](SECURITY.md). Do not post vulnerability details in a public issue.

## Build

You need Rust (via `rustup`; the toolchain is pinned in `rust-toolchain.toml`) and the Visual Studio C++ build tools with a Windows SDK.

```powershell
cargo build --release      # target\release\sqlh.exe, about 3 MB
cargo test                 # no server needed
pwsh scripts/check.ps1     # everything a change must pass; CI runs the same script
```

Releases will be built by GitHub Actions: a signed `sqlh.exe` in a zip with this licence and the third-party notices, plus SHA-256 sums.

## Quick start

Save a connection once. The password is asked for with echo off and stored in Windows Credential Manager:

```powershell
sqlh profiles add dev -S myserver -d AppDb -U agent_reader
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

**Command line or MCP?** The command line is built for local and offline models, such as Qwen on a GPU with 24 GB or less: the agent reads `sqlh --help` once, when it needs it, and each call returns compact output. An MCP server's tool definitions go out with every request (about 16 KB of JSON for sqlh's eleven tools, roughly 4,000–5,000 tokens), which a frontier model barely notices and a small one can't spare. With a frontier model, such as Claude, use MCP: native tools, and connections kept open between calls.

To give a harness such as Claude Code or OpenCode sqlh's commands as tools, add it as an MCP server (list each profile it may use):

```powershell
claude mcp add sqlh -- sqlh mcp --profile dev
```

In OpenCode, add it to `opencode.json`:

```json
{ "mcp": { "sqlh": { "type": "local", "command": ["sqlh", "mcp", "--profile", "dev"] } } }
```

The tools only read: writes stay on the command line. [docs/mcp-mode.md](docs/mcp-mode.md) has the details.

## Using it

### Profiles

A profile holds a server, a database, a login and its settings. Its password is kept in Windows Credential Manager, never in a file or on a command line. Profiles are saved in `%USERPROFILE%\.sqlh\profiles.toml`, where your shell and any agent read the same file; profiles from an older sqlh, under `%APPDATA%`, move there by themselves.

```powershell
sqlh profiles add dev -S myserver -d AppDb -U agent_reader --description "dev copy of AppDb"
sqlh profiles add local -S . -d AppDb                 # no -U: Windows authentication
sqlh profiles list                                    # every profile; --search TEXT narrows it
sqlh profiles test dev                                # connects once and reports
sqlh profiles rm dev
```

Without `-U` a profile uses Windows authentication. `profiles add` tests the connection as it saves, and the first profile saved becomes the default, used when a run names no server or profile. Running `profiles add` again for an existing name updates only the options given. Add `-C` for a server with a self-signed certificate, and `--new-password` to be asked for the password again.

### Queries

```powershell
sqlh --profile dev -Q "SELECT TOP 5 * FROM dbo.Orders ORDER BY Id DESC"
sqlh --profile dev -i report.sql -o report.txt       # a script with GO batches, output to a file
sqlh --profile dev --check -i change.sql             # compile only: syntax and unknown columns
sqlh --profile dev --format json -Q "SELECT 1 AS x"
```

Results stop at 100 rows and 500 characters per value unless you raise them (`--max-rows`, `--max-cell`), and a batch times out after 60 seconds (`-t`). The last line always says how the run ended (`-- ok: ...` or `-- failed (exit N): ...`), and the exit code says why (`sqlh --help` lists them).

### Looking at the schema

```powershell
sqlh --profile dev describe dbo.Orders     # columns, keys, indexes, foreign keys, triggers
sqlh --profile dev tables Ord*             # tables and views matching a pattern
sqlh --profile dev find Customer --text    # objects whose name, or with --text whose code, mentions it
sqlh --profile dev def dbo.usp_GetOrders   # a view's or procedure's source
sqlh --profile dev deps dbo.Orders         # what it uses and what uses it
sqlh --profile dev dbs                     # databases;  info: the server, version and connection
sqlh --profile dev errorlog --since 2h --search "Login failed"   # the error log, newest first
sqlh --profile dev activity                # what runs now, and who blocks whom
```

### Writing

Every run is read-only unless both the profile and the run allow writes. A new profile refuses writes altogether. To allow them, change the profile's policy, then ask for writes on each run:

```powershell
sqlh profiles add dev --write-policy flag
sqlh --profile dev --allow-write -Q "UPDATE dbo.Orders SET Status = 'Closed' WHERE Id = 42"
```

Without `--allow-write`, a write is refused before anything runs (exit 5), with a message that says why and what to do instead. Writes to `#temp` tables and table variables are always allowed. The error log and server state (`sp_readerrorlog`, `xp_readerrorlog`, `sp_configure` without a value, `xp_msver`) can be read without writes; whether the login may run them is up to the server.

### Through MCP

`sqlh mcp --profile NAME [--profile NAME ...]` runs sqlh as an MCP server for a harness (see [Quick start](#quick-start)). Only the profiles listed can be used, and every tool reads only:

| Tool | What it does |
|---|---|
| `query` | Runs T-SQL under the read-only guard, as the command line does; it also refuses what could outlast the call on a reused connection |
| `check` | Compiles T-SQL without running it |
| `info`, `databases`, `tables`, `describe`, `find`, `definition`, `deps` | The schema commands above |
| `errorlog` | The error log, as `sqlh errorlog` reads it |
| `activity` | The server's sessions, as `sqlh activity` shows them; it waits for a connection like any call, so when every one is stuck behind a lock, `sqlh activity` on the command line opens its own |

Each tool also takes `profile` (needed when several are listed), `database` (not `errorlog` or `activity`, which read the whole server), `max_rows`, `max_cell`, `timeout` and `format` (`text` or `json`). Connections are kept open between calls, and each call starts on a clean session. Start the server with `--verbose` to see its connections on stderr.

## Built for agents

What `sqlh` does that `sqlcmd` doesn't, because the reader is a model with a limited context, working through a person's questions across several turns. This list is kept current: a feature is added here in the same change as its code ([AGENTS.md](AGENTS.md)). The tags say when it landed (see [docs/PLAN.md](docs/PLAN.md) and [docs/history/plan-history.md](docs/history/plan-history.md)); the spec is [docs/DESIGN.md](docs/DESIGN.md).

**Tally: 57 built, 2 planned.**

### Built

**Output an agent can afford to read**
- **Compact, typed output.** `name:type` headers, tab-separated rows, ISO dates, exact decimals, floats in the shortest form that reads back the same (a half-precision vector's elements at their own precision, so `0.1` stays `0.1`), and `NULL` kept apart from the string `'NULL'` (written `\NULL`). *(MVP; half-precision vectors since 0.1.0)*
- **Hard limits by default.** 100 rows, 500 characters per value, 20 result sets, and 1 MB for the whole response, errors, notes and diagnostics included (`--max-rows`, `--max-cell`, `--max-results`, `--max-output`), and 10 MB of input. Text that doesn't fit is cut with a marker; the status line never is. *(MVP; `--max-output` in Phase 2.2)*
- **Hidden rows are counted; cut values say how much, and how to get the rest.** `-- 100 of 48213 rows shown (--max-rows 100)` and `…[+N]`, so the agent knows what it didn't see and can narrow the query; a run that cut values ends with a note saying it can read one in pieces with `SUBSTRING` or raise the limit, and through MCP a stopped count suggests a `COUNT_BIG(*)`, which the agent can run itself. *(MVP; the advice since agent trial 3, Phase 5.6)*
- **A huge result doesn't make the agent wait, and doesn't silently cost it the next query.** Past `--max-rows`, a read-only run counts the hidden rows briefly, then reports a lower bound (`100 of ≥800000 rows shown`). It counts for a quarter of a second, or for up to 5 s when more statements follow in the same batch, so a modest result over a slow link still lets them run. If even that stops the count, the run says the rest of that batch was skipped, and that putting each query in its own batch (`GO`) keeps them. Counting every row of a big table took seconds to minutes; `--exact-count` (or `exact_count` in an MCP call) still does, and runs that can write always do, so a `COMMIT` is never skipped. *(Phase 2.15; the GO advice since the 2026-10-31 benchmark; the longer count and the MCP argument since the 2026-10-31 client feedback)*
- **A status line that proves the output is complete.** The last line is `-- ok: ...`, `-- partial: ...` (exit 0, but a stopped count kept later statements from running) or `-- failed (exit N): ...`. Without it (exit 7 or 10), the output can't be trusted. A model reading only the last line can't take a partial answer for a whole one. *(MVP; `partial` since the 2026-10-31 client feedback)*
- **Loops don't flood.** Consecutive row counts collapse into `-- 300 statements, 300 rows affected`. *(Phase 2.2)*
- **Data can't pass for metadata.** Control characters and Unicode line breaks are escaped, and a leading `--` in a value is too, so nothing stored in a database can forge an `-- ok:` line. *(Phase 1)*
- **Three formats.** Text (the default, cheapest in tokens), json (one streamed document of ordered events, valid even when cut short) and csv (data only on stdout, metadata on stderr). *(MVP)*
- **A message never lands in the middle of a result.** A PRINT or warning that arrives while a result streams is written after that result, never among its rows, and the output refuses an event out of order instead of writing a broken document. *(Phase 2.12)*
- **Every response says where it ran.** The status line ends with the server, the database and the connect time (`-- ok: 1 batch, 241 ms; db01/master, connect 195 ms`), so an agent sees at once whether it is in the right database. A json document also has the auth method, the encryption and each batch's duration, and starts with its contract version, which `sqlh --version` gives too. *(Phase 2.11)*

**Errors an agent can act on**
- **Errors point at the input.** File and line, mapped back through `GO` batches and `$(var)` substitution; inside a procedure, the procedure and its own line. *(MVP)*
- **Exit codes with a meaning.** 0 ok, 1 SQL error, 2 connect, 3 usage, 4 timeout, 5 blocked, 6 cancelled, 7 output not delivered, 10 internal. Exit 7 means "don't trust what you received", and it is no reason to retry a write blindly. *(MVP; exit 7 in Phase 1)*
- **A result an error cut short says so.** The rows before a SQL error are marked incomplete in every format (`-- 2 rows so far (interrupted)`), so an agent never takes them for the whole answer. *(Phase 2 review)*
- **Refusals say how to fix them.** A blocked write names the statement and the better spelling (`#temp` instead of `##t`, `#t` instead of `[#t]`, `sys.sp_who`) before it suggests `--allow-write`, and a byte order mark you can't see is shown and named. A refusal quotes no more of a name than a name can hold, so a mistake in the input doesn't cost a model its own input back. *(Phase 1; temp table spellings, the mark and the cut since the 2026-11-02 review)*
- **A refusal doesn't call everything a write.** A procedure or DBCC command sqlh doesn't know is refused as just that, so an agent doesn't reach for `--allow-write` to run what may only read. Where a view does the job the refusal names it first (`sys.dm_db_log_info` for `DBCC LOGINFO`), and DDL names its statement (`ALTER EVENT SESSION`, not `ALTER`). A plan hint such as `INNER MERGE JOIN` or `OPTION (MERGE UNION)` isn't refused as a `MERGE`. *(Phase 6, slice 1; the hints since the 2026-11-02 review)*
- **Connection errors name what was tried** (server, port, auth, encryption) and hint at the usual cause: SQL Browser, certificate trust, TLS version, or a Windows too old for the driver's TLS, so an agent doesn't try certificate flags that can't help. *(MVP; the Windows hint since 2026-11-09)*

**Safe to hand to a model**
- **Read-only by default.** A guard refuses the SQL it knows writes before anything runs, and everything runs in a transaction that is rolled back. It goes by lists, so a statement on none of them still reaches the server inside that transaction. This guards against accidents; the database's permissions are the real boundary. *(MVP)*
- **A statement can't hide in how the text is typed.** The guard reads a word as SQL Server does, as measured against the server on its usual collation, so a no-break space pasted from a web page, a line comment ended by a lone carriage return, a keyword glued to a number (`1DELETE`), a letter SQL Server ignores in a word, or a mark it reads as the letter before it doesn't slip a statement past it. That rests on what was measured: it can't rule out a spelling nobody has tried, or what another collation reads differently. A column whose name only looks like a keyword isn't refused for it. *(Phase 6, slice 1)*
- **`--check`.** Compile only: syntax errors and unknown columns, with nothing executed, and the input can't switch the check off by any spelling of the setting that was measured on SQL Server's usual collation. *(MVP; hardened in Phase 1)*
- **A write policy per profile, and new profiles refuse writes.** `deny` (never), `flag` (needs `--allow-write`) or `allow`; `profiles add` makes new profiles `deny`, because an agent in trial 2 added `--allow-write` on its own. Under `deny`, a refusal says the profile doesn't allow writes rather than suggesting a flag that would fail. *(MVP; deny by default and its refusals since Phase 2.16)*
- **No secrets in the transcript.** `-P` is rejected. Passwords live in Windows Credential Manager (or `SQLCMDPASSWORD`), and a stored password is only ever sent to its own server and login (or to a server that server hands the login to, as Always On read-only routing does), so a profile can't be repointed to harvest it. Each stored password remembers the server, login and encryption it was saved for, so a run that meets a profile mid-edit, or one an interrupted edit left behind, refuses rather than send it elsewhere. `$(SQLCMDPASSWORD)` is never filled in from the environment, so no query can print the password sqlh logs in with; other environment variables still resolve, as in sqlcmd. *(MVP; bound to its profile since Phase 3; `$(SQLCMDPASSWORD)` since the 2026-11-02 review)*
- **`-o` can't hurt the input.** It refuses to write onto an input file, however it is spelled, and replaces the destination whole or not at all. *(Phase 1)*
- **Dirty reads only on request.** `--read-uncommitted` lets a read-only run read past writers' locks without waiting, and the output says the rows may be uncommitted. It is refused when writes are allowed, so the flag never puts a write on top of a dirty read. *(Phase 2.8)*
- **SQL that reads dirty says so.** `NOLOCK`, `READUNCOMMITTED` or `SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED` in the SQL are allowed, and the output says the rows may be uncommitted, so an agent doesn't take them for committed data. A column or alias named nolock doesn't set it off. *(Phase 6, slice 1)*
- **A scan of every index in every database is stopped first.** `sys.dm_db_index_physical_stats` with `NULL` for the database, or a literal the server takes the same way such as `0` or `''`, and `'SAMPLED'` or `'DETAILED'` for the mode, even written in binary, is refused, with the rewrite to use, because a timeout would stop the agent's wait, not the I/O already caused on a server that may be struggling. A mode in a variable can't be seen. *(Phase 6, slice 1)*
- **`AI_GENERATE_EMBEDDINGS` is refused.** On SQL Server 2025 it is written as a function, but with an external model defined it sends its text to an outside endpoint, which no rollback can call back. It is refused in read-only runs and through MCP, as `OPENROWSET` is, and `--allow-write` lifts it where a profile allows that. A view over it can't be seen. *(Phase 6, slice 1)*
- **A function that reads from a URL the query names is refused.** `sys.fn_ledger_retrieve_digests_from_url` passes for a read, but the URL can carry what the query read off the server, with nothing a DBA set up in its way. It is refused in read-only runs and through MCP, and `--allow-write` lifts it where a profile allows that. *(Phase 6, slice 1)*
- **Naming an external model inside a call is refused.** `USE MODEL` has no read-only use once `AI_GENERATE_EMBEDDINGS` is refused, and one such call crashed its session on SQL Server 2025 in testing, so a read-only agent can't send it. `USE model`, switching to the model database, still works. *(Phase 6, slice 1)*
- **A `RAISERROR` by message number is refused.** Hundreds of SQL Server's own messages are written to the error log when raised, where a SQL Server Agent alert can page someone or run a job, so a read-only agent can raise only a quoted message; the refusal shows the equivalent, `RAISERROR('%s', 16, 1, @msg)`. *(Phase 6, slice 1)*
- **A temp-table name can't stand in for a real table.** A read-only run may write to its own `#temp` tables, so an alias or a CTE named like one (`... FROM dbo.Orders AS #o`) is refused: a write through it would reach the real table, holding its locks until the rollback. *(Phase 6, slice 1)*
- **Allowed procedures are taken only as Microsoft spells them.** In a case-sensitive database `SP_WHO` can be someone else's procedure, so a read-only agent may call `sp_who`, not `SP_WHO`, and the refusal names the spelling to use. *(Phase 6, slice 1)*
- **An MCP call can't leave state behind that it isn't sure will be cleared.** `sqlh mcp` reuses connections, so it refuses `SET AUTOPILOT`, `SET IDENTITY_INSERT` and opening a key until a test shows the reset clears them, and the refusal sends the agent to the command line, whose session ends with the run. Until then, reads with `DECRYPTBYKEY` and `SET IDENTITY_INSERT #t` work only there. *(Phase 6, slice 1)*
- **The error log and server settings are readable without writes.** `xp_readerrorlog` and `sp_readerrorlog` (with `sp_enumerrorlogs` to list the logs), `xp_instance_regread`, `xp_fixeddrives`, `xp_msver` and `sp_configure`'s listing are allowed in read-only runs and through MCP, so an agent can read the SQL Server error log during an investigation. Each is allowed by its exact name, so `xp_cmdshell` and registry writes stay blocked, and `sp_configure` with a value is refused with a pointer to `sys.configurations`. Whether the login may run each one stays the server's decision. Some of these tell the reader about the host (registry values, the service account, drives); a DBA who doesn't want that can revoke `EXECUTE` on `xp_instance_regread` from public. *(Phase 6, interim step)*
- **A DBA can see what an agent's session may do.** The login's application name is `sqlh`, `sqlh mcp` or `sqlh (writes)`, so `sp_who2` or `sp_WhoIsActive` tells a read-only look from a run that can change things. It's a label the client sends, not proof. *(Phase 6, slice 1)*

**Never stuck**
- **Never interactive.** No prompts: with nothing to run on a console, it prints usage and exits 3. The one exception is human setup, `sqlh profiles add`. *(MVP)*
- **Timeouts by default, and real ones.** 60 s per batch and 15 s to log in; sqlcmd's default of none is wrong for agents. sqlh keeps the batch timeout itself, as elapsed time, so a result that streams faster than it is read, a huge value or thousands of tiny results can't outrun it. *(MVP; elapsed time since the Phase 2 review)*
- **The read-only check can't hang.** Deeply nested or machine-generated SQL is classified in milliseconds, or refused before connecting with what to do (simplify it, split it with `GO`, or `--allow-write` if writes are meant). *(Phase 2 review)*
- **A blocked query fails fast, and production work comes first.** Every session gives up on a lock after 10 s (error 1222; `--lock-timeout` changes it) and is the one chosen when it deadlocks with ordinary work, so an agent gets a clear error instead of waiting out the query timeout, and a deadlock costs the agent's query, not the application's. *(Phase 2.8)*
- **A reader that stops reading can't hang it.** Output is written from its own thread, so a harness that stalls gets exit 7 after `-t` seconds, and timers and Ctrl+C keep working. A slow reader isn't cut off as long as it takes each 4 KB within `-t`, and an unread stderr can't hold up the response or hang sqlh's own diagnostics. *(Phase 2.3)*
- **Cancelling works, at any point.** Ctrl+C, Ctrl+Break or closing the console stops sqlh while it connects, runs a query, waits on a slow reader or delivers its last lines. It exits 6 and says what became of the statement. A harness that kills sqlh also stops the server's work: measured for waiting, CPU-bound and lock-blocked queries, the server ends the request within about 0.1 s. *(MVP; every console event in Phase 2.4)*
- **The MCP server can't be hung by its client either.** A cancel reaches the query even while the harness has stopped reading answers; a harness that stops reading while its answers pile up is treated as gone after the query timeout (a minute when there is none), so the server never waits on it forever; and Ctrl+C or closing the console still ends the server within a few seconds while it is shutting down. *(Phase 5 review)*
- **Connect failures come back fast, and the same way every time.** One attempt per address: a refused one fails in about 2 s, and one that doesn't answer after `-l` seconds. sqlh never makes the slow, uncancellable Named Pipes attempt to another machine: remote names go over TCP, and an explicit remote pipe is refused with the TCP spelling to use. *(Phase 2.4)*

**Fits how models already work**
- **sqlcmd-compatible.** `-S -d -U -Q -i -v -t`, `GO`, `GO n` and `$(var)`; sqlcmd's formatting flags are accepted and ignored with one note, so a model's habits don't break. *(MVP)*
- **`--help` written for models.** Example-first, about 60 lines, with the discovery tips agent trial 1 showed were missing (`describe` for a table's or view's columns), and one for the error log, the first place a DBA looks. *(Phase 2.9; `describe` in Phase 4; `errorlog` and `activity` in Phase 6, slice 2)*
- **It says which build it is.** `sqlh --version`, named on `--help`'s first line, and the MCP server's info give the release (`0.1.0`), a pre-release (`0.1.0-dev.5`) or `-local` for a build made anywhere else, so an agent or a person can tell which exe answered. Windows' file properties show the same version. *(versioning, 2026-10-31)*
- **The schema in one call.** `describe dbo.Orders` gives a table's columns, keys, indexes, foreign keys both ways, checks and triggers (or a procedure's parameters, or a system view's columns); `def` prints a view's or procedure's source; `deps` lists what an object uses and what uses it, including foreign keys, synonyms and calls resolved only at run time; `tables`, `find` (names, or with `--text` module bodies), `dbs` and `info` answer the rest. Each is a few labeled sections with empty ones folded into one line, where hand-written catalog queries took agent trial 1 five calls and an invented view name. Any name it prints can be passed straight back (odd names come bracketed), and a cut definition says what was left out and which limit cut it. *(Phase 4)*
- **It says what the login may read on the server.** `info` (and `profiles test`) ends with `server_permissions`: `sysadmin`, the server-wide permissions that decide whether server views and the error log can be read (`VIEW SERVER STATE`, `VIEW ANY ERROR LOG` and the like), or `none`. An agent knows before it tries whether a server-wide read will be refused, and which grant to ask a DBA for. *(Phase 6, slice 2)*
- **The error log in one call, whole entries, as far back as asked.** `errorlog --since 2h --search "Login failed"` gives the matching lines newest first together with the lines logged at the same moment by the same source (a failed login's `Error: 18456` line comes with its reason; during a startup, unrelated lines come along too), and reads back into older log files when the window needs them, so "anything since last night?" isn't answered from a log that began at this morning's restart. `--agent` reads the SQL Server Agent log, where a search returns just the matching lines, since each Agent message is one line. A login without the permission is told which one to ask a DBA for. *(Phase 6, slice 2)*
- **Who is running and who blocks whom, in one call.** `activity` lists the sessions that are running, hold a transaction open, are blocked or block another, from one snapshot of the server, blocking chains first: each session's blocker, how many wait on it and how deep in its chain it sits, its wait and resource, durations in milliseconds, its statement, and a head blocker's last batch, each on one line so the cut keeps words, not indentation. It answers "what is blocking?" the way `sp_WhoIsActive` would, with nothing to install. A login that can't see other sessions is told which permission to ask a DBA for, rather than shown an empty list that reads as an idle server. *(Phase 6, slice 2)*
- **Open connections, as native tools.** `sqlh mcp --profile NAME` is an MCP server: a harness such as Claude Code gets sqlh's commands as read-only tools, and an agent's repeated calls reuse an open connection instead of paying the 150–250 ms connect and login each time (on the dev LAN, a `SELECT 1` took 223 ms the first time and 9–11 ms on each call after it). Every call starts clean, so nothing one call does, not even a changed isolation level, leaks into the next. If a kept connection died while idle, the call runs again on a new one instead of failing, and the time that took is reported as connect time, not as the query's. A call with a wrong argument gets an ordinary result the model can read and correct, and where the command line's advice would name a flag, the advice in results names the call's own arguments or the profile's settings instead, or says that a limit is set where the server is started. It is part of the same exe, and its code runs only when a harness starts it: using sqlh only as a command-line tool costs nothing for it at run time (no startup time, no memory, no open connections). *(Phase 5.3)*
- **Multi-line SQL from any shell.** `-i file.sql` works everywhere, and stdin (`-i -`, heredocs) where the shell has it; UTF-8 and UTF-16 input are decoded. *(MVP)*
- **Works the same in cmd.exe, PowerShell and Git Bash.** The `--help` examples run unchanged in each, and a test runs them. A Git Bash path (`/c/dir/f.sql`) that cmd.exe or PowerShell can't find is answered with the Windows spelling to use, and sqlh leaves the console's code page as it found it. A path from Explorer's "Copy as path" works as is after `-i` or `-o`. *(Phase 2.13)*
- **Piped text in the wrong encoding is refused, with the fix.** Input that isn't valid UTF-8 (cmd.exe at an OEM code page, for example) ends in a usage error naming `chcp 65001` and PowerShell's `$OutputEncoding`; `--help` says how to keep non-ASCII text intact. So does UTF-16 without a BOM, or UTF-32, instead of a refusal that blames the SQL. The extra byte order marks Windows PowerShell 5.1 puts in front of piped SQL when its output is captured are dropped, so the documented setup works there too. *(Phase 2.13; wide text since the 2026-11-02 review; repeated marks since the 0.1.0 release check)*
- **The same answers as SSMS.** Session `SET` options match SSMS, so plans and filtered-index behavior match what the DBA sees. *(MVP)*
- **Profiles say what they're for, and can be searched.** Each profile can carry a one-line description (`sqlh profiles add dev --description "prod reporting replica, read-only"`), and `sqlh profiles list --search reporting` lists the profiles whose name or description matches, so an agent picks the right server in one call. *(Phase 4.1–4.2)*
- **One exe, no dependencies.** About 3 MB, with no ODBC, .NET or redistributable to install first. *(MVP)*
- **Flat memory for huge values.** A 1 GB `varchar(max)` shown with `--max-cell 500` costs sqlh about 10 MB instead of 2 GB, rows past `--max-rows` aren't kept, a runaway `$(var)` expansion is refused at 10 MB before it is built, and the read-only guard checks a 10 MB script in under 60 MB whatever it holds, so one careless query can't exhaust the machine the agent runs on. The old `text`, `ntext` and `image` types are the exception: those values are still read whole. *(Phase 2.7; the guard in 2.14)*

### Planned

- **Commands sorted by what they risk**, so DBA workflows (showplans, community procedures such as `sp_Blitz`) run where it is safe, from a categorization of commands, not a longer blocklist. What `sp_WhoIsActive` is mostly run for is built: `activity`, above. *(Phase 6)*
- **A skill and a plugin** that teach agents the investigation workflow and install it in one step. *(Phase 8)*

## Documents

This README is the human-facing overview. The other documents, [AGENTS.md](AGENTS.md) included, are written mainly for the AI agents that work on the project, so they are denser and more exact than this page. Claude Code imports AGENTS.md through CLAUDE.md.

- [docs/VISION.md](docs/VISION.md): what the product is for, and its priorities
- [docs/PLAN.md](docs/PLAN.md): the current plan of attack
- [docs/DESIGN.md](docs/DESIGN.md): the technical spec (output contract, exit codes, guard, profiles)
- [docs/mcp-mode.md](docs/mcp-mode.md): `sqlh mcp`, its tools and how it keeps connections
- [docs/schema-commands.md](docs/schema-commands.md): the schema commands (`describe`, `find`, `deps`, ...)
- [docs/command-categories.md](docs/command-categories.md): the decided design for sorting commands by risk (Phase 6, built in slices: two so far)
- [CONTRIBUTING.md](CONTRIBUTING.md): contribution, build and test guidance

## License

[MIT](LICENSE).

## Local housekeeping

Builds go in the ignored `target/` directory; `cargo clean` frees the disk space, and the next build is slower. `scratch/` and `artifacts/` are ignored too, for experiments and local results. Real server data and connection profiles stay local, and committed examples use synthetic data. Executables will be distributed as GitHub release assets when releases are ready. The other contributor rules are in [AGENTS.md](AGENTS.md).
