<#
.SYNOPSIS
    A one-off diagnostic on AppVeyor's VMs for the 0.2.0 certification's two findings (PLAN.md
    §4, AppVeyor, the sixth run): what each instance's DMVs say about a session's open
    transactions while it runs and while it is idle (finding B), and how long an instance takes
    to answer through SQL Browser after it starts (finding A). It doesn't run sqlh: it uses .NET
    Framework's SqlClient from Windows PowerShell 5.1, so sqlh's wrapper can't shape what is
    measured. Meant for AppVeyor's throwaway VMs: it starts services and makes a table in tempdb.

.DESCRIPTION
    For each instance in $env:SQLH_CI_INSTANCES (or -Instances), one at a time:
    1. Starts SQL Browser and the instance, then connects through Browser once a second for up
       to 60 s, printing how long the first success took and the first error (finding A).
    2. Makes tempdb.dbo.sqlh_diag (one row) and holds an exclusive lock on its row from a
       blocker session.
    3. For each case below, a reader session takes the steps the case names; an observer session
       reads, for the reader's session id: sys.dm_exec_sessions.open_transaction_count,
       sys.dm_exec_requests.open_transaction_count and status and wait, and the count of
       sys.dm_tran_session_transactions rows (finding B).
         fresh-running   a new connection: BEGIN TRANSACTION as one batch, then the blocked
                         SELECT as the next (sqlh's wrapper, then the query)
         reset-running   the same on a pooled connection reused after a close, so the
                         BEGIN TRANSACTION batch carries the connection reset (sqlh mcp on reuse)
         one-batch       a new connection: BEGIN TRANSACTION and the blocked SELECT in one batch
         fresh-idle      a new connection after its BEGIN TRANSACTION batch, no request running
         reset-idle      the same on a reused, reset connection
       A running case waits, at most 10 s, until the reader is blocked on a lock, then reads,
       then cancels the reader and rolls it back.
    4. Rolls the blocker back, drops the table and stops the instance.
    Prints a table per instance. Exits 1 if a step failed; the readings are the result, not a
    pass or fail.
#>
[CmdletBinding()]
param(
    [string[]]$Instances = @(($env:SQLH_CI_INSTANCES -split ' ') | Where-Object { $_ })
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Data

function Open-Session([string]$Server, [string]$App, [bool]$Pooled) {
    $pool = if ($Pooled) { 'Pooling=true;Max Pool Size=1' } else { 'Pooling=false' }
    $c = New-Object System.Data.SqlClient.SqlConnection (
        "Server=$Server;Integrated Security=SSPI;Encrypt=True;TrustServerCertificate=True;" +
        "Application Name=$App;Connect Timeout=5;$pool")
    $c.Open()
    $c
}

function Invoke-Sql($Conn, [string]$Sql) {
    $cmd = $Conn.CreateCommand()
    $cmd.CommandText = $Sql
    $cmd.CommandTimeout = 30
    $cmd.ExecuteScalar()
}

# The reader's session as the observer sees it.
function Read-Session($Observer, [int]$Spid) {
    $cmd = $Observer.CreateCommand()
    $cmd.CommandText = @"
SELECT s.open_transaction_count AS s_open, r.open_transaction_count AS r_open,
    r.status AS r_status, r.wait_type,
    (SELECT COUNT(*) FROM sys.dm_tran_session_transactions st WHERE st.session_id = s.session_id) AS tran_rows
FROM sys.dm_exec_sessions s LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
WHERE s.session_id = $Spid
"@
    $reader = $cmd.ExecuteReader()
    try {
        if (-not $reader.Read()) { throw "session $Spid isn't in sys.dm_exec_sessions" }
        $row = [ordered]@{}
        foreach ($i in 0..($reader.FieldCount - 1)) {
            $row[$reader.GetName($i)] = if ($reader.IsDBNull($i)) { 'NULL' } else { $reader.GetValue($i) }
        }
        $row
    }
    finally { $reader.Close() }
}

# A reader session for a case: new, or pooled and reused after a close (so its next batch
# carries the reset). Returns the open connection and its session id.
function New-Reader([string]$Server, [string]$Case, [bool]$Reset) {
    if (-not $Reset) {
        $c = Open-Session $Server "sqlh-diag-$Case" $false
        return @($c, [int](Invoke-Sql $c 'SELECT @@SPID'))
    }
    $app = "sqlh-diag-$Case"
    $c = Open-Session $Server $app $true
    $first = [int](Invoke-Sql $c 'SELECT @@SPID')
    $c.Close()
    $c = Open-Session $Server $app $true
    # The reset rides on this connection's next batch, not on Open.
    @($c, $first)
}

function Measure-Case($Server, $Observer, [string]$Case) {
    $reset = $Case -like 'reset-*'
    $reader, $spid = New-Reader $Server $Case $reset
    try {
        $blocked = 'SELECT COUNT(*) FROM tempdb.dbo.sqlh_diag WITH (READCOMMITTEDLOCK)'
        $task = $null
        $cmd = $reader.CreateCommand()
        $cmd.CommandTimeout = 30
        if ($Case -eq 'one-batch') {
            $cmd.CommandText = "BEGIN TRANSACTION; $blocked"
            $task = $cmd.ExecuteScalarAsync()
        }
        else {
            # As sqlh's setup: the transaction's batch, which carries a reused connection's
            # reset, then (running cases) the query as the next batch. It also says which
            # session the reused connection is, without a batch of its own.
            $now = [int](Invoke-Sql $reader 'BEGIN TRANSACTION; SELECT @@SPID')
            if ($now -ne $spid) { throw "$Case ran on session $now, not $spid" }
            if ($Case -like '*-running') {
                $cmd.CommandText = $blocked
                $task = $cmd.ExecuteScalarAsync()
            }
        }
        $row = $null
        $deadline = (Get-Date).AddSeconds(10)
        do {
            $row = Read-Session $Observer $spid
            if ($task -eq $null -or "$($row.wait_type)" -like 'LCK_*') { break }
            Start-Sleep -Milliseconds 200
        } while ((Get-Date) -lt $deadline)
        if ($task -ne $null -and "$($row.wait_type)" -notlike 'LCK_*') { throw "$Case never blocked: $($row | Out-String)" }
        if ($task -ne $null) {
            $cmd.Cancel()
            try { [void]$task.Wait(10000) } catch { }
        }
        [void](Invoke-Sql $reader 'IF @@TRANCOUNT > 0 ROLLBACK')
        $row['case'] = $Case
        $row['session'] = $spid
        [pscustomobject]$row
    }
    finally { $reader.Close() }
}

function Test-Instance([string]$Instance) {
    $server = "$env:COMPUTERNAME\$Instance"
    $service = "MSSQL`$$Instance"
    Write-Host "== $Instance" -ForegroundColor Cyan
    Start-Service $service
    $started = Get-Date
    $first = $null
    $observer = $null
    while ($true) {
        try { $observer = Open-Session $server 'sqlh-diag-observer' $false; break }
        catch {
            if ($first -eq $null) { $first = $_.Exception.Message }
            if (((Get-Date) - $started).TotalSeconds -gt 60) { throw "no connect within 60 s: $first" }
            Start-Sleep -Seconds 1
        }
    }
    $took = [int]((Get-Date) - $started).TotalMilliseconds
    Write-Host "first connect $took ms after Start-Service returned$(if ($first) { "; first error: $first" })"
    $blocker = $null
    try {
        Write-Host (Invoke-Sql $observer 'SELECT @@VERSION')
        [void](Invoke-Sql $observer "IF OBJECT_ID('tempdb.dbo.sqlh_diag') IS NOT NULL DROP TABLE tempdb.dbo.sqlh_diag; CREATE TABLE tempdb.dbo.sqlh_diag (n int); INSERT tempdb.dbo.sqlh_diag VALUES (1);")
        $blocker = Open-Session $server 'sqlh-diag-blocker' $false
        [void](Invoke-Sql $blocker 'BEGIN TRANSACTION; UPDATE tempdb.dbo.sqlh_diag SET n = n + 1;')
        $rows = foreach ($case in 'fresh-running', 'reset-running', 'one-batch', 'fresh-idle', 'reset-idle') {
            Measure-Case $server $observer $case
        }
        $rows | Format-Table case, session, s_open, r_open, tran_rows, r_status, wait_type -AutoSize | Out-Host
    }
    finally {
        if ($blocker) { try { [void](Invoke-Sql $blocker 'IF @@TRANCOUNT > 0 ROLLBACK'); $blocker.Close() } catch { } }
        try { [void](Invoke-Sql $observer "IF OBJECT_ID('tempdb.dbo.sqlh_diag') IS NOT NULL DROP TABLE tempdb.dbo.sqlh_diag") } catch { }
        $observer.Close()
        Stop-Service $service -Force
    }
}

Set-Service SQLBrowser -StartupType Manual
Start-Service SQLBrowser
$failed = @()
foreach ($instance in $Instances) {
    try { Test-Instance $instance }
    catch { $failed += "${instance}: $($_.Exception.Message)" }
}
if ($failed) {
    Write-Host "failed: $($failed -join '; ')" -ForegroundColor Red
    exit 1
}
