# After the live run: check no API key leaked into the logs, then package them for submission.
# Usage: .\scripts\package-logs.ps1 -Repo "$HOME\cc\live"
param([Parameter(Mandatory = $true)][string]$Repo)
$ErrorActionPreference = "Stop"

$notary = Join-Path $Repo ".notary"
if (-not (Test-Path $notary)) { throw "no .notary folder in $Repo" }

# 1. Key leak check: known key prefixes, plus the actual values from .env
$patterns = @("AIza", "sk-or-", "sk-ant", "sk-proj")
$envFile = Join-Path $PSScriptRoot "..\.env"
if (Test-Path $envFile) {
    foreach ($line in Get-Content $envFile) {
        if ($line -match '^\s*[A-Z0-9_]+\s*=\s*(\S{12,})') { $patterns += $Matches[1] }
    }
}
$hits = Get-ChildItem $notary -Recurse -File |
    Where-Object { $_.FullName -notmatch '\\trees\\' } |
    Select-String -SimpleMatch -Pattern $patterns -List
if ($hits) {
    $hits | ForEach-Object { Write-Host "KEY FOUND in $($_.Path)" -ForegroundColor Red }
    throw "Key material found in the logs. Fix before submitting."
}
Write-Host "no keys found in .notary (worktrees excluded)"

# 2. Package logs (worktrees excluded) and copy the report
$out = Join-Path $PSScriptRoot "..\live-run"
New-Item -ItemType Directory -Force $out | Out-Null
tar --exclude=".notary/trees" -czf (Join-Path $out "live-logs.tgz") -C $Repo .notary
Copy-Item (Join-Path $notary "report.md") (Join-Path $out "report.md") -Force
Get-ChildItem $out | Format-Table Name, Length
Write-Host "next: git add live-run; git commit -m 'live run logs and report'; git push"
