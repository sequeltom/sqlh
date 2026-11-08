# sqlh

Shared repository instructions for Codex and Claude Code. Keep project guidance in this file; CLAUDE.md imports it.

`sqlh` (`sqlh.exe`; repository `tkwj/sqlh`, private for now) is a SQL Server query tool for AI agents and harnesses. It does the job of `sqlcmd`, but as a single exe with no dependencies. Its output, errors and guardrails are built for models: compact typed output, hard row and size limits, errors with exit codes and real line numbers, no prompts, and read-only by default.

The product vision and priorities are in [docs/VISION.md](docs/VISION.md): fast, context-efficient, human-led DBA investigation across repeated agent turns. The full technical spec is [docs/DESIGN.md](docs/DESIGN.md). Before changing CLI behavior, output formats, exit codes or the read-only guard, read the relevant section, and update it in the same change.

## Hard constraints

Don't trade these away.

- **Zero runtime dependencies.** The first release runs on Windows x64 workstations (validated on Windows 11) and connects to SQL Server 2025 Standard, usually on a remote server. It needs no ODBC, OLE DB, VC++ redistributable, .NET or OpenSSL. It ships as one exe with the CRT statically linked (`+crt-static`), and imports only DLLs that ship with the supported Windows version. Check with `dumpbin /dependents`. SQL Server 2016 SP3 and later are supported for reading too, tested on the owner's 2016 instance (decision 49). A Windows 10 / Windows Server 2016 floor for the machine sqlh runs on is a candidate expansion, conditional on acceptable technical debt; it is not a first-release gate.
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
src/engine.rs    the connection-reuse engine (plan-history.md 5.2, decisions 14, 15, 17, 19): a pool per profile,
                 the reset on reuse, a 5 s probe on a reused connection's setup alone (one it cuts on a
                 live connection, its attention acknowledged within the driver's 5 s, is set up again
                 there; the call's USE then runs once, with the call's timeout), one silent retry on a
                 new connection when the setup or the USE finds it dead (nothing of the call ran yet),
                 the cancel rule (17, amended), put_back's rollback, idle close; no MCP in it
src/mcp/         sqlh mcp (plan-history.md 5.3, docs/mcp-mode.md): protocol.rs (JSON-RPC, the two eras),
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
src/guard.rs     read-only classifier (own T-SQL lexer, streamed, linear lookahead, work budget); XACT_ABORT detection;
                 guard_ignored.rs, its measured table of what the servers ignore in a word, shared with a live test
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
                 verify, read-only; publish, the draft release; plan-history.md 3.6, R22)
about.toml, about.hbs  cargo-about's accepted licences and the THIRD-PARTY-NOTICES.html template
README.md        human-facing intro, build, quick start, safety note, the running list of agent features
docs/VISION.md   product direction and priorities (the owner's); it takes precedence over older plans
docs/PLAN.md     the plan: open and planned work, sequencing, owner decisions, first-release checklist
docs/history/plan-history.md   the plan's finished work: review triage, Phases 1–5, the driver upgrade,
                 versioning, every review and fix verification through R50
docs/DESIGN.md   the spec;  docs/notes/  findings (M0 driver spike, connect latency, connect failures, harness kill, drain
                 experiment, memory, Windows shells, agent trials, the MCP reuse spike;
                 upstream-issues.md, the driver problems for the owner to raise upstream))
docs/schema-commands.md   the schema commands' spec (built);  docs/mcp-mode.md   sqlh mcp's spec (built 2026-10-17)
docs/command-categories.md   Phase 6's design: command categories and their policies (decided 2026-10-23: PLAN.md §6, decisions 22–36)
archive/         git-ignored, never pushed: archive/reviews/ holds the external code reviews
                 (findings F.., then R01 onward; PLAN.md and docs/history/plan-history.md record each one and its fix)
```

Planned, not built: an md format in `src/output/`. The workflows run on GitHub from the first push, 2026-10-15 (plan-history.md 3.5, 3.6).

The exe's version is set by `build.rs` (decision 44): `SQLH_VERSION` at build time if set (the release workflow sets it, and `SQLH_BUILD`, its run number), else `Cargo.toml`'s with `-local`. `build.rs` also writes the Windows version resource and links it as a .res file, with no rc.exe and no crate (DESIGN.md §4). A release tag must match `Cargo.toml`, so bump the version there before tagging: semver, and before 1.0 the minor for a release that changes the output contract or flags, the patch for fixes only.

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

The test server is the owner's, reached through a profile named `test`. The owner creates it with `sqlh profiles add test ...`, which prompts for the password. Never ask for the password, and never put it in a command, file or environment variable. To try profile commands without touching the real profiles, set `SQLH_CONFIG` to a scratch file. The 2016 instance is the owner's profile `test2016`; run the suite on it with `$env:SQLH_TEST_PROFILE='test2016'; pwsh scripts/check.ps1 -Live` (PLAN.md, SQL Server 2016 support). Profiles live in `%USERPROFILE%\.sqlh`, not under `%APPDATA%`, which Windows redirects for packaged apps such as the Claude desktop app (decision 53): a shell it starts sees different files there than the owner's does.

`dumpbin` isn't on PATH. Find it under `C:\Program Files\Microsoft Visual Studio\18\Enterprise\VC\Tools\MSVC\*\bin\Hostx64\x64\`. `dumpbin /dependents target\release\sqlh.exe` must list only in-box Windows DLLs.

## Conventions

### Repository housekeeping

- Commit source, tests and synthetic fixtures, Cargo.lock, build configuration, shared agent instructions, and useful design, plan and review documents. Early-development notes are worth keeping when they record decisions or findings.
- Put disposable experiments in `scratch/` and local query results, logs and copied executables in `artifacts/`. Both root directories are ignored. Promote useful work into source, tests or docs before relying on it.
- Code reviews are kept in the git-ignored `archive/reviews/` and never committed. A reviewer's new round goes there; each finding (R-number), its fix and its verification are recorded in PLAN.md while its work is open, and move with the finished work to docs/history/plan-history.md, so the plan never depends on the review files. What goes to external review is decided by risk (decision 52): the guard and run policy, the output contract, credentials, the engine, cancellation and MCP protocol, driver upgrades and the release workflows do, before they are built on; a P3 fix touching only tests or docs, and docs-only or test-only changes, are self-verified with `scripts/check.ps1`, recorded in PLAN.md, and listed in the next brief instead of getting a round of their own. Before any release, every change since the last review is covered by one.
- The remote is `tkwj/sqlh` on GitHub, private (created by the owner and first pushed 2026-10-15). Push only when the owner asks, and push `main` only: other local refs (`drain-prototype`, tool checkpoints) stay local. Before a push, check what it would publish (the history audit of 2026-10-15 found nothing to rewrite). Making the repository public is the owner's decision. The history was rewritten on 2026-10-12 to drop the review files (PLAN.md §6.8), so never commit one again. Commit ids from before the rewrite (in the local reviews, or in old copies of the docs) map to the new ones in `artifacts/history-rewrite-commit-map.txt`, which stays local.
- Keep real server data, connection profiles, credentials and personal agent state out of commits. Do not blanket-ignore SQL, JSON or CSV: those formats can also hold legitimate fixtures and examples.
- An MCP client test (plan-history.md 5.5) keeps its client's configuration in `scratch/` or outside the repository, and removes from the client what the test added (an MCP entry, a project entry), checked against a fingerprint taken first. `.gitignore` also catches what a client leaves if pointed at the checkout: `.mcp.json`, `opencode.json(c)`, `.opencode/`, `.mcp-inspector/`, and Node tooling (`node_modules/`, npm caches, a root `package.json`); sqlh has no Node code.
- Keep `target/` as a local build cache during active development. Run `cargo clean` when reclaiming disk space or troubleshooting build artifacts; the next build must recreate them. Do not routinely clean between agent turns.
- Before committing, inspect `git status --short`, `git diff --cached --stat`, and `git diff --cached`. Use `git ls-files --cached --ignored --exclude-standard` to catch previously tracked files that ignore rules do not remove. Review new files as well as edits.
- Commit subjects are short and imperative ("Fix", "Record", "Drop", not "Fixed" or "Fixes"); a finding, decision or task number may lead (`R50:`, `Decision 45:`), but no conventional-commit prefixes (`feat:`, `fix:`). Add a body only when the subject doesn't carry the why.
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
- **Comments say why, not what.** Rust shows the what; a comment gives the reason, the invariant, the rule's source (a DESIGN.md section, a decision, a finding) or the trap. No filler ("This ensures that", "In order to", "It's worth noting"), no restating the next line, no narrating an obvious test's steps, and `///` only where it adds that. The docs' density (Documents and their readers) means exactness, not padding: no filler there either. If a senior Rust engineer would roll their eyes at it, don't write it.
- **Hot paths:** the row loop (`src/db/`, `stream.rs`), the formatters (`src/output/`) and the guard's lexer run per row, per value or per token. There, borrow (`&str`, `&[u8]`, `Cow`) rather than copy, reuse buffers, and keep `clone()`, `to_owned()`, `to_vec()`, `collect::<Vec<_>>()` and temporary wrappers out of the loop. A copy needs a reason stated in the change: ownership, a thread boundary (the writer thread's queue), safety, or a measurement. Don't trade a borrow for a copy just to satisfy the borrow checker; narrow the scope or reshape the API first. A performance claim needs a measurement (before and after, `cargo bloat`, peak memory), not intuition; no `#[inline]` by habit. A fast path keeps its fallback's behavior exactly.
- **Before adding a refusal, guard rule or platform check, grep for what already handles it:** the guard, the argument checks in `cli.rs` and `mcp/tools.rs`, the tests and DESIGN.md. A spec calling something unsupported doesn't mean the code hasn't solved it.
- **Catalog SQL** must run on SQL Server 2016 SP3 and later (decision 49), validated on SQL Server 2025 Standard and on the 2016 test instance. A feature that needs a later version says so in its output on an older server instead of failing (for example, a time-zone name needs `CURRENT_TIMEZONE()`, SQL Server 2019 and later). There is no SQL Server 2012 or 2014 requirement; Azure follows its own plan (PLAN.md, Azure SQL). Additional version-specific paths need a supported-target decision and a maintenance justification.
- **Tests:**
  - Reuse the existing helpers before writing new ones: `db::fake_server` and its `Turn`s for the protocol, and in `tests/cli.rs` `Live`, `Reader`, `Blocked`, `OwnScratch`, `Cleanup` and the bounded runners below. A new pattern needs a reason the old ones can't serve.
  - The guard, splitter, renderer and formatter get table-driven unit tests next to the code.
  - Every new output shape gets an exact-output test (see `output/text.rs`).
  - Process-level behavior (exit codes, files, crashes) gets a test in `tests/cli.rs`, isolated from the machine's profiles and `SQLCMD*` variables.
  - Live tests are `#[ignore]`d, so skipping shows; asked for without `SQLH_TEST_PROFILE`, they fail. Before writing, they check the scratch database's `dbo.sqlh_scratch` marker, then write only to tables of their own, which they drop; the one exception is `sqlh_test`'s compatibility level, which a test may set and must restore (owner decision 21, PLAN.md §6). Every live test holds the scratch lock (a file in `%TEMP%`, held across processes): shared through `target()`, or exclusive through `target_alone()` for a test that changes database-level state or reads the whole catalog (`find --text` lost a deadlock to other tests' DDL). Once per test process, before the first hold, `hold_scratch` puts back a level a killed run left. A fixture arms its idempotent cleanup (`Cleanup`, `DROP ... IF EXISTS`) before the write that makes the object, never after (R24).
  - A live test's files go in a directory of its own process (`OwnScratch`, `{test}_{pid}`, removed when dropped), never a fixed name: the debug and release test binaries share `target/tmp`, and the shared scratch lock lets their live tests run at once.
  - `WAITFOR` is refused in every read-only run, so in every `sqlh mcp` call: a live test that needs a call to wait (a timeout, a cancel) reads a table of its own behind a write another session holds (`Blocked`, `live::mcp_tools`), a lock wait that costs the server no CPU; a CPU-bound wait would slow the timing tests running alongside.
  - The `-t 1` timing tests (`timed`, `timed_with_work`, both on `timed_run`) bound a run on sqlh's own clock, the run's time less its connect (DESIGN.md §4.4), not the process's: under a full parallel run a busy server can take the whole 5 s to acknowledge a cancel, and the connect then made an outside clock fail a run that kept its bound (plan-history.md, Phase 5.4, Still open; fixed 2026-10-19). Other live timing tests (`a_timeout_during_the_count_reports_a_bound_not_exit_4`, `a_timeout_that_is_not_in_hidden_rows_is_still_exit_4`, `a_large_result_is_cut_within_about_a_second`, the lock-timeout and deadlock tests) still check the process's time once it has ended. A stopped count's bound is checked for its shape, never for a throughput: `is_stopped_bound(bound, shown, most)` states `Tally::rows_total`'s rule, since the 250 ms budget promises no number of rows. `a_timeout_during_the_count_reports_a_bound_not_exit_4` took only 10,000 or 20,000, and failed on a valid 9,000 under a full parallel run's load, until `d3ed9fc` (the R45 verification's count-budget judgment); `a_large_result_is_cut_within_about_a_second` and `a_fast_narrow_result_is_bounded_by_t_in_every_mode` use the rule too. If a test needs a number of rows to arrive, arrange it deterministically against the fake server; a live clock can't promise one. `the_read_uncommitted_note_counts_against_the_output_cap` failed now and then under that load because it assumed the count finished, and now counts with `--exact-count` (PLAN.md §4, diagnosed 2026-11-07). Never retry a failed test until it passes (no test-runner retries): a flake is a finding to diagnose.
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

Keep this section short: where the work stands and what comes next. Review and fix history (commit ranges, R-numbers, what each verification ran) goes in PLAN.md while its work is open, then moves to docs/history/plan-history.md; never here.

- **Built:** the MVP (DESIGN.md §13); profiles and Credential Manager; the text, json and csv formats; the schema commands; `sqlh mcp` on the connection-reuse engine; versioning (decision 44); and decision 45's longer count and `-- partial:` runs. Phases 1–5 are done (plan-history.md §4). mssql-tds 0.3.0 is on `main` ([docs/notes/driver-0.3.0.md](docs/notes/driver-0.3.0.md)).
- **First release:** a private `0.1.0` for the owner and a few colleagues, with what is built, Phase 6 Part 1's first slice and Windows authentication in scope; its gates are PLAN.md §7 (decision 50, as amended).
- **Next: Phase 6, classifier v2 Part 1, in thin slices** ([docs/command-categories.md](docs/command-categories.md); decisions 22–36, ordered by decision 51): the first slice, refusal wording and the five small items, is on `main` (PLAN.md §4, Phase 6), for 0.1.0; then `activity`/`errorlog`, read list round 2, contention, the pause and showplans, linked servers last; each slice released and used before the next. Part 2 waits for real use. Phase 6's interim EXEC allowlist is built (DESIGN.md §7).
- **Planned, not started** (PLAN.md): `definition` paging and `info`'s dial name, AG role and time zone (MCP client feedback); setting the default profile; the md format; Azure SQL support and testing, whose scope is an owner decision.
- **Direction:** the CLI is the center, MCP a second front end (decision 46). Hosted MCP for remote connectors is planned with Entra pass-through only (decision 47; a shared read-only login is its backup, not built), and builds MCP's own exe first. The CLI is optimized for local, offline and small models (24 GB of VRAM or less): what they must read comes first, and there is no daemon behind it (decision 48).
- **Open:** the items carried over from finished work (PLAN.md §4, "Carried over from finished work"), among them the agent timeout trial and the flake `a_fast_narrow_result_is_bounded_by_t_in_every_mode`; the first-release checklist (PLAN.md §7), SmartScreen and antivirus among it; the driver items still unverified in [docs/notes/m0-driver-spike.md](docs/notes/m0-driver-spike.md).
- **Reviews:** the 0.1.0 external review's R51-R55 are fixed (through `e5bc6e1`) and await the reviewer's fix verification. Findings, fixes and checks are in PLAN.md §4; completed records move to plan-history.md §4. The last finding is R55, so the next is R56.
