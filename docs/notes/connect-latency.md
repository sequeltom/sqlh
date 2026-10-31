# Connect latency (PLAN.md 2.1)

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
- Windows authentication, which skips the password hash but isn't available on this setup. It would also test the inference: a much shorter login step on the same server would point at the hashing.
- Other servers and networks: older SQL Server versions, other hardware, a wired or remote link.
- The split of the login interval (a server-side trace would show it).
- Strict encryption (TDS 8.0), and a second login within one process.
- The release target, SQL Server 2025 Standard over a real network (checklist in PLAN.md §7).

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

**mssql-tds 0.3.0 (2026-10-31):** three runs with `--features timing --verbose` gave tcp 6–41 ms, prelogin 4, TLS 23–74, login 162–171, and 197–292 ms in all: the same shape, the login still about 80% of the connect. `db::phases` still finds every event it times ([driver-0.3.0.md](driver-0.3.0.md) §8).
