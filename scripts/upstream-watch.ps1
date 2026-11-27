<#
.SYNOPSIS
    The driver watch (PLAN.md §4, Upstream watch): files an issue in this repository when
    crates.io has an mssql-tds newer than Cargo.toml's exact pin, or yanks the pinned one.

.DESCRIPTION
    Dependabot ignores mssql-tds (.github/dependabot.yml): the driver is bumped by hand, after
    an analysis of the new crate (AGENTS.md, Stack), so this watch only tells the owner one
    exists. GitHub can't: upstream (microsoft/mssql-rs) publishes no GitHub releases, and its
    CHANGELOG.md keeps one Unreleased section rather than one per version.

    It reads the version list from the crates.io sparse index, the source meant for automated
    reads, and files at most one issue per title, open or closed, so a closed issue is never
    filed again. A newer version and a yank are separate issues. Prereleases are ignored.
    .github/workflows/upstream-watch.yml runs it daily; scripts/upstream-watch.tests.ps1
    checks its decisions against fixed inputs, run by scripts/check.ps1.

.PARAMETER DryRun
    Print the issues it would file instead of filing them. It still reads the index, the
    existing issues and upstream's commits.

.EXAMPLE
    pwsh scripts/upstream-watch.ps1 -DryRun
#>
[CmdletBinding()]
param(
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:Crate = 'mssql-tds'
# Anything that reaches an issue's title or body from outside is one of these shapes first.
$script:Release = '^\d+\.\d+\.\d+$'
$script:Sha = '^[0-9a-f]{40}$'

function Get-PinnedVersion([string]$cargoToml) {
    $pins = [regex]::Matches($cargoToml, '(?m)^mssql-tds = "=(\d+\.\d+\.\d+)"[ \t]*(#.*)?\r?$')
    if ($pins.Count -ne 1) {
        throw 'Cargo.toml has no single exact pin of the form mssql-tds = "=x.y.z"'
    }
    $pins[0].Groups[1].Value
}

# The sparse index has one JSON object per line, one line per published version.
function Read-IndexVersions([string]$index) {
    foreach ($line in $index -split "`n") {
        if (-not $line.Trim()) { continue }
        $entry = $line | ConvertFrom-Json
        [pscustomobject]@{ Version = [string]$entry.vers; Yanked = [bool]$entry.yanked }
    }
}

function Get-Findings([string]$pinned, [object[]]$versions) {
    $own = @($versions | Where-Object { $_.Version -eq $pinned })
    if (-not $own) { throw "the index has no $script:Crate $pinned, the version Cargo.toml pins" }
    if ($own[0].Yanked) {
        [pscustomobject]@{
            Kind = 'yanked'; Version = $pinned
            Title = "$script:Crate $pinned, the pinned driver, was yanked"
        }
    }
    # Only the newest: an upgrade goes straight to it, past any between.
    $newest = $versions |
        Where-Object { -not $_.Yanked -and $_.Version -match $script:Release -and
            [version]$_.Version -gt [version]$pinned } |
        Sort-Object { [version]$_.Version } |
        Select-Object -Last 1
    if ($newest) {
        [pscustomobject]@{
            Kind = 'published'; Version = $newest.Version
            Title = "$script:Crate $($newest.Version) published"
        }
    }
}

# Upstream bumps all its released crates in one commit, "Bump released TDS crates to x.y.z
# (#n)"; lines are "<sha>`t<subject>". A version with no such commit gets no compare link.
function Find-BumpCommit([string[]]$lines, [string]$version) {
    $subject = '^Bump released TDS crates to ' + [regex]::Escape($version) + '(\s|$)'
    foreach ($line in $lines) {
        $sha, $rest = $line -split "`t", 2
        if ($sha -match $script:Sha -and $rest -match $subject) { return $sha }
    }
}

function Get-CommitsLink([string[]]$lines, [string]$from, [string]$to) {
    $old = Find-BumpCommit $lines $from
    $new = Find-BumpCommit $lines $to
    if ($old -and $new) {
        "https://github.com/microsoft/mssql-rs/compare/$old...$new (the whole repository, " +
        "ODBC, Python and JS included; the driver is mssql-tds/)"
    } else {
        "https://github.com/microsoft/mssql-rs/commits/main/mssql-tds (no bump commit found " +
        "for both versions)"
    }
}

function New-IssueBody($finding, [string]$pinned, [string]$commits) {
    $versions = "https://crates.io/crates/$script:Crate/versions"
    $filed = 'Filed by `.github/workflows/upstream-watch.yml` (`scripts/upstream-watch.ps1`).'
    if ($finding.Kind -eq 'yanked') {
        return @"
crates.io yanked $script:Crate $pinned, the version sqlh pins (``Cargo.toml``, ``=$pinned``). Builds from ``Cargo.lock`` still resolve it, but upstream withdrew it: find out why (upstream's issues and commits, RustSec, GitHub's advisories) before the next release, and whether a fixed version exists.

- Versions: $versions
- Upstream commits: https://github.com/microsoft/mssql-rs/commits/main/mssql-tds

$filed
"@
    }
    $v = $finding.Version
    @"
crates.io has $script:Crate $v; sqlh pins $pinned (``Cargo.toml``, ``=$pinned``). Dependabot ignores the driver, which is bumped by hand after its analysis (AGENTS.md, Stack).

- Versions: $versions
- Crate diff: https://diff.rs/$script:Crate/$pinned/$v
- Upstream commits: $commits
- Upstream's ``CHANGELOG.md`` keeps one Unreleased section, not one per version, so it can't say what shipped in ${v}: the published crates' diff is the record.

The upgrade, as for 0.4.0 (PLAN.md §4, Driver upgrade to mssql-tds 0.4.0):

- [ ] Step 1, the analysis: ``docs/notes/driver-$v.md``, the two published crates compared file by file, every claim re-read against both versions and sqlh, ``docs/notes/upstream-issues.md`` re-checked; nothing built.
- [ ] Its findings to the owner one at a time, recorded in PLAN.md §6, among them which release takes the driver.
- [ ] Step 2, on a branch ``driver-$v``: the exact pin, ``windows-sys`` on the version the driver uses, the size against the budget (``cargo bloat --release --crates``), ``scripts/check.ps1 -Live`` on both servers.
- [ ] The external review (decision 52), then the merge.

$filed
"@
}

if ($MyInvocation.InvocationName -eq '.') { return }

$root = Split-Path $PSScriptRoot -Parent
$pinned = Get-PinnedVersion (Get-Content -Raw (Join-Path $root 'Cargo.toml'))

# The index path for a name of four or more characters: its first two, then its next two.
$url = "https://index.crates.io/$($script:Crate.Substring(0, 2))/$($script:Crate.Substring(2, 2))/$script:Crate"
$response = Invoke-WebRequest -Uri $url -UserAgent 'sqlh-upstream-watch' -TimeoutSec 60
$index = if ($response.Content -is [byte[]]) {
    [Text.Encoding]::UTF8.GetString($response.Content)
} else { $response.Content }
$findings = @(Get-Findings $pinned @(Read-IndexVersions $index))
if (-not $findings) {
    Write-Host "$script:Crate $pinned is the newest release and not yanked"
    exit 0
}

$existing = @(gh issue list --state all --limit 1000 --json title --jq '.[].title')
if ($LASTEXITCODE) { throw 'gh issue list failed' }

foreach ($finding in $findings) {
    if ($existing -ccontains $finding.Title) {
        Write-Host "already filed: $($finding.Title)"
        continue
    }
    $commits = ''
    if ($finding.Kind -eq 'published') {
        # A link only; the issue is still worth filing without it.
        $lines = @(gh api 'repos/microsoft/mssql-rs/commits?path=mssql-tds/Cargo.toml&per_page=100' `
                --jq '.[] | .sha + "\t" + (.commit.message | split("\n")[0])' 2>$null)
        if ($LASTEXITCODE) { $lines = @() }
        $commits = Get-CommitsLink $lines $pinned $finding.Version
    }
    $body = New-IssueBody $finding $pinned $commits
    if ($DryRun) {
        Write-Host "would file: $($finding.Title)`n$body"
        continue
    }
    $file = New-TemporaryFile
    try {
        Set-Content -LiteralPath $file -Value $body -NoNewline -Encoding utf8NoBOM
        gh issue create --title $finding.Title --body-file $file --label dependencies
        if ($LASTEXITCODE) { throw "gh issue create failed for: $($finding.Title)" }
    } finally {
        Remove-Item -LiteralPath $file -ErrorAction SilentlyContinue
    }
}
exit 0
