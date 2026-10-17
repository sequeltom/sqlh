# Spike 5.1: connection reset, cancels, the MCP revision, and the cost of JSON

2026-10-17, PLAN.md Phase 5 task 5.1. Three questions the MCP design (docs/mcp-mode.md, owner decisions 14–19) rests on, answered before the engine is built.

## Setup

- **Client:** the dev workstation, Windows 11, a debug build, mssql-tds 0.2.0.
- **Server:** the owner's test SQL Server 2025 (Standard Developer), the `test` profile, database `sqlh_test`; SQL authentication, encryption mandatory.
- **The test:** `db::spike::spike_reset` (`src/db/mod.rs`), an ignored live test that drives one `Session` directly, without sqlh's session setup, and reads this state after each step:

```sql
SELECT OBJECT_ID(N'tempdb..#spike') AS temp, @@TRANCOUNT AS trancount, DB_NAME() AS db,
       @@LOCK_TIMEOUT AS lock_timeout, @@OPTIONS & 16384 AS xact_abort,
       (SELECT deadlock_priority FROM sys.dm_exec_sessions WHERE session_id = @@SPID) AS deadlock_priority,
       @@SPID AS spid;
```

It asserts that every state after a reset equals a fresh login's, and that no kind of cancel loses the connection. Run it with `SQLH_TEST_PROFILE=test cargo test --lib spike_reset -- --ignored --nocapture`.

## 1. The reset clears what a call can leave behind

`Session::reset_before_next` calls the driver's `prepare_reset_connection(false)`, which sets the TDS RESETCONNECTION bit on the next request (MS-TDS 2.2.3.1.2; the server runs `sp_reset_connection` before it). The driver checks the acknowledgement itself: the response to that request must carry the server's ResetConnection ENVCHANGE, or the request fails with `ConnectionResetNotAcknowledged` and the connection is marked dead, so a session that wasn't reset is never handed out.

A dirty batch, `CREATE TABLE #spike (i int); SET LOCK_TIMEOUT 1234; SET XACT_ABORT ON; SET DEADLOCK_PRIORITY HIGH; BEGIN TRANSACTION; USE master;`, then the state:

| | temp | trancount | db | lock_timeout | xact_abort | deadlock_priority | spid |
|---|---|---|---|---|---|---|---|
| fresh login | NULL | 0 | sqlh_test | -1 | 0 | 0 | 61 |
| after the dirty batch, no reset | an id | 1 | master | 1234 | 16384 | 5 | 61 |
| after the dirty batch, reset | NULL | 0 | sqlh_test | -1 | 0 | 0 | 61 |
| the call after that (no new reset) | NULL | 0 | sqlh_test | -1 | 0 | 0 | 61 |

- **Everything the design needs is cleared**, on the same server session (SPID 61): the temp table, the open transaction, the database (back to the login's, `sqlh_test`, not the server default), and the session options, back to the login's defaults. The engine then runs sqlh's session setup (the agent defaults, DESIGN.md §6.7) after the reset, as a CLI run does after connecting.
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
- **What sqlh can observe about a cancel:** only that the driver kept the connection (`is_connection_dead()` false). The driver exposes no receipt for an acknowledged attention: its settlement marker is private, and a kept connection also follows a cancel before sending, and a half-sent request withdrawn with EOM|IGNORE, where the server confirmed nothing (`Session::failure`'s comment). In each of those the connection is in protocol sync, and the next call's reset, whose acknowledgement the driver verifies, is the check that the session is in a known state. **Decision 17 says "confirmed by the server"; what the engine can actually check is "kept by the driver, then a verified reset".** The owner is asked about that wording (PLAN.md 5.1).

Not exercised live, because a real server always acknowledges: an attention the server never acknowledges, which the driver answers by marking the connection dead (the fake-server tests in `src/db` cover it, `COUNT_STOP_UNCONFIRMED`), and a reset the server doesn't acknowledge (task 5.2's fake-server tests).

## 3. The MCP revision

Checked against the published 2026-07-28 pages (versioning, `server/discover`, cancellation) on 2026-10-17. docs/mcp-mode.md §2 matches:
- **No handshake:** every request carries `_meta["io.modelcontextprotocol/protocolVersion"]`; an unsupported one gets `-32022`, "Unsupported protocol version", with `data.supported` and `data.requested`.
- **`server/discover`** is mandatory for servers: `resultType: "complete"`, `supportedVersions`, `capabilities`, `_meta["io.modelcontextprotocol/serverInfo"]`, `instructions`, `ttlMs`, `cacheScope`. Clients send `io.modelcontextprotocol/clientInfo` and `io.modelcontextprotocol/clientCapabilities` in `_meta`.
- **Dual-era:** a request with modern `_meta` is served statelessly; an `initialize` selects legacy semantics for the stdio process; a dual-era server may serve both. A dual-era client probes with `server/discover` and falls back to `initialize` on any non-modern error.
- **Cancellation on stdio:** the client must send `notifications/cancelled` with `requestId` (and an optional `reason`); the server **should** stop, free resources and not respond (mcp-mode.md said "as the spec requires": corrected to "recommends"). Unknown or completed ids are ignored. Clients should also cancel on their own timeouts.
- **Tasks** are now an extension (`io.modelcontextprotocol/tasks`), not core; sqlh doesn't implement them, as planned.

The tools, results and caching pages are checked when 5.3 is built.

## 4. What JSON costs the exe

A temporary build enabled `serde_json` (already a test dependency, at the version in `Cargo.lock`) in the library and parsed, modified and printed one JSON-RPC message with `Value` and `json!`, the way an MCP server does. The release exe went from 2,487,808 to 2,554,880 bytes: **65 KB** for JSON parsing and writing. The probe was reverted. The MCP code itself (protocol, tools, the engine) is still to measure, in 5.3; the design's budget for all of MCP is 300 KB, against the 3 MB size budget.

## Conclusions

- **The design holds:** the reset makes a reused connection behave like a fresh login, at no measurable cost, after every kind of cancel tried. No fallback to reconnecting per call is needed.
- **Decision 17's check becomes "the driver kept the connection, and the next reset is verified"**, if the owner agrees, since the driver gives no receipt for an acknowledged attention.
- **The engine must run sqlh's session setup after each reset**, because the reset restores the login's defaults, not sqlh's agent defaults.
