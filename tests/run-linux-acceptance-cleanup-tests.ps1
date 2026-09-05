#requires -Version 7.0
# Exercises cleanup failure paths with in-memory doubles; never starts WSL/Docker
# and never deletes files. No Pester installation is required.
$ErrorActionPreference = 'Stop'
$savedNativeExitCode = $global:LASTEXITCODE
$parseErrors = $null
$tokens = $null
$runner = Join-Path $PSScriptRoot 'run-linux-acceptance.ps1'
$boundedNativeHelper = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'support/bounded-native-command.ps1'))
$ast = [System.Management.Automation.Language.Parser]::ParseFile($runner, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw "Runner syntax errors: $parseErrors" }
foreach ($name in @(
    'New-AcceptanceNativeProcess',
    'Initialize-AcceptanceJobInterop',
    'New-BoundedNativeProcess',
    'Invoke-BoundedNativeCommand',
    'Invoke-CheckedNativeCommand',
    'Invoke-FixtureDocker',
    'Invoke-WslCommand',
    'Invoke-SshKeygen',
    'Invoke-CargoCommand',
    'Invoke-FixtureCase',
    'Invoke-ReleaseGate',
    'Get-SshPublicKeyFingerprint',
    'Invoke-SshAdd',
    'Assert-ReleaseGateAgentReady',
    'Get-FixtureCleanupCommandTimeout',
    'Invoke-FixtureDockerReadiness',
    'Get-FixtureDockerResourceObservation',
    'Complete-FixtureDockerResources',
    'Complete-FixtureRun'
)) {
    $definition = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $false)
    if ($null -eq $definition) { throw "Missing cleanup function $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}

function Assert-Cleanup {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

# The real process builder separates every argument and never opens a shell.
$nativeProcess = New-AcceptanceNativeProcess -FilePath 'fixture.exe' -Arguments @('one value', '--literal')
Assert-Cleanup ($nativeProcess.StartInfo.FileName -eq 'fixture.exe') 'Native wrapper must retain the exact executable'
Assert-Cleanup (-not $nativeProcess.StartInfo.UseShellExecute -and $nativeProcess.StartInfo.CreateNoWindow) 'Native wrapper must not invoke a shell or visible window'
Assert-Cleanup ($nativeProcess.StartInfo.RedirectStandardOutput -and $nativeProcess.StartInfo.RedirectStandardError) 'Native wrapper must drain both output streams'
Assert-Cleanup ($nativeProcess.StartInfo.ArgumentList[0] -eq 'one value') 'Native arguments must remain separate from shell text'
$nativeProcess.Dispose()

$pwshExecutable = Join-Path $PSHOME 'pwsh.exe'
$nativeResult = Invoke-BoundedNativeCommand -FilePath $pwshExecutable -Arguments @(
    '-NoLogo', '-NoProfile', '-NonInteractive', '-Command',
    '[Console]::Out.Write("stdout"); [Console]::Error.Write("stderr")'
) -TimeoutMilliseconds 5000 -MaxOutputBytes 1024
Assert-Cleanup ($nativeResult.ExitCode -eq 0 -and -not $nativeResult.TimedOut) 'A completed native helper must preserve success'
Assert-Cleanup ($nativeResult.Stdout -eq 'stdout' -and $nativeResult.Stderr -eq 'stderr') 'A completed native helper must capture both streams'

$lateCompletionTimer = [System.Diagnostics.Stopwatch]::StartNew()
$nativeResult = Invoke-BoundedNativeCommand -FilePath $pwshExecutable -Arguments @(
    '-NoLogo', '-NoProfile', '-NonInteractive', '-Command',
    'Start-Sleep -Milliseconds 510'
) -TimeoutMilliseconds 500 -MaxOutputBytes 1024
Assert-Cleanup ($nativeResult.TimedOut -and $null -eq $nativeResult.ExitCode) 'A native command completing after its absolute deadline must fail closed as timed out'
Assert-Cleanup ($lateCompletionTimer.ElapsedMilliseconds -lt 2000) 'Late native completion termination and Job Object reap must remain bounded'

$descendantScript = @'
$child = [System.Diagnostics.Process]::new()
$child.StartInfo.FileName = Join-Path $PSHOME 'pwsh.exe'
$child.StartInfo.UseShellExecute = $false
$child.StartInfo.CreateNoWindow = $true
foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 15')) {
    $child.StartInfo.ArgumentList.Add($argument)
}
if (-not $child.Start()) { exit 91 }
$child.Dispose()
'@
$nativeTimer = [System.Diagnostics.Stopwatch]::StartNew()
$nativeResult = Invoke-BoundedNativeCommand -FilePath $pwshExecutable -Arguments @(
    '-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $descendantScript
) -TimeoutMilliseconds 3000 -MaxOutputBytes 1024
Assert-Cleanup ($nativeResult.TimedOut -and $null -eq $nativeResult.ExitCode) 'A descendant that outlives its exited parent must report timeout rather than success'
Assert-Cleanup $nativeResult.RootExitedWithActiveDescendants 'The regression must observe an exited wrapper root while its descendant remains active'
Assert-Cleanup ($nativeTimer.ElapsedMilliseconds -lt 6000) 'Owned descendant termination and Job Object reap must remain bounded'

# All command-specific wrappers below use an in-memory bounded-command double.
$Distribution = 'fixture distribution'
$script:nativeCalls = [System.Collections.Generic.List[object]]::new()
$script:nativeStdout = ''
$script:nativeExit = 0
$script:nativeListExit = 0
$script:nativeRunExit = 0
$script:nativeTimedOut = $false
$script:caseListing = @()
function Invoke-BoundedNativeCommand {
    param([string]$FilePath, [string[]]$Arguments, [int]$TimeoutMilliseconds, [int]$MaxOutputBytes)
    $script:nativeCalls.Add([pscustomobject]@{
        FilePath = $FilePath; Arguments = $Arguments; Timeout = $TimeoutMilliseconds; Limit = $MaxOutputBytes
    })
    $isCargo = $FilePath -eq 'cargo.exe'
    $isList = $isCargo -and $Arguments -contains '--list'
    $exitCode = if ($isList) { $script:nativeListExit } elseif ($isCargo) { $script:nativeRunExit } else { $script:nativeExit }
    $stdout = if ($isList) { $script:caseListing -join "`n" } else { $script:nativeStdout }
    return [pscustomobject]@{
        ExitCode = $exitCode; TimedOut = $script:nativeTimedOut; Truncated = $false; Stdout = $stdout; Stderr = ''
    }
}

$script:nativeTimedOut = $true
$boundedTimeoutRejected = $false
try { Invoke-SshKeygen -Arguments @('-lf', 'fixture.pub') | Out-Null }
catch { $boundedTimeoutRejected = $_.Exception.Message.Contains('timed out') }
Assert-Cleanup $boundedTimeoutRejected 'A command-specific wrapper must reject a bounded timeout'
$script:nativeTimedOut = $false

# No native Cargo process: validate exact discovery, execution, and deadlines.
$script:caseListing = @()
$script:nativeCalls.Clear()
$noMatchRejected = $false
try { Invoke-FixtureCase -Case 'fixture_case' } catch { $noMatchRejected = $true }
Assert-Cleanup ($noMatchRejected -and $script:nativeCalls.Count -eq 1) 'Zero matching tests must fail before execution'
$script:caseListing = @('fixture_case: test', '1 test, 0 benchmarks')
$script:nativeCalls.Clear()
Invoke-FixtureCase -Case 'fixture_case'
Assert-Cleanup ($script:nativeCalls.Count -eq 2) 'Exactly one discovered case must execute once'
Assert-Cleanup ($script:nativeCalls[0].Timeout -eq 600000 -and $script:nativeCalls[1].Timeout -eq 300000) 'Cargo discovery and execution must have practical independent deadlines'
$script:nativeRunExit = 7
$failedCaseRejected = $false
try { Invoke-FixtureCase -Case 'fixture_case' } catch { $failedCaseRejected = $true }
Assert-Cleanup $failedCaseRejected 'Nonzero test execution must fail the runner'
$script:nativeRunExit = 0

$releaseCase = 'qa01_release_gate_validates_host_key_rotation_agent_sftp_and_cancellation'
$script:caseListing = @("${releaseCase}: test", '1 test, 0 benchmarks')
$script:nativeCalls.Clear()
Invoke-ReleaseGate
Assert-Cleanup ($script:nativeCalls.Count -eq 2) 'Release gate must be discovered and executed exactly once'
Assert-Cleanup (($script:nativeCalls[0].Arguments -join ' ') -eq "test --locked --release --target x86_64-pc-windows-gnu --test linux_ssh_release_gate $releaseCase -- --ignored --exact --list") 'Release gate discovery must pin its exact release-profile GNU target'
Assert-Cleanup (($script:nativeCalls[1].Arguments -join ' ') -eq "test --locked --release --target x86_64-pc-windows-gnu --test linux_ssh_release_gate $releaseCase -- --ignored --exact --nocapture --test-threads=1") 'Release gate execution must retain the exact GNU target and ignored single-thread selection'
Assert-Cleanup ($script:nativeCalls[0].Timeout -eq 600000 -and $script:nativeCalls[1].Timeout -eq 150000) 'Release build and live gate must have separate deadlines with room for forced-abort reconciliation'
$script:caseListing = @()
$missingReleaseGateRejected = $false
try { Invoke-ReleaseGate } catch { $missingReleaseGateRejected = $true }
Assert-Cleanup $missingReleaseGateRejected 'Zero matching release gates must fail before execution'

# Evaluate only the case-selection AST, never the fixture setup/cleanup body.
$caseInitialization = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
        $node.Left.Extent.Text -eq '$cases' -and $node.Operator -eq 'Equals'
}, $true)
Assert-Cleanup ($null -ne $caseInitialization) 'Missing isolated suite selection'
$releaseGateInitialization = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
        $node.Left.Extent.Text -eq '$runReleaseGate' -and $node.Operator -eq 'Equals'
}, $true)
Assert-Cleanup ($null -ne $releaseGateInitialization) 'Missing isolated ReleaseGate selection'
$suiteConditions = @($caseInitialization.Parent.Statements | Where-Object {
    $_ -is [System.Management.Automation.Language.IfStatementAst] -and
        $_.Clauses[0].Item1.Extent.Text -match '^\$Suite '
})
Assert-Cleanup ($suiteConditions.Count -eq 5) 'Expected four acceptance routes and one diagnostic route'
$selection = [scriptblock]::Create((@($caseInitialization.Extent.Text) + @($suiteConditions | ForEach-Object { $_.Extent.Text })) -join "`n")
$Suite = 'All'
. ([scriptblock]::Create($releaseGateInitialization.Extent.Text))
. $selection
Assert-Cleanup (($cases -join '|') -eq (@(
    'real_linux_single_joint_rollback_health_failure_and_cancellation',
    'real_linux_exact_retention_partial_and_archive_only_retry',
    'real_linux_automatic_retention_keeps_latest_five',
    'management_acceptance::real_linux_management_history_rollback_and_connections'
) -join '|')) 'All must retain exactly the four deployment/management gates'
Assert-Cleanup (-not $runReleaseGate) 'All must not acquire the Windows ssh-agent prerequisite'
$Suite = 'ConnectionStability'
. ([scriptblock]::Create($releaseGateInitialization.Extent.Text))
. $selection
Assert-Cleanup ($cases.Count -eq 1 -and $cases[0] -eq 'connection_stability::real_linux_fresh_connection_drop_stability') 'Diagnostic suite must select only its exact ignored case'
Assert-Cleanup (-not $runReleaseGate) 'ConnectionStability must not select the release gate'
$Suite = 'ReleaseGate'
. ([scriptblock]::Create($releaseGateInitialization.Extent.Text))
. $selection
Assert-Cleanup ($cases.Count -eq 0 -and $runReleaseGate) 'ReleaseGate must select only the release-profile gate'

$runnerSource = Get-Content -LiteralPath $runner -Raw
$boundedRunnerDefinition = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq 'Invoke-BoundedNativeCommand'
}, $false).Extent.Text
Assert-Cleanup (
    $boundedRunnerDefinition.IndexOf('if ($timer.ElapsedMilliseconds -ge $TimeoutMilliseconds)') -lt
        $boundedRunnerDefinition.IndexOf('if ($ended[0] -and $ended[1]')
) 'The bounded runner must test its absolute deadline before accepting native completion'
foreach ($name in @(
    'SHIPFORGE_QA01_OPENSSH',
    'SHIPFORGE_QA01_SSH_HOST',
    'SHIPFORGE_QA01_SSH_PORT',
    'SHIPFORGE_QA01_SSH_USER',
    'SHIPFORGE_QA01_SSH_CURRENT_HOST_KEY',
    'SHIPFORGE_QA01_SSH_PREVIOUS_HOST_KEY',
    'SHIPFORGE_QA01_SSH_IDENTITY_FILE',
    'SHIPFORGE_QA01_SSH_AGENT_FINGERPRINT'
)) {
    Assert-Cleanup ($runnerSource.Contains("'$name'")) "Runner must save and restore $name"
}
Assert-Cleanup ($runnerSource.Contains('$env:SHIPFORGE_QA01_SSH_PORT = $env:SHIPFORGE_TEST_PORT_A')) 'Release gate must contact endpoint A'
Assert-Cleanup ($runnerSource.Contains('$env:SHIPFORGE_QA01_SSH_CURRENT_HOST_KEY = $env:SHIPFORGE_TEST_HOST_KEY_A')) 'Endpoint A must supply the current Host Key'
Assert-Cleanup ($runnerSource.Contains('$env:SHIPFORGE_QA01_SSH_PREVIOUS_HOST_KEY = $env:SHIPFORGE_TEST_HOST_KEY_B')) 'Endpoint B must supply only the stale Host Key'
Assert-Cleanup ($runnerSource.Contains("`$identityKey = Join-Path `$temporary 'identity_ed25519'")) 'Release gate must create a dedicated IdentityFile key'
Assert-Cleanup ($runnerSource.Contains("`$agentKey = Join-Path `$temporary 'agent_ed25519'")) 'Release gate must create a separate Agent key'
Assert-Cleanup ($runnerSource.Contains('Invoke-SshAdd -Arguments @($agentKey)')) 'Only the dedicated Agent private key may be added to the Agent'
Assert-Cleanup ($runnerSource.Contains('$env:SHIPFORGE_QA01_SSH_IDENTITY_FILE = $identityKey')) 'The release gate IdentityFile must remain the separate identity key'
Assert-Cleanup ($runnerSource.Contains('$agentIdentity = "$agentKey.pub"')) 'Cleanup ownership must retain the dedicated Agent public key'
Assert-Cleanup ($runnerSource.IndexOf('$agentIdentity = "$agentKey.pub"') -lt $runnerSource.IndexOf('Invoke-SshAdd -Arguments @($agentKey)')) 'Agent cleanup ownership must be recorded before an add can time out after taking effect'
Assert-Cleanup ($runnerSource.Contains("Invoke-SshAdd -Arguments @('-d', `$resolvedIdentity) -TimeoutMilliseconds 10000")) 'Cleanup must remove the exact disposable Agent identity under a deadline'
Assert-Cleanup ($runnerSource.Contains('[string]::Join("`n", $authorizedKeyLines)')) 'Both generated public keys must be assembled into one owned authorized_keys file'
Assert-Cleanup ($runnerSource.Contains('environment="SHIPFORGE_QA01_AUTH=identity-file"')) 'IdentityFile authorized-key entry must carry its exact authentication marker'
Assert-Cleanup ($runnerSource.Contains('environment="SHIPFORGE_QA01_AUTH=ssh-agent"')) 'Agent authorized-key entry must carry its distinct authentication marker'
Assert-Cleanup ($runnerSource.Contains('[System.Text.UTF8Encoding]::new($false)')) 'Copied authorized_keys must use BOM-free UTF-8 with explicit Linux newlines'
Assert-Cleanup ($runnerSource.Contains('@(''cp'', $authorizedKeysLinux, "${container}:/fixture/authorized_keys")')) 'The owned authorized_keys file must be copied into each stopped container'
Assert-Cleanup ($runnerSource -cnotmatch '(?i)--mount') 'Runner must never expose a host key through a Docker bind mount'
Assert-Cleanup ($runnerSource.Contains("@('container', 'create'")) 'Endpoints must be created in the stopped state before key copy'
Assert-Cleanup ($runnerSource.Contains("@('container', 'start', `$container)")) 'Endpoints must start only after authorized_keys is copied'
Assert-Cleanup ($runnerSource.Contains('test -s /run/sshd/shipforge.pid && kill -0 "$(cat /run/sshd/shipforge.pid)"')) 'Endpoint readiness must prove the container sshd PID is live'
Assert-Cleanup ($runnerSource.IndexOf('test -s /run/sshd/shipforge.pid') -lt $runnerSource.IndexOf("`$fingerprintOutput = Invoke-FixtureDocker")) 'Each readiness iteration must check live sshd before accepting its Host Key fingerprint'
Assert-Cleanup ($runnerSource.Contains('Invoke-FixtureDockerReadiness -Clock $readyTimer')) 'Every endpoint probe and port read must share one monotonic readiness budget'
Assert-Cleanup ($runnerSource.IndexOf('$keepAliveStarted = $true') -lt $runnerSource.IndexOf('$keepAlive.Start()')) 'WSL keepalive cleanup ownership must be registered before process start'
Assert-Cleanup ($runnerSource -cnotmatch '(?m)^\s*&?\s*ssh-add\s+-D(?:\s|$)') 'Runner must never clear unrelated SSH Agent identities'
Assert-Cleanup ($runnerSource -notmatch '(?im)^\s*(Set-Service|Start-Service)\b') 'Runner must not persistently change the Windows ssh-agent service'
$directNativeCommands = @($ast.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.CommandAst] -and
        $node.GetCommandName() -cin @('cargo', 'cargo.exe', 'ssh-add', 'ssh-add.exe', 'ssh-keygen', 'ssh-keygen.exe', 'wsl', 'wsl.exe', 'docker', 'docker.exe')
}, $true))
Assert-Cleanup ($directNativeCommands.Count -eq 0) 'All WSL, Docker, Cargo, ssh-add, and ssh-keygen calls must use the bounded process wrapper'

$readinessClock = [System.Diagnostics.Stopwatch]::StartNew()
$readinessTimeout = Get-FixtureCleanupCommandTimeout -Clock $readinessClock -BudgetMilliseconds 100 -MaximumMilliseconds 50
Assert-Cleanup ($readinessTimeout -gt 0 -and $readinessTimeout -le 50) 'A readiness probe must be clipped to its per-command cap'
Start-Sleep -Milliseconds 110
Assert-Cleanup ((Get-FixtureCleanupCommandTimeout -Clock $readinessClock -BudgetMilliseconds 100 -MaximumMilliseconds 50) -eq 0) 'An elapsed monotonic readiness budget must reject later probes'

# Exercise the actual ssh-add wrapper through the bounded-command double.
$script:nativeCalls.Clear()
$script:nativeExit = 1
Invoke-SshAdd -Arguments @('-l') -AllowedExitCodes @(0, 1)
Assert-Cleanup ($script:nativeCalls.Count -eq 1 -and $script:nativeCalls[0].FilePath -eq 'ssh-add.exe') 'ssh-add must use the bounded native wrapper'
Assert-Cleanup (($script:nativeCalls[0].Arguments -join ' ') -eq '-l' -and $script:nativeCalls[0].Timeout -eq 10000) 'Empty running Agent probe must have an explicit deadline'
$script:nativeExit = 2
$unavailableAgentRejected = $false
try { Invoke-SshAdd -Arguments @('-l') -AllowedExitCodes @(0, 1) } catch { $unavailableAgentRejected = $true }
Assert-Cleanup $unavailableAgentRejected 'Unavailable ssh-agent must fail its native wrapper'

$script:agentServiceStatus = 'Stopped'
function Get-Service {
    param([string]$Name, [System.Management.Automation.ActionPreference]$ErrorAction)
    return [pscustomobject]@{ Status = $script:agentServiceStatus }
}
$script:nativeCalls.Clear()
$stoppedAgentRejected = $false
try { Assert-ReleaseGateAgentReady } catch {
    $stoppedAgentRejected = $_.Exception.Message.Contains('requires the Windows OpenSSH Authentication Agent')
}
Assert-Cleanup ($stoppedAgentRejected -and $script:nativeCalls.Count -eq 0) 'Stopped Windows ssh-agent must fail before any Agent command'
$script:agentServiceStatus = 'Running'
$script:nativeExit = 1
Assert-ReleaseGateAgentReady
Assert-Cleanup ($script:nativeCalls.Count -eq 1 -and ($script:nativeCalls[0].Arguments -join ' ') -eq '-l') 'Running empty Windows ssh-agent must satisfy the explicit prerequisite'

# Exercise the actual Docker wrapper through the same bounded-command double.
$script:nativeCalls.Clear()
$script:nativeExit = 29
$nativeFailure = $false
try { Invoke-FixtureDocker -Arguments @('rm', '--force', 'disposable') -TimeoutMilliseconds 15000 | Out-Null }
catch { $nativeFailure = $_.Exception.Message.Contains('failed with exit code 29') }
Assert-Cleanup $nativeFailure 'Docker nonzero exit status must fail cleanup'
Assert-Cleanup ($script:nativeCalls[0].FilePath -eq 'wsl.exe' -and $script:nativeCalls[0].Timeout -eq 15000) 'Docker must run through bounded WSL with the caller deadline'
Assert-Cleanup (($script:nativeCalls[0].Arguments -join ' ') -eq '-d fixture distribution --exec docker -H unix:///var/run/docker.sock rm --force disposable') 'Docker arguments must remain separate and fixed-prefix scoped'

# Cleanup doubles record the shrinking shared deadline and continue after timeouts.
$script:sshNativeExit = 0
$script:sshNativeCalls = [System.Collections.Generic.List[string]]::new()
$script:sshTimeouts = [System.Collections.Generic.List[int]]::new()
function Invoke-SshAdd {
    param([string[]]$Arguments, [int[]]$AllowedExitCodes = @(0), [int]$TimeoutMilliseconds = 10000)
    $script:sshNativeCalls.Add(($Arguments -join ' '))
    $script:sshTimeouts.Add($TimeoutMilliseconds)
    if ($script:sshNativeExit -notin $AllowedExitCodes) { throw 'simulated ssh-add failure' }
}

function Invoke-FixtureDocker {
    param([string[]]$Arguments, [int]$TimeoutMilliseconds = 30000, [switch]$PublishOutput)
    $script:dockerCalls.Add(($Arguments -join ' '))
    $script:dockerTimeouts.Add($TimeoutMilliseconds)
    $operation = "$($Arguments[0]) $($Arguments[1])"
    switch ($operation) {
        'container ls' {
            $filter = [string]$Arguments[-1]
            if (-not $filter.StartsWith('name=^/') -or -not $filter.EndsWith('$')) {
                throw 'simulated cleanup received an inexact container-name filter'
            }
            $name = $filter.Substring(7, $filter.Length - 8)
            if (-not $script:containerIds.ContainsKey($name)) { return '' }
            if ($script:containerObservationDelayMilliseconds -gt 0) {
                Start-Sleep -Milliseconds $script:containerObservationDelayMilliseconds
            }
            if ($script:lateContainerName -eq $name -and
                $script:lateContainerAfterMilliseconds -gt 0 -and
                $null -ne $script:cleanupScenarioClock -and
                $script:cleanupScenarioClock.ElapsedMilliseconds -ge $script:lateContainerAfterMilliseconds) {
                $script:presentContainers[$name] = $true
                $script:lateContainerAfterMilliseconds = 0
            }
            if ($script:timedOutInspect -eq $name) { throw 'simulated bounded inspect timeout' }
            if ($script:failedInspect -eq $name) { throw 'simulated inspect failure' }
            $count = 1 + $(if ($script:containerObservationCounts.ContainsKey($name)) { $script:containerObservationCounts[$name] } else { 0 })
            $script:containerObservationCounts[$name] = $count
            if ($script:deadlineConsumer -eq $name -and $count -eq $script:deadlineConsumerObservation) {
                Start-Sleep -Milliseconds $script:deadlineConsumerSleepMilliseconds
                throw 'simulated probe consumed the final cleanup sweep'
            }
            if ($script:deadlineConsumer -eq $name -and
                $script:deadlineConsumerAfterMilliseconds -gt 0 -and
                -not $script:deadlineConsumerTriggered -and
                $null -ne $script:cleanupScenarioClock -and
                $script:cleanupScenarioClock.ElapsedMilliseconds -ge $script:deadlineConsumerAfterMilliseconds) {
                $script:deadlineConsumerTriggered = $true
                Start-Sleep -Milliseconds $script:deadlineConsumerSleepMilliseconds
                throw 'simulated probe consumed the final cleanup sweep'
            }
            if ($script:lateContainerName -eq $name -and $count -eq $script:lateContainerObservation) {
                $script:presentContainers[$name] = $true
            }
            if ($script:presentContainers[$name]) { return $script:containerIds[$name] }
            return ''
        }
        'container inspect' {
            $identity = [string]$Arguments[-1]
            $name = $script:containerNamesById[$identity]
            if ([string]::IsNullOrWhiteSpace($name) -or -not $script:presentContainers[$name]) {
                throw 'simulated immutable container disappeared before inspect'
            }
            $owner = if ($script:unverified -eq $name) { 'different-owner' } else { $script:testRunId }
            return "$identity|$owner"
        }
        'container rm' {
            $identity = [string]$Arguments[-1]
            $name = $script:containerNamesById[$identity]
            if ($script:failedRemove -eq $name) { throw 'simulated container removal failure' }
            $script:presentContainers[$name] = $false
            return ''
        }
        'image ls' {
            if ($script:timedOutInspect -eq $script:testImage) { throw 'simulated bounded image-list timeout' }
            if ($script:failedInspect -eq $script:testImage) { throw 'simulated image-list failure' }
            $script:imageObservationCount++
            if ($script:lateImage -and $script:imageObservationCount -eq $script:lateImageObservation) {
                $script:imagePresent = $true
            }
            if ($script:imagePresent) { return $script:imageId }
            return ''
        }
        'image inspect' {
            if (-not $script:imagePresent -or $Arguments[-1] -ne $script:imageId) {
                throw 'simulated immutable image disappeared before inspect'
            }
            $owner = if ($script:unverified -eq $script:testImage) { 'different-owner' } else { $script:testRunId }
            return "$($script:imageId)|$owner"
        }
        'image rm' {
            if ($script:failedImage) { throw 'simulated image removal failure' }
            if ($Arguments[-1] -ne $script:imageId) { throw 'cleanup did not use the immutable image ID' }
            $script:imagePresent = $false
            return ''
        }
        default { throw "Unexpected Docker command in cleanup test: $operation" }
    }
}
function Test-Path { param([string]$LiteralPath) return $true }
function Remove-Item {
    param([string]$LiteralPath, [switch]$Recurse, [switch]$Force)
    $script:removedPaths.Add($LiteralPath)
    if ($script:failedDirectory) { throw 'simulated identity directory deletion failure' }
}

function New-CleanupCase {
    $containerA = "shipforge-m1-$script:testRunId-a"
    $containerB = "shipforge-m1-$script:testRunId-b"
    $script:testImage = "shipforge-m1-fixture:$script:testRunId"
    $script:imageId = 'sha256:' + ('c' * 64)
    $script:containerIds = @{}
    $script:containerIds[$containerA] = 'a' * 64
    $script:containerIds[$containerB] = 'b' * 64
    $script:containerNamesById = @{}
    $script:containerNamesById[$script:containerIds[$containerA]] = $containerA
    $script:containerNamesById[$script:containerIds[$containerB]] = $containerB
    $script:presentContainers = @{}
    $script:presentContainers[$containerA] = $true
    $script:presentContainers[$containerB] = $true
    $script:containerObservationCounts = @{}
    $script:imagePresent = $true
    $script:imageObservationCount = 0
    $script:lateContainerName = ''
    $script:lateContainerObservation = 0
    $script:lateContainerAfterMilliseconds = 0
    $script:lateImage = $false
    $script:lateImageObservation = 0
    $script:deadlineConsumer = ''
    $script:deadlineConsumerObservation = 0
    $script:deadlineConsumerAfterMilliseconds = 0
    $script:deadlineConsumerSleepMilliseconds = 0
    $script:deadlineConsumerTriggered = $false
    $script:containerObservationDelayMilliseconds = 0
    $script:cleanupScenarioClock = $null
    $script:dockerCalls = [System.Collections.Generic.List[string]]::new()
    $script:dockerTimeouts = [System.Collections.Generic.List[int]]::new()
    $script:removedPaths = [System.Collections.Generic.List[string]]::new()
    $script:failedRemove = ''
    $script:failedInspect = ''
    $script:timedOutInspect = ''
    $script:unverified = ''
    $script:failedImage = $false
    $script:failedDirectory = $false
    $script:failedInput = $false
    $script:forceKill = $false
    $script:sshNativeExit = 0
    $script:sshNativeCalls.Clear()
    $script:sshTimeouts.Clear()
    $script:lifecycle = [pscustomobject]@{ Closed = $false; Waits = 0; Killed = $false; Tree = $false; Disposed = $false }
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
    $process | Add-Member -MemberType ScriptMethod -Name Kill -Value {
        param($EntireTree)
        $script:lifecycle.Killed = $true
        $script:lifecycle.Tree = $EntireTree
    }
    $process | Add-Member -MemberType ScriptMethod -Name Dispose -Value { $script:lifecycle.Disposed = $true }
    return @{
        Containers = @($containerA, $containerB)
        RunId = $script:testRunId
        Image = $script:testImage
        ImageBuilt = $true
        Temporary = Join-Path ([System.IO.Path]::GetTempPath()) "shipforge-m1-$script:testRunId"
        TemporaryParent = [System.IO.Path]::GetTempPath()
        SavedEnvironment = @{ SHIPFORGE_CLEANUP_TEST_SENTINEL = $script:originalSentinel }
        AgentIdentity = $null
        AgentIdentityAdded = $false
        AgentRecovery = $null
        AgentRecoveryCreated = $false
        KeepAlive = $process
        KeepAliveStarted = $true
        CleanupBudgetMilliseconds = 800
        CleanupObservationMilliseconds = 500
        CleanupSettleMilliseconds = 10
        CleanupFreshnessMilliseconds = 250
        CleanupPollMilliseconds = 1
        RequiredAbsentObservations = 3
        CleanupTerminationReserveMilliseconds = 5
    }
}

$script:testRunId = [guid]::NewGuid().ToString('N')
$script:originalSentinel = [Environment]::GetEnvironmentVariable('SHIPFORGE_CLEANUP_TEST_SENTINEL', 'Process')
try {
    $case = New-CleanupCase
    $env:SHIPFORGE_CLEANUP_TEST_SENTINEL = 'changed-by-test'
    $containerAId = $script:containerIds[$case.Containers[0]]
    $containerBId = $script:containerIds[$case.Containers[1]]
    $imageId = $script:imageId
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'Successful cleanup reported a failure'
    Assert-Cleanup ($script:dockerCalls.Contains("container rm --force $containerAId") -and $script:dockerCalls.Contains("container rm --force $containerBId")) 'Owned containers must be removed by immutable ID'
    Assert-Cleanup ($script:dockerCalls.Contains("image rm $imageId")) 'Owned image must be removed by immutable ID'
    Assert-Cleanup ($script:containerObservationCounts[$case.Containers[0]] -ge 3 -and $script:imageObservationCount -ge 3) 'Cleanup success requires consecutive absence observations across an explicit settle window'
    Assert-Cleanup (@($script:dockerTimeouts | Where-Object { $_ -le 0 -or $_ -gt $case.CleanupBudgetMilliseconds }).Count -eq 0) 'Every Docker command must remain inside the one shared absolute cleanup deadline'
    Assert-Cleanup ($script:removedPaths.Count -eq 1) 'Exact temporary directory must be deleted'
    Assert-Cleanup ($script:lifecycle.Closed -and $script:lifecycle.Disposed) 'WSL helper must be closed and disposed'
    Assert-Cleanup ([Environment]::GetEnvironmentVariable('SHIPFORGE_CLEANUP_TEST_SENTINEL', 'Process') -eq $script:originalSentinel) 'Process environment must be restored'

    # Real WSL Docker probes can make a complete sweep slower than the freshness
    # threshold at the old absolute deadline. The post-horizon phase must reset
    # the earlier evidence and obtain complete fresh sweeps without shortening
    # the observation horizon.
    $case = New-CleanupCase
    $case.ImageBuilt = $false
    $script:presentContainers[$case.Containers[0]] = $false
    $script:presentContainers[$case.Containers[1]] = $false
    $script:containerObservationDelayMilliseconds = 25
    $case.CleanupBudgetMilliseconds = 600
    $case.CleanupObservationMilliseconds = 250
    $case.CleanupSettleMilliseconds = 1
    $case.CleanupFreshnessMilliseconds = 80
    $case.RequiredAbsentObservations = 2
    $slowCleanupClock = [System.Diagnostics.Stopwatch]::StartNew()
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'Slow probes must obtain new complete evidence after the absolute observation horizon'
    Assert-Cleanup ($slowCleanupClock.ElapsedMilliseconds -ge $case.CleanupObservationMilliseconds) 'Slow cleanup must never report success before its observation horizon'
    Assert-Cleanup ($script:containerObservationCounts[$case.Containers[0]] -ge 2 -and $script:containerObservationCounts[$case.Containers[1]] -ge 2) 'Slow cleanup success still requires a complete multi-resource sweep'

    # A daemon request can publish after the old predictive guard would have
    # stopped but before the absolute observation horizon. Keep watching, remove
    # that late owned object, then rebuild all confirmation evidence from zero.
    $case = New-CleanupCase
    $case.ImageBuilt = $false
    $lateContainer = $case.Containers[0]
    $lateContainerId = $script:containerIds[$lateContainer]
    $script:presentContainers[$lateContainer] = $false
    $script:presentContainers[$case.Containers[1]] = $false
    $script:lateContainerName = $lateContainer
    $script:lateContainerAfterMilliseconds = 330
    $script:containerObservationDelayMilliseconds = 50
    $case.CleanupBudgetMilliseconds = 850
    $case.CleanupObservationMilliseconds = 400
    $case.CleanupSettleMilliseconds = 1
    $case.CleanupFreshnessMilliseconds = 150
    $case.RequiredAbsentObservations = 2
    $script:cleanupScenarioClock = [System.Diagnostics.Stopwatch]::StartNew()
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'An owned object published before the absolute observation horizon must be reconciled'
    Assert-Cleanup ($script:cleanupScenarioClock.ElapsedMilliseconds -ge $case.CleanupObservationMilliseconds) 'Delayed-publication cleanup must remain active through the observation horizon'
    Assert-Cleanup ($script:dockerCalls.Contains("container rm --force $lateContainerId")) 'The delayed owned object must be removed by immutable ID before confirmation succeeds'

    # A timed-out create may become visible after cleanup first reports absence.
    $case = New-CleanupCase
    $lateContainer = $case.Containers[0]
    $lateContainerId = $script:containerIds[$lateContainer]
    $script:presentContainers[$lateContainer] = $false
    $script:lateContainerName = $lateContainer
    $case.CleanupBudgetMilliseconds = 800
    $case.CleanupSettleMilliseconds = 1
    # Appear one observation after the old three-observation completion point.
    # Cleanup must keep observing until its shared deadline.
    $script:lateContainerObservation = 4
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'A late owned container must be reconciled instead of being reported cleaned early'
    Assert-Cleanup ($script:containerObservationCounts[$lateContainer] -gt $script:lateContainerObservation) 'A stable early absence must not complete container cleanup before the shared deadline'
    Assert-Cleanup ($script:dockerCalls.Contains("container rm --force $lateContainerId")) 'A late owned container must be removed using its immutable ID'

    # A timed-out build can likewise publish its tagged image after one empty list.
    $case = New-CleanupCase
    $lateImageId = $script:imageId
    $script:imagePresent = $false
    $script:lateImage = $true
    $case.CleanupBudgetMilliseconds = 800
    $case.CleanupSettleMilliseconds = 1
    $script:lateImageObservation = 4
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 0) 'A late owned image must be reconciled instead of being reported cleaned early'
    Assert-Cleanup ($script:imageObservationCount -gt $script:lateImageObservation -and $script:dockerCalls.Contains("image rm $lateImageId")) 'A late owned image must be removed by immutable ID and then settle absent through the shared deadline'

    # A slow later resource in the post-horizon phase must invalidate every
    # resource from the same incomplete final sweep.
    $case = New-CleanupCase
    $case.ImageBuilt = $false
    $script:presentContainers[$case.Containers[0]] = $false
    $script:presentContainers[$case.Containers[1]] = $false
    $script:deadlineConsumer = $case.Containers[1]
    $script:deadlineConsumerAfterMilliseconds = 220
    $script:deadlineConsumerSleepMilliseconds = 600
    $script:containerObservationDelayMilliseconds = 25
    $case.CleanupBudgetMilliseconds = 800
    $case.CleanupObservationMilliseconds = 200
    $case.CleanupSettleMilliseconds = 1
    $case.CleanupFreshnessMilliseconds = 80
    $case.RequiredAbsentObservations = 2
    $script:cleanupScenarioClock = [System.Diagnostics.Stopwatch]::StartNew()
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup (@($failures | Where-Object { $_ -like "Container $($case.Containers[0]):*" }).Count -eq 1) 'An earlier absence in an incomplete final sweep must fail closed'
    Assert-Cleanup (@($failures | Where-Object { $_ -like "Container $($case.Containers[1]):*" }).Count -eq 1) 'The deadline-consuming resource must fail closed'

    $case = New-CleanupCase
    $case.AgentIdentity = Join-Path $case.Temporary 'agent_ed25519.pub'
    $case.AgentIdentityAdded = $true
    $case.AgentRecovery = Join-Path $case.TemporaryParent "shipforge-m1-agent-recovery-$script:testRunId.pub"
    $case.AgentRecoveryCreated = $true
    $failures = @(Complete-FixtureRun @case)
    $expectedIdentity = [System.IO.Path]::GetFullPath($case.AgentIdentity)
    Assert-Cleanup ($failures.Count -eq 0) 'Exact disposable Agent identity cleanup reported a failure'
    Assert-Cleanup ($script:sshNativeCalls.Count -eq 1 -and $script:sshNativeCalls[0] -eq "-d $expectedIdentity") 'Cleanup must remove only the exact disposable Agent identity'
    Assert-Cleanup ($script:sshTimeouts.Count -eq 1 -and $script:sshTimeouts[0] -eq 10000) 'Agent identity cleanup must have its own deadline'
    Assert-Cleanup ($script:sshNativeCalls[0] -cnotmatch '(^|\s)-D(\s|$)') 'Cleanup must not clear the whole Agent'

    $case = New-CleanupCase
    $case.AgentIdentity = Join-Path $case.TemporaryParent 'unowned-id_ed25519'
    $case.AgentIdentityAdded = $true
    $case.AgentRecovery = Join-Path $case.TemporaryParent "shipforge-m1-agent-recovery-$script:testRunId.pub"
    $case.AgentRecoveryCreated = $true
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 1) 'Out-of-scope Agent identity must be reported'
    Assert-Cleanup ($script:sshNativeCalls.Count -eq 0) 'Out-of-scope Agent identity must never reach ssh-add'
    Assert-Cleanup ($script:removedPaths.Count -eq 1 -and $script:removedPaths[0] -eq [System.IO.Path]::GetFullPath($case.Temporary)) 'Agent path refusal must retain only the exact public-key recovery material'

    $case = New-CleanupCase
    $case.AgentIdentity = Join-Path $case.Temporary 'agent_ed25519.pub'
    $case.AgentIdentityAdded = $true
    $case.AgentRecovery = Join-Path $case.TemporaryParent "shipforge-m1-agent-recovery-$script:testRunId.pub"
    $case.AgentRecoveryCreated = $true
    $script:sshNativeExit = 9
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 1) 'Agent identity removal failure must be reported'
    Assert-Cleanup ($script:removedPaths.Count -eq 1 -and $script:removedPaths[0] -eq [System.IO.Path]::GetFullPath($case.Temporary) -and $script:lifecycle.Disposed) 'Agent removal failure must retain only public recovery material while still deleting private keys and releasing the helper'

    $case = New-CleanupCase
    $script:failedRemove = $case.Containers[0]
    $script:failedImage = $true
    $script:failedDirectory = $true
    $script:failedInput = $true
    $script:forceKill = $true
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 5) "Independent cleanup failures and confirmation evidence invalidated by unknown Docker results must be aggregated: $($failures -join '; ')"
    $failedContainerText = @($failures | Where-Object { $_ -like "Container $($case.Containers[0]):*" })[0]
    $invalidatedContainerText = @($failures | Where-Object { $_ -like "Container $($case.Containers[1]):*" })[0]
    $failedImageText = @($failures | Where-Object { $_ -like "Image $($case.Image):*" })[0]
    Assert-Cleanup ($failedContainerText -like '*Docker observation or removal did not complete*') 'The failing container must retain its own Docker failure attribution'
    Assert-Cleanup ($failedImageText -like '*Docker observation or removal did not complete*') 'The failing image must retain its own Docker failure attribution'
    Assert-Cleanup (
        $invalidatedContainerText -notlike '*Docker observation or removal did not complete*' -and
        ($invalidatedContainerText -like '*invalidated*' -or
            $invalidatedContainerText -like '*absence has not completed*' -or
            $invalidatedContainerText -like '*final post-observation cleanup sweep did not complete*')
    ) "A separately removed container must report only its invalidated or incomplete confirmation evidence: $invalidatedContainerText"
    Assert-Cleanup ($script:dockerCalls.Contains("container rm --force $($script:containerIds[$case.Containers[1]])")) 'Later container cleanup must continue after a removal failure'
    Assert-Cleanup ($script:removedPaths.Count -eq 1) 'Identity cleanup must still be attempted after Docker errors'
    Assert-Cleanup ($script:lifecycle.Killed -and $script:lifecycle.Tree -and $script:lifecycle.Waits -eq 2 -and $script:lifecycle.Disposed) 'Pipe or directory failure must not skip bounded owned-tree helper termination/disposal'

    $case = New-CleanupCase
    $script:timedOutInspect = $case.Containers[0]
    $cleanupTimer = [System.Diagnostics.Stopwatch]::StartNew()
    $failures = @(Complete-FixtureRun @case)
    $targetFailure = @($failures | Where-Object { $_ -like "Container $($case.Containers[0]):*" })
    Assert-Cleanup ($targetFailure.Count -eq 1) 'A bounded cleanup timeout for the target must be aggregated'
    Assert-Cleanup ($script:dockerCalls.Contains("container rm --force $($script:containerIds[$case.Containers[1]])")) 'A timed-out cleanup command must not skip the next container'
    Assert-Cleanup ($script:dockerCalls.Contains("image rm $($script:imageId)")) 'A timed-out container check must not skip owned-image cleanup'
    Assert-Cleanup ($script:removedPaths.Count -eq 1 -and $script:lifecycle.Disposed) 'A cleanup timeout must not skip environment, temp, or helper restoration'
    Assert-Cleanup (@($script:dockerTimeouts | Where-Object { $_ -le 0 -or $_ -gt $case.CleanupBudgetMilliseconds }).Count -eq 0) 'Timeout continuation must consume only the shared cleanup deadline'
    Assert-Cleanup ($cleanupTimer.ElapsedMilliseconds -lt 1500) 'Repeated cleanup reconciliation must stop at its absolute deadline'

    $case = New-CleanupCase
    $script:unverified = $case.Containers[0]
    $foreignId = $script:containerIds[$case.Containers[0]]
    $script:presentContainers[$case.Containers[0]] = $false
    $script:lateContainerName = $case.Containers[0]
    $case.CleanupBudgetMilliseconds = 800
    $case.CleanupSettleMilliseconds = 1
    $script:lateContainerObservation = 4
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 1) 'Wrong ownership appearing after an initial absence must be reported'
    Assert-Cleanup ($script:containerObservationCounts[$case.Containers[0]] -ge 2 -and -not $script:dockerCalls.Contains("container rm --force $foreignId")) 'A late wrong-owner container must fail closed and never be deleted'
    Assert-Cleanup ($script:dockerCalls.Contains("container rm --force $($script:containerIds[$case.Containers[1]])")) 'Verified container must still be cleaned up'

    $case = New-CleanupCase
    $script:failedInspect = $case.Containers[0]
    $failedIdentity = $script:containerIds[$case.Containers[0]]
    $failures = @(Complete-FixtureRun @case)
    Assert-Cleanup ($failures.Count -eq 3) 'A persistently failed ownership observation must invalidate the final shared confirmation sweep'
    Assert-Cleanup (@($failures | Where-Object { $_ -like 'Container *' -or $_ -like 'Image *' }).Count -eq 3) 'Every resource in a final sweep with an unknown observation must fail closed'
    Assert-Cleanup (@($failures | Where-Object { $_ -like "Container $($case.Containers[0]):*Docker observation or removal did not complete*" }).Count -eq 1) 'The resource with the failed observation must retain its own failure attribution'
    Assert-Cleanup (-not $script:dockerCalls.Contains("container rm --force $failedIdentity")) 'Failed ownership observation must prevent deletion'
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

$script:diagnosticBoundedCall = $null
function Invoke-BoundedNativeCommand {
    param([string]$FilePath, [string[]]$Arguments, [int]$TimeoutMilliseconds, [int]$MaxOutputBytes)
    $script:diagnosticBoundedCall = [pscustomobject]@{
        FilePath = $FilePath; Arguments = $Arguments; Timeout = $TimeoutMilliseconds; Limit = $MaxOutputBytes
    }
    return [pscustomobject]@{
        ExitCode = 19; TimedOut = $false; Truncated = $false; Stdout = 'diagnostic'; Stderr = ''
    }
}
$result = Invoke-FixtureDiagnosticCommand -Distribution 'fixture only' -Arguments @('logs', '--tail', '80', 'fixed-id') -TimeoutMilliseconds 1000 -MaxOutputBytes 1024
Assert-Cleanup ($result.ExitCode -eq 19) 'Diagnostic wrapper must preserve the shared bounded runner result'
Assert-Cleanup ($script:diagnosticBoundedCall.FilePath -eq 'wsl.exe') 'Diagnostics must use the expected native helper'
Assert-Cleanup (($script:diagnosticBoundedCall.Arguments -join '|') -eq '-d|fixture only|--exec|docker|-H|unix:///var/run/docker.sock|logs|--tail|80|fixed-id') 'Diagnostic arguments must remain separate from shell text and pin the WSL Docker socket'
Assert-Cleanup ($script:diagnosticBoundedCall.Timeout -eq 1000 -and $script:diagnosticBoundedCall.Limit -eq 1024) 'Diagnostics must preserve bounded time and retained output limits'

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
