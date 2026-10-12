# MCP mode: design and plan

Status: proposed (2026-10-09). This doc expands DESIGN.md §10 and becomes the spec once built. It targets MCP revision **2026-07-28** and also serves clients on the older handshake revisions. The tools it exposes are the CLI's query path and the schema commands (`docs/schema-commands.md`).

Product direction updated 2026-10-10: [VISION.md](VISION.md) makes low repeated-call overhead and quick cancellation priorities for human-led DBA/developer investigations. MCP is a proposed way to serve that workflow, not a commitment to every pooling or session detail below. First-release validation targets SQL Server 2025 Standard on Windows Server 2025 Standard; a 2016 floor and Azure support are separate compatibility assessments.

## 1. Why

The one-shot CLI starts a process, connects, logs in and sets up the session on every call, which costs about 150–250 ms on a LAN. It also loses all session state.

An agent with limited context commonly makes several queries across turns. Reusing connections and exposing focused discovery tools should shorten time to the next useful answer. Fast cancellation and retry are essential; bulk analytics throughput is secondary. Exact total row-count usefulness remains unresolved, so the current CLI's draining/counting semantics must not silently become an immutable MCP requirement.

`sqlh mcp` runs as a long-lived stdio server instead:
- **Speed:** connections stay open, so a call costs one round trip plus execution.
- **Native tools:** harnesses such as Claude Code discover the tools natively, with schemas and descriptions.
- **Permission gating:** reads and writes are separate tools, so a harness's permission prompts can allow reads freely and gate writes.
- **Session state:** state such as temp tables can be kept deliberately, through explicit session handles.

## 2. Protocol

**Revision.** The server is **dual-era**:
- It serves modern **2026-07-28** clients statelessly: per-request `_meta`, and no handshake.
- It also serves legacy clients that open with `initialize` (2025-11-25, 2025-06-18 and 2025-03-26), because harness support lags each spec revision.

The era is picked by how the client opens, as the spec describes. An `initialize` request selects legacy semantics for the whole process.

**Transport.** stdio only.
- Messages are newline-delimited JSON-RPC 2.0 on stdin and stdout, and stdout carries nothing else.
- Logs go to stderr, only with `--verbose`.
- The server exits when stdin closes.

**Modern behavior (2026-07-28)**
- **Version check:** every request's `_meta["io.modelcontextprotocol/protocolVersion"]` is checked. An unsupported version gets `UnsupportedProtocolVersionError`: code `-32022`, with `data.supported` and `data.requested`.
- **`server/discover`** (mandatory) returns:
  - `supportedVersions: ["2026-07-28", "2025-11-25"]`
  - `capabilities: {"tools": {}}`
  - `_meta["io.modelcontextprotocol/serverInfo"]: {name: "sqlh", version}`
  - `instructions` (§6)
  - `ttlMs` and `cacheScope: "private"`. The tool set depends on the local profile, so shared caches must not keep it.
- **Every result** carries `resultType: "complete"` and our `serverInfo` in `_meta`.
- **`tools/list`** returns the tools in a fixed order, with `ttlMs` and `cacheScope: "private"`. The set depends only on launch configuration (§4), never on earlier calls, as the spec requires.
- **`notifications/cancelled`** for an in-flight `tools/call` cancels it with a TDS attention through the session's `Cancel`. Afterwards no response is sent for that request id, as the spec requires.

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
sqlh mcp [--profile NAME] [--max-rows N] [--max-cell N] [-t seconds] [--allow-write] [--pool N] [--sessions N] [--verbose]
```

**Connection settings are fixed at launch** by the profile and flags, exactly as for the CLI. No tool argument can name a server, a login or a password. That prevents a prompt-injected agent from pointing the server at another host or harvesting a password (DESIGN.md §8).

**Startup is fast and doesn't touch the database.** `server/discover` and `tools/list` answer without connecting. The first tool call connects, so a harness can list sqlh's tools even while the database is down.

Claude Code:

```bash
claude mcp add sqlh -- C:/code/sqlharness/target/release/sqlh.exe mcp --profile test
```

Or check a project-scoped `.mcp.json` into the repo:

```json
{
  "mcpServers": {
    "sqlh": {
      "command": "C:/code/sqlharness/target/release/sqlh.exe",
      "args": ["mcp", "--profile", "test"]
    }
  }
}
```

## 4. Tools

Short names are used, because clients prefix them with the server name (Claude Code shows `mcp__sqlh__query`). The order below is the `tools/list` order.

| Tool | Arguments | Annotations | What it does |
|---|---|---|---|
| `query` | `sql`, `database?`, `max_rows?`, `max_cell?`, `timeout?`, `format?` (`text`/`json`), `session?` | readOnlyHint | Runs T-SQL under the read-only guard: the classifier, then the rolled-back wrapper transaction. A blocked write returns `isError` with the guard's message. |
| `check` | `sql`, `database?` | readOnlyHint | Compile only (`SET NOEXEC ON`), with `--check`'s caveat about missing tables. |
| `info` | — | readOnlyHint | `sqlh info` |
| `databases` | — | readOnlyHint | `sqlh dbs` |
| `tables` | `pattern?`, `database?` | readOnlyHint | `sqlh tables` |
| `describe` | `object`, `database?`, `session?` | readOnlyHint | `sqlh describe` (with `session`, `#temp` tables work) |
| `find` | `pattern`, `in_definitions?`, `database?` | readOnlyHint | `sqlh find` |
| `definition` | `object`, `database?` | readOnlyHint | `sqlh def` |
| `session_open` | `database?`, `writes?` (listed only when writes are allowed) | readOnlyHint | Opens a session handle (§5) |
| `session_close` | `session` | readOnlyHint, idempotentHint | Ends a session and rolls back its work |
| `execute` | `sql`, `database?`, `timeout?`, `session?` | destructiveHint | Runs T-SQL with writes allowed. **Listed only** when sqlh is launched with `--allow-write` and the profile's write policy isn't `deny`. |

**Notes on the tools**
- **Annotations:** all tools set `openWorldHint: false`; the database is a closed system.
- **Input schemas:** JSON Schema 2020-12 with `additionalProperties: false`. Limits get bounds, for example `max_rows` ≤ 10,000 and `timeout` ≤ the launch `-t`.
- **Read vs write:** keeping `execute` separate from `query` is the key safety property. Read tools can be auto-approved, and every write goes through the harness's confirmation prompt.
- **Database argument:** on the first-release SQL Server target, `database` runs `USE [db]` (brackets escaped) at the start of that call only. With a session, it applies to the session from then on. Stateless calls never leak a database switch to the next call (§5). Future Azure SQL Database support requires a connection directly to the selected database rather than this switching mechanism.

**Results**
- One `text` content item in the sqlh text format, with the same contract, limits and final status line as the CLI. The models using this were tuned for the CLI output, and it's the cheapest in tokens.
- `format: "json"` returns the JSON document as text instead.
- No `structuredContent`/`outputSchema` for now. The spec suggests duplicating structured content as text, which would double the tokens of every result (§10).

**Errors**
- `isError: true` whenever the run's exit code isn't 0: SQL errors, a blocked write, a timeout, a lost connection. The text includes the error lines and the status, so the model can correct itself.
- JSON-RPC protocol errors only for a malformed request, an unknown tool, or invalid arguments (`-32602`).

## 5. Connections and sessions

MCP 2026-07-28 has no protocol sessions; state across calls should use explicit handles. sqlh follows that literally:

**Stateless calls (no `session` argument)**
- They run on a small pool of pooled connections, size 1 by default and `--pool N` to raise it.
- Before each call the connection is reset with the TDS RESETCONNECTION flag (mssql-tds `prepare_reset_connection`, the same mechanism as ADO.NET pooling). That restores login defaults, drops temp tables, rolls back stray transactions and restores the profile's database. Then the session setup runs, followed by the wrapper `BEGIN TRANSACTION` for read-only tools.
- Every stateless call therefore behaves exactly like a one-shot CLI run, just without the connect cost.

**Sessions (`session_open` → `ses_<uuid>`)**
- Each session gets its **own dedicated connection**, so temp tables, `USE` and session `SET`s persist across calls that pass the handle.
- **Read-only sessions** keep **one wrapper transaction for their whole life**: temp tables persist, and everything is rolled back when the session closes. The guard's classifier still applies to `query` calls.
- **Limits:**
  - at most `--sessions N` open sessions (default 4)
  - idle expiry after 10 minutes
  - a hard cap of 60 minutes, so a long-open transaction can't hold tempdb log space indefinitely
  - the `session_open` description states these, so the model knows before creating one
- **Expiry and failures:** an expired or unknown handle returns `isError` that says to open a new session. If a session's connection breaks, the session ends and the error says so; it is never silently replaced.
- **Writes:** an `execute` call against a read-only session is refused. A write session is opened with `session_open {"writes": true}`, which is listed only when writes are allowed. It has no wrapper transaction; writes commit normally.

**Concurrency**
- Requests are read and dispatched concurrently. Database work is serialized per connection, and stateless calls queue for a pooled connection.
- Each call has a timeout (the default from `-t`/the profile, overridable per call up to the launch value) that sends a TDS attention when it fires.
- **Reconnect:** a broken pooled connection is reconnected on the next call. The call that saw the failure gets `isError` and is not retried automatically, because retrying could run a write twice.

## 6. Instructions (`server/discover` / `initialize`)

Short guidance the model sees once:

> sqlh runs T-SQL against SQL Server {version} (database {db}). Use `describe` or `find` before writing queries against unfamiliar tables. Results come in a compact text format: `-- ` lines are metadata, the header is `name:type`, rows are tab-separated, NULL is `NULL`, and the last line is the status. Results are capped at {max_rows} rows and {max_cell} characters per value; raise `max_rows` only when you need more. `query` is read-only (writes are refused and rolled back). {if writes: Use `execute` for writes.} Temp tables only persist inside a `session_open` handle.

The `{version}` and `{db}` placeholders are filled from the profile without connecting. Version details appear only after the first call, via `info`.

## 7. Security

Database permissions are expected to be configured appropriately. The tool's checks also protect against accidental damage when an agent receives excessive privileges. Command categorization and allowlist/blocklist policy need further design around legitimate DBA workflows; the current classifier is an MVP mechanism. Tool separation and annotations do not replace database enforcement.

- **Fixed target:** connection target and credentials are fixed at launch (§3), and tool arguments never carry secrets.
- **Writes are gated three ways:** the launch flag, the profile policy and a separate `execute` tool. The read-only guard covers `query`.
- **Input limits:** a JSON message over 4 MB is answered with a protocol error and its connection state is left untouched. SQL text over 1 MB is refused. Pattern and object arguments are passed as literals (`docs/schema-commands.md` §3).
- **Output limits:** limits always apply. Control characters are escaped by the text format, so database contents can't forge protocol framing; results are JSON-encoded inside the response anyway.
- **Prompt injection through data:** rows can contain text that reads like instructions. sqlh can't prevent that, and the instructions don't pretend otherwise. The harness's tool-result handling is the defense.
- **No secrets in logs:** stderr logs (`--verbose`) never include passwords or connection strings.

## 8. Implementation plan

Prerequisites: fix the execution correctness findings in the architecture review, make output and cancellation dependable, and validate response budgets plus connection cleanup. Product priority is fast, context-efficient investigation; the mechanisms below remain proposals until tested against that workflow. Bounded output must cover the collected MCP response as well as streamed rows.

1. **Runner refactor** (shared with the schema commands)
   - pull the batch loop out of `main.rs` into `Runner::run(&mut Session, &[Batch], &mut dyn Output, &RunOptions) -> RunResult`
   - the CLI and MCP both call it; MCP renders into a `TextSink<Vec<u8>>` and returns the bytes as the tool text
2. **`src/mcp/`**
   - `mod.rs`:
     - an async stdin line reader and a single stdout writer task fed by a channel, so responses never interleave
     - request dispatch on the current-thread runtime
     - cancellation map: request id → `Cancel`
   - `protocol.rs`:
     - JSON-RPC envelopes and per-request `_meta` and era handling
     - `server/discover`, `initialize`, `ping`, `tools/list`, errors
   - `tools.rs`:
     - tool definitions (names, schemas, annotations, descriptions) as static JSON
     - argument validation and mapping to `Runner` and `schema` calls
   - `pool.rs`: pooled connections with reset-on-acquire, the session table with expiry, lazy connect
3. **Dependency:** `serde_json`, already a test-only dependency, becomes a normal one for parsing untrusted JSON safely. Use `serde_json::Value` without serde derive, to keep size down. Budget: under 300 KB of extra exe size, measured after the first build.
4. **Driver check:** confirm `prepare_reset_connection` behaves as expected: temp tables gone, database restored, open transaction rolled back. If it doesn't, fall back to `sp_reset_connection` semantics by reconnecting, which is slower but correct.
5. **Docs:** DESIGN.md §10 points here; `--help` gets an MCP line; the README needs the Claude Code setup snippet.

## 9. Testing

- **Protocol unit tests, with no database:** feed scripted stdin lines and check the stdout lines:
  - discover and tools/list (deterministic order, `cacheScope`)
  - a version mismatch (`-32022`)
  - legacy `initialize`, `initialized` and `ping`
  - an unknown method or tool, and bad arguments
  - a cancel for an unknown id (ignored)
  - stdin EOF leading to exit
- **Tool tests against the test server:**
  - `query` happy path, SQL error leading to `isError`, a blocked write
  - timeout and cancellation (send `notifications/cancelled` mid-query)
  - session temp-table persistence and rollback on close
  - reset hygiene: `USE` and `SET` in one stateless call don't leak into the next
  - `execute` is absent without `--allow-write`
- **Client interop:** connect from Claude Code (`claude mcp add`, then use the tools in a session) and from the MCP Inspector, as both a modern and a legacy client.

## 10. Open questions

- **Exact totals:** do total row counts improve investigation enough to justify draining results? Assess cutoff behavior separately; cancellation must be quick and partial counts must be explicit.
- **Cancellation and reuse:** after abandoning a call, can the connection be proven clean and reusable promptly, or should it be discarded? Test before making reuse guarantees.
- **Command categorization:** evolve checks for DBA workflows and overprivileged connections without relying on tool annotations as enforcement.
- **Compatibility expansion:** assess the candidate 2016 floor and other platforms after validating the 2025 Standard first-release target.

- **structuredContent:** worth adding as an option (`format: "structured"`) once a client benefits from it? Not by default (§4).
- **Resources:** expose the schema as resources (`sqlh://schema/dbo.Users`) for clients that browse resources? Tools cover the need, so it's deferred.
- **The `database` argument:** allow switching databases per call, or pin to the profile's database? Proposal: allow it. Permissions still bound it, and it's explicit in every call. Revisit if it surprises anyone.
- **Pool size:** default 1 keeps server load minimal but serializes concurrent tool calls. Raise it to 2 if harnesses issue parallel calls often.
- **Session limits:** are 10 minutes idle and 60 minutes hard right? They're conservative because a read-only session holds a transaction open.
