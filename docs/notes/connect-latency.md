# Connect latency (plan-history.md 2.1)

Measured 2026-10-10, on a working tree based on `061ef26`. This was a time-boxed investigation: where the time before the first batch goes, and whether sqlh or the driver can win it back.

## Summary

A one-shot `sqlh` call against the dev/test server spends about 195 ms connecting, and about 150 ms of that is the login step: the wait between sending the login packet and the server's reply. The likely cause is SQL Server 2025 hashing SQL-authentication passwords with PBKDF2, which makes each login deliberately slow, but **that is an inference** from the interval and Microsoft's documentation, not an isolated measurement. sqlh and the driver add about 5 ms. On this server the 40 ms target is out of reach for one-shot calls with SQL authentication, so fast repeated turns need connection reuse (Phase 5). One server on one LAN was measured: this supports prioritizing reuse, and is not a universal lower bound.

## Setup

- **Client:** the dev workstation, Windows 11, on Wi-Fi. A release build of sqlh with `--features timing`.
- **Server:** the owner's dev/test SQL Server 2025 (Standard Developer) on the same LAN. Ping is about 2 ms, with occasional spikes to 45 ms.
- **Connection:** SQL authentication through a profile; encryption mandatory, which is TDS 7.4 with TLS inside the prelogin exchange; the server certificate trusted (`-C`). Each run is `SELECT 1`, read-only.

## Method

The driver logs its connect steps through `tracing`, and its `log` bridge delivers those events to a logger in sqlh. `db::phases` keeps only the time of five events: a connection attempt starting, the TCP connection, the first packet received (the prelogin reply), the TLS handshake completing, and the login completing. A build with `--features timing` prints the steps in the `--verbose` line:

```text
sqlh: profile test; connected in 194 ms (tcp 5, prelogin 9, tls 29, login 150), session setup 3 ms; server …
```

As a cross-check, plain TCP connects from PowerShell to the same port took 2.5 to 4 ms; the first connect in a process took 12 ms.

## Results

Ten runs:

| Step | Median | Range | What it is |
|---|---|---|---|
| TCP connect | 5 ms | 5–7 ms | one round trip |
| Prelogin | 6 ms | 3–9 ms | one round trip |
| TLS | 26 ms | 14–88 ms | two round trips plus Schannel work, including about 2 ms to acquire credentials. The spikes are Wi-Fi. |
| Login | 150 ms | 122–159 ms | from sending the login packet to the server's reply |
| Total connect | 195 ms | 145–260 ms | |
| Session setup | 2–3 ms | 2–4 ms | the `SET` options and `BEGIN TRANSACTION`, one round trip |

Before connecting, sqlh spends about 3 ms on startup, settings and the Credential Manager read.

## Findings

1. **The login is about 77% of the connect time, and it is server-side.** The client sends the login packet as soon as TLS completes, then waits 120 to 160 ms for the reply. *(Measured.)*
   - Starting with SQL Server 2025, SQL-authentication passwords are hashed with PBKDF2 (RFC 2898): 100,000 iterations of SHA-512. *(Documented by Microsoft.)*
   - Microsoft notes that this can lengthen SQL-authentication logins, mostly where connections aren't pooled. sqlh opens a new connection on every call, so every call pays it. See [Create a login: Background](https://learn.microsoft.com/sql/relational-databases/security/authentication-access/create-a-login).
   - **Inference, not measurement:** the interval is consistent with 100,000 HMAC-SHA-512 iterations on a server CPU, so hashing is the likely main cause. Nothing here separates it from the server's other login work, so 150 ms is the whole interval, not the hashing alone.
2. **TLS is the second cost, and the most variable one.** It takes two round trips over Wi-Fi, plus Schannel's work. The driver caches Schannel credentials per process, so a one-shot CLI always misses that cache; acquiring them costs about 2 ms.
3. **TCP and prelogin take about one round trip each.**
4. **sqlh and the driver add almost nothing:** about 3 ms before connecting, 2 ms for Schannel credentials, and under 1 ms between the driver's steps.

## Conclusions

- **The 40 ms target can't be met for one-shot calls** with SQL authentication on this server. The login step alone took about 150 ms, which fits the server's documented password hashing. Over this Wi-Fi link, TCP, prelogin and TLS add about 35 ms more.
- **Nothing to fix upstream for speed.** No client-side change saves more than a few milliseconds.
- **Phase 5 is needed:** an agent turn pays about 200 ms of connecting here before its query runs. Reusing a connection (MCP, or a local daemon) pays the login once.

Not measured:
- Other servers and networks: older SQL Server versions, other hardware, a wired or remote link.
- The split of the login interval (a server-side trace would show it).
- A second login within one process.
- Strict encryption (TDS 8.0) and a real network: measured 2026-11-13 below.

## Over the internet: a disposable cloud VM (2026-11-13)

PLAN.md §7's real-network check, using the signed `0.1.0-dev.12` and a disposable cloud VM running SQL Server 2025 Enterprise Developer RTM (17.0.1000.7) on Windows Server 2025. Port 1433 was restricted to the test client; the run used SQL authentication and a self-signed certificate trusted by that client. A session setup's single round trip took about 40 ms.

| Measure | Median | Range | Runs |
|---|---|---|---|
| Connect, mandatory encryption (`--verbose`) | 425 ms | 369–504 | 20 |
| Session setup | 40 ms | 37–67 | 20 |
| TCP (timing build) | 52 ms | 46–101 | 15 |
| Prelogin | 46 ms | 40–49 | 15 |
| TLS | 91 ms | 84–96 | 15 |
| Login | 219 ms | 192–272 | 15 |

- **TCP, prelogin and TLS are about four round trips;** the login is mostly the server's SQL-login work, as on the LAN (about 150 ms there).
- **Every encryption mode connected:** optional (`encrypt_option` FALSE: the server doesn't force encryption, so only the login was encrypted), mandatory validated and with `-C`, strict (TDS 8.0, `protocol_version` 0x08000000) validated: the first strict success, since no earlier server's certificate was trusted. Before the certificate was trusted, mandatory without `-C` failed with the untrusted-root hint and strict with R46's hint, exit 2.
- **`sqlh mcp`:** the first `info` call 570 ms (connect 450), each reused call 113–125 ms; reuse saves about 450 ms a call here, against about 210 on the LAN.

## Windows authentication (2026-11-01)

Decision 50's check, mssql-tds 0.3.0, on the same server and Wi-Fi link: a workstation outside any domain, a local Windows user of the same name and password on the server, `-S <address>,1433 -E -C`. `auth_scheme` is NTLM and `SUSER_SNAME()` is the server's local user. Kerberos is unchecked (no domain here).

| Run | Connect |
|---|---|
| SQL login, by address | 246–266 ms (timing build: login 186 ms) |
| Windows auth, by address, before the fix | 4,671–5,085 ms (timing build: login 4,562–5,000 ms) |
| Windows auth, by address, with the fix | 571–725 ms, NTLM |
| Windows auth, through a local relay to the same server | 31–94 ms, NTLM |
| .NET SqlClient (Framework, pooling off), Windows auth, by address | 9,631–9,776 ms |

- **Almost all of the 5 s is a failed reverse name lookup on the client, not the login.** With no server SPN set, the driver builds `MSSQLSvc/<host>:<port>` through `make_spn_canonicalized`, whose `lookup_addr` is a blocking `getnameinfo`. For an address with no PTR record it fails after 4,533–4,997 ms here (`Dns.GetHostEntry` alone, timed), about NetBIOS's wait (inference: the adapter has NetBIOS over TCP/IP at its default). The failure's fallback is the address itself. A loopback name skips the lookup, which is why the relay's run, on `127.0.0.1`, shows the login without it. SSPI's first token takes about 1 ms, and SqlClient, which pays about twice the wait, shows sqlh isn't alone in this.
- **The call blocks the runtime it's on,** so nothing else runs until it returns (upstream-issues.md U9). Before the fix:
  - `-l 1` and `-l 3` against a healthy server: exit 4 after about 5 s, "didn't complete the login within 2 s; check that it is a reachable SQL Server and not overloaded". The deadline passes during the lookup, and the message blames the server. `-l 8` connects.
  - `sqlh mcp`: two calls at once, each needing a connection, both answered at about 9.1 s; the lookups run one after the other and hold up every call and its cancel. A call on the pooled connection afterwards: 6 ms.
- **The fix (owner's choice, 2026-11-01; DESIGN.md §4.2, `-E`):** sqlh runs the same lookup on a thread of its own and waits 500 ms. In time, the driver is left to it; past that, sqlh sets `server_spn` to what the driver's failure gives, so the driver makes no lookup. The 500 ms is an estimate, unmeasured against a real PTR answer. Measured after it, on the same server and link: `-l 1` and `-l 3` connect (576 and 612 ms, exit 0); `sqlh mcp`'s two calls at once are answered at 616 ms, the pooled one at 9 ms, and the process ends in 6 ms with its lookups still running; the SQL login is unchanged (207–240 ms). No test covers this end to end, since the fake server has no SSPI and a local address skips the lookup: the unit tests inject the lookup, and these runs were by hand. **Replaced by decision 72 (2026-11-08):** the same bounded lookup now runs for each login hop inside the driver's provider hook (`db::spn`), after TCP, prelogin and TLS, and inside the login deadline; in time the SPN is built from its answer, so the driver makes no lookup at all. The fake-server tests now reach the provider through a real connect (a token from a mock context; a routed hop; SQL Browser's port); real SSPI stays a live check (driver-0.4.0.md §10).
- **Corrected by the 0.1.0 review (R52, R53; 2026-11-07).** The fallback SPN for a named instance without a port was `MSSQLSvc/<host>:<instance>`, which the driver's Windows TCP path never builds: it gives `make_spn_canonicalized` the host and the port it dials, the instance's port coming from SQL Browser once it connects. Such a server, and a `-K ReadOnly` login, which expects a routing redirect, now get no SPN from sqlh and keep the driver's own lookup, with its stall (closed by decision 72, 2026-11-08: each now gets its hop's SPN, SQL Browser's port and the routed host's; on mssql-tds 0.4.0 that lookup no longer stalls the runtime: it runs on tokio's blocking pool, inside `-l` + 1 s and the cancel, driver-0.4.0.md §1.1). And the threads left behind by lookups past the budget had no cap; at most 4 now run at once, each holding its place until it ends.
- **The hashing inference holds up.** The same login without the password hash is 31–94 ms all told, against about 250 ms for the SQL login: most of a SQL login's connect is work a Windows login doesn't do. This still doesn't split the interval.

Not measured: a server with a PTR record (a name the lookup finds), a short name whose reverse lookup fails (the same code path), and a domain. A lookup that would succeed but takes over 500 ms now gets an SPN with the host as written, so Kerberos would need an SPN for that name; Kerberos remains unchecked.

## The driver logs packets in clear text at debug level

mssql-tds 0.2.0's debug events dump every TDS packet before encryption. That includes the login packet, whose password field is only obfuscated, and the obfuscation is reversible. The targets are `mssql_tds::io::packet_writer` and `mssql_tds::connection::transport::network_transport`. An application that turns on debug logging for the driver writes the password to its logs.

sqlh never installs a general logger. `db::phases`, which only the `timing` feature builds, watches three targets and keeps timestamps, never text. The scramble itself is required by the TDS protocol; the gap is that the dump happens before TLS encrypts the packet, and other drivers' traces usually mask the password. It's a logging-hygiene issue (CWE-532), not a remote vulnerability: nothing leaks unless an application enables debug logging for the driver. Whether to ask upstream to redact the password field was PLAN.md decision 9; the owner decided not to (2026-10-12), so sqlh's own rule against installing a logger stays the defense.

## Reproduce

```powershell
cargo build --release --features timing
target\release\sqlh.exe --profile test --verbose -Q "SELECT 1"    # repeat a few times
target\release\sqlh.exe profile test test                         # the same steps, as a note
```

Default builds leave the feature out. Listening to the driver's logging keeps all of its log formatting in the exe, which adds 270 KB (about 12%), so a default build's `--verbose` line reports only the connect total and the session setup.

**mssql-tds 0.3.0 (2026-10-31):** three runs with `--features timing --verbose` gave tcp 6–41 ms, prelogin 4, TLS 23–74, login 162–171, and 197–292 ms in all: the same shape: the login is still the largest step (162–171 ms, against 150 ms on 0.2.0), and the rest of the spread was the Wi-Fi link's TCP connect (up to 41 ms) and TLS (up to 74 ms). `db::phases` still finds every event it times ([driver-0.3.0.md](driver-0.3.0.md) §8).
