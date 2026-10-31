# mssql-tds 0.2.0 → 0.3.0: what changes for sqlh

2026-10-27. Step 1 of PLAN.md's driver upgrade task: the analysis, before any code changes. Sources: the two crates as cargo unpacked them (`mssql-tds-0.2.0`, `mssql-tds-0.3.0`), compared file by file (58 files differ). Upstream's CHANGELOG is one cumulative "Unreleased" list, and the repository has no version tags, so it doesn't separate the versions: many entries it lists were already in 0.2.0, and the source is the evidence. Four passes, one per area, each claim with file and line in both versions; the claims that decide anything were re-read by hand (marked **checked**). A trial build on 0.3.0 ran in a throwaway worktree. Nothing here connected to a server; the live checks are step 2's (end of this note).

## Summary

- **One compile break:** `ClientContext::multi_subnet_failover` is `Option<bool>` (0.3.0 `client_context.rs:322`, was `bool` at 0.2.0 `:316`; **checked**). The fix is one line, `Some(options.multi_subnet_failover)`, and must never be `None` (§1).
- **The trial build passes everything that needs no server** with that line: `cargo build --release` and `--features timing`, clippy on both with `-D warnings`, `cargo test` (379 unit, 94 debug process tests), `scripts/check.ps1` (all nine steps).
- **Cost:** the release exe goes from 2,812,416 to 2,893,312 bytes (+80,896, +79.0 KiB; **checked**), 252,416 bytes under the 3 MB budget. No new crates (0.3.0's `[dependencies]` are byte-identical to 0.2.0's; `Cargo.lock` changes only the `mssql-tds` line), the licence is still MIT, `cargo about` passes with today's `about.toml`, and the same ten in-box DLLs are imported. It builds on Rust 1.99.0.
- **Behaviour sqlh sees:** unchanged at sqlh's settings, except an empty `image` value, which now prints `0x` instead of `NULL` (a fix toward DESIGN.md §5.1; §3).
- **Things to accept or record:** session recovery stays off (§2), the process-wide DNS cache (§1.2), and server routing, whose wording in DESIGN.md §8 was already too strong in 0.2.0 (§1.3). These are the owner decisions at the end.

## 1. Connect and login

### 1.1 Multi-subnet failover is on by default, and `None` inherits it

- **0.3.0:** `None` takes, in order, `set_default_multi_subnet_failover`, then the environment variable `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT`, then a built-in **on** (`connection/multi_subnet_failover.rs`, `client_context.rs:826` `prepare_datasource`). The default is frozen at first use. `resolve_policy` (around `:156-205`) suppresses an inherited "on" for non-TCP protocols, a failover partner, a named instance without a port, and Windows local names without a protocol. An explicit `Some(true)` in those cases is a `UsageError`.
- **sqlh with `Some(options.multi_subnet_failover)`:** without `-M`, which is every `sqlh mcp` connect, it's `Some(false)`: the sequential path, as in 0.2.0. The environment variable is read but ignored, and even an invalid value is harmless, since the default is consulted only for `None`. connect-failures.md's behaviour stands (one attempt per address, the exit 2/exit 4 split).
- **If sqlh passed `None`:** every remote TCP connect would race all addresses at once. One `-l` bound would cover DNS and TCP, so its expiry would be `TimeoutError`, exit 4, not today's exit 2 "timed out after 2000ms". A refusal would read "All parallel connection attempts failed to …". The environment variable could change sqlh from outside, and an invalid value would fail every connect.
- **`-M` changes:**
  - Local names (`.`, `localhost`, `(local)`) now connect over parallel `tcp:localhost`; 0.2.0 failed them ("Shared Memory does not support MultiSubnetFailover").
  - `np:`, `lpc:`, `admin:`, LocalDB and `host\INST` without a port are refused before connecting, with new texts ("MultiSubnetFailover is only supported with TCP connections", "…requires an explicit TCP port for a named instance").
  - A parallel connect's timeout is exit 4.
  - The race now aborts its losing attempts when one wins, on drop and at the deadline (`parallel_connect.rs:413`; 0.2.0 left their sockets open until the OS gave up).
  - More than 64 addresses is a `UsageError` (0.2.0 truncated).
- **Step 2:** the one-line fix with its comment, a fake-server unit test that `multi_subnet_failover_policy()` is `{enabled: false, source: Connection}`, a process test that `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=true` and `=garbage` leave `-S 127.0.0.1,1` with today's exit 2 text, `MSSQL_RS_*` added to the variables `tests/cli.rs` scrubs, and DESIGN.md §4's `-M` row updated to the behaviour above.

### 1.2 A process-wide DNS cache (new `connection/dns_cache.rs`)

- **Mechanism (**checked**, `dns_cache.rs:75-101`):**
  - Every TCP connect resolves the name first.
  - Cached addresses are used only when that lookup returns an error. A lookup that times out is a dropped future, not an error, so the connect bounds and cancellation are unchanged.
  - LOGIN7 always advertises feature 0x0B. A host's addresses are kept only after an accepted login (LOGINACK, a final DONE) whose FEATUREEXTACK acknowledges 0x0B (`login.rs:916-929`, `features/dns_caching.rs:58-67`); any other acknowledgement removes the host.
  - Keys are lowercase host names, and IP literals are never cached.
  - The cache is a static with no TTL, so it lives for the whole process.
  - There is no public way to turn it off: the module is crate-private and the feature is always advertised.
- **sqlh:**
  - CLI: one connect per process, so the cache is never read.
  - `sqlh mcp`: if the server acknowledged 0x0B and the profile's server name later fails to resolve, the login, with a SQL password, goes to the addresses the name had at the last accepted login. TLS still checks the certificate against the name wherever it checks it: `strict` always (even with `-C`), `mandatory` without `-C`, and `optional` only when the server requires full encryption and `-C` is off (R46: `handler_factory.rs:494-531` negotiates Mandatory then, and `ssl_handler.rs:29-64` validates unless trusted; LoginOnly never validates).
  - So R20's binding (server name, login, encryption) holds by name. What remains is a stale address where the certificate isn't checked: `-C` without `strict`, or `optional` that negotiated login-only encryption or none.
  - On-prem SQL Server 2025 probably doesn't acknowledge 0x0B, which is an Azure feature [inferred]. Then the cache never holds an on-prem server, and step 2 checks this. **Measured 2026-10-31: the test server's verdict is `allowed=false` (§8, DNS acknowledgement).**
- **Step 2:** a live check of whether the test server acknowledges 0x0B; a sentence in DESIGN.md §8 and mcp-mode.md §5; and an entry in upstream-issues.md asking for an opt-out, for the owner to file. Resolving the name in sqlh and passing an address instead would break Kerberos SPNs, so no workaround is proposed.

### 1.3 Server routing: the password can follow a server-chosen host (0.2.0 too)

- **0.2.0 (**checked**, provider `:401-438`):** an ENVCHANGE type 20 anywhere in the login response became `Error::Redirection { host, port }`. The provider then reconnected to that host with the same `ClientContext`, the password included, up to 10 times. Type 21 was unknown and never requested.
- **0.3.0:**
  - LOGIN7 always advertises Enhanced Routing (0x0F, `login.rs:208-212`).
  - A route must follow LOGINACK and be the only one in the response, or the login fails with a protocol error (`login.rs:627-646`). It is acted on after DONE.
  - Type 21 needs an acknowledgement of 1, and it can carry a **database** that replaces the login's database for that hop (provider `:526-575`).
  - Routing data is validated: TCP only, a nonzero port, a server name of at most 1,024 characters.
  - A failed routed hop is final.
- **sqlh:**
  - In both versions a stored password can go to a host the authenticated server names (Always On read-only routing with `-K ReadOnly`, an Azure gateway). DESIGN.md §8's "a stored password is only ever sent to its own server" was literally untrue already.
  - R20's threat, an agent repointing a profile, is unaffected: only the server sqlh authenticated, inside its TLS session, can route; where the certificate isn't checked (`-C` without strict, or optional encryption the server didn't make full; §1.2) the route is as unprotected as the login itself (found in review; R46 corrected the modes).
  - The status line and `info` read the database and server the client reports after login, so they show the real target.
- **Step 2:** fake-server tests that pin it, so a later bump shows a change:
  - type 20 after LOGINACK: the LOGIN7 arrives at a second listener;
  - type 20 before LOGINACK: exit 2, protocol error;
  - type 21 without the acknowledgement: fails.

  Also: DESIGN.md §8 and the R20 note say "… or a server that server routes the login to".

### 1.4 Unchanged, or already in 0.2.0

- TLS and SSPI (`win_tls/*`, `security/` apart from the keystore), SSRP, the data source parser's order, the application name, language, and LOGIN7's field lengths (its UTF-16 fix was already in 0.2.0, `login.rs:817`).
- DNS through `tokio::net::lookup_host`: already in 0.2.0.
- The connect error variants sqlh maps (`TimeoutError`, `SqlServerError`, `Tls*`, `Certificate*`, `Security`, `Io`, `ConnectionError`, `ConnectionClosed`), so connect-failures.md's texts and the `Hints` hold on the sequential path.
- The synchronous first pipe open (upstream-issues U1) is still there (0.3.0 `named_pipes.rs:89`), so sqlh's refusal of remote Named Pipes stays.
- PRELOGIN's TRACEID now carries a connection id and an activity id (server-side XEvent correlation only).

## 2. Session lifetime and the engine

- **Session recovery stays off, and it is off in both versions (**checked**):**
  - sqlh sets `connect_retry_count = 0` (`src/db/mod.rs:675`).
  - The driver advertises the session recovery feature only when that count is above 0 (0.2.0 `login.rs:305,336`; 0.3.0 `:388,420`). Without the feature, the server never acknowledges it, so `session_recovery_negotiated` is false.
  - `check_and_reconnect`, run before every request, then never reconnects: a dead connection is `ConnectionClosed`, as before.
  - The CHANGELOG's "now works end to end" (the gate set from FEATUREEXTACK, the replayed baseline, the per-attempt bound) was already in 0.2.0 (`session_recovery.rs:87`, `handler_factory.rs:162`, `tds_client.rs:732`). One report from the build pass called it "now active in production"; it isn't for sqlh, for the reason above.
  - So decision 19's probe, the engine's one silent retry, `connect_ms` and the per-batch active-transaction check behave as in 0.2.0.
- **Why it should stay off,** if a later change ever set a retry count:
  - It would reconnect a pooled MCP connection inside the setup's time, so the status line would show `connect 0 ms` for a call that logged in.
  - It would bypass the engine's peer-drop rule.
  - A failed driver reconnect followed by the engine's own retry would be two logins.
  - Under `-l 0` its budget falls back to a day.
  - In an `--allow-write` run, a later batch would run silently in a new session, without its `#temp` tables.
  - It gains nothing for the read-only wrapper: recovery is refused while a transaction is open, and a recovered session has no transaction, which sqlh's check would catch.
- **Unchanged:**
  - attention and its 5 s acknowledgement (`ATTENTION_TIMEOUT_SECONDS`);
  - the 60 s write and 120 s drain of a withdrawn request (`CANCEL_TIMEOUT_SECS`);
  - `prepare_reset_connection` and its acknowledgement check;
  - transaction-state ENVCHANGEs (the routing subtypes are new);
  - `has_active_transaction`;
  - every `TdsClient` method signature sqlh calls.
- **Source-compatible:** `CancelHandle::cancel(self)` became `cancel(&self)` (`core.rs:48`). sqlh's two calls still compile, and its take-once pattern keeps fire-once.
- **New and opt-in, not adopted for the bump:**
  - `CancelHandle::set_cancel_timeout_secs`, a settlement budget for a cancelled request (`core.rs:64`). It addresses U5 for callers that set it. Caveat: its deadline is a `OnceLock` set by the first cancel on any handle sharing the settlement, so it would go on each batch's own handle, never on the run's.
  - `cancel_query(timeout_secs)`, which still gives no receipt for an acknowledged attention (U4 stands).
  - `client_connection_id()` and `database_change_count()`, usable as invariants.
- **Step 2:**
  - a guard in `Session::connect` that the client reports session recovery off (`is_session_recovery_enabled()`, public in both versions);
  - a fake-server test that LOGIN7 carries no session recovery feature (today the fake server discards the login payload);
  - the comment at `src/db/mod.rs:673` saying that `connect_retry_count = 0` also keeps idle connection resiliency off;
  - a line in DESIGN.md §7 and mcp-mode.md §5.

## 3. Data path and server messages

- **Empty `image` now decodes as empty, not NULL (**checked**, `datatypes/decoder.rs` 0.2.0:3065-3089 → 0.3.0:3065-3085):**
  - NULL now means only a zero text-pointer length. `text` and `ntext` already worked this way.
  - `SELECT CAST(0x AS image)` printed `NULL` and now prints `0x` (text and csv) and `"0x"` (json). The old output broke DESIGN.md §5.1 (`NULL` is SQL NULL; binary values are `0x` and hex), so this is a fix.
  - No golden file has an `image` value.
  - **Step 2:** a live test of `CAST(0x AS image)` and `CAST(NULL AS image)` in the three formats, on the plain row path and on the cursor path (add a `varchar(max)` column).
- **Already in 0.2.0** although the CHANGELOG lists them as fixes:
  - fixed-width values that span packets (0.2.0 `network_transport.rs:2043-2075` already loops);
  - the 128/32,768-character string truncation (0.2.0 `:2286-2304` already widens first);
  - empty non-final packets refused;
  - a failed login's full error list;
  - the per-command info buffer;
  - the narrow-encoding `?` (send side only; sqlh sends SQL as UTF-16).
- **Memory:** the large-value cursor paths sqlh streams through (`try_read_row_plp_complete`, `read_row_column`, the PLP chunk reads) are unchanged apart from doc comments, so no new code buffers a whole value. `drain_rows`, which `close_query` uses, now skips columns without decoding them (`tds_client.rs` 0.3.0:5066-5126), so draining an unread result costs less. `text`, `ntext` and `image` are still read whole, up to 100 MB (U3).
- **Messages:** where PRINT and low-severity RAISERROR land (PLAN.md 2.12) is unchanged. The info capture and `take_info_messages` are the same code, and the new drain still captures info.
- **Data classification (token 0xA3):** never requested. The request is crate-private and set only under `test-util`, so the server never sends it. A server that sent it anyway gets a protocol error, as in 0.2.0.
- **No compile break here:**
  - `Error` gains `EncodedValueTooLong`, and `Redirection` gains a `database` field. sqlh's matches have catch-alls and never take `Redirection` apart.
  - `CursorColumn::Value { value, .. }` is the shape sqlh already uses.

## 4. Logging

- **The driver still dumps packets at DEBUG**, the LOGIN7 with its password included (`io/packet_writer.rs:552-562`).
  - sqlh's rule stands: no general logger while the driver runs.
  - `db::phases` (`--features timing`) refuses the packet writer's target in `enabled()`, so that dump is never formatted.
  - It does format received packets (server replies only) into a string it throws away. That is unchanged from 0.2.0.
- **Nothing new logs a credential.** The new events are about:
  - the failover policy;
  - parallel-connect timeouts;
  - failed attempts and redirections (host, port, database);
  - the DNS cache (host and addresses);
  - data classification.
- **`trace_context.rs`** holds correlation ids. It is inert unless enabled, and only the driver's tests enable it.
- **The driver installs no logger.**
- **`db::phases`'s matched texts all still exist** with the same targets. "Connected to TCP transport" now fires after `sequential_connect_with` returns, still at the end of the TCP connect.
- **Step 2:**
  - `src/db/phases.rs`'s "the texts matched below are mssql-tds 0.2.0's" becomes 0.3.0's.
  - Re-verify or reword the `run_until_cancelled` comment near `src/db/mod.rs:699`.

## 5. upstream-issues.md in 0.3.0

| Entry | In 0.3.0 | Action |
|---|---|---|
| U1, the synchronous first pipe open | unchanged (`named_pipes.rs:89`) | keep the refusal of remote Named Pipes; "Seen in" 0.3.0 |
| U2, `DiscardRowWriter` allocates | half fixed: `drain_rows` skips (`ColumnPolicy::SkipAll`), but the `RowWriter` contract still can't skip a value | update; sqlh's workaround stays |
| U3, no streaming for `text`/`ntext`/`image` | unchanged (`MAX_ALLOC_SIZE` 100 MB) | keep |
| U4, no receipt for an acknowledged attention | unchanged (`cancel_query` maps an error on a live connection to `Ok`) | keep; "Seen in" 0.3.0 |
| U5, a withdrawn request waits up to 120 s | fixed for callers that opt in (`set_cancel_timeout_secs`) | update; sqlh keeps `settle_within` |
| U6, `SqlString::decode` panics on malformed UTF-8 | unchanged (`sql_string.rs:346`) | keep; the panic net stays |
| new | the DNS cache can't be turned off | add, for the owner to file |
| new | a route can't be refused | add, for the owner to file |

## 6. Decisions for the owner

**All four taken as recommended on 2026-10-31:** PLAN.md §6, decisions 40 (failover), 41 (session recovery), 42 (DNS cache) and 43 (routing).

1. **Multi-subnet failover:** sqlh passes `Some(false)` unless `-M`, never the driver's default or its environment variable. Recommended; it is what step 2 does.
2. **Session recovery stays off:** `connect_retry_count = 0`, and the engine's one retry (decision 19) is the only reconnect, with a guard and a test. Recommended; it records what sqlh already does.
3. **The DNS cache:** accept it, documented, with the opt-out request upstream; check whether on-prem SQL Server acknowledges it. Recommended.
4. **Server routing:** accept it as the server's decision, and correct DESIGN.md §8's wording. Recommended.

## 7. Live checks for step 2

1. `scripts/check.ps1 -Live`, the smoke goldens, and the smoke script's exit 1.
2. connect-failures.md's table again:
   - `127.0.0.1,1` (about 2.1 s, exit 2);
   - an unroutable address with `-l 2` (2.0 s, exit 2);
   - a silent listener with `-l 1` (exit 4 at 2 s);
   - Ctrl+Break during a connect (exit 6 within 2 s);
   - `localhost,1`;
   - an IPv6 address;
   - `np:` to a remote host (exit 3);
   - a subset of these under `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=true` and `=garbage`.
3. New `-M` rows for DESIGN.md §4:
   - an unroutable address (exit 4);
   - `localhost,1`;
   - `host\INST` without a port (usage error);
   - Ctrl+Break during a `-M` connect.
4. Connect latency with `--features timing --verbose` against connect-latency.md's step figures, which also shows that `db::phases` still finds its events.
5. Whether the test server acknowledges DNS caching (0x0B).
6. Empty `image` in three formats and both row paths (§3).
7. Memory: a 1 GB `varchar(max)` with `--max-cell 500` near 10 MB (`run_measured`).
8. Message order: PRINT and RAISERROR around result rows (2.12).
9. A failed login's text and hint (18456).
10. `live::mcp_tools`, `db::spike::spike_reset`, the `-t 1` timing tests, the lock-timeout, deadlock and Ctrl+Break tests, harness kill (harness-kill.md).
11. New: after a connect and after an MCP call, session recovery reports off and no recovery has happened; `KILL` a pooled session's SPID from another session between two calls, and the next call shows the engine's retry (`connect` above 0 ms) and a fresh session.

## 8. Step 2: the upgrade, as built and measured (2026-10-31)

On the local branch `driver-0.3.0`, decisions 40–43 taken (PLAN.md §6).

**Code (`src/db/` only):**
- `Cargo.toml` pins `=0.3.0`; `Cargo.lock` changes only the `mssql-tds` line.
- `multi_subnet_failover = Some(options.multi_subnet_failover)` (decision 40).
- `Session::connect` fails with a protocol error if the client ever reports session recovery negotiated (decision 41's guard). On 0.3.0 it can't fire, since the driver never asks for recovery with a retry count of 0 (the LOGIN7 test below shows it doesn't); it guards a later version.
- `connect_error`: under `-M`, the driver's "Connection timeout: ..." from its race of every address is a connect failure, exit 2, `no address accepted a TCP connection within N s (-M tries them all at once)`; it had come out as a missed login deadline, exit 4, naming `-l` + 1 s (found in review, below).
- Comments: `connect_retry_count = 0` also keeps idle connection resiliency off; `run_until_cancelled` holds in 0.3.0; `db::phases`' texts are 0.3.0's.
- Exe: 2,895,360 bytes after the review's fixes, 250,368 bytes under the 3 MB budget (the trial build measured the driver's own share: +80,896 bytes over 0.2.0 on the same tree, §Summary); the same ten in-box DLLs.

**New tests:**
- `db::fake_server::the_login_asks_for_no_session_recovery_and_failover_is_sqlhs_own` reads the LOGIN7's FeatureExt list: no 0x01 (session recovery), 0x0B and 0x0F present; the resolved failover policy is off, from the connection.
- `db::fake_server::a_route_after_the_login_acknowledgement_is_followed_and_one_before_it_is_refused` pins routing (decision 43): an ENVCHANGE 20 after LOGINACK takes the login to the named server; one before it is refused (`routing ENVCHANGE before LOGINACK`), and so is an Enhanced Routing one, type 21 with a database, that the login didn't acknowledge (`without an Enhanced Routing feature acknowledgement`); a refused route sends no second login. Each refused reply is otherwise a whole login, so a driver that ignored the route would connect and the test fail.
- `db::tests::a_failover_race_that_times_out_is_a_connect_failure`: the `-M` timeout's wording and exit code, and that the login deadline's keeps its own.
- `the_drivers_failover_default_variable_changes_nothing` (process test): `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=true` and `=garbage` leave a refused connect at exit 2 with the sequential refusal itself (`os error 10061`) and none of the driver's own refusal of a bad value (`Usage Error`, the variable's name). The variable is scrubbed from every process test (`isolated`).
- `ctrl_break_cancels_a_connect` now runs with and without `-M`: exit 6 within 2 s either way.
- `live::an_empty_image_is_0x_and_only_a_null_one_is_null`: text, json and csv, each on the plain row path and on the cursor.
- `live::mcp_tools::a_pooled_connection_killed_between_calls_is_replaced_by_the_engines_retry`: `KILL` of the pooled session from another session between calls; the next call reports a connect time and runs on a new connection (`sys.dm_exec_connections.connection_id`). **Found while writing it:** a connection reset gives the session a new `login_time` in `sys.dm_exec_sessions`, so login time can't tell a reused session from a new one; the connection id can.

**Checks:** `scripts/check.ps1 -Live` passes on 0.3.0, on the branch with the review's fixes: 382 unit tests, 95 debug and 89 release process tests, 60 debug and 54 release live tests (the smoke goldens unchanged, `live::mcp_tools`, the `-t 1` timing, lock-timeout, deadlock, Ctrl+Break and harness-kill tests, the memory tests' bounds). `db::spike::spike_reset` passes all seven sections.

**Re-measured** (release build, the test server, 2026-10-31):

| Measurement | 0.2.0 (the notes) | 0.3.0 |
|---|---|---|
| `-S 127.0.0.1,1 -l 15` | about 2.1 s, exit 2 | 2.14 s, exit 2 |
| `-S 192.0.2.1 -l 2` | 2.0 s, exit 2, "timed out after 2000ms" | 2.11 s, exit 2, the same text |
| `-S localhost,1`; with `-l 2` | 4.1 s, exit 2; 3.1 s, exit 4 | 4.18 s, exit 2; 3.11 s, exit 4 |
| `-S 2001:db8::5 -l 2` | about 2.05 s | 2.12 s, exit 2 |
| `-S np:192.0.2.1` | exit 3 | exit 3, 98 ms |
| the same under `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=true` / `=garbage` | — | unchanged (exit 2 at 2.13 s; exit 4 at 3.11 s) |
| `-M -S 192.0.2.1 -l 2` | — | first exit 4 at 2.10 s, worded as a missed login deadline of 3 s although the race had ended at `-l`; since the review's fix, exit 2 at 2.0 s, `no address accepted a TCP connection within 2 s (-M tries them all at once)` |
| `-M -S localhost,1`; `-M -S 127.0.0.1,1` | — | exit 2 at 2.13 s, "All parallel connection attempts failed" |
| `-M -S host\INST` (no port) | — | exit 2 at once, "Usage Error: MultiSubnetFailover requires an explicit TCP port for a named instance" |
| a failed SQL login (a login that doesn't exist) | error 18456 and its hint, exit 2 | the same |
| connect steps, `--features timing --verbose`, three runs | tcp 5, prelogin 6, TLS 26, login 150, total 195 ms (medians) | tcp 6–41, prelogin 4, TLS 23–74, login 162–171, total 197–292 ms |
| peak working set: 1 GiB `varchar(max)`; 100 MiB; 20 rows of 50 MB (`--max-rows 1 --exact-count`) | 10.3 MB (19.4 s); 9.7 MB; 9.7 MB (15.8 s) | 10.4 MB (18.8 s); 10.4 MB (2.1 s); 10.4 MB (15.3 s) |

So connect bounds, exit codes, texts, latency and memory are 0.2.0's; `-M`'s rows are new and are in DESIGN.md §4.

**Review (2026-10-31, internal: three lenses over `main..driver-0.3.0`, a verifier per finding; not the external reviewer):** 13 findings confirmed, 4 rejected, all fixed:
- **P2, an output defect:** under `-M` a race that timed out was reported as a missed login deadline (exit 4, naming `-l` + 1 s) although it ended at `-l`; now a connect failure worded for it (exit 2), with a unit test, and DESIGN.md §4's `-M` row corrected (it had called the mechanism the login deadline, and called `np:`, LocalDB and the local-name cases measured where they were read from the source; a remote `np:` is sqlh's own exit 3).
- **P2, a record:** the live counts here were from before the last new test; re-run above.
- **P3, tests that could pass for the wrong reason:** the refused route (its reply lacked the collation, so any driver failed it), the `garbage` value of the failover variable (the driver's own usage error met every assertion), and the empty `image` (json and csv on one path only). Each now fails for the wrong reason too.
- **P3, planned and missing:** the Enhanced Routing (type 21) test, Ctrl+Break under `-M`, the session-recovery line in DESIGN.md §7 and mcp-mode.md §5, and the DNS cache in mcp-mode.md §5; all added.
- **P3, wording:** the routing sentence in DESIGN.md §8 and §1.3 above now says it holds only with a checked certificate (not under `-C` or optional encryption); U2 rewritten for 0.3.0; connect-latency.md's "about 80%", which its own figures didn't support, replaced.
- **Rejected:** that the guard is dead code (it guards a later version, as stated); that 0.2.0 labels left in comments are wrong (they are history); a MB/MiB nit; and that the KILL test could kill another session (the SPID is the test's own pooled connection's, checked by its connection id).

**Not settled:** whether on-prem SQL Server 2025 acknowledges DNS caching (decision 42). A one-off probe through the driver's `test-util` API, never committed, found the test profile's server named by an IP literal, which the driver never caches, so it couldn't tell; checking with a host name would mean sending the profile's stored password to a spelling other than its own, which R20's binding refuses, so it was not done. Still the likely answer: no, an Azure feature [inferred]. The external review (below) showed a way that needs no other spelling.

**External review (2026-10-31, `048aa89..7b28076`; PLAN.md, Driver 0.3.0 external review):** accepted, no blocking code finding, one P3, R46; fixed on the branch:
- **R46, the certificate by negotiated mode.** Whether the certificate is checked depends on the encryption the connection negotiated, not on the flag alone (`handler_factory.rs:494-531`, `ssl_handler.rs:29-64`, unchanged from 0.2.0): strict checks it even with `-C`; mandatory unless `-C`; optional negotiates Mandatory, checked unless `-C`, when the server requires encryption, and otherwise LoginOnly (never checked) or none. DESIGN.md §8, mcp-mode.md §5, §1.2–§1.3 above and U7 now say so.
- **Found while fixing R46, an output defect of the same rule:** under `-Ns`, a certificate failure's hint said to pass `-C` (or set `trust_server_certificate`), which strict ignores, so an agent following it failed again. It now says `strict encryption checks the server's certificate even with -C, and needs SQL Server 2022 or later: ...`, and offers `-Nm`, where `-C` applies (`db::tests::a_certificate_failure_under_strict_encryption_offers_no_trust`; DESIGN.md §6, mcp-mode.md §4).
- **Judgment 4's positive case, promoted:** `db::fake_server::an_acknowledged_enhanced_route_takes_the_login_to_its_database`: a type-21 route with FEATUREEXTACK 0x0F = 1 is followed, the routed LOGIN7 asks for the route's database, and it asks for no session recovery.
- **Judgment 2:** decision 40's "a parallel timeout is exit 4" is marked superseded in PLAN.md §6.
- **Judgment 3, the DNS acknowledgement, without R20's problem:** on the IP-named profile, `DnsCache::apply_verdict` still logs `Applied DNS caching acknowledgement` with its `allowed` Boolean; a temporary recorder for that one event, keeping the Boolean only (as `db::phases` does), would show a `true` verdict; telling an omitted 0x0B from a negative bit needs temporary FEATUREEXTACK instrumentation.

**DNS acknowledgement, measured (owner-approved, 2026-10-31): the test server (on-prem SQL Server 2025, 17.0.1000) does not allow DNS caching.**
- **Method:** a throwaway worktree of `1dd341f`, never committed and removed after the run. In it, `db::phases`' recorder also listened to the `::dns_cache` target. Of that one event, `Applied DNS caching acknowledgement`, it kept only `allowed=true` or `allowed=false`, and printed nothing else. No other target was added, and no packet or general driver logging was enabled.
- **Run:** a debug `--features timing` build, three runs of `--profile test --verbose -Q "SELECT 1 AS one"`, read-only. The profile was unchanged, and so was its spelling of the server. Without `--verbose` the recorder is off and no event is seen.
- **Result:** each run printed `verdict event seen, allowed=false` and exited 0.
- **Why `false` is conclusive** (`login.rs:913-929`):
  - The verdict is applied only after a completed, accepted login with no errors that carried a FEATUREEXTACK. A malformed one would have failed the connect.
  - So a well-formed FEATUREEXTACK came back, and either it didn't list 0x0B or bit 0 was clear. Both mean the cache never holds this server.
  - Telling those two apart doesn't change the outcome, so it was not instrumented.
- **So:** decision 42's residual risk does not arise against this server. It applies only where a server allows the cache, likely Azure SQL [inferred: no Azure server was tested].
