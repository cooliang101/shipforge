#requires -Version 7.0
# Read and exercise only the runner functions with in-memory doubles. This
# script never starts WSL, creates services, reads keys, or deletes files.
$ErrorActionPreference = 'Stop'
$savedNativeExitCode = $global:LASTEXITCODE
$parseErrors = $null
$tokens = $null
$runner = Join-Path $PSScriptRoot 'run-systemd-acceptance.ps1'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($runner, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw "Runner syntax errors: $parseErrors" }
foreach ($name in @('Invoke-SystemdFixture', 'Complete-SystemdRun')) {
    $definition = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $false)
    if ($null -eq $definition) { throw "Missing fixture function $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}

function Assert-Cleanup {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function wsl.exe {
    $script:nativeArguments = @($args)
    $global:LASTEXITCODE = $script:nativeExitCode
    return 'simulated fixture output'
}

$Distribution = 'fixture distro with spaces'
$fixture = '/fixture directory/fixture.py'
$script:testRunId = [guid]::NewGuid().ToString('N')
$script:passedCases = 0
$script:sentinelNames = @('SHIPFORGE_SYSTEMD_CLEANUP_TEST_SET', 'SHIPFORGE_SYSTEMD_CLEANUP_TEST_EMPTY', 'SHIPFORGE_SYSTEMD_CLEANUP_TEST_UNSET')
$savedSentinels = @{}
foreach ($name in $script:sentinelNames) { $savedSentinels[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }

function Assert-RestoredEnvironment {
    Assert-Cleanup ([Environment]::GetEnvironmentVariable($script:sentinelNames[0], 'Process') -ceq 'original-value') 'Existing environment value must be restored'
    $empty = [Environment]::GetEnvironmentVariable($script:sentinelNames[1], 'Process')
    Assert-Cleanup ($null -ne $empty -and $empty.Length -eq 0) 'An originally empty environment value must stay empty'
    Assert-Cleanup ($null -eq [Environment]::GetEnvironmentVariable($script:sentinelNames[2], 'Process')) 'An originally absent environment value must be removed'
}

function New-CleanupCase {
    $script:fixtureCalls = [System.Collections.Generic.List[string]]::new()
    $script:inspectedPaths = [System.Collections.Generic.List[string]]::new()
    $script:removedPaths = [System.Collections.Generic.List[string]]::new()
    $script:failedFixture = $false
    $script:failedDirectory = $false
    $script:failedInspection = $false
    $script:directoryExists = $true
    $script:directoryAttributes = [System.IO.FileAttributes]::Directory
    $script:failedInput = $false
    $script:failedWaitAt = 0
    $script:failedKill = $false
    $script:failedDispose = $false
    $script:forceKill = $false
    $script:neverExits = $false
    $script:lifecycle = [pscustomobject]@{ Closed = $false; Waits = 0; KillAttempts = 0; Killed = $false; DisposeAttempts = 0; Disposed = $false }
    $pipe = [pscustomobject]@{}
    $pipe | Add-Member -MemberType ScriptMethod -Name Close -Value {
        $script:lifecycle.Closed = $true
        if ($script:failedInput) { throw 'simulated input close failure' }
    }
    $process = [pscustomobject]@{ StandardInput = $pipe }
    $process | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
        param($Timeout)
        if ($Timeout -ne 5000) { throw 'Unexpected helper wait deadline' }
        $script:lifecycle.Waits++
        if ($script:failedWaitAt -eq $script:lifecycle.Waits) { throw 'simulated helper wait failure' }
        return (-not $script:forceKill -or ($script:lifecycle.Killed -and -not $script:neverExits))
    }
    $process | Add-Member -MemberType ScriptMethod -Name Kill -Value {
        $script:lifecycle.KillAttempts++
        if ($script:failedKill) { throw 'simulated helper kill failure' }
        $script:lifecycle.Killed = $true
    }
    $process | Add-Member -MemberType ScriptMethod -Name Dispose -Value {
        $script:lifecycle.DisposeAttempts++
        if ($script:failedDispose) { throw 'simulated helper dispose failure' }
        $script:lifecycle.Disposed = $true
    }
    foreach ($name in $script:sentinelNames) { [Environment]::SetEnvironmentVariable($name, 'changed-by-cleanup-test', 'Process') }
    return @{
        FixtureAttempted = $true
        RunId = $script:testRunId
        Temporary = Join-Path ([System.IO.Path]::GetTempPath()) "shipforge-systemd-$script:testRunId"
        TemporaryParent = [System.IO.Path]::GetTempPath()
        TemporaryCreated = $true
        SavedEnvironment = @{
            SHIPFORGE_SYSTEMD_CLEANUP_TEST_SET = 'original-value'
            SHIPFORGE_SYSTEMD_CLEANUP_TEST_EMPTY = ''
            SHIPFORGE_SYSTEMD_CLEANUP_TEST_UNSET = $null
        }
        KeepAlive = $process
        KeepAliveStarted = $true
    }
}

function Confirm-CleanupCase {
    Assert-RestoredEnvironment
    $script:passedCases++
}

try {
    # Exercise the production wrapper before replacing it with a cleanup double.
    $script:nativeExitCode = 0
    $output = Invoke-SystemdFixture -Arguments @('cleanup', $script:testRunId)
    Assert-Cleanup ($output -ceq 'simulated fixture output') 'Native success output must be preserved'
    $expectedArguments = @('-d', $Distribution, '-u', 'root', '--exec', 'python3', $fixture, 'cleanup', $script:testRunId)
    Assert-Cleanup (($script:nativeArguments -join "`0") -ceq ($expectedArguments -join "`0")) 'Fixture command must keep the explicit distro and literal argument boundaries'
    $script:nativeExitCode = 29
    $nativeFailure = $false
    try { Invoke-SystemdFixture -Arguments @('cleanup', $script:testRunId) | Out-Null }
    catch { $nativeFailure = $_.Exception.Message.Contains('Systemd fixture command failed (cleanup)') }
    Assert-Cleanup $nativeFailure 'A nonzero native exit status must fail fixture cleanup'

    function Invoke-SystemdFixture {
        param([string[]]$Arguments)
        if ($Arguments.Count -ne 2 -or $Arguments[0] -cne 'cleanup') { throw 'Unexpected fixture command in cleanup test' }
        $script:fixtureCalls.Add(($Arguments -join ' '))
        if ($script:failedFixture) { throw 'simulated systemd fixture cleanup failure' }
    }
    function Test-Path { param([string]$LiteralPath) return $script:directoryExists }
    function Get-Item {
        param([string]$LiteralPath, [switch]$Force)
        $script:inspectedPaths.Add($LiteralPath)
        if ($script:failedInspection) { throw 'simulated identity directory inspection failure' }
        return [pscustomobject]@{ Attributes = $script:directoryAttributes }
    }
    function Remove-Item {
        param([string]$LiteralPath, [switch]$Recurse, [switch]$Force)
        Assert-Cleanup ($Recurse -and $Force) 'Owned directory cleanup must retain its explicit flags'
        $script:removedPaths.Add($LiteralPath)
        if ($script:failedDirectory) { throw 'simulated identity directory deletion failure' }
    }

    $case = New-CleanupCase
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'Successful cleanup reported a failure'
    Assert-Cleanup ($script:fixtureCalls.Count -eq 1 -and $script:fixtureCalls[0] -ceq "cleanup $script:testRunId") 'Cleanup must address exactly the owned run'
    Assert-Cleanup ($script:removedPaths.Count -eq 1 -and $script:removedPaths[0] -eq $case.Temporary) 'Cleanup must delete exactly the created identity directory'
    Assert-Cleanup ($script:lifecycle.Closed -and $script:lifecycle.Waits -eq 1 -and $script:lifecycle.Disposed) 'Successful cleanup must release its WSL helper'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $script:failedFixture = $true
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 1 -and $failures[0].StartsWith('WSL fixture:')) 'Fixture failure must be reported'
    Assert-Cleanup ($script:removedPaths.Count -eq 1 -and $script:lifecycle.Disposed) 'Fixture failure must not skip identity cleanup or helper disposal'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $script:failedFixture = $true
    $script:failedDirectory = $true
    $script:failedInput = $true
    $script:failedDispose = $true
    $script:forceKill = $true
    $case.SavedEnvironment['SHIPFORGE_INVALID=ENVIRONMENT_NAME'] = 'rejected'
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 5) 'Fixture, environment, directory, pipe, and disposal failures must all survive aggregation'
    Assert-Cleanup ($script:removedPaths.Count -eq 1) 'Other failures must not skip identity deletion'
    Assert-Cleanup ($script:lifecycle.Killed -and $script:lifecycle.Waits -eq 2 -and $script:lifecycle.DisposeAttempts -eq 1) 'Other failures must not skip bounded helper termination and disposal'
    Confirm-CleanupCase

    foreach ($invalidRunId in @('', '../foreign', ('A' * 32), ('a' * 31))) {
        $case = New-CleanupCase
        $case.RunId = $invalidRunId
        $failures = @(Complete-SystemdRun @case)
        Assert-Cleanup ($failures.Count -eq 1 -and $failures[0].Contains('Invalid fixture run ID')) 'Invalid run IDs must be rejected'
        Assert-Cleanup ($script:fixtureCalls.Count -eq 0 -and $script:removedPaths.Count -eq 0) 'Invalid run IDs must never reach privileged fixture cleanup or deletion'
        Assert-Cleanup ($script:lifecycle.Closed -and $script:lifecycle.Disposed) 'Run ID refusal must still release the helper'
        Confirm-CleanupCase
    }

    $case = New-CleanupCase
    $case.RunId = [guid]::NewGuid().ToString('N')
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 1) 'A different valid run ID must not authorize the original identity directory'
    Assert-Cleanup ($script:fixtureCalls.Count -eq 0 -and $script:removedPaths.Count -eq 0) 'Mismatched run identity must stop destructive actions'
    Confirm-CleanupCase

    foreach ($wrongPath in @([System.IO.Path]::GetTempPath(), (Join-Path ([System.IO.Path]::GetTempPath()) 'another-directory'), 'relative-directory')) {
        $case = New-CleanupCase
        $case.Temporary = $wrongPath
        $failures = @(Complete-SystemdRun @case)
        Assert-Cleanup ($failures.Count -eq 1) 'An out-of-scope identity path must be refused'
        Assert-Cleanup ($script:fixtureCalls.Count -eq 0 -and $script:removedPaths.Count -eq 0) 'An incoherent run scope must stop destructive actions'
        Assert-Cleanup ($script:lifecycle.Disposed) 'Path refusal must still dispose the helper'
        Confirm-CleanupCase
    }

    foreach ($attributes in @(([System.IO.FileAttributes]::Directory -bor [System.IO.FileAttributes]::ReparsePoint), [System.IO.FileAttributes]::Normal)) {
        $case = New-CleanupCase
        $script:directoryAttributes = $attributes
        $failures = @(Complete-SystemdRun @case)
        Assert-Cleanup ($failures.Count -eq 1) 'Reparse points and non-directory replacements must be refused'
        Assert-Cleanup ($script:fixtureCalls.Count -eq 1 -and $script:removedPaths.Count -eq 0) 'Unsafe local path types must not be deleted; verified fixture cleanup should still run'
        Confirm-CleanupCase
    }

    $case = New-CleanupCase
    $script:failedInspection = $true
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 1 -and $script:removedPaths.Count -eq 0) 'Failed path inspection must stop deletion'
    Assert-Cleanup ($script:lifecycle.Disposed) 'Path inspection failure must not leak the helper'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $script:directoryExists = $false
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 0 -and $script:inspectedPaths.Count -eq 0 -and $script:removedPaths.Count -eq 0) 'An already absent identity directory needs no deletion'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $case.TemporaryCreated = $false
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 0 -and $script:inspectedPaths.Count -eq 0 -and $script:removedPaths.Count -eq 0) 'A directory this run did not create must never be deleted'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $case.FixtureAttempted = $false
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 0 -and $script:fixtureCalls.Count -eq 0 -and $script:removedPaths.Count -eq 1) 'Failure before fixture creation must only clean the owned local resources'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $case.KeepAliveStarted = $false
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'Unstarted helper cleanup should succeed'
    Assert-Cleanup (-not $script:lifecycle.Closed -and $script:lifecycle.Waits -eq 0 -and $script:lifecycle.KillAttempts -eq 0 -and $script:lifecycle.Disposed) 'An unstarted helper must only be disposed'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $case.KeepAlive = $null
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 0 -and $script:lifecycle.DisposeAttempts -eq 0) 'An unallocated helper needs no process operations'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $script:failedWaitAt = 1
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 1 -and $script:lifecycle.Disposed) 'Helper wait failure must be reported while still disposing'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $script:forceKill = $true
    $script:failedKill = $true
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 1 -and $script:lifecycle.KillAttempts -eq 1 -and $script:lifecycle.Disposed) 'Failed helper termination must be reported while still disposing'
    Confirm-CleanupCase

    $case = New-CleanupCase
    $script:forceKill = $true
    $script:neverExits = $true
    $failures = @(Complete-SystemdRun @case)
    Assert-Cleanup ($failures.Count -eq 1 -and $script:lifecycle.Waits -eq 2 -and $script:lifecycle.Disposed) 'A helper surviving termination must fail after the bounded second wait'
    Confirm-CleanupCase
} finally {
    $global:LASTEXITCODE = $savedNativeExitCode
    foreach ($name in $savedSentinels.Keys) {
        if ($null -eq $savedSentinels[$name]) {
            [Environment]::SetEnvironmentVariable($name, [System.Management.Automation.Language.NullString]::Value, 'Process')
        } else { [Environment]::SetEnvironmentVariable($name, $savedSentinels[$name], 'Process') }
    }
}
Write-Output "Passed: native command exit status/arguments and $script:passedCases isolated systemd cleanup scenarios"
