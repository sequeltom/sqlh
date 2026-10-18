# Spike 5.1: connection reset, cancels, the MCP revision, and the cost of JSON

2026-10-17, PLAN.md Phase 5 task 5.1. Three questions the MCP design (docs/mcp-mode.md, owner decisions 14–20) rests on, answered before the engine was built.

Status: done, and built on: the engine (5.2) and `sqlh mcp` (5.3) were built the same day and await the owner's code review. **Amended 2026-10-17 after the self-review of 5.2 and 5.3:** the reset doesn't restore the transaction isolation level, which this spike's first run never read (§1); the test now asserts it, and sqlh's session setup sets the level on every call.

## Setup

- **Client:** the dev workstation, Windows 11, a debug build, mssql-tds 0.2.0.
- **Server:** the owner's test SQL Server 2025 (Standard Developer), the `test` profile, database `sqlh_test`; SQL authentication, encryption mandatory.
- **The test:** `db::spike::spike_reset` (`src/db/mod.rs`), an ignored live test that drives one `Session` directly, without sqlh's session setup, and reads this state after each step:

```sql
SELECT OBJECT_ID(N'tempdb..#spike') AS temp, @@TRANCOUNT AS trancount, DB_NAME() AS db,
       @@LOCK_TIMEOUT AS lock_timeout, @@OPTIONS & 16384 AS xact_abort,
       (SELECT deadlock_priority FROM sys.dm_exec_sessions WHERE session_id = @@SPID) AS deadlock_priority,
       (SELECT transaction_isolation_level FROM sys.dm_exec_sessions WHERE session_id = @@SPID) AS isolation,
       @@SPID AS spid;
```

(`isolation` was added after the self-review; `transaction_isolation_level` is 2 for READ COMMITTED and 4 for SERIALIZABLE.) It asserts that every state after a reset equals a fresh login's (right after the dirty batch, every column but the isolation level, which the reset leaves as it was, §1), and that no kind of cancel loses the connection. Run it with `SQLH_TEST_PROFILE=test cargo test --lib spike_reset -- --ignored --nocapture`.

## 1. The reset clears what a call can leave behind

`Session::reset_before_next` calls the driver's `prepare_reset_connection(false)`, which sets the TDS RESETCONNECTION bit on the next request (MS-TDS 2.2.3.1.2; the server runs `sp_reset_connection` before it). The driver checks the acknowledgement itself: the response to that request must carry the server's ResetConnection ENVCHANGE, or the request fails with `ConnectionResetNotAcknowledged` and the connection is marked dead, so a session that wasn't reset is never handed out.

A dirty batch, `CREATE TABLE #spike (i int); SET LOCK_TIMEOUT 1234; SET XACT_ABORT ON; SET DEADLOCK_PRIORITY HIGH; SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; BEGIN TRANSACTION; USE master;` (the isolation `SET` added after the self-review), then the state:

| | temp | trancount | db | lock_timeout | xact_abort | deadlock_priority | isolation | spid |
|---|---|---|---|---|---|---|---|---|
| fresh login | NULL | 0 | sqlh_test | -1 | 0 | 0 | 2 | 61 |
| after the dirty batch, no reset | an id | 1 | master | 1234 | 16384 | 5 | 4 | 61 |
| after the dirty batch, reset | NULL | 0 | sqlh_test | -1 | 0 | 0 | **4** | 61 |
| the call after that (no new reset) | NULL | 0 | sqlh_test | -1 | 0 | 0 | 4 | 61 |

(The first run, which had no `isolation` column, gave the other values; the rerun after the self-review asserts the isolation values shown and that every other column after the reset equals the fresh login's.)

- **Everything else the design needs is cleared**, on the same server session (SPID 61): the temp table, the open transaction, the database (back to the login's, `sqlh_test`, not the server default), and the session options, back to the login's defaults. The engine then runs sqlh's session setup (the agent defaults, DESIGN.md §4.3) after the reset, as a CLI run does after connecting.
- **The isolation level is not restored.** On SQL Server (all but Azure SQL Database) `sp_reset_connection` leaves the session's isolation level as the last request set it, so a pooled connection keeps it. The first run's state query didn't read the level, so its "behaves like a fresh login" never covered it; the self-review of 5.2 and 5.3 found the gap by reading the code against Microsoft's documented pooling behavior, and the rerun confirmed it live: after the reset the level was still 4, SERIALIZABLE. Through sqlh, a call that ran `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` (the guard allows it) would have handed that level to the next call on its connection.
- **How sqlh handles it:** `run::session_setup` sets the level on every connection, a reused one included, in the setup batch before the wrapper's `BEGIN TRANSACTION`: READ COMMITTED (a fresh login's level, 2, which still honors `READ_COMMITTED_SNAPSHOT`), or READ UNCOMMITTED when the profile reads that way (DESIGN.md §4.3, mcp-mode.md §5). The spike asserts it: sqlh's setup sent after the reset gives level 2, and a reset after that leaves a state equal to the fresh login's. Checked through `sqlh mcp --pool 1` too (2026-10-17): a call that set SERIALIZABLE was followed, on the same session, by a call that read level 2, by hand and then by the live test `mcp_calls_share_a_connection_and_nothing_else` (`tests/cli.rs`). The engine test `every_call_sets_the_isolation_level_the_reset_leaves_alone` holds the setup to it on a reused connection.
- **It costs nothing measurable:** the state query took 2 ms with and without a reset, four times each, because the reset rides on the request instead of costing a round trip.

## 2. Every kind of cancel left a usable connection, clean after a reset

Each scenario starts a transaction and makes a temp table first, then ends the batch with a TDS attention:

| Scenario | Time | Outcome | Connection | After a reset |
|---|---|---|---|---|
| A cancel from outside, 400 ms into `WAITFOR DELAY '00:00:10'` | 455 ms | `Cancelled`, "the connection stayed usable" | kept | clean |
| The bounded count of hidden rows stops (a cross join of `sys.all_columns`, `Counting::BOUNDED`) | 912 ms | complete, `counting_stopped` | kept | clean |
| `-t 1` expires in `WAITFOR DELAY '00:00:05'` | 1017 ms | `Timeout` | kept | clean |
| A cancel before the request is sent | 0 ms | `Cancelled` | kept | clean |
| A cancel 300 ms into streaming the cross join | 1144 ms | `Cancelled` | kept | clean |

- **The reset is needed:** after the cancelled wait, without a reset, the transaction was still open (`@@TRANCOUNT` 1) and the temp table still there. An attention ends the batch, not the transaction (`XACT_ABORT` is off in sqlh's sessions).
- **A cancel mid-rows settles slowly:** about 850 ms after the cancel, while the server sends what it had buffered before its DONE with the attention flag. The count stop's 912 ms is its 250 ms budget plus that settling. That is sqlh's settle bound at work (R15 B), not a fault; it is the cost of the next call starting, not of reconnecting.
- **What sqlh can observe about a cancel:** only that the driver kept the connection (`is_connection_dead()` false). The driver exposes no receipt for an acknowledged attention: its settlement marker is private, and a kept connection also follows a cancel before sending, and a half-sent request withdrawn with EOM|IGNORE, where the server confirmed nothing (`Session::failure`'s comment). In each of those the connection is in protocol sync, and the next call's reset, whose acknowledgement the driver verifies, is the check that the session is in a known state. **Decision 17 says "confirmed by the server"; what the engine can actually check is "kept by the driver, then a verified reset".** The owner amended decision 17 to that on 2026-10-17 (PLAN.md §6); the missing receipt is U4 in [upstream-issues.md](upstream-issues.md).

Not exercised live, because a real server always acknowledges: an attention the server never acknowledges, which the driver answers by marking the connection dead (the fake-server tests in `src/db` cover it, `COUNT_STOP_UNCONFIRMED`), and a reset the server doesn't acknowledge (5.2's engine test against the pooled fake server, `a_reset_the_server_does_not_acknowledge_is_retried_on_a_new_connection`, covers it). Nor is a reused connection gone silent while idle, whose setup, carrying the reset, gets no reply at all: since `becb56f` that setup gets at most 5 s on a reused connection (the probe), and a connection found dead runs the call again on a new one (engine test `a_reused_connection_gone_silent_is_retried_after_the_probe`); since `6e7a0d8` a setup the probe cuts on a connection the driver kept is reset and run again on that connection instead, so a slow live server keeps its connection (`a_setup_the_probe_cuts_short_keeps_its_live_connection`) when it acknowledges the probe's attention within the driver's 5 s; one silent longer, about 10 s after the setup was sent, can't be told from a dead connection and is replaced like one, the profile's idle connections with it (the boundary, stated in `dc24d4b`); since `4519341` the probe and that second try cover the setup alone (`run::setup`), and the call's `USE` (`run::use_database`) follows once, with the call's timeout (`a_use_past_the_calls_timeout_fails_once`; mcp-mode.md §5).

## 3. The MCP revision

Checked against the published 2026-07-28 pages (versioning, `server/discover`, cancellation) on 2026-10-17. docs/mcp-mode.md §2 matches:
- **No handshake:** every request carries `_meta["io.modelcontextprotocol/protocolVersion"]`; an unsupported one gets `-32022`, "Unsupported protocol version", with `data.supported` and `data.requested`.
- **`server/discover`** is mandatory for servers: `resultType: "complete"`, `supportedVersions`, `capabilities`, `_meta["io.modelcontextprotocol/serverInfo"]`, `instructions`, `ttlMs`, `cacheScope`. Clients send `io.modelcontextprotocol/clientInfo` and `io.modelcontextprotocol/clientCapabilities` in `_meta`.
- **Dual-era:** a request with modern `_meta` is served statelessly; an `initialize` selects legacy semantics for the stdio process; a dual-era server may serve both. A dual-era client probes with `server/discover` and falls back to `initialize` on any non-modern error. (As built, sqlh picks the era per request: a request with modern `_meta` is served as modern even after an `initialize`, which only sets the version for requests without it; docs/mcp-mode.md §2.)
- **Cancellation on stdio:** the client must send `notifications/cancelled` with `requestId` (and an optional `reason`); the server **should** stop, free resources and not respond (mcp-mode.md said "as the spec requires": corrected to "recommends"). Unknown or completed ids are ignored. Clients should also cancel on their own timeouts.
- **Tasks** are now an extension (`io.modelcontextprotocol/tasks`), not core; sqlh doesn't implement them, as planned.

The tools, results, caching and stdio pages were checked when 5.3 was built (docs/mcp-mode.md §2 and §4 follow them), and the self-review of 5.2 and 5.3 (2026-10-17) read the 2025-11-25 and 2026-07-28 tools pages and 2026-07-28's subscriptions and base-protocol pages again. It found two places where sqlh departed from them, both fixed in `e8175d4`. Invalid tool arguments were JSON-RPC `-32602` errors where the tools page lists input validation among `isError` tool results: they are tool errors now (mcp-mode.md §4). `subscriptions/listen` got `-32601` although the base protocol lists subscribe-and-notify among the patterns every implementation supports: it is acknowledged now with none of the notifications asked for (the tool list never changes, which the subscriptions page allows), held open, and answered when the server ends (mcp-mode.md §2). The self-review's second round (also 2026-10-17, against the cancellation page) found a third, fixed in `05032c0`: a subscription the server ends must be torn down with `notifications/cancelled` referencing its request id (a MUST on that page), and sqlh sent only the answer; it now sends the answer and then that notification, and holds at most 8 subscriptions open (mcp-mode.md §2). The third round (also 2026-10-17, against the base protocol, JSON-RPC 2.0 §4.1 and the cancellation page) found a fourth, fixed in `becb56f`: sqlh answered messages that must never be answered, an error response whose `id` was `null` (`-32600`) and a notification whose `params` wasn't an object (`-32602`); a response is now ignored whatever its `id`, and such a notification goes without its `params` (mcp-mode.md §2, What isn't answered). The fourth and fifth rounds found none against the pages: their findings were the engine's, the end's and a message's wording (fixed in `6e7a0d8`, `4519341` and `dc24d4b`; PLAN.md, Phase 5.2 and 5.3).

## 4. What JSON costs the exe

A temporary build enabled `serde_json` (already a test dependency, at the version in `Cargo.lock`) in the library and parsed, modified and printed one JSON-RPC message with `Value` and `json!`, the way an MCP server does. The release exe went from 2,487,808 to 2,554,880 bytes: **65 KB** for JSON parsing and writing. The probe was reverted. The design's budget for all of MCP is 300 KB, against the 3 MB size budget. **Measured in 5.3** (2026-10-17): the exe was 2,762,240 bytes as first built (268 KB over 2,487,808), 2,784,768 bytes (290 KB over) after the second self-review round's fixes (`05032c0`), 2,787,840 bytes (293 KB over) after the third's (`becb56f`), 2,788,864 bytes (294 KB over) after the fourth's (`6e7a0d8`), 2,790,400 bytes (295.5 KB over) after the first half of the fifth's (`4519341`), and is 2,789,888 bytes after its second half (`dc24d4b`), 302,080 bytes or 295 KB over it (1,024 bytes a KB), for the engine, the run's split, `src/mcp`, `serde_json` and the `describe` fix found while building it (`d6237da`) together; within the budget (docs/mcp-mode.md §8, which also records that `cargo bloat` wasn't available to split it by crate).

## Conclusions

- **The design holds:** the reset makes a reused connection behave like a fresh login, at no measurable cost, after every kind of cancel tried, except for the isolation level (§1). No fallback to reconnecting per call is needed.
- **Decision 17's check is "the driver kept the connection, and the next reset is verified"** (amended by the owner, 2026-10-17), since the driver gives no receipt for an acknowledged attention.
- **The engine must run sqlh's session setup after each reset**, because the reset restores the login's defaults, not sqlh's agent defaults, and leaves the isolation level as the last call set it; so the setup sets the level too (added after the self-review, §1).
