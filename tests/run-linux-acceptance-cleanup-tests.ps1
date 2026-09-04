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

# Evaluate only the case-selection AST, never the fixture setup/cleanup body.
$caseInitialization = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
        $node.Left.Extent.Text -eq '$cases' -and $node.Operator -eq 'Equals'
}, $true)
Assert-Cleanup ($null -ne $caseInitialization) 'Missing isolated suite selection'
$suiteConditions = @($caseInitialization.Parent.Statements | Where-Object {
    $_ -is [System.Management.Automation.Language.IfStatementAst] -and
        $_.Clauses[0].Item1.Extent.Text -match '^\$Suite '
})
Assert-Cleanup ($suiteConditions.Count -eq 5) 'Expected four acceptance routes and one diagnostic route'
$selection = [scriptblock]::Create((@($caseInitialization.Extent.Text) + @($suiteConditions | ForEach-Object { $_.Extent.Text })) -join "`n")
$Suite = 'All'
. $selection
Assert-Cleanup (($cases -join '|') -eq (@(
    'real_linux_single_joint_rollback_health_failure_and_cancellation',
    'real_linux_exact_retention_partial_and_archive_only_retry',
    'real_linux_automatic_retention_keeps_latest_five',
    'management_acceptance::real_linux_management_history_rollback_and_connections'
) -join '|')) 'All must retain exactly the four deployment/management gates'
$Suite = 'ConnectionStability'
. $selection
Assert-Cleanup ($cases.Count -eq 1 -and $cases[0] -eq 'connection_stability::real_linux_fresh_connection_drop_stability') 'Diagnostic suite must select only its exact ignored case'

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
. (Join-Path $PSScriptRoot 'run-linux-acceptance-diagnostics.ps1')

# Construct, but never start, the real process wrapper to check safe argument handling.
$diagnosticProcess = New-FixtureDiagnosticProcess -Distribution 'fixture only' -Arguments @('logs', '--tail', '80', 'fixed-id')
Assert-Cleanup ($diagnosticProcess.StartInfo.FileName -eq 'wsl.exe') 'Diagnostics use the expected native helper'
Assert-Cleanup (-not $diagnosticProcess.StartInfo.UseShellExecute -and $diagnosticProcess.StartInfo.CreateNoWindow) 'Diagnostics never use a shell or visible window'
Assert-Cleanup ($diagnosticProcess.StartInfo.RedirectStandardOutput -and $diagnosticProcess.StartInfo.RedirectStandardError) 'Both diagnostic streams must be drained'
Assert-Cleanup ($diagnosticProcess.StartInfo.ArgumentList[1] -eq 'fixture only') 'Distribution is one argument, not shell text'
$diagnosticProcess.Dispose()

function New-DiagnosticProcessCase {
    $script:diagnosticStdout = 'stdout'
    $script:diagnosticStderr = 'stderr'
    $script:diagnosticExitImmediately = $true
    $script:diagnosticExitCode = 0
    $script:diagnosticStartSucceeds = $true
    $script:diagnosticLifecycle = [pscustomobject]@{ Started = $false; Killed = $false; Tree = $false; Disposed = $false; WaitMs = 0; OutBytes = 0; ErrBytes = 0 }
}

function New-FixtureDiagnosticProcess {
    param([string]$Distribution, [string[]]$Arguments)
    $process = [pscustomobject]@{
        StandardOutput = [pscustomobject]@{ BaseStream = [System.IO.MemoryStream]::new([System.Text.Encoding]::UTF8.GetBytes($script:diagnosticStdout)) }
        StandardError = [pscustomobject]@{ BaseStream = [System.IO.MemoryStream]::new([System.Text.Encoding]::UTF8.GetBytes($script:diagnosticStderr)) }
    }
    $process | Add-Member -MemberType ScriptProperty -Name HasExited -Value { $script:diagnosticExitImmediately -or $script:diagnosticLifecycle.Killed }
    $process | Add-Member -MemberType ScriptProperty -Name ExitCode -Value { $script:diagnosticExitCode }
    $process | Add-Member -MemberType ScriptMethod -Name Start -Value {
        $script:diagnosticLifecycle.Started = $true
        return $script:diagnosticStartSucceeds
    }
    $process | Add-Member -MemberType ScriptMethod -Name Kill -Value {
        param($EntireTree)
        $script:diagnosticLifecycle.Killed = $true
        $script:diagnosticLifecycle.Tree = $EntireTree
    }
    $process | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
        param($Milliseconds)
        $script:diagnosticLifecycle.WaitMs = $Milliseconds
        return $script:diagnosticLifecycle.Killed
    }
    $process | Add-Member -MemberType ScriptMethod -Name Dispose -Value {
        $script:diagnosticLifecycle.OutBytes = $this.StandardOutput.BaseStream.Position
        $script:diagnosticLifecycle.ErrBytes = $this.StandardError.BaseStream.Position
        $this.StandardOutput.BaseStream.Dispose()
        $this.StandardError.BaseStream.Dispose()
        $script:diagnosticLifecycle.Disposed = $true
    }
    return $process
}

New-DiagnosticProcessCase
$script:diagnosticStdout = 'a' * 10000
$script:diagnosticStderr = 'b' * 10000
$result = Invoke-FixtureDiagnosticCommand -Distribution fixture -Arguments @('logs') -TimeoutMilliseconds 1000 -MaxOutputBytes 1024
Assert-Cleanup ($result.ExitCode -eq 0 -and -not $result.TimedOut -and $result.Truncated) 'Excess diagnostic output must be marked truncated'
Assert-Cleanup (($result.Stdout.Length + $result.Stderr.Length) -eq 1024) 'Combined retained output must respect the byte budget'
Assert-Cleanup ($script:diagnosticLifecycle.OutBytes -eq 10000 -and $script:diagnosticLifecycle.ErrBytes -eq 10000) 'Both streams drain fully after retention is capped'
Assert-Cleanup ($script:diagnosticLifecycle.Disposed -and -not $script:diagnosticLifecycle.Killed) 'Completed helpers must be disposed, not killed'

New-DiagnosticProcessCase
$script:diagnosticExitImmediately = $false
$result = Invoke-FixtureDiagnosticCommand -Distribution fixture -Arguments @('logs') -TimeoutMilliseconds 25 -MaxOutputBytes 1024
Assert-Cleanup ($result.TimedOut -and $null -eq $result.ExitCode) 'A stuck diagnostic helper must time out, not claim success'
Assert-Cleanup ($script:diagnosticLifecycle.Killed -and $script:diagnosticLifecycle.Tree -and $script:diagnosticLifecycle.WaitMs -le 250 -and $script:diagnosticLifecycle.Disposed) 'Only the owned helper is terminated with a bounded exit wait'

New-DiagnosticProcessCase
$script:diagnosticExitCode = 19
$result = Invoke-FixtureDiagnosticCommand -Distribution fixture -Arguments @('logs')
Assert-Cleanup ($result.ExitCode -eq 19) 'Diagnostic nonzero exit must be preserved'
New-DiagnosticProcessCase
$script:diagnosticStartSucceeds = $false
$startRefused = $false
try { Invoke-FixtureDiagnosticCommand -Distribution fixture -Arguments @('logs') | Out-Null } catch { $startRefused = $true }
Assert-Cleanup ($startRefused -and $script:diagnosticLifecycle.Disposed -and -not $script:diagnosticLifecycle.Killed) 'Failed startup disposes without killing an unstarted helper'

function New-DiagnosticCollectionCase {
    $script:diagnosticCalls = [System.Collections.Generic.List[object]]::new()
    $script:diagnosticIdentity = ('a' * 64) + '|' + $script:testRunId
    $script:diagnosticIdentityTruncated = $false
    $script:diagnosticFail = ''
    $script:diagnosticTimeout = ''
    $script:diagnosticNonzero = ''
}

function Invoke-FixtureDiagnosticCommand {
    param([string]$Distribution, [string[]]$Arguments, [int]$TimeoutMilliseconds, [int]$MaxOutputBytes)
    $script:diagnosticCalls.Add([pscustomobject]@{ Arguments = $Arguments; Timeout = $TimeoutMilliseconds; Limit = $MaxOutputBytes })
    $kind = if ($Arguments[0] -eq 'inspect' -and $Arguments[2].Contains('shipforge.test.run')) { 'identity' } else { $Arguments[0] }
    if ($script:diagnosticFail -eq $kind) { throw 'private-diagnostic-error-must-not-be-echoed' }
    $stdout = switch ($kind) {
        'identity' { $script:diagnosticIdentity }
        'logs' { "Accepted publickey for deploy`n" + [char]27 + '[2Jfixture log' }
        'inspect' { 'status=running running=true restarting=false oom=false pid=1 exit=0' }
        'exec' { "loglevel VERBOSE`nmaxstartups 10:30:100`nmaxsessions 10`nlogingracetime 120`nhostkey /fixture/private-key-sentinel`nsetenv SECRET=private-secret-sentinel" }
        default { throw 'Unexpected diagnostic command' }
    }
    return [pscustomobject]@{
        Stdout = $stdout; Stderr = ''; ExitCode = $(if ($script:diagnosticNonzero -eq $kind) { 17 } else { 0 })
        TimedOut = ($script:diagnosticTimeout -eq $kind); Truncated = ($kind -eq 'identity' -and $script:diagnosticIdentityTruncated)
    }
}

New-DiagnosticCollectionCase
$ownedName = "shipforge-m1-$script:testRunId-a"
$text = (Get-FixtureDiagnostics -Container 'not-our-container' -RunId $script:testRunId -Distribution fixture) -join "`n"
Assert-Cleanup ($script:diagnosticCalls.Count -eq 0 -and $text.Contains('refused')) 'An unrelated container must not even be inspected'
$script:diagnosticIdentity = ('a' * 64) + '|different-run'
Get-FixtureDiagnostics -Container $ownedName -RunId $script:testRunId -Distribution fixture | Out-Null
Assert-Cleanup ($script:diagnosticCalls.Count -eq 1) 'Wrong ownership must prevent all logs, state and exec reads'
New-DiagnosticCollectionCase
$script:diagnosticIdentityTruncated = $true
Get-FixtureDiagnostics -Container $ownedName -RunId $script:testRunId -Distribution fixture | Out-Null
Assert-Cleanup ($script:diagnosticCalls.Count -eq 1) 'Truncated identity evidence must fail closed'
foreach ($failure in @('throw', 'timeout', 'nonzero')) {
    New-DiagnosticCollectionCase
    switch ($failure) {
        'throw' { $script:diagnosticFail = 'identity' }
        'timeout' { $script:diagnosticTimeout = 'identity' }
        'nonzero' { $script:diagnosticNonzero = 'identity' }
    }
    $text = (Get-FixtureDiagnostics -Container $ownedName -RunId $script:testRunId -Distribution fixture) -join "`n"
    Assert-Cleanup ($script:diagnosticCalls.Count -eq 1 -and -not $text.Contains('private-diagnostic-error')) 'Failed identity inspection must neither read container content nor echo raw errors'
}

New-DiagnosticCollectionCase
$text = (Get-FixtureDiagnostics -Container $ownedName -RunId $script:testRunId -Distribution fixture) -join "`n"
Assert-Cleanup ($script:diagnosticCalls.Count -eq 4) 'Verified fixture should produce exactly identity plus three diagnostic reads'
Assert-Cleanup ($text.Contains('Accepted publickey') -and $text.Contains('loglevel VERBOSE') -and $text.Contains('maxstartups 10:30:100')) 'Allowed useful diagnostics must remain visible'
Assert-Cleanup (-not $text.Contains('private-key-sentinel') -and -not $text.Contains('private-secret-sentinel') -and -not $text.Contains([char]27)) 'Key paths, environment and terminal control bytes must never be rendered'
foreach ($call in $script:diagnosticCalls) {
    Assert-Cleanup ($call.Timeout -gt 0 -and $call.Timeout -le 5000 -and $call.Limit -le 65536) 'Every diagnostic command must have bounded time and retained output'
}
foreach ($call in @($script:diagnosticCalls | Select-Object -Skip 1)) {
    Assert-Cleanup ($call.Arguments -contains ('a' * 64) -and $call.Arguments -notcontains $ownedName) 'Post-verification reads must use immutable container ID, not a reusable name'
}
$logArguments = $script:diagnosticCalls[1].Arguments
Assert-Cleanup (($logArguments -join ' ') -eq "logs --tail 80 --timestamps $('a' * 64)") 'Logs must be bounded, timestamped, and never followed'
Assert-Cleanup (($script:diagnosticCalls[3].Arguments -join ' ') -eq "exec $('a' * 64) /usr/sbin/sshd -T -f /fixture/sshd_config") 'Only read-only sshd configuration validation may execute'

foreach ($failure in @('throw', 'timeout', 'nonzero')) {
    New-DiagnosticCollectionCase
    switch ($failure) {
        'throw' { $script:diagnosticFail = 'logs' }
        'timeout' { $script:diagnosticTimeout = 'logs' }
        'nonzero' { $script:diagnosticNonzero = 'logs' }
    }
    $case = New-CleanupCase
    $originalFailure = [Exception]::new('original acceptance failure')
    $operationError = $originalFailure
    try { $text = (Get-FixtureDiagnostics -Container $ownedName -RunId $script:testRunId -Distribution fixture) -join "`n" }
    finally { $failures = @(Complete-FixtureRun @case) }
    Assert-Cleanup ($script:diagnosticCalls.Count -eq 4 -and $text.Contains('container state')) 'One failed diagnostic must not prevent the other read-only checks'
    Assert-Cleanup (-not $text.Contains('private-diagnostic-error')) 'Diagnostic failures must not echo raw process exceptions'
    Assert-Cleanup ([object]::ReferenceEquals($operationError, $originalFailure)) 'Diagnostics must preserve the original acceptance error'
    Assert-Cleanup ($failures.Count -eq 0 -and $script:lifecycle.Disposed -and $script:removedPaths.Count -eq 1) 'Diagnostic failure must not prevent finally cleanup'
}

Write-Output 'Passed: exact discovery, cleanup scenarios, bounded diagnostic process and fixture-only diagnostic collection'
