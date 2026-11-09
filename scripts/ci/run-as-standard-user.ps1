# Run cargo test binaries as a throwaway standard (non-administrator) Windows account (#65).
#
# GitHub's Windows runners are elevated. There an elevated scull refuses destructive MCP tools
# and `scull approve` (SAFE-14), so those tests skip themselves, and `clean`/`restore` refuse
# without `--allow-root`, so their tests add the flag. Neither exercises what a normal user
# runs. This runs the test binaries again as a standard user with
# SCULL_TEST_REQUIRE_NOT_ELEVATED=1: an elevated process then fails the tests instead of
# skipping or adding `--allow-root`, and each test prints a `RUN <name>: not elevated` marker,
# which this script requires for every name in -MustRun.
#
#   pwsh scripts/ci/run-as-standard-user.ps1 -Tests scull-cli/mcp_approval,scull-cli/clean `
#        -MustRun agent_plan_runs_only_after_the_human_approves,clean
#
# CI only: it creates a local account and grants it read/execute on the checkout.

param(
    [Parameter(Mandatory)] [string[]] $Tests,   # package/test-target pairs
    [string[]] $MustRun = @()
)
$ErrorActionPreference = 'Stop'

# A throwaway standard account (member of Users only).
$user = 'scullci'
$password = 'Sc1!' + [guid]::NewGuid().ToString('N')
net user $user $password /add /y | Out-Null
if ($LASTEXITCODE -ne 0) { throw "net user failed ($LASTEXITCODE)" }
net localgroup Administrators | Select-String -SimpleMatch $user | ForEach-Object { throw "$user must not be an administrator" }
$cred = [pscredential]::new($user, (ConvertTo-SecureString $password -AsPlainText -Force))

# It reads the checkout (test binaries, the scull binary, files the tests read) and writes to
# a folder of its own. The child inherits this (the runner account's) environment, whose temp
# and profile folders it can't write: point them at folders of its own.
icacls $env:GITHUB_WORKSPACE /grant "${user}:(OI)(CI)RX" /T /C /Q | Out-Null
$outDir = Join-Path $env:SystemDrive 'scullci-out'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
icacls $outDir /grant "${user}:(OI)(CI)F" /T /C /Q | Out-Null
$home_ = Join-Path $outDir 'home'
$temp = Join-Path $outDir 'temp'
foreach ($d in @($home_, $temp, "$home_\AppData\Local", "$home_\AppData\Roaming")) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}
$sets = @(
    'SCULL_TEST_REQUIRE_NOT_ELEVATED=1', 'RUST_BACKTRACE=short',
    "TEMP=$temp", "TMP=$temp", "USERPROFILE=$home_", "HOME=$home_",
    "LOCALAPPDATA=$home_\AppData\Local", "APPDATA=$home_\AppData\Roaming"
) | ForEach-Object { "set `"$_`"&& " }

$failed = $false
$all = ''
foreach ($pair in $Tests) {
    $package, $test = $pair.Split('/', 2)
    # The test executable cargo already built (same profile and features as `cargo test`).
    $exe = $null
    # scull-cli's full-only tests need its `full` feature (#142), as in CI's `cargo test`.
    $features = if ($package -eq 'scull-cli') { @('--features', 'full') } else { @() }
    cargo test -p $package @features --test $test --locked --no-run --message-format=json | ForEach-Object {
        $m = $null
        try { $m = $_ | ConvertFrom-Json } catch { }
        if ($m -and $m.reason -eq 'compiler-artifact' -and $m.target.name -eq $test -and $m.profile.test -and $m.executable) {
            $exe = $m.executable
        }
    }
    if (-not $exe) { throw "could not find the test executable of $package --test $test" }

    $log = Join-Path $outDir "$test.log"
    $rc = Join-Path $outDir "$test.exit"
    Remove-Item -Force -ErrorAction SilentlyContinue $log, $rc
    # cmd sets the variables, runs the tests one at a time, and records output and exit code.
    $cmdLine = ($sets -join '') +
        "`"$exe`" --test-threads=1 --nocapture > `"$log`" 2>&1 & call echo %^ERRORLEVEL% > `"$rc`""
    Write-Host "::group::$package --test $test as $user"
    $p = Start-Process -FilePath "$env:SystemRoot\System32\cmd.exe" -ArgumentList '/d', '/c', $cmdLine `
        -Credential $cred -LoadUserProfile -WorkingDirectory $outDir -Wait -PassThru -WindowStyle Hidden
    $out = if (Test-Path $log) { Get-Content -Raw $log } else { '' }
    Write-Host $out
    Write-Host '::endgroup::'
    $code = if (Test-Path $rc) { (Get-Content -Raw $rc).Trim() } else { "missing (cmd exit $($p.ExitCode))" }
    Write-Host "$package --test ${test}: exit code $code"
    if ($code -ne '0') { Write-Host "::error::$package --test $test failed as a standard user (exit $code)"; $failed = $true }
    if ($out -notmatch 'test result: ok\.') { Write-Host "::error::$test printed no passing test result"; $failed = $true }
    if ($out -match 'SKIP \S+: running elevated') { Write-Host "::error::a test in $test skipped itself as elevated"; $failed = $true }
    $all += $out
}

# A passing run (exit 0, "test result: ok") means every test passed; the RUN markers prove the
# listed ones took the non-elevated path.
foreach ($t in $MustRun) {
    if ($all -match "RUN $([regex]::Escape($t)): not elevated") { Write-Host "RUN marker: $t" }
    else { Write-Host "::error::$t did not run non-elevated"; $failed = $true }
}

net user $user /delete | Out-Null
if ($failed) { exit 1 }
