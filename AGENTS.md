# sqlharness

Shared repository instructions for Codex and Claude Code. Keep project guidance in this file; CLAUDE.md imports it.

`sqlh.exe` (working name) is a SQL Server query tool for AI agents and harnesses. It does the job of `sqlcmd`, but as a single exe with no dependencies. Its output, errors and guardrails are built for models: compact typed output, hard row and size limits, errors with exit codes and real line numbers, no prompts, and read-only by default.

The product vision and priorities are in [docs/VISION.md](docs/VISION.md): fast, context-efficient, human-led DBA investigation across repeated agent turns. The full technical spec is [docs/DESIGN.md](docs/DESIGN.md). Before changing CLI behavior, output formats, exit codes or the read-only guard, read the relevant section, and update it in the same change.

## Hard constraints

Don't trade these away.

- **Zero runtime dependencies.** The first release runs on Windows x64 workstations (validated on Windows 11) and connects to SQL Server 2025 Standard, usually on a remote server. It needs no ODBC, OLE DB, VC++ redistributable, .NET or OpenSSL. It ships as one exe with the CRT statically linked (`+crt-static`), and imports only DLLs that ship with the supported Windows version. Check with `dumpbin /dependents`. A Windows Server 2016 / SQL Server 2016 floor is a candidate expansion, conditional on acceptable technical debt; it is not a first-release gate.
- **Lean and fast.** Every new crate needs a reason. Check what it adds to exe size (`cargo bloat --release --crates`) and build time, and prefer a few lines of our own code. Stream rows, never buffering a whole result set.
- **Never interactive.** Only `sqlh profile add` reads from the console. With nothing to run and stdin attached to a console, print usage and exit 3. Never wait.
- **No secrets on the command line or in output.** `-P` is rejected on purpose. Passwords come from Windows Credential Manager (through profiles) or `SQLCMDPASSWORD`. Never log, echo or format a password or a connection string that contains one.
- **Output is the product.** Agents read every byte. The formats in DESIGN.md §5 are a contract: stable, unambiguous and short on tokens. Any change needs updated golden tests and an updated DESIGN.md.

## Stack

- **Rust:** stable, pinned in `rust-toolchain.toml` to 1.99.0. Edition 2024, target `x86_64-pc-windows-msvc`. The static CRT is set in `.cargo/config.toml`.
- **TDS driver:** Microsoft [`mssql-tds`](https://github.com/microsoft/mssql-rs), MIT, a preview.
  - Its default features provide SSPI and SChannel TLS, so there is no OpenSSL.
  - Pin an exact version (`=x.y.z`) and read the changelog before bumping.
  - Upstream takes no PRs. File issues there and keep workarounds in our code.
- **Only `src/db/` may use `mssql-tds` types.** All other code uses our own types, so the driver can be upgraded, or swapped for the tiberius-ng fallback, in one place.
- **Other crates:**
  - Tokio with the `current_thread` runtime.
  - `lexopt` for arguments. It's tiny, and sqlcmd-style flags (`-v a=1 b=2`, `-Ns`) are awkward in clap.
  - `windows-sys` for Win32 calls. It's already a transitive dependency.

## Layout

```
src/main.rs      orchestration: sources → batches → guard → connect → run → summary line
src/cli.rs       sqlcmd-compatible flags plus our long flags (lexopt); `profile` subcommands
src/config.rs    profiles.toml (own strict parser) and settings resolution/precedence
src/credman.rs   Windows Credential Manager (CredReadW/CredWriteW/CredDeleteW)
src/profile.rs   sqlh profile add | list | test | rm
src/help.txt     --help text (models read it)
src/error.rs     exit codes (DESIGN.md §6) and Failure
src/console.rs   UTF-8 console output, echo-off password prompt
src/files.rs     -o files: identity (refusing an input file) and the run's own .partial file
src/script.rs    input decoding, GO splitting, $(var) substitution, line mapping
src/guard.rs     read-only classifier (own T-SQL tokenizer)
src/db/          the only mssql-tds user: connect, run a batch into a BatchSink, value/type rendering
src/output/      Output trait + Tally (shared limits/counts); text.rs (default), json.rs, csv.rs
tests/cli.rs     process-level tests of the real binary; `live` ones are ignored unless asked for
tests/fixtures/  creates/drops the sqlh_test scratch database the live tests use
tests/smoke.sql  read-only end-to-end script for a real server (manual)
README.md        human-facing intro, build, quick start, safety note
docs/VISION.md   product direction and priorities (the owner's); it takes precedence over older plans
docs/PLAN.md     the plan of attack: review triage, phases, owner decisions
docs/DESIGN.md   the spec;  docs/notes/  findings (M0 driver spike, connect latency)
docs/schema-commands.md, docs/mcp-mode.md   designs and plans for later phases
code-review.md   the architecture review the plan responds to
code-review-phase1.md   the follow-up review of Phase 1 (R01–R05)
```

Planned, not built: `src/schema.rs` (subcommands) and an md format in `src/output/`.

## Commands

**Setup (done on this machine):**
- Rust comes from rustup (`winget install Rustlang.Rustup`) and lives in `%USERPROFILE%\.cargo\bin`. Shells started before the install need that on PATH.
- Linking needs two VS 2026 components: "MSVC x64/x86 build tools" (`Microsoft.VisualStudio.Component.VC.Tools.x86.x64`) and "Windows 11 SDK" (`Microsoft.VisualStudio.Component.Windows11SDK.26100`).

**Day to day:**

```powershell
cargo build --release                               # ~2 MB exe; full LTO takes ~1–2 min
cargo test                                          # unit + process-level tests; no server needed
cargo test --release --test cli                     # process-level tests against the release exe
cargo build --release --features timing             # --verbose also times each connect step (dev builds; +270 KB)
$env:SQLH_TEST_PROFILE='test'; cargo test --test cli -- --ignored live   # live tests (scratch db sqlh_test)
cargo test -- --ignored credman                     # real Credential Manager round trip (cleans up)
cargo clippy --all-targets -- -D warnings
cargo fmt
target\release\sqlh.exe --profile test -i tests\smoke.sql --continue-on-error   # end-to-end; expect exit 1
```

The test server is the owner's, reached through a profile named `test`. The owner creates it with `sqlh profile add test ...`, which prompts for the password. Never ask for the password, and never put it in a command, file or environment variable. To try profile commands without touching the real profiles, set `SQLH_CONFIG` to a scratch file.

`dumpbin` isn't on PATH. Find it under `C:\Program Files\Microsoft Visual Studio\18\Enterprise\VC\Tools\MSVC\*\bin\Hostx64\x64\`. `dumpbin /dependents target\release\sqlh.exe` must list only in-box Windows DLLs.

## Conventions

### Repository housekeeping

- Commit source, tests and synthetic fixtures, Cargo.lock, build configuration, shared agent instructions, and useful design, plan and review documents. Early-development notes are worth keeping when they record decisions or findings.
- Put disposable experiments in `scratch/` and local query results, logs and copied executables in `artifacts/`. Both root directories are ignored. Promote useful work into source, tests or docs before relying on it.
- Keep real server data, connection profiles, credentials and personal agent state out of commits. Do not blanket-ignore SQL, JSON or CSV: those formats can also hold legitimate fixtures and examples.
- Keep `target/` as a local build cache during active development. Run `cargo clean` when reclaiming disk space or troubleshooting build artifacts; the next build must recreate them. Do not routinely clean between agent turns.
- Before committing, inspect `git status --short`, `git diff --cached --stat`, and `git diff --cached`. Use `git ls-files --cached --ignored --exclude-standard` to catch previously tracked files that ignore rules do not remove. Review new files as well as edits.
- Preserve useful development history. Move obsolete documents to a clearly named archive under `docs/` only when they are superseded, and update references in the same change. Before a public push, review the history as well as the current tree for private data.

### Code conventions

- **Errors:** one error enum in `src/error.rs`, mapped to the exit codes in DESIGN.md §6. No `unwrap` or `expect` on anything derived from user input, the network or the server.
- **unsafe:** only in the FFI modules (`credman.rs`, `console.rs`, `files.rs`), with a `// SAFETY:` comment on each block.
- **Driver logging:** never install a general logger (an `env_logger`, a tracing subscriber, a `log::Log` that prints) while mssql-tds runs. At debug level it dumps TDS packets before encryption, including the login packet and its password. `db::phases`, built only with `--features timing`, keeps timestamps of a few events and never their text.
- **Catalog SQL** must be validated on the first-release target, SQL Server 2025 Standard. Prefer inexpensive portability toward the candidate SQL Server 2016 floor, but there is no unconditional SQL Server 2012 or Azure requirement. Additional version-specific paths need a supported-target decision and a maintenance justification.
- **Tests:**
  - The guard, splitter, renderer and formatter get table-driven unit tests next to the code.
  - Every new output shape gets an exact-output test (see `output/text.rs`).
  - Process-level behavior (exit codes, files, crashes) gets a test in `tests/cli.rs`, isolated from the machine's profiles and `SQLCMD*` variables.
  - Live tests are `#[ignore]`d, so skipping shows; asked for without `SQLH_TEST_PROFILE`, they fail. Before writing, they check the scratch database's `dbo.sqlh_scratch` marker, then write only to tables of their own, which they drop.
  - Test server details and credentials never go in the repo.
- **`--help`:** models read it, so keep it example-first and under about 60 lines.

## Status

The MVP is built; DESIGN.md §13 lists what's in and out. Git repo on `main`.

Profiles and Credential Manager are built, and so are the json and csv formats. The smoke test passes against the test server (SQL Server 2025); remaining unverified items are in `docs/notes/m0-driver-spike.md`.

The plan of attack is [docs/PLAN.md](docs/PLAN.md), derived from [docs/VISION.md](docs/VISION.md) and the triage of [code-review.md](code-review.md). Owner decisions are recorded in its §6. Work in order:
1. Phase 1, correctness and trustworthy outcomes: built and tested, including the manual Git Bash password check (PLAN.md 1.9) and the fixes from the follow-up review [code-review-phase1.md](code-review-phase1.md) (PLAN.md, Phase 1 follow-up).
2. Phase 2, fast, bounded, cancellable turns. 2.1, the connect-latency investigation, is done: most of each connect is SQL Server 2025's password hashing for SQL logins ([docs/notes/connect-latency.md](docs/notes/connect-latency.md)). Next come the release requirements 2.2–2.4: the response budget, a stalled reader, and cancellation.
3. Phase 3, execution core, release checks and live fixtures.
4. Phases 4–8: schema discovery, connection reuse, command categorization, evidence-based evaluations, delivery to agents (skill, MCP connector, plugin).
