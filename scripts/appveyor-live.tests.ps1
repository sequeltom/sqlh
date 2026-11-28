# R59's regression and finding A's wait: appveyor-live.ps1's reading of SQL Browser's
# answers, and its instance loop against mocked services, Browser asks and executables, so no
# service, profile or server is touched. scripts/check.ps1 runs it in
# Windows PowerShell 5.1, which AppVeyor's `ps:` uses. Exits 1 with what differed.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'appveyor-live.ps1')

$wrong = @()

# SQL Browser's answers, as Get-BrowserTcpPort reads them.
function Reply([string]$Text, [int]$First = 5, [int]$Short = 0) {
    $data = [Text.Encoding]::ASCII.GetBytes($Text)
    $size = $data.Length + $Short
    [byte[]](@($First, ($size -band 255), ($size -shr 8)) + $data)
}
$answer = 'ServerName;APPVEYOR-VM;InstanceName;SQL2025;IsClustered;No;Version;17.0.1000.7;tcp;49712;np;\\APPVEYOR-VM\pipe\MSSQL$SQL2025\sql\query;;'
$replies = @(
    @{ Case = 'the instance'; Reply = (Reply $answer); Port = 49712 }
    @{ Case = 'the name in another case'; Reply = (Reply ($answer -replace 'SQL2025;Is', 'sql2025;Is')); Port = 49712 }
    @{ Case = 'another instance'; Reply = (Reply ($answer -replace 'SQL2025;Is', 'SQL2022;Is')); Port = $null }
    @{ Case = 'no tcp'; Reply = (Reply ($answer -replace 'tcp;49712;', '')); Port = $null }
    @{ Case = 'a tcp that is not a port'; Reply = (Reply ($answer -replace '49712', '70000')); Port = $null }
    @{ Case = 'not SVR_RESP'; Reply = (Reply $answer -First 4); Port = $null }
    @{ Case = 'shorter than its length'; Reply = (Reply $answer -Short 1); Port = $null }
    @{ Case = 'nothing'; Reply = [byte[]]@(); Port = $null }
)
foreach ($r in $replies) {
    $port = Get-BrowserTcpPort $r.Reply 'SQL2025'
    if ("$port" -cne "$($r.Port)") { $wrong += "Get-BrowserTcpPort, $($r.Case): $port, expected $($r.Port)" }
}

# The real ask, to a fake Browser on a loopback port of its own: a runspace that reads one ask
# and sends $Reply, if any. Returns the port Get-InstancePort found and the ask the fake read.
function Ask-FakeBrowser([string]$Instance, [byte[]]$Reply) {
    $browser = New-Object Net.Sockets.UdpClient (New-Object Net.IPEndPoint ([Net.IPAddress]::Loopback, 0))
    $fake = [powershell]::Create().AddScript({
            param($udp, $reply)
            $udp.Client.ReceiveTimeout = 5000
            $from = New-Object Net.IPEndPoint ([Net.IPAddress]::Any, 0)
            $ask = $udp.Receive([ref]$from)
            if ($reply) { [void]$udp.Send($reply, $reply.Length, $from) }
            , $ask
        }).AddArgument($browser).AddArgument($Reply)
    try {
        $running = $fake.BeginInvoke()
        $port = Get-InstancePort $Instance '127.0.0.1' $browser.Client.LocalEndPoint.Port
        $ask = @($fake.EndInvoke($running))
        # As hex: PowerShell's string comparisons are culture-aware and ignore a NUL.
        @{ Port = $port; Ask = $(if ($ask) { [BitConverter]::ToString($ask[0]) }) }
    }
    finally {
        $fake.Dispose()
        $browser.Close()
    }
}
$listener = New-Object Net.Sockets.TcpListener ([Net.IPAddress]::Loopback, 0)
$closed = New-Object Net.Sockets.TcpListener ([Net.IPAddress]::Loopback, 0)
try {
    $listener.Start()
    $closed.Start()
    $open = $listener.LocalEndpoint.Port
    $refused = $closed.LocalEndpoint.Port
    $closed.Stop()
    $asks = @(
        @{ Case = 'an answer with a listening port'; Reply = (Reply ($answer -replace '49712', $open)); Port = $open }
        @{ Case = 'an answer with a port that refuses'; Reply = (Reply ($answer -replace '49712', $refused)); Port = $null }
        @{ Case = 'no answer'; Reply = $null; Port = $null }
    )
    foreach ($a in $asks) {
        $result = Ask-FakeBrowser 'SQL2025' $a.Reply
        if ("$($result.Port)" -cne "$($a.Port)") { $wrong += "Get-InstancePort, $($a.Case): $($result.Port), expected $($a.Port)" }
        # CLNT_UCAST_INST: 0x04, the name, a NUL.
        if ($result.Ask -cne '04-53-51-4C-32-30-32-35-00') {
            $wrong += "Get-InstancePort, $($a.Case): asked $($result.Ask), not CLNT_UCAST_INST for SQL2025"
        }
    }
}
finally {
    $listener.Stop()
}

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
# SQLD is reported on its third ask; SQLR never is.
$script:asks = @{}
$ReadyTries = 3
function Get-InstancePort([string]$Instance) {
    $script:asks[$Instance] = 1 + [int]$script:asks[$Instance]
    $script:events.Add("ask $Instance")
    if ($Instance -eq 'SQLR' -or ($Instance -eq 'SQLD' -and $script:asks[$Instance] -lt 3)) { return $null }
    1433
}
function Start-Sleep([int]$Seconds) { $script:events.Add("sleep $Seconds") }

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
    $failures = @(Invoke-Instances @('SQLA', 'SQLB', 'SQLMISSING', 'SQLD', 'SQLR', 'SQLC', 'SQLE') `
            @('SQLA', 'SQLB', 'SQLC', 'SQLD', 'SQLE', 'SQLR') $exe $tests $config 6>$null)
}
finally {
    Remove-Item $config -ErrorAction SilentlyContinue
}

$expectedFailures = @(
    'SQLA (start): cannot start MSSQL$SQLA'
    'SQLB (live tests): exit 1'
    'SQLMISSING (not installed)'
    "SQLR (ready): SQL Browser didn't report it on a port that takes a connection in 3 tries a second apart"
    'SQLC (stop): cannot stop MSSQL$SQLC'
    "SQLE (skipped): SQLC didn't stop, and every instance listens on 1433"
)
# A startup failure still tries the stop; a test failure stops its instance and goes on; a
# missing instance is skipped; nothing connects before Browser reports the instance, and an
# instance it never reports is stopped after the last ask, with no wait after it; after a
# failed stop nothing else is started.
$expectedEvents = @(
    'start MSSQL$SQLA', 'stop MSSQL$SQLA'
    'start MSSQL$SQLB', 'ask SQLB', 'sqlh ci_sqlb', 'sqlh ci_sqlb', 'tests ci_sqlb', 'stop MSSQL$SQLB'
    'start MSSQL$SQLD', 'ask SQLD', 'sleep 1', 'ask SQLD', 'sleep 1', 'ask SQLD'
    'sqlh ci_sqld', 'sqlh ci_sqld', 'tests ci_sqld', 'stop MSSQL$SQLD'
    'start MSSQL$SQLR', 'ask SQLR', 'sleep 1', 'ask SQLR', 'sleep 1', 'ask SQLR', 'stop MSSQL$SQLR'
    'start MSSQL$SQLC', 'ask SQLC', 'sqlh ci_sqlc', 'sqlh ci_sqlc', 'tests ci_sqlc', 'stop MSSQL$SQLC'
)

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
Write-Host 'appveyor-live.ps1: Browser answers read, connects wait for Browser, instance failures collected, stops tried, conflicts skipped'
exit 0
