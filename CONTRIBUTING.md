# Contributing

sqlh is pre-release software. The project is not accepting public issues or
pull requests until after its first release. This restriction does not apply
to confidential security reports; see `SECURITY.md`.

## After contributions reopen

- Open an issue first for changes to CLI behavior, output formats, safety
  policy, or other substantial features. Small, focused bug fixes can go
  directly to a pull request.
- Keep changes focused and explain the problem and intended behavior.
- Update the relevant documentation and tests when behavior changes. Output
  changes must preserve the contract documented in `docs/DESIGN.md`.
- Do not include credentials, connection strings, real server names,
  production data, or local profile/configuration files.
- New dependencies need a clear reason and a permissive license compatible
  with the project's MIT license.

The Rust toolchain is pinned in `rust-toolchain.toml`. On Windows, run the
same checks used by CI:

```powershell
pwsh scripts/check.ps1
```

This runs the checks that do not require a SQL Server. Live tests are ignored
by default and are not required for an ordinary contribution.
