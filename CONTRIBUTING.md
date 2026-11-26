# Contributing

## Reporting a problem

Open an issue for a bug, a confusing result or a question. Include:

- `sqlh --version`;
- the SQL Server version (`sqlh info` shows it);
- the command you ran;
- the output, or at least its last line and the exit code.

Remove server names, logins and data you don't want public. Security problems go
through `SECURITY.md`, not an issue.

## Changes

- Open an issue first for changes to CLI behavior, output formats, the read-only
  guard or other substantial features. A small, focused bug fix can go straight to
  a pull request.
- Keep changes focused, and explain the problem and the intended behavior.
- Update the documentation and tests when behavior changes. The output format is a
  contract (`docs/DESIGN.md` §5): a change to it needs updated golden tests and an
  updated spec.
- Don't include credentials, connection strings, real server names, production
  data, or local profile or configuration files.
- A new dependency needs a clear reason and a permissive license compatible with
  the project's MIT license.

The project is built largely by AI agents under the maintainer's direction, and
[AGENTS.md](AGENTS.md) holds the rules they follow. A human contributor is held to
the same rules.

## Building and testing

The Rust toolchain is pinned in `rust-toolchain.toml`. On Windows, run the same
checks CI runs:

```powershell
pwsh scripts/check.ps1
```

This runs the checks that don't need a SQL Server. Live tests are ignored by
default and aren't required for an ordinary contribution.

Builds go in the ignored `target/` directory. `cargo clean` frees the space, and the
next build is slower. `scratch/` and `artifacts/` are ignored too, for experiments
and local results. Real server data and connection profiles stay local, and
committed examples use synthetic data.
