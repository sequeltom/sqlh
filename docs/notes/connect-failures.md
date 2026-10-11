# Connect failures and cancellation (PLAN.md 2.4)

Measured 2026-10-10 on the dev machine (Windows 11) with mssql-tds 0.2.0, using addresses that refuse (`127.0.0.1,1`), addresses that don't answer (the documentation ranges `192.0.2.0/24` and `198.51.100.0/24`), and a local listener that accepts and never answers.

## What the driver does

- **Retries.** `ClientContext` defaults to one retry after 10 s. A refused connection was retried until the deadline, so it failed after 14.2 s with `-l 15`, although one attempt takes about 2 s (Windows retries a refused connect itself).
- **Two timers.** `connect_timeout` bounds each TCP connect attempt, one per resolved address. `login_timeout` is the deadline for the whole connect. When both were `-l`, an unreachable server ended as exit 2 or exit 4, depending on which fired first. The deadline's error says only "Timeout while connecting", so a hung login and a name whose addresses all time out look the same.
- **Protocol waterfall.** For a name without a protocol, port or instance, the driver tries TCP on 1433 and then, on Windows, Named Pipes at `\\host\pipe\sql\query`. Local names get shared memory first (`connection_actions.rs`, `add_protocol_waterfall`). An `np:` prefix or a UNC pipe path goes straight to the pipe, and a UNC path is a pipe even after `tcp:` (`datasource_parser.rs`, `parse_named_pipe`).
- **What is synchronous.** `transport/named_pipes.rs`, `open_named_pipe_with_retry`, opens the pipe with tokio's `ClientOptions::open`, which is a plain `CreateFileW`. **That first open runs synchronously on the runtime thread**, in the async fn itself, for every route to a pipe: the waterfall, `np:` and a UNC path. Only the wait after `ERROR_PIPE_BUSY` is different: `WaitNamedPipeW` runs on `spawn_blocking` (bounded by `NAMED_PIPE_OPEN_TIMEOUT_MS`, 5 s), so it doesn't block the runtime. The freeze below comes from the first open.
  - For an unreachable remote host, SMB took about 21 s to give up. Nothing else could run meanwhile: in a probe, a task ticking every 500 ms stopped at the 2 s TCP timeout and resumed at 23.1 s. So `-l` and Ctrl+C couldn't end it.
  - The error reported was the pipe's ("The specified network name is no longer available", os error 64), not the TCP timeout. A repeat failed after 2.1 s with os error 53, because SMB had cached the failure, which made the first case look random.
  - The explicit routes freeze the same way (`-l 2`, an address that doesn't answer): `-S np:192.0.2.1` took 21.1 s, and so did `-S tcp:\\192.0.2.1\pipe\sql\query`, because the driver turns the path into a pipe whatever the prefix said.
  - An IPv6 address without a port takes the waterfall too: the driver reads `tcp:2001:db8::5` as one name, so there is no prefix to give it. `-S 2001:db8::5` held the run for 23.1 s, and `-S 2001:db8::5,1433` (a port selects TCP only) failed at the TCP timeout after 2.05 s. Text with a single colon, such as `db01:1433`, failed at once (8 ms in the same probe).
- **Cancel during connect.** The driver stops a pending attempt: `execute_action_chain` wraps the whole connect in `run_until_cancelled` (`tds_connection_provider.rs`), and also checks the token before each attempt. Checked with sqlh's own cancel race removed: Ctrl+Break during a TCP attempt to an address that doesn't answer (`-l 30`) ended the run with exit 6 in 50 ms, and during a login against a silent listener it ended it at once too. What a cancel can't interrupt is a synchronous call, such as the first open of a Named Pipe (above). *(An earlier version of this note said the driver ignored a cancel during an attempt. That was wrong.)*

## What sqlh does

- One attempt per address, no retries. Each TCP connect attempt gets `-l` seconds, and the deadline is `-l` + 1, so for one address the timers can't race. A name with several addresses adds the attempts up, and the deadline can cut a later one (exit 4 where the attempts alone would end with exit 2). With `-l 1` and two addresses the second attempt's own timeout and the deadline fall at the same moment, 2 s, and the deadline won in 6 of 6 runs by the few milliseconds the name lookup takes: no margin to rely on. With `-l 2` the deadline has a second to spare.
- A remote name gets a `tcp:` prefix (`db::data_source`); an IPv6 address, which can't take one, gets `,1433` when it has no port. Not covered: an IPv6 address with an instance name (`addr\INST`) has no way onto TCP only, because a port would replace the instance; the driver tries TCP from the SQL Browser reply and then the pipe path in the same reply, so a pipe open to the server's own name can still follow a failed TCP connect. A port (`addr,port`) avoids it. Local names keep the driver's order, which never reaches SMB. The driver itself already defaults to TCP under multi-subnet failover.
- A Named Pipes request to another machine is refused with exit 3 before anything connects (`db::check_server`, called from settings resolution and `profile add`): an `np:` prefix with a remote host, or a UNC pipe path with any prefix. The message gives the server's TCP spelling. Local pipes (`np:.`, `np:localhost`, this computer's name, `\\.\pipe\...`), `lpc:`, `admin:` and LocalDB pass through as written. This replaces passing an explicit protocol through: the first release has no way to time out or cancel a pipe open, so a remote pipe waits for a driver that can (see the first issue below).
- The connect races sqlh's own cancel flag and is dropped when that is set. The driver would stop it too; the race keeps sqlh's behavior from depending on that.
- The exit-4 message says the login wasn't completed within the deadline, which is true of both causes above.

## Results

| Case | Before 2.4 | After |
|---|---|---|
| Refused (`127.0.0.1,1`, `-l 15`) | exit 2 after 14.2 s | exit 2 after about 2.1 s |
| Doesn't answer (`192.0.2.x`, `-l 2`), first try | exit 2 after 23.1 s, os error 64 | exit 2 after 2.0 s, "timed out after 2000ms" |
| Doesn't answer, repeated | exit 2 after 2.1 s, os error 53 | exit 2 after 2.0 s |
| Accepts and never answers, `-l 1` | | exit 4 after 2 s |
| Ctrl+Break during that connect, `-l 30` | ended by Windows, no status line | exit 6 in under 2 s |

Measured later the same day, for the refusal of remote Named Pipes (the first column is the code before that change):

| Case | Before | After |
|---|---|---|
| Explicit remote pipe (`np:192.0.2.1`, `-l 2`) | exit 2 after 21.1 s | refused: exit 3 in under 0.1 s |
| The same as a UNC path, with `tcp:` in front | exit 2 after 21.1 s | refused: exit 3 in under 0.1 s |
| IPv6 address without a port, doesn't answer (`2001:db8::5`, `-l 2`) | exit 2 after 23.1 s | exit 2 after 2.05 s |
| Two addresses that refuse (`localhost,1`: `::1` and `127.0.0.1`) | | exit 2 after 4.1 s (2.0 s for one address) |
| The same with `-l 2` | | exit 4 after 3.1 s: the first attempt ends at 2 s, and the deadline (3 s) cuts the second |

## Upstream issues worth filing

Read in 0.2.0; check `main` (0.3.0) first. Upstream takes issues, not PRs.

1. **The first open of a remote Named Pipe blocks the async runtime.** `open_named_pipe_with_retry` calls tokio's `ClientOptions::open`, a synchronous `CreateFileW`, inside the async fn, whether the pipe came from the protocol waterfall, an `np:` prefix or a UNC path. For an unreachable host that stalls every timer, cancellation and task for about 21 s (SMB's own timeout), and the error that comes back is the pipe's instead of the TCP timeout. The retry that follows `ERROR_PIPE_BUSY` is not part of this: its `WaitNamedPipeW` already runs on `spawn_blocking`. Possible fixes: run the first open on a blocking thread too and bound it by the connect timeout (the open itself can't be cancelled, but the runtime and its timers stay free), or leave remote names on TCP unless Named Pipes are asked for. Related: an IPv6 address can't take a `tcp:` prefix, so without a port it gets the waterfall and the same freeze.

An earlier draft had a second issue, that a pending connect attempt ignores its cancel token. It is withdrawn: 0.2.0 stops the attempt (see "Cancel during connect" above).
