# MCP mode: design and plan

Status: proposed 2026-10-09; its shape decided by the owner on 2026-10-15 to 2026-10-17 (PLAN.md §6, decisions 14–19), to be built in Phase 5. This doc expands DESIGN.md §10 and becomes the spec once built. It targets MCP revision **2026-07-28** and also serves clients on the older handshake revisions; the protocol details in §2 are to be checked against the published revision before they are built. The tools it exposes are the CLI's query path and the schema commands (`docs/schema-commands.md`).

Product direction (2026-10-10): [VISION.md](VISION.md) makes low repeated-call overhead and quick cancellation priorities for human-led DBA/developer investigations. First-release validation targets SQL Server 2025 Standard on Windows Server 2025 Standard; a 2016 floor and Azure support are separate compatibility assessments.

**The decisions** (PLAN.md §6):
- **14:** connection reuse comes through MCP first, on a reuse engine built as a library module; a per-profile daemon behind the CLI waits for evidence that Bash-driven use stays the main path.
- **15:** calls are stateless: each runs on a reset connection with its own wrapper, exactly like one CLI run. No session handles.
- **16:** read tools only. Writes stay on the CLI; an `execute` tool comes with Phase 6's command categorization.
- **17:** a connection is reused after a cancel only when the server confirmed it; anything unclear discards it.
- **18:** a server reaches the profiles listed at its launch, and tools take an optional `profile` among them.
- **19:** up to 2 connections per profile (`--pool`), closed after 15 idle minutes (`--idle`), and a dead reused connection reconnected and the call retried once.

## 1. Why

The one-shot CLI starts a process, connects, logs in and sets up the session on every call, which costs about 150–250 ms on a LAN; on the dev LAN about 195 ms, 150 ms of it SQL Server 2025's login (docs/notes/connect-latency.md). An agent with limited context commonly makes several queries across turns, so that cost repeats on every one.

`sqlh mcp` runs as a long-lived stdio server instead:
- **Speed:** connections stay open, so a call costs one round trip plus execution.
- **Native tools:** harnesses such as Claude Code discover the tools natively, with schemas and descriptions.
- **Lifecycle and cancellation belong to the harness:** it starts sqlh with the session and ends it with the session, and cancels a call in the protocol (decision 14).
- **Read-only:** every tool is a read, in the rolled-back wrapper with the guard (decision 16), so a harness can allow the tools freely.

Fast cancellation and retry are essential; bulk analytics throughput is secondary. Exact total row-count usefulness remains unresolved, so the CLI's counting semantics (decision 6) carry over unchanged rather than becoming a new MCP requirement.

## 2. Protocol

**Revision.** The server is **dual-era**:
- It serves modern **2026-07-28** clients statelessly: per-request `_meta`, and no handshake.
- It also serves legacy clients that open with `initialize` (2025-11-25, 2025-06-18 and 2025-03-26), because harness support lags each spec revision.

The era is picked by how the client opens, as the spec describes. An `initialize` request selects legacy semantics for the whole process.

**Transport.** stdio only.
- Messages are newline-delimited JSON-RPC 2.0 on stdin and stdout, and stdout carries nothing else.
- Logs go to stderr, only with `--verbose`.
- The server exits when stdin closes, closing its connections.

**Modern behavior (2026-07-28)**
- **Version check:** every request's `_meta["io.modelcontextprotocol/protocolVersion"]` is checked. An unsupported version gets `UnsupportedProtocolVersionError`: code `-32022`, with `data.supported` and `data.requested`.
- **`server/discover`** (mandatory) returns:
  - `supportedVersions: ["2026-07-28", "2025-11-25"]`
  - `capabilities: {"tools": {}}`
  - `_meta["io.modelcontextprotocol/serverInfo"]: {name: "sqlh", version}`
  - `instructions` (§6)
  - `ttlMs` and `cacheScope: "private"`. The instructions name the local profiles, so shared caches must not keep them.
- **Every result** carries `resultType: "complete"` and our `serverInfo` in `_meta`.
- **`tools/list`** returns the tools in a fixed order, with `ttlMs` and `cacheScope: "private"`. The set is the same for every launch (§4), never dependent on earlier calls, as the spec requires.
- **`notifications/cancelled`** for an in-flight `tools/call` cancels it with a TDS attention through the call's `Cancel`. Afterwards no response is sent for that request id, as the spec requires.

**Legacy behavior (initialize-based revisions)**
- `initialize` gets back the negotiated `protocolVersion` (the client's, if we support it, else our newest legacy version), `capabilities`, `serverInfo` and `instructions`.
- `notifications/initialized` is accepted.
- `ping` is answered.
- Everything else is shared with modern mode.

**Not implemented**
- Resources, prompts, completion, `subscriptions/listen` and list-changed notifications: the tool list is fixed for the life of the process.
- Sampling, roots and logging: all deprecated in 2026-07-28.
- MRTR/elicitation: there's nothing to ask the user mid-call.
- Tasks.

Unknown methods get `-32601`, and malformed params get `-32602`.

## 3. Launch and configuration

```
sqlh mcp --profile NAME [--profile NAME ...] [--max-rows N] [--max-cell N] [--max-output N] [-t seconds] [--pool N] [--idle MINUTES] [--verbose]
```

**The profiles listed are the server's whole reach** (decision 18). Each tool takes an optional `profile`, which must be one of them; with one listed it is the default, and with several a call without one is refused with the list. A profile not listed can't be reached, whatever a tool call asks. Strict separation stays possible: a profile can get its own server entry, listed alone, so the harness gates it separately.

**Connection settings are fixed at launch** by the profiles and flags, exactly as for the CLI. No tool argument can name a server, a login or a password. That prevents a prompt-injected agent from pointing the server at another host or harvesting a password (DESIGN.md §8). `--allow-write` isn't accepted (decision 16): a profile whose `write_policy` allows writes still gets only read tools.

**Startup is fast and doesn't touch the database.** The profiles are read and checked at launch (an unknown profile is a usage error, exit 3, before any message is read); `server/discover` and `tools/list` answer without connecting. A profile's first tool call connects, so a harness can list sqlh's tools even while a database is down.

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

Short names are used, because clients prefix them with the server name (Claude Code shows `mcp__sqlh__query`). The order below is the `tools/list` order. Every tool takes `profile?` (§3) and `database?`.

| Tool | Arguments | What it does |
|---|---|---|
| `query` | `sql`, `max_rows?`, `max_cell?`, `timeout?`, `format?` (`text`/`json`) | Runs T-SQL under the read-only guard: the classifier, then the rolled-back wrapper transaction. A blocked write returns `isError` with the guard's message. |
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
- **Limits:** a call's limits are the launch limits (the profile's, then the flags), lowered or raised per call within the bounds above; the schema tools keep their own defaults (schema-commands.md §4). `--max-output` caps every result, as in the CLI.

**Results**
- One `text` content item in the sqlh text format, with the same contract, limits and final status line as the CLI. The models using this were tuned for the CLI output, and it's the cheapest in tokens.
- `format: "json"` returns the JSON document as text instead.
- No `structuredContent`/`outputSchema` for now. The spec suggests duplicating structured content as text, which would double the tokens of every result (§10).

**Errors**
- `isError: true` whenever the run's exit code isn't 0: SQL errors, a blocked write, a timeout, a lost connection. The text includes the error lines and the status, so the model can correct itself.
- JSON-RPC protocol errors only for a malformed request, an unknown tool, or invalid arguments (`-32602`), a profile outside the list included.

## 5. Connections

Every call is stateless (decision 15): MCP 2026-07-28 has no protocol sessions, and sqlh keeps none of its own.

**The engine** (decision 14) is a library module, separate from the MCP front end, so a daemon behind the CLI could later use it unchanged. It keeps a small pool per listed profile and runs each call through the execution core (`run`), exactly as the CLI does.

**Reset before every call**
- A reused connection is reset with the TDS RESETCONNECTION flag on the call's first request (mssql-tds `prepare_reset_connection`, the mechanism ADO.NET pooling uses). That restores the login's defaults, drops temp tables, rolls back a stray transaction and restores the profile's database. Then the session setup runs (the agent defaults, DESIGN.md §6.7), followed by the read-only wrapper's `BEGIN TRANSACTION`.
- Every call therefore behaves exactly like a one-shot CLI run, just without the connect cost. Within one call, its batches share the connection and its temp tables, as a CLI run's do.

**Pool** (decision 19)
- Up to 2 connections per listed profile by default (`--pool N`, 1–8), each opened only when a call needs one, so a second exists only when calls overlap. Calls beyond the pool wait for a connection, and that wait counts toward the call's timeout.
- A connection idle for 15 minutes is closed (`--idle MINUTES`; 0 keeps connections open while the server runs). The next call reconnects.
- A reused connection found dead (a network drop, a session a DBA killed) is reconnected and the call retried once, silently; a retry is safe because every call is a read in a rolled-back wrapper. If the retry fails, the call gets the real error. A call is retried only when it failed before any of its work could have run; one that failed mid-run is not retried, and its connection is discarded.

**After a cancel** (decision 17)
- A call ends with a TDS attention when the client cancels it (`notifications/cancelled`), when its timeout fires, or when the bounded count of hidden rows stops at its budget (decision 6). Count stops end any query with many hidden rows, so this is routine.
- When the server acknowledged the attention within the settle bound (R15 B), the connection goes back to the pool, and the reset before the next call clears what the cancelled call left.
- Anything less discards the connection: no acknowledgement (today's `COUNT_STOP_UNCONFIRMED`), the settle grace running out, a protocol error, a lost connection. The next call reconnects.

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
- **Isolation between calls:** the reset before every call (§5), and a connection discarded whenever a cancel wasn't confirmed.
- **Input limits:** a JSON message over 4 MB is answered with a protocol error and its connection state is left untouched. SQL text over 1 MB is refused. Pattern and object arguments are passed as literals (`docs/schema-commands.md` §3).
- **Output limits:** limits always apply. Control characters are escaped by the text format, so database contents can't forge protocol framing; results are JSON-encoded inside the response anyway.
- **Prompt injection through data:** rows can contain text that reads like instructions. sqlh can't prevent that, and the instructions don't pretend otherwise. The harness's tool-result handling is the defense.
- **No secrets in logs:** stderr logs (`--verbose`) never include passwords or connection strings, and no general logger is installed while the driver runs (AGENTS.md).

## 8. Implementation plan

Prerequisites, done: the execution correctness findings (Phase 1), dependable output and cancellation (Phase 2), and the execution core (`src/run.rs`, PLAN.md 3.1). Bounded output must cover the collected MCP response as well as streamed rows.

1. **The reuse engine** (decision 14), a library module with no MCP in it:
   - a pool per profile: lazy connect, at most `--pool` connections, idle close, the dead-connection retry (decision 19)
   - reset on reuse, and the confirmed-cancel rule (decision 17); `src/db/` reports whether a call's attention was acknowledged, and the engine discards on anything less
   - runs a call through `run` into any `Output`; the MCP front end renders into a `TextSink<Vec<u8>>` (or the JSON sink) and returns the bytes as the tool text
2. **`src/mcp/`**, the front end:
   - `mod.rs`: an async stdin line reader and a single stdout writer task fed by a channel, so responses never interleave; request dispatch; the cancellation map, request id → `Cancel`
   - `protocol.rs`: JSON-RPC envelopes, per-request `_meta` and era handling; `server/discover`, `initialize`, `ping`, `tools/list`, errors
   - `tools.rs`: tool definitions (names, schemas, annotations, descriptions) as static JSON; argument validation, the profile allowlist, and the mapping to `run` and `schema`
3. **Dependency:** `serde_json`, already a test-only dependency, becomes a normal one for parsing untrusted JSON safely. Use `serde_json::Value` without serde derive, to keep size down. Budget: under 300 KB of extra exe size, measured after the first build.
4. **Driver check:** confirm `prepare_reset_connection` behaves as expected: temp tables gone, database restored, open transaction rolled back, `SET`s back to the login's defaults. If it doesn't, fall back to reconnecting for every call, which is slower but correct, and tell the owner.
5. **Docs:** DESIGN.md §10 points here; `--help` gets an MCP line; the README gets the Claude Code setup snippet and its "Built for agents" entries.

## 9. Testing

- **Protocol unit tests, with no database:** feed scripted stdin lines and check the stdout lines:
  - discover and tools/list (deterministic order, `cacheScope`)
  - a version mismatch (`-32022`)
  - legacy `initialize`, `initialized` and `ping`
  - an unknown method or tool, bad arguments, a profile outside the list
  - a cancel for an unknown id (ignored)
  - stdin EOF leading to exit
- **Engine tests against a fake TDS server** (the loopback server `src/db` already uses): an unacknowledged or stalled attention discards the connection and the next call reconnects; a connection dropped while idle is reconnected and the call retried once; a failure mid-run is not retried; the pool's bound and its idle close.
- **Tool tests against the test server:**
  - `query` happy path, SQL error leading to `isError`, a blocked write
  - timeout and cancellation (send `notifications/cancelled` mid-query)
  - reset hygiene: a `USE`, a `SET` and a temp table in one call don't leak into the next
  - a confirmed cancel leaves the next call clean: `@@TRANCOUNT` 0, the cancelled call's temp table gone, the agent session's `SET`s restored (decision 17's requirement)
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
