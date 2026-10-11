# sqlharness design

Status: MVP built 2026-10-09; product direction updated 2026-10-10. [VISION.md](VISION.md) records the owner's strategy and takes precedence over older roadmap and compatibility assumptions. Rows marked **Decided** in §2 are the owner's calls. Unbuilt features remain proposals.

## 1. Purpose

Agents such as Claude Code need to query SQL Server. `sqlcmd` works, but it fights them. The classic version needs the ODBC driver installed. Its output is padded for humans. It can block on prompts. It wants the password on the command line. It has no row or size limits, prints NULL ambiguously, and reports errors without enough context to fix them.

sqlharness (`sqlh.exe`, working name) is a single exe with no dependencies that runs T-SQL and returns compact, unambiguous, size-limited output. It never waits for input, reports errors that point at the failing line, and refuses writes unless asked.

The primary workflow is an agent with limited context making several queries across a human-led DBA or developer question-and-answer session. Optimize time to the next useful answer: focused discovery, concise output, low repeated-call overhead, and quick cancellation so the agent can try again. Bulk analytics are secondary. Exact total row-count usefulness remains open; the current draining/counting behavior is documented below and has not changed.

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
| First-release target | `sqlh.exe` on Windows x64 workstations (validated on Windows 11), connecting to SQL Server 2025 Standard, which is remote in about 90% of use cases. Running on the database server is not a first-release target. **Decided** |
| Compatibility expansion | Candidate Windows Server 2016 / SQL Server 2016 floor, conditional on acceptable technical debt. Other targets need separate assessment. **Decided** |
| Test server | The owner's existing remote server. Its details stay out of the repo. **Decided** |
| Workflow | Repeated agent queries in human-led DBA/developer sessions; low context consumption and quick cancellation. **Decided** |
| Modes | One-shot CLI exists. Connection reuse is a priority; MCP and persistent sessions are proposed mechanisms, subject to reliable cleanup and execution policy. |

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

- **Target:** `x86_64-pc-windows-msvc`. The first release runs on Windows x64 workstations, validated on Windows 11; the database server is usually remote, so network latency is part of every call. A Windows 10 / Windows Server 2016 floor is a later candidate, not a current support claim. ARM64 may come later.
- **Static CRT:** set `-C target-feature=+crt-static` in `.cargo/config.toml`. Without it the exe needs `vcruntime140.dll`, which isn't on a clean server.
- **Allowed imports:** only DLLs that ship with Windows (kernel32, advapi32, secur32/sspicli, crypt32, ncrypt, bcrypt, ws2_32, ntdll, and similar). Check with `dumpbin /dependents`.
- **Release profile:** `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = "symbols"`.
- **TLS:** SChannel. Validate the encryption modes (mandatory and strict) from a first-release workstation against SQL Server 2025; evaluate older-platform TLS behavior when assessing the candidate 2016 floor.

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
| `-S server` | `host`, `host\instance`, `host,port`, with an optional `tcp:` prefix. Env: `SQLCMDSERVER`. sqlh connects over TCP. Unlike sqlcmd, it doesn't fall back to Named Pipes for a remote name: the driver's first pipe open is a synchronous call on its runtime thread, so an unreachable host would hold the run for about 21 s, past `-l` and Ctrl+C. Local names (`.`, `(local)`, `localhost`, `127.0.0.1`, `::1`, LocalDB) keep the driver's order: shared memory, TCP, then a local pipe. An IPv6 address without a port is sent with port 1433, so the driver uses TCP only; with an instance name it can't be forced onto TCP, so give it a port.<br>**A Named Pipes request to another machine is refused** (exit 3, before anything connects): an `np:` prefix with a remote host (`np:db01`, `np:db01\SALES`), or a UNC pipe path (`\\db01\pipe\sql\query`) with or without a prefix. The message gives the TCP spelling: `-S db01`. It applies to every source of the server (`-S`, `SQLCMDSERVER`, a profile), and `profile add` won't save one. Local pipes are accepted: `np:` with `.`, `(local)`, `localhost`, `127.0.0.1`, `::1` or this computer's own name (`COMPUTERNAME`), and `\\.\pipe\...`. So are `lpc:` (shared memory, local only), `admin:` and `(localdb)\name`, which are passed to the driver as written. |
| `-d db` | Database. Env: `SQLCMDDBNAME`. Without it, the profile's database applies, else the login's default, which is often `master`. |
| `-E` | Windows integrated auth. This is the default when there is no `-U`, same as sqlcmd. |
| `-U user` | SQL login. The password comes from the profile's credential or `SQLCMDPASSWORD`. Env: `SQLCMDUSER`. |
| `-P` | **Rejected** with exit 3 and a message pointing to profiles or `SQLCMDPASSWORD`. Command lines end up in agent transcripts and process lists. |
| `-Q sql` | SQL text to run. `-q` is treated as `-Q`, because there is no interactive mode. |
| `-i file` | Input file. Repeatable. |
| `-o file` | Write output to a file (UTF-8). Input is read and checked first, so a bad input never touches an existing file. Naming an input file (however spelled, including a hard link) is refused with exit 3, and so is an existing file that can't be written. Each run writes its own new `file.<pid>-<n>.partial`, which no other process can change, and renames it over `file` when the output is complete: the file is replaced whole or not at all. A run whose output isn't complete, or that is killed while writing, leaves no `.partial` file behind: Windows deletes it when the last handle closes, because the file is marked for deletion as soon as it is created. If that mark can't be set (not expected), a run that ends normally still removes the file by name, but a kill, or an error while removing it, can leave it behind. A kill in the moment between finishing and the rename can leave one in either case. sqlh never touches a `.partial` file it didn't create. When runs overlap, each publishes a complete file and the last one wins. A device or pipe (`-o NUL`, `\\.\pipe\name`) is written directly. |
| `-v n=v ...` | Values for `$(n)` substitution. Environment variables also resolve, as in sqlcmd. |
| `-x` | Turn off `$(var)` substitution. |
| `-l sec` | Login timeout. Default 15; 0 means none, as in sqlcmd. A name can resolve to several addresses (`localhost` has `::1` and `127.0.0.1`), and they are tried in turn, once each: nothing is retried. Each TCP connect attempt gets `-l` seconds, and the whole login (all the attempts, TLS and login) one second more. When every attempt fails before that deadline, the run ends with **exit 2**; a refused attempt takes about 2 s, because Windows retries it, so refusals at two addresses end after about 4 s. When the deadline expires first, the run ends with **exit 4**: a server that takes the connection but doesn't finish the login, or a later address's attempt still running at the deadline (`-l 2` against two refusing addresses: the first attempt ends at 2 s, and the deadline at 3 s cuts the second). So a name with one address that refuses or doesn't answer is exit 2, and a name with several can be either, depending on how long each attempt takes. With `-l 0`, a TCP connect ends only when Windows gives up on it, after about 21 s, and a login has no deadline; Ctrl+C still ends it. |
| `-t sec` | Query timeout per batch. Default 60; 0 means none. sqlcmd's default of none is wrong for agents. It is also how long sqlh waits for a reader that stops reading its output (exit 7, §6). |
| `-b` | Accepted. Stopping at the first failing batch is already the default. |
| `-c term` | Batch terminator. Default `GO`. |
| `-C` | Trust the server certificate. `--trust-server-certificate=false` turns off a profile's saved trust for one run. |
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
| `--max-output N` | Bytes of output per run, across stdout and stderr. Default 1000000; 0 means no limit, and otherwise it must be at least 4096. See §4.4. |
| `--allow-write` | Turn off the read-only guard, if the profile's write policy allows it (§7). |
| `--check` | Compile only: `SET NOEXEC ON`, executes nothing. Catches syntax errors and unknown columns of existing tables. Missing tables are only reported when a query runs, because SQL Server defers their resolution. Input that mentions `NOEXEC` is refused (exit 5; §7 lists the false positives), NOEXEC is re-armed before every batch, and the rolled-back wrapper transaction always applies. With `--allow-write`, write statements may be compiled; they still don't run. A later version could add `sp_describe_first_result_set` per statement. |
| `--continue-on-error` | Keep running later batches after a failed one. |
| `--stats` | Turn on `SET STATISTICS IO, TIME` and print a summary instead of the raw messages (M3). |
| `--verbose` | Diagnostics to stderr. Never includes secrets. One line after connecting: the profile, how long the connect and the session setup took, and the server, version, database and encryption. A build with `--features timing` also splits the connect into steps: `connected in 194 ms (tcp 5, prelogin 9, tls 29, login 150)`. |
| `--help`, `--version` | `--help` is read by models: example-first and under about 60 lines. |

### 4.3 Execution semantics

- **Batch splitting.** A line holding only the terminator splits batches. The match is case-insensitive and allows surrounding whitespace, an optional `GO n` repeat count, and a trailing `--` comment. Terminators inside string literals and block comments don't count, which is stricter and more correct than sqlcmd.
- **Variables.** `$(var)` is substituted after splitting, batch by batch, so a variable can't create a batch separator: a value of `GO` stays inside its batch. An unknown variable is a usage error (exit 3). Error lines stay mapped to the input even when a value contains line breaks.
- **Fail fast.** The whole input is read, split and guard-checked (§7) before anything runs.
- **Execution.** Batches run in order on one connection. Every error a batch raises is reported. After a batch with an error of severity 11 or higher, later batches are skipped unless `--continue-on-error` is set.
- **Session SET options** match SSMS: `ANSI_NULLS`, `ANSI_PADDING`, `ANSI_WARNINGS`, `ARITHABORT`, `CONCAT_NULL_YIELDS_NULL` and `QUOTED_IDENTIFIER` ON, and `NUMERIC_ROUNDABORT` OFF. That way filtered and computed-column indexes behave as people expect. (§14 covers the ARITHABORT question.)
- **Error line numbers** are mapped back to the original input: a file and line, not just a line within the batch.
- **Query timeout** sends a TDS attention, reports `-- error: query timeout after 60 s (batch 2)` and exits 4.
- **Cancellation.** Ctrl+C, Ctrl+Break, the console closing, logoff and shutdown cancel the run wherever it is: connecting, setting up the session, running a batch, or waiting for a slow reader.
  - A running statement gets an attention. sqlh waits up to 2 s for the server to confirm it, or 1 s after a close, logoff or shutdown, because Windows ends the process about 5 s after those. Then it closes the connection, which stops the server's work too. A pending connect is dropped at once. No later batch starts.
  - The run exits 6 and says what became of the work: `-- error: cancelled by Ctrl+C: the statement was cancelled; the connection stayed usable`, or `...: the server didn't confirm the cancel, so the connection was closed` (the driver marked the connection dead), `...: nothing confirmed the cancel within 2 s, so the connection was closed` (the grace ran out; 1 s after a close, logoff or shutdown), `...: cancelled while connecting`, `...: cancelled while waiting for the reader; the connection was closed`. sqlh never says that the server confirmed a cancel: the driver marks the connection dead when its attention goes unacknowledged and offers no receipt otherwise, and a connection not marked dead is a liveness observation (a cancel before the request was sent leaves it unmarked too). A run whose batches had all finished keeps its own outcome.
  - The closing lines get 1 s to reach the reader; past that, the run is exit 7 and stderr gives exit 6 and the reason (§6).
  - Before it connects, while it reads and checks the input, Ctrl+C ends sqlh at once; nothing has run. A process started in a new process group gets no Ctrl+C from Windows, so a harness sends it Ctrl+Break. Windows sends logoff and shutdown only to services; other processes are just ended.
  - A harness that kills sqlh outright also cancels the server's work: the server ends the request within about 200 ms of the connection dropping (measured for a waiting query; CPU-bound and blocked queries are PLAN.md 2.5).
- **Malformed input never crashes sqlh.** Unterminated strings and names reach the server, which reports the syntax error.
- **Expected failures are results, never crashes,** including a closed stdout or stderr (§6).
- **A bug that panics still exits 10,** never an abort. Everything else about it is best effort: a stderr line, and a status line on stdout in text output if stdout takes it. Output still buffered is lost, a JSON document stays unfinished, `-o` output isn't published, and a password prompt's console mode is restored. Panic injection (`SQLH_DEBUG_PANIC`) exists only in debug builds, so the tests exercise the hook there; release builds have the same hook.

### 4.4 Output limits

These keep a careless query from flooding an agent's context:

**Product direction:** quick cancellation is essential to iterative investigation. Whether exact total row counts justify draining hidden rows remains open. The current behavior below is retained until a separately designed and tested change defines early cutoff, partial counts, and response completion.

- **Rows.** After `--max-rows`, rows stop being formatted but are still read and counted. Later statements still run, and the footer gives an exact total: `-- 100 of 48213 rows shown (--max-rows 100)`. The query timeout still applies.
- **Cells.** A cell longer than `--max-cell` is cut at a character boundary and ends with `…[+N]`, where N is the number of characters left out. A binary value shows at most `--max-cell / 2` bytes and ends with `…[+N bytes]`. The result set's footer counts the truncated cells.
- **Result sets and messages.** Past their caps (for example, a `WHILE` loop that selects or prints), output is counted, not printed.
- **Row counts.** Consecutive row counts within a batch become one line, `-- 300 statements, 300 rows affected`, so a loop of inserts prints one line rather than 300. A result set, message or error in between starts a new line.
- **Total output.** `--max-output` (default 1,000,000 bytes) caps everything a run writes, in every format, across stdout and stderr.
  - Once the next piece wouldn't fit, nothing more of the body is written: no result sets, rows, messages, errors, row counts or batch markers.
  - The result set being shown still ends with its footer: `-- 80 of 500 rows shown (--max-output)`. Notes and a guard refusal are always written too.
  - The last 1,024 bytes are kept for the closing lines, which are always written: a note that the output stopped and what wasn't shown, then the status. JSON stays a valid document, with the cut result marked `"truncated":true`.
- **Limits never change what runs.** Like the row limit, the output limit only stops printing: the run continues to the end, and the counts (rows, errors, the status line) stay exact. Whether to stop early instead is the open drain question (PLAN.md 2.6).
- **Input.** At most 10 MB of SQL per run, across `-Q`, `-i` files and stdin, and again after `$(var)` substitution; `GO n` runs a batch at most 1000 times. Past either, the run is refused with exit 3 before anything connects.

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
  - The type is everything after the last `:`, so names can contain colons. A type name never does: a `:` in one (only a CLR UDT's name comes from the server as is) is written `\x3A`. A reader splits a header entry at its last literal `:` first, then unescapes the name and the type separately.
  - An unnamed column has an empty name, as in `:int`.
  - Types carry their length or precision: `nvarchar(50)`, `varchar(max)`, `decimal(18,2)`, `datetime2(7)`.
- **Data rows:** tab-separated values.
- **Batch marker:** `-- batch N (line L)` appears only when the input has more than one batch.
- **Footer** after each result set: `-- N rows`, plus any truncation notes.
- **Statement summaries:** `-- N rows affected` for DML. Consecutive ones become one line, `-- 300 statements, 300 rows affected` (§4.4).
- **Messages:** `-- msg: text` for PRINT and messages of severity 10 or lower.
- **Errors:** `-- error <num> (severity S, state T, line L[, proc P]): message`.
- **Final summary:** the last line of complete output, either `-- ok: ...` or `-- failed (exit N): ...`. Output is cut short only when it couldn't be delivered (exit 7) or sqlh crashed (exit 10, where the line is best effort). Then the exit code is what counts (§4.3, §6).

**Escaping (values, names, and every variable field in metadata lines)**

- Backslash, tab, newline and CR are written as `\\`, `\t`, `\n` and `\r`.
- Every other control character becomes `\xNN`, with exactly two uppercase hex digits: the C0 controls and DEL (U+0000–U+001F, U+007F) and the C1 controls (U+0080–U+009F). U+0085 is one of Unicode's line breaks, and U+009B is a control sequence introducer on some 8-bit terminals.
- U+2028 and U+2029, the other two Unicode line breaks, become `\u2028` and `\u2029`: `\u` and exactly four hex digits.
- A backslash before any other character means that character. A reader decodes `\xNN` and `\uNNNN` to the character they name, so a literal backslash is always doubled (`\\x3A` is a backslash followed by `x3A`, while `\x3A` is a colon).
- Other Unicode text (accents, no-break space, CJK) is written as it is.
- The same escaping applies to type names in headers, and to every variable field of a metadata line: column and type names, procedure names, source names in locations, guard reasons and advice, notes and error text. Nothing a database or an input contains can start a new line or pass for a `-- ` line, whether the consumer splits at `\n` only or at every Unicode line break (Python's `splitlines`, for instance). (Windows paths in notes therefore show doubled backslashes.) This is for text, and the metadata lines of csv. A csv data row follows csv's own quoting, and a json string keeps these characters as JSON allows (§5.2, §5.3).
- Bare `NULL` means SQL NULL. A string whose entire value is `NULL` is written as `\NULL`.
- A first-column value, or a first column name in a header, starting with `--` gets a leading backslash (`\--...`), so data and header lines never look like metadata.
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
- **rows_affected:** `count` is the number of rows. A run of consecutive statements is one event, which then also carries `statements`: `{"type":"rows_affected","count":300,"statements":300}`.
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
| 6 | Cancelled: Ctrl+C, Ctrl+Break, the console closing, logoff or shutdown (§4.3). |
| 7 | Output could not be delivered (see below). |
| 10 | Internal error (a bug), including any panic. Details go to stderr, best effort (§4.3). |

**Exit 7, undeliverable output.** sqlh couldn't hand its complete output, including the final status line, to its destination. That happens when:
- the program reading stdout closed it early (a broken pipe), for example a harness that stopped reading. This covers `--help` and `--version` too;
- the reader kept stdout open but read none of the waiting output for `-t` seconds (a stall; time with nothing to write doesn't count). Progress is measured per write of at most 4 KB: every write the reader takes restarts the clock, so a slow reader that keeps reading is not a stall, as long as it takes each 4 KB within `-t` seconds. In csv mode a single thread writes stdout and stderr in order, so a stalled reader of either stream stalls the whole output. A thread writes the output, so a stalled reader can't block timers or Ctrl+C: the batch waits for room in a bounded queue, then gives up;
- in csv mode, stderr was closed: it carries the metadata and the status (§5.3);
- writing or flushing `-o file` failed, for example a full disk or lost permissions;
- the finished `.partial` file couldn't be renamed over `file`.

The SQL may have run, so exit 7 says nothing about the database: it means "don't trust what you received", and it is no reason to retry a write before checking what happened. It takes precedence over the run's own exit code. When that code is known, stderr gives it, as in `sqlh: output could not be delivered (...); the run itself ended with exit 1: ...`. When the failure stopped the run partway, stderr says that the outcome is unknown: the batch being written may have run, and later batches did not. With `-o`, an undelivered run leaves the existing file untouched, and its `.partial` file is deleted.

Diagnostics on stderr are best effort. If stderr is closed they are dropped, and the exit code stays what it would have been (csv mode aside, above).

In the text, json and md formats, everything goes to stdout in order: results, messages, SQL errors, connection and usage errors, and the summary. That gives agents one stream to read. stderr carries only `--verbose` diagnostics, delivery failures (exit 7) and internal errors (exit 10), except in csv mode (§5.3).

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

Database permissions are expected to provide the enforcement boundary. The tool's checks are also important when a DBA or developer has accidentally granted an agent excessive access. An agent can pass `--allow-write` itself, so `flag` is an accident guard; `deny` is the strongest tool policy, but does not replace restricted database permissions.

Command categorization and the balance of allowlists/blocklists need further design using legitimate DBA workflows and their risks. The current classifier below is the MVP mechanism, not the final categorization system. See [VISION.md](VISION.md).

In read-only mode, three layers apply:

1. **Classifier** (`src/guard.rs`). It runs on the whole input before anything executes. It has its own T-SQL tokenizer that understands comments (including nested ones), `'..'`, `N'..'`, `[..]` and `".."`.

   T-SQL doesn't require semicolons, so statement boundaries are unreliable. The classifier therefore doesn't try to find statements to allowlist. Instead it checks **every word token** and fails closed:
   - **Writes to scratch objects are allowed.** These are `INSERT`, `UPDATE`, `DELETE`, `SELECT … INTO`, `OUTPUT … INTO`, `TRUNCATE TABLE`, `CREATE TABLE`, `CREATE INDEX … ON`, `ALTER TABLE` and `DROP TABLE [IF EXISTS]`, when their target is a session temp table (`#t`) or a table variable (`@t`). The same keywords with any other target are blocked, including plain aliases (`DELETE t FROM dbo.T t`).
     - **Global temp tables (`##t`) are blocked:** other sessions share them, so a write could block or confuse them.
     - An alias spelled like a temp table (`UPDATE #o ... FROM dbo.Orders AS #o`) passes the classifier; the rolled-back transaction covers it.
   - **Other DDL and transaction control are blocked:** any other `CREATE`, `ALTER` or `DROP`; `BEGIN TRAN`/`TRANSACTION`/`DISTRIBUTED`/`DIALOG`; and `UPDATE STATISTICS`.
   - **`NEXT VALUE FOR` is blocked:** sequence allocation survives a rollback.
   - **EXEC** is allowed only for allowlisted read-only system procedures:
     - `sp_help`, `sp_helptext`, `sp_helpindex`, `sp_helpconstraint`, `sp_helpdb`, `sp_helpfile`, `sp_helpstats`, `sp_helptrigger` and other `sp_help*`
     - `sp_columns`, `sp_tables`, `sp_pkeys`, `sp_fkeys`, `sp_stored_procedures`
     - `sp_spaceused`, unless `@updateusage` is passed by name or as the second argument (it runs `DBCC UPDATEUSAGE`)
     - `sp_who`, `sp_who2`, `sp_lock`
     - `sp_describe_first_result_set`, `sp_server_info`, `sp_databases`, `sp_depends`

     The name may be bare or qualified only as `sys.`, `master.sys.`, `master.dbo.` or `master..`. Other qualifications are blocked: a linked server (`Lnk.master.sys.sp_who`), another database, or another schema (`sales.sp_who`, `dbo.sp_who`) can name a different procedure with the same name. This is a conservative policy, not a claim about what a given name resolves to, and the refusal suggests the `sys.` spelling (`sys.sp_who`), as Microsoft recommends for system procedures.

     Dynamic SQL (`EXEC (...)`), `EXEC @variable`, `EXECUTE AS` and everything else are blocked, including `sp_executesql` and `xp_*`.
   - **Implicit calls count as EXEC.** A batch that begins with something other than a statement keyword (`dbo.Purge 5`, or `sp_who2` on its own) is treated as a procedure call and checked against the same allowlist.
   - **DBCC** is allowed only for `SHOW_STATISTICS`, `INPUTBUFFER`, `OPENTRAN`, `USEROPTIONS` and `TRACESTATUS`.
   - **These words are blocked anywhere:** `MERGE`, `COMMIT`, `ROLLBACK`, `SAVE`, `GRANT`, `REVOKE`, `DENY`, `BACKUP`, `RESTORE`, `KILL`, `SHUTDOWN`, `RECONFIGURE`, `CHECKPOINT`, `BULK`, `OPENQUERY`, `OPENROWSET`, `OPENDATASOURCE`, `WAITFOR`, `RECEIVE`, `SEND`, `CONVERSATION`, `ENABLE`, `DISABLE`, `UPDATETEXT`, `WRITETEXT`, `SETUSER`. If one of these is really an unbracketed column name, the message suggests writing it as `[Name]`.

   When it blocks something, nothing runs. It reports `-- blocked: <where>: <reason>; nothing was run. <advice>` and exits 5, for example `-- blocked: line 3: UPDATE dbo.Orders would write; nothing was run. Rerun with --allow-write if the write is intended`. The advice fits the refusal. `--allow-write` is suggested only where it helps, and a better spelling comes first where there is one: a `#temp` table instead of `##t`, `sys.sp_who` instead of `dbo.sp_who`, `sp_spaceused` without `@updateusage`, or fixing a typo.

   **Under `--check`** one more rule applies, whether or not writes are allowed: input that mentions `NOEXEC` is refused, because `SET NOEXEC OFF` would run the SQL instead of compiling it. The rule is deliberately coarse. It refuses the word anywhere outside strings, comments and quoted names, so a bare column or alias named `NOEXEC` is refused too, while `[NOEXEC]` is fine. The advice says so, and that `--allow-write` doesn't lift this rule.

   Malformed input (an unterminated string, name or comment) never stops the classifier: everything after the opening quote is treated as quoted, and the server reports the syntax error, so nothing in that batch runs.
2. **Transaction backstop.** The session-setup batch ends with `BEGIN TRANSACTION`, and the run ends with `ROLLBACK`. This catches classifier gaps, such as an allowlisted procedure that writes.
   - `XACT_ABORT` stays OFF, so error behavior matches `--allow-write` runs.
   - The invariant: before each batch, the driver must report an active transaction. If an error rolled the wrapper back (a deadlock victim, a severity 20+ error), the run stops so nothing executes outside it.
   - It does not undo effects outside transactions: sequence and identity values, xp or CLR side effects, linked servers, mail.
3. **Honest docs.** `--help` and the README say plainly that the guard reduces accidents but is not a security boundary. For real enforcement, use a login with only `db_datareader` (plus `VIEW DEFINITION`).

## 8. Credentials and profiles

The profile file is `%APPDATA%\sqlharness\profiles.toml`. Override its path with `SQLH_CONFIG`.

It is read by a small, strict parser of our own, which avoids pulling in the `toml` and serde crates. It understands the subset `sqlh` writes:
- a top-level `default`
- `[profiles.NAME]` tables
- `"strings"` (with the escapes `\" \\ \n \t`), whole numbers and booleans

Unknown keys, repeated keys and contradictions are errors that name the line, so a typo can't fail silently. `sqlh profile add` rewrites the file, which drops any comments.

`auth` decides the auth method: `"windows"` or `"sql"`, and `user` must be set exactly when it's `"sql"`. Files without `auth` fall back to "SQL auth when `user` is set". The method is resolved before the login, with the usual precedence (flags, profile, environment), so a Windows-auth profile stays Windows auth even when `SQLCMDUSER` is set; `-U` or `-E` override it for one run.

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
- A password is never printed or logged. `profile list` checks that a credential exists without reading the password.
- In-memory copies are not yet reliably wiped: only the buffer handed to `CredWriteW` is zeroed. Wiping the prompt, the copy read back and the copy given to the driver is planned (PLAN.md, F14).

**Profile commands**
- `sqlh profile add NAME -S server [-d db] [-U login] [-C] [-N..] [--write-policy ..]`
  - It asks for the password with echo off. This is the tool's only prompt, and it refuses to run if stdin isn't a console, or if echo can't actually be turned off (Git Bash's mintty: use PowerShell, cmd or `winpty`). Echo comes back on every return path, errors included, and after a crash. Ctrl+C, Ctrl+Break or closing the console at the prompt restores it before the process ends; only a process killed outright can leave echo off in that console.
  - It saves the profile and runs `profile test`.
  - Options you leave out keep their saved values, so rerunning `sqlh profile add NAME` just replaces the password.
  - The first profile you add becomes the default.
- `sqlh profile test NAME` connects and reports server, version, edition, database, login, user, encryption and auth scheme, with a note on how long connecting took (the `--verbose` line). Encryption and auth scheme come from `CONNECTIONPROPERTY`, which needs no extra permission. This is the first thing to run when a connection misbehaves.
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

Catalog SQL must be validated on SQL Server 2025 Standard for the first release. Prefer inexpensive portability toward the candidate SQL Server 2016 floor, but evaluate compatibility costs rather than enforcing the earlier SQL Server 2012 restriction. Azure-specific constraints remain relevant to future expansion and are not first-release gates.

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

The primary product measure is time to the next useful answer across repeated agent turns. Measure repeated-call connection/setup overhead, response size/context consumption, and cancellation-to-retry time alongside the existing executable and startup targets. Bulk throughput is secondary; additional numerical interaction targets need measurement before being committed.

- **Size:** a stripped exe under 6 MB. Measure at M0, and justify any growth over 10%.
- **Startup:** under 10 ms from process start to the first TDS packet, on a warm cache.
- **Connect:** measured in [notes/connect-latency.md](notes/connect-latency.md), on one dev server and one LAN. Against SQL Server 2025 with SQL authentication, a connect took about 195 ms, and about 150 ms of it was the login step, the wait for the server's reply to the login packet. SQL Server 2025's documented password hashing (PBKDF2) is the likely cause, but that is an inference from the interval; nothing isolated the hashing from the server's other login work. sqlh and the driver added about 5 ms. Not measured: Windows authentication, other servers and networks, strict encryption. That supports putting connection reuse first for fast repeated turns (PLAN.md Phase 5), not client tuning; it is no universal lower bound for a connect.
- **Failing fast:** a refused connection fails after one attempt, in about 2 s, and an address that doesn't answer after `-l` seconds (§4.2). Ctrl+C ends a pending connect at once.
- **Memory:** rows stream, and no result set is ever buffered. The sinks hand chunks of at most 64 KB to an output thread (`output/delivery.rs`), which writes them in units of at most 4 KB. Once more than 256 KB is queued, the batch loop stops reading rows and waits for the reader, without blocking the async runtime. That is a threshold checked between rows, not a hard ceiling: whatever the sink is writing at the time (a row, a message, an error) is queued whole, as chunks, however large it is, and several events can be queued between two checks. The queue is flushed at the end of each batch, and sqlh waits for it to drain before it exits.
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
  - "never panics" property tests for both lexers, over generated input
- **Golden tests:** fixed outputs for text, json, csv and md on representative result sets.
- **Process-level tests** (`tests/cli.rs`): run the real binary, isolated from the machine's profiles and `SQLCMD*` variables, and assert exit codes, files and output: `-o` safety (identity, concurrent runs, files that can't be replaced, devices), exit 7, closed stdout/stderr in every format, the panic net, `--check`, guard blocks and their advice, auth precedence, connect failures (refused, a server that never answers) and Ctrl+Break during a connect. Tests that need no server use an unreachable address or a local listener that never answers. Ctrl+Break reaches a child started in its own process group, which needs a console shared with the test; `cargo test` run from a terminal has one. They run against the debug and the release binary (`cargo test --release --test cli`); the panic-injection tests exist only in debug builds.
- **Live tests:** the `live` tests in `tests/cli.rs` are `#[ignore]`d, so a plain `cargo test` reports them as ignored rather than passing. Run them with `cargo test --test cli -- --ignored live` and `SQLH_TEST_PROFILE` naming a profile on the dev box; asked for without it, they fail. They use the scratch database `sqlh_test` (default; override with `SQLH_TEST_DATABASE`), created by `tests/fixtures/sqlh_test.sql` and removed by `tests/fixtures/drop_sqlh_test.sql`. A test that writes first checks the database's `dbo.sqlh_scratch` marker, then writes only to a table of its own, which it drops. Planned coverage (PLAN.md 3.4):
  - every data type
  - multiple result sets
  - PRINT and RAISERROR at severities 0–10 and 11–16
  - error line mapping and rows affected
  - query timeout and attention
  - very long values
  - the guard's transaction backstop
- **Compatibility:**
  - check each release's imports with `dumpbin /dependents`
  - validate the first release on a clean Windows 11 x64 workstation against a remote SQL Server 2025 Standard, including supported authentication and cancellation paths
  - assess Windows Server 2016 / SQL Server 2016 compatibility separately, including its maintenance cost, before committing to that floor
  - other versions and Azure may be tested opportunistically; those results do not establish an unqualified support promise

## 13. Milestones

**Current delivery priorities (2026-10-10):** fix known correctness and safety defects, make cancellation and bounded output reliable, then deliver focused schema discovery and reduce repeated connection/setup overhead. Develop command categorization further; assess exact row-count usefulness and compatibility expansion with evidence. The M0–M3 descriptions below group work and record the original plan; they do not override [VISION.md](VISION.md) or make older platforms first-release requirements.

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
  - Measure the exe's size and imports, and validate the first-release platform: a Windows 11 workstation against a remote SQL Server 2025. Evaluate a 2016 floor separately for the candidate expansion.
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
- **mssql-tds maturity.** We pin 0.2.0; size, imports, and SQL Server 2025 data rendering checks are recorded in `docs/notes/m0-driver-spike.md`. First-release validation still needs supported authentication, reliable attention/cancellation, and a clean Windows 11 workstation deployment. Older-server negotiation and Azure redirection are future compatibility questions.
- **Compatibility cost.** Assess a Windows Server 2016 / SQL Server 2016 floor without letting it complicate or delay the first release. SQL Server 2012/2014 and Azure are not current release gates. Supporting any expansion needs explicit tests and an acceptable maintenance budget.
- **Exact row counts.** Their usefulness during investigation is unresolved; assess the latency and server work involved in draining hidden rows. Fast cancellation is a firm priority.
- **Command categorization.** Develop the policy around legitimate DBA workflows and excessive database privileges. The current classifier is an initial implementation, and database permissions remain the enforcement boundary. Owner direction (2026-10-10): the EXEC allowlist must grow to cover read-only server-state procedures such as `xp_readerrorlog` and `xp_instance_regread`, and others where the login's permissions allow them (PLAN.md, Phase 6).
- **ARITHABORT.** SSMS sets it ON, while ODBC and ADO.NET apps leave it OFF, so plans can differ ("fast in SSMS, slow in the app"). Consider an `--app-settings` switch for performance investigations.
- **Environments.** The owner's existing test server is reached through a local profile; details stay outside the repo. Release validation needs a clean Windows 11 x64 workstation and a SQL Server 2025 Standard server reached over the network. A 2016 environment is only required if that expansion is pursued.
- **Code signing.** An unsigned new exe can trip SmartScreen or antivirus on locked-down workstations.
