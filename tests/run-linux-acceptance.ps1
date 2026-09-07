#requires -Version 7.0
param(
    [string]$Distribution = 'Ubuntu-22.04',
    [ValidateSet('All', 'Deployment', 'Retention', 'AutomaticRetention', 'Management', 'ConnectionStability', 'ReleaseGate', 'ServiceCommands')]
    [string]$Suite = 'All',
    [ValidateSet('https://registry.npmjs.org', 'https://registry.npmmirror.com')]
    [string]$NpmRegistry = 'https://registry.npmjs.org'
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'run-linux-acceptance-diagnostics.ps1')
$runId = [guid]::NewGuid().ToString('N')
$image = "shipforge-m1-fixture:$runId"
$imageBuilt = $false
$keepAlive = $null
$keepAliveStarted = $false
$operationError = $null
$containers = @()
$agentIdentity = $null
$agentIdentityAdded = $false
$agentRecovery = $null
$agentRecoveryCreated = $false
$runReleaseGate = $Suite -eq 'ReleaseGate'
$temporaryParent = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$temporary = Join-Path $temporaryParent "shipforge-m1-$runId"
$repository = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$boundedNativeHelper = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'support/bounded-native-command.ps1'))
$environmentNames = @(
    'SHIPFORGE_LINUX_ACCEPTANCE',
    'SHIPFORGE_TEST_KEY',
    'SHIPFORGE_TEST_PORT_A',
    'SHIPFORGE_TEST_PORT_B',
    'SHIPFORGE_TEST_HOST_KEY_A',
    'SHIPFORGE_TEST_HOST_KEY_B',
    'SHIPFORGE_QA01_OPENSSH',
    'SHIPFORGE_QA01_SSH_HOST',
    'SHIPFORGE_QA01_SSH_PORT',
    'SHIPFORGE_QA01_SSH_USER',
    'SHIPFORGE_QA01_SSH_CURRENT_HOST_KEY',
    'SHIPFORGE_QA01_SSH_PREVIOUS_HOST_KEY',
    'SHIPFORGE_QA01_SSH_IDENTITY_FILE',
    'SHIPFORGE_QA01_SSH_AGENT_FINGERPRINT'
)
$savedEnvironment = @{}
foreach ($name in $environmentNames) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

function New-AcceptanceNativeProcess {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [switch]$RedirectStandardInput,
        [switch]$NoOutputRedirect
    )
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo.FileName = $FilePath
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardInput = $RedirectStandardInput
    $process.StartInfo.RedirectStandardOutput = -not $NoOutputRedirect
    $process.StartInfo.RedirectStandardError = -not $NoOutputRedirect
    foreach ($argument in $Arguments) { $process.StartInfo.ArgumentList.Add($argument) }
    return $process
}

function Initialize-AcceptanceJobInterop {
    if ($null -ne ('ShipForgeQa01.JobInterop' -as [type])) { return }
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;

namespace ShipForgeQa01 {
    public static class JobInterop {
        private const int JobObjectBasicAccountingInformation = 1;
        private const int JobObjectExtendedLimitInformation = 9;
        private const uint JobObjectLimitKillOnJobClose = 0x00002000;

        [StructLayout(LayoutKind.Sequential)]
        private struct BasicLimitInformation {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize;
            public UIntPtr MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct IoCounters {
            public ulong ReadOperationCount;
            public ulong WriteOperationCount;
            public ulong OtherOperationCount;
            public ulong ReadTransferCount;
            public ulong WriteTransferCount;
            public ulong OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct ExtendedLimitInformation {
            public BasicLimitInformation BasicLimitInformation;
            public IoCounters IoInfo;
            public UIntPtr ProcessMemoryLimit;
            public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed;
            public UIntPtr PeakJobMemoryUsed;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct BasicAccountingInformation {
            public long TotalUserTime;
            public long TotalKernelTime;
            public long ThisPeriodTotalUserTime;
            public long ThisPeriodTotalKernelTime;
            public uint TotalPageFaultCount;
            public uint TotalProcesses;
            public uint ActiveProcesses;
            public uint TotalTerminatedProcesses;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(
            IntPtr job,
            int informationClass,
            ref ExtendedLimitInformation information,
            uint informationLength
        );

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool QueryInformationJobObject(
            IntPtr job,
            int informationClass,
            out BasicAccountingInformation information,
            uint informationLength,
            IntPtr returnLength
        );

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool TerminateJobObject(IntPtr job, uint exitCode);

        [DllImport("kernel32.dll")]
        private static extern bool CloseHandle(IntPtr handle);

        private static Win32Exception NativeError() {
            return new Win32Exception(Marshal.GetLastWin32Error());
        }

        public static IntPtr CreateKillOnClose() {
            IntPtr job = CreateJobObjectW(IntPtr.Zero, null);
            if (job == IntPtr.Zero) { throw NativeError(); }
            var information = new ExtendedLimitInformation();
            information.BasicLimitInformation.LimitFlags = JobObjectLimitKillOnJobClose;
            if (!SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                ref information,
                (uint)Marshal.SizeOf<ExtendedLimitInformation>())) {
                var error = NativeError();
                CloseHandle(job);
                throw error;
            }
            return job;
        }

        public static void Assign(IntPtr job, Process process) {
            if (!AssignProcessToJobObject(job, process.Handle)) { throw NativeError(); }
        }

        public static uint ActiveProcessCount(IntPtr job) {
            BasicAccountingInformation information;
            if (!QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                out information,
                (uint)Marshal.SizeOf<BasicAccountingInformation>(),
                IntPtr.Zero)) {
                throw NativeError();
            }
            return information.ActiveProcesses;
        }

        public static void Terminate(IntPtr job) {
            if (!TerminateJobObject(job, 124)) { throw NativeError(); }
        }

        public static void Close(IntPtr job) {
            if (job != IntPtr.Zero) { CloseHandle(job); }
        }
    }
}
'@
}

function New-BoundedNativeProcess {
    param([string]$FilePath, [string[]]$Arguments)
    Initialize-AcceptanceJobInterop
    $helper = $boundedNativeHelper
    if (-not [System.IO.File]::Exists($helper)) { throw 'Bounded native helper is unavailable' }
    $options = [System.Text.Json.JsonSerializerOptions]::new()
    $json = [System.Text.Json.JsonSerializer]::Serialize[object]([string[]]$Arguments, $options)
    $encodedArguments = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($json))
    $eventName = 'Local\ShipForgeQa01-' + [guid]::NewGuid().ToString('N')
    $created = $false
    $startEvent = [System.Threading.EventWaitHandle]::new(
        $false,
        [System.Threading.EventResetMode]::ManualReset,
        $eventName,
        [ref]$created
    )
    if (-not $created) {
        $startEvent.Dispose()
        throw 'Bounded native start gate was not created exclusively'
    }
    $job = [IntPtr]::Zero
    $process = $null
    $started = $false
    try {
        $job = [ShipForgeQa01.JobInterop]::CreateKillOnClose()
        $process = New-AcceptanceNativeProcess -FilePath (Join-Path $PSHOME 'pwsh.exe') -Arguments @(
            '-NoLogo', '-NoProfile', '-NonInteractive', '-File', $helper,
            '-StartEvent', $eventName,
            '-FilePath', $FilePath,
            '-ArgumentsBase64', $encodedArguments
        )
        if (-not $process.Start()) { throw 'Bounded native bootstrap did not start' }
        $started = $true
        # The bootstrap waits on startEvent and cannot create the target until
        # it belongs to this kill-on-close Job Object.
        [ShipForgeQa01.JobInterop]::Assign($job, $process)
        if (-not $startEvent.Set()) { throw 'Bounded native start gate could not be released' }
        return [pscustomobject]@{
            Process = $process
            Job = $job
            StartEvent = $startEvent
        }
    } catch {
        if ($job -ne [IntPtr]::Zero) {
            try { [ShipForgeQa01.JobInterop]::Terminate($job) } catch { }
        }
        if ($started -and $null -ne $process) {
            try {
                if (-not $process.HasExited) {
                    $process.Kill($true)
                    [void]$process.WaitForExit(1000)
                }
            } catch { }
        }
        if ($null -ne $process) { $process.Dispose() }
        $startEvent.Dispose()
        [ShipForgeQa01.JobInterop]::Close($job)
        throw 'Bounded native process could not be started inside its owned Job Object'
    }
}

function Invoke-BoundedNativeCommand {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [ValidateRange(1, 900000)][int]$TimeoutMilliseconds,
        [ValidateRange(1, 16777216)][int]$MaxOutputBytes = 1048576
    )
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    $context = $null
    $process = $null
    $timedOut = $false
    $exitCode = $null
    $truncated = $false
    $completed = $false
    $rootExitedWithActiveDescendants = $false
    $remainingBytes = $MaxOutputBytes
    $captures = @([System.IO.MemoryStream]::new(), [System.IO.MemoryStream]::new())
    $streams = @()
    try {
        $context = New-BoundedNativeProcess -FilePath $FilePath -Arguments $Arguments
        $process = $context.Process
        $streams = @($process.StandardOutput.BaseStream, $process.StandardError.BaseStream)
        $buffers = @([byte[]]::new(8192), [byte[]]::new(8192))
        $reads = @($streams[0].ReadAsync($buffers[0], 0, 8192), $streams[1].ReadAsync($buffers[1], 0, 8192))
        $ended = @($false, $false)
        while ($true) {
            # The timeout is fail-closed. Never accept a completion first
            # noticed at or beyond the caller's absolute deadline.
            if ($timer.ElapsedMilliseconds -ge $TimeoutMilliseconds) {
                $timedOut = $true
                break
            }
            $activeProcesses = [ShipForgeQa01.JobInterop]::ActiveProcessCount($context.Job)
            if ($process.HasExited -and $activeProcesses -gt 0) {
                $rootExitedWithActiveDescendants = $true
            }
            if ($ended[0] -and $ended[1] -and $process.HasExited -and $activeProcesses -eq 0) {
                if ($timer.ElapsedMilliseconds -ge $TimeoutMilliseconds) {
                    $timedOut = $true
                    break
                }
                $completed = $true
                break
            }
            for ($index = 0; $index -lt 2; $index++) {
                if ($ended[$index] -or -not $reads[$index].IsCompleted) { continue }
                $count = $reads[$index].GetAwaiter().GetResult()
                if ($count -eq 0) {
                    $ended[$index] = $true
                    continue
                }
                $take = [Math]::Min($remainingBytes, $count)
                if ($take -gt 0) { $captures[$index].Write($buffers[$index], 0, $take) }
                $remainingBytes -= $take
                if ($take -ne $count) { $truncated = $true }
                # Drain both pipes even after the retained-output budget is exhausted.
                $reads[$index] = $streams[$index].ReadAsync($buffers[$index], 0, 8192)
            }
            [System.Threading.Thread]::Sleep(5)
        }
        if ($completed) { $exitCode = $process.ExitCode }
        return [pscustomobject]@{
            ExitCode = $exitCode
            TimedOut = $timedOut
            Truncated = $truncated
            RootExitedWithActiveDescendants = $rootExitedWithActiveDescendants
            Stdout = [System.Text.Encoding]::UTF8.GetString($captures[0].ToArray())
            Stderr = [System.Text.Encoding]::UTF8.GetString($captures[1].ToArray())
        }
    } finally {
        $cleanupFailed = $false
        if ($null -ne $context) {
            try {
                if (-not $completed) {
                    [ShipForgeQa01.JobInterop]::Terminate($context.Job)
                    $reapTimer = [System.Diagnostics.Stopwatch]::StartNew()
                    while ([ShipForgeQa01.JobInterop]::ActiveProcessCount($context.Job) -ne 0 -and
                        $reapTimer.ElapsedMilliseconds -lt 1000) {
                        [System.Threading.Thread]::Sleep(5)
                    }
                    if ([ShipForgeQa01.JobInterop]::ActiveProcessCount($context.Job) -ne 0) {
                        $cleanupFailed = $true
                    }
                }
            } catch { $cleanupFailed = $true }
        }
        foreach ($capture in $captures) { $capture.Dispose() }
        foreach ($stream in $streams) { try { $stream.Dispose() } catch { } }
        if ($null -ne $context) {
            try { $process.Dispose() } catch { $cleanupFailed = $true }
            try { $context.StartEvent.Dispose() } catch { $cleanupFailed = $true }
            [ShipForgeQa01.JobInterop]::Close($context.Job)
        }
        if ($cleanupFailed) {
            throw 'Bounded native command process tree could not be terminated and reaped'
        }
    }
}

function Invoke-CheckedNativeCommand {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [ValidateRange(1, 900000)][int]$TimeoutMilliseconds,
        [ValidateRange(1, 16777216)][int]$MaxOutputBytes = 1048576,
        [int[]]$AllowedExitCodes = @(0),
        [string]$Operation = 'Native command',
        [switch]$PublishOutput
    )
    $result = Invoke-BoundedNativeCommand -FilePath $FilePath -Arguments $Arguments -TimeoutMilliseconds $TimeoutMilliseconds -MaxOutputBytes $MaxOutputBytes
    if ($PublishOutput) {
        if (-not [string]::IsNullOrEmpty($result.Stdout)) { Write-Host -NoNewline $result.Stdout }
        if (-not [string]::IsNullOrEmpty($result.Stderr)) { Write-Host -NoNewline $result.Stderr }
        if ($result.Truncated) { Write-Warning "$Operation output was truncated at its retained-output limit" }
    }
    if ($result.TimedOut) { throw "$Operation timed out after $TimeoutMilliseconds ms" }
    if ($result.ExitCode -notin $AllowedExitCodes) { throw "$Operation failed with exit code $($result.ExitCode)" }
    return $result
}

function Invoke-FixtureDocker {
    param(
        [string[]]$Arguments,
        [ValidateRange(1, 300000)][int]$TimeoutMilliseconds = 30000,
        [switch]$PublishOutput
    )
    $nativeArguments = @('-d', $Distribution, '--exec', 'docker', '-H', 'unix:///var/run/docker.sock') + $Arguments
    $result = Invoke-CheckedNativeCommand -FilePath 'wsl.exe' -Arguments $nativeArguments -TimeoutMilliseconds $TimeoutMilliseconds -MaxOutputBytes 4194304 -Operation "Fixture Docker $($Arguments[0])" -PublishOutput:$PublishOutput
    return $result.Stdout
}

function Invoke-WslCommand {
    param(
        [string[]]$Arguments,
        [ValidateRange(1, 30000)][int]$TimeoutMilliseconds = 15000,
        [string]$Operation = 'WSL setup command'
    )
    $nativeArguments = @('-d', $Distribution, '--exec') + $Arguments
    return Invoke-CheckedNativeCommand -FilePath 'wsl.exe' -Arguments $nativeArguments -TimeoutMilliseconds $TimeoutMilliseconds -Operation $Operation
}

function Invoke-SshKeygen {
    param(
        [string[]]$Arguments,
        [ValidateRange(1, 30000)][int]$TimeoutMilliseconds = 15000,
        [string]$Operation = 'ssh-keygen'
    )
    return Invoke-CheckedNativeCommand -FilePath 'ssh-keygen.exe' -Arguments $Arguments -TimeoutMilliseconds $TimeoutMilliseconds -Operation $Operation
}

function Invoke-SshAdd {
    param(
        [string[]]$Arguments,
        [int[]]$AllowedExitCodes = @(0),
        [ValidateRange(1, 30000)][int]$TimeoutMilliseconds = 10000
    )
    [void](Invoke-CheckedNativeCommand -FilePath 'ssh-add.exe' -Arguments $Arguments -TimeoutMilliseconds $TimeoutMilliseconds -AllowedExitCodes $AllowedExitCodes -Operation "ssh-add $($Arguments[0])")
}

function Invoke-CargoCommand {
    param(
        [string[]]$Arguments,
        [ValidateRange(1, 900000)][int]$TimeoutMilliseconds,
        [switch]$PublishOutput
    )
    return Invoke-CheckedNativeCommand -FilePath 'cargo.exe' -Arguments $Arguments -TimeoutMilliseconds $TimeoutMilliseconds -MaxOutputBytes 8388608 -Operation 'Cargo acceptance command' -PublishOutput:$PublishOutput
}

function Invoke-FixtureCase {
    param([string]$Case, [ValidateRange(1, 660000)][int]$ExecutionTimeoutMilliseconds = 300000)
    # Cargo exits successfully even when a misspelled filter selects zero tests.
    $arguments = @('test', '--locked', '--test', 'linux_ssh_deployment', $Case, '--', '--ignored', '--exact', '--list')
    $listed = Invoke-CargoCommand -Arguments $arguments -TimeoutMilliseconds 600000
    if (@($listed.Stdout -split '\r?\n' | Where-Object { $_ -eq "${Case}: test" }).Count -ne 1) {
        throw "Expected exactly one available disposable Linux test: $Case"
    }
    $arguments = @('test', '--locked', '--test', 'linux_ssh_deployment', $Case, '--', '--ignored', '--exact', '--nocapture')
    [void](Invoke-CargoCommand -Arguments $arguments -TimeoutMilliseconds $ExecutionTimeoutMilliseconds -PublishOutput)
}

function Assert-ReleaseGateNativeBuild {
    foreach ($name in @('CARGO_BUILD_TARGET', 'CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR')) {
        if (-not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable($name, 'Process'))) {
            throw 'ReleaseGate requires the repository native build layout; remove Cargo target/build-directory environment overrides before running it.'
        }
    }
    $compiler = Invoke-CheckedNativeCommand -FilePath 'rustc.exe' -Arguments @('-vV') -TimeoutMilliseconds 10000 -Operation 'Rust host check'
    if (@($compiler.Stdout -split '\r?\n' | Where-Object { $_ -eq 'host: x86_64-pc-windows-gnu' }).Count -ne 1) {
        throw 'ReleaseGate requires the native Windows GNU compiler selected by rust-toolchain.toml; do not substitute an MSVC or cross-compilation target.'
    }
}

function Invoke-ReleaseGate {
    Assert-ReleaseGateNativeBuild
    $case = 'qa01_release_gate_validates_host_key_rotation_agent_sftp_and_cancellation'
    # The gate rejects debug builds, and Cargo succeeds when a filter selects zero tests.
    # Native GNU validation above avoids creating a second --target output tree.
    $arguments = @('test', '--locked', '--release', '--test', 'linux_ssh_release_gate', $case, '--', '--ignored', '--exact', '--list')
    $listed = Invoke-CargoCommand -Arguments $arguments -TimeoutMilliseconds 600000
    if (@($listed.Stdout -split '\r?\n' | Where-Object { $_ -eq "${case}: test" }).Count -ne 1) {
        throw "Expected exactly one available QA-01 release gate: $case"
    }
    $arguments = @('test', '--locked', '--release', '--test', 'linux_ssh_release_gate', $case, '--', '--ignored', '--exact', '--nocapture', '--test-threads=1')
    [void](Invoke-CargoCommand -Arguments $arguments -TimeoutMilliseconds 150000 -PublishOutput)
}

function Get-SshPublicKeyFingerprint {
    param([string]$PublicKey, [string]$Operation)
    $result = Invoke-SshKeygen -Arguments @('-lf', $PublicKey, '-E', 'sha256') -Operation $Operation
    if ($result.Stdout -notmatch '(?:^|\s)(SHA256:[A-Za-z0-9+/]{43})(?:\s|$)') {
        throw "$Operation returned a malformed fingerprint"
    }
    return $Matches[1]
}

function Assert-ReleaseGateAgentReady {
    $service = Get-Service -Name 'ssh-agent' -ErrorAction SilentlyContinue
    if ($null -eq $service -or $service.Status -ne 'Running') {
        throw 'ReleaseGate requires the Windows OpenSSH Authentication Agent (ssh-agent) service to already be running. Start it before rerunning; this script does not change service state or startup configuration.'
    }
    try {
        # Exit code 1 means the running Agent currently has no identities.
        Invoke-SshAdd -Arguments @('-l') -AllowedExitCodes @(0, 1)
    } catch {
        throw 'ReleaseGate could not contact the running Windows ssh-agent. Verify the OpenSSH client and Agent named pipe before rerunning.'
    }
}

function Get-FixtureCleanupCommandTimeout {
    param(
        [System.Diagnostics.Stopwatch]$Clock,
        [ValidateRange(20, 120000)][int]$BudgetMilliseconds,
        [ValidateRange(1, 30000)][int]$MaximumMilliseconds = 1000
    )
    $remaining = $BudgetMilliseconds - [int]$Clock.ElapsedMilliseconds
    if ($remaining -le 0) { return 0 }
    return [Math]::Min($remaining, $MaximumMilliseconds)
}

function Invoke-FixtureDockerReadiness {
    param(
        [System.Diagnostics.Stopwatch]$Clock,
        [string[]]$Arguments,
        [ValidateRange(20, 120000)][int]$BudgetMilliseconds = 30000,
        [ValidateRange(1, 30000)][int]$MaximumMilliseconds = 2000,
        [ValidateRange(1, 5000)][int]$TerminationReserveMilliseconds = 1100
    )
    $observationBudget = $BudgetMilliseconds - $TerminationReserveMilliseconds
    if ($observationBudget -le 0) { throw 'Endpoint readiness budget does not leave time for process-tree cleanup' }
    $timeout = Get-FixtureCleanupCommandTimeout -Clock $Clock -BudgetMilliseconds $observationBudget -MaximumMilliseconds $MaximumMilliseconds
    if ($timeout -le 0) { throw 'Endpoint readiness deadline elapsed before the next Docker probe' }
    Invoke-FixtureDocker -Arguments $Arguments -TimeoutMilliseconds $timeout
}

function Get-FixtureDockerResourceObservation {
    param(
        [ValidateSet('Container', 'Image')][string]$Kind,
        [string]$Name,
        [string]$RunId,
        [System.Diagnostics.Stopwatch]$CleanupClock,
        [ValidateRange(1, 120000)][int]$ObservationBudgetMilliseconds
    )
    $timeout = Get-FixtureCleanupCommandTimeout -Clock $CleanupClock -BudgetMilliseconds $ObservationBudgetMilliseconds
    if ($timeout -le 0) { throw 'Docker cleanup deadline elapsed before observation' }
    if ($Kind -eq 'Container') {
        $listed = Invoke-FixtureDocker -Arguments @(
            'container', 'ls', '--all', '--no-trunc', '--quiet', '--filter', "name=^/${Name}$"
        ) -TimeoutMilliseconds $timeout
        $identities = @($listed -split '\r?\n' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $identityPattern = '^[0-9a-f]{64}$'
    } else {
        $listed = Invoke-FixtureDocker -Arguments @('image', 'ls', '--no-trunc', '--quiet', $Name) -TimeoutMilliseconds $timeout
        $identities = @($listed -split '\r?\n' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $identityPattern = '^sha256:[0-9a-f]{64}$'
    }

    if ($identities.Count -eq 0) {
        return [pscustomobject]@{ State = 'Absent'; Identity = $null; Problem = $null }
    }
    if ($identities.Count -ne 1) {
        return [pscustomobject]@{ State = 'Invalid'; Identity = $null; Problem = 'name resolved to multiple Docker objects' }
    }
    $identity = $identities[0].Trim()
    if ($identity -notmatch $identityPattern) {
        return [pscustomobject]@{ State = 'Invalid'; Identity = $null; Problem = 'Docker returned a malformed immutable identity' }
    }

    $inspectionArguments = if ($Kind -eq 'Container') {
        @('container', 'inspect', '--format', '{{.Id}}|{{ index .Config.Labels "shipforge.test.run" }}', $identity)
    } else {
        @('image', 'inspect', '--format', '{{.Id}}|{{ index .Config.Labels "shipforge.test.run" }}', $identity)
    }
    $timeout = Get-FixtureCleanupCommandTimeout -Clock $CleanupClock -BudgetMilliseconds $ObservationBudgetMilliseconds
    if ($timeout -le 0) { throw 'Docker cleanup deadline elapsed before ownership inspection' }
    $inspection = (Invoke-FixtureDocker -Arguments $inspectionArguments -TimeoutMilliseconds $timeout).Trim()
    $separator = $inspection.IndexOf('|')
    if ($separator -le 0 -or $inspection.Contains("`n") -or $inspection.Contains("`r")) {
        return [pscustomobject]@{ State = 'Invalid'; Identity = $identity; Problem = 'Docker returned malformed ownership evidence' }
    }
    $inspectedIdentity = $inspection.Substring(0, $separator)
    $owner = $inspection.Substring($separator + 1)
    if ($inspectedIdentity -ne $identity) {
        return [pscustomobject]@{ State = 'Invalid'; Identity = $identity; Problem = 'Docker identity changed during ownership verification' }
    }
    if ($owner -ne $RunId) {
        return [pscustomobject]@{ State = 'Foreign'; Identity = $identity; Problem = 'owner label does not match' }
    }
    return [pscustomobject]@{ State = 'Owned'; Identity = $identity; Problem = $null }
}

function Complete-FixtureDockerResources {
    param(
        [string[]]$Containers,
        [string]$RunId,
        [string]$Image,
        [bool]$ImageBuilt,
        [ValidateRange(20, 120000)][int]$CleanupBudgetMilliseconds = 45000,
        [ValidateRange(20, 120000)][int]$CleanupObservationMilliseconds = 30000,
        [ValidateRange(1, 30000)][int]$CleanupSettleMilliseconds = 2000,
        [ValidateRange(1, 30000)][int]$CleanupFreshnessMilliseconds = 1500,
        [ValidateRange(1, 1000)][int]$CleanupPollMilliseconds = 100,
        [ValidateRange(2, 20)][int]$RequiredAbsentObservations = 3,
        [ValidateRange(1, 5000)][int]$CleanupTerminationReserveMilliseconds = 1100
    )
    $failures = [System.Collections.Generic.List[string]]::new()
    $resources = [System.Collections.Generic.List[object]]::new()
    $expectedContainers = @("shipforge-m1-$RunId-a", "shipforge-m1-$RunId-b")
    foreach ($container in $Containers) {
        if ($container -notin $expectedContainers) {
            $failures.Add("Container ${container}: name is outside this disposable run; it was preserved")
            continue
        }
        $resources.Add([pscustomobject]@{
            Kind = 'Container'; Name = $container; Finished = $false
            AbsentSinceMilliseconds = $null; AbsentCount = 0
            LastObservedMilliseconds = $null; LastProblem = 'not yet observed'
            LastOwnProblem = $null
        })
    }
    if ($ImageBuilt) {
        if ($Image -ne "shipforge-m1-fixture:$RunId") {
            $failures.Add("Image ${Image}: name is outside this disposable run; it was preserved")
        } else {
            $resources.Add([pscustomobject]@{
                Kind = 'Image'; Name = $Image; Finished = $false
                AbsentSinceMilliseconds = $null; AbsentCount = 0
                LastObservedMilliseconds = $null; LastProblem = 'not yet observed'
                LastOwnProblem = $null
            })
        }
    }

    # One monotonic total budget contains the absolute observation horizon, a
    # post-horizon confirmation phase, and the final bounded-command Job Object
    # termination reserve. No absence observed before the horizon is eligible
    # for success.
    $commandDeadlineMilliseconds = $CleanupBudgetMilliseconds - $CleanupTerminationReserveMilliseconds
    if ($CleanupObservationMilliseconds -ge $commandDeadlineMilliseconds) {
        foreach ($resource in @($resources | Where-Object { -not $_.Finished })) {
            $failures.Add("$($resource.Kind) $($resource.Name): cleanup budget does not leave a positive post-observation confirmation window")
        }
        return $failures.ToArray()
    }

    # A killed Docker client can leave a daemon request in flight. Reconcile all
    # attempted resources through the complete observation horizon; one early
    # stable absence is not proof that a delayed daemon operation cannot publish.
    # Once the horizon is reached, discard every nonterminal resource's earlier
    # evidence and require a new complete, settled, fresh confirmation sweep.
    $cleanupClock = [System.Diagnostics.Stopwatch]::StartNew()
    $phase = 'Observation'
    $confirmationSucceeded = $false
    :cleanup while (@($resources | Where-Object { -not $_.Finished }).Count -gt 0) {
        if ($phase -eq 'Observation' -and $cleanupClock.ElapsedMilliseconds -ge $CleanupObservationMilliseconds) {
            foreach ($resource in @($resources | Where-Object { -not $_.Finished })) {
                $resource.AbsentSinceMilliseconds = $null
                $resource.AbsentCount = 0
                $resource.LastObservedMilliseconds = $null
                $resource.LastProblem = 'post-observation absence has not been confirmed'
                $resource.LastOwnProblem = $null
            }
            $phase = 'Confirmation'
            continue cleanup
        }

        $phaseDeadlineMilliseconds = if ($phase -eq 'Observation') {
            $CleanupObservationMilliseconds
        } else {
            $commandDeadlineMilliseconds
        }
        if ($cleanupClock.ElapsedMilliseconds -ge $phaseDeadlineMilliseconds) {
            if ($phase -eq 'Observation') { continue cleanup }
            break cleanup
        }

        $sweepResources = @($resources | Where-Object { -not $_.Finished })
        $sweepCompleted = $true
        :sweep foreach ($resource in $sweepResources) {
            $timeout = Get-FixtureCleanupCommandTimeout -Clock $cleanupClock -BudgetMilliseconds $phaseDeadlineMilliseconds
            if ($timeout -le 0) {
                $sweepCompleted = $false
                break sweep
            }
            try {
                $observation = Get-FixtureDockerResourceObservation -Kind $resource.Kind -Name $resource.Name -RunId $RunId -CleanupClock $cleanupClock -ObservationBudgetMilliseconds $phaseDeadlineMilliseconds
                $resource.LastObservedMilliseconds = $cleanupClock.ElapsedMilliseconds
                switch ($observation.State) {
                    'Absent' {
                        $now = $cleanupClock.ElapsedMilliseconds
                        if ($null -eq $resource.AbsentSinceMilliseconds) { $resource.AbsentSinceMilliseconds = $now }
                        $resource.AbsentCount++
                        $resource.LastOwnProblem = $null
                        $resource.LastProblem = 'absence has not completed its settle window'
                        if ($resource.AbsentCount -ge $RequiredAbsentObservations -and
                            ($now - $resource.AbsentSinceMilliseconds) -ge $CleanupSettleMilliseconds) {
                            $resource.LastProblem = $null
                        }
                    }
                    'Owned' {
                        if ($phase -eq 'Confirmation') {
                            # Any post-horizon mutation restarts the shared
                            # confirmation window. Success must be based wholly
                            # on evidence collected after the last removal.
                            foreach ($candidate in @($resources | Where-Object { -not $_.Finished })) {
                                $candidate.AbsentSinceMilliseconds = $null
                                $candidate.AbsentCount = 0
                                $candidate.LastObservedMilliseconds = $null
                                $candidate.LastProblem = 'confirmation evidence was invalidated by an owned-object removal in the same sweep'
                            }
                        }
                        $resource.AbsentSinceMilliseconds = $null
                        $resource.AbsentCount = 0
                        $resource.LastOwnProblem = $null
                        $resource.LastProblem = 'owned object removal has not yet been confirmed absent'
                        $timeout = Get-FixtureCleanupCommandTimeout -Clock $cleanupClock -BudgetMilliseconds $phaseDeadlineMilliseconds
                        if ($timeout -le 0) {
                            $sweepCompleted = $false
                            break sweep
                        }
                        $removeArguments = if ($resource.Kind -eq 'Container') {
                            @('container', 'rm', '--force', $observation.Identity)
                        } else {
                            @('image', 'rm', $observation.Identity)
                        }
                        Invoke-FixtureDocker -Arguments $removeArguments -TimeoutMilliseconds $timeout | Out-Null
                    }
                    default {
                        # Invalid identity or a different owner is never safe to delete.
                        $failures.Add("$($resource.Kind) $($resource.Name): $($observation.Problem); it was preserved")
                        $resource.Finished = $true
                        $resource.LastProblem = $null
                    }
                }
            } catch {
                if ($phase -eq 'Confirmation') {
                    # A timed-out observation or mutation has an unknown result;
                    # no resource may reuse evidence from before that boundary.
                    foreach ($candidate in @($resources | Where-Object { -not $_.Finished })) {
                        $candidate.AbsentSinceMilliseconds = $null
                        $candidate.AbsentCount = 0
                        $candidate.LastObservedMilliseconds = $null
                        $candidate.LastProblem = 'confirmation evidence was invalidated by an unknown Docker result in the same sweep'
                    }
                }
                $resource.AbsentSinceMilliseconds = $null
                $resource.AbsentCount = 0
                $resource.LastProblem = 'Docker observation or removal did not complete'
                $resource.LastOwnProblem = 'Docker observation or removal did not complete'
                if ($cleanupClock.ElapsedMilliseconds -ge $phaseDeadlineMilliseconds) {
                    $sweepCompleted = $false
                    break sweep
                }
            }
        }
        if (-not $sweepCompleted) {
            if ($phase -eq 'Observation') {
                # The bounded command has synchronously terminated/reaped before
                # returning. The next iteration crosses the horizon and resets
                # all partial or stale evidence before confirmation.
                continue cleanup
            }
            foreach ($resource in @($sweepResources | Where-Object { -not $_.Finished })) {
                $resource.AbsentSinceMilliseconds = $null
                $resource.AbsentCount = 0
                $resource.LastProblem = 'the final post-observation cleanup sweep did not complete inside the total cleanup budget'
            }
            break cleanup
        }

        if ($phase -eq 'Confirmation') {
            $sweepEndedMilliseconds = $cleanupClock.ElapsedMilliseconds
            $activeResources = @($resources | Where-Object { -not $_.Finished })
            $allSettledAbsent = $activeResources.Count -gt 0
            foreach ($resource in $activeResources) {
                $freshObservation = $null -ne $resource.LastObservedMilliseconds -and
                    ($sweepEndedMilliseconds - $resource.LastObservedMilliseconds) -le $CleanupFreshnessMilliseconds
                if ($null -ne $resource.LastProblem -or
                    $null -eq $resource.AbsentSinceMilliseconds -or
                    $resource.AbsentCount -lt $RequiredAbsentObservations -or
                    -not $freshObservation) {
                    $allSettledAbsent = $false
                    break
                }
            }
            if ($allSettledAbsent) {
                $confirmationSucceeded = $true
                break cleanup
            }
        }

        if (@($resources | Where-Object { -not $_.Finished }).Count -gt 0) {
            $remaining = $phaseDeadlineMilliseconds - [int]$cleanupClock.ElapsedMilliseconds
            if ($remaining -gt 0) { Start-Sleep -Milliseconds ([Math]::Min($CleanupPollMilliseconds, $remaining)) }
        }
    }
    if (-not $confirmationSucceeded) {
        $confirmationEndedMilliseconds = $cleanupClock.ElapsedMilliseconds
        foreach ($resource in @($resources | Where-Object { -not $_.Finished })) {
            $freshObservation = $null -ne $resource.LastObservedMilliseconds -and
                ($confirmationEndedMilliseconds - $resource.LastObservedMilliseconds) -le $CleanupFreshnessMilliseconds
            $settledAbsent = $null -eq $resource.LastProblem -and
                $null -ne $resource.AbsentSinceMilliseconds -and
                $resource.AbsentCount -ge $RequiredAbsentObservations -and
                $freshObservation
            if (-not $settledAbsent) {
                if ($null -eq $resource.LastProblem -and -not $freshObservation) {
                    $resource.LastProblem = 'the last post-observation absence was stale at the total cleanup deadline'
                }
                $problem = if ($null -ne $resource.LastOwnProblem) {
                    $resource.LastOwnProblem
                } else {
                    $resource.LastProblem
                }
                $failures.Add("$($resource.Kind) $($resource.Name): cleanup confirmation failed ($problem)")
            }
        }
    }
    return $failures.ToArray()
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
        [string]$AgentIdentity,
        [bool]$AgentIdentityAdded,
        [string]$AgentRecovery,
        [bool]$AgentRecoveryCreated,
        $KeepAlive,
        [bool]$KeepAliveStarted,
        [ValidateRange(20, 120000)][int]$CleanupBudgetMilliseconds = 45000,
        [ValidateRange(20, 120000)][int]$CleanupObservationMilliseconds = 30000,
        [ValidateRange(1, 30000)][int]$CleanupSettleMilliseconds = 2000,
        [ValidateRange(1, 30000)][int]$CleanupFreshnessMilliseconds = 1500,
        [ValidateRange(1, 1000)][int]$CleanupPollMilliseconds = 100,
        [ValidateRange(2, 20)][int]$RequiredAbsentObservations = 3,
        [ValidateRange(1, 5000)][int]$CleanupTerminationReserveMilliseconds = 1100
    )
    $failures = [System.Collections.Generic.List[string]]::new()
    $keepAgentRecovery = $false
    $agentRecoveryVerified = $false
    try {
        foreach ($failure in @(Complete-FixtureDockerResources -Containers $Containers -RunId $RunId -Image $Image -ImageBuilt $ImageBuilt -CleanupBudgetMilliseconds $CleanupBudgetMilliseconds -CleanupObservationMilliseconds $CleanupObservationMilliseconds -CleanupSettleMilliseconds $CleanupSettleMilliseconds -CleanupFreshnessMilliseconds $CleanupFreshnessMilliseconds -CleanupPollMilliseconds $CleanupPollMilliseconds -RequiredAbsentObservations $RequiredAbsentObservations -CleanupTerminationReserveMilliseconds $CleanupTerminationReserveMilliseconds)) {
            $failures.Add($failure)
        }
        if ($AgentIdentityAdded) {
            try {
                $resolvedTemporary = [System.IO.Path]::GetFullPath($Temporary)
                $parentPrefix = [System.IO.Path]::TrimEndingDirectorySeparator([System.IO.Path]::GetFullPath($TemporaryParent)) + [System.IO.Path]::DirectorySeparatorChar
                $expectedRecovery = [System.IO.Path]::GetFullPath((Join-Path $TemporaryParent "shipforge-m1-agent-recovery-$RunId.pub"))
                if (-not $AgentRecoveryCreated -or [string]::IsNullOrWhiteSpace($AgentRecovery)) {
                    throw 'Exact SSH Agent public-key recovery material was not registered before identity add'
                }
                $resolvedRecovery = [System.IO.Path]::GetFullPath($AgentRecovery)
                if (-not $resolvedRecovery.Equals($expectedRecovery, [StringComparison]::OrdinalIgnoreCase) -or
                    -not (Test-Path -LiteralPath $resolvedRecovery)) {
                    throw 'Exact SSH Agent public-key recovery material is unavailable'
                }
                $agentRecoveryVerified = $true
                $expectedIdentity = [System.IO.Path]::GetFullPath((Join-Path $resolvedTemporary 'agent_ed25519.pub'))
                if ([string]::IsNullOrWhiteSpace($AgentIdentity)) { throw 'Owned Agent identity path is missing' }
                $resolvedIdentity = [System.IO.Path]::GetFullPath($AgentIdentity)
                if (-not $resolvedTemporary.StartsWith($parentPrefix, [StringComparison]::OrdinalIgnoreCase) -or
                    (Split-Path $resolvedTemporary -Leaf) -ne "shipforge-m1-$RunId" -or
                    -not $resolvedIdentity.Equals($expectedIdentity, [StringComparison]::OrdinalIgnoreCase)) {
                    throw 'Refused to remove an SSH Agent identity outside the exact disposable Agent public-key path'
                }
                Invoke-SshAdd -Arguments @('-d', $resolvedIdentity) -TimeoutMilliseconds 10000
            } catch {
                $keepAgentRecovery = $true
                $failures.Add("SSH Agent identity: $($_.Exception.Message); exact public-key recovery material retained at $AgentRecovery")
            }
        }
        if ($AgentRecoveryCreated -and -not $keepAgentRecovery) {
            try {
                $resolvedRecovery = [System.IO.Path]::GetFullPath($AgentRecovery)
                $expectedRecovery = [System.IO.Path]::GetFullPath((Join-Path $TemporaryParent "shipforge-m1-agent-recovery-$RunId.pub"))
                if (-not $resolvedRecovery.Equals($expectedRecovery, [StringComparison]::OrdinalIgnoreCase)) {
                    throw 'Refused to remove an SSH Agent recovery key outside the exact disposable public-key path'
                }
                if (Test-Path -LiteralPath $resolvedRecovery) {
                    Remove-Item -LiteralPath $resolvedRecovery -Force
                }
            } catch { $failures.Add("SSH Agent recovery material: $($_.Exception.Message)") }
        }
        foreach ($name in $SavedEnvironment.Keys) {
            try {
                if ($null -eq $SavedEnvironment[$name]) {
                    [Environment]::SetEnvironmentVariable($name, [System.Management.Automation.Language.NullString]::Value, 'Process')
                } else {
                    [Environment]::SetEnvironmentVariable($name, $SavedEnvironment[$name], 'Process')
                }
            } catch { $failures.Add("Environment ${name}: $($_.Exception.Message)") }
        }
        try {
            $resolvedTemporary = [System.IO.Path]::GetFullPath($Temporary)
            $parentPrefix = [System.IO.Path]::TrimEndingDirectorySeparator([System.IO.Path]::GetFullPath($TemporaryParent)) + [System.IO.Path]::DirectorySeparatorChar
            if (-not $resolvedTemporary.StartsWith($parentPrefix, [StringComparison]::OrdinalIgnoreCase) -or
                (Split-Path $resolvedTemporary -Leaf) -ne "shipforge-m1-$RunId") {
                throw 'Refused cleanup outside the exact disposable directory'
            }
            if ($AgentIdentityAdded -and -not $agentRecoveryVerified) {
                foreach ($sensitiveLeaf in @('identity_ed25519', 'identity_ed25519.pub', 'agent_ed25519', 'authorized_keys')) {
                    $sensitivePath = Join-Path $resolvedTemporary $sensitiveLeaf
                    if (Test-Path -LiteralPath $sensitivePath) {
                        Remove-Item -LiteralPath $sensitivePath -Force
                    }
                }
            } elseif (Test-Path -LiteralPath $resolvedTemporary) {
                Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force
            }
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
                            # This object is the exact WSL process created by this run.
                            $KeepAlive.Kill($true)
                            if (-not $KeepAlive.WaitForExit(1000)) {
                                throw 'Owned WSL helper did not exit after bounded process-tree termination'
                            }
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

if ($runReleaseGate) { Assert-ReleaseGateAgentReady }

try {
    # systemd services do not keep a WSL instance alive. Hold an input pipe open
    # while Windows cargo is running, without changing WSL or Docker settings.
    $keepAlive = New-AcceptanceNativeProcess -FilePath 'wsl.exe' -Arguments @('-d', $Distribution, '--exec', '/bin/cat') -RedirectStandardInput -NoOutputRedirect
    # Register cleanup ownership before Start: interruption after the native
    # process is created must not leave an untracked WSL helper.
    $keepAliveStarted = $true
    if (-not $keepAlive.Start()) { throw 'Could not keep the disposable Docker runtime available' }

    New-Item -ItemType Directory -Path $temporary | Out-Null
    $identityKey = Join-Path $temporary 'identity_ed25519'
    $agentKey = Join-Path $temporary 'agent_ed25519'
    $authorizedKeys = Join-Path $temporary 'authorized_keys'
    [void](Invoke-SshKeygen -Arguments @('-q', '-t', 'ed25519', '-N', '', '-f', $identityKey) -Operation 'Disposable IdentityFile key generation')

    $authorizedKeyLines = [System.Collections.Generic.List[string]]::new()
    foreach ($line in [System.IO.File]::ReadAllLines("$identityKey.pub")) {
        if (-not [string]::IsNullOrWhiteSpace($line)) {
            $authorizedKeyLines.Add('environment="SHIPFORGE_QA01_AUTH=identity-file" ' + $line)
        }
    }
    if ($runReleaseGate) {
        [void](Invoke-SshKeygen -Arguments @('-q', '-t', 'ed25519', '-N', '', '-f', $agentKey) -Operation 'Disposable Agent key generation')
        foreach ($line in [System.IO.File]::ReadAllLines("$agentKey.pub")) {
            if (-not [string]::IsNullOrWhiteSpace($line)) {
                $authorizedKeyLines.Add('environment="SHIPFORGE_QA01_AUTH=ssh-agent" ' + $line)
            }
        }
        $identityFingerprint = Get-SshPublicKeyFingerprint -PublicKey "$identityKey.pub" -Operation 'IdentityFile public-key fingerprint'
        $agentFingerprint = Get-SshPublicKeyFingerprint -PublicKey "$agentKey.pub" -Operation 'Agent public-key fingerprint'
        if ($agentFingerprint -eq $identityFingerprint) { throw 'IdentityFile and Agent keys must be distinct' }
        $agentIdentity = "$agentKey.pub"
        $agentRecovery = Join-Path $temporaryParent "shipforge-m1-agent-recovery-$runId.pub"
        [System.IO.File]::Copy($agentIdentity, $agentRecovery, $false)
        $agentRecoveryCreated = $true
        # Mark the exact identity for cleanup before ssh-add: a timed-out add may
        # have reached the Agent even when the client cannot report success.
        $agentIdentityAdded = $true
        Invoke-SshAdd -Arguments @($agentKey)
    }
    if ($authorizedKeyLines.Count -ne $(if ($runReleaseGate) { 2 } else { 1 })) {
        throw 'Disposable authorized_keys must contain exactly the generated public keys'
    }
    $authorizedKeysText = ([string]::Join("`n", $authorizedKeyLines) + "`n")
    [System.IO.File]::WriteAllText($authorizedKeys, $authorizedKeysText, [System.Text.UTF8Encoding]::new($false))

    $contextResult = Invoke-WslCommand -Arguments @('wslpath', '-u', (Join-Path $PSScriptRoot 'fixtures/openssh')) -Operation 'Fixture build-context path conversion'
    $context = $contextResult.Stdout.Trim()
    if ([string]::IsNullOrWhiteSpace($context)) { throw 'Could not resolve the fixture build context in WSL' }
    $authorizedKeysResult = Invoke-WslCommand -Arguments @('wslpath', '-u', $authorizedKeys) -Operation 'authorized_keys path conversion'
    $authorizedKeysLinux = $authorizedKeysResult.Stdout.Trim()
    if ([string]::IsNullOrWhiteSpace($authorizedKeysLinux)) { throw 'Could not resolve the disposable authorized_keys path in WSL' }

    $imageBuilt = $true
    $fixtureDockerfile = if ($Suite -eq 'ServiceCommands') { "$context/Dockerfile.pm2" } else { "$context/Dockerfile" }
    $buildArguments = @('build', '-f', $fixtureDockerfile, '--label', "shipforge.test.run=$runId", '-t', $image)
    if ($Suite -eq 'ServiceCommands') { $buildArguments += @('--build-arg', "NPM_REGISTRY=$NpmRegistry") }
    Invoke-FixtureDocker -Arguments ($buildArguments + @($context)) -TimeoutMilliseconds 300000 -PublishOutput | Out-Null
    foreach ($suffix in @('A', 'B')) {
        $container = "shipforge-m1-$runId-$($suffix.ToLowerInvariant())"
        # Track the exact name before create: a timed-out create may still finish in the daemon.
        $containers += $container
        Invoke-FixtureDocker -Arguments @('container', 'create', '--name', $container, '--label', "shipforge.test.run=$runId", '--publish', '127.0.0.1::22', $image) -TimeoutMilliseconds 30000 | Out-Null
        Invoke-FixtureDocker -Arguments @('cp', $authorizedKeysLinux, "${container}:/fixture/authorized_keys") -TimeoutMilliseconds 30000 | Out-Null
        Invoke-FixtureDocker -Arguments @('container', 'start', $container) -TimeoutMilliseconds 30000 | Out-Null

        $fingerprint = $null
        $readinessBudgetMilliseconds = 30000
        $readinessTerminationReserveMilliseconds = 1100
        $readinessObservationBudgetMilliseconds = $readinessBudgetMilliseconds - $readinessTerminationReserveMilliseconds
        $readyTimer = [System.Diagnostics.Stopwatch]::StartNew()
        while ($readyTimer.ElapsedMilliseconds -lt $readinessObservationBudgetMilliseconds) {
            try {
                # Re-check both facts in this iteration: stale success from an
                # earlier round must not make a dead sshd look ready.
                Invoke-FixtureDockerReadiness -Clock $readyTimer -BudgetMilliseconds $readinessBudgetMilliseconds -TerminationReserveMilliseconds $readinessTerminationReserveMilliseconds -Arguments @(
                    'exec', $container, '/bin/sh', '-c',
                    'test -s /run/sshd/shipforge.pid && kill -0 "$(cat /run/sshd/shipforge.pid)"'
                ) | Out-Null
                $fingerprintOutput = Invoke-FixtureDockerReadiness -Clock $readyTimer -BudgetMilliseconds $readinessBudgetMilliseconds -TerminationReserveMilliseconds $readinessTerminationReserveMilliseconds -Arguments @('exec', $container, 'ssh-keygen', '-lf', '/fixture/host_ed25519.pub', '-E', 'sha256')
                if ($fingerprintOutput -match '(?:^|\s)(SHA256:[A-Za-z0-9+/]{43})(?:\s|$)') {
                    $fingerprint = $Matches[1]
                    break
                }
            } catch { }
            $readinessRemaining = $readinessObservationBudgetMilliseconds - [int]$readyTimer.ElapsedMilliseconds
            if ($readinessRemaining -gt 0) {
                Start-Sleep -Milliseconds ([Math]::Min(100, $readinessRemaining))
            }
        }
        if ([string]::IsNullOrWhiteSpace($fingerprint)) { throw "Endpoint $suffix sshd and Host Key were not simultaneously ready within 30 seconds" }
        [Environment]::SetEnvironmentVariable("SHIPFORGE_TEST_HOST_KEY_$suffix", $fingerprint, 'Process')
        $binding = (Invoke-FixtureDockerReadiness -Clock $readyTimer -BudgetMilliseconds $readinessBudgetMilliseconds -TerminationReserveMilliseconds $readinessTerminationReserveMilliseconds -Arguments @('port', $container, '22/tcp')).Trim()
        if ($binding -notmatch '^127\.0\.0\.1:([1-9][0-9]*)$') { throw 'Fixture must listen on exactly one IPv4 loopback port' }
        [Environment]::SetEnvironmentVariable("SHIPFORGE_TEST_PORT_$suffix", $Matches[1], 'Process')
    }
    if ($env:SHIPFORGE_TEST_HOST_KEY_A -eq $env:SHIPFORGE_TEST_HOST_KEY_B) {
        throw 'Independent fixture endpoints unexpectedly use the same Host Key'
    }

    $env:SHIPFORGE_LINUX_ACCEPTANCE = '1'
    $env:SHIPFORGE_TEST_KEY = $identityKey
    if ($runReleaseGate) {
        $env:SHIPFORGE_QA01_OPENSSH = '1'
        $env:SHIPFORGE_QA01_SSH_HOST = '127.0.0.1'
        $env:SHIPFORGE_QA01_SSH_PORT = $env:SHIPFORGE_TEST_PORT_A
        $env:SHIPFORGE_QA01_SSH_USER = 'deploy'
        $env:SHIPFORGE_QA01_SSH_CURRENT_HOST_KEY = $env:SHIPFORGE_TEST_HOST_KEY_A
        $env:SHIPFORGE_QA01_SSH_PREVIOUS_HOST_KEY = $env:SHIPFORGE_TEST_HOST_KEY_B
        $env:SHIPFORGE_QA01_SSH_IDENTITY_FILE = $identityKey
        $env:SHIPFORGE_QA01_SSH_AGENT_FINGERPRINT = $agentFingerprint
    }

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
        if ($Suite -in @('All', 'Management')) {
            $cases += 'management_acceptance::real_linux_management_history_rollback_and_connections'
        }
        if ($Suite -eq 'ConnectionStability') {
            $cases += 'connection_stability::real_linux_fresh_connection_drop_stability'
        }
        if ($Suite -eq 'ServiceCommands') {
            $cases += 'service_commands::real_pm2_versions_failure_recovery_and_component_isolation'
        }
        foreach ($case in $cases) {
            # The PM2 case covers multiple full lifecycle operations. Its own
            # ten-minute deadline must finish before the native process guard.
            $caseTimeout = if ($Suite -eq 'ServiceCommands') { 660000 } else { 300000 }
            Invoke-FixtureCase -Case $case -ExecutionTimeoutMilliseconds $caseTimeout
        }
        if ($runReleaseGate) { Invoke-ReleaseGate }
    } finally { Pop-Location }
} catch {
    $operationError = $_
    foreach ($container in $containers) {
        try { Get-FixtureDiagnostics -Container $container -RunId $runId -Distribution $Distribution | Out-Host }
        catch { Write-Warning "Could not read diagnostics for $container" }
    }
} finally {
    $cleanupErrors = @(Complete-FixtureRun -Containers $containers -RunId $runId -Image $image -ImageBuilt $imageBuilt -Temporary $temporary -TemporaryParent $temporaryParent -SavedEnvironment $savedEnvironment -AgentIdentity $agentIdentity -AgentIdentityAdded $agentIdentityAdded -AgentRecovery $agentRecovery -AgentRecoveryCreated $agentRecoveryCreated -KeepAlive $keepAlive -KeepAliveStarted $keepAliveStarted)
}
if ($null -ne $operationError) {
    foreach ($cleanupError in $cleanupErrors) { Write-Warning "Cleanup incomplete: $cleanupError" }
    throw $operationError
}
if ($cleanupErrors.Count -gt 0) { throw "Acceptance cleanup incomplete: $($cleanupErrors -join '; ')" }
