#requires -Version 7.0
param([string]$Distribution = 'Ubuntu-22.04')

# Requires explicit authority to create temporary system units and a loopback
# root SSH endpoint in this WSL distro. Does not install or enable default SSH.
$ErrorActionPreference = 'Stop'
$runId = [guid]::NewGuid().ToString('N')
$repository = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$temporaryParent = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$temporary = Join-Path $temporaryParent "shipforge-systemd-$runId"
$temporaryCreated = $false
$fixture = $null
$fixtureAttempted = $false
$keepAlive = $null
$keepAliveStarted = $false
$operationError = $null
$environmentNames = @('SHIPFORGE_SYSTEMD_ACCEPTANCE', 'SHIPFORGE_SYSTEMD_RUN_ID', 'SHIPFORGE_TEST_SSH_PORT', 'SHIPFORGE_TEST_SSH_HOST_KEY', 'SHIPFORGE_TEST_SSH_IDENTITY_FILE')
$savedEnvironment = @{}
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }

function Invoke-SystemdFixture {
    param([string[]]$Arguments)
    $output = & wsl.exe -d $Distribution -u root --exec python3 $fixture @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Systemd fixture command failed ($($Arguments[0]))" }
    return $output
}

function Complete-SystemdRun {
    param(
        [bool]$FixtureAttempted,
        [string]$RunId,
        [string]$Temporary,
        [string]$TemporaryParent,
        [bool]$TemporaryCreated,
        [hashtable]$SavedEnvironment,
        $KeepAlive,
        [bool]$KeepAliveStarted
    )
    $failures = [System.Collections.Generic.List[string]]::new()
    $validRunContext = $false
    try {
        if ($RunId -cnotmatch '^[0-9a-f]{32}$') { throw 'Invalid fixture run ID' }
        if (-not [System.IO.Path]::IsPathFullyQualified($Temporary) -or -not [System.IO.Path]::IsPathFullyQualified($TemporaryParent)) { throw 'Identity cleanup requires absolute paths' }
        $resolved = [System.IO.Path]::GetFullPath($Temporary)
        $expected = [System.IO.Path]::GetFullPath((Join-Path $TemporaryParent "shipforge-systemd-$RunId"))
        if (-not $resolved.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) { throw 'Refused deletion outside the exact temporary identity directory' }
        $validRunContext = $true
    } catch { $failures.Add("Cleanup scope: $($_.Exception.Message)") }
    try {
        if ($FixtureAttempted -and $validRunContext) {
            try { Invoke-SystemdFixture -Arguments @('cleanup', $RunId) | Out-Host }
            catch { $failures.Add("WSL fixture: $($_.Exception.Message)") }
        }
        foreach ($name in $SavedEnvironment.Keys) {
            try {
                $value = if ($null -eq $SavedEnvironment[$name]) { [System.Management.Automation.Language.NullString]::Value } else { $SavedEnvironment[$name] }
                [Environment]::SetEnvironmentVariable($name, $value, 'Process')
            } catch { $failures.Add("Environment ${name}: $($_.Exception.Message)") }
        }
        if ($TemporaryCreated -and $validRunContext) {
            try {
                if (Test-Path -LiteralPath $resolved) {
                    $attributes = (Get-Item -LiteralPath $resolved -Force).Attributes
                    if ($attributes -band [System.IO.FileAttributes]::ReparsePoint) { throw 'Refused deletion of a reparse-point identity directory' }
                    if (-not ($attributes -band [System.IO.FileAttributes]::Directory)) { throw 'Refused deletion of a non-directory identity path' }
                    Remove-Item -LiteralPath $resolved -Recurse -Force
                }
            } catch { $failures.Add("Temporary identity directory: $($_.Exception.Message)") }
        }
    } finally {
        if ($null -ne $KeepAlive) {
            try {
                if ($KeepAliveStarted) {
                    try { $KeepAlive.StandardInput.Close() }
                    catch { $failures.Add("WSL input pipe: $($_.Exception.Message)") }
                    try {
                        if (-not $KeepAlive.WaitForExit(5000)) {
                            $KeepAlive.Kill()
                            if (-not $KeepAlive.WaitForExit(5000)) { throw 'Owned WSL helper did not exit' }
                        }
                    } catch { $failures.Add("WSL helper: $($_.Exception.Message)") }
                }
            } finally {
                try { $KeepAlive.Dispose() }
                catch { $failures.Add("WSL helper disposal: $($_.Exception.Message)") }
            }
        }
    }
    return $failures.ToArray()
}

try {
    # Hold this distro open while Windows cargo drives its loopback SSH server.
    $keepAlive = [System.Diagnostics.Process]::new()
    $keepAlive.StartInfo.FileName = 'wsl.exe'
    $keepAlive.StartInfo.UseShellExecute = $false
    $keepAlive.StartInfo.CreateNoWindow = $true
    $keepAlive.StartInfo.RedirectStandardInput = $true
    foreach ($argument in @('-d', $Distribution, '--exec', '/bin/cat')) { $keepAlive.StartInfo.ArgumentList.Add($argument) }
    if (-not $keepAlive.Start()) { throw 'Could not keep the test WSL distro available' }
    $keepAliveStarted = $true
    New-Item -ItemType Directory -Path $temporary | Out-Null
    $temporaryCreated = $true
    $key = Join-Path $temporary 'id_ed25519'
    & ssh-keygen -q -t ed25519 -N '' -f $key
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the disposable identity' }
    $fixture = & wsl.exe -d $Distribution --exec wslpath -u (Join-Path $PSScriptRoot 'fixtures/systemd/fixture.py')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($fixture)) { throw 'Could not resolve the WSL fixture script' }
    $fixture = $fixture.Trim()
    $publicKey = & wsl.exe -d $Distribution --exec wslpath -u "$key.pub"
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($publicKey)) { throw 'Could not resolve the public key in WSL' }
    $fixtureAttempted = $true
    $endpoint = (Invoke-SystemdFixture -Arguments @('create', $runId, $publicKey.Trim())) | ConvertFrom-Json
    if ($endpoint.port -lt 1024 -or $endpoint.port -gt 65535 -or $endpoint.host_key -cnotmatch '^SHA256:[A-Za-z0-9+/]+$') { throw 'Invalid disposable SSH endpoint' }
    $env:SHIPFORGE_SYSTEMD_ACCEPTANCE = '1'
    $env:SHIPFORGE_SYSTEMD_RUN_ID = $runId
    $env:SHIPFORGE_TEST_SSH_PORT = [string]$endpoint.port
    $env:SHIPFORGE_TEST_SSH_HOST_KEY = $endpoint.host_key
    $env:SHIPFORGE_TEST_SSH_IDENTITY_FILE = $key
    Write-Output "Testing disposable systemd fixture $runId on 127.0.0.1:$($endpoint.port)"
    Push-Location $repository
    try {
        & cargo test --test linux_ssh_systemd -- --ignored --nocapture
        if ($LASTEXITCODE -ne 0) { throw 'Real systemd acceptance failed' }
    } finally { Pop-Location }
} catch {
    $operationError = $_
    if ($fixtureAttempted) {
        try { Invoke-SystemdFixture -Arguments @('diagnostics', $runId) | Out-Host }
        catch { Write-Warning "Could not read owned fixture diagnostics: $($_.Exception.Message)" }
    }
} finally {
    $cleanupErrors = @(Complete-SystemdRun -FixtureAttempted $fixtureAttempted -RunId $runId -Temporary $temporary -TemporaryParent $temporaryParent -TemporaryCreated $temporaryCreated -SavedEnvironment $savedEnvironment -KeepAlive $keepAlive -KeepAliveStarted $keepAliveStarted)
}
if ($null -ne $operationError) {
    foreach ($cleanupError in $cleanupErrors) { Write-Warning "Cleanup incomplete: $cleanupError" }
    throw $operationError
}
if ($cleanupErrors.Count -gt 0) { throw "Systemd acceptance cleanup incomplete: $($cleanupErrors -join '; ')" }
