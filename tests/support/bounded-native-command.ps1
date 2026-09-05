#requires -Version 7.0
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^Local\\ShipForgeQa01-[a-f0-9]{32}$')]
    [string]$StartEvent,
    [Parameter(Mandatory)]
    [string]$FilePath,
    [Parameter(Mandatory)]
    [string]$ArgumentsBase64
)

$ErrorActionPreference = 'Stop'
$startGate = $null
$target = $null
try {
    $startGate = [System.Threading.EventWaitHandle]::OpenExisting($StartEvent)
    if (-not $startGate.WaitOne(30000)) { exit 125 }

    $json = [System.Text.Encoding]::UTF8.GetString(
        [Convert]::FromBase64String($ArgumentsBase64)
    )
    $options = [System.Text.Json.JsonSerializerOptions]::new()
    $arguments = [System.Text.Json.JsonSerializer]::Deserialize[string[]]($json, $options)
    if ($null -eq $arguments) { exit 125 }

    $target = [System.Diagnostics.Process]::new()
    $target.StartInfo.FileName = $FilePath
    $target.StartInfo.UseShellExecute = $false
    $target.StartInfo.CreateNoWindow = $true
    $target.StartInfo.RedirectStandardInput = $true
    foreach ($argument in $arguments) { $target.StartInfo.ArgumentList.Add($argument) }
    if (-not $target.Start()) { exit 125 }
    $target.StandardInput.Close()
    $target.WaitForExit()
    exit $target.ExitCode
} catch {
    [Console]::Error.WriteLine('ShipForge bounded native helper could not execute the requested program.')
    exit 125
} finally {
    if ($null -ne $target) { $target.Dispose() }
    if ($null -ne $startGate) { $startGate.Dispose() }
}
