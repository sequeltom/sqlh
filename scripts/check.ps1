<#
.SYNOPSIS
    The local release check (plan-history.md 3.3): everything a change must pass, in one
    command.

.DESCRIPTION
    Runs, in order, and stops at the first failure with a non-zero exit code:
      1. Cargo.lock is current: cargo metadata --locked
      2. cargo fmt --check, then scripts/appveyor-live.tests.ps1 (mocked; no service touched)
      3. cargo clippy --all-targets -- -D warnings, without and with --features timing
      4. cargo test (unit and process-level tests; no server needed)
      5. cargo build --release
      6. cargo test --release --test cli (the process-level tests against the release exe)
      7. the size budget: target\release\sqlh.exe, unsigned, at most 3 MB (owner, 2026-10-12;
         decision 97), built with the cargo registry's path remapped and none of it left in it
      8. the imports: dumpbin /dependents lists only DLLs that ship with Windows
    Every cargo command that resolves dependencies runs with --locked, so none of them can
    update a stale Cargo.lock and let a later step pass on it (R23): a stale lockfile fails
    step 1 and is left as it was.
    With -Live it then runs the live tests (debug and release) through the profile named in
    $env:SQLH_TEST_PROFILE, or "test". CI (plan-history.md 3.5) runs it without -Live: no
    server there.

.PARAMETER Live
    Also run the live tests against the test server (scratch database sqlh_test).

.EXAMPLE
    pwsh scripts/check.ps1
    pwsh scripts/check.ps1 -Live
#>
[CmdletBinding()]
param(
    [switch]$Live
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# The size budget for the release exe, in bytes (3 MB; 2.42 MB on 2026-10-12): the unsigned exe
# this check builds. Signing appends a signature, so the signed exe is larger (decision 97).
$sizeBudget = 3 * 1024 * 1024

# Decision 97: dependencies' panic locations embed their source paths, under the builder's
# cargo registry, which names the user's folder (GitHub's runner made the exe 1,024 bytes larger
# than a local build). Remapped to `crates`, the exe has no registry prefix in it, and the local
# and CI builds measured the same size; other inputs (the version, the toolchain) can still
# differ between machines. Set for every cargo command here, debug ones too, so that no step builds
# the exe with other flags; on GitHub it is passed on to the job's later steps (GITHUB_ENV), so
# release.yml's own `cargo test --release --no-run` doesn't rebuild the exe it stages. Cargo
# adds it to .cargo/config.toml's `+crt-static` and splits it at spaces, so the path can't hold
# one.
# The registry may not exist yet: a fresh GitHub runner fills it at step 1, and the remap
# doesn't need the folder.
$registry = Join-Path ($env:CARGO_HOME ?? (Join-Path $env:USERPROFILE '.cargo')) 'registry\src'
if ($registry -match '\s') {
    Write-Host "check failed: the cargo registry's path holds a space, which a remapped flag can't ($registry); set CARGO_HOME to a path without one" -ForegroundColor Red
    exit 1
}
$remap = "--remap-path-prefix=$registry=crates"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = $remap
if ($env:GITHUB_ENV) {
    Add-Content -Path $env:GITHUB_ENV -Value "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS=$remap"
}

# The DLLs sqlh may import: all ship with every supported Windows version (AGENTS.md, zero
# runtime dependencies). A new import fails the check until it is shown to be in-box and
# added here.
$inBox = @(
    'kernel32.dll', 'ntdll.dll', 'api-ms-win-core-synch-l1-2-0.dll', 'bcryptprimitives.dll',
    'bcrypt.dll', 'ws2_32.dll', 'secur32.dll', 'crypt32.dll', 'advapi32.dll', 'oleaut32.dll'
)

$started = Get-Date
$step = 0

function Step([string]$name, [scriptblock]$action) {
    $script:step++
    Write-Host ("[{0}] {1}" -f $script:step, $name) -ForegroundColor Cyan
    & $action
    if ($LASTEXITCODE -ne 0) {
        Write-Host ("check failed at step {0}: {1} (exit {2})" -f $script:step, $name, $LASTEXITCODE) -ForegroundColor Red
        exit 1
    }
}

function Fail([string]$message) {
    Write-Host "check failed at step ${script:step}: $message" -ForegroundColor Red
    exit 1
}

Step 'Cargo.lock is current' { cargo metadata --locked --format-version 1 | Out-Null }
Step 'cargo fmt --check' { cargo fmt --check }
# appveyor-live.ps1's instance loop against mocks, in Windows PowerShell 5.1 as AppVeyor runs
# it (R59): failures collected by stage, stops tried, no instance started after a failed stop.
Step 'AppVeyor script (mocked)' {
    $out = powershell -NoProfile -ExecutionPolicy Bypass -File scripts/appveyor-live.tests.ps1 2>&1
    if ($LASTEXITCODE) { $out | Write-Host } else { $out | Select-Object -Last 1 | Write-Host }
}
Step 'cargo clippy' { cargo clippy --locked --all-targets -- -D warnings }
Step 'cargo clippy --features timing' { cargo clippy --locked --all-targets --features timing -- -D warnings }
Step 'cargo test' { cargo test --locked }
Step 'cargo build --release' { cargo build --release --locked }
Step 'cargo test --release --test cli' { cargo test --locked --release --test cli }

$exe = Join-Path $root 'target\release\sqlh.exe'

$step++
Write-Host ("[{0}] size budget" -f $step) -ForegroundColor Cyan
$size = (Get-Item $exe).Length
Write-Host ("    {0:N0} bytes of {1:N0}" -f $size, $sizeBudget)
if ($size -gt $sizeBudget) {
    Fail ("sqlh.exe is {0:N0} bytes, over the {1:N0}-byte budget; see what grew with cargo bloat --release --crates" -f $size, $sizeBudget)
}
# The remap held (decision 97): no registry path is left in the exe, however its folder is
# spelled (rustc writes them as UTF-8), and the remapped ones are there.
$text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($exe))
if ($text.Contains('\registry\src\index.crates.io-') -or -not $text.Contains('crates\index.crates.io-')) {
    Fail "sqlh.exe holds cargo registry paths that weren't remapped; the remap above didn't reach its build"
}

$step++
Write-Host ("[{0}] imports" -f $step) -ForegroundColor Cyan
# dumpbin isn't on PATH: find the newest MSVC toolset of any Visual Studio installation.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$dumpbin = $null
if (Test-Path $vswhere) {
    $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($vs) {
        $dumpbin = Get-ChildItem (Join-Path $vs 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe') -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending | Select-Object -First 1
    }
}
if (-not $dumpbin) {
    Fail 'dumpbin.exe was not found (it comes with the Visual Studio "MSVC x64/x86 build tools" component)'
}
$imports = & $dumpbin.FullName /nologo /dependents $exe |
    ForEach-Object { $_.Trim().ToLowerInvariant() } |
    Where-Object { $_ -like '*.dll' }
if ($LASTEXITCODE -ne 0 -or -not $imports) {
    Fail 'dumpbin /dependents failed or listed no DLLs'
}
$unexpected = @($imports | Where-Object { $inBox -notcontains $_ })
Write-Host ("    {0} imports: {1}" -f @($imports).Count, ($imports -join ', '))
if ($unexpected.Count -gt 0) {
    Fail ("sqlh.exe imports DLLs that aren't on the in-box list: {0}" -f ($unexpected -join ', '))
}

if ($Live) {
    if (-not $env:SQLH_TEST_PROFILE) { $env:SQLH_TEST_PROFILE = 'test' }
    Step "live tests (debug, profile $env:SQLH_TEST_PROFILE)" { cargo test --locked --test cli -- --ignored live }
    Step "live tests (release, profile $env:SQLH_TEST_PROFILE)" { cargo test --locked --release --test cli -- --ignored live }
}

Write-Host ("check passed in {0:N0} s" -f ((Get-Date) - $started).TotalSeconds) -ForegroundColor Green
exit 0
