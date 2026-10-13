<#
.SYNOPSIS
    The local release check (PLAN.md 3.3): everything a change must pass, in one command.

.DESCRIPTION
    Runs, in order, and stops at the first failure with a non-zero exit code:
      1. cargo fmt --check
      2. cargo clippy --all-targets -- -D warnings, without and with --features timing
      3. cargo test (unit and process-level tests; no server needed)
      4. cargo build --release --locked
      5. cargo test --release --test cli (the process-level tests against the release exe)
      6. the size budget: target\release\sqlh.exe at most 3 MB (owner, 2026-10-12)
      7. the imports: dumpbin /dependents lists only DLLs that ship with Windows
    With -Live it then runs the live tests (debug and release) through the profile named in
    $env:SQLH_TEST_PROFILE, or "test". CI (PLAN.md 3.5) runs it without -Live: no server there.

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

# The size budget for the release exe, in bytes (3 MB; 2.42 MB on 2026-10-12).
$sizeBudget = 3 * 1024 * 1024

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

Step 'cargo fmt --check' { cargo fmt --check }
Step 'cargo clippy' { cargo clippy --all-targets -- -D warnings }
Step 'cargo clippy --features timing' { cargo clippy --all-targets --features timing -- -D warnings }
Step 'cargo test' { cargo test }
Step 'cargo build --release --locked' { cargo build --release --locked }
Step 'cargo test --release --test cli' { cargo test --release --test cli }

$exe = Join-Path $root 'target\release\sqlh.exe'

$step++
Write-Host ("[{0}] size budget" -f $step) -ForegroundColor Cyan
$size = (Get-Item $exe).Length
Write-Host ("    {0:N0} bytes of {1:N0}" -f $size, $sizeBudget)
if ($size -gt $sizeBudget) {
    Fail ("sqlh.exe is {0:N0} bytes, over the {1:N0}-byte budget; see what grew with cargo bloat --release --crates" -f $size, $sizeBudget)
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
    Step "live tests (debug, profile $env:SQLH_TEST_PROFILE)" { cargo test --test cli -- --ignored live }
    Step "live tests (release, profile $env:SQLH_TEST_PROFILE)" { cargo test --release --test cli -- --ignored live }
}

Write-Host ("check passed in {0:N0} s" -f ((Get-Date) - $started).TotalSeconds) -ForegroundColor Green
exit 0
