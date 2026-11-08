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
       Each zip must match the digest GitHub recorded for it. Nothing is compiled here. The
       token is then removed from the environment, so no test inherits it.
    2. Checks the exe: a valid Authenticode signature, its SHA256SUMS.txt hash, and that
       `--version` reports the artifact's version.
    3. Runs the process tests once (no server), against the signed exe.
    4. For each instance named in $env:SQLH_CI_INSTANCES (or -Instances), one at a time, since
       every instance on the images listens on 1433 by default (Invoke-Instances): starts it;
       writes a Windows-auth profile for COMPUTERNAME\INSTANCE, a named instance found through
       SQL Browser; prints the server's version and how the login was made; creates the
       scratch database (tests/fixtures/sqlh_test.sql); runs the live tests; stops it. The
       login is real local SSPI through the driver's per-hop provider (db::spn, decision 72),
       with SQL Browser's port. It doesn't exercise the provider's name lookup: the driver
       takes this computer's own name as loopback, which isn't looked up (R61). The live tests
       also make SQL-auth logins of their own (Reader), so the instance needs mixed mode.
    A failure is recorded with its stage and the rest still runs, except that after an
    instance fails to stop, the later ones are skipped (R59). The script exits 1 if anything
    failed. The tests find the signed exe, this checkout and a scratch folder through
    SQLH_TEST_EXE, SQLH_TEST_ROOT and SQLH_TEST_TMPDIR (tests/cli.rs).

    Dot-sourced, it defines its functions and runs nothing (scripts/appveyor-live.tests.ps1).

.PARAMETER Instances
    The instance names, such as SQL2025. Defaults to $env:SQLH_CI_INSTANCES, space-separated.
#>
[CmdletBinding()]
param(
    [string[]]$Instances = @(($env:SQLH_CI_INSTANCES -split ' ') | Where-Object { $_ })
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# One instance, from start to stop. Returns its failures, each naming its stage, and whether
# its service is known to have stopped. Any error, a cmdlet's or an executable's exit code,
# ends the instance's work at its stage; the stop is always tried. `$Exe` and `$TestsExe` are
# paths, or script blocks in the mock test. Their output goes to the host, not into the
# result.
function Test-Instance([string]$Instance, $Exe, $TestsExe, [string]$Config) {
    $failures = @()
    $service = "MSSQL`$$Instance"
    $stage = 'start'
    try {
        Start-Service $service
        $stage = 'connect'
        $name = 'ci_' + $Instance.ToLowerInvariant()
        # sqlh's own subset of TOML (DESIGN.md §9): the backslash is escaped. The server's
        # certificate is the instance's self-signed one.
        $toml = "[profiles.$name]`n" +
            "server = `"$env:COMPUTERNAME\\$Instance`"`n" +
            "auth = `"windows`"`n" +
            "trust_server_certificate = true`n" +
            "write_policy = `"flag`"`n"
        [IO.File]::WriteAllText($Config, $toml)
        & $Exe --profile $name -Q "SELECT @@VERSION AS version, SUSER_SNAME() AS login, IS_SRVROLEMEMBER('sysadmin') AS sysadmin, auth_scheme, net_transport FROM sys.dm_exec_connections WHERE session_id = @@SPID" | Out-Host
        if ($LASTEXITCODE) { throw "exit $LASTEXITCODE" }
        $stage = 'scratch database'
        & $Exe --profile $name --allow-write -i tests/fixtures/sqlh_test.sql | Out-Host
        if ($LASTEXITCODE) { throw "exit $LASTEXITCODE" }
        $stage = 'live tests'
        $env:SQLH_TEST_PROFILE = $name
        & $TestsExe --ignored live | Out-Host
        if ($LASTEXITCODE) { throw "exit $LASTEXITCODE" }
    }
    catch {
        $failures += "$Instance (${stage}): $($_.Exception.Message)"
    }
    $stopped = $false
    try {
        Stop-Service $service -Force
        $status = (Get-Service $service).Status
        $stopped = "$status" -eq 'Stopped'
        if (-not $stopped) { $failures += "$Instance (stop): the service is $status" }
    }
    catch {
        $failures += "$Instance (stop): $($_.Exception.Message)"
    }
    [pscustomobject]@{ Failures = $failures; Stopped = $stopped }
}

# The instances, one at a time. Every instance listens on 1433, so once one hasn't stopped, no
# later one is started: its tests could reach the wrong server (R59).
function Invoke-Instances([string[]]$Instances, [string[]]$Installed, $Exe, $TestsExe, [string]$Config) {
    $failures = @()
    $running = $null
    foreach ($instance in $Instances) {
        Write-Host "== $instance" -ForegroundColor Cyan
        if ($running) {
            $failures += "$instance (skipped): $running didn't stop, and every instance listens on 1433"
            continue
        }
        if ($Installed -notcontains $instance) {
            $failures += "$instance (not installed)"
            continue
        }
        $result = Test-Instance $instance $Exe $TestsExe $Config
        $failures += $result.Failures
        if (-not $result.Stopped) { $running = $instance }
    }
    $failures
}

if ($MyInvocation.InvocationName -eq '.') { return }

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
New-Item -ItemType Directory -Force $work | Out-Null
foreach ($a in $signed[0], $tests[0]) {
    $zip = Join-Path $work "$($a.name).zip"
    # The download redirects to storage, which gets no Authorization header (checked under
    # 5.1 and 7 in the R59-R61 review).
    Invoke-WebRequest -Headers $headers -Uri $a.archive_download_url -OutFile $zip -UseBasicParsing
    # The digest GitHub recorded at upload: the zip arrived whole and is that run's.
    $digest = 'sha256:' + (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if (-not $a.digest -or $digest -ne $a.digest) {
        throw "$($a.name).zip is $digest; the run recorded $($a.digest)"
    }
    Expand-Archive $zip (Join-Path $work $a.name)
}
# Nothing below needs GitHub: what runs next (the tests, the exe) doesn't inherit the token.
$headers = $null
Remove-Item Env:SQLH_GITHUB_TOKEN
$dist = Join-Path $work $signed[0].name
$exe = Join-Path $dist 'sqlh.exe'
$testsExe = Join-Path $work 'sqlh-live-tests\sqlh-live-tests.exe'

# 2. The exe is the release's, signed and whole.
$signature = Get-AuthenticodeSignature $exe
Write-Host "signature: $($signature.Status), $($signature.SignerCertificate.Subject)"
if ($signature.Status -ne 'Valid') { throw "sqlh.exe's signature is $($signature.Status)" }
foreach ($line in Get-Content (Join-Path $dist 'SHA256SUMS.txt')) {
    $hash, $file = $line -split '  ', 2
    $actual = (Get-FileHash (Join-Path $dist $file) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $hash) { throw "$file doesn't match SHA256SUMS.txt" }
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
if ($LASTEXITCODE) { $failed += "process tests: exit $LASTEXITCODE" }

# 4. The live tests, instance by instance.
$installed = @((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Microsoft SQL Server\Instance Names\SQL').PSObject.Properties.Name |
    Where-Object { $_ -notlike 'PS*' })
Write-Host "instances on this image: $($installed -join ', ')"
Set-Service SQLBrowser -StartupType Manual
Start-Service SQLBrowser
$config = Join-Path $env:USERPROFILE '.sqlh\profiles.toml'
New-Item -ItemType Directory -Force (Split-Path $config) | Out-Null
$failed += @(Invoke-Instances $Instances $installed $exe $testsExe $config)

if ($failed) {
    Write-Host "failed: $($failed -join '; ')" -ForegroundColor Red
    exit 1
}
Write-Host "sqlh $version certified on $env:APPVEYOR_BUILD_WORKER_IMAGE against $($Instances -join ', ')" -ForegroundColor Green
exit 0
