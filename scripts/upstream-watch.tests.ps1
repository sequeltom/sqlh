# upstream-watch.ps1's decisions against fixed inputs: no network, no gh, nothing filed.
# scripts/check.ps1 runs it in pwsh, as the workflow does. Exits 1 with what differed.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'upstream-watch.ps1')

$wrong = [Collections.Generic.List[string]]::new()
function Expect([string]$name, $actual, $expected) {
    if (($actual -join '|') -cne ($expected -join '|')) {
        $wrong.Add("${name}: got '$($actual -join '|')', expected '$($expected -join '|')'")
    }
}
function Expect-Throw([string]$name, [scriptblock]$block) {
    try { & $block; $wrong.Add("${name}: no error") } catch { }
}
function Index([string[]]$entries) {
    ($entries | ForEach-Object {
            $vers, $yanked = $_ -split ':'
            "{`"name`":`"mssql-tds`",`"vers`":`"$vers`",`"deps`":[],`"yanked`":$yanked}"
        }) -join "`n"
}
function Titles([string]$pinned, [string[]]$entries) {
    @(Get-Findings $pinned @(Read-IndexVersions (Index $entries)) | ForEach-Object Title)
}

Expect 'pin' (Get-PinnedVersion "[dependencies]`r`nmssql-tds = `"=0.4.0`"`r`ntokio = `"1`"") '0.4.0'
Expect 'pin with a comment' (Get-PinnedVersion "mssql-tds = `"=0.4.0`" # exact") '0.4.0'
Expect-Throw 'a caret requirement' { Get-PinnedVersion 'mssql-tds = "0.4.0"' }
Expect-Throw 'no pin' { Get-PinnedVersion 'tokio = "1"' }
Expect-Throw 'two pins' { Get-PinnedVersion "mssql-tds = `"=0.4.0`"`nmssql-tds = `"=0.5.0`"" }

Expect 'current' (Titles '0.5.0' @('0.4.0:false', '0.5.0:false')) @()
Expect 'newer' (Titles '0.4.0' @('0.3.0:false', '0.4.0:false', '0.5.0:false')) @('mssql-tds 0.5.0 published')
Expect 'only the newest' (Titles '0.4.0' @('0.4.0:false', '0.5.0:false', '0.10.0:false', '0.6.0:false')) @('mssql-tds 0.10.0 published')
Expect 'a yanked newer' (Titles '0.4.0' @('0.4.0:false', '0.5.0:true')) @()
Expect 'a prerelease' (Titles '0.4.0' @('0.4.0:false', '0.5.0-beta.1:false')) @()
Expect 'an older one only' (Titles '0.4.0' @('0.3.0:false', '0.4.0:false')) @()
Expect 'the pin yanked' (Titles '0.4.0' @('0.4.0:true', '0.4.1:false')) @(
    'mssql-tds 0.4.0, the pinned driver, was yanked', 'mssql-tds 0.4.1 published')
Expect-Throw 'the pin missing' { Titles '0.4.0' @('0.5.0:false') }
Expect-Throw 'a malformed line' { Read-IndexVersions '{"vers":' }

$old = 'a04e22742' + ('0' * 31)
$new = '6a4b88d25' + ('1' * 31)
$lines = @(
    "$new`tBump released TDS crates to 0.5.0 (#878)",
    ('2b2718ae6' + ('2' * 31) + "`tRead resident Data Classification strings in one run (#851)"),
    (('3' * 40) + "`tBump released TDS crates to 0.4.01 (#1)"),
    "$old`tBump released TDS crates to 0.4.0 (#784)",
    "not-a-sha`tBump released TDS crates to 0.3.0 (#700)"
)
Expect 'bump commit' (Find-BumpCommit $lines '0.5.0') $new
Expect 'no prefix match' (Find-BumpCommit $lines '0.4.0') $old
Expect 'no bad sha' (Find-BumpCommit $lines '0.3.0') @()
Expect 'compare link' ((Get-CommitsLink $lines '0.4.0' '0.5.0') -split ' ', 2)[0] "https://github.com/microsoft/mssql-rs/compare/$old...$new"
Expect 'history link' ((Get-CommitsLink $lines '0.3.0' '0.5.0') -split ' ', 2)[0] 'https://github.com/microsoft/mssql-rs/commits/main/mssql-tds'

$published = [pscustomobject]@{ Kind = 'published'; Version = '0.5.0'; Title = 'x' }
$body = New-IssueBody $published '0.4.0' 'LINK'
foreach ($part in 'https://diff.rs/mssql-tds/0.4.0/0.5.0', 'Upstream commits: LINK',
    'docs/notes/driver-0.5.0.md', 'branch `driver-0.5.0`', 'sqlh pins 0.4.0 (`Cargo.toml`, `=0.4.0`)') {
    if (-not $body.Contains($part)) { $wrong.Add("published body lacks: $part") }
}
$yanked = New-IssueBody ([pscustomobject]@{ Kind = 'yanked'; Version = '0.4.0'; Title = 'x' }) '0.4.0' ''
if (-not $yanked.Contains('yanked mssql-tds 0.4.0')) { $wrong.Add('yanked body lacks its version') }

if ($wrong.Count) {
    Write-Host ($wrong -join "`n") -ForegroundColor Red
    exit 1
}
Write-Host 'upstream-watch.ps1: pins, newest releases, yanks and bump commits read as expected'
exit 0
