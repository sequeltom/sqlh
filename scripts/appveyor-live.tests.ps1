# R59's regression: appveyor-live.ps1's instance loop against mocked services and
# executables, so no service, profile or server is touched. scripts/check.ps1 runs it in
# Windows PowerShell 5.1, which AppVeyor's `ps:` uses. Exits 1 with what differed.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'appveyor-live.ps1')

$script:events = New-Object System.Collections.Generic.List[string]
$script:startFails = @('MSSQL$SQLA')
$script:stopFails = @('MSSQL$SQLC')
$script:liveExit = @{ ci_sqlb = 1 }

# Functions take precedence over the cmdlets of the same name.
function Start-Service([string]$Name) {
    $script:events.Add("start $Name")
    if ($script:startFails -contains $Name) { throw "cannot start $Name" }
}
function Stop-Service([string]$Name, [switch]$Force) {
    $script:events.Add("stop $Name")
    if ($script:stopFails -contains $Name) { throw "cannot stop $Name" }
}
function Get-Service([string]$Name) { [pscustomobject]@{ Status = 'Stopped' } }

$exe = {
    $script:events.Add("sqlh $($args[1])")
    'sqlh output, not a result'
    $global:LASTEXITCODE = 0
}
$tests = {
    $script:events.Add("tests $env:SQLH_TEST_PROFILE")
    'test output, not a result'
    $code = $script:liveExit[$env:SQLH_TEST_PROFILE]
    $global:LASTEXITCODE = if ($code) { $code } else { 0 }
}

$config = Join-Path ([IO.Path]::GetTempPath()) "sqlh-appveyor-mock-$PID.toml"
try {
    $failures = @(Invoke-Instances @('SQLA', 'SQLB', 'SQLMISSING', 'SQLD', 'SQLC', 'SQLE') `
            @('SQLA', 'SQLB', 'SQLC', 'SQLD', 'SQLE') $exe $tests $config 6>$null)
}
finally {
    Remove-Item $config -ErrorAction SilentlyContinue
}

$expectedFailures = @(
    'SQLA (start): cannot start MSSQL$SQLA'
    'SQLB (live tests): exit 1'
    'SQLMISSING (not installed)'
    'SQLC (stop): cannot stop MSSQL$SQLC'
    "SQLE (skipped): SQLC didn't stop, and every instance listens on 1433"
)
# A startup failure still tries the stop; a test failure stops its instance and goes on; a
# missing instance is skipped; after a failed stop nothing else is started.
$expectedEvents = @(
    'start MSSQL$SQLA', 'stop MSSQL$SQLA'
    'start MSSQL$SQLB', 'sqlh ci_sqlb', 'sqlh ci_sqlb', 'tests ci_sqlb', 'stop MSSQL$SQLB'
    'start MSSQL$SQLD', 'sqlh ci_sqld', 'sqlh ci_sqld', 'tests ci_sqld', 'stop MSSQL$SQLD'
    'start MSSQL$SQLC', 'sqlh ci_sqlc', 'sqlh ci_sqlc', 'tests ci_sqlc', 'stop MSSQL$SQLC'
)

$wrong = @()
if (($failures -join "`n") -cne ($expectedFailures -join "`n")) {
    $wrong += "failures:`n  $($failures -join "`n  ")`nexpected:`n  $($expectedFailures -join "`n  ")"
}
if (($script:events -join "`n") -cne ($expectedEvents -join "`n")) {
    $wrong += "events:`n  $($script:events -join "`n  ")`nexpected:`n  $($expectedEvents -join "`n  ")"
}
if ($wrong) {
    Write-Host ($wrong -join "`n") -ForegroundColor Red
    exit 1
}
Write-Host 'appveyor-live.ps1: instance failures are collected, stops tried, conflicts skipped'
exit 0
