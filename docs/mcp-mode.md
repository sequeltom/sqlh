# MCP mode: design and plan

Status: **built 2026-10-17** (PLAN.md 5.2, the engine, `src/engine.rs`; 5.3, `sqlh mcp`, `src/mcp/`); proposed 2026-10-09, its shape decided by the owner on 2026-10-15 to 2026-10-17 (PLAN.md §6, decisions 14–20, decision 17 amended after spike 5.1). Still to do: 5.4 (live tool tests), 5.5 (client interop) and 5.6 (agent trial 3). This doc expands DESIGN.md §10 and is the spec of what's built. It targets MCP revision **2026-07-28** and also serves clients on the older handshake revisions; §2 was checked against the published revision on 2026-10-17 (versioning, discovery, cancellation, tools, caching and stdio; docs/notes/mcp-reuse-spike.md §3). The tools it exposes are the CLI's query path and the schema commands (`docs/schema-commands.md`).

Product direction (2026-10-10): [VISION.md](VISION.md) makes low repeated-call overhead and quick cancellation priorities for human-led DBA/developer investigations. First-release validation targets SQL Server 2025 Standard on Windows Server 2025 Standard; a 2016 floor and Azure support are separate compatibility assessments.

**The decisions** (PLAN.md §6):
- **14:** connection reuse comes through MCP first, on a reuse engine built as a library module; a per-profile daemon behind the CLI waits for evidence that Bash-driven use stays the main path.
- **15:** calls are stateless: each runs on a reset connection with its own wrapper, exactly like one CLI run. No session handles.
- **16:** read tools only. Writes stay on the CLI; an `execute` tool comes with Phase 6's command categorization.
- **17** (amended after spike 5.1): after a cancel, a connection the driver kept is reused, and the next call's verified reset is the check that it is clean; one the driver marked dead is discarded. (The driver gives no receipt for an acknowledged attention.)
- **18:** a server reaches the profiles listed at its launch, and tools take an optional `profile` among them.
- **19:** up to 2 connections per profile (`--pool`), closed after 15 idle minutes (`--idle`), and a dead reused connection reconnected and the call retried once.
- **20:** MCP ships in the one exe, `sqlh mcp`, behind a size gate: its cost stays within 300 KB, or the decision is reopened.

## 1. Why

The one-shot CLI starts a process, connects, logs in and sets up the session on every call, which costs about 150–250 ms on a LAN; on the dev LAN about 195 ms, 150 ms of it SQL Server 2025's login (docs/notes/connect-latency.md). An agent with limited context commonly makes several queries across turns, so that cost repeats on every one.

`sqlh mcp` runs as a long-lived stdio server instead:
- **Speed:** connections stay open, so a call costs one round trip plus execution.
- **Native tools:** harnesses such as Claude Code discover the tools natively, with schemas and descriptions.
- **Lifecycle and cancellation belong to the harness:** it starts sqlh with the session and ends it with the session, and cancels a call in the protocol (decision 14).
- **Read-only:** every tool is a read, in the rolled-back wrapper with the guard (decision 16), so a harness can allow the tools freely.
- **In the same exe, and free when unused** (decision 20): `sqlh mcp` is a subcommand of `sqlh.exe`. Its code runs only when a harness launches `sqlh mcp`, so using sqlh only as a CLI costs nothing for it at run time: no startup time, no memory, no open connections. The cost is on disk, about 0.2 MB, held to a budget (§8).

Fast cancellation and retry are essential; bulk analytics throughput is secondary. Exact total row-count usefulness remains unresolved, so the CLI's counting semantics (decision 6) carry over unchanged rather than becoming a new MCP requirement.

## 2. Protocol

**Revision.** The server is **dual-era**:
- It serves modern **2026-07-28** clients statelessly: per-request `_meta`, and no handshake.
- It also serves legacy clients that open with `initialize` (2025-11-25 and 2025-06-18), because harness support lags each spec revision. 2025-03-26 isn't served: it obliges a server to take JSON-RPC batches, which sqlh doesn't (an `initialize` asking for it is answered with 2025-11-25, and the client decides).

The era is picked by how the client opens, as the spec describes. An `initialize` request selects legacy semantics for the whole process.

**Transport.** stdio only.
- Messages are newline-delimited JSON-RPC 2.0 on stdin and stdout, and stdout carries nothing else.
- Logs go to stderr, only with `--verbose`.
- One message per line, at most 4 MB; a longer line is answered with `-32600` (no id) and skipped. A JSON array (a batch) is `-32600`; a response from the client is ignored.
- **The end:** when stdin closes, the calls in flight get up to 6 s to finish and answer (a client that pipes its requests and closes stdin gets its results), then what is left is cancelled and given 6 s more; on a console event or a broken stdout they are cancelled at once. Idle connections are closed and the process exits 0.

**Modern behavior (2026-07-28)**
- **Version check:** every request's `_meta["io.modelcontextprotocol/protocolVersion"]` is checked. An unsupported version gets `UnsupportedProtocolVersionError`: code `-32022`, with `data.supported` and `data.requested`. A request whose `_meta` lacks the version or `io.modelcontextprotocol/clientCapabilities`, before any `initialize`, gets `-32602`, as the spec requires of a malformed request.
- **`server/discover`** (mandatory) returns:
  - `supportedVersions: ["2026-07-28", "2025-11-25", "2025-06-18"]` (the legacy ones through `initialize`)
  - `capabilities: {"tools": {}}`
  - `_meta["io.modelcontextprotocol/serverInfo"]: {name: "sqlh", version}`
  - `instructions` (§6)
  - `ttlMs` and `cacheScope: "private"`. The instructions name the local profiles, so shared caches must not keep them.
- **Every result** carries `resultType: "complete"` and our `serverInfo` in `_meta`.
- **`tools/list`** returns the tools in a fixed order, with `ttlMs` and `cacheScope: "private"`. The set is the same for every launch (§4), never dependent on earlier calls, as the spec requires.
- **`notifications/cancelled`** for an in-flight `tools/call` cancels it with a TDS attention through the call's `Cancel`. Afterwards nothing is sent for that request id: the stdio binding says a server MUST NOT send further messages for a cancelled request (the cancellation page says SHOULD). A cancel for an unknown or finished id is ignored.

**Legacy behavior (initialize-based revisions)**
- `initialize` gets back the negotiated `protocolVersion` (the client's, if we support it, else our newest legacy version), `capabilities`, `serverInfo` and `instructions`.
- `notifications/initialized` is accepted.
- `ping` is answered (in both eras: harmless in the modern one, where the method was removed).
- Everything else is shared with modern mode.

**Not implemented**
- Resources, prompts, completion, `subscriptions/listen` and list-changed notifications: the tool list is fixed for the life of the process.
- Sampling, roots and logging: all deprecated in 2026-07-28.
- MRTR/elicitation: there's nothing to ask the user mid-call.
- Tasks.

Unknown methods, `subscriptions/listen` among them (sqlh advertises no list changes), get `-32601`; malformed params get `-32602`. An error whose request id couldn't be read leaves `id` out, as the MCP schema allows.

## 3. Launch and configuration

```
sqlh mcp --profile NAME [--profile NAME ...] [--max-rows N] [--max-cell N] [--max-output N] [-t seconds] [--pool N] [--idle MINUTES] [--verbose]
```

**The profiles listed are the server's whole reach** (decision 18). Each tool takes an optional `profile`, which must be one of them; with one listed it is the default, and with several a call without one is refused with the list. A profile not listed can't be reached, whatever a tool call asks. Strict separation stays possible: a profile can get its own server entry, listed alone, so the harness gates it separately.

**Connection settings are fixed at launch** by the profiles and flags, exactly as for the CLI. No tool argument can name a server, a login or a password. That prevents a prompt-injected agent from pointing the server at another host or harvesting a password (DESIGN.md §8). `--allow-write` isn't accepted (decision 16): a profile whose `write_policy` allows writes still gets only read tools.

**Launch rules.** `sqlh mcp` must be the first argument (`sqlh --profile x mcp` is refused, exit 3, where it would have run `mcp` as SQL). At least one `--profile`; one listed twice, `--allow-write`, connection flags (`-S`, `-U`, `-E`, `-d`), `--pool` outside 1–8 and `--max-output` under 4096 bytes are refused: a call's result is held in memory, so it can't be unlimited (0). A stdin that is a console is refused too: sqlh mcp is started by a harness, and never waits on a person. Launch errors go to stderr (`sqlh mcp: usage error: ...`), exit 3, and stdout stays empty; the panic hook leaves stdout alone too.

**Startup is fast and doesn't touch the database.** The profiles are read and checked at launch, each resolved as a run's would be with the launch flags, its password from Credential Manager as usual (an unknown profile is a usage error, exit 3, before any message is read); `server/discover` and `tools/list` answer without connecting. A profile's first tool call connects, so a harness can list sqlh's tools even while a database is down.

Claude Code:

```bash
claude mcp add sqlh -- C:/code/sqlharness/target/release/sqlh.exe mcp --profile test --profile reporting
```

Or check a project-scoped `.mcp.json` into the repo:

```json
{
  "mcpServers": {
    "sqlh": {
      "command": "C:/code/sqlharness/target/release/sqlh.exe",
      "args": ["mcp", "--profile", "test", "--profile", "reporting"]
    }
  }
}
```

## 4. Tools

Short names are used, because clients prefix them with the server name (Claude Code shows `mcp__sqlh__query`). The order below is the `tools/list` order. Every tool also takes `profile?` (§3), `database?`, `max_rows?` (0–10,000), `max_cell?` (0–1,000,000), `timeout?` (seconds, at least 1, capped at the profile's `-t`) and `format?` (`text`/`json`).

| Tool | Arguments | What it does |
|---|---|---|
| `query` | `sql` | Runs T-SQL under the read-only guard: the classifier, then the rolled-back wrapper transaction. A blocked write returns `isError` with the guard's message. |
| `check` | `sql` | Compile only (`SET NOEXEC ON`), with `--check`'s caveat about missing tables. |
| `info` | — | `sqlh info` |
| `databases` | — | `sqlh dbs` |
| `tables` | `pattern?` | `sqlh tables` |
| `describe` | `object` | `sqlh describe` |
| `find` | `pattern`, `in_definitions?` | `sqlh find` (`in_definitions` is `--text`) |
| `definition` | `object` | `sqlh def` |
| `deps` | `object` | `sqlh deps` |

**Notes on the tools**
- **Annotations:** every tool sets `readOnlyHint: true` and `openWorldHint: false`; the database is a closed system. Annotations describe the tools; they don't enforce anything (§7).
- **Input schemas:** JSON Schema 2020-12 with `additionalProperties: false`. Limits get bounds, for example `max_rows` ≤ 10,000 and `timeout` ≤ the launch `-t`. `profile` is an enum of the listed profiles.
- **No write tool** (decision 16). An `execute` tool, separate from `query` so a harness can gate it, comes with Phase 6's command categorization. Until then a write goes through the CLI, with its own safeguards.
- **Database argument:** on the first-release SQL Server target, `database` runs `USE [db]` (brackets escaped) at the start of that call only; the reset before the next call restores the profile's database (§5), so it never leaks. Future Azure SQL Database support requires a connection directly to the selected database rather than this switching mechanism.
- **Limits:** a call's limits are the launch limits (the profile's, then the flags), lowered or raised per call within the bounds above; the schema tools keep their own defaults (schema-commands.md §4) unless a launch flag or the call sets them. `--max-output` caps every result, as in the CLI.
- **No `$(var)`:** substitution is off for every call. In the CLI it reads the caller's own environment; here the environment is the server's, so a call could read what the harness keeps there.
- **SQL text** over 1 MB is refused as a tool error (exit 3).
- **The JSON document's `source`** for `query` and `check` is `-Q`, as in the CLI (a schema tool's is its command's name); text shows no source for a single batch.

**Results**
- One `text` content item in the sqlh text format, with the same contract, limits and final status line as the CLI. The models using this were tuned for the CLI output, and it's the cheapest in tokens.
- `format: "json"` returns the JSON document as text instead.
- No `structuredContent`/`outputSchema` for now. The spec suggests duplicating structured content as text, which would double the tokens of every result (§10).

**Errors**
- `isError: true` whenever the run's exit code isn't 0: SQL errors, a blocked write, a timeout, a lost connection. The text includes the error lines and the status, so the model can correct itself.
- JSON-RPC protocol errors only for a malformed request, an unknown tool (`-32602`, `Unknown tool: X`), or arguments that don't fit the tool's schema (`-32602`, `Invalid arguments for tool X: ...`: not an object, an unknown argument, a wrong type, a missing required one, a number out of bounds, a profile outside the list). Everything the run itself finds (a bad object name, a refused write, a SQL error, a timeout, no connection) is a tool error the model can read.
- **A refused write's advice** says `sqlh mcp is read-only`, never `--allow-write`, which the server can't take (`guard::WritesOff`, as 2.16 did for `deny` profiles).

## 5. Connections

Every call is stateless (decision 15): MCP 2026-07-28 has no protocol sessions, and sqlh keeps none of its own.

**The engine** (decision 14) is a library module, separate from the MCP front end, so a daemon behind the CLI could later use it unchanged. It keeps a small pool per listed profile and runs each call through the execution core (`run`), exactly as the CLI does.

**Reset before every call**
- A reused connection is reset with the TDS RESETCONNECTION flag on the call's first request (mssql-tds `prepare_reset_connection`, the mechanism ADO.NET pooling uses). That restores the login's defaults, drops temp tables, rolls back a stray transaction and restores the profile's database. Then the session setup runs (the agent defaults, DESIGN.md §6.7), followed by the read-only wrapper's `BEGIN TRANSACTION`.
- Every call therefore behaves exactly like a one-shot CLI run, just without the connect cost. Within one call, its batches share the connection and its temp tables, as a CLI run's do.

**Pool** (decision 19)
- Up to 2 connections per listed profile by default (`--pool N`, 1–8), each opened only when a call needs one, so a second exists only when calls overlap. Calls beyond the pool wait for a connection, and that wait counts toward the call's timeout.
- A connection idle for 15 minutes is closed (`--idle MINUTES`; 0 keeps connections open while the server runs). The next call reconnects.
- A reused connection found dead (a network drop, a session a DBA killed) is reconnected and the call retried once, silently; a retry is safe because every call is a read in a rolled-back wrapper. If the retry fails, the call gets the real error. **The rule as built:** the call's first request (the session setup, which carries the reset, `run::open`) failed, the driver marked the connection dead or it was retired, and the error isn't a timeout, a cancel or a SQL error. A dead idle connection shows up as `Lost` (the socket was closed); an unacknowledged reset as `Protocol`. The retry connects afresh, and the profile's other idle connections are dropped, since they most likely died the same way. A call that failed after its first request is not retried, and its connection is discarded.
- **The wait for a connection** is bounded by the call's timeout: past it, the call fails with exit 4, `query timeout after N s waiting for a free connection (all M of the profile's connections were busy); raise -t, or --pool for more connections`. A cancel ends the wait too.
- **A reused connection's status line** says `connect 0 ms` (JSON `connect_ms: 0`): the call paid no connect.
- **Putting a connection back:** a connection the driver kept goes back to the pool, with its wrapper rolled back first if a cancel left it open (`IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION`), so an idle connection holds no transaction; if that fails, or takes more than 2 s (the driver's own timeout and attention wait included), the connection is dropped instead, so a server gone silent can't hold up the call's reply.
- **A call whose first request fails** and isn't retried ends without the status line's `server/db, connect N ms` tail: a connection is described only once it is ready (`run::open` succeeded), so a retry never describes the dead one.

**After a cancel** (decision 17)
- A call ends with a TDS attention when the client cancels it (`notifications/cancelled`), when its timeout fires, or when the bounded count of hidden rows stops at its budget (decision 6). Count stops end any query with many hidden rows, so this is routine.
- When the driver kept the connection (it settled the cancel within sqlh's settle bound, R15 B, and didn't mark it dead), the connection goes back to the pool, and the reset before the next call clears what the cancelled call left. mssql-tds 0.2.0 gives no receipt for an acknowledged attention: a kept connection also follows a cancel before the request was sent and a half-sent request it withdrew (upstream-issues U4), all in step with the server.
- **The check is the reset itself:** the driver verifies that the reply to the next request starts with the server's ResetConnection acknowledgement, or fails that request (`ConnectionResetNotAcknowledged`) and marks the connection dead, so a session in an unknown state is never trusted. Then the connection is discarded and the call retried once on a new one (§5 Pool).
- A connection the driver marked dead is discarded: an attention the server never acknowledged (today's `COUNT_STOP_UNCONFIRMED`), a protocol error, a lost connection; so is one past sqlh's settle grace. The next call reconnects.
- **A cancelled call gets 5 s to settle** (the driver's own wait for an attention's acknowledgement); past it, the call is dropped where it stands, and its connection with it: `cancelled: the call didn't finish within 5 s of the cancel, so its connection was closed`.

**Concurrency**
- Requests are read and dispatched concurrently on the current-thread runtime. Database work is serialized per connection.
- Each call has a timeout (the default from `-t`/the profile, overridable per call up to the launch value) that sends a TDS attention when it fires.

## 6. Instructions (`server/discover` / `initialize`)

Short guidance the model sees once:

> sqlh runs read-only T-SQL against SQL Server. Profiles: {name — description, for each listed profile}{; pass `profile` to choose one, if several}. Use `describe`, `find` or `deps` before writing queries against unfamiliar tables. Results come in a compact text format: `-- ` lines are metadata, the header is `name:type`, rows are tab-separated, NULL is `NULL`, and the last line is the status. Results are capped at {max_rows} rows and {max_cell} characters per value; raise `max_rows` only when you need more. Every call is read-only (writes are refused and rolled back) and starts fresh: temp tables last only within one call.

The profile names and descriptions come from the profiles file at launch, without connecting. Server version details appear only after a call, via `info`. Profile descriptions are the owner's text (4.1), escaped like any metadata field.

## 7. Security

Database permissions are expected to be configured appropriately. The tool's checks also protect against accidental damage when an agent receives excessive privileges. Command categorization and allowlist/blocklist policy need further design around legitimate DBA workflows (Phase 6); the current classifier is an MVP mechanism. Tool annotations do not replace database enforcement.

- **Fixed reach:** the profiles listed at launch are the only targets, connection settings and credentials are fixed at launch (§3), and tool arguments never carry secrets.
- **Read-only:** every tool runs under the guard and in the rolled-back wrapper; there is no write tool, and `--allow-write` isn't accepted (decision 16).
- **Isolation between calls:** the reset before every call (§5), and a connection discarded whenever the driver lost track of it, with the reset's acknowledgement verified on every reuse.
- **Input limits:** a JSON message over 4 MB is answered with `-32600` and skipped, and nothing else changes. SQL text over 1 MB is refused (a tool error). Pattern and object arguments are passed as literals (`docs/schema-commands.md` §3).
- **No `$(var)`:** a call can't read the server's environment (§4).
- **Output limits:** limits always apply. Control characters are escaped by the text format, so database contents can't forge protocol framing; results are JSON-encoded inside the response anyway.
- **Prompt injection through data:** rows can contain text that reads like instructions. sqlh can't prevent that, and the instructions don't pretend otherwise. The harness's tool-result handling is the defense.
- **No secrets in logs:** stderr logs (`--verbose`) never include passwords or connection strings, and no general logger is installed while the driver runs (AGENTS.md).

## 8. Implementation (built 2026-10-17, PLAN.md 5.2 and 5.3)

Prerequisites, done: the execution correctness findings (Phase 1), dependable output and cancellation (Phase 2), and the execution core (`src/run.rs`, PLAN.md 3.1). Bounded output must cover the collected MCP response as well as streamed rows.

1. **The reuse engine** (decision 14), a library module with no MCP in it:
   - a pool per profile: lazy connect, at most `--pool` connections, idle close, the dead-connection retry (decision 19)
   - reset on reuse, and the cancel rule (decision 17, amended): a connection the driver kept is reused, one it marked dead is discarded, and a reset the server didn't acknowledge discards the connection and retries the call once
   - runs a call through `run` into any `Output`; the MCP front end renders into a `TextSink<Vec<u8>>` (or the JSON sink) and returns the bytes as the tool text
2. **`src/mcp/`**, the front end:
   - `mod.rs`: an async stdin line reader and a single stdout writer task fed by a channel, so responses never interleave; request dispatch; the cancellation map, request id → `Cancel`
   - `protocol.rs`: JSON-RPC envelopes, per-request `_meta` and era handling; `server/discover`, `initialize`, `ping`, `tools/list`, errors
   - `tools.rs`: tool definitions (names, schemas, annotations, descriptions) as static JSON; argument validation, the profile allowlist, and the mapping to `run` and `schema`
3. **Dependency and the size gate** (decision 20): `serde_json`, already a test-only dependency, becomes a normal one for parsing untrusted JSON safely. Use `serde_json::Value` without serde derive, to keep size down; it costs 65 KB (spike 5.1). All of MCP must stay within 300 KB of extra exe size, measured with `cargo bloat --release --crates` and the exe size before and after, and the exe within the 3 MB budget; past either, decision 20 (one exe) is reopened. **Measured 2026-10-17:** 2,762,240 bytes against 2,487,808 before Phase 5: 268 KB for the engine, the run's split, `src/mcp` and `serde_json` together (`cargo bloat` isn't installed on the dev machine, so the split by crate isn't measured; `serde_json` alone was 65 KB). Within the gate. If headroom is needed, the tool definitions could come from a static JSON string instead of `json!` code.
4. **Driver check, done (spike 5.1):** `prepare_reset_connection` drops temp tables, rolls back an open transaction, restores the login's database and `SET`s, on the same session, at no measurable cost (docs/notes/mcp-reuse-spike.md); `db::spike::spike_reset` asserts it live.
5. **Docs:** DESIGN.md §10 points here; `--help` gets an MCP line; the README gets the Claude Code setup snippet and its "Built for agents" entries.

## 9. Testing

- **Protocol tests, with no database** (built: `src/mcp` unit tests, and `tests/cli.rs` `mcp::` through the real binary, a modern and a legacy session and the launch errors): feed scripted stdin lines and check the stdout lines:
  - discover and tools/list (deterministic order, `cacheScope`)
  - a version mismatch (`-32022`)
  - legacy `initialize`, `initialized` and `ping`
  - an unknown method or tool, bad arguments, a profile outside the list
  - a cancel for an unknown id (ignored)
  - stdin EOF leading to exit
- **Engine tests against a fake TDS server** (the loopback server `src/db` already uses): an unacknowledged or stalled attention discards the connection and the next call reconnects; a reset the server doesn't acknowledge discards the connection and retries the call once; a connection dropped while idle is reconnected and the call retried once; a failure mid-run is not retried; the pool's bound and its idle close.
- **Tool tests against the test server** (5.4; a live session was checked by hand on 2026-10-17: reuse at `connect 0 ms` on the same SPID, a cancel then a clean call on that SPID, a timeout, `database` and its reset, JSON, `check`, `find`, a profile outside the list):
  - `query` happy path, SQL error leading to `isError`, a blocked write
  - timeout and cancellation (send `notifications/cancelled` mid-query)
  - reset hygiene: a `USE`, a `SET` and a temp table in one call don't leak into the next
  - a cancel the driver kept leaves the next call clean: `@@TRANCOUNT` 0, the cancelled call's temp table gone, the agent session's `SET`s restored (decision 17's requirement)
  - two listed profiles, each reached by `profile`; a profile not listed refused
- **Client interop:** connect from Claude Code (`claude mcp add`, then use the tools in a session) and from the MCP Inspector, as both a modern and a legacy client.

## 10. Open questions

Settled by the decisions: cancellation and reuse (17), pool size (19), sessions and their limits (15: none for now).

- **Exact totals:** do total row counts improve investigation enough to justify draining results? Assess cutoff behavior separately; cancellation must be quick and partial counts must be explicit.
- **Command categorization:** evolve checks for DBA workflows and overprivileged connections without relying on tool annotations as enforcement (Phase 6, which also brings the `execute` tool).
- **Compatibility expansion:** assess the candidate 2016 floor and other platforms after validating the 2025 Standard first-release target.
- **structuredContent:** worth adding as an option (`format: "structured"`) once a client benefits from it? Not by default (§4).
- **Resources:** expose the schema as resources (`sqlh://schema/dbo.Users`) for clients that browse resources? Tools cover the need, so it's deferred.
- **The `database` argument:** allow switching databases per call, or pin to the profile's database? As designed it's allowed: permissions still bound it, it's explicit in every call, and the reset keeps it from leaking. Revisit if it surprises anyone.
- **Sessions and a daemon:** add them on the same engine only if an agent trial shows the need (decisions 14, 15).
