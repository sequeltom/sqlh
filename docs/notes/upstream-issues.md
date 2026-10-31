# Issues to raise with mssql-tds upstream

The one list of driver problems worth raising with Microsoft's [mssql-rs](https://github.com/microsoft/mssql-rs) (`mssql-tds`). **The owner files them; agents add to this list and never file anything themselves** (owner, 2026-10-17). Upstream takes issues, not pull requests (AGENTS.md).

Before filing one: check upstream's `main` (0.3.0 when these were read; crates.io has 0.2.0), since the API moves and a problem may be gone; search the existing issues; and keep sqlh's workaround in our code either way, since sqlh pins an exact version.

Each entry says what was read or observed, in which version, why it matters to sqlh, what sqlh does about it, and a possible fix.

## To raise

### U1. The first open of a remote Named Pipe blocks the async runtime

- **Seen in:** 0.2.0, reading the source (docs/notes/connect-failures.md, PLAN.md 2.4).
- **What:** `open_named_pipe_with_retry` calls tokio's `ClientOptions::open`, a synchronous `CreateFileW`, inside the async fn, whether the pipe came from the protocol waterfall, an `np:` prefix or a UNC path. For an unreachable host that stalls every timer, cancellation and task for about 21 s (SMB's own timeout), and the error that comes back is the pipe's instead of the TCP timeout. The retry after `ERROR_PIPE_BUSY` is not part of this: its `WaitNamedPipeW` already runs on `spawn_blocking`. Related: an IPv6 address can't take a `tcp:` prefix, so without a port it gets the waterfall and the same freeze.
- **sqlh's workaround:** remote Named Pipes are refused before connecting (exit 3, with the TCP spelling), and an IPv6 address without a port is sent with port 1433, so the driver uses TCP (DESIGN.md §4, `-S`).
- **Possible fix:** run the first open on a blocking thread too and bound it by the connect timeout (the open itself can't be cancelled, but the runtime and its timers stay free), or leave remote names on TCP unless Named Pipes are asked for.

### U2. `DiscardRowWriter` allocates what it discards

- **Seen in:** 0.2.0, measuring memory (docs/notes/memory.md, PLAN.md 2.7).
- **What:** a `RowWriter` can't decline a PLP value without the decoder allocating its declared length (`value_destination` has no way to say "skip"), so `DiscardRowWriter`, `drain_rows` and every consumer that wants a prefix of a large value pay for all of it.
- **sqlh's workaround:** large values are read through the column cursor and cut as they arrive (`src/db/stream.rs`); hidden rows use the cursor's skip.
- **Possible fix:** a skip or a limit in the writer's contract, or `drain_rows` using `ColumnPolicy::SkipAll` as `drain_result_set_rows` does.

### U3. `text`, `ntext` and `image` have no streaming path

- **Seen in:** 0.2.0 (docs/notes/memory.md).
- **What:** the pull cursor returns them as `CursorColumn::Value`, fully decoded, and the decoder refuses a value over 100 MB (`MAX_ALLOC_SIZE`), a limit the PLP types don't have.
- **sqlh's workaround:** none possible above the driver; the limitation is documented (DESIGN.md §11, README).
- **Possible fix:** the same cursor streaming the PLP types get.

### U4. No receipt for an acknowledged attention

- **Seen in:** 0.2.0, reading the source and on SQL Server 2025 (docs/notes/mcp-reuse-spike.md §2, `Session::failure`'s comment in `src/db/mod.rs`).
- **What:** after a cancel, a caller can learn only whether the connection was marked dead (`is_connection_dead()`). A kept connection follows an acknowledged attention (DONE with the ATTN flag), but also a cancel that came before the request was sent, and a half-sent request withdrawn with EOM|IGNORE, where the server confirmed nothing. The settlement marker is private, and `send_attention_with_timeout` would send a new attention rather than report on the old one.
- **Why it matters:** a pool deciding whether to reuse a connection after a cancel, and a user told whether the server confirmed it, both want the receipt.
- **sqlh's workaround:** owner decision 17, as amended: a connection the driver kept is reused, and the next call's reset, whose acknowledgement the driver verifies, is the check; a dead one is discarded. Messages say only what was observed ("the connection stayed usable"), never that the server confirmed.
- **Possible fix:** an outcome on the cancelled operation's error, or a query on the client: acknowledged, never sent, withdrawn, or unacknowledged (dead).

### U5. Withdrawing a half-sent request waits up to 120 s, without the cancel

- **Seen in:** 0.2.0 (PLAN.md, Phase 2 follow-up, R15 B).
- **What:** a request cancelled between two of its packets is withdrawn (EOM|IGNORE) and its DONE awaited in `withdraw_sent_message` for up to 120 s, with the cancel removed and no bound from the caller's timeout; an attention, by contrast, gets 5 s.
- **sqlh's workaround:** `settle_within`: a batch's work races `-t` plus a 5 s grace; past it the work is dropped where it stands and the session retired (exit 4).
- **Possible fix:** bound the withdrawal by the operation's timeout or the attention's 5 s, and keep the cancel honored while it waits.

### U6. `SqlString::decode` panics on malformed UTF-8

- **Seen in:** 0.2.0 (docs/notes/memory.md). Already upstream's own **#310**: add a comment or a reproduction there rather than a new issue.
- **What:** under a UTF-8 collation, `String::from_utf8(...).unwrap()` panics on a malformed value.
- **sqlh's workaround:** sqlh's cutter decodes with replacement, so a malformed value shows U+FFFD where the driver's path would have panicked.

### U7. The DNS cache can't be turned off

- **Seen in:** 0.3.0, reading the source (docs/notes/driver-0.3.0.md §1.2; owner decision 42).
- **What:** LOGIN7 always advertises DNS caching (feature 0x0B), and when a server acknowledges it the addresses the name resolved to are kept in a process-wide cache with no TTL (`connection/dns_cache.rs`), used whenever a later lookup of that name fails. The module is crate-private and the feature is always requested, so a client can't opt out.
- **Why it matters to sqlh:** `sqlh mcp` is a long-lived process; in a DNS outage, its login, with a SQL password, goes to the addresses the name had at the last accepted login, which TLS covers only by the certificate's name (not under `-C` or optional encryption).
- **sqlh's workaround:** none (resolving names itself would break Kerberos SPNs); documented in DESIGN.md §8.
- **Possible fix:** a `ClientContext` option to not advertise 0x0B or not use the cache, or a TTL.

### U8. A route can't be refused

- **Seen in:** 0.2.0 and 0.3.0, reading the source (docs/notes/driver-0.3.0.md §1.3; owner decision 43).
- **What:** an ENVCHANGE routing token (type 20, and in 0.3.0 type 21 with a database) makes the provider log in to the named host with the same `ClientContext`, the password included; 0.3.0 always advertises Enhanced Routing (0x0F). There is no option to refuse or confirm a route.
- **Why it matters to sqlh:** a stored password then reaches a host sqlh didn't name; acceptable as the server's decision (Always On read-only routing), but a tool that binds a password to one server would want the choice.
- **sqlh's workaround:** none; documented in DESIGN.md §8, and pinned by fake-server tests in the 0.3.0 upgrade.
- **Possible fix:** a `ClientContext` option to fail on a routing token, or a callback to approve its target.

## Decided not to raise

- **The password in debug packet dumps** (owner decision 9, 2026-10-12): at debug level the driver hex-dumps every TDS packet before encryption, the login packet included. sqlh's own rule is the defense: no general logger while the driver runs (AGENTS.md, Driver logging). Re-check the dump when bumping the driver.

## Withdrawn

- **A pending connect attempt ignores its cancel token:** wrong; 0.2.0 stops the attempt (`run_until_cancelled`; docs/notes/connect-failures.md, "Cancel during connect").
