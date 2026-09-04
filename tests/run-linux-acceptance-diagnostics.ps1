# Failure-only, read-only diagnostics for the runner's exact disposable containers.
# Loading this file defines functions only; it never starts a process.

function New-FixtureDiagnosticProcess {
    param([string]$Distribution, [string[]]$Arguments)
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo.FileName = 'wsl.exe'
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    foreach ($argument in @('-d', $Distribution, '--exec', 'docker', '-H', 'unix:///var/run/docker.sock') + $Arguments) {
        $process.StartInfo.ArgumentList.Add($argument)
    }
    return $process
}

function Invoke-FixtureDiagnosticCommand {
    param(
        [string]$Distribution,
        [string[]]$Arguments,
        [ValidateRange(1, 5000)][int]$TimeoutMilliseconds = 5000,
        [ValidateRange(1, 65536)][int]$MaxOutputBytes = 65536
    )
    $process = New-FixtureDiagnosticProcess -Distribution $Distribution -Arguments $Arguments
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    $started = $false
    $timedOut = $false
    $truncated = $false
    $exitCode = $null
    $remainingBytes = $MaxOutputBytes
    $captures = @([System.IO.MemoryStream]::new(), [System.IO.MemoryStream]::new())
    $streams = @()
    try {
        if (-not $process.Start()) { throw 'Diagnostic helper did not start' }
        $started = $true
        $streams = @($process.StandardOutput.BaseStream, $process.StandardError.BaseStream)
        $buffers = @([byte[]]::new(4096), [byte[]]::new(4096))
        $reads = @($streams[0].ReadAsync($buffers[0], 0, 4096), $streams[1].ReadAsync($buffers[1], 0, 4096))
        $ended = @($false, $false)
        while (-not ($ended[0] -and $ended[1] -and $process.HasExited)) {
            if ($timer.ElapsedMilliseconds -ge $TimeoutMilliseconds) { $timedOut = $true; break }
            for ($index = 0; $index -lt 2; $index++) {
                if ($ended[$index] -or -not $reads[$index].IsCompleted) { continue }
                $count = $reads[$index].GetAwaiter().GetResult()
                if ($count -eq 0) { $ended[$index] = $true; continue }
                $take = [Math]::Min($remainingBytes, $count)
                $captures[$index].Write($buffers[$index], 0, $take)
                $remainingBytes -= $take
                if ($take -ne $count) { $truncated = $true }
                # Continue draining even after the retained-byte bound is reached.
                $reads[$index] = $streams[$index].ReadAsync($buffers[$index], 0, 4096)
            }
            if (-not ($ended[0] -and $ended[1] -and $process.HasExited)) {
                [System.Threading.Thread]::Sleep(5)
            }
        }
        if (-not $timedOut) { $exitCode = $process.ExitCode }
        return [pscustomobject]@{
            ExitCode = $exitCode
            TimedOut = $timedOut
            Truncated = $truncated
            Stdout = [System.Text.Encoding]::UTF8.GetString($captures[0].ToArray())
            Stderr = [System.Text.Encoding]::UTF8.GetString($captures[1].ToArray())
        }
    } finally {
        if ($started) {
            try {
                if (-not $process.HasExited) {
                    # Only the helper created above, never a container or arbitrary process.
                    $process.Kill($true)
                    [void]$process.WaitForExit(250)
                }
            } catch { }
        }
        foreach ($capture in $captures) { $capture.Dispose() }
        try { $process.Dispose() }
        finally {
            # Process.Dispose need not close manually consumed redirected streams.
            foreach ($stream in $streams) { try { $stream.Dispose() } catch { } }
        }
    }
}

function ConvertTo-FixtureDiagnosticText {
    param([string]$Text)
    # OpenSSH output is untrusted terminal text; fixture diagnostics need ASCII only.
    return [regex]::Replace($Text, '[^\x09\x0A\x0D\x20-\x7E]', '?')
}

function Get-FixtureDiagnostics {
    param([string]$Container, [string]$RunId, [string]$Distribution)
    if ($RunId -cnotmatch '^[a-f0-9]{32}$' -or $Container -cnotin @("shipforge-m1-$RunId-a", "shipforge-m1-$RunId-b")) {
        return 'Diagnostics refused: container name is not an exact member of this fixture run.'
    }
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $identity = Invoke-FixtureDiagnosticCommand -Distribution $Distribution -Arguments @(
            'inspect', '--format', '{{.Id}}|{{ index .Config.Labels "shipforge.test.run" }}', $Container
        ) -TimeoutMilliseconds 5000 -MaxOutputBytes 4096
        $expected = '^([a-f0-9]{64})\|' + $RunId + '$'
        if ($identity.TimedOut -or $identity.Truncated -or $identity.ExitCode -ne 0 -or $identity.Stdout.Trim() -cnotmatch $expected) {
            return 'Diagnostics refused: fixture identity could not be verified.'
        }
        # Names can be reused. Every subsequent read uses the verified immutable ID.
        $id = $Matches[1]
    } catch { return 'Diagnostics unavailable: fixture identity read failed.' }
    "Fixture diagnostics: $Container"
    $checks = @(
        @{ Label = 'sshd log tail'; Arguments = @('logs', '--tail', '80', '--timestamps', $id); Limit = 65536 },
        @{ Label = 'container state'; Arguments = @('inspect', '--format', 'status={{.State.Status}} running={{.State.Running}} restarting={{.State.Restarting}} oom={{.State.OOMKilled}} pid={{.State.Pid}} exit={{.State.ExitCode}} started={{.State.StartedAt}} finished={{.State.FinishedAt}}', $id); Limit = 4096 },
        @{ Label = 'effective sshd limits'; Arguments = @('exec', $id, '/usr/sbin/sshd', '-T', '-f', '/fixture/sshd_config'); Limit = 16384 }
    )
    foreach ($check in $checks) {
        $remaining = 20000 - $timer.ElapsedMilliseconds
        if ($remaining -le 0) { 'Diagnostic time budget exhausted; proceeding to cleanup.'; break }
        try {
            $result = Invoke-FixtureDiagnosticCommand -Distribution $Distribution -Arguments $check.Arguments -TimeoutMilliseconds ([int][Math]::Min(5000, $remaining)) -MaxOutputBytes $check.Limit
            if ($result.TimedOut -or $result.ExitCode -ne 0) {
                "$($check.Label): unavailable (timeout or nonzero exit); continuing diagnostics."
                continue
            }
            "$($check.Label):"
            if ($check.Label -eq 'effective sshd limits') {
                # Never print HostKey, AuthorizedKeysFile, environment, or full configuration.
                $result.Stdout -split '\r?\n' | Where-Object {
                    $_ -cmatch '^(loglevel|logingracetime|maxstartups|maxsessions|persourcemaxstartups|usedns|clientaliveinterval|clientalivecountmax) [A-Za-z0-9:]+$'
                } | ForEach-Object { ConvertTo-FixtureDiagnosticText $_ }
            } else {
                ConvertTo-FixtureDiagnosticText ($result.Stdout + $result.Stderr)
            }
            if ($result.Truncated) { 'Diagnostic output truncated at its retained-byte limit.' }
        } catch { "$($check.Label): diagnostic read failed; continuing to cleanup." }
    }
}
