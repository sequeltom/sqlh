# M0 driver spike: mssql-tds 0.2.0

Status: **go** (2026-10-09). The build, size, import and live-server checks pass against SQL Server 2025. This is a historical record; what remains to verify for the first release is tracked in [PLAN.md §7](../PLAN.md).

## Confirmed

- **Builds cleanly** on Rust 1.99.0 (`x86_64-pc-windows-msvc`, static CRT). It needs the VS "MSVC x64/x86 build tools" and "Windows 11 SDK" components.
- **Release exe is about 2.1 MB** with `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` and `strip`. That's well under the 6 MB budget, even though mssql-tds pulls in tokio `full`, x509-parser, bigdecimal and tracing.
- **Imports are all in-box:** KERNEL32, ntdll, advapi32, ws2_32, secur32, crypt32, bcrypt, bcryptprimitives, oleaut32, and the `api-ms-win-core-synch-l1-2-0` API set (WaitOnAddress, Windows 8+). There's no vcruntime and no OpenSSL. Re-check with `dumpbin /dependents` after any dependency change.
- **The API covers everything the MVP needs:**
  - `TdsClient::execute` with `ExecuteOptions { timeout, cancel, on_error: BatchErrorMode::Continue }` returns statement errors in order alongside rows (the ODBC `SQLMoreResults` model).
  - `advance()` walks the statements.
  - `StatementResult::NoRows { rows_affected }` reports DML counts.
  - `take_info_messages()` returns PRINT and severity ≤ 10 messages. They're kept apart from errors, so we drain them after each step to keep the order.
  - `has_active_transaction()` drives the read-only backstop check.
  - `CancelHandle` child handles give us Ctrl+C cancellation.

## Live results: SQL Server 2025 (17.0.1000.7, Developer), over TCP from Windows 11

- **SQL login** works through a profile with the password in Credential Manager. The connection is TLS-encrypted, and connect plus login plus one query takes about 150–220 ms on the LAN.
- **The server's certificate is self-signed**, and we connect by IP, so `-C` (trust it) is needed.
- **Every type in `tests/smoke.sql` renders as DESIGN.md §5.1 specifies:**
  - numbers are exact, including money and decimal
  - `nvarchar(n)` lengths halve correctly
  - `sql_variant`, `xml`, binary, GUID and truncation are right
  - the native 2025 `json` and `vector` types decode
  - `time`, `datetime2` and `datetimeoffset` are right after a fix (see below), including 0001-01-01, 9999-12-31 and negative offsets
- **Execution behavior is correct:**
  - PRINT and RAISERROR come in order
  - `--max-rows` shows "5 of 2775"
  - `GO 2` works, and temp-table writes run under the read-only guard
  - the wrapper transaction is real: `@@TRANCOUNT` is 1 in read-only runs and 0 with `--allow-write`
  - error lines map back to the right lines in `tests/smoke.sql`
- **Query timeout works:** `-t 2` on a slow recursive CTE exits 4 after 2.2 s.
- **Throughput:** 299k `Users` rows with every column, including `nvarchar(max)`, streamed to NUL in 1.9 s. With the default limit, draining and counting them took 1.3 s.

## Gotchas found

- `ClientContext::default()` sets encryption to **Strict** (TDS 8). We always set the mode explicitly; our default is Mandatory.
- **`SqlTime::time_nanoseconds` holds 100 ns ticks, not nanoseconds.** The decoder normalizes every scale to 10^-7 s. Treating it as nanoseconds made times about 100× too small, which the live smoke test caught.
- `datetimeoffset` arrives as UTC plus an offset. We add the offset to show local time, the way SSMS does. Verified live.
- `SqlXml::as_string()` and `SqlJson::as_string()` call `unwrap()` on decoding. We decode lossily ourselves, because with `panic = "abort"` a bad value would kill the process.
- Money comes as two mixed-endian i32 halves. The crate's own conversion goes through `f64`, so we rebuild the exact i64 ourselves.
- **TLS failures on the direct Schannel path** arrive as `ImplementationError("Schannel TLS handshake failed: ... SEC_E_UNTRUSTED_ROOT ...")`, not as `TlsError`. Our connection hints therefore match on SSPI status names in the text.
- **`CONNECTIONPROPERTY('encrypt_option')` doesn't exist** and returns NULL. Encryption comes from the client side (`TdsClient::is_encrypted`).
- **`SET NOEXEC ON` (`--check`)** reports syntax errors and unknown columns, but not missing tables, because of deferred name resolution.
- Type names for headers come from `TdsDataType` plus `type_info.length`. Nullable wire types (`IntN`, `FltN`, `MoneyN`, `DateTimeN`) are resolved by length. `vector` has no dimension in the header yet.
- The upstream repo's `main` is already at 0.3.0, while crates.io has 0.2.0. Read the changelog before bumping; the API moves.

## Still to verify

Superseded by the first-release checklist in [PLAN.md §7](../PLAN.md). The first release runs on Windows 11 workstations against a remote SQL Server 2025 (VISION.md), so the Server 2016, SQL Server 2012/2014 and Azure checks once listed here are now compatibility assessments, not release gates. Windows auth (SSPI) and Ctrl+C mid-query still need verifying.
