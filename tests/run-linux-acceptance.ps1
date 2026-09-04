#requires -Version 7.0
param(
    [string]$Distribution = 'Ubuntu-22.04',
    [ValidateSet('All', 'Deployment', 'Retention', 'AutomaticRetention')]
    [string]$Suite = 'All'
)

$ErrorActionPreference = 'Stop'
$runId = [guid]::NewGuid().ToString('N')
$image = "shipforge-m1-fixture:$runId"
$imageBuilt = $false
$keepAlive = $null
$keepAliveStarted = $false
$operationError = $null
$containers = @()
$temporaryParent = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$temporary = Join-Path $temporaryParent "shipforge-m1-$runId"
$repository = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$environmentNames = @('SHIPFORGE_LINUX_ACCEPTANCE', 'SHIPFORGE_TEST_KEY', 'SHIPFORGE_TEST_PORT_A', 'SHIPFORGE_TEST_PORT_B', 'SHIPFORGE_TEST_HOST_KEY_A', 'SHIPFORGE_TEST_HOST_KEY_B')
$savedEnvironment = @{}
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }

function Invoke-FixtureDocker {
    param([string[]]$Arguments)
    $output = & wsl.exe -d $Distribution --exec docker -H unix:///var/run/docker.sock @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Fixture Docker command failed ($($Arguments[0]))" }
    return $output
}

function Invoke-FixtureCase {
    param([string]$Case)
    # Cargo exits successfully even when a misspelled filter selects zero tests.
    $listed = @(& cargo test --locked --test linux_ssh_deployment $Case -- --ignored --exact --list)
    if ($LASTEXITCODE -ne 0 -or @($listed | Where-Object { $_ -eq "${Case}: test" }).Count -ne 1) {
        throw "Expected exactly one available disposable Linux test: $Case"
    }
    & cargo test --locked --test linux_ssh_deployment $Case -- --ignored --exact --nocapture
    if ($LASTEXITCODE -ne 0) { throw "Disposable Linux acceptance failed: $Case" }
}

function Complete-FixtureRun {
    param(
        [string[]]$Containers,
        [string]$RunId,
        [string]$Image,
        [bool]$ImageBuilt,
        [string]$Temporary,
        [string]$TemporaryParent,
        [hashtable]$SavedEnvironment,
        $KeepAlive,
        [bool]$KeepAliveStarted
    )
    $failures = [System.Collections.Generic.List[string]]::new()
    try {
        foreach ($container in $Containers) {
            try {
                $label = (Invoke-FixtureDocker -Arguments @('inspect', '--format', '{{ index .Config.Labels "shipforge.test.run" }}', $container)).Trim()
                if ($label -ne $RunId) { throw 'Container run label does not match; refusing deletion' }
                Invoke-FixtureDocker -Arguments @('rm', '--force', $container) | Out-Host
            } catch { $failures.Add("Container ${container}: $($_.Exception.Message)") }
        }
        if ($ImageBuilt) {
            try { Invoke-FixtureDocker -Arguments @('image', 'rm', $Image) | Out-Host }
            catch { $failures.Add("Image ${Image}: $($_.Exception.Message)") }
        }
        foreach ($name in $SavedEnvironment.Keys) {
            try {
                if ($null -eq $SavedEnvironment[$name]) {
                    [Environment]::SetEnvironmentVariable($name, [System.Management.Automation.Language.NullString]::Value, 'Process')
                } else { [Environment]::SetEnvironmentVariable($name, $SavedEnvironment[$name], 'Process') }
            }
            catch { $failures.Add("Environment ${name}: $($_.Exception.Message)") }
        }
        try {
            $resolvedTemporary = [System.IO.Path]::GetFullPath($Temporary)
            $parentPrefix = [System.IO.Path]::TrimEndingDirectorySeparator([System.IO.Path]::GetFullPath($TemporaryParent)) + [System.IO.Path]::DirectorySeparatorChar
            if (-not $resolvedTemporary.StartsWith($parentPrefix, [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolvedTemporary -Leaf) -ne "shipforge-m1-$RunId") {
                throw 'Refused cleanup outside the exact disposable directory'
            }
            if (Test-Path -LiteralPath $resolvedTemporary) { Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force }
        } catch { $failures.Add("Temporary identity directory: $($_.Exception.Message)") }
    } finally {
        # Release this process even if another resource cannot be cleaned up.
        if ($null -ne $KeepAlive) {
            try {
                if ($KeepAliveStarted) {
                    try { $KeepAlive.StandardInput.Close() }
                    catch { $failures.Add("WSL input pipe: $($_.Exception.Message)") }
                    try {
                        if (-not $KeepAlive.WaitForExit(5000)) {
                            $KeepAlive.Kill()
                            if (-not $KeepAlive.WaitForExit(5000)) { throw 'Owned WSL helper did not exit after termination' }
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
    # systemd services do not keep a WSL instance alive. Hold an input pipe open
    # while Windows cargo is running, without changing WSL or Docker settings.
    $keepAlive = [System.Diagnostics.Process]::new()
    $keepAlive.StartInfo.FileName = 'wsl.exe'
    $keepAlive.StartInfo.UseShellExecute = $false
    $keepAlive.StartInfo.CreateNoWindow = $true
    $keepAlive.StartInfo.RedirectStandardInput = $true
    foreach ($argument in @('-d', $Distribution, '--exec', '/bin/cat')) { $keepAlive.StartInfo.ArgumentList.Add($argument) }
    if (-not $keepAlive.Start()) { throw 'Could not keep the disposable Docker runtime available' }
    $keepAliveStarted = $true
    New-Item -ItemType Directory -Path $temporary | Out-Null
    $key = Join-Path $temporary 'id_ed25519'
    & ssh-keygen -q -t ed25519 -N '' -f $key
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the disposable identity' }
    $context = & wsl.exe -d $Distribution --exec wslpath -u (Join-Path $PSScriptRoot 'fixtures/openssh')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($context)) { throw 'Could not resolve the fixture build context in WSL' }
    $context = $context.Trim()
    $publicKey = & wsl.exe -d $Distribution --exec wslpath -u "$key.pub"
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($publicKey)) { throw 'Could not resolve the disposable public key in WSL' }
    $publicKey = $publicKey.Trim()
    Invoke-FixtureDocker -Arguments @('build', '-t', $image, $context) | Out-Host
    $imageBuilt = $true
    foreach ($suffix in @('A', 'B')) {
        $container = "shipforge-m1-$runId-$($suffix.ToLowerInvariant())"
        $containers += $container
        Invoke-FixtureDocker -Arguments @('run', '--detach', '--name', $container, '--label', "shipforge.test.run=$runId", '--publish', '127.0.0.1::22', '--mount', "type=bind,source=$publicKey,target=/fixture/authorized_keys,readonly", $image) | Out-Host
        $fingerprint = $null
        for ($attempt = 0; $attempt -lt 30; $attempt++) {
            $fingerprint = & wsl.exe -d $Distribution --exec docker -H unix:///var/run/docker.sock exec $container ssh-keygen -lf /fixture/host_ed25519.pub -E sha256 2>$null
            if ($LASTEXITCODE -eq 0) { break }
            Start-Sleep -Milliseconds 200
        }
        if ($LASTEXITCODE -ne 0 -or $fingerprint -notmatch 'SHA256:[A-Za-z0-9+/]+') { throw 'Host Key not ready' }
        [Environment]::SetEnvironmentVariable("SHIPFORGE_TEST_HOST_KEY_$suffix", $Matches[0], 'Process')
        $binding = (Invoke-FixtureDocker -Arguments @('port', $container, '22/tcp')).Trim()
        if ($binding -notmatch '^127\.0\.0\.1:(\d+)$') { throw 'Fixture must listen on loopback only' }
        [Environment]::SetEnvironmentVariable("SHIPFORGE_TEST_PORT_$suffix", $Matches[1], 'Process')
    }
    $env:SHIPFORGE_LINUX_ACCEPTANCE = '1'
    $env:SHIPFORGE_TEST_KEY = $key
    Push-Location $repository
    try {
        $cases = @()
        if ($Suite -in @('All', 'Deployment')) {
            $cases += 'real_linux_single_joint_rollback_health_failure_and_cancellation'
        }
        if ($Suite -in @('All', 'Retention')) {
            $cases += 'real_linux_exact_retention_partial_and_archive_only_retry'
        }
        if ($Suite -in @('All', 'AutomaticRetention')) {
            $cases += 'real_linux_automatic_retention_keeps_latest_five'
        }
        foreach ($case in $cases) {
            Invoke-FixtureCase -Case $case
        }
    } finally { Pop-Location }
} catch {
    $operationError = $_
    foreach ($container in $containers) {
        try { & wsl.exe -d $Distribution --exec docker -H unix:///var/run/docker.sock logs --tail 60 $container 2>&1 | Out-Host }
        catch { Write-Warning "Could not read diagnostics for $container" }
    }
} finally {
    $cleanupErrors = @(Complete-FixtureRun -Containers $containers -RunId $runId -Image $image -ImageBuilt $imageBuilt -Temporary $temporary -TemporaryParent $temporaryParent -SavedEnvironment $savedEnvironment -KeepAlive $keepAlive -KeepAliveStarted $keepAliveStarted)
}
if ($null -ne $operationError) {
    foreach ($cleanupError in $cleanupErrors) { Write-Warning "Cleanup incomplete: $cleanupError" }
    throw $operationError
}
if ($cleanupErrors.Count -gt 0) { throw "Acceptance cleanup incomplete: $($cleanupErrors -join '; ')" }
