# Run one cargo test binary as a throwaway standard (non-administrator) Windows account (#65).
#
# GitHub's Windows runners are elevated, and an elevated scull refuses destructive MCP tools and
# `scull approve` by design (SAFE-14), so tests of that flow skip themselves there. This runs the
# test binary again as a standard user, with SCULL_TEST_REQUIRE_NOT_ELEVATED=1 so that a test
# which would still skip fails instead, and checks that every listed test actually ran.
#
#   pwsh scripts/ci/run-as-standard-user.ps1 -Package scull-cli -Test mcp_approval `
#        -MustRun agent_plan_runs_only_after_the_human_approves,approve_prompt_shows_the_plan_and_needs_the_shown_code
#
# CI only: it creates a local account and grants it read/execute on the checkout.

param(
    [Parameter(Mandatory)] [string] $Package,
    [Parameter(Mandatory)] [string] $Test,
    [string[]] $MustRun = @()
)
$ErrorActionPreference = 'Stop'

# The test executable cargo already built (same profile and features as `cargo test`).
$exe = $null
cargo test -p $Package --test $Test --locked --no-run --message-format=json | ForEach-Object {
    $m = $null
    try { $m = $_ | ConvertFrom-Json } catch { }
    if ($m -and $m.reason -eq 'compiler-artifact' -and $m.target.name -eq $Test -and $m.profile.test -and $m.executable) {
        $exe = $m.executable
    }
}
if (-not $exe) { throw "could not find the test executable of $Package --test $Test" }
Write-Host "test executable: $exe"

# A throwaway standard account (member of Users only).
$user = 'scullci'
$password = 'Sc1!' + [guid]::NewGuid().ToString('N')
net user $user $password /add /y | Out-Null
if ($LASTEXITCODE -ne 0) { throw "net user failed ($LASTEXITCODE)" }
net localgroup Administrators | Select-String -SimpleMatch $user | ForEach-Object { throw "$user must not be an administrator" }

# It reads the checkout (test binaries, the scull binary, files the tests read) and writes
# its output to a folder of its own.
icacls $env:GITHUB_WORKSPACE /grant "${user}:(OI)(CI)RX" /T /C /Q | Out-Null
$outDir = Join-Path $env:SystemDrive 'scullci-out'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
icacls $outDir /grant "${user}:(OI)(CI)F" /T /C /Q | Out-Null
$log = Join-Path $outDir 'test.log'
$rc = Join-Path $outDir 'exit.txt'
Remove-Item -Force -ErrorAction SilentlyContinue $log, $rc
# The child inherits this (the runner account's) environment, whose temp and profile folders
# the standard user can't write: point them at folders of its own.
$home_ = Join-Path $outDir 'home'
$temp = Join-Path $outDir 'temp'
foreach ($d in @($home_, $temp, "$home_\AppData\Local", "$home_\AppData\Roaming")) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}

# cmd sets the variables, runs the tests one at a time, and records output and exit code.
$sets = @(
    'SCULL_TEST_REQUIRE_NOT_ELEVATED=1', 'RUST_BACKTRACE=short',
    "TEMP=$temp", "TMP=$temp", "USERPROFILE=$home_", "HOME=$home_",
    "LOCALAPPDATA=$home_\AppData\Local", "APPDATA=$home_\AppData\Roaming"
) | ForEach-Object { "set `"$_`"&& " }
$cmdLine = ($sets -join '') +
    "`"$exe`" --test-threads=1 --nocapture > `"$log`" 2>&1 & call echo %^ERRORLEVEL% > `"$rc`""
$cred = [pscredential]::new($user, (ConvertTo-SecureString $password -AsPlainText -Force))
$p = Start-Process -FilePath "$env:SystemRoot\System32\cmd.exe" -ArgumentList '/d', '/c', $cmdLine `
    -Credential $cred -LoadUserProfile -WorkingDirectory $outDir -Wait -PassThru -WindowStyle Hidden

$out = if (Test-Path $log) { Get-Content -Raw $log } else { '' }
Write-Host $out
$code = if (Test-Path $rc) { (Get-Content -Raw $rc).Trim() } else { "missing (cmd exit $($p.ExitCode))" }
Write-Host "test exit code: $code"

$failed = $false
if ($code -ne '0') { Write-Host "::error::$Package --test $Test failed as a standard user (exit $code)"; $failed = $true }
if ($out -notmatch 'test result: ok\.') { Write-Host '::error::no passing test result line'; $failed = $true }
if ($out -match 'SKIP \S+: running elevated') { Write-Host '::error::a test skipped itself as elevated'; $failed = $true }
# A passing run (exit 0, "test result: ok") means every test passed; the RUN markers prove
# the listed ones took the non-elevated path instead of skipping.
foreach ($t in $MustRun) {
    if ($out -notmatch "RUN $([regex]::Escape($t)): not elevated") { Write-Host "::error::$t did not run non-elevated"; $failed = $true }
}

net user $user /delete | Out-Null
if ($failed) { exit 1 }
