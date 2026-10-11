# Connect failures and cancellation (PLAN.md 2.4)

Measured 2026-10-10 on the dev machine (Windows 11) with mssql-tds 0.2.0, using addresses that refuse (`127.0.0.1,1`), addresses that don't answer (the documentation ranges `192.0.2.0/24` and `198.51.100.0/24`), and a local listener that accepts and never answers.

## What the driver does

- **Retries.** `ClientContext` defaults to one retry after 10 s. A refused connection was retried until the deadline, so it failed after 14.2 s with `-l 15`, although one attempt takes about 2 s (Windows retries a refused connect itself).
- **Two timers.** `connect_timeout` bounds each TCP connect attempt, one per resolved address. `login_timeout` is the deadline for the whole connect. When both were `-l`, an unreachable server ended as exit 2 or exit 4, depending on which fired first. The deadline's error says only "Timeout while connecting", so a hung login and a name whose addresses all time out look the same.
- **Protocol waterfall.** For a name without a protocol, port or instance, the driver tries TCP on 1433 and then, on Windows, Named Pipes at `\\host\pipe\sql\query`. Local names get shared memory first (`connection_actions.rs`, `add_protocol_waterfall`). The pipe is opened with synchronous Win32 calls on the runtime thread (`transport/named_pipes.rs`: `ClientOptions::open`, `WaitNamedPipeW`).
  - For an unreachable remote host, SMB took about 21 s to give up. Nothing else could run meanwhile: in a probe, a task ticking every 500 ms stopped at the 2 s TCP timeout and resumed at 23.1 s. So `-l` and Ctrl+C couldn't end it.
  - The error reported was the pipe's ("The specified network name is no longer available", os error 64), not the TCP timeout. A repeat failed after 2.1 s with os error 53, because SMB had cached the failure, which made the first case look random.
- **Cancel during connect.** The driver checks its cancel token only before each attempt (`tds_connection_provider.rs`), so a pending attempt ignores a cancel until it ends.

## What sqlh does

- One attempt, no retries. Each TCP connect attempt gets `-l` seconds, and the deadline is `-l` + 1, so the timers can't race.
- A remote name gets a `tcp:` prefix (`db::data_source`). Local names keep the driver's order, which never reaches SMB, and an explicit protocol passes through. The driver itself already defaults to TCP under multi-subnet failover.
- The connect races sqlh's own cancel flag and is dropped when that is set.
- The exit-4 message says the login wasn't completed within the deadline, which is true of both causes above.

## Results

| Case | Before 2.4 | After |
|---|---|---|
| Refused (`127.0.0.1,1`, `-l 15`) | exit 2 after 14.2 s | exit 2 after about 2.1 s |
| Doesn't answer (`192.0.2.x`, `-l 2`), first try | exit 2 after 23.1 s, os error 64 | exit 2 after 2.0 s, "timed out after 2000ms" |
| Doesn't answer, repeated | exit 2 after 2.1 s, os error 53 | exit 2 after 2.0 s |
| Accepts and never answers, `-l 1` | | exit 4 after 2 s |
| Ctrl+Break during that connect, `-l 30` | ended by Windows, no status line | exit 6 in under 2 s |

## Upstream issues worth filing

Read in 0.2.0; check `main` (0.3.0) first. Upstream takes issues, not PRs.

1. **Remote Named Pipes block the async runtime.** In the protocol waterfall, the pipe open runs synchronously on the runtime thread. For an unreachable host that stalls every timer, cancellation and task for about 21 s, and the error that comes back is the pipe's instead of the TCP timeout. Possible fixes: open the pipe on a blocking thread, bound it by the connect timeout, or leave remote names on TCP unless Named Pipes are asked for.
2. **A pending connect attempt ignores its cancel token.** The token is checked between attempts only, so a cancel waits for the TCP connect or the login to time out.
