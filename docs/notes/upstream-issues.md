# Issues to raise with mssql-tds upstream

The one list of driver problems worth raising with Microsoft's [mssql-rs](https://github.com/microsoft/mssql-rs) (`mssql-tds`). **The owner files them; agents add to this list and never file anything themselves** (owner, 2026-10-17). Upstream takes issues, not pull requests (AGENTS.md).

Before filing one: check upstream's `main` (sqlh pins 0.4.0 since 2026-11-08, and 0.1.0 takes no later driver, decision 70; every entry was re-read in 0.4.0's source, docs/notes/driver-0.4.0.md §5), since the API moves and a problem may be gone; search the existing issues; and keep sqlh's workaround in our code either way, since sqlh pins an exact version.

Each entry says what was read or observed, in which version, why it matters to sqlh, what sqlh does about it, and a possible fix.

## To raise

### U1. The first open of a remote Named Pipe blocks the async runtime

- **Seen in:** 0.2.0, reading the source (docs/notes/connect-failures.md, plan-history.md 2.4); still in 0.3.0 (`named_pipes.rs:89`; docs/notes/driver-0.3.0.md §5) and 0.4.0 (`named_pipes.rs:87-89`, byte-identical; driver-0.4.0.md §5).
- **What:** `open_named_pipe_with_retry` calls tokio's `ClientOptions::open`, a synchronous `CreateFileW`, inside the async fn, whether the pipe came from the protocol waterfall, an `np:` prefix or a UNC path. For an unreachable host that stalls every timer, cancellation and task for about 21 s (SMB's own timeout), and the error that comes back is the pipe's instead of the TCP timeout. The retry after `ERROR_PIPE_BUSY` is not part of this: its `WaitNamedPipeW` already runs on `spawn_blocking`. Related: in 0.3.0 an IPv6 address couldn't take a `tcp:` prefix, so without a port it got the waterfall and the same freeze; 0.4.0 splits a protocol off an address (`tcp:fe80::1`), which also opens `np:<IPv6>\INST` and `np:\\<IPv6>\pipe\...` to the open (driver-0.4.0.md §1.2).
- **sqlh's workaround:** remote Named Pipes are refused before connecting (exit 3, with the TCP spelling), an IPv6 address included, and a remote address gets `tcp:` like a name (DESIGN.md §4, `-S`; on 0.3.0 one without a port was sent with port 1433).
- **Possible fix:** run the first open on a blocking thread too and bound it by the connect timeout (the open itself can't be cancelled, but the runtime and its timers stay free), or leave remote names on TCP unless Named Pipes are asked for.

### U2. A `RowWriter` can't decline a large value

- **Seen in:** 0.2.0, measuring memory (docs/notes/memory.md, plan-history.md 2.7); half fixed in 0.3.0: `drain_rows` now skips (`ColumnPolicy::SkipAll`), but the `RowWriter` contract still can't decline a value, so the workaround stays (driver-0.3.0.md §5); unchanged in 0.4.0 (`row_writer.rs` byte-identical).
- **What:** a `RowWriter` offered a PLP value can only return a destination or `None`, and `None` makes the decoder allocate the value's declared length (`value_destination` has no way to say "skip"), so `DiscardRowWriter` and every consumer that wants a prefix of a large value pay for all of it. In 0.3.0 `drain_rows` no longer does: it reads with `ColumnPolicy::SkipAll` (rewritten for 0.3.0 in review, 2026-10-31).
- **sqlh's workaround:** large values are read through the column cursor and cut as they arrive (`src/db/stream.rs`); hidden rows use the cursor's skip.
- **Possible fix:** a skip or a limit in `value_destination`'s contract.

### U3. `text`, `ntext` and `image` have no streaming path

- **Seen in:** 0.2.0 (docs/notes/memory.md); unchanged in 0.3.0 and 0.4.0 (`MAX_ALLOC_SIZE` still 100 MB; `decoder.rs` byte-identical in 0.4.0).
- **What:** the pull cursor returns them as `CursorColumn::Value`, fully decoded, and the decoder refuses a value over 100 MB (`MAX_ALLOC_SIZE`), a limit the PLP types don't have.
- **sqlh's workaround:** none possible above the driver; the limitation is documented (DESIGN.md §11, README).
- **Possible fix:** the same cursor streaming the PLP types get.

### U4. No receipt for an acknowledged attention

- **Seen in:** 0.2.0, reading the source and on SQL Server 2025 (docs/notes/mcp-reuse-spike.md §2, `Session::failure`'s comment in `src/db/mod.rs`); unchanged in 0.3.0 (its new `cancel_query` maps an error on a live connection to `Ok`, which is no receipt) and 0.4.0, whose new `abandon_query` does the same.
- **What:** after a cancel, a caller can learn only whether the connection was marked dead (`is_connection_dead()`). A kept connection follows an acknowledged attention (DONE with the ATTN flag), but also a cancel that came before the request was sent, and a half-sent request withdrawn with EOM|IGNORE, where the server confirmed nothing. The settlement marker is private, and `send_attention_with_timeout` would send a new attention rather than report on the old one.
- **Why it matters:** a pool deciding whether to reuse a connection after a cancel, and a user told whether the server confirmed it, both want the receipt.
- **sqlh's workaround:** owner decision 17, as amended: a connection the driver kept is reused, and the next call's reset, whose acknowledgement the driver verifies, is the check; a dead one is discarded. Messages say only what was observed ("the connection stayed usable"), never that the server confirmed.
- **Possible fix:** an outcome on the cancelled operation's error, or a query on the client: acknowledged, never sent, withdrawn, or unacknowledged (dead).

### U5. Withdrawing a half-sent request waits up to 120 s, without the cancel

- **Seen in:** 0.2.0 (plan-history.md, Phase 2 follow-up, R15 B); fixed in 0.3.0 for callers that opt in (`CancelHandle::set_cancel_timeout_secs`), which sqlh doesn't yet; `settle_within` stays either way (driver-0.3.0.md §2); unchanged for sqlh in 0.4.0, where a budgeted write also stops at any request deadline (driver-0.4.0.md §5).
- **What:** a request cancelled between two of its packets is withdrawn (EOM|IGNORE) and its DONE awaited in `withdraw_sent_message` for up to 120 s, with the cancel removed and no bound from the caller's timeout; an attention, by contrast, gets 5 s.
- **sqlh's workaround:** `settle_within`: a batch's work races `-t` plus a 5 s grace; past it the work is dropped where it stands and the session retired (exit 4).
- **Possible fix:** bound the withdrawal by the operation's timeout or the attention's 5 s, and keep the cancel honored while it waits.

### U6. `SqlString::decode` panics on malformed UTF-8

- **Seen in:** 0.2.0 (docs/notes/memory.md). Already upstream's own **#310**: add a comment or a reproduction there rather than a new issue; unchanged in 0.3.0 (`sql_string.rs:346`) and 0.4.0 (`:375`).
- **What:** under a UTF-8 collation, `String::from_utf8(...).unwrap()` panics on a malformed value.
- **sqlh's workaround:** sqlh's cutter decodes with replacement, so a malformed value shows U+FFFD where the driver's path would have panicked.

### U7. The DNS cache can't be turned off

- **Seen in:** 0.3.0, reading the source (docs/notes/driver-0.3.0.md §1.2; owner decision 42); unchanged in 0.4.0, where a fallback is visible to a tracing subscriber as `source=fallbackCache`, never through the `log` bridge (driver-0.4.0.md §5).
- **What:** LOGIN7 always advertises DNS caching (feature 0x0B), and when a server acknowledges it the addresses the name resolved to are kept in a process-wide cache with no TTL (`connection/dns_cache.rs`), used whenever a later lookup of that name fails. The module is crate-private and the feature is always requested, so a client can't opt out.
- **Why it matters to sqlh:** `sqlh mcp` is a long-lived process; in a DNS outage, its login, with a SQL password, goes to the addresses the name had at the last accepted login, which TLS covers only by the certificate's name, and only where the certificate is checked (strict; mandatory without `-C`; optional only when the server requires full encryption and `-C` is off).
- **sqlh's workaround:** none (resolving names itself would break Kerberos SPNs); documented in DESIGN.md §8.
- **Possible fix:** a `ClientContext` option to not advertise 0x0B or not use the cache, or a TTL.

### U8. A route can't be refused

- **Seen in:** 0.2.0, 0.3.0 and 0.4.0, reading the source (docs/notes/driver-0.3.0.md §1.3; owner decision 43; driver-0.4.0.md §5).
- **What:** an ENVCHANGE routing token (type 20, and in 0.3.0 type 21 with a database) makes the provider log in to the named host with the same `ClientContext`, the password included; 0.3.0 always advertises Enhanced Routing (0x0F). There is no option to refuse or confirm a route.
- **Why it matters to sqlh:** a stored password then reaches a host sqlh didn't name; acceptable as the server's decision (Always On read-only routing), but a tool that binds a password to one server would want the choice.
- **sqlh's workaround:** none; documented in DESIGN.md §8, and pinned by fake-server tests in the 0.3.0 upgrade. 0.4.0's `SecurityContextProvider` sees each Windows-auth hop before any token, so `-E` could refuse a route through it (driver-0.4.0.md §6.1, a candidate); SQL logins get no hook. sqlh's provider (decision 72) sets only each hop's SPN; refusing a route sqlh didn't name stays a candidate.
- **Possible fix:** a `ClientContext` option to fail on a routing token, or a callback to approve its target.

### U9. The SPN's name lookup has no bound of its own (partly fixed in 0.4.0)

- **Seen in:** 0.3.0, measured and read in the source (docs/notes/connect-latency.md, Windows authentication; `security/spn.rs`, `security/windows/sspi_context.rs:102`). **Partly fixed in 0.4.0** (driver-0.4.0.md §1.1): the lookup and every SSPI call run on `spawn_blocking` (`off_runtime_thread`), inside the login deadline and the cancel, so the runtime, its timers and other connects no longer stall; the lookup is the same code, still with no bound of its own and no option to skip it, and lookups abandoned by a timeout or cancel can now pile up on tokio's blocking pool.
- **What (as seen in 0.3.0):** with no `server_spn`, Windows auth canonicalizes the host with blocking `dns_lookup` calls inside the async connect: a reverse lookup for an address, forward and reverse for a short name. The function's own comment says to consider a blocking task pool. For an address with no PTR record it took about 5 s here, and it stalls every timer, cancellation and task meanwhile, so a login timeout shorter than the lookup fails a healthy server.
- **Why it matters to sqlh:** a workstation outside a domain connecting by address pays it on every CLI call, a short `-l` fails with a message that blames the server, and in `sqlh mcp` one connect holds up every call.
- **sqlh's workaround (owner decision 72, 2026-11-08):** `db::spn::HopSpn`, the driver's per-hop `SecurityContextProvider`, runs the lookup on a thread of its own for each hop, waits 500 ms and always sets the SPN, so the driver's unbounded lookup never runs (DESIGN.md §4.2, `-E`). Before it (2026-11-01 to decision 72), `db::spn_if_lookup_is_slow` ran the lookup before the connect and set `server_spn` only past 500 ms; the driver's own lookup still ran when sqlh's came back in time, and always for a named instance without a port and under `-K ReadOnly`, where no fixed SPN could match the driver's (R52).
- **Also worth asking:** a hook to set the SPN after SQL Browser gives an instance's port, or for each routed hop, so a caller that bounds the lookup itself can still give the driver's SPN. Answered in 0.4.0 by `ClientContext::security_context_provider` (driver-0.4.0.md §6.1), which sqlh uses (decision 72).
- **Possible fix (what is left after 0.4.0):** a bound of the lookup's own, or an option to skip canonicalization.

### U10. A long run of DONE tokens before the first result is an error

- **Seen in:** 0.3.0, on SQL Server 2025 and 2016 SP3, and read in the source (`connection/tds_client.rs`, `advance_to_result_boundary`, `loop_count > 10000`); unchanged in 0.4.0's source (`:5898`, `:6266`), where the count is of every token read since the last result boundary, checked at a DONE with MORE (driver-0.4.0.md §3.4).
- **What:** the driver gives up with `UsageError("Too many Done tokens with has_more=true without ColMetadata")` after 10,000 DONE tokens with `has_more` before a result's metadata, as a guard against a malicious server. A legal read-only loop produces that many: under `SET NOCOUNT ON`, `CREATE TABLE #s (n int); DECLARE @i int = 0; WHILE @i < 5000 BEGIN INSERT #s VALUES (@i); SET @i += 1; END; SELECT COUNT(*) AS c FROM #s` fails at 5,000, 20,000 and 60,000 iterations and returns `c` at 2,000.
- **Why it matters to sqlh:** the run ends with `-- failed (exit 10): internal error`, its later result is lost, and nothing about it says the loop is the cause. A `#temp` table filled in a loop is ordinary in an investigation.
- **sqlh's workaround:** none yet; a workaround belongs in `src/db/` if one is wanted, and nothing is planned.
- **Possible fix:** no count on a token stream that keeps making progress, a much higher limit, or an option to set it.

### U11. More than 1,000 errors before a DONE is an error of its own

- **Seen in:** 0.3.0, on SQL Server 2025, under `--check`, and read in the source (`connection/tds_client.rs`, `MAX_ERRORS_PER_FAILED_STATEMENT`, `too_many_errors_for_one_statement`). Not tried without `--check`, or on 2016. Unchanged in 0.4.0's source (`:75`, `:5968`, `:8641`; driver-0.4.0.md §3.4).
- **What:** under `BatchErrorMode::Continue` the driver gives up with `ProtocolError("Too many ERROR tokens for one statement without a DONE")` at the 1,001st ERROR token before a DONE, as a guard against a misbehaving server, and drops the errors it collected (`abandon_failed_statement`). A legal batch produces that many, plain ASCII: `SELECT nope_zz FROM (SELECT 1 a) t;` repeated, with `--check`. 999 and 1,000 statements give exit 1 and each error; 1,001, 1,002 and 5,000 give exit 10 and none.
- **Why it matters to sqlh:** the run ends with `-- error: Protocol Error: Too many ERROR tokens for one statement without a DONE` and `-- failed (exit 10): internal error`, so a generated script checked with `--check` gets none of the errors that would have fixed it, one more statement having lost them all, and nothing says that its size is the cause.
- **sqlh's workaround:** none yet; a workaround belongs in `src/db/` if one is wanted, and nothing is planned.
- **Possible fix:** keep the first 1,000, count the rest and read on to the DONE, as `SkippedErrors` already does for a drain; failing that, return what was collected with the error, or an option to set the limit.

### U12. TLS needs Windows 10 1809 or Windows Server 2019: no fallback for older Windows

- **Seen in:** 0.4.0, on AppVeyor's Windows Server 2016 image (1607) against a local SQL Server 2017, with the signed `0.1.0-dev.10` (decision 75, 2026-11-08). The source is the same in 0.3.0. `connection/transport/win_tls/cred.rs`, `acquire_client_cred`, builds only `SCH_CREDENTIALS` and says so: "We intentionally do NOT fall back to the legacy SCHANNEL_CRED (v4) struct on older OSes — Windows Server 2016 is therefore unsupported."
- **What:** every TLS connect fails before anything reaches the server: `AcquireCredentialsHandleW(UNISP_NAME) failed: unknown SECURITY_STATUS (status=0x8009030D)` (`SEC_E_UNKNOWN_CREDENTIALS`). sqlh encrypts by default (mandatory), and even optional encryption sends the login over TLS, so in practice it can't connect from those systems. On Windows Server 2019 (1809) the same jobs connect.
- **Why it matters to sqlh:** the Windows 10 / Windows Server 2016 floor for the machine sqlh runs on is a candidate expansion (AGENTS.md, Hard constraints). This blocks it for Server 2016 and Windows 10 before 1809, whatever sqlh does. The error names neither cause, and until 2026-11-09 sqlh's hint was wrong for it (PLAN.md §4, AppVeyor).
- **sqlh's workaround:** none possible in sqlh: the TLS stack is the driver's. Its hint for the failure now names the Windows the driver needs instead of `-C` and `-No` (2026-11-09, DESIGN.md §6).
- **Possible fix:** fall back to `SCHANNEL_CRED` (TLS 1.2) when `SCH_CREDENTIALS` is refused, or report the requirement in the error.

## Decided not to raise

- **The password in debug packet dumps** (owner decision 9, 2026-10-12): at debug level the driver hex-dumps every TDS packet before encryption, the login packet included. sqlh's own rule is the defense: no general logger while the driver runs (AGENTS.md, Driver logging). Re-check the dump when bumping the driver: re-checked in 0.4.0, still there (`io/packet_writer.rs:597-607`, driver-0.4.0.md §4.1).

## Withdrawn

- **A pending connect attempt ignores its cancel token:** wrong; 0.2.0 stops the attempt (`run_until_cancelled`; docs/notes/connect-failures.md, "Cancel during connect").
