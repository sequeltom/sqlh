# sqlh

Shared repository instructions for Codex and Claude Code. Keep project guidance in this file; CLAUDE.md imports it.

`sqlh` (`sqlh.exe`; repository `tkwj/sqlh`, private for now) is a SQL Server query tool for AI agents and harnesses. It does the job of `sqlcmd`, but as a single exe with no dependencies. Its output, errors and guardrails are built for models: compact typed output, hard row and size limits, errors with exit codes and real line numbers, no prompts, and read-only by default.

The product vision and priorities are in [docs/VISION.md](docs/VISION.md): fast, context-efficient, human-led DBA investigation across repeated agent turns. The full technical spec is [docs/DESIGN.md](docs/DESIGN.md). Before changing CLI behavior, output formats, exit codes or the read-only guard, read the relevant section, and update it in the same change.

## Hard constraints

Don't trade these away.

- **Zero runtime dependencies.** The first release runs on Windows x64 workstations (validated on Windows 11) and connects to SQL Server 2025 Standard, usually on a remote server. It needs no ODBC, OLE DB, VC++ redistributable, .NET or OpenSSL. It ships as one exe with the CRT statically linked (`+crt-static`), and imports only DLLs that ship with the supported Windows version. Check with `dumpbin /dependents`. A Windows Server 2016 / SQL Server 2016 floor is a candidate expansion, conditional on acceptable technical debt; it is not a first-release gate.
- **Lean and fast.** Every new crate needs a reason. Check what it adds to exe size (`cargo bloat --release --crates`) and build time, and prefer a few lines of our own code. Stream rows, never buffering a whole result set.
- **Never interactive.** Only `sqlh profiles add` reads from the console. With nothing to run and stdin attached to a console, print usage and exit 3. Never wait.
- **No secrets on the command line or in output.** `-P` is rejected on purpose. Passwords come from Windows Credential Manager (through profiles) or `SQLCMDPASSWORD`. Never log, echo or format a password or a connection string that contains one.
- **Output is the product.** Agents read every byte. The formats in DESIGN.md §5 are a contract: stable, unambiguous and short on tokens. Any change needs updated golden tests and an updated DESIGN.md.

## Stack

- **Rust:** stable, pinned in `rust-toolchain.toml` to 1.99.0. Edition 2024, target `x86_64-pc-windows-msvc`. The static CRT is set in `.cargo/config.toml`.
- **TDS driver:** Microsoft [`mssql-tds`](https://github.com/microsoft/mssql-rs), MIT, a preview.
  - Its default features provide SSPI and SChannel TLS, so there is no OpenSSL.
  - Pin an exact version (`=x.y.z`) and read the changelog before bumping.
  - Upstream takes no PRs, only issues, and agents never file them: a driver problem worth raising goes in `docs/notes/upstream-issues.md`, the one list the owner files from (owner, 2026-10-17). Keep workarounds in our code.
- **Only `src/db/` may use `mssql-tds` types.** All other code uses our own types, so the driver can be upgraded, or swapped for the tiberius-ng fallback, in one place.
- **Other crates:**
  - Tokio with the `current_thread` runtime.
  - `lexopt` for arguments. It's tiny, and sqlcmd-style flags (`-v a=1 b=2`, `-Ns`) are awkward in clap.
  - `windows-sys` for Win32 calls. It's already a transitive dependency.
  - `serde_json` for `sqlh mcp`'s JSON-RPC: `Value` and `json!` only, no serde derive, to keep it small. It costs the exe 65 KB (spike 5.1); all of MCP is 310.0 KiB of decision 20's 350 KiB gate (amended 2026-10-18), with `serde_json` built for size (`opt-level = "z"`, `Cargo.toml`; PLAN.md §6).
  - `zeroize` wipes sqlh's own copies of a password on drop (`src/secret.rs`, F14). It's already a transitive dependency.
  - `log`, only with `--features timing` (dev builds): it reads the driver's log events to time each connect step, and prints none of them (see Driver logging below).

## Layout

```
build.rs         the version (SQLH_VERSION, else Cargo.toml's with -local) and the Windows
                 version resource, written as a .res file the linker takes (decision 44);
                 src/build_version.rs checks SQLH_VERSION (R47), shared with the lib's tests
src/main.rs      the binary: calls sqlh::main; everything else is the library (src/lib.rs)
src/app.rs       the command: arguments, output and its closing lines (Closing), -o publication,
                 the panic net and last-resort diagnostics; profile commands use it
src/run.rs       the execution core, free of the CLI and of delivery: Request (what to run, built by
                 app from the flags), RunPolicy (guard, wrapper, --check), QueryOrigin (user SQL or
                 sqlh's own, which is always read-only), prepare (sources → batches → guard), the
                 session and batch loop into any Output, cancellation, Accounting (the run's own
                 count of errors and stopped counts) and RunOutcome (exit code and counts); the run
                 is split so a lent session can run it: preflight, open (setup, which a reused
                 connection's reset rides on, then use_database, a call's USE; the CLI calls open,
                 the engine the two apart), run_opened; Closing and close() end any output
src/engine.rs    the connection-reuse engine (PLAN.md 5.2, decisions 14, 15, 17, 19): a pool per profile,
                 the reset on reuse, a 5 s probe on a reused connection's setup alone (one it cuts on a
                 live connection, its attention acknowledged within the driver's 5 s, is set up again
                 there; the call's USE then runs once, with the call's timeout), one silent retry on a
                 new connection when the setup or the USE finds it dead (nothing of the call ran yet),
                 the cancel rule (17, amended), put_back's rollback, idle close; no MCP in it
src/mcp/         sqlh mcp (PLAN.md 5.3, docs/mcp-mode.md): protocol.rs (JSON-RPC, the two eras),
                 tools.rs (the read-only tools, their schemas, argument refusals as tool errors, a
                 call into the engine), mod.rs (the stdin, stdout and stderr threads, dispatch,
                 requests held back and the stall rule, cancellation, subscriptions, shutdown)
src/cli.rs       sqlcmd-compatible flags plus our long flags (lexopt); `profile` subcommands
src/config.rs    profiles.toml (own strict parser) and settings resolution/precedence
src/credman.rs   Windows Credential Manager (CredReadW/CredWriteW/CredDeleteW); each password is bound
                 to its login and profile identity (R20)
src/secret.rs    Secret: a password as sqlh holds it, wiped on drop (zeroize)
src/profile.rs   sqlh profiles add | list | test | rm
src/schema.rs    schema commands (info, dbs, tables, describe, find, def, deps): their SQL, N'...' literals,
                 patterns, name parsing and resolution (a lookup, run::Plan::Lookup), the bracketing
                 of names it shows, and labeled sections
src/help.txt     --help text (models read it)
src/error.rs     exit codes (DESIGN.md §6) and Failure
src/console.rs   echo-off password prompt; the Git Bash path hint (the console code page is left alone)
src/signals.rs   the console events that cancel a run: Ctrl+C, Ctrl+Break, close, logoff, shutdown
src/files.rs     -o files: identity (refusing an input file) and the run's own .partial file
src/script.rs    input decoding, GO splitting, $(var) substitution, line mapping
src/guard.rs     read-only classifier (own T-SQL lexer, streamed, linear lookahead, work budget); XACT_ABORT detection
src/db/          the only mssql-tds user: connect (a failed connect's hints name flags, or in sqlh mcp
                 the profile's keys: Hints), run a batch into a BatchSink, the bounded count of
                 hidden rows (Counting), value/type rendering;
                 stream.rs reads large (PLP) values in chunks and cuts them as they arrive
src/output/      Output trait + Tally (shared limits/counts), Advice (CLI or sqlh mcp wording of
                 what a result advises and of the limits it names); text.rs (default), json.rs,
                 csv.rs; delivery.rs (the writer thread and its bounded queue)
tests/cli.rs     process-level tests of the real binary; `live` ones are ignored unless asked for
tests/fixtures/  creates/drops the sqlh_test scratch database the live tests use; shrinks it
tests/smoke.sql  read-only end-to-end script for a real server; tests/golden/ holds its expected
                 output (text and JSON, volatile values normalized), checked by a live test
scripts/check.ps1  the release check: fmt, clippy (both feature sets), tests, --locked release build,
                 size budget (3 MB), in-box imports; -Live adds the live tests. CI runs it too
.github/workflows/  ci.yml (check.ps1 on every push and PR); release.yml (tag or dispatch: build,
                 read-only; sign, the only job with the Azure token, which never runs the exe;
                 verify, read-only; publish, the draft release; PLAN.md 3.6, R22)
about.toml, about.hbs  cargo-about's accepted licences and the THIRD-PARTY-NOTICES.html template
README.md        human-facing intro, build, quick start, safety note, the running list of agent features
docs/VISION.md   product direction and priorities (the owner's); it takes precedence over older plans
docs/PLAN.md     the plan of attack: review triage, phases, owner decisions
docs/DESIGN.md   the spec;  docs/notes/  findings (M0 driver spike, connect latency, connect failures, harness kill, drain
                 experiment, memory, Windows shells, agent trials, the MCP reuse spike;
                 upstream-issues.md, the driver problems for the owner to raise upstream))
docs/schema-commands.md   the schema commands' spec (built);  docs/mcp-mode.md   sqlh mcp's spec (built 2026-10-17)
docs/command-categories.md   Phase 6's design: command categories and their policies (decided 2026-10-23: PLAN.md §6, decisions 22–36)
archive/         git-ignored, never pushed: archive/reviews/ holds the external code reviews
                 (findings F.., R01–R47; PLAN.md records each one and its fix)
```

Planned, not built: an md format in `src/output/`. The workflows run on GitHub from the first push, 2026-10-15 (PLAN.md 3.5, 3.6).

The exe's version is set by `build.rs` (decision 44): `SQLH_VERSION` at build time if set (the release workflow sets it, and `SQLH_BUILD`, its run number), else `Cargo.toml`'s with `-local`. `build.rs` also writes the Windows version resource and links it as a .res file, with no rc.exe and no crate (DESIGN.md §4). A release tag must match `Cargo.toml`, so bump the version there before tagging: semver, and before 1.0 the minor for a release that changes the output contract or flags, the patch for fixes only. External review of `7fed369..157334f` on October 31, 2026 accepted decision 44 with nonblocking P3 R47: tighten SQLH_VERSION validation before Cargo output, since malformed suffixes can split the code/resource identity. R47 is fixed (`src/build_version.rs`), awaiting external verification. Resource layout, SDK comparison, native queries, default non-live checks and local verification-span probes passed; the hosted dispatch was still in progress at the recorded snapshot (PLAN.md, Versioning external review).

## Commands

**Setup (done on this machine):**
- Rust comes from rustup (`winget install Rustlang.Rustup`) and lives in `%USERPROFILE%\.cargo\bin`. Shells started before the install need that on PATH.
- Linking needs two VS 2026 components: "MSVC x64/x86 build tools" (`Microsoft.VisualStudio.Component.VC.Tools.x86.x64`) and "Windows 11 SDK" (`Microsoft.VisualStudio.Component.Windows11SDK.26100`).

**Day to day:** `pwsh scripts/check.ps1` runs every check below that needs no server and stops at the first failure; `pwsh scripts/check.ps1 -Live` adds the live tests. Run it before every commit that touches code. The individual commands:

```powershell
cargo build --release                               # ~2.9 MB exe (budget 3 MB); full LTO takes ~1–2 min
cargo test                                          # unit + process-level tests; no server needed
cargo test --release --test cli                     # process-level tests against the release exe
cargo build --release --features timing             # --verbose also times each connect step (dev builds; +270 KB)
$env:SQLH_TEST_PROFILE='test'; cargo test --test cli -- --ignored live   # live tests (scratch db sqlh_test)
cargo test -- --ignored credman                     # real Credential Manager round trip (cleans up)
cargo clippy --all-targets -- -D warnings
cargo fmt
target\release\sqlh.exe --profile test -i tests\smoke.sql --continue-on-error   # end-to-end; expect exit 1
```

The test server is the owner's, reached through a profile named `test`. The owner creates it with `sqlh profiles add test ...`, which prompts for the password. Never ask for the password, and never put it in a command, file or environment variable. To try profile commands without touching the real profiles, set `SQLH_CONFIG` to a scratch file.

`dumpbin` isn't on PATH. Find it under `C:\Program Files\Microsoft Visual Studio\18\Enterprise\VC\Tools\MSVC\*\bin\Hostx64\x64\`. `dumpbin /dependents target\release\sqlh.exe` must list only in-box Windows DLLs.

## Conventions

### Repository housekeeping

- Commit source, tests and synthetic fixtures, Cargo.lock, build configuration, shared agent instructions, and useful design, plan and review documents. Early-development notes are worth keeping when they record decisions or findings.
- Put disposable experiments in `scratch/` and local query results, logs and copied executables in `artifacts/`. Both root directories are ignored. Promote useful work into source, tests or docs before relying on it.
- Code reviews are kept in the git-ignored `archive/reviews/` and never committed. A reviewer's new round goes there; PLAN.md records each finding (R-number), its fix and its verification, so the plan never depends on the review files.
- The remote is `tkwj/sqlh` on GitHub, private (created by the owner and first pushed 2026-10-15). Push only when the owner asks, and push `main` only: other local refs (`drain-prototype`, tool checkpoints) stay local. Before a push, check what it would publish (the history audit of 2026-10-15 found nothing to rewrite). Making the repository public is the owner's decision. The history was rewritten on 2026-10-12 to drop the review files (PLAN.md §6.8), so never commit one again. Commit ids from before the rewrite (in the local reviews, or in old copies of the docs) map to the new ones in `artifacts/history-rewrite-commit-map.txt`, which stays local.
- Keep real server data, connection profiles, credentials and personal agent state out of commits. Do not blanket-ignore SQL, JSON or CSV: those formats can also hold legitimate fixtures and examples.
- An MCP client test (PLAN.md 5.5) keeps its client's configuration in `scratch/` or outside the repository, and removes from the client what the test added (an MCP entry, a project entry), checked against a fingerprint taken first. `.gitignore` also catches what a client leaves if pointed at the checkout: `.mcp.json`, `opencode.json(c)`, `.opencode/`, `.mcp-inspector/`, and Node tooling (`node_modules/`, npm caches, a root `package.json`); sqlh has no Node code.
- Keep `target/` as a local build cache during active development. Run `cargo clean` when reclaiming disk space or troubleshooting build artifacts; the next build must recreate them. Do not routinely clean between agent turns.
- Before committing, inspect `git status --short`, `git diff --cached --stat`, and `git diff --cached`. Use `git ls-files --cached --ignored --exclude-standard` to catch previously tracked files that ignore rules do not remove. Review new files as well as edits.
- sqlh is licensed MIT (`LICENSE`). A new dependency must be under a license compatible with that (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, Unlicense or similar permissive terms); check its `license` field before adding it, and never add a copyleft one.
- Never commit executables. When releases are ready, distribute them as GitHub release assets.
- Preserve useful development history. Move obsolete documents to a clearly named archive under `docs/` only when they are superseded, and update references in the same change. Before a public push, review the history as well as the current tree for private data.

### Documents and their readers

- **README.md is for humans:** people deciding whether to use sqlh, building it, or getting started. Write it plainly and briefly: what the tool does and why it matters, with little spec detail and no instructions addressed to agents.
- **Everything else is consumed mostly by agents** (and the owner skimming): this file, `docs/` (VISION, PLAN, DESIGN, the notes, the designs), the reviews, and `--help`. Write these for exactness and completeness: precise strings, section numbers to cross-reference, and the reasons an agent needs to act correctly. Density is fine there.
- **The docs are the source of truth.** A fact goes into DESIGN.md or PLAN.md first, and the README gets a plain-language summary of it, never the only record. If the README and the docs disagree, the docs win, and the README is corrected.
- Instructions for agents go in this file, not the README.

### Code conventions

- **Errors:** one error enum in `src/error.rs`, mapped to the exit codes in DESIGN.md §6. No `unwrap` or `expect` on anything derived from user input, the network or the server.
- **unsafe:** only in the FFI modules (`credman.rs`, `console.rs`, `files.rs`) and the tests' Ctrl+Break and peak-memory helpers, with a `// SAFETY:` comment on each block.
- **Driver logging:** never install a general logger (an `env_logger`, a tracing subscriber, a `log::Log` that prints) while mssql-tds runs. At debug level it dumps TDS packets before encryption, including the login packet and its password. `db::phases`, built only with `--features timing`, keeps timestamps of a few events and never their text.
- **Catalog SQL** must be validated on the first-release target, SQL Server 2025 Standard. Prefer inexpensive portability toward the candidate SQL Server 2016 floor, but there is no unconditional SQL Server 2012 or Azure requirement. Additional version-specific paths need a supported-target decision and a maintenance justification.
- **Tests:**
  - The guard, splitter, renderer and formatter get table-driven unit tests next to the code.
  - Every new output shape gets an exact-output test (see `output/text.rs`).
  - Process-level behavior (exit codes, files, crashes) gets a test in `tests/cli.rs`, isolated from the machine's profiles and `SQLCMD*` variables.
  - Live tests are `#[ignore]`d, so skipping shows; asked for without `SQLH_TEST_PROFILE`, they fail. Before writing, they check the scratch database's `dbo.sqlh_scratch` marker, then write only to tables of their own, which they drop; the one exception is `sqlh_test`'s compatibility level, which a test may set and must restore (owner decision 21, PLAN.md §6). Every live test holds the scratch lock (a file in `%TEMP%`, held across processes): shared through `target()`, or exclusive through `target_alone()` for a test that changes database-level state. Once per test process, before the first hold, `hold_scratch` puts back a level a killed run left. A fixture arms its idempotent cleanup (`Cleanup`, `DROP ... IF EXISTS`) before the write that makes the object, never after (R24).
  - A live test's files go in a directory of its own process (`OwnScratch`, `{test}_{pid}`, removed when dropped), never a fixed name: the debug and release test binaries share `target/tmp`, and the shared scratch lock lets their live tests run at once.
  - `WAITFOR` is refused in every read-only run, so in every `sqlh mcp` call: a live test that needs a call to wait (a timeout, a cancel) reads a table of its own behind a write another session holds (`Blocked`, `live::mcp_tools`), a lock wait that costs the server no CPU; a CPU-bound wait would slow the timing tests running alongside.
  - The `-t 1` timing tests (`timed`, `timed_with_work`, both on `timed_run`) bound a run on sqlh's own clock, the run's time less its connect (DESIGN.md §4.4), not the process's: under a full parallel run a busy server can take the whole 5 s to acknowledge a cancel, and the connect then made an outside clock fail a run that kept its bound (PLAN.md, Phase 5.4, Still open; fixed 2026-10-19). Other live timing tests (`a_timeout_during_the_count_reports_a_bound_not_exit_4`, `a_timeout_that_is_not_in_hidden_rows_is_still_exit_4`, `a_large_result_is_cut_within_about_a_second`, the lock-timeout and deadlock tests) still check the process's time once it has ended. A stopped count's bound is checked for its shape, never for a throughput: `is_stopped_bound(bound, shown, most)` states `Tally::rows_total`'s rule, since the 250 ms budget promises no number of rows. `a_timeout_during_the_count_reports_a_bound_not_exit_4` took only 10,000 or 20,000, and failed on a valid 9,000 under a full parallel run's load, until `d3ed9fc` (the R45 verification's count-budget judgment); `a_large_result_is_cut_within_about_a_second` and `a_fast_narrow_result_is_bounded_by_t_in_every_mode` use the rule too. If a test needs a number of rows to arrive, arrange it deterministically against the fake server; a live clock can't promise one. `the_read_uncommitted_note_counts_against_the_output_cap` has also failed now and then under that load: when it fails, record its figures, and don't loosen a bound without that diagnosis (PLAN.md, R45 fix verification).
  - Every wait for a run's end is bounded (R43), but one. The watchdog kills the process tree and fails the test with what the run wrote:
    - `run_bounded` at a test's own limit (`timed_run` at 11.5 s);
    - `finish` at `HANG` (300 s), which `run`, `sqlh_args`, `run_closed`, `run_with_stdin`, `live`, `live_with`, `collect` and the fixtures' `write` use. `write` reports a hung run instead of panicking, since cleanups call it while a test unwinds. Both write their stdin on a thread (`feed`), so the watchdog covers the write too;
    - spawned runs use `wait_within` or `Mcp`, which kill at their own limits.

    Keep `Server::write` fallible during cleanup: `Live::write` goes through `write_never_panicking`, on the fallible watchdog core `try_bounded_child`, so an OS error polling or waiting for the run or starting a thread comes back as a failed run, never a panic during another test's unwind (R45, `0d3a5f2`; `a_cleanup_write_reports_the_watchdogs_failures_instead_of_panicking` injects each through `Faults`). Other runners keep `bounded_child`, which panics on those errors.

    The exception is `run_measured`, which must keep the child's handle to read its peak memory. A new test never calls a bare `Command::output` or `Child::wait_with_output`.
  - Waits for a run's progress, not its end, are not bounded either: a fake server's `accept` (in several connect tests, and the concurrent `-o` test before run A reaches `finish`), a loop reading for a `--verbose` line (the Ctrl+Break tests), and the MCP slow-reader tests' reads to EOF before their `wait_within`. For those and `run_measured`, CI's 60-minute job cap is the bound.
  - Test server details and credentials never go in the repo.
  - Live tests that need permissions short of sysadmin use `Reader`: a login made per test with only read rights and a name of its own (`sqlh_reader_<pid>_<n>`, since the tests run in parallel), whose random password exists only in the test process and in the environment of the sqlh processes it starts (decision 5). That is the one password a test may put in an environment variable; the owner's never.
  - `SQLH_DEBUG_PANIC`, `SQLH_DEBUG_STOP_HOLD_MS`, `SQLH_DEBUG_SESSION_PREFIX` (SQL sent ahead of the session setup, in the same batch), `SQLH_DEBUG_ROW_WORK_US` (work per row and per value chunk in the row loop) and `SQLH_DEBUG_MCP_STALL_MS` (`sqlh mcp`'s stall limit for `-t 0`, in place of 60 s) exist only when debug assertions are on, so a test that needs one is `#[cfg(debug_assertions)]`: a release run must not report it as passed.
- **`--help`:** models read it, so keep it example-first and under about 60 lines.
- **README's "Built for agents" is a running tally.** When a change adds, alters or removes something made for agents (output shape, limits, errors, exit codes, guard behavior, safeguards, `--help` content), update that list in the same change: the entry, its `*(since ...)*` tag and the tally line. A planned item moves to Built only when its code and tests are committed; until then it stays under Planned with its PLAN.md task number. The README has a human reader (see "Documents and their readers"), so write each entry as what an agent gains and why it matters, in one or two plain lines; the exact behavior belongs in DESIGN.md.

## Status

The MVP is built; DESIGN.md §13 lists what's in and out. Git repo on `main`.

Profiles and Credential Manager are built, and so are the json and csv formats. The smoke test passes against the test server (SQL Server 2025); remaining unverified items are in `docs/notes/m0-driver-spike.md`.

The plan of attack is [docs/PLAN.md](docs/PLAN.md), derived from [docs/VISION.md](docs/VISION.md) and the triage of the architecture review (archive/reviews/, local only). Owner decisions are recorded in its §6. Work in order:
1. Phase 1, correctness and trustworthy outcomes: built and tested, including the manual Git Bash password check (PLAN.md 1.9) and the fixes from the follow-up review (PLAN.md, Phase 1 follow-up).
2. Phase 2, fast, bounded, cancellable turns. 2.1, the connect-latency investigation, is done: most of each connect is the login step, which the measurements attribute to SQL Server 2025's password hashing for SQL logins ([docs/notes/connect-latency.md](docs/notes/connect-latency.md)). 2.2 (the response budget), 2.3 (a writer thread, so a stalled reader can't block the run) and 2.4 (cancellation everywhere, and connect failures that end the same way every time: [docs/notes/connect-failures.md](docs/notes/connect-failures.md)) are done, and so are the fixes the Phase 2 review asked for (PLAN.md, Phase 2 follow-up). 2.5 (harness kill), 2.6 (the drain experiment; the default is owner decision 6), 2.7 (memory), 2.12 (where messages land), 2.13 (Windows shells and paths) and 2.11 (provenance in every format) are done too. Owner decisions 6, 7, 10 and 11 (2026-10-11, PLAN.md §6) are built: 2.8 (agent session defaults: lock timeout, deadlock priority, opt-in READ UNCOMMITTED), 2.15 (the bounded count of hidden rows; its `-t` rule under `--allow-write` and `XACT_ABORT` is confirmed, §6.6), the text part of 2.11 and 2.13's follow-up; so is 2.14 (the guard's memory). The completion review of Phase 2 (R14–R18: `XACT_ABORT OFF` in the session setup, sqlh's own elapsed `-t`, JSON contract 2.0, incomplete closure after a SQL error, the guard's work bound) is fixed; the verification closed all but R15, whose follow-ups were fixed (the settlement bound was accepted, and the late-finish rule now goes by the phase the deadline passed in: the row that fills `--max-rows` is a row to show) and found closed by the Phase 3 review (PLAN.md, Phase 2 follow-up and Phase 3 initial review). Agent trial 2 (2.10) ran on 2026-10-12 ([docs/notes/agent-trial-2.md](docs/notes/agent-trial-2.md)); task 2.16 and owner decision 12 (new profiles refuse writes) followed from it. Phase 2 is done; decision 12's last question was settled on 2026-10-12 (`profiles list` keeps showing the server and login).
3. Phase 3, execution core, release checks, live fixtures, and GitHub Actions CI plus a signed release workflow: 3.1–3.4 are done, and the code review accepted Phase 3 after the R20–R24 fixes (PLAN.md, Phase 3 fix verification). 3.5 and 3.6 ran on GitHub on 2026-10-15: CI is green, and the release workflow signed a draft pre-release, `v0.1.0-dev.1`, after the federated credential took GitHub's immutable subject (PLAN.md 3.5, 3.6). Phase 3 is done; SmartScreen and antivirus on a locked-down workstation remain on the first-release checklist (PLAN.md §7).
4. Phases 4–8: schema discovery, connection reuse, command categorization, evidence-based evaluations, delivery to agents (skill, MCP connector, plugin). Phase 4 began with profile descriptions and `profiles list --search` (4.1–4.3, accepted after the R25–R26 fix verification on 2026-10-13; PLAN.md, Phase 4.1–4.3 fix verification). Owner decision 13 keeps `profiles list` text only. The schema commands (`info`, `dbs`, `tables`, `describe`, `find`, `def`) were accepted after R27–R33 closed on October 14, 2026 (PLAN.md, Schema commands fix verification). The preview boundary fix and `deps` implementation were accepted on October 14, 2026; R34 was fully closed on October 18 after verification of the original corrections and removal of the default-grant parenthetical (PLAN.md, Phase 5.2 and 5.3 fix verification). JSON contract 2.1 adds their `label`. Phase 5 (connection reuse) follows owner decisions 14–20 (2026-10-15 to 2026-10-17, PLAN.md §6): MCP first, stateless, read-only, a launch allowlist of profiles, one exe behind a size gate. 5.1, the spike, is done ([docs/notes/mcp-reuse-spike.md](docs/notes/mcp-reuse-spike.md)). 5.2 (the engine) and 5.3 (`sqlh mcp`) were built and self-reviewed through `9b5318d`; the external review of `2f53735..a8c5390` on October 18, 2026 requested R35–R39 (cancellation/EOF under stdout backpressure, console events throughout shutdown, the aggregation threshold, lossless numeric request ids, method-envelope validation) and accepted decision 19's setup probe with its ten-second boundary. The external fix verification of `a8c5390..c50dd3c` closed R34–R39 and accepted 5.2/5.3 on October 18, 2026, with one nonblocking P3 interop follow-up, R40 (integral numeric-id forms), fixed in `aa06856` and closed by external verification of `c50dd3c..0d4cf2c` on October 18, 2026. That verification records one separate nonblocking P3 follow-up, R41 (exact integer validation for MCP tool limits), fixed in `a55c70d` and closed by external verification of `0d4cf2c..3472910` on October 18, 2026. That review found new P2 R42: generic numeric-argument rereading rescans the whole message per float member and delays cancellation. R42 is fixed in `e60a1de` (the reread limited to the three declared numeric arguments: at most three linear scans of a call's line, on top of `serde_json`'s own parse, which is bounded but not linear in the message), closed by external verification of `3472910..ab6eed7` on October 18, 2026. This increment is accepted, no new finding; see PLAN.md, R42 fix verification. Owner decision 21 lets a live test set `sqlh_test`'s compatibility level; its documented next-process recovery limit is accepted for the current test workflow. The fixes took MCP to 310.0 KiB, past decision 20's 300 KiB gate; the owner amended decision 20 on 2026-10-18 (a 350 KiB gate, `serde_json` built for size: 300.5 KiB, 309.0 KiB with the R40–R42 fixes). See PLAN.md, Phase 5.2 and 5.3 fix verification; docs/mcp-mode.md is the as-built spec. 5.4, the live tool tests, is done (2026-10-18: `a33d2ea`, review fixes `814ee12`, and `fbbe1a8`, a login name per `Reader`; tests only; externally reviewed in ef321ab..a6a4bc2 with the supplied live passes distinguished from independent checks; PLAN.md, Phase 5.4–5.6 external completion review): six live tests drive `sqlh mcp` against the test server (`tests/cli.rs`, `live::mcp_tools`), and the review's question 3 is answered: after `ROWCOUNT`, `TEXTSIZE`, `LANGUAGE`, `DATEFORMAT`, `DATEFIRST`, `CONTEXT_INFO` and a global cursor the next call is a fresh login's, values and effects, so `run::session_setup` is unchanged. `scripts/check.ps1 -Live` passes again since its `-t` bounds are measured on sqlh's own clock (owner, 2026-10-19; PLAN.md, Phase 5.4, Still open), and `sqlh mcp` now waits up to 0.5 s for its last `--verbose` lines before exiting (`32c82d5`, the CI failure after `ef321ab`). 5.5 (interop) ran with OpenCode (owner, 2026-10-20, in place of Claude Code; mcp-mode.md §9, Client interop): the legacy era works end to end, refusals and errors reach the model as `isError`, and parallel calls run on pooled connections; the MCP Inspector exercised the modern era (2026-07-28, `server/discover` and `_meta`) as well as legacy and `auto` (2026-10-21), and Claude Code 2.1.288 connects through the README's `claude mcp add`, speaks the modern era, and a session of it ran the tools on one reused connection (2026-10-21); 5.5 is done. 5.6, agent trial 3, ran on 2026-10-21 with OpenCode ([docs/notes/agent-trial-3.md](docs/notes/agent-trial-3.md)): offered both, the model used only the MCP tools, and asked to keep a temp table across questions it kept the work in one call, so decisions 14 and 15 stand; its two output follow-ups are built (`3915347`: a cut value's note says how to read the rest, and MCP's stopped-count note offers a `COUNT_BIG(*)`, R44), and a timeout trial stays open (PLAN.md 5.6). Phase 5 is done. External review of `ef321ab..a6a4bc2` accepted the increment with two nonblocking P3 follow-ups, both fixed in `901908f` and closed by external verification of `a6a4bc2..5db639b` on October 22, 2026: R43 (an active watchdog for the live timing helpers, whose outside bound had been checked only after process exit: `run_bounded`) and R44 (COUNT_BIG(*) in the new large-count advice). That fix verification raised nonblocking P3 R45 (fallible watchdog errors inside fixture cleanup), fixed in `0d3a5f2` and closed by external verification of `5db639b..135d60c` on October 22, 2026; increment accepted with no new finding. Standard non-live checks and isolated error/unwind/lifecycle probes passed; see PLAN.md, R45 fix verification. No independent live/client rerun; the recorded timeout trial and count-budget flakes remain open. See PLAN.md, Phase 5.4–5.6 external completion review. Phase 6 (command categorization) began on 2026-10-23 with its interim step, built: the EXEC allowlist reads the error log (`xp_readerrorlog`, `sp_readerrorlog`, `sp_enumerrorlogs`), registry values, drives, the version and the `sp_configure` listing, each by exact name, with SQL Server 2025's permissions for them recorded live (DESIGN.md §7). The categorization was designed and decided on 2026-10-23 ([docs/command-categories.md](docs/command-categories.md); PLAN.md §6, decisions 22–36): kinds of change granted per profile and run, a deny tier, Part 1's read-only changes, and no `execute` tool in Phase 6. Building it (classifier v2, Part 1 first) is next, after the live checks the design lists.

Driver upgrade: local branch `driver-0.3.0`, `048aa89..7b28076`, externally reviewed on October 31, 2026. No blocking code finding; accepted with nonblocking P3 R46, correcting the optional/Strict certificate wording by negotiated TLS mode (PLAN.md, Driver 0.3.0 external review). Independent non-live checks and synthetic routing/USE/reset/connect/TLS-policy probes passed; owner live tests and measurements were reviewed, not rerun. R46 and the same rule's strict-hint defect are fixed in `e7dc9fb`, closed by external verification of `7b28076..e7dc9fb` on October 31, 2026, with no new finding. Non-live checks and six synthetic end-to-end TLS/protocol probes passed, including MCP profile hints; the positive type-21 test passed. The upgrade is merged into `main` as `1dd341f` (PLAN.md, R46 fix verification). The on-prem DNS acknowledgement is measured (2026-10-31, event-only recorder in a throwaway worktree): the test server's verdict is `allowed=false`, so its cache never holds it (driver-0.3.0.md §8). Planned, not started: Azure SQL support and testing (PLAN.md, after Phase 8), whose scope is an owner decision.
