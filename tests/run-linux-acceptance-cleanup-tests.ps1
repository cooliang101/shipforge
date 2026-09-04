#requires -Version 7.0
# Exercises cleanup failure paths with in-memory doubles; never starts WSL/Docker
# and never deletes files. No Pester installation is required.
$ErrorActionPreference = 'Stop'
$savedNativeExitCode = $global:LASTEXITCODE
$parseErrors = $null
$tokens = $null
$runner = Join-Path $PSScriptRoot 'run-linux-acceptance.ps1'
$ast = [System.Management.Automation.Language.Parser]::ParseFile($runner, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw "Runner syntax errors: $parseErrors" }
foreach ($name in @('Invoke-FixtureDocker', 'Invoke-FixtureCase', 'Complete-FixtureRun')) {
    $definition = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $false)
    if ($null -eq $definition) { throw "Missing cleanup function $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}

function Assert-Cleanup {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

# No native Cargo process: validate exact test discovery and execution failures.
$script:caseCalls = 0
$script:caseListing = @()
$script:caseExit = 0
function cargo {
    $script:caseCalls++
    if ($args -contains '--list') { $global:LASTEXITCODE = 0; return $script:caseListing }
    $global:LASTEXITCODE = $script:caseExit
}
$noMatchRejected = $false
try { Invoke-FixtureCase -Case 'fixture_case' } catch { $noMatchRejected = $true }
Assert-Cleanup ($noMatchRejected -and $script:caseCalls -eq 1) 'Zero matching tests must fail before execution'
$script:caseListing = @('fixture_case: test', '1 test, 0 benchmarks')
Invoke-FixtureCase -Case 'fixture_case'
Assert-Cleanup ($script:caseCalls -eq 3) 'Exactly one discovered case must execute once'
$script:caseExit = 7
$failedCaseRejected = $false
try { Invoke-FixtureCase -Case 'fixture_case' } catch { $failedCaseRejected = $true }
Assert-Cleanup $failedCaseRejected 'Nonzero test execution must fail the runner'

# Check the production native-command wrapper without invoking a native process.
function wsl.exe { $global:LASTEXITCODE = 29; return 'simulated Docker failure' }
$nativeFailure = $false
try { Invoke-FixtureDocker -Arguments @('rm', '--force', 'disposable') | Out-Null }
catch { $nativeFailure = $_.Exception.Message.Contains('Fixture Docker command failed (rm)') }
Assert-Cleanup $nativeFailure 'Docker nonzero exit status must fail cleanup'

function Invoke-FixtureDocker {
    param([string[]]$Arguments)
    $script:dockerCalls.Add(($Arguments -join ' '))
    switch ($Arguments[0]) {
        'inspect' {
            if ($script:failedInspect -eq $Arguments[-1]) { throw 'simulated inspect failure' }
            if ($script:unverified -eq $Arguments[-1]) { return 'different-owner' }
            return $script:testRunId
        }
        'rm' { if ($script:failedRemove -eq $Arguments[-1]) { throw 'simulated container removal failure' } }
        'image' { if ($script:failedImage) { throw 'simulated image removal failure' } }
        default { throw 'Unexpected Docker command in cleanup test' }
    }
}
function Test-Path { param([string]$LiteralPath) return $true }
function Remove-Item {
    param([string]$LiteralPath, [switch]$Recurse, [switch]$Force)
    $script:removedPaths.Add($LiteralPath)
    if ($script:failedDirectory) { throw 'simulated identity directory deletion failure' }
}

function New-CleanupCase {
    $script:dockerCalls = [System.Collections.Generic.List[string]]::new()
    $script:removedPaths = [System.Collections.Generic.List[string]]::new()
    $script:failedRemove = ''
    $script:failedInspect = ''
    $script:unverified = ''
    $script:failedImage = $false
    $script:failedDirectory = $false
    $script:failedInput = $false
    $script:forceKill = $false
    $script:lifecycle = [pscustomobject]@{ Closed = $false; Waits = 0; Killed = $false; Disposed = $false }
    $pipe = [pscustomobject]@{}
    $pipe | Add-Member -MemberType ScriptMethod -Name Close -Value {
        $script:lifecycle.Closed = $true
        if ($script:failedInput) { throw 'simulated input close failure' }
    }
    $process = [pscustomobject]@{ StandardInput = $pipe }
    $process | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
        param($Timeout)
        $script:lifecycle.Waits++
        return (-not $script:forceKill -or $script:lifecycle.Killed)
    }
    $process | Add-Member -MemberType ScriptMethod -Name Kill -Value { $script:lifecycle.Killed = $true }
    $process | Add-Member -MemberType ScriptMethod -Name Dispose -Value { $script:lifecycle.Disposed = $true }
    return @{
        Containers = @('fixture-a', 'fixture-b')
        RunId = $script:testRunId
        Image = "fixture:$script:testRunId"
        ImageBuilt = $true
        Temporary = Join-Path ([System.IO.Path]::GetTempPath()) "shipforge-m1-$script:testRunId"
        TemporaryParent = [System.IO.Path]::GetTempPath()
        SavedEnvironment = @{ SHIPFORGE_CLEANUP_TEST_SENTINEL = $script:originalSentinel }
        KeepAlive = $process
        KeepAliveStarted = $true
    }
}

$script:testRunId = [guid]::NewGuid().ToString('N')
$script:originalSentinel = [Environment]::GetEnvironmentVariable('SHIPFORGE_CLEANUP_TEST_SENTINEL', 'Process')
try {
    $case = New-CleanupCase
    $env:SHIPFORGE_CLEANUP_TEST_SENTINEL = 'changed-by-test'
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'Successful cleanup reported a failure'
    Assert-Cleanup ($script:dockerCalls.Count -eq 5) 'Both containers and the image must be visited'
    Assert-Cleanup ($script:removedPaths.Count -eq 1) 'Exact temporary directory must be deleted'
    Assert-Cleanup ($script:lifecycle.Closed -and $script:lifecycle.Disposed) 'WSL helper must be closed and disposed'
    Assert-Cleanup ([Environment]::GetEnvironmentVariable('SHIPFORGE_CLEANUP_TEST_SENTINEL', 'Process') -eq $script:originalSentinel) 'Process environment must be restored'

    $case = New-CleanupCase
    $script:failedRemove = 'fixture-a'
    $script:failedImage = $true
    $script:failedDirectory = $true
    $script:failedInput = $true
    $script:forceKill = $true
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 4) 'Independent cleanup failures must be aggregated'
    Assert-Cleanup ($script:dockerCalls.Contains('rm --force fixture-b')) 'Later container cleanup must continue after a removal failure'
    Assert-Cleanup ($script:removedPaths.Count -eq 1) 'Identity cleanup must still be attempted after Docker errors'
    Assert-Cleanup ($script:lifecycle.Killed -and $script:lifecycle.Waits -eq 2 -and $script:lifecycle.Disposed) 'Pipe or directory failure must not skip helper termination/disposal'

    $case = New-CleanupCase
    $script:unverified = 'fixture-a'
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 1) 'Unverified ownership must be reported'
    Assert-Cleanup (-not $script:dockerCalls.Contains('rm --force fixture-a')) 'Unverified container must not be deleted'
    Assert-Cleanup ($script:dockerCalls.Contains('rm --force fixture-b')) 'Verified container must still be cleaned up'

    $case = New-CleanupCase
    $script:failedInspect = 'fixture-a'
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 1) 'Failed ownership observation must be reported'
    Assert-Cleanup (-not $script:dockerCalls.Contains('rm --force fixture-a')) 'Failed ownership observation must prevent deletion'
    Assert-Cleanup ($script:lifecycle.Disposed) 'Inspection failure must not leak the helper'

    $case = New-CleanupCase
    $case.KeepAliveStarted = $false
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'Unstarted helper should dispose without touching unavailable streams'
    Assert-Cleanup (-not $script:lifecycle.Closed -and $script:lifecycle.Waits -eq 0 -and $script:lifecycle.Disposed) 'Failed startup must only dispose the process object'

    $case = New-CleanupCase
    $case.Temporary = Join-Path $case.TemporaryParent 'not-the-owned-directory'
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 1) 'Out-of-scope directory must be refused'
    Assert-Cleanup ($script:removedPaths.Count -eq 0) 'Out-of-scope directory must not be deleted'
    Assert-Cleanup ($script:lifecycle.Closed -and $script:lifecycle.Disposed) 'Path refusal must still release the WSL helper'
} finally {
    $global:LASTEXITCODE = $savedNativeExitCode
    if ($null -eq $script:originalSentinel) {
        [Environment]::SetEnvironmentVariable('SHIPFORGE_CLEANUP_TEST_SENTINEL', [System.Management.Automation.Language.NullString]::Value, 'Process')
    } else { [Environment]::SetEnvironmentVariable('SHIPFORGE_CLEANUP_TEST_SENTINEL', $script:originalSentinel, 'Process') }
}
Write-Output 'Passed: exact test discovery/execution, native exit status and six isolated cleanup scenarios'
