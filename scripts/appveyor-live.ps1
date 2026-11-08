<#
.SYNOPSIS
    Release certification on an AppVeyor VM (appveyor.yml; decision 75; PLAN.md §4,
    AppVeyor): the release's signed exe and the tests built with it, against each SQL Server
    instance the image has. Meant for AppVeyor's throwaway VMs: it starts services and
    rewrites the build account's profiles file. Runs in Windows PowerShell 5.1 or later.

.DESCRIPTION
    1. Finds the commit's successful run of GitHub's release workflow (waiting up to 30
       minutes for one still going) and downloads two of its artifacts: sqlh-<version>, the
       signed exe with SHA256SUMS.txt, and sqlh-live-tests, the tests' binary built with it.
       Nothing is compiled here.
    2. Checks the exe: a valid Authenticode signature, its SHA256SUMS.txt hash, and that
       `--version` reports the artifact's version.
    3. Runs the process tests once (no server), against the signed exe.
    4. For each instance named in $env:SQLH_CI_INSTANCES (or -Instances), one at a time, since
       every instance on the images listens on 1433 by default: starts it; writes a
       Windows-auth profile for COMPUTERNAME\INSTANCE (a named instance found through SQL
       Browser, and by the computer's name rather than localhost, so the login takes the SPN
       lookup's path, db::spn, decision 72); prints the server's version and how the login
       was made; creates the scratch database (tests/fixtures/sqlh_test.sql); runs the live
       tests; stops it.
    A failure is recorded and the rest still runs; the script exits 1 if anything failed.
    The tests find the signed exe, this checkout and a scratch folder through SQLH_TEST_EXE,
    SQLH_TEST_ROOT and SQLH_TEST_TMPDIR (tests/cli.rs).

.PARAMETER Instances
    The instance names, such as SQL2025. Defaults to $env:SQLH_CI_INSTANCES, space-separated.
#>
[CmdletBinding()]
param(
    [string[]]$Instances = @(($env:SQLH_CI_INSTANCES -split ' ') | Where-Object { $_ })
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if (-not $Instances) { throw 'no instances: set SQLH_CI_INSTANCES or pass -Instances' }
if (-not $env:SQLH_GITHUB_TOKEN) {
    throw 'SQLH_GITHUB_TOKEN is not set: add it in the AppVeyor project settings as a secure variable (decision 75)'
}
$repo = $env:APPVEYOR_REPO_NAME
$commit = $env:APPVEYOR_REPO_COMMIT
$headers = @{
    Authorization          = "Bearer $env:SQLH_GITHUB_TOKEN"
    Accept                 = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
}

function GitHub([string]$path) {
    Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/repos/$repo/$path"
}

# 1. The release run for this commit, and its two artifacts.
$deadline = (Get-Date).AddMinutes(30)
while ($true) {
    $runs = @((GitHub "actions/workflows/release.yml/runs?head_sha=$commit&per_page=20").workflow_runs)
    $done = @($runs | Where-Object { $_.status -eq 'completed' -and $_.conclusion -eq 'success' } |
        Sort-Object run_number -Descending)
    if ($done) { $run = $done[0]; break }
    $going = @($runs | Where-Object { $_.status -ne 'completed' })
    if (-not $going) {
        throw "no successful release run for $commit in ${repo}: run the release workflow for it first"
    }
    if ((Get-Date) -gt $deadline) { throw "the release run for $commit didn't finish within 30 minutes" }
    Write-Host "waiting for release run $($going[0].run_number) ($($going[0].status))"
    Start-Sleep -Seconds 60
}
Write-Host "release run $($run.run_number): $($run.html_url)"

$artifacts = @((GitHub "actions/runs/$($run.id)/artifacts?per_page=100").artifacts)
$signed = @($artifacts | Where-Object { $_.name -match '^sqlh-\d' })
$tests = @($artifacts | Where-Object { $_.name -eq 'sqlh-live-tests' })
if ($signed.Count -ne 1 -or $tests.Count -ne 1) {
    throw "release run $($run.run_number) has no single signed exe and tests artifact: $(($artifacts | ForEach-Object name) -join ', ')"
}
$version = $signed[0].name.Substring('sqlh-'.Length)

$work = Join-Path $env:TEMP 'sqlh-cert'
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
foreach ($a in $signed[0], $tests[0]) {
    $zip = Join-Path $work "$($a.name).zip"
    New-Item -ItemType Directory -Force $work | Out-Null
    # The download redirects to storage, which gets no Authorization header.
    Invoke-WebRequest -Headers $headers -Uri $a.archive_download_url -OutFile $zip -UseBasicParsing
    Expand-Archive $zip (Join-Path $work $a.name)
}
$dist = Join-Path $work $signed[0].name
$exe = Join-Path $dist 'sqlh.exe'
$testsExe = Join-Path $work 'sqlh-live-tests\sqlh-live-tests.exe'

# 2. The exe is the release's, signed and whole.
$signature = Get-AuthenticodeSignature $exe
Write-Host "signature: $($signature.Status), $($signature.SignerCertificate.Subject)"
if ($signature.Status -ne 'Valid') { throw "sqlh.exe's signature is $($signature.Status)" }
foreach ($line in Get-Content (Join-Path $dist 'SHA256SUMS.txt')) {
    $hash, $name = $line -split '  ', 2
    $actual = (Get-FileHash (Join-Path $dist $name) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $hash) { throw "$name doesn't match SHA256SUMS.txt" }
}
$reported = & $exe --version
Write-Host $reported
if ($reported -notmatch "^sqlh $([regex]::Escape($version)) ") { throw "the exe reports '$reported', not $version" }

$env:SQLH_TEST_EXE = $exe
$env:SQLH_TEST_ROOT = $root
$env:SQLH_TEST_TMPDIR = Join-Path $work 'tmp'
New-Item -ItemType Directory -Force $env:SQLH_TEST_TMPDIR | Out-Null

$failed = @()

# 3. The process tests, against the signed exe.
Write-Host '== process tests' -ForegroundColor Cyan
& $testsExe
if ($LASTEXITCODE) { $failed += 'process tests' }

# 4. The live tests, instance by instance.
$installed = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Microsoft SQL Server\Instance Names\SQL').PSObject.Properties.Name
Write-Host "instances on this image: $($installed -join ', ')"
Set-Service SQLBrowser -StartupType Manual
Start-Service SQLBrowser
$config = Join-Path $env:USERPROFILE '.sqlh\profiles.toml'
New-Item -ItemType Directory -Force (Split-Path $config) | Out-Null

foreach ($instance in $Instances) {
    Write-Host "== $instance" -ForegroundColor Cyan
    if ($installed -notcontains $instance) {
        Write-Host "$instance isn't installed on this image" -ForegroundColor Red
        $failed += "$instance (not installed)"
        continue
    }
    $service = "MSSQL`$$instance"
    Start-Service $service
    try {
        $name = 'ci_' + $instance.ToLowerInvariant()
        # sqlh's own subset of TOML (DESIGN.md §9): the backslash is escaped. The server's
        # certificate is the instance's self-signed one.
        $toml = "[profiles.$name]`n" +
            "server = `"$env:COMPUTERNAME\\$instance`"`n" +
            "auth = `"windows`"`n" +
            "trust_server_certificate = true`n" +
            "write_policy = `"flag`"`n"
        [IO.File]::WriteAllText($config, $toml)

        & $exe --profile $name -Q "SELECT @@VERSION AS version, SUSER_SNAME() AS login, IS_SRVROLEMEMBER('sysadmin') AS sysadmin, auth_scheme, net_transport FROM sys.dm_exec_connections WHERE session_id = @@SPID"
        if ($LASTEXITCODE) { $failed += "$instance (connect, exit $LASTEXITCODE)"; continue }

        & $exe --profile $name --allow-write -i tests/fixtures/sqlh_test.sql
        if ($LASTEXITCODE) { $failed += "$instance (scratch database, exit $LASTEXITCODE)"; continue }

        $env:SQLH_TEST_PROFILE = $name
        & $testsExe --ignored live
        if ($LASTEXITCODE) { $failed += "$instance (live tests)" }
    }
    finally {
        Stop-Service $service -Force
    }
}

if ($failed) {
    Write-Host "failed: $($failed -join '; ')" -ForegroundColor Red
    exit 1
}
Write-Host "sqlh $version certified on $env:APPVEYOR_BUILD_WORKER_IMAGE against $($Instances -join ', ')" -ForegroundColor Green
exit 0
