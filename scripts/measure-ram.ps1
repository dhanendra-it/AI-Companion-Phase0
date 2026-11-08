[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('robot-only', 'chat-open', 'chat-closed')]
    [string] $State,
    [int] $Samples = 10,
    [int] $IntervalSeconds = 1,
    [int] $RootPid = 0,
    [string] $ProcessName = 'ai-companion-phase0',
    [string] $OutputPath = '.\ram-measurements.csv'
)

$ErrorActionPreference = 'Stop'
if ($Samples -lt 1 -or $IntervalSeconds -lt 1) { throw 'Samples and IntervalSeconds must be positive.' }
if ($RootPid -eq 0) {
    $matches = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)
    if ($matches.Count -ne 1) {
        throw "Expected one '$ProcessName' process, found $($matches.Count). Pass -RootPid <PID> if needed."
    }
    $RootPid = $matches[0].Id
}

function Get-ProcessTreeIds([int] $Root) {
    $all = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId)
    $ids = [System.Collections.Generic.HashSet[int]]::new()
    [void]$ids.Add($Root)
    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($proc in $all) {
            $pidValue = [int]$proc.ProcessId
            if (-not $ids.Contains($pidValue) -and $ids.Contains([int]$proc.ParentProcessId)) {
                [void]$ids.Add($pidValue); $changed = $true
            }
        }
    }
    return @($ids)
}

$rows = for ($sample = 1; $sample -le $Samples; $sample++) {
    $stamp = Get-Date -Format 'yyyy-MM-ddTHH:mm:ss.fffK'
    $ids = @(Get-ProcessTreeIds -Root $RootPid)
    $processes = @(Get-Process -Id $ids -ErrorAction SilentlyContinue)
    $workingSet = [int64](($processes | Measure-Object -Property WorkingSet64 -Sum).Sum)
    $privateBytes = [int64](($processes | Measure-Object -Property PrivateMemorySize64 -Sum).Sum)
    [pscustomobject]@{
        Timestamp = $stamp; State = $State; Sample = $sample; RootPid = $RootPid
        ProcessCount = $processes.Count; WorkingSetBytes = $workingSet; PrivateBytes = $privateBytes
        WorkingSetMiB = [math]::Round($workingSet / 1MB, 2); PrivateMiB = [math]::Round($privateBytes / 1MB, 2)
    }
    if ($sample -lt $Samples) { Start-Sleep -Seconds $IntervalSeconds }
}

$rows | Export-Csv -Path $OutputPath -NoTypeInformation -Append:(Test-Path $OutputPath)
$rows | Format-Table Timestamp, State, Sample, ProcessCount, WorkingSetMiB, PrivateMiB -AutoSize
$summary = [pscustomobject]@{
    State = $State; Samples = $Samples; AverageWorkingSetMiB = [math]::Round(($rows | Measure-Object WorkingSetMiB -Average).Average, 2)
    PeakWorkingSetMiB = [math]::Round(($rows | Measure-Object WorkingSetMiB -Maximum).Maximum, 2)
    AveragePrivateMiB = [math]::Round(($rows | Measure-Object PrivateMiB -Average).Average, 2)
    PeakPrivateMiB = [math]::Round(($rows | Measure-Object PrivateMiB -Maximum).Maximum, 2)
}
Write-Host "`nState summary (MiB):"
$summary | Format-List
Write-Host "CSV appended to: $((Resolve-Path $OutputPath -ErrorAction SilentlyContinue) ?? $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath))"
