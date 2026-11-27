# sqlh

`sqlh.exe` (sqlh: SQL for harnesses) is a SQL Server query tool for AI agents working through a person's DBA or developer questions, in Claude Code and other agent harnesses. It does the job of `sqlcmd`, as a single Windows executable with no runtime dependencies: no ODBC, no OLE DB, no .NET.

What makes it agent-friendly, in brief: compact typed output with hard limits, errors that point at the input line, documented exit codes, no prompts, and read-only by default. More in [Built for agents](#built-for-agents), and the full list in [docs/agent-features.md](docs/agent-features.md).

Status: early (0.x). Releases are signed and tested, and the output format is versioned, but flags and output may still change between minor versions. See [docs/PLAN.md](docs/PLAN.md) for what's being worked on.

**Requirements:**
- Windows x64. Validated on Windows 11.
- SQL Server 2016 SP3 or later. Tested on 2016 SP3, 2017, 2019, 2022 and 2025.
- A SQL login, or Windows authentication (NTLM or Kerberos).
- Connections are encrypted by default. `-C` trusts a self-signed certificate.
- Not yet supported: Azure SQL, Entra ID, macOS and Linux.

## Safety: read this first

Read-only mode **guards against accidents; it is not a security boundary.** Before anything runs, sqlh refuses the SQL it knows writes, and it runs everything inside a transaction that it rolls back. That stops a confused agent from changing data. It can't stop a determined one with the same login: in the middle of a batch, a statement sqlh has no rule for still reaches the server, and some effects survive a rollback.

**Give agents a login with only the permissions they need**, for example `db_datareader` and `VIEW DEFINITION`. The database is the enforcement boundary. A new profile refuses writes altogether; a profile changed to allow them (`--write-policy flag`) still needs `--allow-write` on each run.

For vulnerability reports, see [SECURITY.md](SECURITY.md). Do not post vulnerability details in a public issue.

## Install

1. Download `sqlh-VERSION-windows-x64.zip` and `SHA256SUMS.txt` from the [latest release](https://github.com/sequeltom/sqlh/releases/latest). The zip holds `sqlh.exe`, the licence and the third-party notices.
2. Check the download against the published sums. Compare the hash with the zip's line in `SHA256SUMS.txt`:

   ```powershell
   (Get-FileHash .\sqlh-*-windows-x64.zip -Algorithm SHA256).Hash
   ```
3. Unzip it to a folder of your own, for example `C:\Tools\sqlh`, and add that folder to your `PATH`.
4. Check the signature before you run it: `Get-AuthenticodeSignature .\sqlh.exe` should report `Valid`, because the exe is code-signed. Then check that it runs: `sqlh --version`. A newly released file can still get a Windows SmartScreen prompt until the file builds up a reputation, and company application-control policies may block unknown executables.

Nothing else needs installing.

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

**Command line or MCP?** MCP (the Model Context Protocol) is how agent harnesses add tools. With a frontier model such as Claude, use sqlh as an MCP server: you get native tools, and connections stay open between calls. With a small local model, use the command line: it reads `sqlh --help` once, while MCP's tool definitions (about 4,000–5,000 tokens) go out with every request.

To give a harness such as Claude Code or OpenCode sqlh's commands as tools, add it as an MCP server (list each profile it may use):

```powershell
claude mcp add sqlh -- sqlh mcp --profile dev
```

In OpenCode, add it to `opencode.json`:

```json
{ "mcp": { "sqlh": { "type": "local", "command": ["sqlh", "mcp", "--profile", "dev"] } } }
```

Every tool runs under the read-only guard, and writes are only possible from the command line. [docs/mcp-mode.md](docs/mcp-mode.md) has the details.

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

`sqlh mcp --profile NAME [--profile NAME ...]` runs sqlh as an MCP server for a harness (see [Quick start](#quick-start)). Only the profiles listed can be used, and every tool runs read-only, under the same guard and rollback as the command line:

| Tool | What it does |
|---|---|
| `query` | Runs T-SQL under the read-only guard, as the command line does; it also refuses what could outlast the call on a reused connection |
| `check` | Compiles T-SQL without running it |
| `info`, `databases`, `tables`, `describe`, `find`, `definition`, `deps` | The schema commands above |
| `errorlog` | The error log, as `sqlh errorlog` reads it |
| `activity` | The server's sessions, as `sqlh activity` shows them; it waits for a connection like any call, so when every one is stuck behind a lock, `sqlh activity` on the command line opens its own |

Each tool also takes `profile` (needed when several are listed), `database` (not `errorlog` or `activity`, which read the whole server), `max_rows`, `max_cell`, `timeout` and `format` (`text` or `json`). Connections are kept open between calls, and each call starts on a clean session. Start the server with `--verbose` to see its connections on stderr.

## Built for agents

What `sqlh` does that `sqlcmd` doesn't, because the reader is a model with a limited context, working through a person's questions across several turns. In brief:

- **Output sized for a model's context.** Compact typed text (or json or csv), hard limits on rows, value length and total size, and hidden rows counted, so the agent knows what it didn't see and how to get the rest.
- **A last line that says whether to trust the answer.** `-- ok`, `-- partial` or `-- failed (exit N)`; output without it is incomplete.
- **Errors an agent can act on.** Exit codes with a meaning, errors mapped back to the input's file and line, and refusals and connection errors that say what to do next.
- **Read-only by default.** A guard refuses the SQL it knows writes before anything runs, everything runs in a transaction that is rolled back, and new profiles refuse writes. The database's permissions stay the real boundary.
- **No secrets in the transcript.** No password on the command line; passwords live in Windows Credential Manager and are only ever sent to the server and login they were saved for.
- **Never stuck.** No prompts, timeouts by default, a 10 s limit on waiting for a lock, fast connect failures, and cancelling works at any point; killing sqlh stops the server's work too.
- **The schema in one call.** `describe`, `def`, `deps`, `tables`, `find`, `dbs` and `info` answer what hand-written catalog queries took several calls and guesses to.
- **A DBA's first looks, one call each.** `errorlog` reads whole error-log entries, as far back as asked; `activity` shows who is running and who blocks whom, with nothing to install.
- **An MCP server in the same exe.** `sqlh mcp` gives a harness such as Claude Code sqlh's commands as read-only tools on open connections, so a repeated call takes milliseconds instead of a new login, and each call starts clean.
- **Fits how models already work.** sqlcmd-compatible flags, a `--help` written for models, the same behavior in cmd.exe, PowerShell and Git Bash, and one exe with nothing to install.

The full list, with every safeguard and when it landed, is kept current in [docs/agent-features.md](docs/agent-features.md).

## Documents

This README is the overview. For the details:

- [docs/DESIGN.md](docs/DESIGN.md): the technical spec (output contract, exit codes, the read-only guard, profiles)
- [docs/mcp-mode.md](docs/mcp-mode.md): `sqlh mcp`, its tools and how it keeps connections
- [docs/schema-commands.md](docs/schema-commands.md): the schema and server commands (`describe`, `find`, `deps`, `errorlog`, `activity`, ...)

For contributors: [CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and how to report a problem. The other documents are written mainly for the AI agents that work on the project, so they are denser and more exact than this page: [AGENTS.md](AGENTS.md) (imported by Claude Code through CLAUDE.md), [docs/VISION.md](docs/VISION.md) (what the product is for), [docs/PLAN.md](docs/PLAN.md) (the plan and the owner's decisions) and [docs/command-categories.md](docs/command-categories.md) (the design for sorting commands by risk, built in slices).

## Build from source

You need Rust (via `rustup`; the toolchain is pinned in `rust-toolchain.toml`) and the Visual Studio C++ build tools with a Windows SDK.

```powershell
cargo build --release      # target\release\sqlh.exe, about 3 MB
cargo test                 # no server needed
pwsh scripts/check.ps1     # everything a change must pass; CI runs the same script
```

Releases are built, signed and published by GitHub Actions.

## License

[MIT](LICENSE).
