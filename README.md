# sqlh

`sqlh.exe` is a SQL Server query tool for AI agents (Claude Code and other harnesses) working through a person's DBA or developer questions. It does the job of `sqlcmd`, as a single Windows executable with no runtime dependencies: no ODBC, no OLE DB, no .NET.

What makes it agent-friendly:
- **Compact, typed output** that costs few tokens, with hard limits on rows and value lengths. Hidden rows are counted, not lost.
- **Errors that point at the input line**, and documented exit codes (`sqlh --help`).
- **Never interactive:** it never waits for input. Passwords never appear on the command line.
- **Read-only by default.**

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

## Documents

- [docs/VISION.md](docs/VISION.md): what the product is for, and its priorities
- [docs/PLAN.md](docs/PLAN.md): the current plan of attack
- [docs/DESIGN.md](docs/DESIGN.md): the technical spec (output contract, exit codes, guard, profiles)
- [code-review.md](code-review.md): the architecture review the plan responds to
