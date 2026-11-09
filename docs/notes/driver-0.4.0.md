# mssql-tds 0.3.0 → 0.4.0: what changes for sqlh

2026-11-08. This is step 1 of the driver upgrade to 0.4.0 (released 2026-11-07): the analysis, before any code changes.

**Sources.** The two crates as cargo unpacked them (`mssql-tds-0.3.0`, `mssql-tds-0.4.0`), compared file by file. 51 files differ and 7 are new: `build.rs`, `connection/blocking.rs`, `connection/connect_stage.rs`, `connection/transport/tls/network_framework_engine.rs` with its directory (macOS only), and `docs/client-guide.md`, `docs/connection-behavior.md`, `docs/data-classification.md`. Neither crate ships a CHANGELOG, so the source is the evidence. The new docs mostly move 0.3.0's README text (§4.3).

**Method.** Six areas: connect and transport; login and authentication; the client and session; the data path; logging, security and the compile surface; upstream's docs and new features. One pass read each area. An independent checker then re-read every claim against both versions and sqlh, corrected some, and added what the first pass missed. Only claims that survived are here.

**Conventions.** Driver paths are under each crate's `src/`, and "provider" means `connection_provider/tds_connection_provider.rs`. A bare driver line number is 0.4.0's; 0.3.0's are labelled. Every claim was read in source unless marked [inferred]. **checked** marks the claims a decision or a code change rests on.

**Limits.** Nothing was built, no cargo command ran, nothing connected to a server, and no network was used. The trial build, the cost and the live checks belong to step 2 (§8, §9).

**Not read by any pass:**
- the macOS Network.framework engine;
- the driver's test modules, apart from the provider test cited in §6.1;
- the line-by-line bodies of the RPC, TVP, bulk copy, `cursor_ops` and Always Encrypted diffs (only their hunks and signatures);
- sqlh's `tests/cli.rs` (grep only).

## Summary

- **No compile break found (**checked**).** Every driver item that sqlh's `src/db/` names keeps its shape:
  - `create_client` (provider `:211-216`), the `ClientContext` fields sqlh assigns, and `EncryptionOptions`' four fields (`core.rs:342-355`; that file's diff is a doc comment only);
  - `ExecuteOptions`, `BatchErrorMode` and `StatementResult` (0.3.0 `tds_client.rs:9476-9643` is byte-identical to 0.4.0 `:9924-10091`), plus `CursorColumn`, `CursorPoll`, `PlpChunk` and every `TdsClient` method sqlh calls;
  - `SqlString::decode`, `SqlCollation`, `VectorData`, `security::{canonicalize_hostname, make_spn}`, `TimeoutErrorType::String` and `SecurityError::LoadLibraryFailed`.

  Four changes touch sqlh's code, and all still compile:
  - `TlsHandshakeError` lost `cert_sans` and now boxes its source; sqlh matches it with `{ .. }` (`src/db/mod.rs:1681`, `:1718`).
  - The new `Error::TvpCellSerialization` falls into sqlh's catch-alls (`:1376`, `:1709`).
  - `ClientContext` gains a field; sqlh starts from `default()` and assigns fields (`:677-739`).
  - `ResultSet` gains a method with a default body; sqlh implements no driver trait.

  The bump itself is `Cargo.toml` and `Cargo.lock`. Step 2's trial build confirms it.
- **Cost: unknown until step 2's trial build.**
  - New code compiled into the Windows exe: `blocking.rs`, the connect-stage spans on every connect (not cfg-gated), the data-classification reading, and `diagnose_client`. sqlh never calls `diagnose_client`, so LTO likely drops it [inferred].
  - Headroom: 145,920 bytes (2,999,808 of 3 MiB; PLAN.md §4, the 0.1.0 release-check review).
  - No new Windows crate. windows-sys stays at 0.59 and gains `Win32_Security` and `Win32_System_Threading` (driver `Cargo.toml:242-251`). These serve `blocking.rs`'s `OpenThreadToken`, `SetThreadToken` and `RevertToSelf` (advapi32.dll) and `GetCurrentThread` (kernel32.dll). sqlh already imports both DLLs (**checked** in windows-sys-0.59.0's source, not in the exe).
  - `block2` is macOS-only (`:208-210`). `Cargo.lock` therefore gains macOS-only entries: block2 0.6.2 and the objc2 crates it needs, all MIT. They aren't compiled for Windows and don't reach THIRD-PARTY-NOTICES, because `about.toml` lists only `x86_64-pc-windows-msvc`. Unlike 0.3.0's bump, `Cargo.lock` changes beyond the `mssql-tds` line.
  - Licence MIT (`:40`), edition 2024, no `rust-version`. The new `build.rs` runs on the Windows build host and returns at once (`build.rs:12-16`).
- **Behaviour sqlh sees at its settings:**
  - **U9 partly fixed (§1.1):** the driver's SPN lookup and every SSPI call now run on tokio's blocking pool, inside `-l` + 1 s and Ctrl+C. sqlh's 500 ms pre-lookup stays (§7.3), but its comments and DESIGN.md §4.2's `-E` row become wrong. (Superseded: decision 72 replaced the pre-lookup with a per-hop provider, §10.)
  - **A refusal gap that the bump opens (§1.2):** 0.4.0 accepts `np:` or `tcp:` before an IPv6 address. sqlh still parses the 0.3.0 way, so `np:<IPv6>\INST` and `np:\\<IPv6>\pipe\...` get through to a remote pipe open (U1). Step 2 must fix this. The same parser change lets sqlh keep an IPv6 named instance on TCP.
  - **Connect error detail (§1.3):** Schannel failures now read `TLS handshake failed for '<host>': ...` instead of `Unexpected Implementation Error: Schannel TLS handshake failed: ...`. Hints and exit 2 are unchanged.
  - **LOGIN7 asks for vector feature version 2 (§3.1),** because the driver's default moved. float16 vector columns may print differently. §7.2.
  - **The driver's request timeout charges the send against the response (§2.1).** Only sqlh's own short statements use that timeout.
  - **An unreadable token now retires the connection in sqlh's own statements too (§2.2).** This works in sqlh's favour.
  - **Unchanged:** sequential TCP, `-l`, one attempt per address, the exit 2/exit 4 split, `-M`'s texts, the DNS cache (U7), routing (U8), session recovery off, cancel and attention, the reset, messages and row counts, PLP streaming and the cursor, value rendering, U10 and U11.
- **Things to accept or record:**
  - SQL Browser replies must now come from port 1434 (§1.4);
  - the LocalDB and `lpc:` changes, and the SSPI loopback flag computed per hop (§1.5);
  - a retired transport is dropped without TLS shutdown (§2.2);
  - data classification is public and never requested (§3.2);
  - `abandon_query` not adopted (§2.4);
  - any tracing subscriber would silence `db::phases` (§4.3).

  The decisions for the owner are §7.1–§7.5.

## 1. Connect and login

### 1.1 The driver's SPN lookup and SSPI calls leave the runtime thread (U9, partly fixed)

**Superseded by decision 72 (2026-11-08):** the pre-lookup this describes is replaced by a per-hop `SecurityContextProvider` (`db::spn`, §10); what follows is step 1's analysis, kept as written.

- **0.3.0 (`handler_factory.rs:736-737`, `:597-598`):** three calls ran inline in the async login, on the runtime thread:
  - `SspiAuthHandler::new`, which calls `WindowsSspiContext::new` and then, when `server_spn` is None, `make_spn_canonicalized` and its blocking `dns_lookup` (`security/windows/sspi_context.rs:99-102`);
  - `get_initial_token`;
  - `process_challenge`.

  The only `spawn_blocking` in 0.3.0's `src` is `named_pipes.rs:126`.
- **0.4.0 (**checked**):**
  - `SspiAuthHandler::new` and `get_initial_token` run inside `off_runtime_thread` (`handler_factory.rs:804-816`). Each challenge round goes through `process_challenge_off_runtime` (`:663-666`; `sspi_handler.rs:201-213`).
  - `off_runtime_thread` is `tokio::task::spawn_blocking` carrying the caller's impersonation token (`connection/blocking.rs:13-26`). Dropping its future abandons the work on its thread.
  - `spn.rs` and `sspi_context.rs` are byte-identical, so the lookup is the same code on another thread. It still has no bound of its own and no option to skip it.
  - The await sits inside the provider's login deadline (computed at `:333-338`, applied at `:546-560`, and again per redirect hop at `:787-797`) and inside `run_until_cancelled` (`:316`).
  - `AcquireCredentialsHandleW` and `InitializeSecurityContextW`, with any Kerberos exchange inside them, moved too.
- **What it means for sqlh.** sqlh leaves the lookup to the driver in three cases:
  - its own 500 ms pre-lookup came back in time (the driver then repeats it);
  - a named instance without a port (`host_and_spn` gives no SPN, `src/db/mod.rs:1529-1533`);
  - `-K ReadOnly` (no pre-lookup, `:683-691`).

  In those cases:
  - **The runtime is no longer frozen.** `-l` + 1 s (`:724-729`), Ctrl+C (`:750-755`) and `sqlh mcp`'s other calls keep running.
  - **It still has no bound of its own.**
    - With `-l 3` or less, a 5 s failed reverse lookup is still exit 4, `login timeout: ... didn't complete the login within N s`, with a message that blames the server (`:1647-1653`). It now ends at the deadline rather than after the lookup (0.3.0's timing was measured: connect-latency.md:75-76).
    - At `-l 15` it costs about 5 s and connects.
    - At `-l 0` it is unbounded but cancellable.
  - **Abandoned lookups can pile up.**
    - A lookup abandoned by a timeout or cancel holds a blocking-pool thread until it ends. In 0.3.0 the driver's lookups ran one at a time on the runtime thread.
    - They fall outside R53's cap (`LOOKUP_THREADS`, `:1468`), bounded only by tokio's default of 512 blocking threads. That default is in tokio 1.53.2, sqlh's locked version (`runtime/builder.rs:306`), which also keeps idle threads 10 s (`blocking/pool.rs:172`). sqlh sets no `max_blocking_threads` (`src/app.rs:201-203`, `src/mcp/mod.rs:161-163`).
    - tokio's `lookup_host` already did this for the driver's TCP DNS, so the risk is not new in kind.
  - **Exit doesn't wait for them.** Both runtimes end with `shutdown_background` (`src/app.rs:228`, `src/mcp/mod.rs:177`), and their comments (`app.rs:225-227`, `mcp/mod.rs:173-175`) already describe this case.
  - **A panic in the moved work still exits 10.** sqlh's panic hook exits from any thread (`src/app.rs:97-118`), so `off_runtime_thread`'s `JoinError` mapping (`blocking.rs:20-24`) is never reached.
  - **Small new cost.** Every Windows-auth login now makes at least two blocking-pool hops, plus one per challenge round and one `OpenThreadToken` per hop [inferred negligible, not measured].
- **Impersonation capture does nothing for sqlh.**
  - sqlh calls no `Impersonate*`, `SetThreadToken` or `RevertToSelf` (grep of `src` and the tests). `OpenThreadToken` therefore returns `ERROR_NO_TOKEN`, and the work runs as the process (`blocking.rs:97-121`).
  - Any other failure there would fail a `-E` connect as `Security(InternalError("could not read the calling thread's impersonation token: ..."))` (`:115-120`), which sqlh labels with its Kerberos/SPN hint (`src/db/mod.rs:1698-1704`). No trigger is known.
  - `Reverting::drop` aborts the process if `RevertToSelf` fails (`:142-150`), a path only an impersonating caller takes.
- **sqlh's workaround stays (§7.3).** `db::spn_if_lookup_is_slow` still saves about 5 s per connect to an address with no PTR record, and keeps `-l 1` and `-l 3` working (576 and 612 ms, connect-latency.md:78), as DESIGN.md §4.2 promises. These texts are now inaccurate:
  - the doc comment at `src/db/mod.rs:1439-1449` ("a blocking call on the runtime thread ... -l can't bound it, a cancel can't reach it", "unbounded lookup");
  - DESIGN.md §4.2's `-E` row (`:111`), which says the R52 cases "keep the driver's own, unbounded lookup" and that "The driver's own synchronous lookups do not use that worker cap (R56)". Those lookups are now bounded by `-l` + 1 s, cancellable, and run on tokio's blocking pool, which sqlh doesn't cap either;
  - U9 ("the stall remains for those") and connect-latency.md:79 ("with its stall").

  PLAN.md's R52 acceptance (`:227`) is a dated record and stays as written.

### 1.2 A protocol prefix before an IPv6 address: a gap in sqlh's remote-pipe refusal

- **0.3.0 (`connection/datasource_parser.rs:236-251`):** no prefix was split off text containing `::` or more than one colon (`:240-242`).
- **0.4.0 (**checked**, `:234-256`; test `a_prefixed_ipv6_address_keeps_its_prefix`, `:1086-1105`):**
  - `tcp`, `np`, `lpc` or `admin` before the first colon is a prefix, whatever follows: `tcp:::1` is TCP to `::1`, and `tcp:fe80::1,1433` is server `fe80::1`.
  - After a prefix, text starting with `\\` is a pipe path, even after `tcp:` (`:134-137`; `parse_named_pipe`, `:259-325`).
  - `np:` or `admin:` followed by `,port` fails with `Protocol Error: Port parameter only valid for TCP protocol, got: np` (`:373-381`).
- **sqlh still parses the 0.3.0 way.** `split_protocol` and `looks_like_ipv6` (`src/db/mod.rs:1413-1432`) say they split "the way the driver does". Their test pins 0.3.0's rule (`protocol_prefixes_are_split_like_the_driver_does`, `:2150-2165`: `("np:::1", None)`, `("tcp:2001:db8:0:0:0:0:0:5", None)`), and passes because it tests only sqlh's own function.
- **What now reaches a remote pipe open.** U1 is the synchronous `CreateFileW` on the runtime thread (`named_pipes.rs:87-89`, byte-identical). 0.3.0 read each spelling below as a server name, which failed harmlessly at name resolution.
  - **`np:<IPv6>\<instance>`, for example `np:2001:db8::5\SALES`.**
    - `check()` (`src/db/mod.rs:1557-1590`) finds no protocol and accepts it. `data_source` then passes it as written, because it contains `\` (`:1403`).
    - The driver asks SQL Browser. After any answer, even one listing TCP only, it queues Browser's pipe, then always the default `\\<host>\pipe\MSSQL$<instance>\sql\query` (`datasource_parser.rs:793-801`, `:669-679`; an empty pipe slot is skipped, `connection_actions.rs:515-520`).
    - If SQL Browser fails, the connect ends before any pipe (provider `:432`, `:436`, `:670-678`).
  - **`np:\\<IPv6>\pipe\...` and `tcp:\\<IPv6>\pipe\...`:** no SQL Browser; the pipe opens at once (the explicit pipe branch, `:830-837`).
  - **Not `np:<IPv6>` or `admin:<IPv6>` alone.** sqlh appends `,1433` (`src/db/mod.rs:1403-1404`), and the driver refuses the port at once: exit 2 with the error above, where 0.3.0 failed at name resolution.
  - **Every source of the server is affected,** profiles and `profiles add` included (`check_server`, `:1553`).
- **Unsettled.** Does `CreateFileW` on a raw IPv6 UNC name (`\\2001:db8::5\pipe\...`) stall for SMB's roughly 21 s, or fail at once? Windows documents the `ipv6-literal.net` name as the UNC form for an IPv6 host [inferred, not tried]. Live check 5 measures it. The fix stands either way: DESIGN.md §4.2's `-S` row refuses every remote pipe, and README.md:185 promises that sqlh never makes "the slow, uncancellable Named Pipes attempt" to another machine.
- **`host_and_spn` (`:1506-1535`)** reads `tcp:<IPv6>[,port]` as the host `tcp:<IPv6>`. 0.3.0 failed on that spelling anyway; 0.4.0 connects it. The wrong fallback SPN would be used only if sqlh's lookup of that bogus name outlasted 500 ms [inferred unlikely].
- **The upside (**checked**).**
  - With `tcp:`, a named instance gets SQL Browser and then TCP on Browser's port only. The pipe fallback is queued only when no prefix is given (`datasource_parser.rs:803-811`, the check at `:809`). 0.3.0 had the same arms (`:781-806`) but couldn't split `tcp:` off an IPv6 address.
  - Today `data_source` sends `2001:db8::5\SALES` unprefixed (`src/db/mod.rs:1403`), so the driver may open Browser's remote pipe. That is U1, on 0.3.0 too, and the reason for DESIGN.md §4.2's "with an instance name it can't be forced onto TCP".
  - On 0.4.0, `data_source` can send `tcp:` before every remote IPv6 spelling, as it already does for host names (`:1401-1402`).
- **Stale:** the doc comments at `src/db/mod.rs:1386-1392` ("An IPv6 address can't take `tcp:`") and `:1418-1421`.
- **Not traced:** what 0.4.0 opens for `np:::1`, which the fix will pass as written: `\\.\pipe\...` or `\\::1\pipe\...` (`pipe_server`, `datasource_parser.rs:641-661`). Either is local, so neither is U1's hang.
- **Step 2:** §9, item 2.

### 1.3 Schannel failures are `TlsHandshakeError`

- **0.3.0 (`win_tls/engine.rs:66-69`, `:93-95`):** `ImplementationError("Schannel AcquireCredentialsHandle failed: {e}")` and `ImplementationError("Schannel TLS handshake failed: {e}")`, displayed as `Unexpected Implementation Error: ...` (0.3.0 `error/mod.rs:222`).
- **0.4.0 (**checked**, `:66-70`, `:94-97`):** `TlsHandshakeError { source: Box::new(e), expected_host: params.host_name }`.
  - The variant lost `cert_sans` and its `native_tls::Error` source (0.3.0 `error/mod.rs:181-191`). It displays as `TLS handshake failed for '{expected_host}': {source}` (`:205-212`).
  - The inner text is unchanged (`win_tls/handshake.rs:386-389`, `errors.rs:44-51`): `InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)`, or `AcquireCredentialsHandleW(UNISP_NAME) failed: ...` (`cred.rs:296`).
  - Both variants are non-transient (0.3.0 `error/mod.rs:421-435`, 0.4.0 `:492-506`), so the provider still stops at the first one.
- **sqlh's hints and exit code are unchanged.**
  - `connect_error` now takes its `TlsHandshakeError` arm (`src/db/mod.rs:1680-1689`) instead of the catch-all (`:1709`).
  - `tls_hint` (`:1746-1782`) still finds the `SEC_E_` and `CERT_E_` names. Its fallback needle `TLS` (`:1772`) matches the new prefix where `Schannel` matched before, so it still overrides the arm's own hint.
  - `certificate` (`:1714-1722`), R46's strict hint (`:1725-1737`) and exit 2 are unchanged.
- **Only the detail sqlh prints changes:**
  - before: `Unexpected Implementation Error: Schannel TLS handshake failed: InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)`
  - after: `TLS handshake failed for 'db01': InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)`

  A credential-acquisition failure also reads "TLS handshake failed for ...", though no handshake was attempted, and it gets the generic TLS hint (since 2026-11-09 a hint of its own, naming the Windows the driver's TLS needs: DESIGN.md §6, upstream-issues.md U12). Which name `params.host_name` holds (the `-S` host, or a `host_name_in_cert`) was not checked; live check 4 shows it.
- **Stale:**
  - the comment at `src/db/mod.rs:1711-1712` ("The direct Schannel path reports TLS failures as generic errors carrying SSPI status names");
  - the fixtures in `tls_hints_from_schannel_status` (`:2449-2450`) and `a_certificate_failure_under_strict_encryption_offers_no_trust` (`:2519-2523`). They still build the 0.3.0 shape, so they pass without testing the real one;
  - docs/notes/m0-driver-spike.md:44.

  No golden or process test pins this text. mcp-mode.md §4's row (`:246`) stays accurate.

### 1.4 SQL Browser: the reply's port, and one deadline

- **A reply must now come from the asked IP and port 1434.** 0.3.0 matched the IP only (`ssrp.rs:326-327`, `:388`); 0.4.0 matches both (`:548-551`, `:617`). A reply whose source port was rewritten (NAT, a load balancer) is now dropped, and the lookup ends as `SQL Server Browser did not respond within ...` [inferred rare].
- **One deadline now covers bind, send and receive** (`:504-526`, `:627-637`). 0.3.0 timed only the receive (`:399-411`). The no-answer text now reports the time left at the receive (`waited`, `:626-631`), where 0.3.0 printed the configured timeout. So the figure can read slightly under it, 999 ms for example (**checked**, both versions).
- **Same texts, new type.** Failures are a new public `SsrpLookupError` (`:68-170`) whose texts are 0.3.0's word for word. They map back to `ConnectionError` (`:172-179`) and are wrapped, as before, in `Error Locating Server/Instance Specified [...]` (provider `:670-678`; 0.3.0 `:459-469`).
- **No new bound on a normal connect.** Resolution isn't bounded by the Browser timeout (`bound_resolution = diagnosis.is_some()`, provider `:432`, `:436`), and `TimedOut` appears only in a diagnosis. sqlh's `tcp:host\INST` connects get the same texts and hint (`src/db/mod.rs:1705-1708`).
- `SSRP_PORT`, `DEFAULT_SSRP_TIMEOUT_MS` and `build_transport_list` became `pub(crate)`. sqlh uses none of them.

### 1.5 Smaller changes

- **The SSPI loopback flag is now the dialled endpoint's.**
  - `is_loopback` allows one retry with an empty SPN when the client's own `InitializeSecurityContextW` fails (`sspi_context.rs:301-340`).
  - 0.3.0 computed it from the parsed data source (0.3.0 `client_context.rs:727-755`), which a redirect keeps. 0.4.0 computes it per dialled endpoint (`client_context.rs:725-743`; `TransportContext::is_loopback`, `:1040-1060`; `handler_factory.rs:781`; test `:1323-1334`).
  - Without routing the two agree. They differ only for a redirect that crosses between loopback and a remote host: `-E -K ReadOnly -S localhost`, routed to a remote replica, retried with an empty SPN on 0.3.0 and fails on 0.4.0.
  - Recorded, no change. It sits beside R56's routed Windows-auth wording (DESIGN.md §8).
- **LocalDB.**
  - 0.3.0 always started the instance synchronously on the runtime thread (`localdb.rs:387-389`, `:441-455`).
  - 0.4.0 first reads a running instance's pipe on the calling thread (`LocalDBGetInstanceInfo`, `:479-494`; the struct is now laid out as in msodbcsql.h, size asserted at `:72`). It starts a stopped instance on `off_runtime_thread` (`:514-541`).
  - So Ctrl+C and `sqlh mcp`'s other calls stay live while an instance starts. `-l` still doesn't bound the start, because it runs before the deadline (provider `:473-475`, `:323-330`).
  - The function types changed from `"system"` to `"C"` (`:101`, `:110`), which is the same ABI on x64. sqlh passes `(localdb)\name` as written (`src/db/mod.rs:1396-1400`).
- **`lpc:host\INST` no longer asks SQL Browser** (`datasource_parser.rs:703-706`). 0.3.0's `_` arm (`:801-804`) asked Browser and then connected over TCP. This is local only, and sqlh passes `lpc:` as written.
- **Pipe paths match `\PIPE\` and `MSSQL$` in any case** (`:292-315`). `\\db01\PIPE\sql\query` now parses, where 0.3.0 failed with `Invalid named pipe path`. sqlh refuses it either way.
- **An empty address list's text** changed from `No addresses provided for sequential connection` (0.3.0 `network_transport.rs:410-413`) to `Failed to connect to {host}:{port}` (`:738-743`).
- **`connection::this_computer_name()` is now public** (`connection.rs:33-37`).
  - It returns the name the driver compares a host with to decide what is local: `hostname::get()`, which is `GetComputerNameExW(ComputerNamePhysicalDnsHostname)` (`datasource_parser.rs:621-625`, private in 0.3.0 at `:616-620`; hostname-0.4.2 `src/windows/mod.rs:11`, `:20`). The driver uses it in `is_local` and `pipe_server` (`:641-661`).
  - The driver's behaviour is 0.3.0's; only the accessor is new.
  - sqlh's `check_server` uses the `COMPUTERNAME` environment variable instead (`src/db/mod.rs:1553-1555`, `:1567-1572`). That is the NetBIOS name (at most 15 characters), and a parent process can set it. Three cases follow:
    - if `COMPUTERNAME` names another host, sqlh treats `np:<that host>` as local while the driver opens `\\host\pipe\...` (U1);
    - if the DNS name differs from it, `np:<NetBIOS name>` goes through SMB to this machine, which is reachable [inferred];
    - `np:<long DNS name>` is refused.

  §7.4.

### 1.6 Unchanged at sqlh's settings

- **The sequential TCP walk (**checked**).**
  - It was restructured: 0.3.0 `network_transport.rs:346-433` became 0.4.0 `:518-584` (`resolve_sequential`) and `:629-749` (`connect_in_order`, `connect_in_order_with`).
  - It does the same thing. It resolves with `dns_cache::lookup_host`, then `tokio::net::lookup_host` on `spawn_blocking`. It tries each address once, in IP-preference order, each within `connect_timeout_ms` and with the same text, `Connection to {} timed out after {}ms` (0.3.0 `:416-427`, 0.4.0 `:689-713`). Failures are `Error::Io`, and the last one is returned.
  - On Windows every connect error maps to `Unreachable` (`:640-650`); `Rejected` comes only from macOS's engine.
  - A socket-setup error still ends the connect at once (0.3.0 `:415`, 0.4.0 `:685-686`), and keepalive and nodelay are set as before (`:432-453`).
  - `create_client` passes no diagnosis (provider `:222`), so the new diagnostic branches never run.
- **sqlh's settings:** `connect_retry_count` 0, `connect_timeout` `-l`, `login_timeout` `-l` + 1 s, `multi_subnet_failover` `Some(-M)`, and all four `EncryptionOptions` fields (`src/db/mod.rs:701-739`). With these, connect-failures.md's table, DESIGN.md §4.2's `-l` row, `Timeout while connecting` (exit 4) and `connect_error`'s `-M` match (`:1632-1646`) all still apply.
- **`-M`:** `multi_subnet_failover.rs` is byte-identical, and the texts are 0.3.0's (`parallel_connect.rs` 0.3.0 `:165`, `:234`, `:241`, `:348` → 0.4.0 `:215`, `:287`, `:294`, `:405`).
- **`ClientContext`.**
  - `default()` is `new()` (`client_context.rs:799-803`). Its diff is the new `security_context_provider` (None, `:599`, `:666`), the vector default (§3.1), `requested_data_classification_version` made public (§3.2) and the per-hop loopback flag (§1.5).
  - Session recovery is still advertised only when `connect_retry_count` > 0 (0.3.0 `login.rs:388`, `:420`; 0.4.0 `:392`, `:424`), and the count still defaults to 1 (`client_context.rs:633`). So sqlh's explicit 0 (`src/db/mod.rs:717`) and decision 41's guard (`:759-763`) are still needed.
  - `EncryptionOptions::new()` is Strict in both (0.3.0 `core.rs:357-364`, 0.4.0 `:358-366`), and upstream now documents it (README.md:55-59, connection-behavior.md:8-12). sqlh sets every field.
- **Routing (U8).**
  - `follow_redirects` (0.3.0 provider `:526-580`, 0.4.0 `:748-800`) gains only the diagnosis parameter. Each hop gets the same context, password included, with the remaining deadline, up to 10 hops (`:43`).
  - LOGIN7 still always advertises 0x0B and 0x0F (`login.rs:203-216`); its only other change is a `negotiated_version` method with a default body (`:133-135`).
- **The SPN for a named instance and a routed hop.**
  - It is still built from the dialled host and port (0.3.0 `handler_factory.rs:728-736`, 0.4.0 `:796-806`), and a fixed `server_spn` still goes to every hop (`client_context.rs:736`). R52's reasoning holds for every spelling except `tcp:<IPv6>` (§1.2).
  - The new `target_spn` (`imported_name()`, which equals `spn()` on Windows, `security_context.rs:215-220`) is only recorded on the login span.
- **The DNS cache (U7):** `dns_cache.rs` gains only a callback that reports a fallback (`:90-119`, `:159-168`). The module is still crate-private (`connection.rs:22`), and there is still no option to turn it off. Decision 42 stands.
- **U1:** `named_pipes.rs` is byte-identical. The first open is still synchronous (`:87-89`); only the busy-pipe wait runs on `spawn_blocking` (`:124-129`). The new `open_pipe_stage` (`network_transport.rs:362-382`) only adds a span.

## 2. Session lifetime and the engine

### 2.1 The driver's request timeout charges the send against the response, and cuts a stalled send

- **0.3.0:**
  - The packet writer checked its budget only after a packet's write had returned, in whole seconds and strictly greater (`packet_writer.rs:542-550`).
  - `send_query_batch` never took the send's time out of the response's budget, so a request could take up to about twice `timeout_secs`.
- **0.4.0 (**checked**):**
  - `send_query_batch` starts the request clock before sending and deducts the send's time from the response's budget (`tds_client.rs:1872-1883`).
  - The writer refuses to start once the deadline has passed (`packet_writer.rs:509-517`). It races each packet's write against the deadline (`:534-542` with a cancel handle, `:560-567` without) and checks `Instant::now() >= deadline` afterwards (`:591-596`). Its clock is now tokio's (`:10-12`).
  - A deadline hit mid-packet returns `TimeoutError("Timeout expired")` with the send incomplete. `retract_partial_request` then calls `retire_without_writing` (`tds_client.rs:3010-3013`), so the connection is dead.
  - A final packet that completes past the deadline takes the 0.3.0 path, attention plus 5 s (`:3023-3046`), up to 1 s sooner than before.
  - A non-final packet that completes past the deadline is withdrawn (EOM|IGNORE, `:3049`, `:3066-3145`; U5). Only a multi-packet request can reach this.
  - A timeout of 0 still means no deadline (`deduct_timeout`, `:1727-1729`; `packet_writer.rs:264-265`).
  - Upstream says the same (client-guide.md:34-36, :55-61; connection-behavior.md:102-105).
- **sqlh's batches are unaffected.** `run_batch` passes `timeout_secs(0)` (`src/db/mod.rs:878-881`), and `-t` stays sqlh's own deadline.
- **Only `exec_internal` (`:1244-1262`) passes the driver's timeout.** Its callers:
  - the session setup and a call's `USE` (`src/run.rs:1041`, `:1058`);
  - the engine's 5 s probe on a reused connection's setup (`src/engine.rs:237-255`, `:458`);
  - `SET NOEXEC ON` and `OFF`, and the wrapper's statements;
  - `put_back`'s `SET NOEXEC OFF;` and rollback.

  Their sends are tiny, so a cut mid-write is unlikely [inferred]. If one happens:
  - the connection is dead, so the probe's re-setup branch is skipped (`!session.connection_dead()`, `engine.rs:246`), `dead_before_work` holds (`:467-470`), and the call gets decision 19's one silent retry;
  - on the CLI it is exit 4.

  In 0.3.0, such a stalled write was bounded only by a cancel. The exception was `put_back`, whose writes already ran inside `ROLLBACK_WAIT` (`engine.rs:359-362`). The probe's worst case falls from about 10 s to 5 s. A deadline that passes while waiting for the reply is handled as before: attention, 5 s, and the connection kept if the server acknowledges.
- **Stale:** DESIGN.md §4.4 (`:211`, "it adds up only the time its transport reads wait") and `src/db/mod.rs:444-447`. The wording at `:846-848` ("the time spent waiting for the server") still holds. So does the reason sqlh keeps its own `-t`: buffered rows and sqlh's own work never count against the driver's clock.

### 2.2 Retirement is stricter

- **A retired transport is now dropped without TLS shutdown (**checked**).**
  - `retire_without_writing` now also turns off session recovery for a send that was still live, and calls `abort_transport()`. That drops the stream with no flush and no close_notify (`tds_client.rs:3207-3216`; `network_transport.rs:1686-1693`).
  - Its 0.3.0 body (`:3069-3073`) is now `mark_failed_request_dead` (`:3218-3222`), which the budgeted drain uses, so that path behaves as before.
  - The new behaviour is reached from the incomplete-send branch (`:3010-3013`), an abandoned write (`:2991`, `:3059`) and bulk copy (`:3706`).
  - It does reach sqlh's user batches. A Drain cancel (`-t`, Ctrl+C) mid-packet takes the arm with no budget and no deadline (`run_until_cancelled`, `packet_writer.rs:568-570`), and the stream is now dropped at once rather than with the `Session`.
  - sqlh's classification is unchanged, since `is_connection_dead()` is true either way. `Session::close` on an aborted transport does nothing (`close_transport`, `network_transport.rs:1673-1684`).
- **An unreadable token now retires the connection in Abort mode too.**
  - `fail_read` now abandons the statement on an unknown token type, a token without a parser, or a rejected DATACLASSIFICATION (`tds_client.rs:8849-8874`; `leaves_a_token_unread`, `:8852-8856`; `token/tokens.rs:83-101`).
  - 0.3.0's `fail_read` (`:8565-8568`) did so only under Continue, where `end_walk_on_read_error` abandoned on any error that wasn't a SQL error. Under Abort it returned the error unchanged (`:8581-8588`).
  - sqlh's user batches run under Continue (`src/db/mod.rs:881`), so they see no change.
  - `exec_internal` runs under Abort, and calls `close_query` only when `execute` returns Ok (`:1256-1259`). In 0.3.0 such an error left the connection marked alive: `put_back` (`engine.rs:343`) could pool a desynchronized session, and a failing reused setup or `USE` didn't count as `dead_before_work`. In 0.4.0 the connection is retired and the call is retried.
  - This works in sqlh's favour, and no 2016-2025 server sends such a token in normal traffic [inferred].
  - The synchronous `try_*` row reads (`:7323` and the others) still retire only under Continue; the cursor region diffs empty, and sqlh uses them only in Continue batches.

### 2.3 Unchanged

- **Cancel and attention.**
  - `cancel_query` (0.3.0 `tds_client.rs:8866`, 0.4.0 `:9181`), `send_attention_with_timeout` and `is_connection_dead` have identical bodies.
  - `ATTENTION_TIMEOUT_SECONDS` is still 5 (`bulk_copy_state.rs:28`, an unchanged file), and `CANCEL_TIMEOUT_SECS` still 120 (0.3.0 `:137`, 0.4.0 `:140`).
  - `read_to_attention_boundary`, `send_attention_and_wait` and `wait_for_attention_ack` (`network_transport.rs:1327`, `:2175`, `:2230`) fall outside every hunk.
  - `withdraw_sent_message` still drains with the cancel taken out, for 120 s when no budget is set (0.3.0 `:2976-2980`, 0.4.0 `:3119-3123`).

  So everything sqlh builds on this still holds: `stopped_by_budget`, the stopped-count arms, `classify`'s three cancel wordings and `COUNT_STOP_UNCONFIRMED` (`src/db/mod.rs:1020-1022`, `:1108-1125`, `:1346-1364`), the engine's probe and `dead_before_work`, and `settle_within`'s `-t` + 5 s (`:390-408`, `:852-864`). sqlh calls none of `set_cancel_timeout_secs`, `cancel_query` or `abandon_query`.
- **The reset and reuse.**
  - `prepare_reset_connection`, `on_reset_connection_ack`, `observe_response_token`, `check_and_reconnect`, `has_active_transaction`, `is_session_recovery_enabled`, `drain_rows` and `retire_after_failed_drain` have identical bodies.
  - `close_query` only switches to `clear_return_values` and changes a comment; `advance` only clears the new classification. Decisions 14, 15, 17 and 19 rest on unchanged code.
  - The transaction requests now share `execute_transaction_request` (`:9473-9536`). sqlh rolls back with T-SQL (`engine.rs:350-355`), so that code never runs for it.
- **Signatures:** every `TdsClient` method sqlh calls (Summary), and `CancelHandle`'s API (`core.rs:40-109`; one doc comment changed, `:62-64`).

### 2.4 New and not adopted

- **`abandon_query(timeout_secs)`** (`tds_client.rs:9199-9229`) discards the unread tail's rows, messages and statement errors. It skips the attention when the response has fully arrived, and maps a `SqlServerError` on a live connection to Ok (`:9220-9227`). sqlh reports every error a batch raises and counts hidden rows, so it doesn't fit. It gives no receipt either (U4).
- **`commit_transaction_with_options` and `rollback_transaction_with_options`** (`:9529`, `:9568`): sqlh sends its transaction statements as SQL.
- **`has_prepared_batch` and `quietly_discarded_parameter_set`** (`:2119-2142`): RPC only.

## 3. Data path and server messages

### 3.1 LOGIN7 asks for vector feature version 2

- **The default moved (**checked**).**
  - 0.3.0 defaulted to `VectorVersion::V1` (`client_context.rs:598`, `:665`, with a TODO to move it). 0.4.0 defaults to `V2` (`:605`, `:672`), documented at `:386` and in client-guide.md:65-70.
  - V1 is float32 only; V2 adds float16 (`:86-93`).
  - LOGIN7's 0x0E feature carries the version as one byte (`vectorfeature.rs:40-48`, `:74-88`; `login.rs:217-219`), so only that byte changes, from 01 to 02. `VectorVersion::Off` sends no vector feature (`vectorfeature.rs:43`).
  - The acknowledged version is read only by checks on the send side (`handler_factory.rs:142-148`; `tds_client.rs:2005`, `:3579`, `:6517`).
  - Decoding is unchanged: `decoder.rs` is byte-identical, and `sql_vector.rs`'s `try_from_raw` is unchanged (float16 is widened with `half::f16::to_f32`; `half` is 2.7.1 in both).
- **sqlh inherits it.** `Session::connect` starts from `ClientContext::default()` (`src/db/mod.rs:677`) and never sets `vector_version`.
- **Effect.**
  - float32 vectors arrive natively under both V1 and V2, so they don't change. SQL Server 2016 has no vector type.
  - On SQL Server 2025, a `vector(n, float16)` column (a preview feature [inferred]) would arrive natively once the server acknowledges V2. A V1 client got the server's down-level form, probably JSON text [inferred: server behaviour, not in the driver source].
  - Then the header type may change to `vector` (`src/db/types.rs:92`), and each element is printed by `real()` from the widened f32 (`value.rs:103-116`, `:128-134`). 0.1 stored as float16 would print as `0.099975586` [inferred arithmetic]. That is more digits than DESIGN.md §5.1's shortest round-trip form needs at the element's own precision.
  - DESIGN.md §5.1's vector row (`:340`) already describes sqlh's own rendering of float32.
- **Nothing in sqlh would notice the change.** The LOGIN7 fake-server test reads feature ids only (`login_features`, `src/db/mod.rs:3698-3720`; test `:3806-3826`), and no test has a vector column: tests/cli.rs has only the `VECTOR_DISTANCE` and `USE MODEL` refusal tests.
- **§7.2.** Whatever version is chosen, step 2 sets it explicitly in `Session::connect`, as decision 40 did for failover, and pins the 0x0E byte in that test.

### 3.2 Data classification: a public opt-in, never requested

- **0.4.0 makes it public (**checked**).**
  - `requested_data_classification_version` is now `pub` (`client_context.rs:420-427`), still `None` by default (`:612`, `:679`). In 0.3.0 it was `pub(crate)`, with a test-only setter (`:416-419`, `:685-693`).
  - `token::data_classification` is a public module (`token.rs:6-7`).
  - `negotiated_data_classification_version()` replaces a test-only accessor (`tds_client.rs:1517`; 0.3.0 `:4979`).
  - `ResultSet::data_classification()` is a new method with a default body (`:10105`).
  - LOGIN7 adds feature 0x09 only when the version is set (`login.rs:228-233`).
  - The parser now replaces unpaired surrogates instead of rejecting them, and allows 16 MiB instead of 256 KiB (`parsers/data_classification_parser.rs:51`, `:216-223`; 0.3.0 `:38`, `:185`).
- **sqlh never sets it, so LOGIN7 and the token stream are unchanged.**
  - An unrequested 0xA3 token is still a protocol error (0.3.0 `:332-334`, 0.4.0 `:381-385`).
  - The look-ahead after each COLMETADATA (`tds_client.rs:6153-6206`) is a branch that never suspends (`any_transport.rs:83-92`; `network_transport.rs:3343-3352`).
  - A column with `ADD SENSITIVITY CLASSIFICATION` reaches sqlh as it did in 0.3.0.
- **Step 2** adds "0x09 absent" to the LOGIN7 test, so a later driver that requests it by default shows up. Other clients do: msodbcsql asks for v1, SqlClient and JDBC for v2 (data-classification.md:148-166).
- **As a feature:** §6.5.

### 3.3 The batch's UTF-16 encoder was rewritten

- **What changed.** `write_string_unicode_async` was a per-unit `encode_utf16` loop (0.3.0 `packet_writer.rs:694-745`). It is now `:740-781` with `Utf16Units` (`:857-970`), which widens ASCII runs in bulk, stages up to 256 units, and keeps a supplementary character's low surrogate pending across a packet boundary (`:944-951`).
- **Every SQL batch's text goes through it** (`message/batch.rs:55`, an unchanged file), and so do LOGIN7's strings (`login.rs:1133-1177`).
- **Same output.** Read side by side, both versions give the same bytes and the same flush points:
  - a text that ends inside a packet;
  - a text that ends exactly on a full packet (a flush, a pass of 0 units, then return, in both);
  - the 1-byte remainder (`:753-765`; 0.3.0 `:706-716`).

  Upstream adds tests of equality with `encode_utf16`.
- **Why sqlh adds its own test.** A regression here would change queries silently, and short ASCII tests wouldn't see it. Step 2 adds a fake-server test, plus live check 9.
- The driver requests 8000-byte packets (`client_context.rs:588`, `:655`) and sqlh sets no size; that the server grants 8000 is [inferred].

### 3.4 Unchanged

- **Values (**checked**).**
  - `decoder.rs`, `column_values.rs`, `row_writer.rs`, `sqldatatypes.rs`, `query/metadata.rs` and `lcid_encoding.rs` are byte-identical. So text, json and csv output are 0.3.0's for every type, and an empty `image` is still `0x`.
  - `SqlString::decode` is the same code (0.3.0 `sql_string.rs:342-377`, 0.4.0 `:371-406`). The new narrow-string byte limit is for sending only (`:43-58`, `:332-343`).
  - `SqlCollation`'s fields are unchanged (`tokens.rs:364-374`); it gains `code_page()` (`:439`).
  - connection-behavior.md:236-247's BOM rule is 0.3.0's README text.
- **PLP and the cursor.**
  - `tds_client.rs` 0.3.0 `:7000-8200` and 0.4.0 `:7288-8488` diff empty. That range covers `read_active_plp_chunk`, `next_row_into`, `try_next_row_cursor`, `next_row_cursor`, `try_read_row_column`, `try_read_row_plp_complete` and `read_row_column`.
  - `PlpChunk`, `CursorColumn` and `CursorPoll` are at the same lines (`:308`, `:327`, `:357`).
  - `src/db/stream.rs`'s plan and the memory bounds rest on unchanged code.
- **Messages and row counts.** `take_info_messages`, `record_error_token`, `continues_after` and `apply_result_boundary` have identical bodies, and no hunk in `advance_to_result_boundary_inner` touches its INFO, ERROR or DONE arms. Where PRINT and RAISERROR land (plan-history.md 2.12) is unchanged.
- **U10 and U11.**
  - U10's guard, `loop_count > 10000`, is at 0.3.0 `:5726-5734` and 0.4.0 `:5898-5906` (the RPC twin is at `:6266`). It counts every token read since the last result boundary, and is checked at a DONE with MORE (`:5728-5739`).
  - U11's `MAX_ERRORS_PER_FAILED_STATEMENT = 1_000` is at 0.3.0 `:72` and 0.4.0 `:75`, checked at `:5968` and `:8641`. `abandon_failed_statement` (`:8833-8850`, an identical body) still drops what it collected.
- **Changes on the send side only, which sqlh can't reach:** `SqlType::Image` and legacy LOB RPC framing (`sqltypes.rs`), the parameter serializers, TVP cell validation, bulk copy, `vector(N, float16)` in bulk-copy DDL, and RPC value binding. sqlh uses no `SqlType`, RPC parameter, TVP or bulk API, and matches no enum that gained a variant.

## 4. Logging

### 4.1 The packet dumps are still there

- At DEBUG the driver still hex-dumps every sent packet before encryption, LOGIN7 and its password included (0.3.0 `io/packet_writer.rs:552-562`, 0.4.0 `:597-607`), and every received packet (0.3.0 `network_transport.rs:1474-1485`, 0.4.0 `:2034-2045`). No feature gates them.
- Decision 9 and AGENTS.md's Driver logging rule stand.
- `db::phases` never enables `mssql_tds::io::packet_writer` (`src/db/phases.rs:53-57`, test `:214-222`). It still formats received-packet dumps, which hold server replies only, into a string it throws away.

### 4.2 `db::phases` still finds its events

- All five texts it matches, in three targets (`classify`, `phases.rs:61-82`), are still there, with the same level and module path:
  - `Attempt {}: connecting with {:?}` (provider 0.3.0 `:365`, 0.4.0 `:534`);
  - `Connection successful via action chain` (`:395`, `:564`);
  - `Connected to TCP transport: {}:{}` (`network_transport.rs` 0.3.0 `:397`, 0.4.0 `:691`). It is now logged inside `connect_in_order_with` for the address that answered, still at the end of the TCP connect;
  - `Received packet of size` (`:1474-1478`, `:2034-2038`);
  - `win_tls: handshake complete` (`win_tls/handshake.rs:365`, an unchanged file).
- The `-M` path still emits no TCP event.
- tracing keeps its `log` feature (`Cargo.toml:128-130`).
- `phases.rs:11` still says the texts are 0.3.0's.

### 4.3 The connect-stage spans carry no secret, and sqlh never sees them

- **What they are.** `connection::connect_stage` is a new public module (`connection.rs:16`; `connect_stage.rs:23-153`). It defines INFO spans with target `mssql_tds::connect`, named `dns`, `tcp`, `instance_lookup`, `pipe`, `prelogin`, `tls` and `login`; each records `ok`.
  - Their fields hold host names, addresses, ports, pipe paths, the negotiated encryption, the SPN, `redirected`, `timed_out`, OS error codes and error Display texts. A login error's text names the login, never the password.
  - The spans are created at `network_transport.rs:367-376`, `:410-421`, `:532-541`, `:577-585` and `:795-810`; `handler_factory.rs:322-330`, `:348-357` and `:617-624`; and provider `:632`.
  - `ClientContext` has no `Debug` (`client_context.rs:253`; its `Clone` is written by hand, `:941`). No span records a context, a LOGIN7, an SSPI token or a password (**checked**; not every input to `record_error` was traced).
- **sqlh never sees them (**checked**).**
  - The driver sets no dispatcher outside its tests, and sqlh installs none, so the spans are disabled.
  - tracing's `log` bridge still sends a disabled span's records to `log` (tracing-0.1.44 `macros.rs:76-84`, `span.rs:1213-1236`). They go under the span's own target, or under `tracing::span` for a record whose fields are all empty and for a span's close (`:416`, `:1457-1475`). The bridge calls `logger.enabled()` before formatting anything (`:1331-1360`).
  - A default build sets no logger, so `log`'s level stays Off. The timing build's `enabled()` refuses both targets (`phases.rs:53-57`, `:127-130`).
  - The `record_*` helpers return early on a disabled span (`connect_stage.rs:179-224`), so no failure field ever crosses the bridge.
- **Other new events carry no credential:** skipping a DATACLASSIFICATION token while draining (`tds_client.rs:5557`), address counts, LocalDB's instance and pipe, SSRP's server, instance and timeout, and macOS-only connect lines. `tds_value_serializer.rs`'s debug line moved and still logs lengths only. The 41 `#[instrument]` attributes are byte-identical.
- **A trap for later (**checked**).** Setting any tracing dispatcher, global or scoped, sets tracing-core's `EXISTS` flag for the life of the process (tracing-core-0.1.36 `dispatcher.rs:327`, `:849`). From then on tracing sends nothing to `log` (tracing `macros.rs:3311-3313`), and `db::phases` goes silent. A tracing subscriber and the log recorder can't coexist. Step 2 adds this to AGENTS.md's Driver logging rule.
- **Upstream's docs lag their code.**
  - `connect_stage.rs:16-19` says the spans cost nothing without a subscriber, but their success records reach a `log` logger.
  - connection-behavior.md:226-234 (0.3.0's README:112-125, word for word) says no worker-thread handoff was introduced. That is true of the boxing it describes, and silent about `off_runtime_thread`.

  Neither is worth an upstream entry.

## 5. upstream-issues.md in 0.4.0

| Entry | In 0.4.0 | Action |
|---|---|---|
| U1, the synchronous first pipe open | unchanged (`named_pipes.rs:87-89`, byte-identical). Its "Related" sentence, that an IPv6 address can't take `tcp:`, is no longer true (`datasource_parser.rs:234-256`), and the same change opens `np:<IPv6>\INST` and the IPv6 UNC forms to it (§1.2) | keep the refusal and widen it; "Seen in" 0.4.0; rewrite "Related" |
| U2, a `RowWriter` can't decline a value | unchanged (`row_writer.rs` byte-identical) | keep; "Seen in" 0.4.0 |
| U3, no streaming for `text`/`ntext`/`image` | unchanged (`decoder.rs` byte-identical; `MAX_ALLOC_SIZE` 100 MB at `:181`) | keep |
| U4, no receipt for an acknowledged attention | unchanged (`cancel_query`, `:9189-9196`); `abandon_query` also maps an error on a live connection to Ok (`:9220-9227`) | keep |
| U5, a withdrawn request waits up to 120 s | unchanged for sqlh, still opt-in (`CANCEL_TIMEOUT_SECS` 120 at `:140`; `withdraw_sent_message` `:3066-3145`); a budgeted write now also stops at any request deadline (`packet_writer.rs:546-553`) | keep; `settle_within` stays |
| U6, `SqlString::decode` panics on malformed UTF-8 | unchanged (`sql_string.rs:375`, 0.3.0 `:346`) | keep; the panic net stays |
| U7, the DNS cache can't be turned off | unchanged (`connection.rs:22`, no option); a fallback is now visible to a tracing subscriber as `source=fallbackCache` (`network_transport.rs:542-551`, `:818-827`), never through the `log` bridge | keep; "Seen in" 0.4.0 |
| U8, a route can't be refused | unchanged (provider `:748-800`); for `-E` only, a `SecurityContextProvider` sees each hop before any token (§6.1); SQL logins get no hook | keep; mention the `-E` hook |
| U9, the SPN lookup blocks the runtime | **partly fixed:** on `spawn_blocking`, inside the login deadline and the cancel (`handler_factory.rs:663-666`, `:804-816`); still no bound of its own and no option to skip it, and abandoned lookups can now pile up on tokio's pool; "Also worth asking" is answered by `ClientContext::security_context_provider` (`client_context.rs:380-383`) | rewrite What and the workaround (§1.1); keep the request for a bound |
| U10, 10,000 DONE tokens before a result | unchanged (`tds_client.rs:5898`, `:6266`) | "Seen in" 0.4.0; the count is of every token since the last result boundary, checked at a DONE with MORE |
| U11, 1,001 errors before a DONE | unchanged (`:75`, `:5968`, `:8641`; text `:8969`) | "Seen in" 0.4.0 |
| Decided not to raise: the password in debug packet dumps | re-checked: still at `packet_writer.rs:597-607` | record the re-check |
| new | none | — |

The file's line 5 ("sqlh pins 0.3.0 since 2026-10-31") changes with the pin.

## 6. New features sqlh could use

Each entry says what it would replace and what it risks. None is for the bump (§7.5).

### 6.1 `SecurityContextProvider`: a per-hop SPN hook

**Built by decision 72 (2026-11-08), for 0.1.0:** `db::spn::HopSpn` (§10). What follows is step 1's analysis, kept as written.

- **What it is (**checked**).**
  - `ClientContext::security_context_provider: Option<Arc<dyn SecurityContextProvider>>` (`client_context.rs:380-383`; None at `:599` and `:666`; cloned to routed contexts at `:982`).
  - The trait's `create_context(&IntegratedAuthConfig, server, port)` (`security/security_context.rs:222-238`, exported at `security/mod.rs:40-42`) is called once per login hop, after TLS, inside `off_runtime_thread`, with the dialled server and port (`sspi_handler.rs:89-99`; `handler_factory.rs:796-808`). That is after SQL Browser has given the port, and again for each routed host.
  - `IntegratedAuthConfig` is `Clone`, with a public `server_spn` (`security_context.rs:88-113`). `create_security_context` was already public in 0.3.0 (`security/mod.rs:77-85`).
- **What it would replace.** A provider in `src/db/` could run sqlh's own bounded lookup per hop and build the driver's SPN. That would replace `host_and_spn`, the `-K ReadOnly` skip (`src/db/mod.rs:683-691`), `spn_if_lookup_is_slow`'s thread before the connect, and the double lookup.
  - It would close R52's two exclusions.
  - If it ignores `config.server_spn` (which carries a fixed SPN on to a routed hop), it would also close the residual case of a redirect sqlh didn't ask for.
  - For `-E`, it could refuse a routed host sqlh didn't name (U8). SQL logins get nothing from it.
- **Risks.**
  - The 500 ms moves inside `-l`, after TCP, prelogin and TLS, not before `-l` as DESIGN.md §4.2 promises.
  - The provider runs synchronously on the driver's pool thread. Its bound would be a thread of sqlh's with a timed wait, holding a pool thread meanwhile.
  - It must clone the config it is given and change only `server_spn`, or it drops the channel bindings (`handler_factory.rs:777-794`).
  - Past the budget, the SPN is the driver's lookup-failure fallback, not what a slow but successful lookup would give. That is the same trade sqlh makes today.
  - It is new authentication code, under decision 52.
- **Not tried:** whether a provider's context behaves like the built-in one in every path (channel bindings, loopback). Upstream's test (provider `:1617-1688`) uses a hand-made endpoint. That SQL Browser's port and a routed host reach the provider follows from the code (provider `:546`, `:772-786`; `ssrp.rs:284-310`), not from a test.

### 6.2 Connect-stage spans and `diagnose_client`

- **What it would replace.** `db::phases` matches five debug texts, which must be rechecked at every bump. The spans offer documented public names instead (`connect_stage.rs:23-153`; semantics `:1-19`). They would also give `--verbose` and `sqlh profiles test` more to report: the DNS source (U7's fallback), each address's TCP result, SQL Browser's answer, the negotiated encryption, and the SPN used (useful for `connect_error`'s Kerberos/SPN hint, `src/db/mod.rs:1698-1704`).
- **Option A: the log recorder also watches `mssql_tds::connect`.**
  - It sees creation records with values (the dns host, each tcp address and port) and the records made whatever the span's state: `ok=true`, `source=resolver`, prelogin's encryption, the login's SPN, `includes_tls` and `redirected`.
  - It doesn't see the resolved addresses (`network_transport.rs:559`), the instance lookup's addresses (provider `:652`, `:662`) or any failure.
  - The starts of prelogin, tls and login go to `tracing::span`.
- **Option B: a tracing subscriber that enables only spans of that target,** as upstream's own test does (`connect_stage.rs:289-292`).
  - It sees everything, failures included.
  - It replaces the log recorder, since the two can't coexist (§4.3).
  - It must refuse every other callsite: the packet dump, the 41 `#[instrument]` spans and every event. It should read only the booleans unless the owner decides `--verbose` may print driver text.
  - It makes tracing a direct dependency. Its size, against the log path's +270 KB, is unmeasured [inferred it could be smaller].
- **`diagnose_client`** (provider `:222-277`; `DiagnosticStop`, `:57-71`) makes the same attempt observed through the spans, and can stop after resolution or after the transport. Risks:
  - without a stop, it logs in fully, password included;
  - over sequential TCP it connects to up to 64 more addresses (`network_transport.rs:727-735`) and waits for them up to `connect_timeout` (provider `:267`, `:271`), which is 86,400 s under `-l 0` (`src/db/mod.rs:724-726`);
  - it takes no `CancelHandle`; to cancel it, drop it (`:247`);
  - it refuses a resolution stop under `-M` (`:255-265`), and skips the instance cache (`:422`).

### 6.3 Refusing a cached-address or routed login

- The dns stage records `source=fallbackCache` before any TCP connect (`network_transport.rs:542-551`; under `-M`, before the race, `:818-827`). The login stage records `redirected=true` before `follow_redirects` dials the new host (`handler_factory.rs:362-368`).
- A narrow subscriber watching those two fields could fire the connect's `CancelHandle`, which the connect races (provider `:316`; `src/db/mod.rs:750-755`). The password would then never reach a cached address or a routed host [inferred: the design is untried, and whether the cancel lands before the next connect wasn't traced].
- This would turn decisions 42 and 43 from documented risks into choices.
- Risks: a dispatcher in the default build (§4.3's trap, and exe size), and new code on the credential path.

### 6.4 `this_computer_name()` in the local-pipe test

- **What it would replace:** `COMPUTERNAME` in `check_server` (§1.5). sqlh's idea of "local" would then be the driver's, and the environment couldn't move it.
- **Risk:** the DNS host name and the NetBIOS name differ on machines whose name is longer than 15 characters [inferred; the only other cause is an edited environment]. On those machines, which `np:` names pass changes: `np:<NetBIOS name>` becomes a refusal.
- §7.4.

### 6.5 Sensitivity labels

- **What it would add.** With the version requested, `ResultSet::data_classification()` gives each result column's labels and information types, plus ranks under v2 (data-classification.md:11-50). `describe` or a result header could show these to a DBA.
- **Risks.**
  - LOGIN7 changes for every connect.
  - A malformed or over-budget token is a protocol error that retires the connection (data-classification.md:115-127).
  - The token is parsed whole, up to 16 MiB, against sqlh's peak memory of about 10 MB.
  - It needs SQL Server 2019 or later [inferred].
  - It changes the output contract (DESIGN.md §5).
- **Alternative:** the catalog view `sys.sensitivity_classifications` (2019 and later [inferred]) needs no driver feature.

### 6.6 Small ones

- **`SqlCollation::code_page()`** (`tokens.rs:439`), so `info` or `describe` could show a column's code page.
- **A `vector(N)` or `vector(N, float16)` header type,** the spelling bulk copy now uses (`datatypes/bulk_copy_metadata.rs:705-714`), where sqlh prints `vector` (`src/db/types.rs:92`).
  - Only with V2, or if richer headers are wanted. It is an output-contract change.
  - N would be the type's length less the 8-byte header, divided by the element size [inferred; sqlh's reading of the TYPE_INFO wasn't traced].

Not adopted: `abandon_query` and the transaction options (§2.4).

## 7. Decisions for the owner

Numbered §7.1–§7.5, so they aren't confused with PLAN.md §6's decisions; they go to the owner one at a time, in this order, and each taken one becomes a PLAN.md decision.

1. **When to take 0.4.0.**
   - 0.1.0's gates were checked on 0.3.0 (PLAN.md §7, 2026-11-08).
   - The bump needs its own external review (decision 52 lists driver upgrades). Its live checks repeat gates already ticked: cancellation during connect, both servers.
   - No §7 gate needs 0.4.0, and the one code change it requires closes a gap the bump itself opens (§1.2).

   **Recommended:** release 0.1.0 on 0.3.0, then do step 2 on a branch.
2. **The vector feature version sqlh asks for (§3.1):** V1, as 0.3.0 did; V2, the driver's new default; or Off.

   **Recommended: V1 for the bump,** set explicitly, so the upgrade changes no output.
   - V2 can come later as its own item, with float16 rendered at its own precision, a `vector(N, float16)` header type (§6.6) and goldens.
   - Off would change float32 vectors too [inferred: down-level text].
3. **(Decided otherwise: decision 72 replaced the pre-lookup with §6.1's provider, built in §10.)** **sqlh's 500 ms SPN pre-lookup stays (§1.1).** It still saves about 5 s per connect to an address with no PTR record, and keeps a short `-l` working; only its stated reasons change.

   **Recommended;** this records what is already built. Dropping it would break DESIGN.md §4.2's promise; the better replacement is §6.1.
4. **Which name counts as "this computer" for local Named Pipes (§1.5, §6.4):** the driver's `this_computer_name()`, falling back to `COMPUTERNAME` only when it returns nothing, or `COMPUTERNAME` as now.

   **Recommended: the driver's,** in step 2 alongside the IPv6 fix. It is the same refusal and goes to the same review, and the refusal exists to match what the driver will actually open.
5. **Follow-ups, none in the bump.** Record these in PLAN.md as candidates for after 0.1.0:
   - first, the `SecurityContextProvider` (§6.1), since it closes R52's gaps;
   - the connect-stage spans for `db::phases` and `profiles test` (§6.2), option B only if it measures no larger;
   - sensitivity labels, by the catalog route (§6.5);
   - the cached-address and route refusals (§6.3), only if decisions 42 and 43 should become enforceable.

   **Recommended.**

## 8. Live checks for step 2

Read-only unless stated.

- None of these uses the shapes that faulted the 2025 server (PLAN.md §4: `VECTOR_DISTANCE ... USE MODEL`, and deep comment nesting given to `sys.dm_exec_describe_first_result_set`), and none is a sweep.
- The Windows-auth checks use `-E -S <address>`, never `--profile test`, so no stored password is sent. They run by hand, as PLAN.md §4 records for Windows authentication's connect time.

1. `scripts/check.ps1 -Live` on `test` and `test2016`; the smoke goldens; the smoke script's exit 1.
2. **connect-failures.md's table and driver-0.3.0.md §8's rows:**
   - `127.0.0.1,1` (about 2.1 s, exit 2);
   - `192.0.2.1 -l 2` (2.0 s, exit 2, `timed out after 2000ms`);
   - `localhost,1`, plain and with `-l 2`;
   - `2001:db8::5 -l 2`;
   - `np:192.0.2.1` (exit 3);
   - the `-M` rows, and a subset under `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=true` and `=garbage`;
   - Ctrl+Break during a connect, with and without `-M` (exit 6 within 2 s);
   - a failed SQL login (18456 and its hint).
3. **U9 (§1.1),** against the test server's address, which has no PTR record (connect-latency.md). (These expectations are the pre-lookup's, as run in §10 item 3; under decision 72's provider `-K ReadOnly -l 2` and `-l 15` connect in about 0.6 s, as run in §10, Decision 72.)
   - `-E -S <address> -l 1 -Q "SELECT 1"`: about 0.6 s, exit 0 (sqlh's own pre-lookup, unchanged).
   - `-E -K ReadOnly -S <address> -l 2`: exit 4 at about 3 s. On 0.3.0 it ended only after the lookup (connect-latency.md:75-76).
   - The same with `-l 15`, in a `--features timing --verbose` build: the login step takes about 5 s.
   - Ctrl+Break during it: exit 6 within 2 s, and the process gone within DESIGN.md §4.3's bound.

   The login may fail off a domain; only the timing is checked. If the owner has a `-K ReadOnly` Windows-auth profile, also check in `sqlh mcp` that a call on another profile is answered while it connects.
4. **The TLS detail (§1.3).**
   - `-E -Nm -S <test server> -Q "SELECT 1"` without `-C`, if this machine doesn't trust the server's certificate (TLS fails before LOGIN7). Expect exit 2, `TLS handshake failed for '<host>': ...` and the 0.3.0 hint. Record which host name it shows.
   - `-Ns -C`: expect the strict hint (R46).
5. **IPv6 pipe spellings (§1.2),** timed, on the 0.4.0 trial build before the fix, against an unreachable documentation address:
   - `-E -S "np:\\2001:db8::5\pipe\sql\query" -l 2`. This settles §1.2's open question: does it end at once, or after about 21 s?
   - `-E -S "np:2001:db8::5\SALES" -l 2`. No SQL Browser will answer, so this one should end at the Browser lookup.

   After the fix, both of these and `tcp:\\fe80::1\pipe\sql\query` are exit 3 at once, with the TCP spelling. If the test server has an IPv6 address, `-S tcp:<IPv6>,1433` connects.
6. **SQL Browser (§1.4),** if the owner has a named instance: `-S "<host>\<INST>"` connects, and `-S "<host>\NOPE"` gives the Browser error and the `host,port` hint.
7. **`db::phases`:** a `--features timing --verbose` connect, three runs. The tcp, prelogin, tls and login steps appear, near driver-0.3.0.md §8's figures.
8. **Vectors (§3.1),** on 2025:
   - `SELECT CAST('[0.1,2,3]' AS vector(3)) AS v` in text, json and csv, on the 0.3.0 exe and on the 0.4.0 build with §7.2's version. The output should be identical. On 2016 both builds fail the same way.
   - float16 only in a database that already allows it. Enabling it is a database-scoped change outside decision 21, so it is the owner's call.
   - Read the error log for `SQLDump` lines before and after, as PLAN.md §4 asks of testers.
9. **The UTF-16 encoder (§3.3),** on both servers, from a UTF-8 file with `-i`:
   - `SELECT DATALENGTH(N'<k ASCII letters><3,000 U+1F600>') AS n, HASHBYTES('SHA2_256', N'...') AS h`, for k = 0 and k = 1, so that in one of them a surrogate pair falls across a packet boundary.
   - Compare both values with ones computed locally from the UTF-16 LE bytes.
10. **The engine and cancellation:**
    - `live::mcp_tools`, including the probe and `a_pooled_connection_killed_between_calls_is_replaced_by_the_engines_retry`;
    - `db::spike::spike_reset`, all seven sections;
    - the `-t 1` timing, lock-timeout, deadlock and Ctrl+Break tests;
    - `a_timeout_during_the_count_reports_a_bound_not_exit_4`;
    - harness kill.
11. **U10 and U11,** reproduced as upstream-issues.md records them: U10 at 2,000 and 5,000 iterations on both servers, U11 at 1,000 and 1,001 statements with `--check` on 2025. Expect the recorded results.
12. **Memory and `image`:** a 1 GiB `varchar(max)` with `--max-cell 500` stays near 10 MB (`run_measured`); `live::an_empty_image_is_0x_and_only_a_null_one_is_null`.
13. **LocalDB,** only if it is installed on the workstation: `-S "(localdb)\MSSQLLocalDB"` with the instance stopped, and Ctrl+Break while it starts (exit 6 within 2 s).
14. **The exe:**
    - size against 2,999,808 bytes and the 3 MiB budget;
    - `cargo bloat --release --crates`;
    - `dumpbin /dependents`: the same ten in-box DLLs;
    - `cargo about generate`;
    - `cargo tree -e features -i windows-sys`.

## 9. Step 2 plan

On a local branch, `driver-0.4.0`, once §7's decisions are taken.

1. **The bump.**
   - `Cargo.toml` pins `mssql-tds = "=0.4.0"`.
   - `Cargo.lock` is regenerated and committed, macOS-only entries included (Summary), since `scripts/check.ps1` builds `--locked`.
   - Trial first: `cargo check --all-targets` and clippy with `-D warnings` on both feature sets, `cargo test`, and `scripts/check.ps1`. Record the exe size before any other change, so the driver's own share is known, as driver-0.3.0.md did.
2. **The refusal (§1.2), in `src/db/` only.**
   - `split_protocol` takes 0.4.0's rule: a known protocol before the first colon is a prefix, whatever follows. `looks_like_ipv6` drops out of it and stays only for `data_source`, applied to the text after any prefix.
   - `data_source` sends `tcp:` before every remote IPv6 spelling, an instance included, as it does for host names. That keeps the driver on TCP without the `,1433` workaround. `np:`, `admin:` and `lpc:` spellings pass as written and never get a port added.
   - `check()`'s suggested TCP spelling is then TCP only.
   - `host_and_spn` takes the host from after the prefix.
   - §7.4's name source, if taken.
   - The stale comments at `:1386-1392` and `:1418-1421`.
   - Tests, in the existing tables:
     - flip `("np:::1", None)` and `("tcp:2001:db8:0:0:0:0:0:5", None)` in `protocol_prefixes_are_split_like_the_driver_does`;
     - `check()` refuses `np:2001:db8::5\SALES`, `np:\\2001:db8::5\pipe\sql\query` and `tcp:\\fe80::1\pipe\sql\query`, each with the TCP spelling, and keeps `np:::1` local (after reading `pipe_server` for what the driver opens for it);
     - `data_source` gives `tcp:2001:db8::5\SALES`, and passes `admin:fe80::1` as written;
     - `host_and_spn("tcp:2001:db8::5,1500")` gives the host `2001:db8::5`;
     - a process test that `-S "np:2001:db8::5\SALES"` exits 3 before connecting;
     - if §7.4 is taken, a unit row where the two computer names differ.
3. **The vector version (§7.2).**
   - Set `context.vector_version` in `Session::connect`, with its reason in a comment.
   - The LOGIN7 fake-server test pins the 0x0E data byte, and that 0x09 is absent.
   - A live test of a float32 vector in the three formats (2025; on 2016 the expectation is that the type doesn't exist).
   - A fake-server unit test that feeds a float32 and a float16 vector row, to pin `value.rs`'s rendering.
4. **The TLS fixtures (§1.3).**
   - Both tests gain the 0.4.0 shape: `TdsError::TlsHandshakeError { source: Box::new(io::Error::other("InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)")), expected_host: "db01".into() }`.
   - Keep one `ImplementationError` case for the text fallback.
   - Update the comment at `src/db/mod.rs:1711-1712`.
5. **The UTF-16 encoder (§3.3):** a fake-server test that keeps the SQLBatch payload and compares it with `encode_utf16` of the text, with supplementary characters at each alignment around a packet boundary.
6. **Comments.**
   - `src/db/mod.rs:1439-1449` (§1.1) and `:444-447` (§2.1).
   - `:744-749`, the `run_until_cancelled` comment: it still holds in 0.4.0 (provider `:316`).
   - `phases.rs:11`.
   - Any other comment that states current behaviour as 0.3.0's (`:735`, `:1628`). Dated 0.3.0 labels stay as history.
7. **Docs.**
   - **DESIGN.md:**
     - §4.2, the `-S` row: IPv6 and `tcp:`, and the computer's name if §7.4 is taken;
     - §4.2, the `-E` row (§1.1): the driver's lookup is bounded by `-l` + 1 s, cancellable, and runs on tokio's blocking pool, outside R53's cap;
     - §4.2, the `-M` row's version labels, after live check 2;
     - §4.4 (`:211`): the driver's timeout counts its reads' waits and the request's send, in one budget;
     - §5.1's vector row, only if §7.2 picks V2;
     - §14 (`:858`): the pin.
   - **mcp-mode.md §5** (`:268`): the version, and that a setup write the driver cuts leaves the connection dead, so the call is retried (decision 19).
   - **upstream-issues.md:** line 5's pin, and each entry as §5's table says.
   - **connect-latency.md:79,** and **m0-driver-spike.md:44** (a dated line that keeps the old text as history).
   - **AGENTS.md:**
     - Status: "mssql-tds 0.3.0 is on `main`".
     - The Driver logging rule gains §4.3's trap: a tracing dispatcher silences `db::phases`, and any subscriber may enable `mssql_tds::connect` spans only, never events or other targets.
     - The Stack's TDS driver bullet names no version and stays as it is.
   - **README.md:** unchanged if the refusal fix ships with the bump, since its connect entry (`:185`) then holds again. The "Built for agents" tally changes only if §7.2 changes output.
   - **PLAN.md:** the task, §7's decisions in §6, and §7.5's candidates.
   - **This note:** a §10, "as built and measured", like driver-0.3.0.md §8.
8. **Checks:** `scripts/check.ps1 -Live` on both servers, then §8's list.
9. **External review (decision 52).** A driver upgrade goes to external review before anything is built on it.
   - The brief covers `main..driver-0.4.0` and this note: the refusal change (connect and cancellation), the vector pin (the output contract), the TLS fixtures and the documentation changes.
   - Include the record of §8's measurements.
   - Before any release, every change since the last review is covered by one.

## 10. As built and measured (step 2, 2026-11-08)

On the local branch `driver-0.4.0`. Decisions taken: §7.1 is PLAN.md decision 70 (0.1.0 ships on 0.4.0 and takes no later driver), §7.2 is decision 71 (vector version 2, set explicitly; the owner chose the driver's default over the recommended version 1). §7.3 is decision 72 (the owner chose to build §6.1's provider now, in place of keeping the pre-lookup; built below); §7.4 is decision 73 (the local-pipe check takes the driver's `this_computer_name()`, with `COMPUTERNAME` only when it has none; built in `db::this_computer`, with a unit test that the driver's name is the one used and that a differing `COMPUTERNAME` is refused; on this workstation the two names are the same, and `np:<this computer>` reaches the driver's local pipe); §7.5 is decision 74 (the connect-stage spans, sensitivity labels by the catalog, and the cached-address and route refusals are candidates after 0.1.0). Every §7 decision is taken.

**Built:**
- `1352077`: the bump alone, `Cargo.toml` and `Cargo.lock` (block2 0.6.2, objc2 0.6.5, objc2-encode 4.1.0 added, macOS only). No source change; `scripts/check.ps1` passed.
- `15a0e87`: §9 items 2, 4, 5, 6 and 7. `split_protocol` takes 0.4.0's rule and `data_source` puts `tcp:` before a remote IPv6 address, so `np:<IPv6>[\INST]` and the IPv6 UNC spellings are refused (exit 3) and an IPv6 named instance stays on TCP. The TLS fixtures take the `TlsHandshakeError` shape, with one 0.3.0 case kept. `a_batch_reaches_the_server_unit_for_unit_across_packets` covers the encoder: the fake's packets were checked at 4,096 bytes, 2,033 units of SQL in the first, so a pair is cut for an odd `k`.
- Decision 71: `Session::connect` sets `VectorVersion::V2`; `the_login_asks_for_vector_version_2_and_no_data_classification` pins the 0x0E byte at 2 and 0x09 absent, and fails with V1 set. `value::push_half` writes a float16 element in the shortest decimal that reads back as the same float16, in `{:?}`'s form, and a whole number whole. It works in integers: a first version formatted each candidate to a precision (`{:.*e}`) and parsed it back, which brought Rust's exact float formatter into the exe, +25,600 bytes; the integer one costs 4,608. Checked against every finite float16 (round trip, no shorter decimal, the form `float` gives), with `half` 2.7.1 as a dev-dependency reference (from f32 only: its `from_f64` rounds twice). Live test `a_vector_prints_its_elements_at_their_own_precision`. `native_float32_and_float16_vectors_reach_the_sink_as_sqlh_prints_them` feeds both column types and a NULL row through the driver's decoder (§9 item 3's fake-server test). Before commit, an independent review (three lenses, each finding put to a skeptic) confirmed one P3, fixed here: the docs said float16 arrives natively unconditionally, where it does so only once the server acknowledges version 2 (the driver accepts 1), and the fake-server test was missing. It refuted three: the first version's float16 rendering cost about 0.8 µs an element against 0.1 µs for float32 (measured; no output was wrong; the integer version that replaced it after the review, for size, wasn't timed), float32's existing allocation per element (unchanged code), and the DESIGN.md row's wording.

**Size (§8 item 14):** 2,999,808 bytes on 0.3.0; 3,099,648 with the bare bump; 3,099,136 at `15a0e87`; 3,103,744 with decision 71 (41,984 bytes of the 3 MiB budget left). `cargo bloat --release --crates`: mssql_tds 863.8 → 923.7 KiB, tokio 108.2 → 127.8 KiB, std 482.9 → 493.3 KiB, sqlh 623.5 → 618.0 KiB. The largest growths are the connect paths with their stage spans (`create_base_stream` +27 KiB, `handler_factory::execute` +21 KiB, `connect_with_transport_context` +18 KiB, `query_browser` +16 KiB). About 29 KiB is the data-classification parser, which sqlh never requests. Imports: the same ten in-box DLLs. `cargo about generate`: accepted, no macOS crate in the notices. windows-sys 0.59 gains `Win32_Security` and `Win32_System_Threading` only.

**Live checks (§8), against `test` (SQL Server 2025, 17.0.5035) and `test2016` (2016 SP3 GDR):**

1. `scripts/check.ps1 -Live` at `15a0e87` passed on both servers: 495 unit tests, 126 debug and 120 release process tests, 86 debug and 79 release live tests each.
2. Connect failures, each beside 0.3.0 (a `main` build) measured the same way: refused `127.0.0.1,1` 2.03 s, exit 2; unroutable `192.0.2.1 -l 2` 2.01 s with a SQL login and 2.51 s with Windows auth on both drivers (the extra 0.5 s is sqlh's own SPN pre-lookup, which postdates driver-0.3.0.md §8's 2.11 s); `localhost,1` 4.08 s exit 2, and with `-l 2` exit 4 at 3.01 s; `2001:db8::5 -l 2` 2.01 s, `timed out after 2000ms` (now sent as `tcp:2001:db8::5`); `np:192.0.2.1` exit 3. `-M`: `192.0.2.1 -l 2` exit 2 at 2.51 s with the `-M` wording; `localhost,1` exit 2 at 2.1 s, `All parallel connection attempts failed`; `db01\SALES` refused, exit 2. `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=true` and `=garbage` change nothing. A failed SQL login: error 18456 with its hint, exit 2. Ctrl+Break during a connect, with and without `-M`: the process test `ctrl_break_cancels_a_connect`.
3. U9 (§1.1), Windows auth by the 2025 server's address, which has no PTR record:
   - `-E -S <address> -l 1`: connects in 0.56 s (0.64 s on 0.3.0): sqlh's pre-lookup, unchanged.
   - `-E -K ReadOnly -S <address> -l 2`: exit 4 at 3.02 s. On 0.3.0, 5.14 s: its deadline passed during the lookup and fired only once the lookup ended.
   - `-l 15`: connects in about 5.1 s on both.
   - Ctrl+C 1.5 s into that lookup: exit 6 after 0.32 s (also at 3.5 s in). On 0.3.0, exit 6 only after 3.52 s, when the lookup ended.
   - Not tried: `sqlh mcp` with a `-K ReadOnly` Windows-auth profile (none exists).
   - A trap found on the way: Ctrl+C sent to a sqlh started from Git Bash never arrives, so the console-event checks run from PowerShell.
4. TLS (§1.3): `-E -Nm` without `-C` reads `TLS handshake failed for '<address>': InitializeSecurityContextW failed: SEC_E_UNTRUSTED_ROOT (status=0x80090325)` with the untrusted-root hint, exit 2. The name is the `-S` host. `-Ns -C` reads `TLS handshake failed for '<host>': peer closed connection during TLS handshake` with R46's strict hint. Hints and exit codes are 0.3.0's.
5. IPv6 pipes (§1.2), from PowerShell, since Git Bash collapses a leading `\\`:
   - On the bare bump, `np:\\2001:db8::5\pipe\sql\query` and `tcp:\\fe80::1\pipe\sql\query` with `-l 2` each ended after 21.0 s with `The specified network name is no longer available (os error 64)`. That is U1's stall, past `-l`, so the gap was real.
   - `np:2001:db8::5\SALES` ended at the SQL Browser lookup, 1.1 s.
   - With the fix, all three are exit 3 in 14 ms. `2001:db8::5\SALES` asks SQL Browser and ends in 1.1 s.
   - The test server has no IPv6 address, so `tcp:<IPv6>,1433` wasn't connected.
6. SQL Browser with a named instance: not run (no named instance).
7. `db::phases`, three `--features timing --verbose` connects: tcp 5–6, prelogin 2–16, tls 17–34, login 126–175 ms, totals 154–231 ms, in line with 0.3.0.
8. Vectors on 2025: `CAST('[0.1,2,-3.5,1e-8]' AS vector(4))` gives `[0.1,2,-3.5,1e-8]` in text, json and csv on 0.4.0 with version 2, as on 0.3.0. float16 is refused with error 195, `'float16' is not a recognized vector base type`, since `sqlh_test` has PREVIEW_FEATURES off; turning it on is the owner's call. On 2016: error 243, `Type vector is not a defined system type`. The error log had no `SQLDump` line before or after.
9. The encoder: from a UTF-8 file with `-i`, k = 0–3 ASCII letters, then 3,000 U+1F600, 600 ASCII and 500 × `é日本U+10FFFF`. Each server's `DATALENGTH` and `SHA2_256` matched the values computed locally from the UTF-16 LE bytes, on both servers.
10. The engine and cancellation: `live::mcp_tools` and the `-t`, lock-timeout, deadlock and count tests in item 1; `db::spike::spike_reset` passed on both servers.
11. U10: 2,000 iterations return `c`, and 5,000 fail with `Too many Done tokens with has_more=true without ColMetadata`, exit 10, on both servers. U11 (2025, `--check`): 1,000 statements give exit 1 and 1,000 errors; 1,001 give exit 10 and none. Both are as upstream-issues.md records.
12. Memory and `image`: the live tests in item 1.
13. LocalDB: not installed on the workstation; not run.
14. The exe: above.

### Decision 72: the per-hop SPN provider (2026-11-08)

The owner chose to build §6.1's hook now, in place of §7.3's recommendation to keep the pre-lookup: "build the long term replacement now. as I said I want to flow with the 0.4.0's defaults, this is in the spirit of that ask" (PLAN.md decision 72). A read-only workflow (three readers, each with an independent checker, then a design) mapped the hook, sqlh's code and the fake server's reach before any code was written.

**Still open:** real SSPI against a named instance through a real SQL Browser, an AG listener's routed hop, Kerberos on a domain, and channel bindings under TLS with a server that enforces Extended Protection; none is at hand. The fake-server tests cover the provider's calls with a mock context.

**Built:**
- `src/db/spn.rs`, `HopSpn`: the driver's `SecurityContextProvider`, set on every Windows-auth connect (`Session::connect`; `connect_with` is the tests' seam).
  - For each login hop the driver calls it with the host and port it dials. sqlh looks the host up with the driver's `canonicalize_hostname` on a thread of its own, waits at most 500 ms, and sets `server_spn`: `make_spn(<answer>, None, port)` in time, which is `make_spn_canonicalized`'s; `make_spn(<host>, None, port)` past the budget, the driver's own fallback.
  - The SPN is always set, so the driver never runs its own unbounded lookup. Nothing else in the config changes, so the TLS channel bindings, the loopback flag of the empty-SPN retry and the package are the driver's.
  - A loopback name is not looked up (the driver's public `is_loopback_address`, which compares with this machine's DNS host name); a local `\\.\pipe`, shared memory and LocalDB arrive as `localhost`, a pipe named by host under that name. The design proposed also copying the driver's private `is_fqdn` rule; not done, so a later driver can't change it unseen: a dotted name goes to the lookup thread, where `canonicalize_hostname` returns it at once, and under a full cap it can wait the 500 ms for a slot, for the SPN it would have had anyway (accepted in review).
  - R53's cap of 4, now on std's `Mutex` and `Condvar` (the provider is synchronous, on the driver's blocking thread, where a tokio timer would need the runtime thread). A slot is held until its lookup ends; the wait for one comes out of the 500 ms.
  - The provider makes no error of its own: slowness, a full cap or a failed thread give the fallback SPN. The driver's own error from creating its context passes through.
- Removed: `spn_if_lookup_is_slow`, `host_and_spn`, `outlasts`/`outlasts_within`, the tokio semaphore and the `-K ReadOnly` skip (R52), with their six tests.

**Tests:** seven unit tests in `spn.rs` (the dialled host and port in the fallback SPN; an in-time answer; loopback names not looked up; the config passing through with only the SPN set, channel bindings included; the driver's error; the cap, with a slot held until its lookup ends; a slot freed within the budget being taken) and six fake-server tests through a real connect, with a mock context's token in the LOGIN7: the dialled host and port; each routed hop's own SPN, with a read-only intent and through an acknowledged Enhanced Routing route (R52, closed); SQL Browser's port for a named instance, from a fake SQL Browser on 127.0.0.3:1434 (R52, closed); the driver's security error ending the connect before any LOGIN7; a cancel during the wait; and the wait holding no runtime thread. Real SSPI, Kerberos and channel bindings under TLS stay live checks.

**Review before commit:** an independent adversarial review (four lenses, each finding put to a skeptic; 19 agents) confirmed eight findings, none in the provider's behaviour, all fixed here:
- three tests depended on scheduling and failed under 48 busy threads on the 12-core workstation: two asserted what a lookup thread had done before anything made it run (now a bounded wait for it), and the fake SQL Browser bound the fixed 127.0.0.3:1434 and replied from a thread that could start after the driver's 1 s window (now an address of the process's own, 127.a.b.3 from the process id, a clear failure if the port is taken, and a responder already waiting before the connect); after the fixes, ten runs of all four under the same load passed;
- no test pinned the host given to the lookup (a mutation that looked up the wrong name passed all 13): the in-time test now derives its answer from the host;
- the docs: this section lacked decision 72's record, §8 item 3 still expected the pre-lookup's results, and two comments and DESIGN.md overstated the lookup rule (dotted names and pipes named by host).
It refuted six, among them the slot wait of a dotted name (a latency inside every stated bound) and that LocalDB reaches the provider as `(localdb)\name` (sqlh's connect path resolves it to a local pipe first).

**Size:** 3,098,112 bytes, 5,632 fewer than with the pre-lookup (47,616 of the 3 MiB budget left).

**Live, against both servers by address (no PTR record), from PowerShell:**
- `-E -S <address> -l 1`: connects in 0.59–0.61 s, NTLM (0.56 s with the pre-lookup).
- `-E -K ReadOnly -S <address> -l 2`: **connects in 0.56–0.61 s**; with the pre-lookup, exit 4 at 3.02 s (§10 item 3).
- `-l 15`: 0.59–0.66 s (about 5.1 s before).
- The 2016 server, by address and port: 0.54–0.68 s, NTLM, in all three.
- A timing build: the login step now carries the wait (525–620 ms, against about 160 ms for a SQL login); tcp, prelogin and tls as before.
- Ctrl+C 150, 250 and 400 ms after the start, inside the wait: exit 6 at the moment of the event (the run's own time 137–395 ms), six runs.
- `192.0.2.1 -E -l 2`, with and without `-M`: exit 2 at 2.01 s (2.51 s with the pre-lookup, which ran before TCP).
- `sqlh mcp --pool 2` with two Windows-auth profiles by address, from a scratch `SQLH_CONFIG`: two first calls at once both answered at about 0.6 s; a pooled call 13 ms; a first call cancelled 150 ms in got no answer, and the profile's next call connected afresh in 525 ms.
- `admin:<address>`: exit 4 at 3.0 s with Windows auth and with a SQL login alike, since remote DAC is off on the test server: before the provider runs, so not its doing.
- `scripts/check.ps1 -Live` passed on both servers after the review's fixes: 507 unit tests, 126 debug and 120 release process tests, 87 debug and 80 release live tests each.
- Not run: the server by DNS name (it doesn't resolve on this workstation), a named instance through a real SQL Browser, an AG listener's two hops, and Kerberos on a domain.
