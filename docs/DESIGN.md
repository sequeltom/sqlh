# sqlharness design

Status: draft, pre-code (started 2026-10-09). Rows marked **Decided** in §2 are the owner's calls. Everything else is a proposal and open to change.

## 1. Purpose

Agents such as Claude Code need to query SQL Server. `sqlcmd` works, but it fights them. The classic version needs the ODBC driver installed. Its output is padded for humans. It can block on prompts. It wants the password on the command line. It has no row or size limits, prints NULL ambiguously, and reports errors without enough context to fix them.

sqlharness (`sqlh.exe`, working name) is a single exe with no dependencies that runs T-SQL and returns compact, unambiguous, size-limited output. It never waits for input, reports errors that point at the failing line, and refuses writes unless asked.

**Non-goals:** an interactive REPL, the full sqlcmd scripting language (`:r`, `:connect`, `:!!`), a GUI, and non-Windows builds (they may come for free later).

## 2. Decisions

| Topic | Decision |
|---|---|
| Language | Rust, stable, with the toolchain pinned (currently 1.99.0). **Decided** |
| TDS driver | Microsoft `mssql-tds` (MIT, preview). **Decided** |
| Default output | Compact typed text (TSV-like), §5.1. **Decided** |
| Writes | Read-only by default, §7. **Decided** |
| Auth | SQL login and Windows integrated (SSPI). No Entra ID for now. **Decided** |
| sqlcmd compatibility | Core flags, `GO` and `$(var)`. Agent-specific features get long flags. **Decided** |
| Server targets | SQL Server 2012 through 2025, Azure SQL DB and Managed Instance. **Decided** |
| Test server | The owner's existing remote server. Its details stay out of the repo. **Decided** |
| Modes | One-shot CLI first, then an MCP stdio server, with a persistent session later. The owner had no preference. |

### Why mssql-tds

Alternatives considered:
- **go-mssqldb:** mature and Microsoft-maintained, but produces an 8–10 MB exe.
- **tiberius-ng:** a community fork of the abandoned tiberius driver.
- **C++ on the legacy in-box "SQL Server" ODBC driver:** deprecated, has no TDS 8, and handles modern types poorly.
- **.NET Framework SqlClient:** ships with Windows, but pays the CLR startup cost.

`mssql-tds` is Microsoft's new Rust TDS core, the base for their Python driver core and a new ODBC driver. On Windows it uses SChannel for TLS, so there is no OpenSSL, and SSPI is built in.

The risk is that it is a preview (0.x): its API may churn and upstream takes no outside PRs. Mitigations:
- Pin an exact version.
- Keep all driver use inside `src/db/`.
- Keep tiberius-ng as the fallback.

Milestone 0 is a spike meant to find any deal-breakers early.

## 3. Platform and build

- **Target:** `x86_64-pc-windows-msvc`. Rust's tier-1 MSVC targets need Windows 10 / Server 2016 or newer, which matches our floor. ARM64 may come later.
- **Static CRT:** set `-C target-feature=+crt-static` in `.cargo/config.toml`. Without it the exe needs `vcruntime140.dll`, which isn't on a clean server.
- **Allowed imports:** only DLLs that ship with Windows (kernel32, advapi32, secur32/sspicli, crypt32, ncrypt, bcrypt, ws2_32, ntdll, and similar). Check with `dumpbin /dependents`.
- **Release profile:** `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"`.
- **TLS:** SChannel, so Server 2016 tops out at TLS 1.2 (TLS 1.3 needs Server 2022 or Windows 11). That is enough for SQL Server 2016+, Azure SQL, and TDS 8 strict encryption.

## 4. CLI

### 4.1 Invocation

```
sqlh [conn] [opts] -Q "<sql>"            run, print, exit
sqlh [conn] [opts] -i a.sql [-i b.sql]   run files in order; "-i -" reads stdin
<sql> | sqlh [conn] [opts]               piped stdin when there's no -Q or -i
sqlh [conn] <subcommand> ...             tables, describe, find, def, dbs, info (M2)
sqlh profile add|list|test|rm ...        human setup; the only command that may prompt
sqlh mcp [conn]                          MCP stdio server (M3)
```

With no SQL source and stdin attached to a console, `sqlh` prints a short usage message and exits 3. It never waits.

Agents should send multi-line SQL on stdin rather than quoting it in `-Q`. In bash, that means a heredoc:

```bash
sqlh --profile dev <<'SQL'
SELECT TOP 5 * FROM dbo.Orders ORDER BY OrderDate DESC;
SQL
```

PowerShell can pipe a here-string. Windows PowerShell 5.1 turns non-ASCII text piped to a native exe into `?` unless `$OutputEncoding` is UTF-8, so `-i file.sql` is safer there.

Input is decoded as UTF-8. A BOM is honored, including UTF-16LE (older SSMS saves files that way).

### 4.2 Flags

Precedence is: flags, then the profile, then `SQLCMD*` environment variables, then defaults.

The profile is chosen by `--profile`, then by `SQLH_PROFILE`. Failing both, the file's `default` profile applies, but only when no server was given by `-S` or `SQLCMDSERVER`; giving a server means you mean a different one.

These flags are sqlcmd-compatible:

| Flag | Meaning |
|---|---|
| `-S server` | `host`, `host\instance`, `host,port`, with an optional `tcp:` prefix. Env: `SQLCMDSERVER`. |
| `-d db` | Database. Env: `SQLCMDDBNAME`. |
| `-E` | Windows integrated auth. This is the default when there is no `-U`, same as sqlcmd. |
| `-U user` | SQL login. The password comes from the profile's credential or `SQLCMDPASSWORD`. Env: `SQLCMDUSER`. |
| `-P` | **Rejected** with exit 3 and a message pointing to profiles or `SQLCMDPASSWORD`. Command lines end up in agent transcripts and process lists. |
| `-Q sql` | SQL text to run. `-q` is treated as `-Q`, because there is no interactive mode. |
| `-i file` | Input file. Repeatable. |
| `-o file` | Write output to a file (UTF-8). |
| `-v n=v ...` | Values for `$(n)` substitution. Environment variables also resolve, as in sqlcmd. |
| `-x` | Turn off `$(var)` substitution. |
| `-l sec` | Login timeout covering TCP, TLS and login. Default 15. |
| `-t sec` | Query timeout per batch. Default 60; 0 means none. sqlcmd's default of none is wrong for agents. |
| `-b` | Accepted. Stopping at the first failing batch is already the default. |
| `-c term` | Batch terminator. Default `GO`. |
| `-C` | Trust the server certificate. |
| `-N[s\|m\|o]` | Encryption: strict, mandatory or optional. Default mandatory. |
| `-K ReadOnly` | Application intent, for routing to AG readable secondaries. |
| `-M` | Multi-subnet failover, if the driver supports it. |
| `-h -s -W -w -y -Y -e -r -m` | sqlcmd formatting flags. Accepted and ignored, with one `-- note:` line, so models' sqlcmd habits don't break. |

These long flags are our own:

| Flag | Meaning |
|---|---|
| `--profile NAME` | Connection profile (§8). Env: `SQLH_PROFILE`. |
| `--format text\|json\|csv` | Output format (§5). Default `text`. `md` is planned. |
| `--max-rows N` | Rows shown per result set. Default 100; 0 means unlimited. |
| `--max-cell N` | Characters shown per cell. Default 500; 0 means unlimited. |
| `--max-results N` | Result sets shown per run. Default 20. The rest are counted, not printed. Messages are capped at 100. |
| `--allow-write` | Turn off the read-only guard, if the profile's write policy allows it (§7). |
| `--check` | Compile only: `SET NOEXEC ON`, executes nothing. Catches syntax errors and unknown columns of existing tables. Missing tables are only reported when a query runs, because SQL Server defers their resolution. A later version could add `sp_describe_first_result_set` per statement. |
| `--continue-on-error` | Keep running later batches after a failed one. |
| `--stats` | Turn on `SET STATISTICS IO, TIME` and print a summary instead of the raw messages (M3). |
| `--verbose` | Diagnostics to stderr. Never includes secrets. |
| `--help`, `--version` | `--help` is read by models: example-first and under about 60 lines. |

### 4.3 Execution semantics

- **Batch splitting.** A line holding only the terminator splits batches. The match is case-insensitive and allows surrounding whitespace, an optional `GO n` repeat count, and a trailing `--` comment. Terminators inside string literals and block comments don't count, which is stricter and more correct than sqlcmd.
- **Variables.** `$(var)` is substituted before splitting. An unknown variable is a usage error (exit 3).
- **Fail fast.** The whole input is read, split and guard-checked (§7) before anything runs.
- **Execution.** Batches run in order on one connection. Every error a batch raises is reported. After a batch with an error of severity 11 or higher, later batches are skipped unless `--continue-on-error` is set.
- **Session SET options** match SSMS: `ANSI_NULLS`, `ANSI_PADDING`, `ANSI_WARNINGS`, `ARITHABORT`, `CONCAT_NULL_YIELDS_NULL` and `QUOTED_IDENTIFIER` ON, and `NUMERIC_ROUNDABORT` OFF. That way filtered and computed-column indexes behave as people expect. (§14 covers the ARITHABORT question.)
- **Error line numbers** are mapped back to the original input: a file and line, not just a line within the batch.
- **Query timeout** sends a TDS attention, reports `-- error: query timeout after 60 s (batch 2)` and exits 4.
- **Ctrl+C, Ctrl+Break or console close** sends an attention, waits up to 2 s for the acknowledgment, then exits 6. This matters because harnesses kill tools that run long.

### 4.4 Output limits

These keep a careless query from flooding an agent's context:

- **Rows.** After `--max-rows`, rows stop being formatted but are still read and counted. Later statements still run, and the footer gives an exact total: `-- 100 of 48213 rows shown (limit 100)`. The query timeout still applies.
- **Cells.** A cell longer than `--max-cell` is cut at a character boundary and ends with `…[+N]`, where N is the number of characters left out. A binary value shows at most `--max-cell / 2` bytes and ends with `…[+N bytes]`. The result set's footer counts the truncated cells.
- **Result sets and messages.** Past their caps (for example, a `WHILE` loop that selects or prints), output is counted, not printed.

## 5. Output formats

All formats stream. None holds a whole result set in memory.

### 5.1 text (default)

```
-- result 1
id:int	name:nvarchar(50)	created:datetime2(7)
1	Alice	2024-01-05T09:12:00.0000000
2	Bob	2024-02-11T14:30:00.0000000
3	O'Neil\tCo	NULL
-- 3 rows
-- msg: cleanup starting
-- 12 rows affected
-- error 208 (severity 16, state 1, line 14): Invalid object name 'dbo.Nope'.
-- failed (exit 1): 1 error in batch 2, 48 ms
```

**Line types**

- Lines starting with `-- ` are tool metadata. Every other line is data.
- **Header line:** after `-- result N`, one `name:type` entry per column, tab-separated.
  - The type is everything after the last `:`, so names can contain colons.
  - An unnamed column has an empty name, as in `:int`.
  - Types carry their length or precision: `nvarchar(50)`, `varchar(max)`, `decimal(18,2)`, `datetime2(7)`.
- **Data rows:** tab-separated values.
- **Batch marker:** `-- batch N (line L)` appears only when the input has more than one batch.
- **Footer** after each result set: `-- N rows`, plus any truncation notes.
- **Statement summaries:** `-- N rows affected` for DML.
- **Messages:** `-- msg: text` for PRINT and messages of severity 10 or lower.
- **Errors:** `-- error <num> (severity S, state T, line L[, proc P]): message`.
- **Final summary:** always the last line, either `-- ok: ...` or `-- failed (exit N): ...`.

**Escaping (values, names and message text)**

- Backslash, tab, newline and CR are written as `\\`, `\t`, `\n` and `\r`. Other ASCII control characters become `\xNN`. A backslash before any other character means that character.
- Bare `NULL` means SQL NULL. A string whose entire value is `NULL` is written as `\NULL`.
- A first-column value starting with `--` gets a leading backslash (`\--...`), so data lines never look like metadata.
- An empty string is an empty field.

**Value rendering**

| Type | Rendering |
|---|---|
| char, varchar, nchar, nvarchar, text, ntext, xml, json | The text, escaped as above. |
| tinyint, smallint, int, bigint | Decimal. |
| bit | `0` or `1`. |
| decimal, numeric, money, smallmoney | Exact decimal string with scale kept (`12.50`). Never goes through a float. |
| float, real | Shortest form that round-trips. |
| date, time, datetime, datetime2, smalldatetime | ISO 8601. Fractional digits match the type: datetime has 3, `datetime2(n)` has n. |
| datetimeoffset | ISO 8601 with offset, e.g. `2024-01-05T09:12:00.0000000+01:00`. |
| uniqueidentifier | Uppercase canonical form, same as SSMS. |
| binary, varbinary, image, rowversion | `0x` followed by uppercase hex. |
| sql_variant | The value of its base type. |
| vector (2025) | JSON array text, which is what down-level clients receive. |
| geography, geometry, hierarchyid, CLR UDTs | `0x` hex. The help text suggests `.ToString()` or `.STAsText()` in the query. |

### 5.2 json

`--format json` writes one JSON document. It is streamed, so memory stays flat, and the outcome fields come last because they are only known at the end.

```json
{"batches":[
  {"batch":1,"source":"-Q","line":1,"events":[
    {"type":"msg","text":"starting"},
    {"type":"result",
     "columns":[{"name":"id","type":"int","nullable":false},{"name":"total","type":"decimal(18,2)","nullable":true}],
     "rows":[[1,"12.50"],[2,null]],
     "row_count":48213,"truncated":true,"cells_cut":0},
    {"type":"rows_affected","count":5},
    {"type":"error","number":208,"severity":16,"state":1,"line":14,"text":"Invalid object name 'dbo.Nope'."}]}],
 "notes":["ignored sqlcmd flags -W (sqlh output is always compact)"],
 "error":"query timeout after 60 s in batch 2; ...",
 "ok":false,"exit_code":1,"summary":"failed (exit 1): 1 error, first in batch 1, 48 ms","elapsed_ms":48}
```

**Batches and events**
- Each batch carries `batch`, `source` (a file name, `-Q` or `stdin`), `line` (where it starts), and `run`/`runs` when repeated by `GO n`.
- Its `events` array keeps wire order, so messages and errors stay where they happened relative to results.
- **result:**
  - `rows` holds only the shown rows (`--max-rows`), and `row_count` counts all of them.
  - `truncated` means rows were hidden; `cells_cut` counts values cut by `--max-cell` (they end in `…[+N]`).
  - Result sets hidden by `--max-results` emit no event; a note counts them.
- **error:**
  - `line` is mapped to the input, as in the text format. `source` is added when there are several inputs.
  - Inside a procedure, the event has `procedure`, and `line` is the procedure's own line.

**Values**
- Integers within ±2^53 are numbers; larger ones are strings, so nothing loses precision. bit is `0`/`1`.
- Floats are numbers in shortest round-trip form.
- decimal, numeric, money, dates and GUIDs are strings in exactly the text format's rendering.
- Binary is a `"0x..."` string. NULL is `null`.

**Outcome**
- `notes` (when present), `error` (when the run stopped early, including a guard block), `ok`, `exit_code`, `summary` (the text format's status line) and `elapsed_ms`.
- Failures before any batch, such as a bad profile or a refused connection, still produce a well-formed document with `"batches":[]`.
- A run that dies mid-result closes the open result with `"interrupted":true`.

### 5.3 csv and md

- **csv:**
  - RFC 4180 fields with LF line endings: a header row of column names, the rows, then a blank line before the next result set.
  - NULL is an empty unquoted field, and an empty string is `""`.
  - A field is quoted when it contains a comma, quote or line break, or starts or ends with whitespace.
  - The data stream (stdout, or `-o`) holds only CSV. Everything else goes to stderr as text-format `-- ` lines: `-- result N: name:type, ...` with the column types, row counts and truncation, messages, errors, notes, and the status line.
  - Limits apply as everywhere; for a full export pass `--max-rows 0 --max-cell 0`.
  - Known ambiguity: a one-column row holding NULL is an empty line, the same as the separator between result sets.
- **md (not built yet):** unpadded GitHub tables. `|` is written as `\|` and newlines as `<br>`. Messages are plain lines.

Usage errors found while parsing the arguments happen before `--format` is known, so they always use the text format.

## 6. Exit codes and streams

| Code | Meaning |
|---|---|
| 0 | Success. |
| 1 | One or more SQL errors were reported. |
| 2 | Couldn't connect, or login failed. |
| 3 | Usage or config error: bad flag, missing profile, unknown `$(var)`, `-P`. |
| 4 | Timeout (login or query). |
| 5 | Blocked by the read-only guard. Nothing was executed. |
| 6 | Cancelled (Ctrl+C or console close). |
| 10 | Internal error (a bug). Details come with `--verbose`. |

In the text, json and md formats, everything goes to stdout in order: results, messages, SQL errors, connection and usage errors, and the summary. That gives agents one stream to read. stderr carries only `--verbose` diagnostics, except in csv mode (§5.3).

Connection errors must be actionable. They name the server, port, auth method and encryption mode, give the underlying error, and add a hint where one applies, such as:
- a named instance with SQL Browser unreachable: try `host,port`
- an untrusted certificate: pass `-C` or fix the cert
- a TLS version mismatch with an old server

## 7. Read-only guard

Read-only is the default. Each profile sets a write policy:

| Policy | Behavior |
|---|---|
| `deny` | The profile can never write, and `--allow-write` is rejected. |
| `flag` (default) | `--allow-write` turns on writes for that run. |
| `allow` | Writes are on by default. |

An agent can pass `--allow-write` itself, so `flag` is only a speed bump. The real controls are `deny` and a read-only login.

In read-only mode, three layers apply:

1. **Classifier** (`src/guard.rs`). It runs on the whole input before anything executes. It has its own T-SQL tokenizer that understands comments (including nested ones), `'..'`, `N'..'`, `[..]` and `".."`.

   T-SQL doesn't require semicolons, so statement boundaries are unreliable. The classifier therefore doesn't try to find statements to allowlist. Instead it checks **every word token** and fails closed:
   - **Writes to scratch objects are allowed.** These are `INSERT`, `UPDATE`, `DELETE`, `SELECT … INTO`, `OUTPUT … INTO`, `TRUNCATE TABLE`, `CREATE TABLE`, `CREATE INDEX … ON`, `ALTER TABLE` and `DROP TABLE [IF EXISTS]`, when their target is a temp table (`#t`) or a table variable (`@t`). The same keywords with any other target are blocked. That includes aliases, even an alias that points at a temp table.
   - **Other DDL and transaction control are blocked:** any other `CREATE`, `ALTER` or `DROP`; `BEGIN TRAN`/`TRANSACTION`/`DISTRIBUTED`/`DIALOG`; and `UPDATE STATISTICS`.
   - **EXEC** is allowed only for allowlisted read-only system procedures:
     - `sp_help`, `sp_helptext`, `sp_helpindex`, `sp_helpconstraint`, `sp_helpdb`, `sp_helpfile`, `sp_helpstats`, `sp_helptrigger` and other `sp_help*`
     - `sp_columns`, `sp_tables`, `sp_pkeys`, `sp_fkeys`, `sp_stored_procedures`
     - `sp_spaceused` without `@updateusage`
     - `sp_who`, `sp_who2`, `sp_lock`
     - `sp_describe_first_result_set`, `sp_server_info`, `sp_databases`, `sp_depends`
     
     Dynamic SQL (`EXEC (...)`), `EXEC @variable`, `EXECUTE AS` and everything else are blocked, including `sp_executesql` and `xp_*`.
   - **Implicit calls count as EXEC.** A batch that begins with something other than a statement keyword (`dbo.Purge 5`, or `sp_who2` on its own) is treated as a procedure call and checked against the same allowlist.
   - **DBCC** is allowed only for `SHOW_STATISTICS`, `INPUTBUFFER`, `OPENTRAN`, `USEROPTIONS` and `TRACESTATUS`.
   - **These words are blocked anywhere:** `MERGE`, `COMMIT`, `ROLLBACK`, `SAVE`, `GRANT`, `REVOKE`, `DENY`, `BACKUP`, `RESTORE`, `KILL`, `SHUTDOWN`, `RECONFIGURE`, `CHECKPOINT`, `BULK`, `OPENQUERY`, `OPENROWSET`, `OPENDATASOURCE`, `WAITFOR`, `RECEIVE`, `SEND`, `CONVERSATION`, `ENABLE`, `DISABLE`, `UPDATETEXT`, `WRITETEXT`, `SETUSER`. If one of these is really an unbracketed column name, the message suggests writing it as `[Name]`.

   When it blocks something, nothing runs. It reports `-- blocked: line 3: UPDATE dbo.Orders would write; nothing was run. Rerun with --allow-write if the write is intended` and exits 5.
2. **Transaction backstop.** The session-setup batch ends with `BEGIN TRANSACTION`, and the run ends with `ROLLBACK`. This catches classifier gaps, such as an allowlisted procedure that writes.
   - `XACT_ABORT` stays OFF, so error behavior matches `--allow-write` runs.
   - The invariant: before each batch, the driver must report an active transaction. If an error rolled the wrapper back (a deadlock victim, a severity 20+ error), the run stops so nothing executes outside it.
   - It does not undo effects outside transactions: sequence and identity values, xp or CLR side effects, linked servers, mail.
3. **Honest docs.** The help text and the README say plainly that the guard reduces accidents but is not a security boundary. For real enforcement, use a login with only `db_datareader` (plus `VIEW DEFINITION`).

## 8. Credentials and profiles

The profile file is `%APPDATA%\sqlharness\profiles.toml`. Override its path with `SQLH_CONFIG`.

It is read by a small, strict parser of our own, which avoids pulling in the `toml` and serde crates. It understands the subset `sqlh` writes:
- a top-level `default`
- `[profiles.NAME]` tables
- `"strings"` (with the escapes `\" \\ \n \t`), whole numbers and booleans

Unknown keys are errors that name the line, so a typo can't fail silently. `sqlh profile add` rewrites the file, which drops any comments. A profile with `user` set uses SQL auth; without one it uses Windows auth. The `auth` key is written only for readability.

```toml
default = "dev"

[profiles.dev]
server = "devsql01,1433"
database = "AppDb"
auth = "windows"             # windows | sql
encrypt = "mandatory"        # optional | mandatory | strict
trust_server_certificate = false
write_policy = "flag"        # deny | flag | allow
max_rows = 100
query_timeout = 60

[profiles.reporting]
server = "tcp:myserver.database.windows.net"
database = "Reporting"
auth = "sql"
user = "agent_reader"        # password: Credential Manager target "sqlharness:reporting"
write_policy = "deny"
```

**Passwords**
- Stored in Windows Credential Manager as a generic credential with target `sqlharness:<profile>`. Windows protects it per user with DPAPI.
- Accessed through `CredWriteW` and `CredReadW` from `windows-sys`, so no extra crate is needed.
- A stored password is only ever sent to its profile's own server and login. `--profile x -S elsewhere` refuses rather than leak it, and `profile add` asks for the password again whenever the server or login changes. Otherwise a prompt-injected agent could repoint a profile and harvest the password. Other edits, like `-C` or `-d`, keep the stored password; `--new-password` forces a new prompt.
- `SQLCMDPASSWORD` also works, for CI.
- A password is never printed or logged, and is zeroed after use.

**Profile commands**
- `sqlh profile add NAME -S server [-d db] [-U login] [-C] [-N..] [--write-policy ..]`
  - It asks for the password with echo off. This is the tool's only prompt, and it refuses to run if stdin isn't a console.
  - It saves the profile and runs `profile test`.
  - Options you leave out keep their saved values, so rerunning `sqlh profile add NAME` just replaces the password.
  - The first profile you add becomes the default.
- `sqlh profile test NAME` connects and reports server, version, edition, database, login, user, encryption and auth scheme. Encryption and auth scheme come from `CONNECTIONPROPERTY`, which needs no extra permission. This is the first thing to run when a connection misbehaves.
- `sqlh profile list` shows every profile and whether its password is stored, without revealing it.
- `sqlh profile rm NAME` removes the profile and its credential.

## 9. Agent subcommands (M2)

The detailed design and plan is [schema-commands.md](schema-commands.md). It covers labeled output sections, name resolution, patterns, the per-command SQL, and the implementation plan. Summary:

| Subcommand | Output |
|---|---|
| `info` | Server version and edition, current database, login and user, encryption, auth scheme, server time. |
| `dbs` | Databases with state, compatibility level and recovery model. On Azure SQL DB, just the current database. |
| `tables [pattern]` | Schema-qualified tables and views with an estimated row count. |
| `describe <object>` | Columns (type, nullability, identity, computed, default), primary key, indexes (keys, includes, filter), FKs both ways, triggers, and estimated rows. Parameters for procedures and functions. |
| `find <pattern> [--text]` | Objects and columns whose names match (substring, or a `*`/`?` glob; case-insensitive). `--text` also searches module bodies. |
| `def <object>` | `OBJECT_DEFINITION`, printed raw and exempt from `--max-cell`. |

All catalog SQL must run on SQL Server 2012 and Azure SQL DB. That rules out:
- `STRING_AGG`, `STRING_SPLIT`, `TRIM`, `CONCAT_WS`, `OPENJSON`, and `DROP … IF EXISTS`
- cross-database names, and `USE` on Azure SQL DB

The commands are canned SQL run through the normal pipeline, with lists aggregated server-side by `FOR XML PATH`; schema-commands.md §3 explains why.

## 10. MCP mode (M3)

The detailed design and plan is [mcp-mode.md](mcp-mode.md). Summary:

- `sqlh mcp [--profile X]` is a stdio MCP server.
  - **Dual-era protocol:** it serves revision 2026-07-28 statelessly, with per-request `_meta` and `server/discover`, and also serves legacy clients that open with `initialize`.
  - **Fixed target:** the connection target and credentials are fixed at launch, so no tool argument can choose a server or carry a secret.
- **Tools:**
  - `query` (read-only, guarded), `check`, `info`, `databases`, `tables`, `describe`, `find`, `definition`, and `session_open`/`session_close`
  - `execute` for writes, listed only when launched with `--allow-write` and the policy allows it. Keeping it separate means harness permission prompts can gate writes.
- **Connections:** stateless calls run on pooled connections that are reset (TDS RESETCONNECTION) before each call. Explicit session handles get a dedicated connection where temp tables persist, inside one rolled-back transaction for read-only sessions.
- **Results:** the text format as a single text content item. `isError` is set when the exit code isn't 0.

## 11. Performance targets

- **Size:** a stripped exe under 6 MB. Measure at M0, and justify any growth over 10%.
- **Startup:** under 10 ms from process start to the first TDS packet, on a warm cache.
- **Memory:** rows stream through one locked 64 KB `BufWriter` on stdout. It flushes at the end of each batch and before exit, and no result set is ever buffered.
- **Runtime:** Tokio `current_thread`, with only the features we use enabled in our crate.
- **Lazy setup:** the profile file isn't read when flags alone are enough.
- **Console encoding:** when stdout is a console, call `SetConsoleOutputCP(CP_UTF8)`. Output is always UTF-8 without a BOM.

## 12. Testing

- **Unit tests (no server):**
  - batch splitting and `$(var)` handling
  - line mapping
  - argument parsing
  - value rendering and escaping
  - the guard classifier, table-driven, including adversarial cases: keywords hidden in comments or strings, nested brackets, a CTE followed by `UPDATE`, `SELECT INTO`, `EXEC('...')`
- **Golden tests:** fixed outputs for text, json, csv and md on representative result sets.
- **Integration tests:** they skip themselves unless `SQLH_TEST_PROFILE` names a profile on the dev box. They use temp tables or a scratch `sqlh_test` schema and clean up after themselves. Coverage:
  - every data type
  - multiple result sets
  - PRINT and RAISERROR at severities 0–10 and 11–16
  - error line mapping and rows affected
  - query timeout and attention
  - very long values
  - the guard's transaction backstop
- **Compatibility:**
  - check each release's imports with `dumpbin /dependents`
  - smoke-test each release on Server 2016 (or Windows 10 1607 LTSB)
  - test against whatever server versions are available: 2012, 2014, 2016, 2019, 2022, 2025, and Azure SQL DB

## 13. Milestones

- **MVP (built 2026-10-09).** This is M0 plus most of M1 and the M2 guard:
  - connection flags and environment variables, `-Q`, `-i` and stdin input, `GO n` and `$(var)`
  - the text format with all limits, mapped error lines and exit codes
  - timeouts and Ctrl+C
  - the read-only classifier and transaction backstop, and `--check`

  Profiles and Credential Manager followed the same day (§8), then the json and csv formats (§5.2, §5.3). Not built yet: the md format, the subcommands, and MCP. The driver checks that need a server are tracked in `docs/notes/m0-driver-spike.md`.
- **M0: driver spike.** Aims to surface deal-breakers early.
  - Set up the toolchain and the project scaffold.
  - Connect to the test server with SQL auth and SSPI.
  - SELECT every data type.
  - Capture PRINT, info messages, errors and the DONE row counts.
  - Cancel a running query with an attention.
  - Measure the exe's size and imports, and run it on Server 2016.
  - Deliverable: `docs/notes/m0-driver-spike.md` with findings and a go/no-go on mssql-tds (fallback: tiberius-ng).
- **M1: core CLI.**
  - flags, environment variables, profiles and Credential Manager
  - `-Q`, `-i` and stdin input
  - `GO` and `$(var)`
  - text format with limits
  - mapped error lines and exit codes
  - timeouts and Ctrl+C
- **M2: safety and conveniences.**
  - read-only guard with the transaction backstop
  - write policies and `--check`
  - json, csv and md formats
  - the subcommands, plus `profile test`
- **M3:** the MCP stdio server, `--stats`, and `explain` (plan XML condensed to an operator tree).
- **Later:**
  - persistent session sharing temp tables across one-shot calls
  - ARM64 build
  - code signing
  - parameterized queries
  - Entra token pass-in, if it becomes needed

## 14. Open questions and risks

- **Name.** `sqlh` is a working name.
- **mssql-tds maturity.** We pin 0.2.0 (crates.io, 2026-10-06), while upstream main is already at 0.3.0. Size and imports are fine (`docs/notes/m0-driver-spike.md`). Still unverified:
  - TDS 7.4 login against SQL Server 2012 and 2014
  - Azure SQL gateway redirection
  - attention and cancel on a live server
  - data type rendering on a live server
  - whether its Win32 calls stay within the Windows 10 1607 floor (needs a run on Server 2016)
- **Old servers.** SQL Server 2012 and 2014 without their TLS 1.2 patches can't negotiate TLS 1.2. Even with optional encryption, TDS encrypts the login packet, so both sides still need a TLS version in common. Document `-No` and its limits.
- **ARITHABORT.** SSMS sets it ON, while ODBC and ADO.NET apps leave it OFF, so plans can differ ("fast in SSMS, slow in the app"). Consider an `--app-settings` switch for performance investigations.
- **Environments.** The test server's details are still to come, from the owner and outside the repo. A Server 2016 machine is still needed for compatibility smoke tests.
- **Code signing.** An unsigned new exe can trip SmartScreen or antivirus on locked-down servers.
