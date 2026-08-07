#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ColayExe,

    [Parameter(Mandatory = $true)]
    [string]$FakeProviderExe,

    [Parameter(Mandatory = $true)]
    [string]$EvidenceRoot,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedSourceCommit
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ResponseTimeoutMs = 10000
$SerialMaxLimitMs = 5000
$SerialP95LimitMs = 5000
$ConcurrentLimitMs = 8000
$MinimumFreeGiB = 5
$CimOperationTimeoutSec = 5
$ProviderKeyNames = @(
    'OPENAI_API_KEY',
    'ANTHROPIC_API_KEY',
    'GEMINI_API_KEY',
    'GOOGLE_API_KEY',
    'AGY_API_KEY',
    'CODEX_API_KEY',
    'CLAUDE_API_KEY'
)
$ForbiddenUtilityNames = @('whoami.exe', 'icacls.exe')

$script:OwnedProcessIdentities = [System.Collections.Generic.List[object]]::new()
$script:ProcessOwnershipRefusals = [System.Collections.Generic.List[object]]::new()
$script:ForcedProcessCleanupEvidence = [System.Collections.Generic.List[object]]::new()
$script:HarnessProcessIdentity = $null
$script:ObservedStarts = [System.Collections.Generic.List[object]]::new()
$script:ProcessSnapshots = [System.Collections.Generic.List[object]]::new()
$script:ForbiddenStarts = [System.Collections.Generic.List[object]]::new()
$script:CommandEvidence = [System.Collections.Generic.List[object]]::new()
$script:CleanupErrors = [System.Collections.Generic.List[object]]::new()
$script:DiskVolumes = [ordered]@{}
$script:MinimumObservedFreeGiBByRoot = [ordered]@{}
$script:ProcessEventQueue = [System.Collections.Concurrent.ConcurrentQueue[object]]::new()
$script:LastProcessSnapshot = @()
$script:AmbiguousSnapshotPids = [System.Collections.Generic.HashSet[int]]::new()
$script:Watcher = $null
$script:WatcherSubscription = $null
$script:ProcessObservationMode = 'defense-in-depth-win32-process-post-exit-snapshots'
$script:WatcherFailure = $null
$script:ProcessObservationDelayForTestMs = 0
$script:ProcessObservationFailureForTest = $false
$script:ProcessExitTimeFailureForTest = $false
$script:ProcessFinalizeFailureForTest = $null
$script:ProcessSetupFailureForTest = $null
$script:ProcessSetupFailureEvidence = [System.Collections.Generic.List[object]]::new()
$script:ProcessBatchSetupFailureIndexForTest = 0
$script:ProcessBatchSetupFailureStageForTest = $null
$script:ProcessBatchCleanupEvidence = [System.Collections.Generic.List[object]]::new()
$script:TimingSelfTestFailureCleanupEvidence = $null
$script:RepoRoot = $null
$script:RunRoot = $null
$script:ColayHome = $null
$script:ResolvedColay = $null
$script:PythonExe = $null
$script:MainDaemonReadinessTimeoutMs = 5000
$script:MainDaemonReadinessPollIntervalMs = 50
$script:MainDaemonReadinessExitWaitLimitMs = 400
$script:MainDaemonReadinessOutputDrainLimitMs = 100
$script:MainDaemonReadinessInitialParseDelayForTestMs = 0

function Register-DiskVolume {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $fullPath = [System.IO.Path]::GetFullPath($Path)
    $root = [System.IO.Path]::GetPathRoot($fullPath)
    if ([string]::IsNullOrWhiteSpace($root)) {
        throw "cannot resolve a volume root for ${Label}: $fullPath"
    }
    $drive = [System.IO.DriveInfo]::new($root)
    if (-not $drive.IsReady) {
        throw "volume for ${Label} is not ready: $root"
    }
    $normalizedRoot = $drive.RootDirectory.FullName.TrimEnd('\').ToLowerInvariant()
    if (-not $script:DiskVolumes.Contains($normalizedRoot)) {
        $script:DiskVolumes[$normalizedRoot] = [pscustomobject]@{
            root = $drive.RootDirectory.FullName
            labels = [System.Collections.Generic.List[string]]::new()
        }
        $script:MinimumObservedFreeGiBByRoot[$normalizedRoot] = [double]::PositiveInfinity
    }
    $labels = $script:DiskVolumes[$normalizedRoot].labels
    if (-not $labels.Contains($Label)) {
        $labels.Add($Label)
    }
}

function Assert-FreeDisk {
    if ($script:DiskVolumes.Count -eq 0) {
        throw 'no runtime or evidence volume was registered for free-space checks'
    }
    $observed = [System.Collections.Generic.List[object]]::new()
    $belowFloor = [System.Collections.Generic.List[string]]::new()
    foreach ($entry in $script:DiskVolumes.GetEnumerator()) {
        $drive = [System.IO.DriveInfo]::new([string]$entry.Value.root)
        if (-not $drive.IsReady) {
            throw "registered volume is no longer ready: $($entry.Value.root)"
        }
        $freeGiB = [math]::Round($drive.AvailableFreeSpace / 1GB, 3)
        if ($freeGiB -lt [double]$script:MinimumObservedFreeGiBByRoot[$entry.Key]) {
            $script:MinimumObservedFreeGiBByRoot[$entry.Key] = $freeGiB
        }
        $observed.Add([pscustomobject]@{
            root = [string]$entry.Value.root
            labels = @($entry.Value.labels)
            free_gib = $freeGiB
        })
        if ($freeGiB -lt $MinimumFreeGiB) {
            $belowFloor.Add("$($entry.Value.root) (${freeGiB}GiB)")
        }
    }
    if ($belowFloor.Count -ne 0) {
        throw "free space fell below the ${MinimumFreeGiB}GiB safety floor on: $($belowFloor -join ', ')"
    }
    return $observed.ToArray()
}

function Get-DiskVolumeEvidence {
    $evidence = [System.Collections.Generic.List[object]]::new()
    foreach ($entry in $script:DiskVolumes.GetEnumerator()) {
        $minimum = [double]$script:MinimumObservedFreeGiBByRoot[$entry.Key]
        $evidence.Add([pscustomobject]@{
            root = [string]$entry.Value.root
            labels = @($entry.Value.labels)
            minimum_free_gib = if ([double]::IsPositiveInfinity($minimum)) { $null } else { $minimum }
        })
    }
    return $evidence.ToArray()
}

function Resolve-RequiredFile {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Label)
    $resolved = Resolve-Path -LiteralPath $Path -ErrorAction Stop
    if (-not (Test-Path -LiteralPath $resolved.Path -PathType Leaf)) {
        throw "$Label is not a file: $($resolved.Path)"
    }
    return [System.IO.Path]::GetFullPath($resolved.Path)
}

function New-ProcessIdentityKey {
    param(
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][datetime]$CreationTimeUtc
    )
    $normalizedCreation = ConvertTo-NormalizedProcessCreationUtc $CreationTimeUtc
    return "${ProcessId}:$($normalizedCreation.ToFileTimeUtc())"
}

function ConvertTo-NormalizedExecutablePath {
    param([Parameter(Mandatory = $true)][string]$Path)
    $fullPath = [System.IO.Path]::GetFullPath($Path)
    if ($fullPath.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        return '\\' + $fullPath.Substring(8)
    }
    if ($fullPath.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) {
        return $fullPath.Substring(4)
    }
    return $fullPath
}

function Get-ProcessIdentityEvidence {
    param([Parameter(Mandatory = $true)]$Identity)
    return [pscustomobject][ordered]@{
        identity_key = [string]$Identity.identity_key
        process_id = [int]$Identity.process_id
        parent_process_id = [int]$Identity.parent_process_id
        parent_identity_key = $Identity.parent_identity_key
        parent_chain = @($Identity.parent_chain)
        creation_time_utc = ([datetime]$Identity.creation_time_utc).ToString('o')
        exit_time_utc = if ($null -eq $Identity.exit_time_utc) { $null } else { ([datetime]$Identity.exit_time_utc).ToString('o') }
        executable_path = [string]$Identity.executable_path
        name = [string]$Identity.name
        source = [string]$Identity.source
        label = $Identity.label
        depth = [int]$Identity.depth
    }
}

function Find-OwnedProcessIdentity {
    param(
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][datetime]$CreationTimeUtc
    )
    $key = New-ProcessIdentityKey -ProcessId $ProcessId -CreationTimeUtc $CreationTimeUtc
    return @($script:OwnedProcessIdentities | Where-Object identity_key -CEQ $key | Select-Object -First 1)
}

function Add-DeduplicatedProcessOwnershipRefusal {
    param(
        [Parameter(Mandatory = $true)][string]$RefusalKey,
        [Parameter(Mandatory = $true)]$Evidence
    )
    $alreadyRecorded = @($script:ProcessOwnershipRefusals | Where-Object {
        $_.PSObject.Properties.Name -contains 'refusal_key' -and
        [string]$_.refusal_key -ceq $RefusalKey
    }).Count -ne 0
    if (-not $alreadyRecorded) {
        $script:ProcessOwnershipRefusals.Add($Evidence)
    }
}

function Register-OwnedProcessIdentity {
    param(
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][int]$ParentProcessId,
        [Parameter(Mandatory = $true)][datetime]$CreationTimeUtc,
        [Parameter(Mandatory = $true)][string]$ExecutablePath,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)]$ParentIdentity,
        [AllowNull()][string]$Label
    )
    if ($ProcessId -le 0 -or $ProcessId -eq $PID) {
        throw "refusing unsafe owned process id $ProcessId"
    }
    if ($ParentProcessId -ne [int]$ParentIdentity.process_id) {
        throw "owned process $ProcessId parent PID did not match its authoritative parent identity"
    }
    $createdAt = ConvertTo-NormalizedProcessCreationUtc $CreationTimeUtc
    $fullPath = ConvertTo-NormalizedExecutablePath $ExecutablePath
    $identityKey = New-ProcessIdentityKey -ProcessId $ProcessId -CreationTimeUtc $createdAt
    $existing = @($script:OwnedProcessIdentities | Where-Object identity_key -CEQ $identityKey | Select-Object -First 1)
    if ($existing.Count -ne 0) {
        if (-not ([string]$existing[0].executable_path).Equals($fullPath, [StringComparison]::OrdinalIgnoreCase) -or
            [int]$existing[0].parent_process_id -ne $ParentProcessId -or
            [string]$existing[0].parent_identity_key -cne [string]$ParentIdentity.identity_key) {
            throw "owned process identity $identityKey changed path or parent identity"
        }
        return $existing[0]
    }
    $identity = [pscustomobject][ordered]@{
        identity_key = $identityKey
        process_id = $ProcessId
        parent_process_id = $ParentProcessId
        parent_identity_key = [string]$ParentIdentity.identity_key
        parent_chain = @($ParentIdentity.parent_chain) + $identityKey
        creation_time_utc = $createdAt
        exit_time_utc = $null
        executable_path = $fullPath
        name = $Name
        source = $Source
        label = $Label
        depth = [int]$ParentIdentity.depth + 1
        registered_at_utc = [datetime]::UtcNow
    }
    $script:OwnedProcessIdentities.Add($identity)
    return $identity
}

function Set-OwnedProcessIdentityExit {
    param(
        [AllowNull()]$Identity,
        [Parameter(Mandatory = $true)][datetime]$ExitTimeUtc
    )
    if ($null -eq $Identity) { return }
    $normalizedExit = ConvertTo-NormalizedProcessCreationUtc $ExitTimeUtc
    if ($normalizedExit -lt [datetime]$Identity.creation_time_utc) {
        throw "owned process $($Identity.identity_key) exit preceded its creation"
    }
    if ($null -eq $Identity.exit_time_utc -or $normalizedExit -lt [datetime]$Identity.exit_time_utc) {
        $Identity.exit_time_utc = $normalizedExit
    }
}

function Get-ProcessGenerationObservation {
    param(
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][datetime]$ExpectedCreationTimeUtc,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutablePath
    )
    $observation = [pscustomobject][ordered]@{
        process_id = $ProcessId
        expected_creation_time_utc = (ConvertTo-NormalizedProcessCreationUtc $ExpectedCreationTimeUtc).ToString('o')
        expected_executable_path = ConvertTo-NormalizedExecutablePath $ExpectedExecutablePath
        process_exists = $false
        observed_creation_time_utc = $null
        observed_executable_path = $null
        identity_verified = $false
        expected_generation_live = $false
        observation_error = $null
    }
    $candidate = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($null -eq $candidate) { return $observation }
    $observation.process_exists = $true
    try {
        if ($candidate.HasExited) {
            $observation.process_exists = $false
            return $observation
        }
        $observedCreation = ConvertTo-NormalizedProcessCreationUtc $candidate.StartTime.ToUniversalTime()
        $rawObservedPath = [string]$candidate.Path
        if ([string]::IsNullOrWhiteSpace($rawObservedPath)) {
            throw 'live process exposed no executable path'
        }
        $observedPath = ConvertTo-NormalizedExecutablePath $rawObservedPath
        $observation.observed_creation_time_utc = $observedCreation.ToString('o')
        $observation.observed_executable_path = $observedPath
        $observation.identity_verified = $true
        $observation.expected_generation_live = $observedCreation -eq
            (ConvertTo-NormalizedProcessCreationUtc $ExpectedCreationTimeUtc) -and
            $observedPath.Equals(
                (ConvertTo-NormalizedExecutablePath $ExpectedExecutablePath),
                [StringComparison]::OrdinalIgnoreCase
            )
    } catch {
        $observation.observation_error = $_.Exception.Message
        try {
            if ($candidate.HasExited) {
                $observation.process_exists = $false
                $observation.observation_error = $null
            }
        } catch { }
    } finally {
        $candidate.Dispose()
    }
    return $observation
}

function Get-ProcessLivenessObservation {
    param([Parameter(Mandatory = $true)][int]$ProcessId)
    $observation = [pscustomobject][ordered]@{
        process_id = $ProcessId
        process_exists = $false
        observation_error = $null
    }
    $candidate = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($null -eq $candidate) { return $observation }
    try {
        if (-not $candidate.HasExited) {
            $observation.process_exists = $true
        }
    } catch {
        $observation.process_exists = $true
        $observation.observation_error = $_.Exception.Message
    } finally {
        $candidate.Dispose()
    }
    return $observation
}

function Test-SnapshotMatchesOwnedIdentity {
    param(
        [Parameter(Mandatory = $true)]$SnapshotRow,
        [Parameter(Mandatory = $true)]$Identity
    )
    if ([int]$SnapshotRow.process_id -ne [int]$Identity.process_id -or
        $null -eq $SnapshotRow.creation_time_utc -or
        (ConvertTo-NormalizedProcessCreationUtc $SnapshotRow.creation_time_utc) -ne [datetime]$Identity.creation_time_utc -or
        [string]::IsNullOrWhiteSpace([string]$SnapshotRow.executable_path)) {
        return $false
    }
    return ([string]$SnapshotRow.executable_path).Equals(
        [string]$Identity.executable_path,
        [StringComparison]::OrdinalIgnoreCase
    )
}

function Start-AncestryObservation {
    $currentProcess = [System.Diagnostics.Process]::GetCurrentProcess()
    try {
        $harnessCreation = ConvertTo-NormalizedProcessCreationUtc $currentProcess.StartTime.ToUniversalTime()
        $harnessPath = ConvertTo-NormalizedExecutablePath $currentProcess.MainModule.FileName
        $harnessKey = New-ProcessIdentityKey -ProcessId $PID -CreationTimeUtc $harnessCreation
        $script:HarnessProcessIdentity = [pscustomobject][ordered]@{
            identity_key = $harnessKey
            process_id = $PID
            parent_process_id = 0
            parent_identity_key = $null
            parent_chain = @($harnessKey)
            creation_time_utc = $harnessCreation
            exit_time_utc = $null
            executable_path = $harnessPath
            name = [System.IO.Path]::GetFileName($harnessPath)
            source = 'harness-root'
            label = 'windows-state-acl-stress-harness'
            depth = 0
        }
    } finally {
        $currentProcess.Dispose()
    }
    try {
        $query = [System.Management.WqlEventQuery]::new('SELECT * FROM Win32_ProcessStartTrace')
        $script:Watcher = [System.Management.ManagementEventWatcher]::new($query)
        $identifier = "colay-state-acl-$PID-$([guid]::NewGuid().ToString('N'))"
        $script:WatcherSubscription = Register-ObjectEvent `
            -InputObject $script:Watcher `
            -EventName EventArrived `
            -SourceIdentifier $identifier `
            -MessageData $script:ProcessEventQueue `
            -Action {
                $eventRecord = $Event.SourceEventArgs.NewEvent
                $Event.MessageData.Enqueue([pscustomobject]@{
                    process_id = [int]$eventRecord.ProcessID
                    parent_process_id = [int]$eventRecord.ParentProcessID
                    name = [string]$eventRecord.ProcessName
                    observed_at_utc = [datetime]::UtcNow.ToString('o')
                    source = 'Win32_ProcessStartTrace'
                    identity_verified = $false
                    ownership_promoted = $false
                })
            }
        $script:Watcher.Start()
        $script:ProcessObservationMode = 'defense-in-depth-win32-process-events-and-post-exit-snapshots'
    } catch {
        $script:WatcherFailure = $_.Exception.Message
        if ($null -ne $script:WatcherSubscription) {
            try { Unregister-Event -SourceIdentifier $script:WatcherSubscription.Name -ErrorAction SilentlyContinue } catch { }
            try { Remove-Job -Id $script:WatcherSubscription.Id -Force -ErrorAction SilentlyContinue } catch { }
            $script:WatcherSubscription = $null
        }
        if ($null -ne $script:Watcher) {
            try { $script:Watcher.Dispose() } catch { }
            $script:Watcher = $null
        }
    }
    Update-ProcessObservation
}

function Update-ProcessObservation {
    if ($script:ProcessObservationDelayForTestMs -gt 0) {
        Start-Sleep -Milliseconds $script:ProcessObservationDelayForTestMs
    }
    if ($script:ProcessObservationFailureForTest) {
        throw [System.TimeoutException]::new('injected bounded Win32_Process observation timeout')
    }
    $started = $null
    while ($script:ProcessEventQueue.TryDequeue([ref]$started)) {
        $script:ObservedStarts.Add([pscustomobject][ordered]@{
            process_id = [int]$started.process_id
            parent_process_id = [int]$started.parent_process_id
            name = [string]$started.name
            observed_at_utc = if ($started.PSObject.Properties.Name -contains 'observed_at_utc') {
                [string]$started.observed_at_utc
            } else {
                [datetime]::UtcNow.ToString('o')
            }
            source = if ($started.PSObject.Properties.Name -contains 'source') { [string]$started.source } else { 'process-start-event' }
            identity_verified = $false
            ownership_promoted = $false
            identity_key = $null
        })
        $started = $null
    }

    $snapshot = @(Get-CimInstance -ClassName Win32_Process `
        -Property ProcessId, ParentProcessId, CreationDate, Name, ExecutablePath `
        -OperationTimeoutSec $CimOperationTimeoutSec -ErrorAction Stop | ForEach-Object {
        $snapshotSource = $_
        $createdAt = $null
        $creationTimeObservationStatus = 'missing'
        $creationTimeObservationError = 'Win32_Process CreationDate was missing'
        try {
            if ($null -ne $snapshotSource.CreationDate) {
                $createdAt = ConvertTo-NormalizedProcessCreationUtc $snapshotSource.CreationDate
                $creationTimeObservationStatus = 'available'
                $creationTimeObservationError = $null
            }
        } catch {
            $creationTimeObservationStatus = 'conversion-error'
            $creationTimeObservationError = $_.Exception.Message
        }
        $executablePath = $null
        $executablePathObservationStatus = 'missing'
        $executablePathObservationError = 'Win32_Process ExecutablePath was missing or empty'
        try {
            if (-not [string]::IsNullOrWhiteSpace([string]$snapshotSource.ExecutablePath)) {
                $executablePath = ConvertTo-NormalizedExecutablePath ([string]$snapshotSource.ExecutablePath)
                $executablePathObservationStatus = 'available'
                $executablePathObservationError = $null
            }
        } catch {
            $executablePathObservationStatus = 'conversion-error'
            $executablePathObservationError = $_.Exception.Message
        }
        [pscustomobject]@{
            process_id = [int]$snapshotSource.ProcessId
            parent_process_id = [int]$snapshotSource.ParentProcessId
            name = [string]$snapshotSource.Name
            creation_time_utc = $createdAt
            creation_time_observation_status = $creationTimeObservationStatus
            creation_time_observation_error = $creationTimeObservationError
            executable_path = $executablePath
            executable_path_observation_status = $executablePathObservationStatus
            executable_path_observation_error = $executablePathObservationError
        }
    })
    $script:LastProcessSnapshot = $snapshot

    $ambiguousSnapshotPids = [System.Collections.Generic.HashSet[int]]::new()
    foreach ($duplicateGroup in @($snapshot | Where-Object process_id -GT 0 | Group-Object process_id | Where-Object Count -GT 1)) {
        $duplicateProcessId = [int]$duplicateGroup.Name
        [void]$ambiguousSnapshotPids.Add($duplicateProcessId)
        $script:ProcessOwnershipRefusals.Add([pscustomobject][ordered]@{
            process_id = $duplicateProcessId
            creation_time_utc = $null
            reason = 'duplicate process id generations in one Win32_Process snapshot; snapshot adoption disabled'
            observed_at_utc = [datetime]::UtcNow.ToString('o')
        })
    }
    $script:AmbiguousSnapshotPids = $ambiguousSnapshotPids
    $snapshotAdoptionAllowed = $ambiguousSnapshotPids.Count -eq 0

    foreach ($snapshotRow in $snapshot) {
        $snapshotProcessId = [int]$snapshotRow.process_id
        if ($snapshotProcessId -le 0 -or $snapshotProcessId -eq $PID -or
            $ambiguousSnapshotPids.Contains($snapshotProcessId)) {
            continue
        }
        $candidateIdentities = @($script:OwnedProcessIdentities | Where-Object {
            [int]$_.process_id -eq $snapshotProcessId -and $null -eq $_.exit_time_utc
        })
        if ($candidateIdentities.Count -eq 0) { continue }

        if ([string]$snapshotRow.creation_time_observation_status -ceq 'available') {
            $candidateIdentities = @($candidateIdentities | Where-Object {
                [datetime]$_.creation_time_utc -eq [datetime]$snapshotRow.creation_time_utc
            })
        }
        if ($candidateIdentities.Count -eq 0) { continue }

        if ([string]$snapshotRow.executable_path_observation_status -ceq 'available') {
            $observedPath = [string]$snapshotRow.executable_path
            $candidateIdentities = @($candidateIdentities | Where-Object {
                ([string]$_.executable_path).Equals($observedPath, [StringComparison]::OrdinalIgnoreCase)
            })
        }
        if ($candidateIdentities.Count -eq 0) { continue }
        if ([string]$snapshotRow.creation_time_observation_status -ceq 'available' -and
            [string]$snapshotRow.executable_path_observation_status -ceq 'available') {
            continue
        }

        $candidateIdentityKeys = @($candidateIdentities | ForEach-Object {
            [string]$_.identity_key
        } | Sort-Object)
        $observedCreationTime = if ($null -eq $snapshotRow.creation_time_utc) {
            $null
        } else {
            ([datetime]$snapshotRow.creation_time_utc).ToString('o')
        }
        $refusalKey = [pscustomobject][ordered]@{
            reason_code = 'owned-process-snapshot-identity-unverifiable'
            process_id = $snapshotProcessId
            observed_creation_time_utc = $observedCreationTime
            observed_executable_path = $snapshotRow.executable_path
            creation_time_observation_status = [string]$snapshotRow.creation_time_observation_status
            executable_path_observation_status = [string]$snapshotRow.executable_path_observation_status
            candidate_identity_keys = $candidateIdentityKeys
        } | ConvertTo-Json -Compress -Depth 4
        Add-DeduplicatedProcessOwnershipRefusal -RefusalKey $refusalKey -Evidence ([pscustomobject][ordered]@{
            refusal_key = $refusalKey
            process_id = $snapshotProcessId
            creation_time_utc = $observedCreationTime
            reason_code = 'owned-process-snapshot-identity-unverifiable'
            reason = 'active registered owned process identity could not be verified because Win32_Process identity fields were missing or unreadable'
            candidate_identity_keys = $candidateIdentityKeys
            expected_generations = @($candidateIdentities | ForEach-Object {
                [pscustomobject][ordered]@{
                    identity_key = [string]$_.identity_key
                    creation_time_utc = ([datetime]$_.creation_time_utc).ToString('o')
                    executable_path = [string]$_.executable_path
                }
            })
            observed_executable_path = $snapshotRow.executable_path
            creation_time_observation_status = [string]$snapshotRow.creation_time_observation_status
            creation_time_observation_error = $snapshotRow.creation_time_observation_error
            executable_path_observation_status = [string]$snapshotRow.executable_path_observation_status
            executable_path_observation_error = $snapshotRow.executable_path_observation_error
            observed_at_utc = [datetime]::UtcNow.ToString('o')
        })
    }

    $adoptedCount = 0
    do {
        $added = $false
        if (-not $snapshotAdoptionAllowed) { break }
        foreach ($candidate in $snapshot) {
            if ([int]$candidate.process_id -le 0 -or [int]$candidate.process_id -eq $PID -or
                $null -eq $candidate.creation_time_utc -or
                [string]::IsNullOrWhiteSpace([string]$candidate.executable_path)) {
                continue
            }
            $existing = @(Find-OwnedProcessIdentity -ProcessId ([int]$candidate.process_id) `
                -CreationTimeUtc ([datetime]$candidate.creation_time_utc))
            if ($existing.Count -ne 0) { continue }

            $eligibleParents = @($script:OwnedProcessIdentities | Where-Object {
                if ([int]$_.process_id -ne [int]$candidate.parent_process_id -or
                    [datetime]$candidate.creation_time_utc -lt [datetime]$_.creation_time_utc) {
                    return $false
                }
                $parentIdentity = $_
                $liveParent = @($snapshot | Where-Object {
                    Test-SnapshotMatchesOwnedIdentity -SnapshotRow $_ -Identity $parentIdentity
                }).Count -ne 0
                return $liveParent -or ($null -ne $_.exit_time_utc -and
                    [datetime]$candidate.creation_time_utc -le [datetime]$_.exit_time_utc)
            })
            if ($eligibleParents.Count -gt 1) {
                $script:ProcessOwnershipRefusals.Add([pscustomobject][ordered]@{
                    process_id = [int]$candidate.process_id
                    creation_time_utc = ([datetime]$candidate.creation_time_utc).ToString('o')
                    reason = 'ambiguous verified parent identity'
                    observed_at_utc = [datetime]::UtcNow.ToString('o')
                })
                continue
            }
            if ($eligibleParents.Count -eq 1) {
                $parent = $eligibleParents[0]
                if (@($parent.parent_chain) -contains (New-ProcessIdentityKey `
                        -ProcessId ([int]$candidate.process_id) `
                        -CreationTimeUtc ([datetime]$candidate.creation_time_utc))) {
                    $script:ProcessOwnershipRefusals.Add([pscustomobject][ordered]@{
                        process_id = [int]$candidate.process_id
                        creation_time_utc = ([datetime]$candidate.creation_time_utc).ToString('o')
                        reason = 'cyclic process identity parent chain'
                        observed_at_utc = [datetime]::UtcNow.ToString('o')
                    })
                    continue
                }
                $lineageRootKey = if ([int]$parent.depth -eq 1) {
                    [string]$parent.identity_key
                } else {
                    [string](@($parent.parent_chain)[1])
                }
                $lineageDescendantCount = @($script:OwnedProcessIdentities | Where-Object {
                    [int]$_.depth -gt 1 -and @($_.parent_chain).Count -gt 1 -and
                    [string](@($_.parent_chain)[1]) -ceq $lineageRootKey
                }).Count
                if ($adoptedCount -ge 32 -or $lineageDescendantCount -ge 32) {
                    $script:ProcessOwnershipRefusals.Add([pscustomobject][ordered]@{
                        process_id = [int]$candidate.process_id
                        creation_time_utc = ([datetime]$candidate.creation_time_utc).ToString('o')
                        reason = 'snapshot process ownership adoption exceeded the persistent 32-descendant lineage or per-snapshot safety cap'
                        observed_at_utc = [datetime]::UtcNow.ToString('o')
                    })
                    continue
                }
                $null = Register-OwnedProcessIdentity -ProcessId ([int]$candidate.process_id) `
                    -ParentProcessId ([int]$candidate.parent_process_id) `
                    -CreationTimeUtc ([datetime]$candidate.creation_time_utc) `
                    -ExecutablePath ([string]$candidate.executable_path) -Name ([string]$candidate.name) `
                    -Source 'verified-win32-process-snapshot' -ParentIdentity $parent -Label $null
                $added = $true
                $adoptedCount++
            }
        }
    } while ($added)

    $attributableSnapshot = @($snapshot | Where-Object {
        $snapshotRow = $_
        -not $ambiguousSnapshotPids.Contains([int]$snapshotRow.process_id) -and
        @($script:OwnedProcessIdentities | Where-Object {
            Test-SnapshotMatchesOwnedIdentity -SnapshotRow $snapshotRow -Identity $_
        }).Count -ne 0
    })
    $script:ProcessSnapshots.Add([pscustomobject]@{
        observed_at_utc = [datetime]::UtcNow.ToString('o')
        processes = @($attributableSnapshot | ForEach-Object {
            $snapshotRow = $_
            $identity = @($script:OwnedProcessIdentities | Where-Object {
                Test-SnapshotMatchesOwnedIdentity -SnapshotRow $snapshotRow -Identity $_
            } | Select-Object -First 1)[0]
            [pscustomobject][ordered]@{
                process_id = [int]$snapshotRow.process_id
                parent_process_id = [int]$snapshotRow.parent_process_id
                name = [string]$snapshotRow.name
                creation_time_utc = ([datetime]$snapshotRow.creation_time_utc).ToString('o')
                executable_path = [string]$snapshotRow.executable_path
                identity_key = [string]$identity.identity_key
                parent_identity_key = [string]$identity.parent_identity_key
                parent_chain = @($identity.parent_chain)
                identity_verified = $true
            }
        })
    })

    foreach ($candidate in $attributableSnapshot) {
        $identity = @($script:OwnedProcessIdentities | Where-Object {
            Test-SnapshotMatchesOwnedIdentity -SnapshotRow $candidate -Identity $_
        } | Select-Object -First 1)[0]
        if (-not ($script:ObservedStarts | Where-Object identity_key -CEQ $identity.identity_key)) {
            $script:ObservedStarts.Add([pscustomobject][ordered]@{
                process_id = [int]$candidate.process_id
                parent_process_id = [int]$candidate.parent_process_id
                name = [string]$candidate.name
                observed_at_utc = [datetime]::UtcNow.ToString('o')
                source = 'verified-win32-process-snapshot'
                identity_verified = $true
                ownership_promoted = $true
                identity_key = [string]$identity.identity_key
            })
        }
        if ($ForbiddenUtilityNames -contains ([string]$candidate.name).ToLowerInvariant()) {
            if (-not ($script:ForbiddenStarts | Where-Object identity_key -CEQ $identity.identity_key)) {
                $script:ForbiddenStarts.Add([pscustomobject][ordered]@{
                    process_id = [int]$candidate.process_id
                    parent_process_id = [int]$candidate.parent_process_id
                    name = [string]$candidate.name
                    creation_time_utc = ([datetime]$candidate.creation_time_utc).ToString('o')
                    executable_path = [string]$candidate.executable_path
                    identity_key = [string]$identity.identity_key
                    identity_verified = $true
                })
            }
        }
    }
}

function Stop-ProcessObservation {
    $failures = [System.Collections.Generic.List[string]]::new()
    if ($null -ne $script:Watcher) {
        try { $script:Watcher.Stop() } catch { $failures.Add("watcher stop: $($_.Exception.Message)") }
    }
    if ($null -ne $script:WatcherSubscription) {
        try {
            Unregister-Event -SourceIdentifier $script:WatcherSubscription.Name -ErrorAction Stop
        } catch {
            $failures.Add("event unregister: $($_.Exception.Message)")
        }
        try {
            Remove-Job -Id $script:WatcherSubscription.Id -Force -ErrorAction Stop
        } catch {
            $failures.Add("event job removal: $($_.Exception.Message)")
        }
        $script:WatcherSubscription = $null
    }
    if ($null -ne $script:Watcher) {
        try { $script:Watcher.Dispose() } catch { $failures.Add("watcher dispose: $($_.Exception.Message)") }
        $script:Watcher = $null
    }
    if ($failures.Count -ne 0) {
        throw "process observer teardown failed: $($failures -join '; ')"
    }
}

function New-IsolatedEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$ColayHomePath,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$FakeProvider,
        [Parameter(Mandatory = $true)][string]$InspectionMarker,
        [Parameter(Mandatory = $true)][string]$InspectionMarkerDirectory,
        [Parameter(Mandatory = $true)]
        [ValidateSet('LatencyAttributedOff', 'CorrectnessAttributedOn', IgnoreCase = $false)]
        [string]$MarkerPhase
    )
    $userHome = Join-Path $Root 'user-home'
    $temp = Join-Path $Root 'temp'
    $appData = Join-Path $userHome 'AppData/Roaming'
    $localAppData = Join-Path $userHome 'AppData/Local'
    foreach ($directory in @($ColayHomePath, $userHome, $temp, $appData, $localAppData)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $environment = [ordered]@{
        'COLAY_HOME' = $ColayHomePath
        'COLAY_TEST_FAKE_PROVIDERS_ONLY' = '1'
        'COLAY_TEST_LEGACY_INSPECT_MARKER' = $InspectionMarker
        'COLAY_TEST_DAEMON_STDERR' = (Join-Path $Root 'daemon-stderr.log')
        'COLAY_TEST_DAEMON_CHILD_RESOLUTION' = (Join-Path $Root 'daemon-child-resolution.log')
        'HOME' = $userHome
        'USERPROFILE' = $userHome
        'APPDATA' = $appData
        'LOCALAPPDATA' = $localAppData
        'TEMP' = $temp
        'TMP' = $temp
        'SystemRoot' = $env:SystemRoot
        'WINDIR' = $env:SystemRoot
        'OS' = 'Windows_NT'
        'PATH' = (Split-Path -Parent $FakeProvider)
        'PATHEXT' = '.EXE;.CMD'
        'RUST_BACKTRACE' = '1'
    }
    if ($MarkerPhase -ceq 'CorrectnessAttributedOn') {
        $environment['COLAY_TEST_LEGACY_INSPECT_MARKER_DIR'] = $InspectionMarkerDirectory
    }
    foreach ($key in $ProviderKeyNames) {
        [void]$environment.Remove($key)
    }
    return $environment
}

function Assert-HarnessDeadlineContract {
    param(
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000,
        [int]$RequestedExecutionTimeoutMs = 0
    )
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    if ($boundDeadlineParameterCount -notin @(0, $deadlineParameterNames.Count)) {
        throw 'bounded process deadline requires one atomic deadline contract'
    }
    if ($boundDeadlineParameterCount -eq 0) { return $false }
    if ($null -eq $OverallDeadlineStopwatch -or $OverallDeadlineMs -le 0 -or
        -not $OverallDeadlineStopwatch.IsRunning -or $ExitWaitLimitMs -lt 0 -or
        $OutputDrainLimitMs -lt 0 -or $RequestedExecutionTimeoutMs -le 0) {
        throw 'bounded process deadline contract received an invalid stopwatch or execution/cleanup limit'
    }
    if (($ExitWaitLimitMs + $OutputDrainLimitMs) -ge $OverallDeadlineMs) {
        throw 'bounded process cleanup limits leave no possible execution budget'
    }
    return $true
}

function Get-MonotonicElapsedCeilingMs {
    param([Parameter(Mandatory = $true)][System.Diagnostics.Stopwatch]$Stopwatch)
    return [int64][Math]::Ceiling($Stopwatch.Elapsed.TotalMilliseconds)
}

function Get-BoundedPhaseWaitMs {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.Stopwatch]$Stopwatch,
        [Parameter(Mandatory = $true)][int64]$OverallDeadlineMs,
        [Parameter(Mandatory = $true)][int64]$PhaseDeadlineElapsedMs,
        [Parameter(Mandatory = $true)][int]$MaximumWaitMs
    )
    $elapsedMs = Get-MonotonicElapsedCeilingMs -Stopwatch $Stopwatch
    $remainingMs = [Math]::Min($OverallDeadlineMs - $elapsedMs, $PhaseDeadlineElapsedMs - $elapsedMs)
    if ($remainingMs -le 0 -or $MaximumWaitMs -le 0) { return 0 }
    return [int][Math]::Min([int64]$MaximumWaitMs, $remainingMs)
}

function Start-HarnessProcess {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$ArgumentValues,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label,
        [AllowNull()][string]$StandardInputText,
        [switch]$CaptureFirstStdoutLine,
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000,
        [int]$RequestedExecutionTimeoutMs = 0,
        [switch]$DeferObservation
    )
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    if ($boundDeadlineParameterCount -notin @(0, $deadlineParameterNames.Count)) {
        throw 'process launch requires one atomic bounded deadline contract'
    }
    $deadlineAware = if ($boundDeadlineParameterCount -eq $deadlineParameterNames.Count) {
        Assert-HarnessDeadlineContract `
            -OverallDeadlineStopwatch $OverallDeadlineStopwatch -OverallDeadlineMs $OverallDeadlineMs `
            -ExitWaitLimitMs $ExitWaitLimitMs -OutputDrainLimitMs $OutputDrainLimitMs `
            -RequestedExecutionTimeoutMs $RequestedExecutionTimeoutMs
    } else {
        Assert-HarnessDeadlineContract -RequestedExecutionTimeoutMs $RequestedExecutionTimeoutMs
    }
    Assert-FreeDisk | Out-Null
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = -not $CaptureFirstStdoutLine
    $startInfo.RedirectStandardInput = $null -ne $StandardInputText
    $startInfo.Environment.Clear()
    foreach ($entry in $Environment.GetEnumerator()) {
        $startInfo.Environment[[string]$entry.Key] = [string]$entry.Value
    }
    foreach ($argument in $ArgumentValues) {
        $startInfo.ArgumentList.Add($argument)
    }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $launchRequestedAt = [datetime]::UtcNow
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $processStarted = $false
    $processId = $null
    $processStartedAt = $null
    $ownershipIdentity = $null
    $stdoutTask = $null
    $stderrTask = $null
    $stdinWriter = $null
    $setupStage = 'process-start'
    $deadlineLaunchElapsedMs = $null
    $deadlineRemainingAtLaunchMs = $null
    $deadlineExecutionTimeoutMs = $null
    $deadlineExecutionEndMs = $null
    $deadlineExitEndMs = $null
    $deadlineDrainEndMs = $null
    try {
        if ($deadlineAware) {
            $deadlineLaunchElapsedMs = Get-MonotonicElapsedCeilingMs -Stopwatch $OverallDeadlineStopwatch
            $deadlineRemainingAtLaunchMs = [int64]$OverallDeadlineMs - $deadlineLaunchElapsedMs
            $cleanupBudgetMs = [int64]$ExitWaitLimitMs + $OutputDrainLimitMs
            $availableExecutionMs = $deadlineRemainingAtLaunchMs - $cleanupBudgetMs
            $deadlineExecutionTimeoutMs = [int][Math]::Min(
                [int64]$RequestedExecutionTimeoutMs,
                $availableExecutionMs
            )
            if ($deadlineExecutionTimeoutMs -le 0) {
                throw "bounded process deadline had no execution budget at launch (remaining=${deadlineRemainingAtLaunchMs}ms, cleanup=${cleanupBudgetMs}ms)"
            }
            $deadlineExecutionEndMs = $deadlineLaunchElapsedMs + $deadlineExecutionTimeoutMs
            $deadlineExitEndMs = $deadlineExecutionEndMs + $ExitWaitLimitMs
            $deadlineDrainEndMs = $deadlineExitEndMs + $OutputDrainLimitMs
            if ($deadlineDrainEndMs -gt $OverallDeadlineMs -or
                ($deadlineExecutionTimeoutMs + $cleanupBudgetMs) -gt $deadlineRemainingAtLaunchMs) {
                throw 'bounded process launch budget exceeded the shared overall deadline'
            }
        }
        $startReturned = if ($script:ProcessSetupFailureForTest -ceq 'process-start-false') {
            $setupStage = 'process-start-false'
            $false
        } else {
            $process.Start()
        }
        if (-not $startReturned) { throw "failed to start $Label" }
        $processStarted = $true
        $processId = [int]$process.Id

        $setupStage = 'start-time-read'
        if ($script:ProcessSetupFailureForTest -ceq $setupStage) {
            throw [System.InvalidOperationException]::new('injected OS StartTime read failure')
        }
        $processStartedAt = $process.StartTime.ToUniversalTime()

        $setupStage = 'identity-registration'
        if ($null -eq $script:HarnessProcessIdentity) {
            throw 'harness process identity was not initialized before a child launch'
        }
        $ownershipIdentity = Register-OwnedProcessIdentity -ProcessId $processId `
            -ParentProcessId $PID -CreationTimeUtc $processStartedAt `
            -ExecutablePath (ConvertTo-NormalizedExecutablePath $Executable) `
            -Name ([System.IO.Path]::GetFileName($Executable)) -Source 'direct-process-start' `
            -ParentIdentity $script:HarnessProcessIdentity -Label $Label

        $setupStage = 'stdout-read-start'
        if ($script:ProcessSetupFailureForTest -ceq $setupStage) {
            throw [System.InvalidOperationException]::new('injected stdout reader setup failure')
        }
        $stdoutTask = if ($CaptureFirstStdoutLine) {
            $process.StandardOutput.ReadLineAsync()
        } else {
            $process.StandardOutput.ReadToEndAsync()
        }

        $setupStage = 'stderr-read-start'
        if ($script:ProcessSetupFailureForTest -ceq $setupStage) {
            throw [System.InvalidOperationException]::new('injected stderr reader setup failure')
        }
        $stderrTask = if ($startInfo.RedirectStandardError) {
            $process.StandardError.ReadToEndAsync()
        } else {
            $null
        }

        if ($null -ne $StandardInputText) {
            $stdinWriter = $process.StandardInput
            $setupStage = 'stdin-write'
            if ($script:ProcessSetupFailureForTest -ceq $setupStage) {
                throw [System.InvalidOperationException]::new('injected stdin write failure')
            }
            $stdinWriter.Write($StandardInputText)
            $setupStage = 'stdin-close'
            if ($script:ProcessSetupFailureForTest -ceq $setupStage) {
                throw [System.InvalidOperationException]::new('injected stdin close failure')
            }
            $stdinWriter.Close()
            $stdinWriter = $null
        }
        return [pscustomobject]@{
            Process = $process
            StdoutTask = $stdoutTask
            StderrTask = $stderrTask
            StandardInput = $null
            Stopwatch = $stopwatch
            LaunchRequestedAt = $launchRequestedAt
            ProcessStartedAt = $processStartedAt
            ProcessId = $processId
            OwnershipIdentity = $ownershipIdentity
            Label = $Label
            ExecutableName = [System.IO.Path]::GetFileName($Executable)
            ArgumentCount = $ArgumentValues.Count
            CaptureFirstStdoutLine = [bool]$CaptureFirstStdoutLine
            DeadlineAware = [bool]$deadlineAware
            OverallDeadlineStopwatch = $OverallDeadlineStopwatch
            OverallDeadlineMs = $OverallDeadlineMs
            RequestedExecutionTimeoutMs = $RequestedExecutionTimeoutMs
            DeadlineLaunchElapsedMs = $deadlineLaunchElapsedMs
            DeadlineRemainingAtLaunchMs = $deadlineRemainingAtLaunchMs
            DeadlineExecutionTimeoutMs = $deadlineExecutionTimeoutMs
            DeadlineExecutionEndMs = $deadlineExecutionEndMs
            DeadlineExitEndMs = $deadlineExitEndMs
            DeadlineDrainEndMs = $deadlineDrainEndMs
            ExitWaitLimitMs = $ExitWaitLimitMs
            OutputDrainLimitMs = $OutputDrainLimitMs
            DeadlineExitWaitConsumedMs = [int64]0
            DeadlineOutputDrainConsumedMs = [int64]0
            DeferObservation = [bool]$DeferObservation
        }
    } catch {
        $setupFailure = $_.Exception.Message
        $stopwatch.Stop()
        $stdoutTaskCreated = $null -ne $stdoutTask
        $stderrTaskCreated = $null -ne $stderrTask
        $stdinWriterCreated = $null -ne $stdinWriter
        $cleanup = $null
        if ($processStarted) {
            $partialRecord = [pscustomobject]@{
                Process = $process
                StdoutTask = $stdoutTask
                StderrTask = $stderrTask
                StandardInput = $stdinWriter
                ProcessStartedAt = $processStartedAt
                ProcessId = $processId
                OwnershipIdentity = $ownershipIdentity
                Label = $Label
                DeadlineAware = [bool]$deadlineAware
                OverallDeadlineStopwatch = $OverallDeadlineStopwatch
                OverallDeadlineMs = $OverallDeadlineMs
                RequestedExecutionTimeoutMs = $RequestedExecutionTimeoutMs
                DeadlineLaunchElapsedMs = $deadlineLaunchElapsedMs
                DeadlineRemainingAtLaunchMs = $deadlineRemainingAtLaunchMs
                DeadlineExecutionTimeoutMs = $deadlineExecutionTimeoutMs
                DeadlineExecutionEndMs = $deadlineExecutionEndMs
                DeadlineExitEndMs = $deadlineExitEndMs
                DeadlineDrainEndMs = $deadlineDrainEndMs
                ExitWaitLimitMs = $ExitWaitLimitMs
                OutputDrainLimitMs = $OutputDrainLimitMs
                DeadlineExitWaitConsumedMs = [int64]0
                DeadlineOutputDrainConsumedMs = [int64]0
                DeferObservation = [bool]$DeferObservation
            }
            $cleanupDeadlineArguments = @{}
            if ($deadlineAware) {
                $cleanupDeadlineArguments = @{
                    OverallDeadlineStopwatch = $OverallDeadlineStopwatch
                    OverallDeadlineMs = $OverallDeadlineMs
                    ExitWaitLimitMs = $ExitWaitLimitMs
                    OutputDrainLimitMs = $OutputDrainLimitMs
                }
            }
            $cleanup = Complete-FailedHarnessProcess -Record $partialRecord -FailureStage $setupStage `
                -Terminate -DeferObservation:$DeferObservation @cleanupDeadlineArguments
        } else {
            $cleanupErrors = [System.Collections.Generic.List[string]]::new()
            $disposed = $false
            try {
                $process.Dispose()
                $disposed = $true
            } catch {
                $cleanupErrors.Add("process dispose failed: $($_.Exception.Message)")
            }
            $cleanup = [pscustomobject][ordered]@{
                failure_stage = $setupStage
                terminate_requested = $false
                kill_tree_attempted = $false
                exit_confirmed = $null
                stdin_writer_present = $false
                stdin_closed = $true
                stdout_task_present = $false
                stderr_task_present = $false
                stdout_completed = $true
                stderr_completed = $true
                process_disposed = $disposed
                record_cleared = $true
                cleanup_errors = $cleanupErrors.ToArray()
            }
        }
        $evidence = [pscustomobject][ordered]@{
            label = $Label
            executable = [System.IO.Path]::GetFileName($Executable)
            failure_stage = $setupStage
            failure_message = $setupFailure
            child_started = $processStarted
            process_id = $processId
            identity_registered = $null -ne $ownershipIdentity
            process_identity = if ($null -eq $ownershipIdentity) { $null } else { Get-ProcessIdentityEvidence $ownershipIdentity }
            process_start_returned_false = -not $processStarted -and $script:ProcessSetupFailureForTest -ceq 'process-start-false'
            record_created = $false
            stdout_task_created = $stdoutTaskCreated
            stderr_task_created = $stderrTaskCreated
            stdin_writer_created = $stdinWriterCreated
            elapsed_wall_ms = [int64]$stopwatch.ElapsedMilliseconds
            cleanup = $cleanup
        }
        $script:ProcessSetupFailureEvidence.Add($evidence)
        $message = "$Label setup failed during ${setupStage}: $setupFailure"
        if (@($cleanup.cleanup_errors).Count -ne 0) {
            $message += "; cleanup: $($cleanup.cleanup_errors -join '; ')"
        }
        throw $message
    }
}

function ConvertTo-NormalizedProcessCreationUtc {
    param([Parameter(Mandatory = $true)]$Value)
    $createdAt = if ($Value -is [datetime]) {
        ([datetime]$Value).ToUniversalTime()
    } else {
        $text = [string]$Value
        if ($text -match '^\d{14}\.\d{6}[+-]\d{3}$') {
            [System.Management.ManagementDateTimeConverter]::ToDateTime($text).ToUniversalTime()
        } else {
            [datetime]::Parse($text, [Globalization.CultureInfo]::InvariantCulture,
                [Globalization.DateTimeStyles]::AssumeUniversal).ToUniversalTime()
        }
    }
    return [datetime]::new(
        $createdAt.Ticks - ($createdAt.Ticks % 10),
        [DateTimeKind]::Utc
    )
}

function Get-OwnedDescendantPlan {
    param(
        [Parameter(Mandatory = $true)][object[]]$Snapshot,
        [Parameter(Mandatory = $true)][int]$RootProcessId,
        [Parameter(Mandatory = $true)][datetime]$RootStartedAtUtc,
        [Parameter(Mandatory = $true)][datetime]$RootExitedAtUtc
    )
    $rowsByPid = @{}
    $refusedReason = $null
    foreach ($row in $Snapshot) {
        $processId = [int]$row.ProcessId
        if ($processId -le 0) { continue }
        $key = [string]$processId
        if ($rowsByPid.ContainsKey($key)) {
            $refusedReason = "duplicate process id $processId in ownership snapshot"
            break
        }
        try {
            $createdAt = ConvertTo-NormalizedProcessCreationUtc $row.CreationDate
        } catch {
            continue
        }
        $executablePath = $null
        try {
            if (-not [string]::IsNullOrWhiteSpace([string]$row.ExecutablePath)) {
                $executablePath = ConvertTo-NormalizedExecutablePath ([string]$row.ExecutablePath)
            }
        } catch { }
        $rowsByPid[$key] = [pscustomobject]@{
            process_id = $processId
            parent_process_id = [int]$row.ParentProcessId
            creation_time_utc = $createdAt
            identity_key = New-ProcessIdentityKey -ProcessId $processId -CreationTimeUtc $createdAt
            executable_path = $executablePath
            name = [string]$row.Name
        }
    }
    if ($null -eq $refusedReason -and $rowsByPid.ContainsKey([string]$RootProcessId)) {
        $refusedReason = "exited root process id $RootProcessId is live or reused"
    }
    $rootStart = ConvertTo-NormalizedProcessCreationUtc $RootStartedAtUtc
    $rootExit = ConvertTo-NormalizedProcessCreationUtc $RootExitedAtUtc
    $rootIdentityKey = New-ProcessIdentityKey -ProcessId $RootProcessId -CreationTimeUtc $rootStart
    if ($null -eq $refusedReason -and ($RootProcessId -le 0 -or $RootProcessId -eq $PID -or $rootExit -lt $rootStart)) {
        $refusedReason = 'invalid or unsafe exited-root identity'
    }
    $owned = @{}
    if ($null -eq $refusedReason) {
        $changed = $true
        while ($changed) {
            $changed = $false
            foreach ($row in $rowsByPid.Values) {
                $key = [string]$row.process_id
                if ($owned.ContainsKey($key)) { continue }
                $parentChain = $null
                $parentPidChain = $null
                $depth = 0
                if ($row.parent_process_id -eq $RootProcessId) {
                    if ($row.creation_time_utc -lt $rootStart -or $row.creation_time_utc -gt $rootExit) { continue }
                    if ([string]::IsNullOrWhiteSpace([string]$row.executable_path)) {
                        $refusedReason = "descendant $($row.process_id) executable path was unavailable"
                        break
                    }
                    $parentChain = @($rootIdentityKey, [string]$row.identity_key)
                    $parentPidChain = @($RootProcessId, [int]$row.process_id)
                    $depth = 1
                } else {
                    $parentKey = [string]$row.parent_process_id
                    if (-not $owned.ContainsKey($parentKey)) { continue }
                    $parent = $owned[$parentKey]
                    if ($row.creation_time_utc -lt $parent.creation_time_utc) { continue }
                    if ([string]::IsNullOrWhiteSpace([string]$row.executable_path)) {
                        $refusedReason = "descendant $($row.process_id) executable path was unavailable"
                        break
                    }
                    $parentChain = @($parent.parent_chain) + [string]$row.identity_key
                    $parentPidChain = @($parent.parent_pid_chain) + [int]$row.process_id
                    if ($parentChain.Count -ne (@($parent.parent_chain).Count + 1) -or
                        @($parentChain | Select-Object -Unique).Count -ne $parentChain.Count) {
                        $refusedReason = 'cyclic descendant parent chain'
                        break
                    }
                    $depth = [int]$parent.depth + 1
                }
                if ($row.process_id -eq $PID) {
                    $refusedReason = 'ownership plan reached the current harness process id'
                    break
                }
                $owned[$key] = [pscustomobject][ordered]@{
                    process_id = [int]$row.process_id
                    parent_process_id = [int]$row.parent_process_id
                    creation_time_utc = [datetime]$row.creation_time_utc
                    identity_key = [string]$row.identity_key
                    executable_path = [string]$row.executable_path
                    name = [string]$row.name
                    depth = $depth
                    parent_chain = $parentChain
                    parent_pid_chain = $parentPidChain
                }
                if ($owned.Count -gt 32) {
                    $refusedReason = 'descendant ownership plan exceeded the 32-process safety cap'
                    break
                }
                $changed = $true
            }
            if ($null -ne $refusedReason) { break }
        }
    }
    $candidates = @()
    if ($null -eq $refusedReason) {
        $candidates = @($owned.Values | Sort-Object -Property @{ Expression = 'depth'; Descending = $false },
            @{ Expression = 'process_id'; Descending = $false })
    }
    return [pscustomobject][ordered]@{
        refused_reason = $refusedReason
        candidate_count = $candidates.Count
        candidates = $candidates
    }
}

function Initialize-OwnedDescendantNativeApi {
    if ($null -ne ('ColayOwnedDescendantNativeApi' -as [type])) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class ColayOwnedDescendantNativeApi
{
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr OpenProcess(uint desiredAccess, bool inheritHandle, uint processId);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetProcessTimes(
        IntPtr processHandle,
        out long creationTime,
        out long exitTime,
        out long kernelTime,
        out long userTime);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool QueryFullProcessImageName(
        IntPtr processHandle,
        uint flags,
        StringBuilder executablePath,
        ref uint pathLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool TerminateProcess(IntPtr processHandle, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool CloseHandle(IntPtr handle);
}
'@
}

function Stop-BoundedOwnedDescendants {
    param(
        [Parameter(Mandatory = $true)][int]$RootProcessId,
        [Parameter(Mandatory = $true)][datetime]$RootStartedAtUtc,
        [Parameter(Mandatory = $true)][datetime]$RootExitedAtUtc
    )
    $wall = [Diagnostics.Stopwatch]::StartNew()
    $errors = [System.Collections.Generic.List[string]]::new()
    $opened = [System.Collections.Generic.List[object]]::new()
    $killed = [System.Collections.Generic.List[object]]::new()
    $evidence = [pscustomobject][ordered]@{
        attempted = $true
        root_process_id = $RootProcessId
        root_started_at_utc = $RootStartedAtUtc.ToUniversalTime().ToString('o')
        root_exited_at_utc = $RootExitedAtUtc.ToUniversalTime().ToString('o')
        query_count = 0
        operation_timeout_sec = 2
        snapshot_count = $null
        candidate_count = 0
        killed_descendants = @()
        refused_reason = $null
        wait_limit_ms = 2000
        opened_handle_count = 0
        closed_handle_count = 0
        close_error_count = 0
        handles_disposed = $false
        wall_ms = $null
        errors = @()
    }
    try {
        $snapshot = @(Get-CimInstance -ClassName Win32_Process `
            -Property ProcessId, ParentProcessId, CreationDate, Name, ExecutablePath `
            -OperationTimeoutSec 2 -ErrorAction Stop)
        $evidence.query_count = 1
        $evidence.snapshot_count = $snapshot.Count
        $plan = Get-OwnedDescendantPlan -Snapshot $snapshot -RootProcessId $RootProcessId `
            -RootStartedAtUtc $RootStartedAtUtc -RootExitedAtUtc $RootExitedAtUtc
        $evidence.refused_reason = $plan.refused_reason
        $evidence.candidate_count = $plan.candidate_count
        if ($null -ne $plan.refused_reason) { return $evidence }

        Initialize-OwnedDescendantNativeApi
        $processAccess = [uint32](0x0001 -bor 0x1000 -bor 0x100000)
        foreach ($candidate in $plan.candidates) {
            $handle = [ColayOwnedDescendantNativeApi]::OpenProcess(
                $processAccess, $false, [uint32]$candidate.process_id
            )
            if ($handle -eq [IntPtr]::Zero) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $evidence.refused_reason = "candidate $($candidate.process_id) handle preflight failed with Win32 error $errorCode"
                return $evidence
            }
            $openedEntry = [pscustomobject]@{ candidate = $candidate; handle = $handle; evidence = $null }
            $opened.Add($openedEntry)
            $evidence.opened_handle_count = $opened.Count
            $creationFileTime = [long]0
            $exitFileTime = [long]0
            $kernelFileTime = [long]0
            $userFileTime = [long]0
            if (-not [ColayOwnedDescendantNativeApi]::GetProcessTimes(
                    $handle,
                    [ref]$creationFileTime,
                    [ref]$exitFileTime,
                    [ref]$kernelFileTime,
                    [ref]$userFileTime)) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $evidence.refused_reason = "candidate $($candidate.process_id) creation-time preflight failed with Win32 error $errorCode"
                return $evidence
            }
            $liveStart = ConvertTo-NormalizedProcessCreationUtc ([datetime]::FromFileTimeUtc($creationFileTime))
            if ($liveStart -ne [datetime]$candidate.creation_time_utc) {
                $evidence.refused_reason = "candidate $($candidate.process_id) native-handle identity did not match the ownership snapshot"
                return $evidence
            }
            $pathBuffer = [Text.StringBuilder]::new(32768)
            $pathLength = [uint32]$pathBuffer.Capacity
            if (-not [ColayOwnedDescendantNativeApi]::QueryFullProcessImageName(
                    $handle, 0, $pathBuffer, [ref]$pathLength)) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $evidence.refused_reason = "candidate $($candidate.process_id) executable-path preflight failed with Win32 error $errorCode"
                return $evidence
            }
            $livePath = ConvertTo-NormalizedExecutablePath $pathBuffer.ToString()
            if (-not $livePath.Equals([string]$candidate.executable_path, [StringComparison]::OrdinalIgnoreCase)) {
                $evidence.refused_reason = "candidate $($candidate.process_id) native-handle path did not match the ownership snapshot"
                return $evidence
            }
        }

        $waitWall = [Diagnostics.Stopwatch]::StartNew()
        foreach ($entry in $opened) {
            $terminateCalled = $false
            $exitConfirmed = $false
            try {
                $waitResult = [ColayOwnedDescendantNativeApi]::WaitForSingleObject($entry.handle, 0)
                if ($waitResult -eq 0x00000102) {
                    if (-not [ColayOwnedDescendantNativeApi]::TerminateProcess($entry.handle, 1)) {
                        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                        throw "TerminateProcess failed with Win32 error $errorCode"
                    }
                    $terminateCalled = $true
                } elseif ($waitResult -eq [uint32]::MaxValue) {
                    $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                    throw "initial native process wait failed with Win32 error $errorCode"
                } elseif ($waitResult -ne 0) {
                    throw "initial native process wait returned 0x$($waitResult.ToString('x8'))"
                }
                $remainingMs = [math]::Max(0, 2000 - [int]$waitWall.ElapsedMilliseconds)
                $waitResult = [ColayOwnedDescendantNativeApi]::WaitForSingleObject(
                    $entry.handle, [uint32]$remainingMs
                )
                if ($waitResult -eq [uint32]::MaxValue) {
                    $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                    throw "bounded native process wait failed with Win32 error $errorCode"
                }
                $exitConfirmed = $waitResult -eq 0
                if (-not $exitConfirmed) {
                    $errors.Add("descendant $($entry.candidate.process_id) did not exit within the shared 2000ms limit")
                }
            } catch {
                $errors.Add("descendant $($entry.candidate.process_id) termination failed: $($_.Exception.Message)")
            }
            $killedRow = [pscustomobject][ordered]@{
                process_id = [int]$entry.candidate.process_id
                parent_process_id = [int]$entry.candidate.parent_process_id
                creation_time_utc = ([datetime]$entry.candidate.creation_time_utc).ToString('o')
                identity_key = [string]$entry.candidate.identity_key
                executable_path = [string]$entry.candidate.executable_path
                parent_chain = @($entry.candidate.parent_chain)
                parent_pid_chain = @($entry.candidate.parent_pid_chain)
                identity_verified = $true
                executable_path_verified = $true
                terminate_called = $terminateCalled
                exit_confirmed = $exitConfirmed
                handle_closed = $false
            }
            $entry.evidence = $killedRow
            $killed.Add($killedRow)
        }
    } catch {
        $errors.Add("descendant ownership sweep failed: $($_.Exception.Message)")
    } finally {
        foreach ($entry in $opened) {
            if (-not [ColayOwnedDescendantNativeApi]::CloseHandle($entry.handle)) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $errors.Add("descendant process handle close failed with Win32 error $errorCode")
                $evidence.close_error_count = [int]$evidence.close_error_count + 1
            } elseif ($null -ne $entry.evidence) {
                $entry.evidence.handle_closed = $true
                $evidence.closed_handle_count = [int]$evidence.closed_handle_count + 1
            } else {
                $evidence.closed_handle_count = [int]$evidence.closed_handle_count + 1
            }
        }
        $wall.Stop()
        $evidence.killed_descendants = $killed.ToArray()
        $evidence.handles_disposed = $evidence.opened_handle_count -eq $evidence.closed_handle_count -and
            $evidence.close_error_count -eq 0
        $evidence.wall_ms = [int64]$wall.ElapsedMilliseconds
        $evidence.errors = $errors.ToArray()
    }
    return $evidence
}

function Complete-FailedHarnessProcess {
    param(
        [Parameter(Mandatory = $true)]$Record,
        [Parameter(Mandatory = $true)][string]$FailureStage,
        [switch]$Terminate,
        [switch]$DeferObservation,
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000
    )
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    $recordDeadlineAware = $Record.PSObject.Properties.Name -contains 'DeadlineAware' -and
        [bool]$Record.DeadlineAware
    $recordDeferObservation = $Record.PSObject.Properties.Name -contains 'DeferObservation' -and
        [bool]$Record.DeferObservation
    $deadlineContractFailure = $null
    $deadlineAware = $false
    if ($recordDeadlineAware) {
        if ($boundDeadlineParameterCount -ne $deadlineParameterNames.Count) {
            $deadlineContractFailure = 'deadline-aware failure cleanup omitted or partially supplied its sealed deadline contract'
        } elseif (-not [object]::ReferenceEquals(
                $OverallDeadlineStopwatch,
                $Record.OverallDeadlineStopwatch
            ) -or $OverallDeadlineMs -ne [int]$Record.OverallDeadlineMs -or
            $ExitWaitLimitMs -ne [int]$Record.ExitWaitLimitMs -or
            $OutputDrainLimitMs -ne [int]$Record.OutputDrainLimitMs) {
            $deadlineContractFailure = 'failure cleanup did not receive the exact shared launch deadline contract'
        }
        $OverallDeadlineStopwatch = $Record.OverallDeadlineStopwatch
        $OverallDeadlineMs = [int]$Record.OverallDeadlineMs
        $ExitWaitLimitMs = [int]$Record.ExitWaitLimitMs
        $OutputDrainLimitMs = [int]$Record.OutputDrainLimitMs
        $deadlineAware = $true
        try {
            [void](Assert-HarnessDeadlineContract `
                -OverallDeadlineStopwatch $OverallDeadlineStopwatch -OverallDeadlineMs $OverallDeadlineMs `
                -ExitWaitLimitMs $ExitWaitLimitMs -OutputDrainLimitMs $OutputDrainLimitMs `
                -RequestedExecutionTimeoutMs ([int]$Record.RequestedExecutionTimeoutMs))
        } catch {
            if ($null -eq $deadlineContractFailure) {
                $deadlineContractFailure = "sealed failure cleanup deadline contract is invalid: $($_.Exception.Message)"
            }
        }
    } else {
        if ($boundDeadlineParameterCount -ne 0) {
            $deadlineContractFailure = 'non-deadline failure cleanup cannot add or explicitly downgrade a deadline after launch'
        }
        $OverallDeadlineStopwatch = $null
        $OverallDeadlineMs = 0
        $ExitWaitLimitMs = 5000
        $OutputDrainLimitMs = 2000
        $deadlineAware = $false
    }
    if ($PSBoundParameters.ContainsKey('DeferObservation') -and
        [bool]$DeferObservation -ne $recordDeferObservation) {
        $deadlineContractFailure = 'failure cleanup changed the sealed process-observation policy'
    }
    $DeferObservation = $recordDeferObservation
    if ($null -ne $deadlineContractFailure) {
        $Record.Stopwatch.Stop()
        $Terminate = $true
    }
    $cleanupErrors = [System.Collections.Generic.List[string]]::new()
    $cleanup = [pscustomobject][ordered]@{
        failure_stage = $FailureStage
        terminate_requested = [bool]$Terminate
        kill_tree_attempted = $false
        kill_tree_error = $null
        tree_kill_request_succeeded = $null
        single_process_fallback_attempted = $false
        single_process_fallback_succeeded = $false
        exit_confirmed = $false
        exit_code = $null
        process_exit_at_utc = $null
        descendant_sweep = $null
        deadline_aware = $deadlineAware
        overall_deadline_ms = if ($deadlineAware) { $OverallDeadlineMs } else { $null }
        exit_wait_limit_ms = $ExitWaitLimitMs
        exit_wait_applied_ms = $null
        exit_wait_consumed_ms = $null
        stdin_writer_present = $false
        stdin_closed = $true
        stdout_task_present = $false
        stderr_task_present = $false
        stdout_completed = $false
        stderr_completed = $false
        output_drain_limit_ms = $OutputDrainLimitMs
        output_drain_consumed_ms = $null
        output_drain_wall_ms = $null
        observer_deferred = [bool]$DeferObservation
        observer_wall_ms = $null
        process_disposed = $false
        record_cleared = $false
        total_wall_ms = $null
        cleanup_errors = @()
    }
    $totalWall = [System.Diagnostics.Stopwatch]::StartNew()
    $process = $Record.Process
    $stdoutTask = $Record.StdoutTask
    $stderrTask = $Record.StderrTask
    $stdinWriter = if ($Record.PSObject.Properties.Name -contains 'StandardInput') { $Record.StandardInput } else { $null }
    $confirmedExitAt = $null
    $cleanup.stdin_writer_present = $null -ne $stdinWriter
    $cleanup.stdin_closed = $null -eq $stdinWriter
    $cleanup.stdout_task_present = $null -ne $stdoutTask
    $cleanup.stderr_task_present = $null -ne $stderrTask
    try {
        if ($null -eq $process) {
            $cleanupErrors.Add('process record was already empty')
        } else {
            $exitAlreadyConfirmed = $false
            try {
                $exitAlreadyConfirmed = $process.WaitForExit(0)
            } catch {
                $cleanupErrors.Add("process exit-state query failed: $($_.Exception.Message)")
            }
            if ($Terminate -and -not $exitAlreadyConfirmed) {
                $cleanup.kill_tree_attempted = $true
                $cleanup.tree_kill_request_succeeded = $false
                try {
                    $process.Kill($true)
                    $cleanup.tree_kill_request_succeeded = $true
                } catch {
                    $treeKillError = $_.Exception.Message
                    $cleanup.kill_tree_error = $treeKillError
                    $cleanupErrors.Add("process tree termination failed: $treeKillError")
                    $cleanup.single_process_fallback_attempted = $true
                    try {
                        $process.Kill()
                        $cleanup.single_process_fallback_succeeded = $true
                    } catch {
                        $cleanupErrors.Add("direct child fallback termination failed: $($_.Exception.Message)")
                    }
                }
            }
            $exitWaitMs = $ExitWaitLimitMs
            if ($deadlineAware) {
                $exitWaitRemainingLimitMs = $ExitWaitLimitMs -
                    [int]$Record.DeadlineExitWaitConsumedMs
                $phaseDeadlineMs = [int64]$Record.DeadlineExitEndMs
                $exitWaitMs = Get-BoundedPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                    -OverallDeadlineMs $OverallDeadlineMs -PhaseDeadlineElapsedMs $phaseDeadlineMs `
                    -MaximumWaitMs $exitWaitRemainingLimitMs
            }
            $cleanup.exit_wait_applied_ms = $exitWaitMs
            $exitWaitWall = [System.Diagnostics.Stopwatch]::StartNew()
            try {
                $cleanup.exit_confirmed = $process.WaitForExit($exitWaitMs)
            } catch {
                $cleanupErrors.Add("bounded process exit wait failed: $($_.Exception.Message)")
            } finally {
                $exitWaitWall.Stop()
                if ($deadlineAware) {
                    $Record.DeadlineExitWaitConsumedMs = [int64]$Record.DeadlineExitWaitConsumedMs +
                        [int64][Math]::Ceiling($exitWaitWall.Elapsed.TotalMilliseconds)
                }
            }
            if (-not $cleanup.exit_confirmed) {
                $cleanupErrors.Add("process did not exit within the ${exitWaitMs}ms cleanup limit")
            } else {
                try { $cleanup.exit_code = [int]$process.ExitCode } catch { $cleanupErrors.Add("exit-code read failed: $($_.Exception.Message)") }
                try {
                    $confirmedExitAt = $process.ExitTime.ToUniversalTime()
                    $cleanup.process_exit_at_utc = $confirmedExitAt.ToString('o')
                    if ($Record.PSObject.Properties.Name -contains 'OwnershipIdentity') {
                        Set-OwnedProcessIdentityExit -Identity $Record.OwnershipIdentity -ExitTimeUtc $confirmedExitAt
                    }
                } catch {
                    $cleanupErrors.Add("cleanup ExitTime read failed: $($_.Exception.Message)")
                }
            }
        }

        if (-not $deadlineAware -and $Terminate -and $FailureStage -ceq 'output-drain' -and $cleanup.exit_confirmed -and
            $null -ne $confirmedExitAt -and $Record.PSObject.Properties.Name -contains 'ProcessId') {
            $cleanup.descendant_sweep = Stop-BoundedOwnedDescendants `
                -RootProcessId ([int]$Record.ProcessId) `
                -RootStartedAtUtc ([datetime]$Record.ProcessStartedAt) `
                -RootExitedAtUtc $confirmedExitAt
            foreach ($descendantError in @($cleanup.descendant_sweep.errors)) {
                $cleanupErrors.Add($descendantError)
            }
            if ($null -ne $cleanup.descendant_sweep.refused_reason) {
                $cleanupErrors.Add("descendant ownership sweep refused: $($cleanup.descendant_sweep.refused_reason)")
            }
        }

        if ($null -ne $stdinWriter) {
            try {
                $stdinWriter.Close()
                $cleanup.stdin_closed = $true
            } catch {
                $cleanupErrors.Add("redirected stdin close failed: $($_.Exception.Message)")
            }
        }

        $outputDrain = [System.Diagnostics.Stopwatch]::StartNew()
        $drainStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
        while (($null -ne $stdoutTask -and -not $stdoutTask.IsCompleted) -or
            ($null -ne $stderrTask -and -not $stderrTask.IsCompleted)) {
            $drainRemainingMs = if ($deadlineAware) {
                $phaseDeadlineMs = [int64]$Record.DeadlineDrainEndMs
                Get-BoundedPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                    -OverallDeadlineMs $OverallDeadlineMs -PhaseDeadlineElapsedMs $phaseDeadlineMs `
                    -MaximumWaitMs ($OutputDrainLimitMs - [int]$Record.DeadlineOutputDrainConsumedMs -
                        [int][Math]::Ceiling($drainStopwatch.Elapsed.TotalMilliseconds))
            } else {
                $OutputDrainLimitMs - [int][Math]::Ceiling($drainStopwatch.Elapsed.TotalMilliseconds)
            }
            if ($drainRemainingMs -le 0) { break }
            Start-Sleep -Milliseconds ([int][Math]::Min(10, $drainRemainingMs))
        }
        $drainStopwatch.Stop()
        if ($deadlineAware) {
            $Record.DeadlineOutputDrainConsumedMs = [int64]$Record.DeadlineOutputDrainConsumedMs +
                [int64][Math]::Ceiling($drainStopwatch.Elapsed.TotalMilliseconds)
        }
        $outputDrain.Stop()
        $cleanup.output_drain_wall_ms = [int64]$outputDrain.ElapsedMilliseconds
        $cleanup.stdout_completed = $null -eq $stdoutTask -or [bool]$stdoutTask.IsCompleted
        $cleanup.stderr_completed = $null -eq $stderrTask -or [bool]$stderrTask.IsCompleted
        if (-not $cleanup.stdout_completed -or -not $cleanup.stderr_completed) {
            $cleanupErrors.Add("redirected output did not drain (stdout=$($cleanup.stdout_completed), stderr=$($cleanup.stderr_completed))")
        }

        $observerWall = [System.Diagnostics.Stopwatch]::StartNew()
        if (-not $DeferObservation) {
            try {
                Update-ProcessObservation
            } catch {
                $cleanupErrors.Add("post-failure process observation failed: $($_.Exception.Message)")
            }
        }
        $observerWall.Stop()
        $cleanup.observer_wall_ms = [int64]$observerWall.ElapsedMilliseconds
    } catch {
        $cleanupErrors.Add("unhandled failure cleanup error: $($_.Exception.Message)")
    } finally {
        if ($null -ne $process) {
            try {
                $process.Dispose()
                $cleanup.process_disposed = $true
            } catch {
                $cleanupErrors.Add("process dispose failed: $($_.Exception.Message)")
            }
        }
        try {
            $Record.Process = $null
            $Record.StdoutTask = $null
            $Record.StderrTask = $null
            if ($Record.PSObject.Properties.Name -contains 'StandardInput') {
                $Record.StandardInput = $null
            }
            $cleanup.record_cleared = $true
        } catch {
            $cleanupErrors.Add("process record clearing failed: $($_.Exception.Message)")
        }
        $totalWall.Stop()
        $cleanup.total_wall_ms = [int64]$totalWall.ElapsedMilliseconds
        $cleanup.exit_wait_consumed_ms = if ($deadlineAware) {
            [int64]$Record.DeadlineExitWaitConsumedMs
        } else { $null }
        $cleanup.output_drain_consumed_ms = if ($deadlineAware) {
            [int64]$Record.DeadlineOutputDrainConsumedMs
        } else { $null }
        $cleanup.cleanup_errors = $cleanupErrors.ToArray()
    }
    if ($null -ne $deadlineContractFailure) {
        $exception = [System.InvalidOperationException]::new(
            "failure cleanup deadline contract violation: $deadlineContractFailure"
        )
        $exception.Data['ColayHarnessDeadlineContractCleanup'] = $cleanup
        throw $exception
    }
    return $cleanup
}

function Start-OwnedHarnessProcessBatch {
    param([Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$Requests)

    $owned = [System.Collections.Generic.List[object]]::new()
    $primaryFailure = $null
    $cleanupFailures = [System.Collections.Generic.List[string]]::new()
    $cleanupRows = [System.Collections.Generic.List[object]]::new()
    $batchCompleted = $false
    $requestIndex = 0
    try {
        foreach ($request in $Requests) {
            $requestIndex++
            $savedSetupFailure = $script:ProcessSetupFailureForTest
            try {
                if ($script:ProcessBatchSetupFailureIndexForTest -eq $requestIndex) {
                    $script:ProcessSetupFailureForTest = [string]$script:ProcessBatchSetupFailureStageForTest
                }
                $record = Start-HarnessProcess -Executable ([string]$request.executable) `
                    -ArgumentValues @($request.argument_values) `
                    -WorkingDirectory ([string]$request.working_directory) `
                    -Environment $request.environment -Label ([string]$request.label) `
                    -StandardInputText $request.standard_input_text `
                    -CaptureFirstStdoutLine:([bool]$request.capture_first_stdout_line) `
                    -DeferObservation:([bool]$request.defer_observation)
            } finally {
                $script:ProcessSetupFailureForTest = $savedSetupFailure
            }
            $owned.Add([pscustomobject]@{
                seed = $request.seed
                process = $record
            })
        }
        $batchCompleted = $true
    } catch {
        $primaryFailure = $_
    } finally {
        if (-not $batchCompleted) {
            foreach ($entry in $owned) {
                $record = $entry.process
                $processId = if ($null -ne $record.Process) { [int]$record.Process.Id } else { $null }
                $processStartedAt = $record.ProcessStartedAt
                $stdoutTask = $record.StdoutTask
                $stderrTask = $record.StderrTask
                $cleanup = Complete-FailedHarnessProcess -Record $record `
                    -FailureStage 'batch-start' -Terminate `
                    -DeferObservation:([bool]$record.DeferObservation)
                $identityStillRunning = $false
                $generationObservation = $null
                if ($null -ne $processId) {
                    $expectedExecutablePath = if ($record.PSObject.Properties.Name -contains 'OwnershipIdentity' -and
                        $null -ne $record.OwnershipIdentity) {
                        [string]$record.OwnershipIdentity.executable_path
                    } else { $null }
                    if ($null -eq $processStartedAt -or
                        [string]::IsNullOrWhiteSpace($expectedExecutablePath)) {
                        $identityStillRunning = $true
                        $cleanupFailures.Add(
                            "process ${processId}: batch rollback omitted its expected process identity"
                        )
                    } else {
                        $generationObservation = Get-ProcessGenerationObservation -ProcessId $processId `
                            -ExpectedCreationTimeUtc $processStartedAt `
                            -ExpectedExecutablePath $expectedExecutablePath
                        if ($generationObservation.process_exists -and
                            -not $generationObservation.identity_verified) {
                            $identityStillRunning = $true
                            $cleanupFailures.Add(
                                "process ${processId}: batch rollback could not verify process generation: $($generationObservation.observation_error)"
                            )
                        } else {
                            $identityStillRunning = [bool]$generationObservation.expected_generation_live
                        }
                    }
                }
                $cleanupRows.Add([pscustomobject][ordered]@{
                    process_id = $processId
                    identity_key = if ($record.PSObject.Properties.Name -contains 'OwnershipIdentity' -and
                        $null -ne $record.OwnershipIdentity) { [string]$record.OwnershipIdentity.identity_key } else { $null }
                    failure_stage = 'batch-start'
                    exit_confirmed = [bool]$cleanup.exit_confirmed
                    process_identity_running_after_cleanup = $identityStillRunning
                    process_generation_observation = $generationObservation
                    stdout_completed = $null -eq $stdoutTask -or [bool]$stdoutTask.IsCompleted
                    stderr_completed = $null -eq $stderrTask -or [bool]$stderrTask.IsCompleted
                    process_disposed = [bool]$cleanup.process_disposed
                    record_cleared = [bool]$cleanup.record_cleared
                    cleanup_errors = @($cleanup.cleanup_errors)
                })
                foreach ($cleanupError in @($cleanup.cleanup_errors)) {
                    $cleanupFailures.Add("process ${processId}: $cleanupError")
                }
            }
            try {
                Update-ProcessObservation
            } catch {
                $cleanupFailures.Add("post-batch process observation: $($_.Exception.Message)")
            }
            $script:ProcessBatchCleanupEvidence.Add([pscustomobject][ordered]@{
                requested_count = $Requests.Count
                failed_request_index = $requestIndex
                started_record_count = $owned.Count
                cleaned_record_count = $cleanupRows.Count
                records = $cleanupRows.ToArray()
                cleanup_errors = $cleanupFailures.ToArray()
            })
        }
    }
    if (-not $batchCompleted) {
        $message = "process batch start failed at request ${requestIndex}: $($primaryFailure.Exception.Message)"
        if ($cleanupFailures.Count -ne 0) {
            $message += "; cleanup: $($cleanupFailures -join '; ')"
        }
        throw $message
    }
    return $owned.ToArray()
}

function Wait-HarnessProcess {
    param(
        [Parameter(Mandatory = $true)]$Record,
        [Parameter(Mandatory = $true)][int]$TimeoutMs,
        [switch]$AllowFailure,
        [switch]$DeferObservation,
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000
    )
    $label = [string]$Record.Label
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    $recordDeadlineAware = $Record.PSObject.Properties.Name -contains 'DeadlineAware' -and
        [bool]$Record.DeadlineAware
    $recordDeferObservation = $Record.PSObject.Properties.Name -contains 'DeferObservation' -and
        [bool]$Record.DeferObservation
    $deadlineContractFailure = $null
    $deadlineAware = $false
    $sealedDeadlineArguments = @{}
    if ($recordDeadlineAware) {
        $sealedDeadlineArguments = @{
            OverallDeadlineStopwatch = $Record.OverallDeadlineStopwatch
            OverallDeadlineMs = [int]$Record.OverallDeadlineMs
            ExitWaitLimitMs = [int]$Record.ExitWaitLimitMs
            OutputDrainLimitMs = [int]$Record.OutputDrainLimitMs
        }
    }
    if ($recordDeadlineAware) {
        if ($boundDeadlineParameterCount -ne $deadlineParameterNames.Count) {
            $deadlineContractFailure = 'deadline-aware process wait omitted or partially supplied its sealed deadline contract'
        } elseif (-not [object]::ReferenceEquals(
                $OverallDeadlineStopwatch,
                $Record.OverallDeadlineStopwatch
            ) -or $OverallDeadlineMs -ne [int]$Record.OverallDeadlineMs -or
            $ExitWaitLimitMs -ne [int]$Record.ExitWaitLimitMs -or
            $OutputDrainLimitMs -ne [int]$Record.OutputDrainLimitMs -or
            $TimeoutMs -ne [int]$Record.RequestedExecutionTimeoutMs) {
            $deadlineContractFailure = 'process wait did not receive the exact shared launch deadline contract'
        }
        if ($null -eq $deadlineContractFailure) {
            try {
                $deadlineAware = Assert-HarnessDeadlineContract `
                    -OverallDeadlineStopwatch $OverallDeadlineStopwatch -OverallDeadlineMs $OverallDeadlineMs `
                    -ExitWaitLimitMs $ExitWaitLimitMs -OutputDrainLimitMs $OutputDrainLimitMs `
                    -RequestedExecutionTimeoutMs $TimeoutMs
            } catch {
                $deadlineContractFailure = $_.Exception.Message
            }
        }
    } else {
        if ($boundDeadlineParameterCount -ne 0) {
            $deadlineContractFailure = 'non-deadline process wait cannot add or explicitly downgrade a deadline after launch'
        }
    }
    if ($PSBoundParameters.ContainsKey('DeferObservation') -and
        [bool]$DeferObservation -ne $recordDeferObservation) {
        $deadlineContractFailure = 'process wait changed the sealed process-observation policy'
    }
    $DeferObservation = $recordDeferObservation
    if ($null -ne $deadlineContractFailure) {
        $Record.Stopwatch.Stop()
        $cleanup = Complete-FailedHarnessProcess -Record $Record -FailureStage 'deadline-contract' `
            -Terminate -DeferObservation:$recordDeferObservation @sealedDeadlineArguments
        $exception = [System.InvalidOperationException]::new(
            "process wait deadline contract violation: $deadlineContractFailure"
        )
        $exception.Data['ColayHarnessDeadlineContractCleanup'] = $cleanup
        throw $exception
    }
    $executableName = [string]$Record.ExecutableName
    $argumentCount = [int]$Record.ArgumentCount
    $launchRequestedAt = [datetime]$Record.LaunchRequestedAt
    $processStartedAt = [datetime]$Record.ProcessStartedAt
    $launchOverheadMs = [math]::Round(($processStartedAt - $launchRequestedAt).TotalMilliseconds, 3)
    $deadlineEvidence = if ($deadlineAware) {
        [pscustomobject][ordered]@{
            overall_timeout_ms = $OverallDeadlineMs
            launch_elapsed_ms = [int64]$Record.DeadlineLaunchElapsedMs
            remaining_at_launch_ms = [int64]$Record.DeadlineRemainingAtLaunchMs
            requested_command_timeout_ms = $TimeoutMs
            command_timeout_ms = [int]$Record.DeadlineExecutionTimeoutMs
            exit_wait_limit_ms = $ExitWaitLimitMs
            output_drain_limit_ms = $OutputDrainLimitMs
            total_operation_budget_ms = [int]$Record.DeadlineExecutionTimeoutMs +
                $ExitWaitLimitMs + $OutputDrainLimitMs
        }
    } else { $null }
    $processExited = $false
    $hardTimedOut = $false
    while (-not $processExited) {
        if ($deadlineAware) {
            $executionWaitMs = Get-BoundedPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                -OverallDeadlineMs $OverallDeadlineMs `
                -PhaseDeadlineElapsedMs ([int64]$Record.DeadlineExecutionEndMs) -MaximumWaitMs 10
            if ($executionWaitMs -le 0) {
                $hardTimedOut = $true
                break
            }
            $processExited = $Record.Process.WaitForExit($executionWaitMs)
            if (-not $processExited -and
                (Get-BoundedPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                    -OverallDeadlineMs $OverallDeadlineMs `
                    -PhaseDeadlineElapsedMs ([int64]$Record.DeadlineExecutionEndMs) -MaximumWaitMs 1) -le 0) {
                $hardTimedOut = $true
                break
            }
        } else {
            $processExited = $Record.Process.WaitForExit(10)
            if (-not $processExited -and $Record.Stopwatch.ElapsedMilliseconds -gt $TimeoutMs) {
                $hardTimedOut = $true
                break
            }
        }
    }
    if ($hardTimedOut) {
            $Record.Stopwatch.Stop()
            $timeoutElapsedMs = [int64]$Record.Stopwatch.ElapsedMilliseconds
            $failureCleanup = Complete-FailedHarnessProcess -Record $Record -FailureStage 'hard-timeout' `
                -Terminate -DeferObservation:$DeferObservation @sealedDeadlineArguments
            $timeoutResult = [pscustomobject]@{
                label = $label
                executable = $executableName
                process_id = [int]$Record.ProcessId
                argument_count = $argumentCount
                started_at_utc = $launchRequestedAt.ToString('o')
                process_started_at_utc = $processStartedAt.ToString('o')
                process_exit_at_utc = $failureCleanup.process_exit_at_utc
                measurement_method = 'monotonic-hard-timeout'
                elapsed_ms = $timeoutElapsedMs
                launch_overhead_ms = $launchOverheadMs
                exit_detection_wall_ms = $timeoutElapsedMs
                output_drain_wall_ms = $failureCleanup.output_drain_wall_ms
                post_exit_total_wall_ms = $null
                observer_wall_ms = $failureCleanup.observer_wall_ms
                observer_deferred = [bool]$DeferObservation
                exit_code = $failureCleanup.exit_code
                timed_out = $true
                stdout = $null
                stderr = $null
                timeout_kill_failure = $failureCleanup.kill_tree_error
                failure_stage = 'hard-timeout'
                failure_cleanup = $failureCleanup
                deadline = $deadlineEvidence
            }
            $script:CommandEvidence.Add($timeoutResult)
            $effectiveTimeoutMs = if ($deadlineAware) { [int]$Record.DeadlineExecutionTimeoutMs } else { $TimeoutMs }
            $message = "$label exceeded hard process timeout ${effectiveTimeoutMs}ms"
            if ($failureCleanup.cleanup_errors.Count -ne 0) {
                $message += "; failure cleanup: $($failureCleanup.cleanup_errors -join '; ')"
            }
            throw $message
    }
    try {
        if ($script:ProcessExitTimeFailureForTest) {
            throw [System.InvalidOperationException]::new('injected OS ExitTime read failure')
        }
        $processExitAt = $Record.Process.ExitTime.ToUniversalTime()
        Set-OwnedProcessIdentityExit -Identity $Record.OwnershipIdentity -ExitTimeUtc $processExitAt
    } catch {
        $exitTimeFailure = $_.Exception.Message
        $Record.Stopwatch.Stop()
        $exitDetectionWallMs = [int64]$Record.Stopwatch.ElapsedMilliseconds
        $failureCleanup = Complete-FailedHarnessProcess -Record $Record -FailureStage 'exit-time-read' `
            -Terminate -DeferObservation:$DeferObservation @sealedDeadlineArguments
        $failureResult = [pscustomobject]@{
            label = $label
            executable = $executableName
            argument_count = $argumentCount
            started_at_utc = $launchRequestedAt.ToString('o')
            process_started_at_utc = $processStartedAt.ToString('o')
            process_exit_at_utc = $failureCleanup.process_exit_at_utc
            measurement_method = 'unavailable-os-process-lifetime'
            elapsed_ms = $exitDetectionWallMs
            launch_overhead_ms = $launchOverheadMs
            exit_detection_wall_ms = $exitDetectionWallMs
            output_drain_wall_ms = $failureCleanup.output_drain_wall_ms
            post_exit_total_wall_ms = $null
            observer_wall_ms = $failureCleanup.observer_wall_ms
            observer_deferred = [bool]$DeferObservation
            exit_code = $failureCleanup.exit_code
            timed_out = $false
            stdout = $null
            stderr = $null
            timeout_kill_failure = $failureCleanup.kill_tree_error
            failure_stage = 'exit-time-read'
            failure_cleanup = $failureCleanup
            deadline = $deadlineEvidence
        }
        $script:CommandEvidence.Add($failureResult)
        $message = "failed to read the OS process exit timestamp for ${label}: $exitTimeFailure"
        if ($failureCleanup.cleanup_errors.Count -ne 0) {
            $message += "; failure cleanup: $($failureCleanup.cleanup_errors -join '; ')"
        }
        throw $message
    }
    $Record.Stopwatch.Stop()
    $exitDetectionWallMs = [int64]$Record.Stopwatch.ElapsedMilliseconds
    $rawLifetimeMs = ($processExitAt - $Record.ProcessStartedAt).TotalMilliseconds
    if ([double]::IsNaN($rawLifetimeMs) -or [double]::IsInfinity($rawLifetimeMs) -or $rawLifetimeMs -lt 0) {
        $failureCleanup = Complete-FailedHarnessProcess -Record $Record -FailureStage 'invalid-os-process-lifetime' `
            -Terminate -DeferObservation:$DeferObservation @sealedDeadlineArguments
        $failureResult = [pscustomobject]@{
            label = $label
            executable = $executableName
            argument_count = $argumentCount
            started_at_utc = $launchRequestedAt.ToString('o')
            process_started_at_utc = $processStartedAt.ToString('o')
            process_exit_at_utc = $failureCleanup.process_exit_at_utc
            measurement_method = 'invalid-os-process-lifetime'
            elapsed_ms = $exitDetectionWallMs
            launch_overhead_ms = $launchOverheadMs
            exit_detection_wall_ms = $exitDetectionWallMs
            output_drain_wall_ms = $failureCleanup.output_drain_wall_ms
            post_exit_total_wall_ms = $null
            observer_wall_ms = $failureCleanup.observer_wall_ms
            observer_deferred = [bool]$DeferObservation
            exit_code = $failureCleanup.exit_code
            timed_out = $false
            stdout = $null
            stderr = $null
            timeout_kill_failure = $failureCleanup.kill_tree_error
            failure_stage = 'invalid-os-process-lifetime'
            failure_cleanup = $failureCleanup
            deadline = $deadlineEvidence
        }
        $script:CommandEvidence.Add($failureResult)
        $message = "$label produced an invalid OS process lifetime: ${rawLifetimeMs}ms"
        if ($failureCleanup.cleanup_errors.Count -ne 0) {
            $message += "; failure cleanup: $($failureCleanup.cleanup_errors -join '; ')"
        }
        throw $message
    }
    $processLifetimeMs = [int64][math]::Ceiling($rawLifetimeMs)
    $postExitTotal = [System.Diagnostics.Stopwatch]::StartNew()
    $outputDrain = [System.Diagnostics.Stopwatch]::StartNew()
    $observerWall = $null
    $finalizationStage = 'post-exit-wait'
    try {
        if ($script:ProcessFinalizeFailureForTest -ceq $finalizationStage) {
            throw [System.InvalidOperationException]::new('injected post-exit WaitForExit failure')
        }
        $postExitWaitMs = if ($deadlineAware) {
            Get-BoundedPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                -OverallDeadlineMs $OverallDeadlineMs `
                -PhaseDeadlineElapsedMs ([int64]$Record.DeadlineExitEndMs) `
                -MaximumWaitMs ($ExitWaitLimitMs - [int]$Record.DeadlineExitWaitConsumedMs)
        } else { 5000 }
        $postExitWaitWall = [System.Diagnostics.Stopwatch]::StartNew()
        $postExitConfirmed = $Record.Process.WaitForExit($postExitWaitMs)
        $postExitWaitWall.Stop()
        if ($deadlineAware) {
            $Record.DeadlineExitWaitConsumedMs = [int64]$Record.DeadlineExitWaitConsumedMs +
                [int64][Math]::Ceiling($postExitWaitWall.Elapsed.TotalMilliseconds)
        }
        if (-not $postExitConfirmed) {
            throw [System.TimeoutException]::new("post-exit process confirmation exceeded ${postExitWaitMs}ms")
        }
        $finalizationStage = 'output-drain'
        $drainStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
        while (-not $Record.StdoutTask.IsCompleted -or
            ($null -ne $Record.StderrTask -and -not $Record.StderrTask.IsCompleted)) {
            $drainRemainingMs = if ($deadlineAware) {
                Get-BoundedPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                    -OverallDeadlineMs $OverallDeadlineMs `
                    -PhaseDeadlineElapsedMs ([int64]$Record.DeadlineDrainEndMs) `
                    -MaximumWaitMs ($OutputDrainLimitMs - [int]$Record.DeadlineOutputDrainConsumedMs -
                        [int][Math]::Ceiling($drainStopwatch.Elapsed.TotalMilliseconds))
            } else {
                2000 - [int][Math]::Ceiling($drainStopwatch.Elapsed.TotalMilliseconds)
            }
            if ($drainRemainingMs -le 0) { break }
            Start-Sleep -Milliseconds ([int][Math]::Min(10, $drainRemainingMs))
        }
        $drainStopwatch.Stop()
        if ($deadlineAware) {
            $Record.DeadlineOutputDrainConsumedMs = [int64]$Record.DeadlineOutputDrainConsumedMs +
                [int64][Math]::Ceiling($drainStopwatch.Elapsed.TotalMilliseconds)
        }
        if (-not $Record.StdoutTask.IsCompleted -or ($null -ne $Record.StderrTask -and -not $Record.StderrTask.IsCompleted)) {
            $stderrCompleted = $null -eq $Record.StderrTask -or $Record.StderrTask.IsCompleted
            throw [InvalidOperationException]::new(
                "$($Record.Label) left redirected output handles open after process exit (stdout_completed=$($Record.StdoutTask.IsCompleted), stderr_completed=$stderrCompleted)"
            )
        }
        $finalizationStage = 'stdout-read'
        if ($script:ProcessFinalizeFailureForTest -ceq $finalizationStage) {
            throw [System.InvalidOperationException]::new('injected stdout task read failure')
        }
        $stdout = if ($Record.StdoutTask.IsCompleted) { $Record.StdoutTask.GetAwaiter().GetResult() } else { $null }
        $finalizationStage = 'stderr-read'
        if ($script:ProcessFinalizeFailureForTest -ceq $finalizationStage) {
            throw [System.InvalidOperationException]::new('injected stderr task read failure')
        }
        $stderr = if ($null -eq $Record.StderrTask) {
            ''
        } elseif ($Record.StderrTask.IsCompleted) {
            $Record.StderrTask.GetAwaiter().GetResult()
        } else {
            $null
        }
        $outputDrain.Stop()
        $finalizationStage = 'exit-code-read'
        if ($script:ProcessFinalizeFailureForTest -ceq $finalizationStage) {
            throw [System.InvalidOperationException]::new('injected process ExitCode read failure')
        }
        $exitCode = [int]$Record.Process.ExitCode
        $observerFailure = $null
        $observerWall = [System.Diagnostics.Stopwatch]::StartNew()
        if (-not $DeferObservation) {
            try {
                Update-ProcessObservation
            } catch {
                $observerFailure = $_.Exception.Message
            }
        }
        $observerWall.Stop()
        $postExitTotal.Stop()
    } catch {
        $finalizationFailure = $_.Exception.Message
        if ($outputDrain.IsRunning) { $outputDrain.Stop() }
        if ($null -ne $observerWall -and $observerWall.IsRunning) { $observerWall.Stop() }
        if ($postExitTotal.IsRunning) { $postExitTotal.Stop() }
        $failureCleanup = Complete-FailedHarnessProcess -Record $Record -FailureStage $finalizationStage `
            -Terminate -DeferObservation:$DeferObservation @sealedDeadlineArguments
        $failureResult = [pscustomobject]@{
            label = $label
            executable = $executableName
            argument_count = $argumentCount
            started_at_utc = $launchRequestedAt.ToString('o')
            process_started_at_utc = $processStartedAt.ToString('o')
            process_exit_at_utc = $processExitAt.ToString('o')
            measurement_method = 'os-process-lifetime-finalization-failed'
            elapsed_ms = $processLifetimeMs
            launch_overhead_ms = $launchOverheadMs
            exit_detection_wall_ms = $exitDetectionWallMs
            output_drain_wall_ms = [int64]$outputDrain.ElapsedMilliseconds
            post_exit_total_wall_ms = $null
            observer_wall_ms = $failureCleanup.observer_wall_ms
            observer_deferred = [bool]$DeferObservation
            exit_code = $failureCleanup.exit_code
            timed_out = $false
            stdout = $null
            stderr = $null
            timeout_kill_failure = $failureCleanup.kill_tree_error
            failure_stage = $finalizationStage
            failure_cleanup = $failureCleanup
            deadline = $deadlineEvidence
        }
        $script:CommandEvidence.Add($failureResult)
        $message = "$label finalization failed during ${finalizationStage}: $finalizationFailure"
        if ($failureCleanup.cleanup_errors.Count -ne 0) {
            $message += "; failure cleanup: $($failureCleanup.cleanup_errors -join '; ')"
        }
        throw $message
    }
    $result = [pscustomobject]@{
        label = $label
        executable = $executableName
        argument_count = $argumentCount
        started_at_utc = $launchRequestedAt.ToString('o')
        process_started_at_utc = $processStartedAt.ToString('o')
        process_exit_at_utc = $processExitAt.ToString('o')
        measurement_method = 'os-process-lifetime'
        elapsed_ms = $processLifetimeMs
        launch_overhead_ms = $launchOverheadMs
        exit_detection_wall_ms = $exitDetectionWallMs
        output_drain_wall_ms = [int64]$outputDrain.ElapsedMilliseconds
        post_exit_total_wall_ms = [int64]$postExitTotal.ElapsedMilliseconds
        observer_wall_ms = [int64]$observerWall.ElapsedMilliseconds
        observer_deferred = [bool]$DeferObservation
        exit_code = $exitCode
        timed_out = $false
        stdout = $stdout
        stderr = $stderr
        timeout_kill_failure = $null
        failure_stage = $null
        failure_cleanup = $null
        deadline = $deadlineEvidence
    }
    $script:CommandEvidence.Add($result)
    $disposeFailure = $null
    try {
        $Record.Process.Dispose()
    } catch {
        $disposeFailure = $_.Exception.Message
    } finally {
        $Record.Process = $null
        $Record.StdoutTask = $null
        $Record.StderrTask = $null
    }
    $effectiveHardTimeoutMs = if ($deadlineAware) { [int]$Record.DeadlineExecutionTimeoutMs } else { $TimeoutMs }
    if ($processLifetimeMs -ge $effectiveHardTimeoutMs) {
        throw "$($result.label) OS process lifetime ${processLifetimeMs}ms reached or exceeded hard timeout ${effectiveHardTimeoutMs}ms"
    }
    if ($null -ne $observerFailure) {
        throw "$($result.label) post-exit process observation failed: $observerFailure"
    }
    if ($null -ne $disposeFailure) {
        throw "$($result.label) process dispose failed: $disposeFailure"
    }
    if (-not $AllowFailure -and $result.exit_code -ne 0) {
        throw "$($result.label) failed with exit code $($result.exit_code): $stderr"
    }
    return $result
}

function Invoke-HarnessProcess {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$ArgumentValues,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][int]$TimeoutMs,
        [AllowNull()][string]$StandardInputText,
        [switch]$CaptureFirstStdoutLine,
        [switch]$AllowFailure,
        [switch]$DeferObservation,
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000
    )
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    if ($boundDeadlineParameterCount -notin @(0, $deadlineParameterNames.Count)) {
        throw 'process invocation requires one atomic bounded deadline contract'
    }
    $deadlineArguments = @{}
    if ($boundDeadlineParameterCount -eq $deadlineParameterNames.Count) {
        $deadlineArguments = @{
            OverallDeadlineStopwatch = $OverallDeadlineStopwatch
            OverallDeadlineMs = $OverallDeadlineMs
            ExitWaitLimitMs = $ExitWaitLimitMs
            OutputDrainLimitMs = $OutputDrainLimitMs
        }
    }
    $record = Start-HarnessProcess -Executable $Executable -ArgumentValues $ArgumentValues `
        -WorkingDirectory $WorkingDirectory -Environment $Environment -Label $Label `
        -StandardInputText $StandardInputText -CaptureFirstStdoutLine:$CaptureFirstStdoutLine `
        -RequestedExecutionTimeoutMs $TimeoutMs -DeferObservation:$DeferObservation @deadlineArguments
    return Wait-HarnessProcess -Record $record -TimeoutMs $TimeoutMs -AllowFailure:$AllowFailure `
        -DeferObservation:$DeferObservation @deadlineArguments
}

function Invoke-ProcessLifetimeMeasurementSelfTest {
    param(
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$BaseEnvironment
    )
    foreach ($key in $ProviderKeyNames) {
        if ($BaseEnvironment.Contains($key)) {
            throw "timing self-test base environment contains provider credential key: $key"
        }
    }
    $portablePowerShell = Resolve-RequiredFile (Join-Path $PSHOME 'pwsh.exe') 'current portable PowerShell'
    $currentProcess = [System.Diagnostics.Process]::GetCurrentProcess()
    try {
        $currentPowerShell = [System.IO.Path]::GetFullPath($currentProcess.MainModule.FileName)
    } finally {
        $currentProcess.Dispose()
    }
    if (-not $portablePowerShell.Equals($currentPowerShell, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "timing self-test PowerShell is not the exact current host: expected $currentPowerShell, found $portablePowerShell"
    }
    $selfTestEnvironment = [ordered]@{}
    foreach ($name in @('SystemRoot', 'WINDIR', 'TEMP', 'TMP', 'HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA')) {
        if ($BaseEnvironment.Contains($name)) {
            $selfTestEnvironment[$name] = [string]$BaseEnvironment[$name]
        }
    }
    foreach ($key in $ProviderKeyNames) {
        if ($selfTestEnvironment.Contains($key)) {
            throw "timing self-test environment contains provider credential key: $key"
        }
    }
    $arguments = @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Milliseconds 100')
    $outerWall = [System.Diagnostics.Stopwatch]::StartNew()
    $record = $null
    $result = $null
    $expectedProcessLifetimeMs = $null
    $parentPreWaitDelayMs = 650
    $injectedObserverDelayMs = 950
    $ownershipStage = 'start'
    $primaryFailure = $null
    $failureCleanup = $null
    $script:TimingSelfTestFailureCleanupEvidence = $null
    try {
        $record = Start-HarnessProcess -Executable $portablePowerShell -ArgumentValues $arguments `
            -WorkingDirectory $WorkingDirectory -Environment $selfTestEnvironment -Label 'timing-self-test' `
            -StandardInputText $null
        $ownershipStage = 'direct-exit-wait'
        if (-not $record.Process.WaitForExit(5000)) {
            $failureCleanup = Complete-FailedHarnessProcess -Record $record `
                -FailureStage 'timing-self-test-exit-wait' -Terminate
            $script:TimingSelfTestFailureCleanupEvidence = $failureCleanup
            throw 'timing self-test child did not exit before the parent-delay probe'
        }
        $ownershipStage = 'direct-timestamp-validation'
        $directProcessStartedAt = $record.Process.StartTime.ToUniversalTime()
        $directProcessExitAt = $record.Process.ExitTime.ToUniversalTime()
        if ($directProcessStartedAt -ne $record.ProcessStartedAt) {
            throw 'timing self-test record start timestamp differs from direct OS Process.StartTime'
        }
        $expectedProcessLifetimeMs = [int64][math]::Ceiling(
            ($directProcessExitAt - $record.ProcessStartedAt).TotalMilliseconds
        )
        $ownershipStage = 'direct-lifetime-validation'
        if ($expectedProcessLifetimeMs -lt 50 -or $expectedProcessLifetimeMs -ge 10000) {
            throw "timing self-test independently computed an invalid OS lifetime: ${expectedProcessLifetimeMs}ms"
        }
        $ownershipStage = 'parent-delay'
        Start-Sleep -Milliseconds $parentPreWaitDelayMs
        $ownershipStage = 'wait-harness'
        $script:ProcessObservationDelayForTestMs = $injectedObserverDelayMs
        $result = Wait-HarnessProcess -Record $record -TimeoutMs 10000
        $record = $null
    } catch {
        $primaryFailure = $_
    } finally {
        $script:ProcessObservationDelayForTestMs = 0
        if ($null -ne $record -and $null -ne $record.Process) {
            $failureCleanup = Complete-FailedHarnessProcess -Record $record `
                -FailureStage "timing-self-test-$ownershipStage" -Terminate
            $script:TimingSelfTestFailureCleanupEvidence = $failureCleanup
        }
        $outerWall.Stop()
    }
    if ($null -ne $primaryFailure) {
        $message = $primaryFailure.Exception.Message
        if ($null -ne $failureCleanup -and @($failureCleanup.cleanup_errors).Count -ne 0) {
            $message += "; timing self-test cleanup: $($failureCleanup.cleanup_errors -join '; ')"
        }
        throw $message
    }
    $excludedTailMs = [int64]$outerWall.ElapsedMilliseconds - [int64]$result.elapsed_ms
    $exitDetectionTailMs = [int64]$result.exit_detection_wall_ms - [int64]$result.elapsed_ms
    if ($result.exit_code -ne 0 -or [string]$result.measurement_method -cne 'os-process-lifetime') {
        throw 'timing self-test child did not complete with OS process lifetime evidence'
    }
    if ([int64]$result.elapsed_ms -ne $expectedProcessLifetimeMs) {
        throw "timing self-test measurement differs from independent OS timestamps: expected=${expectedProcessLifetimeMs}ms, found=$($result.elapsed_ms)ms"
    }
    if ([int64]$result.observer_wall_ms -lt 900) {
        throw "timing self-test did not observe its injected delay: observer=$($result.observer_wall_ms)ms"
    }
    if ($exitDetectionTailMs -lt 550) {
        throw "timing self-test did not exclude the pre-wait parent delay: detection=$($result.exit_detection_wall_ms)ms, measured=$($result.elapsed_ms)ms, excluded=${exitDetectionTailMs}ms"
    }
    if ($excludedTailMs -lt 1400) {
        throw "timing self-test included observer tail in measured lifetime: outer=$($outerWall.ElapsedMilliseconds)ms, measured=$($result.elapsed_ms)ms, excluded=${excludedTailMs}ms"
    }
    return [pscustomobject][ordered]@{
        status = 'passed'
        executable = $portablePowerShell
        executable_sha256 = Get-Sha256 $portablePowerShell
        arguments = $arguments
        environment_names = @($selfTestEnvironment.Keys | Sort-Object)
        provider_credential_names_present = @()
        provider_invoked = $false
        parent_pre_wait_delay_ms = $parentPreWaitDelayMs
        injected_observer_delay_ms = $injectedObserverDelayMs
        expected_process_lifetime_ms = $expectedProcessLifetimeMs
        process_lifetime_ms = [int64]$result.elapsed_ms
        launch_overhead_ms = $result.launch_overhead_ms
        observer_wall_ms = [int64]$result.observer_wall_ms
        output_drain_wall_ms = [int64]$result.output_drain_wall_ms
        post_exit_total_wall_ms = [int64]$result.post_exit_total_wall_ms
        outer_wall_ms = [int64]$outerWall.ElapsedMilliseconds
        exit_detection_tail_excluded_ms = $exitDetectionTailMs
        observer_tail_excluded_ms = $excludedTailMs
    }
}

function Invoke-HarnessFailureCleanupSelfTest {
    param(
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$BaseEnvironment
    )
    $globalCleanupErrorCountBefore = $script:CleanupErrors.Count
    foreach ($key in $ProviderKeyNames) {
        if ($BaseEnvironment.Contains($key)) {
            throw "failure-cleanup self-test base environment contains provider credential key: $key"
        }
    }
    $portablePowerShell = Resolve-RequiredFile (Join-Path $PSHOME 'pwsh.exe') 'current portable PowerShell'
    $currentProcess = [System.Diagnostics.Process]::GetCurrentProcess()
    try {
        $currentPowerShell = [System.IO.Path]::GetFullPath($currentProcess.MainModule.FileName)
    } finally {
        $currentProcess.Dispose()
    }
    if (-not $portablePowerShell.Equals($currentPowerShell, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "failure-cleanup self-test PowerShell is not the exact current host: expected $currentPowerShell, found $portablePowerShell"
    }
    $selfTestEnvironment = [ordered]@{}
    foreach ($name in @('SystemRoot', 'WINDIR', 'TEMP', 'TMP', 'HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA')) {
        if ($BaseEnvironment.Contains($name)) {
            $selfTestEnvironment[$name] = [string]$BaseEnvironment[$name]
        }
    }
    foreach ($key in $ProviderKeyNames) {
        if ($selfTestEnvironment.Contains($key)) {
            throw "failure-cleanup self-test environment contains provider credential key: $key"
        }
    }
    $setupCases = @(
        [pscustomobject]@{ stage = 'process-start-false'; child_started = $false; stdin = $null },
        [pscustomobject]@{ stage = 'start-time-read'; child_started = $true; stdin = $null },
        [pscustomobject]@{ stage = 'stdout-read-start'; child_started = $true; stdin = $null },
        [pscustomobject]@{ stage = 'stderr-read-start'; child_started = $true; stdin = $null },
        [pscustomobject]@{ stage = 'stdin-write'; child_started = $true; stdin = 'setup-probe' },
        [pscustomobject]@{ stage = 'stdin-close'; child_started = $true; stdin = 'setup-probe' }
    )
    $setupCaseResults = [System.Collections.Generic.List[object]]::new()
    foreach ($setupCase in $setupCases) {
        $evidenceIndex = $script:ProcessSetupFailureEvidence.Count
        $identityIndex = $script:OwnedProcessIdentities.Count
        $failureMessage = $null
        try {
            $script:ProcessSetupFailureForTest = [string]$setupCase.stage
            Start-HarnessProcess -Executable $portablePowerShell `
                -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30') `
                -WorkingDirectory $WorkingDirectory -Environment $selfTestEnvironment `
                -Label "setup-failure-self-test-$($setupCase.stage)" `
                -StandardInputText $setupCase.stdin | Out-Null
        } catch {
            $failureMessage = $_.Exception.Message
        } finally {
            $script:ProcessSetupFailureForTest = $null
        }
        if ([string]::IsNullOrWhiteSpace($failureMessage)) {
            throw "setup-failure self-test $($setupCase.stage) did not fail at its injected stage"
        }
        if ($script:ProcessSetupFailureEvidence.Count -ne ($evidenceIndex + 1)) {
            throw "setup-failure self-test $($setupCase.stage) did not add exactly one evidence row"
        }
        $setupEvidence = $script:ProcessSetupFailureEvidence[$evidenceIndex]
        if ([string]$setupEvidence.failure_stage -cne [string]$setupCase.stage -or
            [bool]$setupEvidence.child_started -ne [bool]$setupCase.child_started -or
            $setupEvidence.record_created -ne $false) {
            throw "setup-failure self-test $($setupCase.stage) evidence did not match the injected stage"
        }
        $expectedIdentityDelta = if ([bool]$setupCase.child_started -and
            [string]$setupCase.stage -cne 'start-time-read') { 1 } else { 0 }
        if ($script:OwnedProcessIdentities.Count -ne ($identityIndex + $expectedIdentityDelta) -or
            [bool]$setupEvidence.identity_registered -ne ($expectedIdentityDelta -eq 1)) {
            throw "setup-failure self-test $($setupCase.stage) registered an unexpected process identity"
        }
        if ($expectedIdentityDelta -eq 1 -and ($null -eq $setupEvidence.process_identity -or
                [string]::IsNullOrWhiteSpace([string]$setupEvidence.process_identity.creation_time_utc) -or
                [string]::IsNullOrWhiteSpace([string]$setupEvidence.process_identity.exit_time_utc))) {
            throw "setup-failure self-test $($setupCase.stage) omitted its normalized creation/exit identity"
        }
        $setupLivenessObservation = $null
        if ([bool]$setupCase.child_started) {
            if ($null -eq $setupEvidence.process_id) {
                throw "setup-failure self-test $($setupCase.stage) omitted its started process id"
            }
            $setupLivenessObservation = Get-ProcessLivenessObservation `
                -ProcessId ([int]$setupEvidence.process_id)
            if ($setupLivenessObservation.process_exists) {
                throw "setup-failure self-test $($setupCase.stage) left its started process alive"
            }
            if (-not $setupEvidence.cleanup.exit_confirmed -or
                -not $setupEvidence.cleanup.process_disposed -or
                -not $setupEvidence.cleanup.record_cleared -or
                -not $setupEvidence.cleanup.stdin_closed -or
                -not $setupEvidence.cleanup.stdout_completed -or
                -not $setupEvidence.cleanup.stderr_completed) {
                throw "setup-failure self-test $($setupCase.stage) did not complete started-child cleanup"
            }
        } elseif ($null -ne $setupEvidence.process_id -or
            -not $setupEvidence.process_start_returned_false -or
            -not $setupEvidence.cleanup.process_disposed) {
            throw 'process-start-false self-test created a child or did not dispose its unstarted Process object'
        }
        if (@($setupEvidence.cleanup.cleanup_errors).Count -ne 0) {
            throw "setup-failure self-test $($setupCase.stage) cleanup errors: $($setupEvidence.cleanup.cleanup_errors -join '; ')"
        }
        $setupCaseResults.Add([pscustomobject][ordered]@{
            failure_stage = [string]$setupCase.stage
            child_started = [bool]$setupEvidence.child_started
            process_id = $setupEvidence.process_id
            process_residue_count = 0
            record_created = [bool]$setupEvidence.record_created
            stdout_task_created = [bool]$setupEvidence.stdout_task_created
            stderr_task_created = [bool]$setupEvidence.stderr_task_created
            stdin_writer_created = [bool]$setupEvidence.stdin_writer_created
            stdin_closed = [bool]$setupEvidence.cleanup.stdin_closed
            process_disposed = [bool]$setupEvidence.cleanup.process_disposed
            cleanup_error_count = @($setupEvidence.cleanup.cleanup_errors).Count
            process_liveness_observation = $setupLivenessObservation
        })
    }
    $batchEvidenceIndex = $script:ProcessBatchCleanupEvidence.Count
    $batchSetupEvidenceIndex = $script:ProcessSetupFailureEvidence.Count
    $batchRequests = [System.Collections.Generic.List[object]]::new()
    foreach ($index in 1..4) {
        $batchRequests.Add([pscustomobject]@{
            seed = $index
            executable = $portablePowerShell
            argument_values = @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30')
            working_directory = $WorkingDirectory
            environment = $selfTestEnvironment
            label = "batch-start-ownership-self-test-$index"
            standard_input_text = $null
            capture_first_stdout_line = $false
            defer_observation = $true
        })
    }
    $batchFailureMessage = $null
    try {
        $script:ProcessBatchSetupFailureIndexForTest = 2
        $script:ProcessBatchSetupFailureStageForTest = 'start-time-read'
        [void](Start-OwnedHarnessProcessBatch -Requests $batchRequests.ToArray())
    } catch {
        $batchFailureMessage = $_.Exception.Message
    } finally {
        $script:ProcessBatchSetupFailureIndexForTest = 0
        $script:ProcessBatchSetupFailureStageForTest = $null
    }
    if ([string]::IsNullOrWhiteSpace($batchFailureMessage) -or
        $script:ProcessBatchCleanupEvidence.Count -ne ($batchEvidenceIndex + 1) -or
        $script:ProcessSetupFailureEvidence.Count -ne ($batchSetupEvidenceIndex + 1)) {
        throw 'batch-start ownership self-test did not produce one second-request setup failure and cleanup row'
    }
    $batchEvidence = $script:ProcessBatchCleanupEvidence[$batchEvidenceIndex]
    $batchSetupEvidence = $script:ProcessSetupFailureEvidence[$batchSetupEvidenceIndex]
    $batchRecord = $batchEvidence.records[0]
    $batchSetupLivenessObservation = if ($null -eq $batchSetupEvidence.process_id) {
        $null
    } else {
        Get-ProcessLivenessObservation -ProcessId ([int]$batchSetupEvidence.process_id)
    }
    if ($batchEvidence.requested_count -ne 4 -or $batchEvidence.failed_request_index -ne 2 -or
        $batchEvidence.started_record_count -ne 1 -or $batchEvidence.cleaned_record_count -ne 1 -or
        @($batchEvidence.cleanup_errors).Count -ne 0 -or
        $batchRecord.process_identity_running_after_cleanup -or -not $batchRecord.exit_confirmed -or
        -not $batchRecord.stdout_completed -or -not $batchRecord.stderr_completed -or
        -not $batchRecord.process_disposed -or -not $batchRecord.record_cleared -or
        @($batchRecord.cleanup_errors).Count -ne 0 -or
        [string]$batchSetupEvidence.failure_stage -cne 'start-time-read' -or
        -not $batchSetupEvidence.cleanup.record_cleared -or
        -not $batchSetupEvidence.cleanup.stdout_completed -or
        -not $batchSetupEvidence.cleanup.stderr_completed -or
        @($batchSetupEvidence.cleanup.cleanup_errors).Count -ne 0 -or
        $null -eq $batchSetupLivenessObservation -or
        $batchSetupLivenessObservation.process_exists) {
        throw "batch-start ownership self-test cleanup evidence was incomplete: $($batchEvidence | ConvertTo-Json -Compress -Depth 8)"
    }
    $batchStartOwnership = [pscustomobject][ordered]@{
        status = 'passed'
        requested_count = 4
        failed_request_index = 2
        started_record_count = 1
        cleaned_record_count = 1
        process_residue_count = 0
        record_residue_count = 0
        incomplete_pipe_task_count = 0
        cleanup_error_count = 0
        setup_failure_process_liveness = $batchSetupLivenessObservation
    }
    $descendantPipeCommand = @'
$descendantInfo = [System.Diagnostics.ProcessStartInfo]::new()
$descendantInfo.FileName = Join-Path $PSHOME 'pwsh.exe'
$descendantInfo.UseShellExecute = $false
$descendantInfo.CreateNoWindow = $true
foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30')) {
    $descendantInfo.ArgumentList.Add($argument)
}
$descendant = [System.Diagnostics.Process]::Start($descendantInfo)
$descendantStartedAt = $descendant.StartTime.ToUniversalTime()
Write-Output "$($descendant.Id)|$($descendantStartedAt.ToFileTimeUtc())"
[Console]::Out.Flush()
Start-Sleep -Seconds 30
'@
    $exitedRootDescendantPipeCommand = @'
$descendantInfo = [System.Diagnostics.ProcessStartInfo]::new()
$descendantInfo.FileName = Join-Path $PSHOME 'pwsh.exe'
$descendantInfo.UseShellExecute = $false
$descendantInfo.CreateNoWindow = $true
foreach ($argument in @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30')) {
    $descendantInfo.ArgumentList.Add($argument)
}
$descendant = [System.Diagnostics.Process]::Start($descendantInfo)
$descendantStartedAt = $descendant.StartTime.ToUniversalTime()
Write-Output "$($descendant.Id)|$($descendantStartedAt.ToFileTimeUtc())"
[Console]::Out.Flush()
$descendant.Dispose()
exit 0
'@
    $cases = @(
        [pscustomobject]@{
            name = 'hard-timeout'
            child_command = 'Start-Sleep -Seconds 30'
            timeout_ms = 100
            expected_stage = 'hard-timeout'
            exit_time_failure = $false
            finalization_failure = $null
            expect_descendant_pid = $false
            observer_failure = $false
            expected_cleanup_error_count = 0
        },
        [pscustomobject]@{
            name = 'descendant-pipe-hard-timeout'
            child_command = $descendantPipeCommand
            timeout_ms = 3000
            expected_stage = 'hard-timeout'
            exit_time_failure = $false
            finalization_failure = $null
            expect_descendant_pid = $true
            observer_failure = $false
            expected_cleanup_error_count = 0
        },
        [pscustomobject]@{
            name = 'descendant-pipe-output-drain'
            child_command = $exitedRootDescendantPipeCommand
            timeout_ms = 5000
            expected_stage = 'output-drain'
            exit_time_failure = $false
            finalization_failure = $null
            expect_descendant_pid = $true
            observer_failure = $false
            expected_cleanup_error_count = 0
        },
        [pscustomobject]@{
            name = 'exit-time-read'
            child_command = 'exit 0'
            timeout_ms = 5000
            expected_stage = 'exit-time-read'
            exit_time_failure = $true
            finalization_failure = $null
            expect_descendant_pid = $false
            observer_failure = $false
            expected_cleanup_error_count = 0
        },
        [pscustomobject]@{
            name = 'observer-timeout'
            child_command = 'exit 0'
            timeout_ms = 5000
            expected_stage = 'exit-time-read'
            exit_time_failure = $true
            finalization_failure = $null
            expect_descendant_pid = $false
            observer_failure = $true
            expected_cleanup_error_count = 1
        },
        [pscustomobject]@{
            name = 'post-exit-wait'
            child_command = 'exit 0'
            timeout_ms = 5000
            expected_stage = 'post-exit-wait'
            exit_time_failure = $false
            finalization_failure = 'post-exit-wait'
            expect_descendant_pid = $false
            observer_failure = $false
            expected_cleanup_error_count = 0
        },
        [pscustomobject]@{
            name = 'stdout-read'
            child_command = 'Write-Output cleanup-self-test'
            timeout_ms = 5000
            expected_stage = 'stdout-read'
            exit_time_failure = $false
            finalization_failure = 'stdout-read'
            expect_descendant_pid = $false
            observer_failure = $false
            expected_cleanup_error_count = 0
        },
        [pscustomobject]@{
            name = 'exit-code-read'
            child_command = 'exit 0'
            timeout_ms = 5000
            expected_stage = 'exit-code-read'
            exit_time_failure = $false
            finalization_failure = 'exit-code-read'
            expect_descendant_pid = $false
            observer_failure = $false
            expected_cleanup_error_count = 0
        }
    )
    $caseResults = [System.Collections.Generic.List[object]]::new()
    try {
        foreach ($case in $cases) {
            $arguments = @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', [string]$case.child_command)
            $record = Start-HarnessProcess -Executable $portablePowerShell -ArgumentValues $arguments `
                -WorkingDirectory $WorkingDirectory -Environment $selfTestEnvironment `
                -Label "failure-cleanup-self-test-$($case.name)" -StandardInputText $null
            $processId = [int]$record.Process.Id
            $directResidueObservation = $null
            $descendantResidueObservation = $null
            $stdoutTask = $record.StdoutTask
            $stderrTask = $record.StderrTask
            $evidenceIndex = $script:CommandEvidence.Count
            $failureMessage = $null
            $caseWall = [System.Diagnostics.Stopwatch]::StartNew()
            try {
                $script:ProcessExitTimeFailureForTest = [bool]$case.exit_time_failure
                $script:ProcessFinalizeFailureForTest = $case.finalization_failure
                $script:ProcessObservationFailureForTest = [bool]$case.observer_failure
                Wait-HarnessProcess -Record $record -TimeoutMs ([int]$case.timeout_ms) | Out-Null
            } catch {
                $failureMessage = $_.Exception.Message
            } finally {
                $caseWall.Stop()
                $script:ProcessExitTimeFailureForTest = $false
                $script:ProcessFinalizeFailureForTest = $null
                $script:ProcessObservationFailureForTest = $false
            }
            if ([string]::IsNullOrWhiteSpace($failureMessage)) {
                throw "failure-cleanup self-test $($case.name) did not fail at its injected stage"
            }
            if ($null -ne $record.Process -or $null -ne $record.StdoutTask -or $null -ne $record.StderrTask) {
                if ($null -ne $record.Process) {
                    [void](Complete-FailedHarnessProcess -Record $record -FailureStage 'self-test-emergency-cleanup' -Terminate)
                }
                throw "failure-cleanup self-test $($case.name) retained a process or task record"
            }
            if (-not $stdoutTask.IsCompleted -or ($null -ne $stderrTask -and -not $stderrTask.IsCompleted)) {
                throw "failure-cleanup self-test $($case.name) retained an incomplete redirected pipe task"
            }
            $directResidueObservation = Get-ProcessGenerationObservation -ProcessId $processId `
                -ExpectedCreationTimeUtc ([datetime]$record.OwnershipIdentity.creation_time_utc) `
                -ExpectedExecutablePath ([string]$record.OwnershipIdentity.executable_path)
            if ($directResidueObservation.process_exists -and -not $directResidueObservation.identity_verified) {
                throw "failure-cleanup self-test $($case.name) could not verify process $processId generation: $($directResidueObservation.observation_error)"
            }
            if ($directResidueObservation.expected_generation_live) {
                throw "failure-cleanup self-test $($case.name) left process $processId alive"
            }
            $descendantProcessId = $null
            if ([bool]$case.expect_descendant_pid) {
                $descendantOutput = $stdoutTask.GetAwaiter().GetResult().Trim()
                $parsedDescendantProcessId = 0
                $parsedDescendantCreationFileTime = [long]0
                $descendantIdentityParts = @($descendantOutput -split '\|', 2)
                if ($descendantIdentityParts.Count -ne 2 -or
                    -not [int]::TryParse($descendantIdentityParts[0], [ref]$parsedDescendantProcessId) -or
                    -not [long]::TryParse($descendantIdentityParts[1], [ref]$parsedDescendantCreationFileTime) -or
                    $parsedDescendantProcessId -le 0 -or $parsedDescendantCreationFileTime -le 0) {
                    throw "failure-cleanup self-test $($case.name) did not emit one exact descendant process identity: $descendantOutput"
                }
                $descendantProcessId = $parsedDescendantProcessId
                $descendantResidueObservation = Get-ProcessGenerationObservation `
                    -ProcessId $descendantProcessId `
                    -ExpectedCreationTimeUtc ([datetime]::FromFileTimeUtc($parsedDescendantCreationFileTime)) `
                    -ExpectedExecutablePath $portablePowerShell
                if ($descendantResidueObservation.process_exists -and
                    -not $descendantResidueObservation.identity_verified) {
                    throw "failure-cleanup self-test $($case.name) could not verify descendant $descendantProcessId generation: $($descendantResidueObservation.observation_error)"
                }
                if ($descendantResidueObservation.expected_generation_live) {
                    throw "failure-cleanup self-test $($case.name) left descendant process $descendantProcessId alive"
                }
            }
            if ($caseWall.ElapsedMilliseconds -gt ([int]$case.timeout_ms + 12000)) {
                throw "failure-cleanup self-test $($case.name) exceeded its bounded wall allowance: $($caseWall.ElapsedMilliseconds)ms"
            }
            if ($script:CommandEvidence.Count -ne ($evidenceIndex + 1)) {
                throw "failure-cleanup self-test $($case.name) did not add exactly one failure evidence row"
            }
            $failureEvidence = $script:CommandEvidence[$evidenceIndex]
            if ([string]$failureEvidence.failure_stage -cne [string]$case.expected_stage) {
                throw "failure-cleanup self-test $($case.name) recorded stage $($failureEvidence.failure_stage); expected $($case.expected_stage)"
            }
            $cleanup = $failureEvidence.failure_cleanup
            $cleanupErrorCount = @($cleanup.cleanup_errors).Count
            if ($null -eq $record.OwnershipIdentity -or $null -eq $record.OwnershipIdentity.exit_time_utc) {
                throw "failure-cleanup self-test $($case.name) omitted its terminal process identity"
            }
            if ($cleanup.exit_confirmed -ne $true -or
                $cleanup.process_disposed -ne $true -or
                $cleanup.record_cleared -ne $true -or
                -not $cleanup.stdout_completed -or
                -not $cleanup.stderr_completed -or
                $cleanupErrorCount -ne [int]$case.expected_cleanup_error_count) {
                throw "failure-cleanup self-test $($case.name) produced incomplete cleanup evidence: $($cleanup | ConvertTo-Json -Compress -Depth 8)"
            }
            if ([string]$case.expected_stage -ceq 'output-drain') {
                $sweep = $cleanup.descendant_sweep
                $killedPids = @($sweep.killed_descendants | ForEach-Object { [int]$_.process_id })
                $unverifiedOrOpen = @($sweep.killed_descendants | Where-Object {
                    -not $_.identity_verified -or -not $_.executable_path_verified -or
                    -not $_.exit_confirmed -or -not $_.handle_closed
                })
                if ($null -eq $sweep -or -not $sweep.attempted -or
                    $sweep.candidate_count -lt 1 -or $killedPids -notcontains $descendantProcessId -or
                    $unverifiedOrOpen.Count -ne 0 -or
                    $sweep.opened_handle_count -ne $sweep.closed_handle_count -or
                    $sweep.opened_handle_count -ne $sweep.candidate_count -or
                    -not $sweep.handles_disposed -or $sweep.close_error_count -ne 0 -or
                    @($sweep.errors).Count -ne 0) {
                    throw "failure-cleanup self-test $($case.name) descendant sweep evidence was incomplete: $($sweep | ConvertTo-Json -Compress -Depth 8)"
                }
            }
            if ([bool]$case.observer_failure -and
                [string]$cleanup.cleanup_errors[0] -cnotmatch 'injected bounded Win32_Process observation timeout') {
                throw "failure-cleanup self-test $($case.name) did not preserve its bounded observer error"
            }
            if ($failureEvidence.observer_deferred -ne $false -or $null -eq $cleanup.observer_wall_ms) {
                throw "failure-cleanup self-test $($case.name) omitted its post-failure observation"
            }
            $caseResults.Add([pscustomobject][ordered]@{
                name = [string]$case.name
                failure_stage = [string]$failureEvidence.failure_stage
                exit_confirmed = [bool]$cleanup.exit_confirmed
                stdout_completed = [bool]$stdoutTask.IsCompleted
                stderr_completed = $null -eq $stderrTask -or [bool]$stderrTask.IsCompleted
                process_residue_count = 0
                descendant_process_id = $descendantProcessId
                descendant_process_residue_count = 0
                direct_residue_observation = $directResidueObservation
                descendant_residue_observation = $descendantResidueObservation
                record_residue_count = 0
                cleanup_error_count = $cleanupErrorCount
                descendant_sweep_candidate_count = if ($null -eq $cleanup.descendant_sweep) { 0 } else { [int]$cleanup.descendant_sweep.candidate_count }
                observer_deferred = [bool]$failureEvidence.observer_deferred
                wall_ms = [int64]$caseWall.ElapsedMilliseconds
            })
        }
    } finally {
        $script:ProcessExitTimeFailureForTest = $false
        $script:ProcessFinalizeFailureForTest = $null
        $script:ProcessSetupFailureForTest = $null
        $script:ProcessObservationFailureForTest = $false
    }
    if ($script:CleanupErrors.Count -ne $globalCleanupErrorCountBefore) {
        throw 'failure-cleanup self-test leaked an injected error into aggregate harness cleanup errors'
    }
    $expectedInjectedCleanupErrorCount = @($caseResults | Where-Object name -CEQ 'observer-timeout' |
        ForEach-Object { [int]$_.cleanup_error_count } | Measure-Object -Sum).Sum
    $unexpectedCleanupErrorCount = @($setupCaseResults.cleanup_error_count | Measure-Object -Sum).Sum +
        @($caseResults | Where-Object name -CNE 'observer-timeout' |
            ForEach-Object { [int]$_.cleanup_error_count } | Measure-Object -Sum).Sum
    return [pscustomobject][ordered]@{
        status = 'passed'
        executable = $portablePowerShell
        executable_sha256 = Get-Sha256 $portablePowerShell
        environment_names = @($selfTestEnvironment.Keys | Sort-Object)
        provider_credential_names_present = @()
        provider_invoked = $false
        total_case_count = $setupCaseResults.Count + $caseResults.Count + 1
        setup_failure_case_count = $setupCaseResults.Count
        setup_failure_cases = $setupCaseResults.ToArray()
        batch_start_ownership_case_count = 1
        batch_start_ownership = $batchStartOwnership
        case_count = $caseResults.Count
        cases = $caseResults.ToArray()
        process_residue_count = 0
        descendant_process_residue_count = 0
        record_residue_count = 0
        incomplete_pipe_task_count = 0
        expected_injected_cleanup_error_count = [int]$expectedInjectedCleanupErrorCount
        unexpected_cleanup_error_count = [int]$unexpectedCleanupErrorCount
        aggregate_cleanup_error_delta = 0
    }
}

function ConvertTo-TomlPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    return '"' + $Path.Replace('\', '\\').Replace('"', '\"') + '"'
}

function New-FakeProviderConfig {
    param([Parameter(Mandatory = $true)][string]$ColayHomePath, [Parameter(Mandatory = $true)][string]$FakeProvider)
    $escaped = ConvertTo-TomlPath $FakeProvider
    $config = @"
config_version = 4
[orchestrator.providers.codex]
executable = $escaped
[orchestrator.providers.claude]
executable = $escaped
[orchestrator.providers.gemini]
executable = $escaped
[orchestrator.providers.agy]
executable = $escaped
"@
    Set-Content -LiteralPath (Join-Path $ColayHomePath 'config.toml') -Value $config -Encoding utf8NoBOM
}

function Invoke-Sqlite {
    param(
        [Parameter(Mandatory = $true)][string]$Database,
        [Parameter(Mandatory = $true)][string]$Sql,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [switch]$ReadOnly,
        [switch]$Csv,
        [string]$Label = 'sqlite',
        [int]$TimeoutMs = 30000
    )
    $pythonCode = @'
import csv
import pathlib
import sqlite3
import sys

database = pathlib.Path(sys.argv[1]).resolve()
mode = sys.argv[2]
sql = sys.stdin.read()
if mode == "query":
    connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)
    cursor = connection.execute(sql)
    writer = csv.writer(sys.stdout, lineterminator="\n")
    writer.writerow([column[0] for column in cursor.description])
    writer.writerows(cursor)
else:
    connection = sqlite3.connect(database)
    connection.executescript(sql)
    connection.commit()
connection.close()
'@
    $mode = if ($ReadOnly) { 'query' } else { 'script' }
    $arguments = @('-I', '-c', $pythonCode, $Database, $mode)
    $result = Invoke-HarnessProcess -Executable $script:PythonExe -ArgumentValues $arguments `
        -WorkingDirectory $WorkingDirectory -Environment $Environment -Label $Label `
        -TimeoutMs $TimeoutMs -StandardInputText $Sql
    if ($Csv) {
        if ([string]::IsNullOrWhiteSpace($result.stdout)) { return ,([object[]]@()) }
        return ,([object[]]@($result.stdout | ConvertFrom-Csv))
    }
    return $result.stdout
}

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-Utf8Sha256 {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        return [System.Convert]::ToHexString($sha256.ComputeHash($bytes)).ToLowerInvariant()
    } finally {
        $sha256.Dispose()
    }
}

function ConvertTo-GitWorkingTreeEvidence {
    param(
        [Parameter(Mandatory = $true)][int]$ExitCode,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$PorcelainV1Output
    )
    $entries = @($PorcelainV1Output -split "`r?`n" | Where-Object {
            -not [string]::IsNullOrEmpty([string]$_)
        })
    $canonicalStatus = [string]::Join("`n", [string[]]$entries)
    return [pscustomobject][ordered]@{
        status = if ($ExitCode -ne 0) { 'invalid' } elseif ($entries.Count -eq 0) { 'clean' } else { 'dirty' }
        git_status_exit_code = $ExitCode
        entry_count = $entries.Count
        porcelain_v1_sha256 = Get-Utf8Sha256 $canonicalStatus
        tracked_changes_included = $true
        staged_changes_included = $true
        untracked_files_included = $true
        ignored_files_included = $false
    }
}

function Clear-SourceCleanTreeCommandEvidenceOutput {
    param([Parameter(Mandatory = $true)]$CommandEvidence)
    $matchedCount = 0
    $stdoutClearedCount = 0
    $stderrClearedCount = 0
    foreach ($entry in @($CommandEvidence)) {
        if ([string]$entry.label -cne 'source-clean-tree') { continue }
        $matchedCount++
        if ($entry.PSObject.Properties.Name -contains 'stdout') {
            $entry.stdout = $null
            $stdoutClearedCount++
        }
        if ($entry.PSObject.Properties.Name -contains 'stderr') {
            $entry.stderr = $null
            $stderrClearedCount++
        }
    }
    return [pscustomobject][ordered]@{
        matched_entry_count = $matchedCount
        stdout_cleared_count = $stdoutClearedCount
        stderr_cleared_count = $stderrClearedCount
    }
}

function Invoke-SourceCleanTreeCommandEvidenceRedactionSelfTest {
    $failureCommands = [System.Collections.Generic.List[object]]::new()
    $failureFilename = 'synthetic-private-failure-name.txt'
    $primaryFailureMessage = 'synthetic source-clean-tree boundary failure'
    $primaryFailureIdentity = 'SyntheticSourceCleanTreePrimary'
    $cleanupErrorMessage = 'synthetic cleanup error retained'
    $syntheticSourceIdentity = [pscustomobject][ordered]@{
        verification_status = 'pending'
        working_tree = [pscustomobject][ordered]@{ status = 'pending' }
    }
    $primaryException = [System.InvalidOperationException]::new($primaryFailureMessage)
    $primaryException.Data['synthetic_identity'] = $primaryFailureIdentity
    $primaryErrorRecord = [System.Management.Automation.ErrorRecord]::new(
        $primaryException,
        $primaryFailureIdentity,
        [System.Management.Automation.ErrorCategory]::InvalidOperation,
        'source-clean-tree'
    )
    $observedPrimaryFailure = $null
    $failureScrub = $null
    try {
        try {
            $failureCommands.Add([pscustomobject][ordered]@{
                label = 'source-clean-tree'
                stdout = "?? $failureFilename"
                stderr = $failureFilename
                failure_stage = 'synthetic-post-evidence-throw'
                failure_cleanup = [pscustomobject][ordered]@{
                    cleanup_errors = @($cleanupErrorMessage)
                }
            })
            throw $primaryErrorRecord
        } catch {
            $syntheticSourceIdentity.working_tree.status = 'invalid'
            $syntheticSourceIdentity.verification_status = 'invalid'
            throw
        } finally {
            $failureScrub = Clear-SourceCleanTreeCommandEvidenceOutput -CommandEvidence $failureCommands
        }
    } catch {
        $observedPrimaryFailure = $_
    }
    $primaryIdentityPreserved = $null -ne $observedPrimaryFailure -and
        [object]::ReferenceEquals($observedPrimaryFailure.Exception, $primaryException) -and
        [string]$observedPrimaryFailure.FullyQualifiedErrorId -ceq $primaryFailureIdentity -and
        [string]$observedPrimaryFailure.Exception.Data['synthetic_identity'] -ceq $primaryFailureIdentity
    $primaryMessagePreserved = $null -ne $observedPrimaryFailure -and
        [string]$observedPrimaryFailure.Exception.Message -ceq $primaryFailureMessage
    $primaryCategoryPreserved = $null -ne $observedPrimaryFailure -and
        [string]$observedPrimaryFailure.CategoryInfo.Category -ceq 'InvalidOperation'
    $invalidStatesRecorded = [string]$syntheticSourceIdentity.working_tree.status -ceq 'invalid' -and
        [string]$syntheticSourceIdentity.verification_status -ceq 'invalid'
    if (-not $primaryIdentityPreserved -or -not $primaryMessagePreserved -or
        -not $primaryCategoryPreserved -or -not $invalidStatesRecorded) {
        throw 'source-clean-tree redaction self-test did not preserve the primary failure'
    }
    $cleanupEvidencePreserved = @($failureCommands[0].failure_cleanup.cleanup_errors).Count -eq 1 -and
        [string]$failureCommands[0].failure_cleanup.cleanup_errors[0] -ceq $cleanupErrorMessage
    if (-not $cleanupEvidencePreserved) {
        throw 'source-clean-tree redaction self-test did not preserve cleanup errors'
    }
    $failureJson = [ordered]@{
        source_identity = $syntheticSourceIdentity
        failure = [ordered]@{
            identity = $observedPrimaryFailure.FullyQualifiedErrorId
            message = $observedPrimaryFailure.Exception.Message
            category = [string]$observedPrimaryFailure.CategoryInfo.Category
        }
        commands = $failureCommands
    } | ConvertTo-Json -Depth 10 -Compress
    $rawFilenameAbsentAfterFinalSerialization = -not $failureJson.Contains($failureFilename)
    if (-not $rawFilenameAbsentAfterFinalSerialization -or
        $null -ne $failureCommands[0].stdout -or
        $null -ne $failureCommands[0].stderr -or
        [int]$failureScrub.matched_entry_count -ne 1) {
        throw 'source-clean-tree redaction self-test leaked failure-path output'
    }

    $successCommands = [System.Collections.Generic.List[object]]::new()
    $successFilenameOne = 'synthetic-private-success-one.txt'
    $successFilenameTwo = 'synthetic-private-success-two.txt'
    $successCommands.Add([pscustomobject][ordered]@{
        label = 'source-clean-tree'
        stdout = " M $successFilenameOne"
        stderr = $successFilenameOne
    })
    $successCommands.Add([pscustomobject][ordered]@{
        label = 'source-commit'
        stdout = 'allowed-source-commit-output'
        stderr = $null
    })
    $successCommands.Add([pscustomobject][ordered]@{
        label = 'source-clean-tree'
        stdout = "?? $successFilenameTwo"
        stderr = $successFilenameTwo
    })
    $successCommands.Add([pscustomobject][ordered]@{
        label = 'Source-Clean-Tree'
        stdout = 'allowed-case-distinct-output'
        stderr = $null
    })
    $successScrub = Clear-SourceCleanTreeCommandEvidenceOutput -CommandEvidence $successCommands
    $successJson = $successCommands | ConvertTo-Json -Depth 10 -Compress
    if ([int]$successScrub.matched_entry_count -ne 2 -or
        [int]$successScrub.stdout_cleared_count -ne 2 -or
        [int]$successScrub.stderr_cleared_count -ne 2 -or
        $successJson.Contains($successFilenameOne) -or
        $successJson.Contains($successFilenameTwo) -or
        $null -ne $successCommands[0].stdout -or
        $null -ne $successCommands[0].stderr -or
        $null -ne $successCommands[2].stdout -or
        $null -ne $successCommands[2].stderr -or
        [string]$successCommands[1].stdout -cne 'allowed-source-commit-output' -or
        [string]$successCommands[3].stdout -cne 'allowed-case-distinct-output') {
        throw 'source-clean-tree redaction self-test did not scrub every exact-label success entry'
    }
    return [pscustomobject][ordered]@{
        status = 'passed'
        failure_injection = [pscustomobject][ordered]@{
            evidence_added_before_throw = $true
            production_shaped_catch_finally = $true
            invalid_states_recorded = $invalidStatesRecorded
            primary_failure_preserved = $primaryIdentityPreserved -and
                $primaryMessagePreserved -and $primaryCategoryPreserved
            primary_failure_identity_preserved = $primaryIdentityPreserved
            primary_failure_message_preserved = $primaryMessagePreserved
            primary_failure_category_preserved = $primaryCategoryPreserved
            primary_failure_identity = $primaryFailureIdentity
            primary_failure_category = 'InvalidOperation'
            cleanup_errors_preserved = $cleanupEvidencePreserved
            cleanup_evidence_preserved = $cleanupEvidencePreserved
            sensitive_filename_absent_from_final_json = $rawFilenameAbsentAfterFinalSerialization
            raw_filename_absent_after_final_serialization = $rawFilenameAbsentAfterFinalSerialization
            scrubbed_entry_count = [int]$failureScrub.matched_entry_count
        }
        success = [pscustomobject][ordered]@{
            exact_label_entry_count = 2
            scrubbed_entry_count = [int]$successScrub.matched_entry_count
            all_exact_label_outputs_cleared = $true
            nonmatching_entries_preserved = $true
            sensitive_filenames_absent_from_final_json = $true
        }
    }
}

function Get-SqliteFamilyHashes {
    param([Parameter(Mandatory = $true)][string]$Database)
    $hashes = [ordered]@{}
    foreach ($suffix in @('', '-wal', '-shm', '-journal')) {
        $path = $Database + $suffix
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $hashes[$suffix] = [pscustomobject]@{
                bytes = (Get-Item -LiteralPath $path).Length
                sha256 = Get-Sha256 $path
            }
        }
    }
    if (-not $hashes.Contains('')) {
        throw "SQLite family has no primary database: $Database"
    }
    return $hashes
}

function Test-JsonElementStructuralEquality {
    param(
        [Parameter(Mandatory = $true)][System.Text.Json.JsonElement]$Expected,
        [Parameter(Mandatory = $true)][System.Text.Json.JsonElement]$Actual
    )
    if ($Expected.ValueKind -ne $Actual.ValueKind) { return $false }

    switch ([string]$Expected.ValueKind) {
        'Object' {
            $expectedProperties = [System.Collections.Generic.Dictionary[string, System.Text.Json.JsonElement]]::new(
                [System.StringComparer]::Ordinal
            )
            $actualProperties = [System.Collections.Generic.Dictionary[string, System.Text.Json.JsonElement]]::new(
                [System.StringComparer]::Ordinal
            )
            foreach ($property in $Expected.EnumerateObject()) {
                if (-not $expectedProperties.TryAdd($property.Name, $property.Value)) { return $false }
            }
            foreach ($property in $Actual.EnumerateObject()) {
                if (-not $actualProperties.TryAdd($property.Name, $property.Value)) { return $false }
            }
            if ($expectedProperties.Count -ne $actualProperties.Count) { return $false }
            foreach ($property in $expectedProperties.GetEnumerator()) {
                if (-not $actualProperties.ContainsKey($property.Key)) { return $false }
                if (-not (Test-JsonElementStructuralEquality -Expected $property.Value `
                            -Actual $actualProperties[$property.Key])) {
                    return $false
                }
            }
            return $true
        }
        'Array' {
            if ($Expected.GetArrayLength() -ne $Actual.GetArrayLength()) { return $false }
            $expectedItems = [System.Collections.Generic.List[System.Text.Json.JsonElement]]::new()
            $actualItems = [System.Collections.Generic.List[System.Text.Json.JsonElement]]::new()
            foreach ($item in $Expected.EnumerateArray()) { $expectedItems.Add($item) }
            foreach ($item in $Actual.EnumerateArray()) { $actualItems.Add($item) }
            for ($index = 0; $index -lt $expectedItems.Count; $index++) {
                if (-not (Test-JsonElementStructuralEquality -Expected $expectedItems[$index] `
                            -Actual $actualItems[$index])) {
                    return $false
                }
            }
            return $true
        }
        'String' {
            return [string]::Equals(
                $Expected.GetString(),
                $Actual.GetString(),
                [System.StringComparison]::Ordinal
            )
        }
        'Number' { return $Expected.GetRawText() -ceq $Actual.GetRawText() }
        'True' { return $true }
        'False' { return $true }
        'Null' { return $true }
        default { return $Expected.GetRawText() -ceq $Actual.GetRawText() }
    }
}

function Assert-EquivalentJson {
    param($Expected, $Actual, [string]$Label)
    $expectedJson = ConvertTo-Json -InputObject $Expected -Depth 20 -Compress
    $actualJson = ConvertTo-Json -InputObject $Actual -Depth 20 -Compress
    $expectedDocument = [System.Text.Json.JsonDocument]::Parse($expectedJson)
    $actualDocument = [System.Text.Json.JsonDocument]::Parse($actualJson)
    try {
        if (-not (Test-JsonElementStructuralEquality -Expected $expectedDocument.RootElement `
                    -Actual $actualDocument.RootElement)) {
            throw "$Label changed: expected $expectedJson, found $actualJson"
        }
    } finally {
        $expectedDocument.Dispose()
        $actualDocument.Dispose()
    }
}

function Assert-LatencySourcePreparationEvidence {
    param(
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Seeds,
        [Parameter(Mandatory = $true)][object[]]$CommandEvidence
    )
    $expectedLabels = @(1..9 | ForEach-Object { "seed-schema-v8-$_" })
    if ($Seeds.Count -ne $expectedLabels.Count) {
        throw "latency fixture preparation retained $($Seeds.Count) sources; expected $($expectedLabels.Count)"
    }

    $integralTypes = @([byte], [sbyte], [int16], [uint16], [int32], [uint32], [int64], [uint64])
    foreach ($index in 1..9) {
        if (-not $Seeds.Contains($index)) {
            throw "latency fixture preparation omitted retained source index $index"
        }
        $seed = $Seeds[$index]
        if ($null -eq $seed -or $seed.PSObject.Properties.Name -cnotcontains 'index' -or
            $null -eq $seed.index -or $integralTypes -notcontains $seed.index.GetType() -or
            [int64]$seed.index -ne [int64]$index) {
            throw "latency fixture preparation retained an invalid source at index $index"
        }
    }

    $seedCommands = @($CommandEvidence | Where-Object {
            $_.PSObject.Properties.Name -ccontains 'label' -and
            $_.label -is [string] -and
            ([string]$_.label).StartsWith('seed-schema-v8-', [System.StringComparison]::Ordinal)
        })
    if ($seedCommands.Count -ne $expectedLabels.Count) {
        throw "latency fixture preparation recorded $($seedCommands.Count) seed commands; expected $($expectedLabels.Count)"
    }
    for ($offset = 0; $offset -lt $expectedLabels.Count; $offset++) {
        $command = $seedCommands[$offset]
        foreach ($propertyName in @('label', 'measurement_method', 'exit_code', 'timed_out', 'elapsed_ms')) {
            if ($command.PSObject.Properties.Name -cnotcontains $propertyName) {
                throw "latency fixture command $offset omitted $propertyName"
            }
        }
        if ($command.label -isnot [string] -or [string]$command.label -cne $expectedLabels[$offset]) {
            throw "latency fixture command label mismatch at ordinal $offset"
        }
        if ($command.measurement_method -isnot [string] -or
            [string]$command.measurement_method -cne 'os-process-lifetime') {
            throw "latency fixture command $($command.label) lacks OS process lifetime evidence"
        }
        if ($null -eq $command.exit_code -or
            $integralTypes -notcontains $command.exit_code.GetType() -or
            [int64]$command.exit_code -ne 0) {
            throw "latency fixture command $($command.label) did not exit successfully"
        }
        if ($command.timed_out -isnot [bool] -or [bool]$command.timed_out) {
            throw "latency fixture command $($command.label) has invalid timeout evidence"
        }
        if ($null -eq $command.elapsed_ms -or
            $integralTypes -notcontains $command.elapsed_ms.GetType() -or
            [int64]$command.elapsed_ms -lt 0) {
            throw "latency fixture command $($command.label) has invalid elapsed time evidence"
        }
    }

    $seedWriteTimes = @($seedCommands | ForEach-Object { [int64]$_.elapsed_ms })
    $sortedSeedWriteTimes = @($seedWriteTimes | Sort-Object)
    return [pscustomobject][ordered]@{
        scope = 'fixture-only-python-sqlite-write-not-product-latency'
        source_count = $Seeds.Count
        command_labels = @($expectedLabels)
        seed_write_times_ms = $seedWriteTimes
        minimum_ms = [int64]$sortedSeedWriteTimes[0]
        median_ms = [int64]$sortedSeedWriteTimes[4]
        maximum_ms = [int64]$sortedSeedWriteTimes[-1]
        completed_before_timing_self_test = $true
        completed_before_daemon_start = $true
        timing_included_in_latency_thresholds = $false
    }
}

function ConvertTo-ComparableWindowsPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    $full = [System.IO.Path]::GetFullPath($Path)
    if ($full.StartsWith('\\?\', [System.StringComparison]::Ordinal)) {
        $full = $full.Substring(4)
    }
    return $full.TrimEnd('\').ToLowerInvariant()
}

function Assert-ControlledPublicationContents {
    param(
        [Parameter(Mandatory = $true)][string]$PublicationPath,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $publicationItem = Get-Item -LiteralPath $PublicationPath -Force -ErrorAction Stop
    if (-not $publicationItem.PSIsContainer -or
        ($publicationItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw "$Label publication is not a regular directory: $PublicationPath"
    }
    $publishedDatabase = Join-Path $PublicationPath 'legacy.db'
    $publicationChildren = @(Get-ChildItem -LiteralPath $PublicationPath -Force -ErrorAction Stop)
    if ($publicationChildren.Count -ne 1 -or
        $publicationChildren[0].Name -cne 'legacy.db' -or
        $publicationChildren[0].PSIsContainer -or
        ($publicationChildren[0].Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
        (ConvertTo-ComparableWindowsPath $publicationChildren[0].FullName) -cne
        (ConvertTo-ComparableWindowsPath $publishedDatabase)) {
        throw "$Label controlled-source publication must contain exactly one regular legacy.db child"
    }
    return $publishedDatabase
}

function Assert-GlobalPublicationLayout {
    param(
        [Parameter(Mandatory = $true)][string]$WorkspacesRoot,
        [Parameter(Mandatory = $true)]$ExpectedWorkspaceRootPaths,
        [Parameter(Mandatory = $true)]$ExpectedPublicationPaths,
        [Parameter(Mandatory = $true)][int]$ExpectedCount
    )
    $expectedLockPaths = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($publicationPath in $ExpectedPublicationPaths) {
        [void]$expectedLockPaths.Add("${publicationPath}.lock")
    }
    $observedPublicationPaths = [System.Collections.Generic.List[string]]::new()
    $observedPublicationSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $observedLockPaths = [System.Collections.Generic.List[string]]::new()
    $observedLockSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $observedWorkspaceRootPaths = [System.Collections.Generic.List[string]]::new()
    $observedWorkspaceRootSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $stagingDirectories = [System.Collections.Generic.List[string]]::new()
    $unexpectedItems = [System.Collections.Generic.List[string]]::new()
    if (Test-Path -LiteralPath $WorkspacesRoot -PathType Container) {
        $workspacesRootItem = Get-Item -LiteralPath $WorkspacesRoot -Force -ErrorAction Stop
        if ($workspacesRootItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw "global workspace publication root is a reparse point: $WorkspacesRoot"
        }
        foreach ($workspaceDirectory in @(Get-ChildItem -LiteralPath $WorkspacesRoot -Force -ErrorAction Stop)) {
            $normalizedWorkspaceRoot = ConvertTo-ComparableWindowsPath $workspaceDirectory.FullName
            if (-not $workspaceDirectory.PSIsContainer -or
                ($workspaceDirectory.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
                -not $ExpectedWorkspaceRootPaths.Contains($normalizedWorkspaceRoot)) {
                $unexpectedItems.Add("$($workspaceDirectory.FullName) [unexpected-workspace-root-item]")
                continue
            }
            $observedWorkspaceRootPaths.Add($workspaceDirectory.FullName)
            [void]$observedWorkspaceRootSet.Add($normalizedWorkspaceRoot)
            $importsDirectory = Join-Path $workspaceDirectory.FullName 'imports'
            if (-not (Test-Path -LiteralPath $importsDirectory -PathType Container)) {
                continue
            }
            $importsItem = Get-Item -LiteralPath $importsDirectory -Force -ErrorAction Stop
            if ($importsItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
                throw "legacy imports root is a reparse point: $importsDirectory"
            }
            foreach ($item in @(Get-ChildItem -LiteralPath $importsDirectory -Force -ErrorAction Stop)) {
                $normalized = ConvertTo-ComparableWindowsPath $item.FullName
                if ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
                    $unexpectedItems.Add("$($item.FullName) [reparse-point]")
                    continue
                }
                if ($item.PSIsContainer) {
                    $observedPublicationPaths.Add($item.FullName)
                    [void]$observedPublicationSet.Add($normalized)
                    if ($item.Name.EndsWith('.staging', [System.StringComparison]::OrdinalIgnoreCase)) {
                        $stagingDirectories.Add($item.FullName)
                    }
                    if ($item.Name -cnotmatch '^[0-9a-f]{64}$' -or -not $ExpectedPublicationPaths.Contains($normalized)) {
                        $unexpectedItems.Add("$($item.FullName) [unexpected-directory]")
                    }
                } else {
                    $observedLockPaths.Add($item.FullName)
                    [void]$observedLockSet.Add($normalized)
                    if ($item.Name -cnotmatch '^[0-9a-f]{64}\.lock$' -or
                        -not $expectedLockPaths.Contains($normalized) -or
                        $item.Length -ne 0) {
                        $unexpectedItems.Add("$($item.FullName) [unexpected-file length=$($item.Length)]")
                    }
                }
            }
        }
    }
    $missingWorkspaceRoots = @($ExpectedWorkspaceRootPaths | Where-Object { -not $observedWorkspaceRootSet.Contains($_) })
    $missingDirectories = @($ExpectedPublicationPaths | Where-Object { -not $observedPublicationSet.Contains($_) })
    $missingLockFiles = @($expectedLockPaths | Where-Object { -not $observedLockSet.Contains($_) })
    if ($observedWorkspaceRootPaths.Count -ne $ExpectedWorkspaceRootPaths.Count) {
        throw "global workspace filesystem-root count mismatch: expected exactly $($ExpectedWorkspaceRootPaths.Count), found $($observedWorkspaceRootPaths.Count)"
    }
    if ($missingWorkspaceRoots.Count -ne 0) {
        throw "durable workspace filesystem roots are missing: $($missingWorkspaceRoots -join ', ')"
    }
    if ($observedPublicationPaths.Count -ne $ExpectedCount) {
        throw "global publication count mismatch: expected exactly $ExpectedCount, found $($observedPublicationPaths.Count)"
    }
    if ($observedLockPaths.Count -ne $ExpectedCount) {
        throw "global publication lock count mismatch: expected exactly $ExpectedCount, found $($observedLockPaths.Count)"
    }
    if ($missingDirectories.Count -ne 0) {
        throw "expected publication directories are missing: $($missingDirectories -join ', ')"
    }
    if ($missingLockFiles.Count -ne 0) {
        throw "expected publication lock files are missing: $($missingLockFiles -join ', ')"
    }
    if ($stagingDirectories.Count -ne 0) {
        throw "legacy import staging directories remained: $($stagingDirectories -join ', ')"
    }
    if ($unexpectedItems.Count -ne 0) {
        throw "unexpected or orphan legacy import items remained: $($unexpectedItems -join ', ')"
    }
    return [pscustomobject]@{
        root = $WorkspacesRoot
        expected_workspace_root_count = $ExpectedWorkspaceRootPaths.Count
        observed_workspace_root_count = $observedWorkspaceRootPaths.Count
        observed_workspace_roots = $observedWorkspaceRootPaths.ToArray()
        expected_count = $ExpectedCount
        observed_count = $observedPublicationPaths.Count
        observed_paths = $observedPublicationPaths.ToArray()
        expected_lock_count = $ExpectedCount
        observed_lock_count = $observedLockPaths.Count
        observed_lock_paths = $observedLockPaths.ToArray()
        staging_directories = $stagingDirectories.ToArray()
        unexpected_items = $unexpectedItems.ToArray()
    }
}

function New-LegacyWorkspace {
    param(
        [Parameter(Mandatory = $true)][int]$Index,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment
    )
    $repository = Join-Path $Root ("legacy-workspace-{0:D2}" -f $Index)
    $state = Join-Path $repository '.colay'
    New-Item -ItemType Directory -Path $state -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $state 'config.toml') -Value "config_version = 4`n" -Encoding utf8NoBOM
    $database = Join-Path $state 'orchestrator.db'
    $names = @('core', 'execution', 'audit_and_control', 'durable_sessions', 'chat_workspace_state', 'approved_task_graphs', 'parallel_execution', 'result_integration')
    $sql = [System.Text.StringBuilder]::new()
    [void]$sql.AppendLine('PRAGMA foreign_keys = ON;')
    for ($version = 1; $version -le 8; $version++) {
        $migration = Join-Path $script:RepoRoot ("migrations/{0:D4}_{1}.sql" -f $version, $names[$version - 1])
        if (-not (Test-Path -LiteralPath $migration -PathType Leaf)) {
            throw "missing schema-v8 migration: $migration"
        }
        [void]$sql.AppendLine((Get-Content -LiteralPath $migration -Raw))
        $checksum = Get-Sha256 $migration
        $applied = [datetime]::UtcNow.ToString('o')
        [void]$sql.AppendLine("INSERT INTO schema_migrations(version, name, checksum, applied_at) VALUES ($version, '$($names[$version - 1])', '$checksum', '$applied');")
    }
    $sessionId = "01987d4e-2a54-7000-8000-{0:D12}" -f $Index
    $title = "native Windows stress workspace $Index"
    [void]$sql.AppendLine("INSERT INTO sessions(session_id, schema_version, revision, title, state, created_at, updated_at) VALUES ('$sessionId', '1.0', 0, '$title', 'planning', '2026-08-05T00:00:00Z', '2026-08-05T00:00:00Z');")
    Invoke-Sqlite -Database $database -Sql $sql.ToString() -WorkingDirectory $repository `
        -Environment $Environment -Label "seed-schema-v8-$Index" | Out-Null
    $sessionRows = Invoke-Sqlite -Database $database `
        -Sql 'SELECT count(*) AS row_count FROM sessions;' -WorkingDirectory $repository `
        -Environment $Environment -ReadOnly -Csv -Label "verify-non-empty-$Index"
    if ($sessionRows.Count -ne 1 -or [int]$sessionRows[0].row_count -ne 1) {
        throw "legacy workspace $Index is not a distinct non-empty schema-v8 source"
    }
    $hashes = Get-SqliteFamilyHashes $database
    return [pscustomobject]@{
        index = $Index
        repository = [System.IO.Path]::GetFullPath($repository)
        canonical_repository = (Resolve-Path -LiteralPath $repository).Path
        database = $database
        session_id = $sessionId
        source_hashes = [pscustomobject][ordered]@{
            before = $hashes
            after = $null
        }
        source_root_hash = $null
        source_evidence = $null
        config_sha256 = Get-Sha256 (Join-Path $state 'config.toml')
    }
}

function Add-SourceEvidence {
    param(
        [Parameter(Mandatory = $true)]$Seed,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Summary
    )
    $sourceEvidence = [pscustomobject][ordered]@{
        index = $Seed.index
        session_id = $Seed.session_id
        database = $Seed.database
        source_root_hash = $null
        sqlite_family_hashes = $Seed.source_hashes
        config_sha256 = $Seed.config_sha256
    }
    $Seed.source_evidence = $sourceEvidence
    $Summary.sources += $sourceEvidence
}

function Get-InspectionCount {
    param([Parameter(Mandatory = $true)][string]$Marker)
    if (-not (Test-Path -LiteralPath $Marker -PathType Leaf)) { return 0 }
    return @((Get-Content -LiteralPath $Marker) | Where-Object { $_ -ceq 'legacy-inspect' }).Count
}

function Get-AttributedInspectionSnapshot {
    param([Parameter(Mandatory = $true)][string]$Directory)
    if (-not (Test-Path -LiteralPath $Directory -PathType Container)) {
        throw "attributed inspection marker directory is missing: $Directory"
    }
    $rootItem = Get-Item -LiteralPath $Directory -Force -ErrorAction Stop
    if ($rootItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
        throw "attributed inspection marker directory is a reparse point: $Directory"
    }
    $groups = [ordered]@{}
    foreach ($groupItem in @(Get-ChildItem -LiteralPath $Directory -Force -ErrorAction Stop)) {
        if (-not $groupItem.PSIsContainer -or
            ($groupItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
            $groupItem.Name -cnotmatch '^[0-9a-f]{64}$') {
            throw "unexpected attributed inspection marker item: $($groupItem.FullName)"
        }
        $eventNames = [System.Collections.Generic.List[string]]::new()
        $eventNameSet = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)
        $eventPaths = [System.Collections.Generic.List[string]]::new()
        foreach ($eventItem in @(Get-ChildItem -LiteralPath $groupItem.FullName -Force -ErrorAction Stop)) {
            if ($eventItem.PSIsContainer -or
                ($eventItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
                $eventItem.Length -ne 0 -or
                $eventItem.Name -cnotmatch '^event-[0-9]+-[0-9]+$' -or
                -not $eventNameSet.Add($eventItem.Name)) {
                throw "attributed inspection event is not an empty regular file: $($eventItem.FullName)"
            }
            $eventNames.Add($eventItem.Name)
            $eventPaths.Add($eventItem.FullName)
        }
        if ($eventNames.Count -ne 2) {
            throw "attributed inspection group $($groupItem.Name) contains $($eventNames.Count) events; expected exactly 2"
        }
        $groups[$groupItem.Name] = [pscustomobject][ordered]@{
            group_id = $groupItem.Name
            event_count = $eventNames.Count
            event_names = @($eventNames | Sort-Object)
            event_paths = @($eventPaths | Sort-Object)
        }
    }
    return ,$groups
}

function Assert-LatencyInspectionMarkers {
    param(
        [Parameter(Mandatory = $true)][string]$AggregateMarker,
        [Parameter(Mandatory = $true)][string]$AttributedDirectory,
        [Parameter(Mandatory = $true)][ValidateRange(0, [int]::MaxValue)][int]$ExpectedAggregateCount,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $aggregateCount = Get-InspectionCount $AggregateMarker
    if ($aggregateCount -ne $ExpectedAggregateCount) {
        throw "$Label aggregate inspection count was $aggregateCount; expected exactly $ExpectedAggregateCount"
    }
    $groups = Get-AttributedInspectionSnapshot $AttributedDirectory
    if ($groups.Count -ne 0) {
        throw "$Label produced $($groups.Count) attributed inspection group(s); expected exactly zero"
    }
    return [pscustomobject][ordered]@{
        aggregate_count = $aggregateCount
        attributed_group_count = 0
        attributed_event_count = 0
        groups = @()
    }
}

function Assert-CorrectnessInspectionMarkers {
    param(
        [Parameter(Mandatory = $true)][string]$AggregateMarker,
        [Parameter(Mandatory = $true)][string]$AttributedDirectory,
        [Parameter(Mandatory = $true)][string]$ExpectedGroup,
        [Parameter(Mandatory = $true)][string]$Label
    )
    if ($ExpectedGroup -cnotmatch '^[0-9a-f]{64}$') {
        throw "$Label expected group is not one lowercase 64-hex source_root_hash"
    }
    $aggregateCount = Get-InspectionCount $AggregateMarker
    if ($aggregateCount -ne 2) {
        throw "$Label aggregate inspection count was $aggregateCount; expected exactly 2"
    }
    $groups = Get-AttributedInspectionSnapshot $AttributedDirectory
    if ($groups.Count -ne 1 -or -not $groups.Contains($ExpectedGroup)) {
        throw "$Label did not contain exactly the durable source_root_hash marker group"
    }
    $group = $groups[$ExpectedGroup]
    return [pscustomobject][ordered]@{
        aggregate_count = $aggregateCount
        attributed_group_count = 1
        attributed_event_count = [int]$group.event_count
        groups = @($group)
    }
}

function Assert-DatabaseHealth {
    param([Parameter(Mandatory = $true)][string]$Database, [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment)
    $integrity = Invoke-Sqlite -Database $Database -Sql 'PRAGMA integrity_check;' `
        -WorkingDirectory $script:RunRoot -Environment $Environment -ReadOnly -Csv -Label 'global-integrity'
    if ($integrity.Count -ne 1 -or $integrity[0].integrity_check -cne 'ok') {
        throw "global SQLite integrity_check was not exactly ok"
    }
    $foreignKeys = Invoke-Sqlite -Database $Database -Sql 'PRAGMA foreign_key_check;' `
        -WorkingDirectory $script:RunRoot -Environment $Environment -ReadOnly -Csv -Label 'global-foreign-keys'
    if ($foreignKeys.Count -ne 0) {
        throw "global SQLite foreign_key_check found $($foreignKeys.Count) violation(s)"
    }
}

function Assert-DurableState {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$Seeds,
        [Parameter(Mandatory = $true)][int]$ExpectedWorkspaceCount,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [string]$ColayHomePath = $script:ColayHome
    )
    if ([string]::IsNullOrWhiteSpace($ColayHomePath)) {
        throw 'durable-state validation requires a COLAY_HOME path'
    }
    $database = Join-Path $ColayHomePath 'state/state.db'
    if (-not (Test-Path -LiteralPath $database -PathType Leaf)) {
        throw "global database is missing: $database"
    }
    Assert-DatabaseHealth -Database $database -Environment $Environment
    $counts = Invoke-Sqlite -Database $database -WorkingDirectory $script:RunRoot -Environment $Environment `
        -ReadOnly -Csv -Label 'global-cardinality' -Sql @'
SELECT 'workspaces' AS table_name, count(*) AS row_count FROM workspaces
UNION ALL SELECT 'workspace_paths', count(*) FROM workspace_paths
UNION ALL SELECT 'legacy_imports', count(*) FROM legacy_imports
UNION ALL SELECT 'sessions', count(*) FROM sessions;
'@
    $countMap = @{}
    foreach ($row in $counts) { $countMap[[string]$row.table_name] = [int]$row.row_count }
    $expectedImports = $Seeds.Count
    foreach ($expectation in @{
        workspaces = $ExpectedWorkspaceCount
        workspace_paths = $ExpectedWorkspaceCount
        legacy_imports = $expectedImports
        sessions = $expectedImports
    }.GetEnumerator()) {
        if ($countMap[$expectation.Key] -ne $expectation.Value) {
            throw "durable cardinality mismatch for $($expectation.Key): expected $($expectation.Value), found $($countMap[$expectation.Key])"
        }
    }
    $workspaceIdentityRows = Invoke-Sqlite -Database $database -WorkingDirectory $script:RunRoot `
        -Environment $Environment -ReadOnly -Csv -Label 'global-workspace-identities' `
        -Sql 'SELECT workspace_id FROM workspaces ORDER BY workspace_id;'
    if ($workspaceIdentityRows.Count -ne $ExpectedWorkspaceCount) {
        throw "durable workspace identity count mismatch: expected $ExpectedWorkspaceCount, found $($workspaceIdentityRows.Count)"
    }
    $expectedWorkspaceRootPaths = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($workspaceIdentityRow in $workspaceIdentityRows) {
        $workspaceId = [string]$workspaceIdentityRow.workspace_id
        if ([string]::IsNullOrWhiteSpace($workspaceId)) {
            throw 'durable workspace identity was empty'
        }
        $workspaceRootPath = Join-Path (Join-Path $ColayHomePath 'data/workspaces') $workspaceId
        if (-not $expectedWorkspaceRootPaths.Add((ConvertTo-ComparableWindowsPath $workspaceRootPath))) {
            throw "duplicate durable workspace filesystem root: $workspaceRootPath"
        }
    }

    $seedEvidence = [System.Collections.Generic.List[object]]::new()
    $expectedPublicationPaths = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($seed in $Seeds) {
        $sessionId = ([string]$seed.session_id).Replace("'", "''")
        $rows = Invoke-Sqlite -Database $database -WorkingDirectory $script:RunRoot -Environment $Environment `
            -ReadOnly -Csv -Label "durable-workspace-$($seed.index)" -Sql @"
SELECT wp.workspace_id, wp.canonical_path, li.source_fingerprint, li.manifest_hash, li.result_json,
       (SELECT count(*) FROM sessions s WHERE s.workspace_id = wp.workspace_id) AS session_count
FROM sessions selected
JOIN workspace_paths wp ON wp.workspace_id = selected.workspace_id
JOIN legacy_imports li ON li.workspace_id = wp.workspace_id
WHERE wp.is_current = 1 AND selected.session_id = '$sessionId';
"@
        if ($rows.Count -ne 1 -or [int]$rows[0].session_count -ne 1) {
            throw "workspace $($seed.index) lacks one exact durable path/import/session mapping"
        }
        if ((ConvertTo-ComparableWindowsPath ([string]$rows[0].canonical_path)) -cne (ConvertTo-ComparableWindowsPath ([string]$seed.canonical_repository))) {
            throw "workspace $($seed.index) durable path does not match its source repository"
        }
        if ([string]$rows[0].source_fingerprint -notmatch '^[0-9a-f]{64}$' -or [string]$rows[0].manifest_hash -notmatch '^[0-9a-f]{64}$') {
            throw "workspace $($seed.index) has malformed durable import hashes"
        }
        $result = $rows[0].result_json | ConvertFrom-Json -Depth 20
        if (-not $result.imported -or [string]$result.workspace_id -cne [string]$rows[0].workspace_id) {
            throw "workspace $($seed.index) import ledger result does not match its workspace"
        }
        if ([string]$result.source_fingerprint -cne [string]$rows[0].source_fingerprint -or [string]$result.manifest_hash -cne [string]$rows[0].manifest_hash) {
            throw "workspace $($seed.index) indexed import hashes differ from result_json"
        }
        $sourceRootHash = [string]$result.source_root_hash
        if ($sourceRootHash -cnotmatch '^[0-9a-f]{64}$') {
            throw "workspace $($seed.index) result_json has malformed opaque source_root_hash"
        }
        $seed.source_root_hash = $sourceRootHash
        if ($null -ne $seed.source_evidence) {
            $seed.source_evidence.source_root_hash = $sourceRootHash
        }
        $workspaceId = [string]$rows[0].workspace_id
        $sourceFingerprint = [string]$rows[0].source_fingerprint
        $expectedPublishedPath = Join-Path `
            (Join-Path (Join-Path (Join-Path $ColayHomePath 'data/workspaces') $workspaceId) 'imports') `
            $sourceFingerprint
        $publishedPath = [string]$result.published_path
        if ((ConvertTo-ComparableWindowsPath $publishedPath) -cne (ConvertTo-ComparableWindowsPath $expectedPublishedPath)) {
            throw "workspace $($seed.index) result_json publication escaped its exact global workspace fingerprint namespace: expected $expectedPublishedPath, found $publishedPath"
        }
        [void]$expectedPublicationPaths.Add((ConvertTo-ComparableWindowsPath $expectedPublishedPath))
        $publishedDatabase = Assert-ControlledPublicationContents -PublicationPath $expectedPublishedPath `
            -Label "workspace $($seed.index)"
        $seedEvidence.Add([pscustomobject]@{
            index = $seed.index
            workspace_id = $workspaceId
            canonical_path = [string]$rows[0].canonical_path
            source_fingerprint = $sourceFingerprint
            source_root_hash = $sourceRootHash
            manifest_hash = [string]$rows[0].manifest_hash
            published_path = $expectedPublishedPath
            published_hashes = Get-SqliteFamilyHashes $publishedDatabase
        })
    }

    $workspacesRoot = Join-Path $ColayHomePath 'data/workspaces'
    $publicationLayout = Assert-GlobalPublicationLayout -WorkspacesRoot $workspacesRoot `
        -ExpectedWorkspaceRootPaths $expectedWorkspaceRootPaths `
        -ExpectedPublicationPaths $expectedPublicationPaths -ExpectedCount $Seeds.Count
    return [pscustomobject]@{
        counts = $countMap
        seeds = $seedEvidence
        publication_layout = $publicationLayout
    }
}

function Assert-ZeroWritableRows {
    param([Parameter(Mandatory = $true)][string]$Database, [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment)
    $rows = Invoke-Sqlite -Database $Database -WorkingDirectory $script:RunRoot -Environment $Environment `
        -ReadOnly -Csv -Label 'zero-writable-rows' -Sql @'
SELECT 'tasks' AS table_name, count(*) AS row_count FROM tasks
UNION ALL SELECT 'task_attempts', count(*) FROM task_attempts
UNION ALL SELECT 'worktrees', count(*) FROM worktrees
UNION ALL SELECT 'coordinator_leases', count(*) FROM coordinator_leases
UNION ALL SELECT 'worker_leases', count(*) FROM worker_leases;
'@
    $counts = @{}
    foreach ($row in $rows) {
        $counts[[string]$row.table_name] = [int]$row.row_count
        if ([int]$row.row_count -ne 0) {
            throw "writable table $($row.table_name) is not empty: $($row.row_count)"
        }
    }
    return $counts
}

function Invoke-Colay {
    param(
        [Parameter(Mandatory = $true)][string]$Repository,
        [Parameter(Mandatory = $true)][string[]]$ArgumentValues,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label,
        [int]$TimeoutMs = 12000,
        [switch]$AllowFailure,
        [switch]$DeferObservation,
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000
    )
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    if ($boundDeadlineParameterCount -notin @(0, $deadlineParameterNames.Count)) {
        throw 'Colay invocation requires one atomic bounded deadline contract'
    }
    $deadlineArguments = @{}
    if ($boundDeadlineParameterCount -eq $deadlineParameterNames.Count) {
        $deadlineArguments = @{
            OverallDeadlineStopwatch = $OverallDeadlineStopwatch
            OverallDeadlineMs = $OverallDeadlineMs
            ExitWaitLimitMs = $ExitWaitLimitMs
            OutputDrainLimitMs = $OutputDrainLimitMs
        }
    }
    return Invoke-HarnessProcess -Executable $script:ResolvedColay -ArgumentValues $ArgumentValues `
        -WorkingDirectory $Repository -Environment $Environment -Label $Label -TimeoutMs $TimeoutMs `
        -StandardInputText $null -CaptureFirstStdoutLine -AllowFailure:$AllowFailure `
        -DeferObservation:$DeferObservation @deadlineArguments
}

function Assert-StatusJson {
    param($Result)
    if ([string]::IsNullOrWhiteSpace($Result.stdout)) {
        throw "$($Result.label) emitted empty stdout"
    }
    try { return $Result.stdout | ConvertFrom-Json -Depth 30 }
    catch { throw "$($Result.label) did not emit valid JSON: $($_.Exception.Message)" }
}

function ConvertTo-StressDaemonDocumentIdentity {
    param(
        [Parameter(Mandatory = $true)]$Document,
        [Parameter(Mandatory = $true)]
        [ValidateSet('daemon_start', 'daemon_status', IgnoreCase = $false)]
        [string]$ExpectedCommand,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable
    )
    if ($null -eq $Document -or
        $Document.PSObject.Properties.Name -cnotcontains 'schema_version' -or
        $Document.PSObject.Properties.Name -cnotcontains 'command' -or
        $Document.PSObject.Properties.Name -cnotcontains 'data' -or
        $Document.schema_version -isnot [string] -or
        [string]$Document.schema_version -cne '1' -or
        $Document.command -isnot [string] -or
        [string]$Document.command -cne $ExpectedCommand) {
        throw "$ExpectedCommand did not return exact schema-v1 $ExpectedCommand JSON"
    }
    if ($null -eq $Document.data -or
        $Document.data.PSObject.Properties.Name -cnotcontains 'status' -or
        $null -eq $Document.data.status -or
        $Document.data.status.PSObject.Properties.Name -cnotcontains 'state' -or
        $Document.data.status.PSObject.Properties.Name -cnotcontains 'instance') {
        throw "$ExpectedCommand JSON has no exact status identity"
    }
    $status = $Document.data.status
    $instance = $status.instance
    if ($null -eq $instance) { throw "$ExpectedCommand JSON has no exact instance identity" }
    foreach ($propertyName in @('instance_id', 'pid', 'phase', 'executable_path')) {
        if ($instance.PSObject.Properties.Name -cnotcontains $propertyName) {
            throw "$ExpectedCommand instance is missing exact property: $propertyName"
        }
    }
    if ($status.state -isnot [string] -or $instance.phase -isnot [string] -or
        $instance.instance_id -isnot [string] -or $instance.executable_path -isnot [string]) {
        throw "$ExpectedCommand state, phase, instance id, and executable path must be exact JSON strings"
    }
    $state = [string]$status.state
    $phase = [string]$instance.phase
    if ([string]::IsNullOrWhiteSpace($state) -or [string]::IsNullOrWhiteSpace($phase) -or
        $state -cne $phase) {
        throw "$ExpectedCommand state/phase mismatch: state '$state', phase '$phase'"
    }
    $instanceIdText = [string]$instance.instance_id
    try { $instanceId = ([guid]::ParseExact($instanceIdText, 'D')).ToString('D') }
    catch { throw "$ExpectedCommand returned a malformed instance id: $instanceIdText" }
    if ($instanceIdText -cne $instanceId) {
        throw "$ExpectedCommand instance id is not canonical UUID text: $instanceIdText"
    }
    $integralPidTypes = @([byte], [sbyte], [int16], [uint16], [int32], [uint32], [int64], [uint64])
    if ($null -eq $instance.pid -or $integralPidTypes -notcontains $instance.pid.GetType()) {
        $actualPidType = if ($null -eq $instance.pid) { 'null' } else { $instance.pid.GetType().FullName }
        throw "$ExpectedCommand PID is not an exact JSON integer: $actualPidType"
    }
    $rawPid = [int64]$instance.pid
    if ($rawPid -le 0 -or $rawPid -gt [uint32]::MaxValue -or $rawPid -eq $PID) {
        throw "$ExpectedCommand returned an unsafe process id: $rawPid"
    }
    $executableText = [string]$instance.executable_path
    if ([string]::IsNullOrWhiteSpace($executableText) -or
        -not [System.IO.Path]::IsPathFullyQualified($executableText)) {
        throw "$ExpectedCommand executable path is not an exact absolute path: $executableText"
    }
    $actualPath = ConvertTo-NormalizedExecutablePath $executableText
    $expectedPath = ConvertTo-NormalizedExecutablePath $ExpectedExecutable
    if (-not $actualPath.Equals($expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$ExpectedCommand executable path mismatch: expected $expectedPath, found $actualPath"
    }
    return [pscustomobject][ordered]@{
        Document = $Document
        Command = [string]$Document.command
        State = $state
        Phase = $phase
        InstanceId = $instanceId
        ProcessId = [uint32]$rawPid
        ExecutablePath = $actualPath
    }
}

function Assert-StressDaemonReadinessDeadline {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.Stopwatch]$Stopwatch,
        [Parameter(Mandatory = $true)][ValidateRange(1, [int]::MaxValue)][int]$OverallTimeoutMs,
        [Parameter(Mandatory = $true)][string]$Scope
    )
    if ((Get-MonotonicElapsedCeilingMs -Stopwatch $Stopwatch) -ge $OverallTimeoutMs) {
        throw "$Scope daemon readiness timed out after ${OverallTimeoutMs}ms"
    }
}

function Wait-MainDaemonReadiness {
    param(
        [Parameter(Mandatory = $true)]$DaemonStartDocument,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable,
        [Parameter(Mandatory = $true)][string]$Repository,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $evidenceKey = 'ColayStressMainDaemonReadinessEvidence'
    $polls = [System.Collections.Generic.List[object]]::new()
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $cleanupBudgetMs = $script:MainDaemonReadinessExitWaitLimitMs +
        $script:MainDaemonReadinessOutputDrainLimitMs
    $evidence = [pscustomobject][ordered]@{
        readiness_status = 'failed'
        original_state = $null
        final_state = $null
        poll_count = 0
        elapsed_ms = 0
        overall_timeout_ms = $script:MainDaemonReadinessTimeoutMs
        poll_interval_ms = $script:MainDaemonReadinessPollIntervalMs
        exit_wait_limit_ms = $script:MainDaemonReadinessExitWaitLimitMs
        output_drain_limit_ms = $script:MainDaemonReadinessOutputDrainLimitMs
        cleanup_reserve_ms = $cleanupBudgetMs
        status_command = @('--json', 'daemon', 'status')
        timing_included_in_latency_thresholds = $false
        anchored_identity = $null
        polls = @()
        online_document = $null
        failure = $null
    }
    try {
        if ($script:MainDaemonReadinessInitialParseDelayForTestMs -gt 0) {
            Start-Sleep -Milliseconds $script:MainDaemonReadinessInitialParseDelayForTestMs
        }
        [void](Assert-StressDaemonReadinessDeadline -Stopwatch $stopwatch `
            -OverallTimeoutMs $script:MainDaemonReadinessTimeoutMs -Scope 'main')
        $anchor = ConvertTo-StressDaemonDocumentIdentity -Document $DaemonStartDocument `
            -ExpectedCommand daemon_start -ExpectedExecutable $ExpectedExecutable
        $evidence.original_state = $anchor.State
        $evidence.final_state = $anchor.State
        $evidence.anchored_identity = [pscustomobject][ordered]@{
            instance_id = $anchor.InstanceId
            process_id = [int64]$anchor.ProcessId
            executable_path = $anchor.ExecutablePath
        }
        if (@('booting', 'probing', 'online') -cnotcontains $anchor.State) {
            throw "main daemon readiness start returned terminal or non-progress state '$($anchor.State)'"
        }
        if ($anchor.State -ceq 'online') {
            [void](Assert-StressDaemonReadinessDeadline -Stopwatch $stopwatch `
                -OverallTimeoutMs $script:MainDaemonReadinessTimeoutMs -Scope 'main')
            $evidence.readiness_status = 'online'
            $evidence.online_document = $DaemonStartDocument
            $evidence.elapsed_ms = Get-MonotonicElapsedCeilingMs -Stopwatch $stopwatch
            return [pscustomobject][ordered]@{ Evidence = $evidence; OnlineDocument = $DaemonStartDocument }
        }
        while ($true) {
            $remainingBeforeSleepMs = $script:MainDaemonReadinessTimeoutMs -
                (Get-MonotonicElapsedCeilingMs -Stopwatch $stopwatch)
            $sleepBudgetMs = $remainingBeforeSleepMs - $cleanupBudgetMs
            if ($sleepBudgetMs -le 0) {
                throw "main daemon readiness timed out after $($script:MainDaemonReadinessTimeoutMs)ms"
            }
            $sleepMs = [int][Math]::Min($script:MainDaemonReadinessPollIntervalMs, $sleepBudgetMs)
            Start-Sleep -Milliseconds $sleepMs
            $remainingMs = $script:MainDaemonReadinessTimeoutMs -
                (Get-MonotonicElapsedCeilingMs -Stopwatch $stopwatch)
            $commandBudgetMs = [int]($remainingMs - $cleanupBudgetMs)
            if ($commandBudgetMs -le 0) {
                throw "main daemon readiness timed out after $($script:MainDaemonReadinessTimeoutMs)ms"
            }
            $pollNumber = $polls.Count + 1
            $commandLabel = "$Label-daemon-readiness-{0:D3}" -f $pollNumber
            $pollEvidence = [pscustomobject][ordered]@{
                poll = $pollNumber
                command_label = $commandLabel
                remaining_at_launch_ms = $remainingMs
                command_timeout_ms = $commandBudgetMs
                exit_wait_limit_ms = $script:MainDaemonReadinessExitWaitLimitMs
                output_drain_limit_ms = $script:MainDaemonReadinessOutputDrainLimitMs
                total_operation_budget_ms = $commandBudgetMs + $cleanupBudgetMs
                observed_elapsed_ms = Get-MonotonicElapsedCeilingMs -Stopwatch $stopwatch
                state = $null
                phase = $null
                instance_id = $null
                process_id = $null
                executable_path = $null
            }
            $polls.Add($pollEvidence)
            $evidence.poll_count = $polls.Count
            $evidence.polls = $polls.ToArray()
            try {
                $statusResult = Invoke-Colay -Repository $Repository `
                    -ArgumentValues @('--json', 'daemon', 'status') -Environment $Environment `
                    -Label $commandLabel -TimeoutMs $commandBudgetMs -DeferObservation `
                    -OverallDeadlineStopwatch $stopwatch `
                    -OverallDeadlineMs $script:MainDaemonReadinessTimeoutMs `
                    -ExitWaitLimitMs $script:MainDaemonReadinessExitWaitLimitMs `
                    -OutputDrainLimitMs $script:MainDaemonReadinessOutputDrainLimitMs
            } catch {
                $commandEvidence = @($script:CommandEvidence | Where-Object {
                    [string]$_.label -ceq $commandLabel -and $null -ne $_.deadline
                } | Select-Object -Last 1)
                if ($commandEvidence.Count -eq 1) {
                    $pollEvidence.remaining_at_launch_ms = [int64]$commandEvidence[0].deadline.remaining_at_launch_ms
                    $pollEvidence.command_timeout_ms = [int]$commandEvidence[0].deadline.command_timeout_ms
                    $pollEvidence.exit_wait_limit_ms = [int]$commandEvidence[0].deadline.exit_wait_limit_ms
                    $pollEvidence.output_drain_limit_ms = [int]$commandEvidence[0].deadline.output_drain_limit_ms
                    $pollEvidence.total_operation_budget_ms = [int]$commandEvidence[0].deadline.total_operation_budget_ms
                }
                throw
            }
            if ($null -ne $statusResult.deadline) {
                $pollEvidence.remaining_at_launch_ms = [int64]$statusResult.deadline.remaining_at_launch_ms
                $pollEvidence.command_timeout_ms = [int]$statusResult.deadline.command_timeout_ms
                $pollEvidence.exit_wait_limit_ms = [int]$statusResult.deadline.exit_wait_limit_ms
                $pollEvidence.output_drain_limit_ms = [int]$statusResult.deadline.output_drain_limit_ms
                $pollEvidence.total_operation_budget_ms = [int]$statusResult.deadline.total_operation_budget_ms
            }
            [void](Assert-StressDaemonReadinessDeadline -Stopwatch $stopwatch `
                -OverallTimeoutMs $script:MainDaemonReadinessTimeoutMs -Scope 'main')
            $statusDocument = Assert-StatusJson $statusResult
            $statusIdentity = ConvertTo-StressDaemonDocumentIdentity -Document $statusDocument `
                -ExpectedCommand daemon_status -ExpectedExecutable $ExpectedExecutable
            $pollEvidence.observed_elapsed_ms = Get-MonotonicElapsedCeilingMs -Stopwatch $stopwatch
            $pollEvidence.state = $statusIdentity.State
            $pollEvidence.phase = $statusIdentity.Phase
            $pollEvidence.instance_id = $statusIdentity.InstanceId
            $pollEvidence.process_id = [int64]$statusIdentity.ProcessId
            $pollEvidence.executable_path = $statusIdentity.ExecutablePath
            if ($statusIdentity.InstanceId -cne $anchor.InstanceId -or
                $statusIdentity.ProcessId -ne $anchor.ProcessId -or
                -not $statusIdentity.ExecutablePath.Equals($anchor.ExecutablePath, [StringComparison]::OrdinalIgnoreCase)) {
                throw "main daemon readiness identity drift at status poll $pollNumber"
            }
            [void](Assert-StressDaemonReadinessDeadline -Stopwatch $stopwatch `
                -OverallTimeoutMs $script:MainDaemonReadinessTimeoutMs -Scope 'main')
            $evidence.final_state = $statusIdentity.State
            if ($statusIdentity.State -ceq 'online') {
                $evidence.readiness_status = 'online'
                $evidence.online_document = $statusDocument
                $evidence.elapsed_ms = Get-MonotonicElapsedCeilingMs -Stopwatch $stopwatch
                return [pscustomobject][ordered]@{ Evidence = $evidence; OnlineDocument = $statusDocument }
            }
            if (@('booting', 'probing') -cnotcontains $statusIdentity.State) {
                throw "main daemon readiness status poll $pollNumber returned terminal or non-progress state '$($statusIdentity.State)'"
            }
        }
    } catch {
        $evidence.poll_count = $polls.Count
        $evidence.polls = $polls.ToArray()
        $evidence.elapsed_ms = Get-MonotonicElapsedCeilingMs -Stopwatch $stopwatch
        $evidence.failure = $_.Exception.Message
        $_.Exception.Data[$evidenceKey] = $evidence
        throw
    } finally {
        $stopwatch.Stop()
    }
}

function Assert-AuditDaemonReadinessEvidence {
    param(
        [Parameter(Mandatory = $true)]$ReadinessEvidence,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable,
        [Parameter(Mandatory = $true)][ValidateNotNullOrEmpty()][string]$ExpectedLabel,
        [ValidateRange(1, [int]::MaxValue)][int]$ExpectedOverallTimeoutMs = 5000,
        [ValidateRange(1, [int]::MaxValue)][int]$ExpectedPollIntervalMs = 50,
        [ValidateRange(0, [int]::MaxValue)][int]$ExpectedExitWaitLimitMs = 400,
        [ValidateRange(0, [int]::MaxValue)][int]$ExpectedOutputDrainLimitMs = 100
    )
    $integralTypes = @([byte], [sbyte], [int16], [uint16], [int32], [uint32], [int64], [uint64])
    if ($null -eq $ReadinessEvidence -or $ReadinessEvidence -is [string]) {
        throw 'audit child readiness evidence is missing or has a truncated scalar type'
    }
    foreach ($propertyName in @(
            'readiness_status', 'original_state', 'final_state', 'poll_count', 'elapsed_ms',
            'overall_timeout_ms', 'poll_interval_ms', 'cleanup_reserve_ms', 'exit_wait_limit_ms',
            'output_drain_limit_ms', 'status_command', 'anchored_identity', 'polls',
            'online_document', 'failure'
        )) {
        if ($ReadinessEvidence.PSObject.Properties.Name -cnotcontains $propertyName) {
            throw "audit child readiness evidence is missing exact property: $propertyName"
        }
    }
    if ($ReadinessEvidence.readiness_status -isnot [string] -or
        [string]$ReadinessEvidence.readiness_status -cne 'online' -or
        $ReadinessEvidence.original_state -isnot [string] -or
        @('booting', 'probing', 'online') -cnotcontains [string]$ReadinessEvidence.original_state -or
        $ReadinessEvidence.final_state -isnot [string] -or
        [string]$ReadinessEvidence.final_state -cne 'online' -or
        $null -ne $ReadinessEvidence.failure) {
        throw 'audit child readiness did not prove an exact failure-free online state'
    }
    foreach ($numericProperty in @(
            'poll_count', 'elapsed_ms', 'overall_timeout_ms', 'poll_interval_ms', 'cleanup_reserve_ms',
            'exit_wait_limit_ms', 'output_drain_limit_ms'
        )) {
        $numericValue = $ReadinessEvidence.$numericProperty
        if ($null -eq $numericValue -or $integralTypes -notcontains $numericValue.GetType()) {
            throw "audit child readiness $numericProperty is not an exact JSON integer"
        }
    }
    $overallTimeoutMs = [int64]$ReadinessEvidence.overall_timeout_ms
    $elapsedMs = [int64]$ReadinessEvidence.elapsed_ms
    $pollCount = [int64]$ReadinessEvidence.poll_count
    $pollIntervalMs = [int64]$ReadinessEvidence.poll_interval_ms
    $exitWaitLimitMs = [int64]$ReadinessEvidence.exit_wait_limit_ms
    $outputDrainLimitMs = [int64]$ReadinessEvidence.output_drain_limit_ms
    if ($overallTimeoutMs -ne $ExpectedOverallTimeoutMs -or $elapsedMs -lt 0 -or
        $elapsedMs -ge $overallTimeoutMs -or $pollCount -lt 0 -or
        $pollIntervalMs -ne $ExpectedPollIntervalMs -or
        $exitWaitLimitMs -ne $ExpectedExitWaitLimitMs -or
        $outputDrainLimitMs -ne $ExpectedOutputDrainLimitMs -or
        [int64]$ReadinessEvidence.cleanup_reserve_ms -ne
            ($ExpectedExitWaitLimitMs + $ExpectedOutputDrainLimitMs)) {
        throw 'audit child readiness deadline or cleanup evidence is out of bounds'
    }
    if ($ReadinessEvidence.status_command -isnot [array]) {
        throw 'audit child readiness status command is not an exact JSON array'
    }
    $statusCommand = @($ReadinessEvidence.status_command)
    if ($statusCommand.Count -ne 3 -or [string]$statusCommand[0] -cne '--json' -or
        [string]$statusCommand[1] -cne 'daemon' -or [string]$statusCommand[2] -cne 'status') {
        throw 'audit child readiness status command was not exact separated daemon status arguments'
    }
    $anchor = $ReadinessEvidence.anchored_identity
    if ($null -eq $anchor -or $anchor -is [string]) {
        throw 'audit child readiness anchored identity is missing or truncated'
    }
    foreach ($propertyName in @('instance_id', 'process_id', 'executable_path')) {
        if ($anchor.PSObject.Properties.Name -cnotcontains $propertyName) {
            throw "audit child readiness anchored identity is missing exact property: $propertyName"
        }
    }
    if ($anchor.instance_id -isnot [string] -or $anchor.executable_path -isnot [string] -or
        $null -eq $anchor.process_id -or $integralTypes -notcontains $anchor.process_id.GetType()) {
        throw 'audit child readiness anchored identity has a wrong JSON type'
    }
    $anchorIdText = [string]$anchor.instance_id
    try { $anchorId = ([guid]::ParseExact($anchorIdText, 'D')).ToString('D') }
    catch { throw 'audit child readiness anchored identity has a malformed UUID' }
    if ($anchorIdText -cne $anchorId) {
        throw 'audit child readiness anchored identity UUID is not canonical'
    }
    $anchorPid = [int64]$anchor.process_id
    if ($anchorPid -le 0 -or $anchorPid -gt [uint32]::MaxValue -or $anchorPid -eq $PID) {
        throw 'audit child readiness anchored PID is unsafe'
    }
    $anchorPath = ConvertTo-NormalizedExecutablePath ([string]$anchor.executable_path)
    $expectedPath = ConvertTo-NormalizedExecutablePath $ExpectedExecutable
    if (-not $anchorPath.Equals($expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'audit child readiness anchored executable path mismatch'
    }
    if ($ReadinessEvidence.polls -isnot [array]) {
        throw 'audit child readiness polls are not an exact JSON array'
    }
    $polls = @($ReadinessEvidence.polls)
    if ($polls.Count -ne $pollCount) {
        throw 'audit child readiness poll_count does not match polls cardinality'
    }
    if (($pollCount -eq 0 -and [string]$ReadinessEvidence.original_state -cne 'online') -or
        ($pollCount -gt 0 -and [string]$ReadinessEvidence.original_state -cnotin @('booting', 'probing'))) {
        throw 'audit child readiness original state does not match its poll transition'
    }
    [int64]$previousObservedElapsedMs = -1
    for ($index = 0; $index -lt $polls.Count; $index++) {
        $poll = $polls[$index]
        if ($null -eq $poll -or $poll -is [string]) {
            throw "audit child readiness poll $($index + 1) is missing or truncated"
        }
        foreach ($propertyName in @(
                'poll', 'command_label', 'remaining_at_launch_ms', 'command_timeout_ms',
                'exit_wait_limit_ms', 'output_drain_limit_ms', 'total_operation_budget_ms',
                'observed_elapsed_ms', 'state', 'phase', 'instance_id', 'process_id', 'executable_path'
            )) {
            if ($poll.PSObject.Properties.Name -cnotcontains $propertyName) {
                throw "audit child readiness poll $($index + 1) is missing exact property: $propertyName"
            }
        }
        foreach ($numericProperty in @(
                'poll', 'remaining_at_launch_ms', 'command_timeout_ms', 'exit_wait_limit_ms',
                'output_drain_limit_ms', 'total_operation_budget_ms', 'observed_elapsed_ms', 'process_id'
            )) {
            $numericValue = $poll.$numericProperty
            if ($null -eq $numericValue -or $integralTypes -notcontains $numericValue.GetType()) {
                throw "audit child readiness poll $($index + 1) $numericProperty is not an exact JSON integer"
            }
        }
        $remainingAtLaunchMs = [int64]$poll.remaining_at_launch_ms
        $commandTimeoutMs = [int64]$poll.command_timeout_ms
        $pollExitMs = [int64]$poll.exit_wait_limit_ms
        $pollDrainMs = [int64]$poll.output_drain_limit_ms
        $totalOperationBudgetMs = [int64]$poll.total_operation_budget_ms
        $observedElapsedMs = [int64]$poll.observed_elapsed_ms
        if ([int64]$poll.poll -ne ($index + 1) -or $remainingAtLaunchMs -le 0 -or
            $remainingAtLaunchMs -gt $overallTimeoutMs -or $commandTimeoutMs -le 0 -or
            $pollExitMs -ne $exitWaitLimitMs -or $pollDrainMs -ne $outputDrainLimitMs -or
            $totalOperationBudgetMs -ne ($commandTimeoutMs + $pollExitMs + $pollDrainMs) -or
            $totalOperationBudgetMs -gt $remainingAtLaunchMs -or
            $observedElapsedMs -lt 0 -or $observedElapsedMs -ge $overallTimeoutMs -or
            $observedElapsedMs -lt $previousObservedElapsedMs -or $observedElapsedMs -gt $elapsedMs) {
            throw "audit child readiness poll $($index + 1) exceeded its launch or cleanup budget"
        }
        $previousObservedElapsedMs = $observedElapsedMs
        if ($poll.command_label -isnot [string] -or
            [string]$poll.command_label -cne
                ("$ExpectedLabel-daemon-readiness-{0:D3}" -f ($index + 1)) -or
            $poll.state -isnot [string] -or $poll.phase -isnot [string] -or
            [string]$poll.state -cne [string]$poll.phase -or
            @('booting', 'probing', 'online') -cnotcontains [string]$poll.state) {
            throw "audit child readiness poll $($index + 1) has an invalid state/phase or command label"
        }
        if (($index -lt ($polls.Count - 1) -and [string]$poll.state -ceq 'online') -or
            ($index -eq ($polls.Count - 1) -and [string]$poll.state -cne 'online')) {
            throw 'audit child readiness poll sequence did not terminate exactly at online'
        }
        if ($poll.instance_id -isnot [string] -or [string]$poll.instance_id -cne $anchorId -or
            [int64]$poll.process_id -ne $anchorPid -or $poll.executable_path -isnot [string] -or
            -not (ConvertTo-NormalizedExecutablePath ([string]$poll.executable_path)).Equals(
                $anchorPath,
                [StringComparison]::OrdinalIgnoreCase
            )) {
            throw "audit child readiness poll $($index + 1) identity drift"
        }
    }
    $onlineExpectedCommand = if ($pollCount -eq 0) { 'daemon_start' } else { 'daemon_status' }
    $onlineIdentity = ConvertTo-StressDaemonDocumentIdentity -Document $ReadinessEvidence.online_document `
        -ExpectedCommand $onlineExpectedCommand -ExpectedExecutable $ExpectedExecutable
    if ($onlineIdentity.State -cne 'online' -or $onlineIdentity.Phase -cne 'online' -or
        $onlineIdentity.InstanceId -cne $anchorId -or $onlineIdentity.ProcessId -ne $anchorPid -or
        -not $onlineIdentity.ExecutablePath.Equals($anchorPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'audit child readiness online document did not preserve anchored identity'
    }
    return $ReadinessEvidence
}

function Get-LiveAttributedProcesses {
    Update-ProcessObservation
    $live = [System.Collections.Generic.List[object]]::new()
    foreach ($snapshotRow in $script:LastProcessSnapshot) {
        if ($script:AmbiguousSnapshotPids.Contains([int]$snapshotRow.process_id)) { continue }
        $matches = @($script:OwnedProcessIdentities | Where-Object {
            Test-SnapshotMatchesOwnedIdentity -SnapshotRow $snapshotRow -Identity $_
        })
        if ($matches.Count -ne 1) { continue }
        $identity = $matches[0]
        $live.Add([pscustomobject][ordered]@{
            identity_key = [string]$identity.identity_key
            process_id = [int]$identity.process_id
            parent_process_id = [int]$identity.parent_process_id
            parent_identity_key = [string]$identity.parent_identity_key
            parent_chain = @($identity.parent_chain)
            creation_time_utc = ([datetime]$identity.creation_time_utc).ToString('o')
            executable_path = [string]$identity.executable_path
            name = [string]$identity.name
            source = [string]$identity.source
            depth = [int]$identity.depth
            current_snapshot_identity_verified = $true
        })
    }
    return $live.ToArray()
}

function Stop-AttributedProcesses {
    param([Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$Processes)
    $wall = [System.Diagnostics.Stopwatch]::StartNew()
    $errors = [System.Collections.Generic.List[string]]::new()
    $opened = [System.Collections.Generic.List[object]]::new()
    $rows = [System.Collections.Generic.List[object]]::new()
    $evidence = [pscustomobject][ordered]@{
        attempted = $Processes.Count -ne 0
        candidate_count = $Processes.Count
        preflight_all_before_mutation = $true
        same_handle_identity_check_and_termination = $true
        wait_limit_ms = 2000
        opened_handle_count = 0
        closed_handle_count = 0
        close_error_count = 0
        verified_candidate_count = 0
        terminate_call_count = 0
        exit_confirmed_count = 0
        refused_reason = $null
        processes = @()
        handles_disposed = $false
        wall_ms = $null
        errors = @()
    }
    try {
        Initialize-OwnedDescendantNativeApi
        $processAccess = [uint32](0x0001 -bor 0x1000 -bor 0x100000)
        $seenIdentityKeys = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($processInfo in @($Processes | Sort-Object -Property @{ Expression = 'depth'; Descending = $false },
                @{ Expression = 'process_id'; Descending = $false })) {
            $row = [pscustomobject][ordered]@{
                identity_key = [string]$processInfo.identity_key
                process_id = [int]$processInfo.process_id
                parent_process_id = [int]$processInfo.parent_process_id
                parent_identity_key = [string]$processInfo.parent_identity_key
                parent_chain = @($processInfo.parent_chain)
                expected_creation_time_utc = [string]$processInfo.creation_time_utc
                expected_executable_path = [string]$processInfo.executable_path
                native_creation_time_utc = $null
                native_exit_time_utc = $null
                native_executable_path = $null
                handle_opened = $false
                creation_identity_verified = $false
                executable_path_verified = $false
                identity_verified = $false
                initial_wait_result = $null
                terminate_called = $false
                exit_confirmed = $false
                handle_closed = $false
                refusal = $null
            }
            $rows.Add($row)
            if ([int]$processInfo.process_id -le 0 -or [int]$processInfo.process_id -eq $PID -or
                [string]::IsNullOrWhiteSpace([string]$processInfo.identity_key) -or
                [string]::IsNullOrWhiteSpace([string]$processInfo.executable_path) -or
                -not $seenIdentityKeys.Add([string]$processInfo.identity_key)) {
                $row.refusal = 'unsafe, incomplete, or duplicate candidate identity'
                $evidence.refused_reason = $row.refusal
                break
            }
            $expectedCreation = ConvertTo-NormalizedProcessCreationUtc $processInfo.creation_time_utc
            $expectedPath = ConvertTo-NormalizedExecutablePath ([string]$processInfo.executable_path)
            $registryMatches = @($script:OwnedProcessIdentities | Where-Object {
                [string]$_.identity_key -ceq [string]$processInfo.identity_key -and
                [int]$_.process_id -eq [int]$processInfo.process_id -and
                [datetime]$_.creation_time_utc -eq $expectedCreation -and
                ([string]$_.executable_path).Equals($expectedPath, [StringComparison]::OrdinalIgnoreCase)
            })
            if ($registryMatches.Count -ne 1) {
                $row.refusal = 'candidate did not exactly match one registered owned identity'
                $evidence.refused_reason = $row.refusal
                break
            }
            $registeredIdentity = $registryMatches[0]
            $registeredChain = @($registeredIdentity.parent_chain)
            $providedChain = @($processInfo.parent_chain)
            $chainMatches = $registeredChain.Count -eq $providedChain.Count
            if ($chainMatches) {
                for ($chainIndex = 0; $chainIndex -lt $registeredChain.Count; $chainIndex++) {
                    if ([string]$registeredChain[$chainIndex] -cne [string]$providedChain[$chainIndex]) {
                        $chainMatches = $false
                        break
                    }
                }
            }
            if ([int]$processInfo.parent_process_id -ne [int]$registeredIdentity.parent_process_id -or
                [string]$processInfo.parent_identity_key -cne [string]$registeredIdentity.parent_identity_key -or
                [int]$processInfo.depth -ne [int]$registeredIdentity.depth -or -not $chainMatches) {
                $row.refusal = 'candidate parent identity chain did not match the registered owned identity'
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            }
            $row.parent_process_id = [int]$registeredIdentity.parent_process_id
            $row.parent_identity_key = [string]$registeredIdentity.parent_identity_key
            $row.parent_chain = @($registeredIdentity.parent_chain)

            $handle = [ColayOwnedDescendantNativeApi]::OpenProcess(
                $processAccess, $false, [uint32]$processInfo.process_id
            )
            if ($handle -eq [IntPtr]::Zero) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $row.refusal = "OpenProcess failed with Win32 error $errorCode"
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            }
            $row.handle_opened = $true
            $entry = [pscustomobject]@{
                handle = $handle
                row = $row
                registered_identity = $registeredIdentity
            }
            $opened.Add($entry)
            $evidence.opened_handle_count = $opened.Count

            $creationFileTime = [long]0
            $exitFileTime = [long]0
            $kernelFileTime = [long]0
            $userFileTime = [long]0
            if (-not [ColayOwnedDescendantNativeApi]::GetProcessTimes(
                    $handle,
                    [ref]$creationFileTime,
                    [ref]$exitFileTime,
                    [ref]$kernelFileTime,
                    [ref]$userFileTime)) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $row.refusal = "GetProcessTimes failed with Win32 error $errorCode"
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            }
            $nativeCreation = ConvertTo-NormalizedProcessCreationUtc ([datetime]::FromFileTimeUtc($creationFileTime))
            $row.native_creation_time_utc = $nativeCreation.ToString('o')
            if ($nativeCreation -ne $expectedCreation) {
                $row.refusal = 'native creation identity did not match the registered snapshot identity'
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            }
            $row.creation_identity_verified = $true

            $pathBuffer = [Text.StringBuilder]::new(32768)
            $pathLength = [uint32]$pathBuffer.Capacity
            if (-not [ColayOwnedDescendantNativeApi]::QueryFullProcessImageName(
                    $handle, 0, $pathBuffer, [ref]$pathLength)) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $row.refusal = "QueryFullProcessImageName failed with Win32 error $errorCode"
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            }
            $nativePath = ConvertTo-NormalizedExecutablePath $pathBuffer.ToString()
            $row.native_executable_path = $nativePath
            if (-not $nativePath.Equals($expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
                $row.refusal = 'native executable path did not match the registered snapshot identity'
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            }
            $row.executable_path_verified = $true
            $row.identity_verified = $true
            $waitResult = [ColayOwnedDescendantNativeApi]::WaitForSingleObject($handle, 0)
            if ($waitResult -eq 0) {
                $row.initial_wait_result = 'signaled'
            } elseif ($waitResult -eq 0x00000102) {
                $row.initial_wait_result = 'timeout-live'
            } elseif ($waitResult -eq [uint32]::MaxValue) {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $row.refusal = "initial WaitForSingleObject failed with Win32 error $errorCode"
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            } else {
                $row.refusal = "initial WaitForSingleObject returned 0x$($waitResult.ToString('x8'))"
                $evidence.refused_reason = "candidate $($processInfo.process_id) $($row.refusal)"
                break
            }
            $evidence.verified_candidate_count = [int]$evidence.verified_candidate_count + 1
        }

        if ($null -eq $evidence.refused_reason -and
            $evidence.verified_candidate_count -eq $Processes.Count) {
            $waitWall = [System.Diagnostics.Stopwatch]::StartNew()
            foreach ($entry in $opened) {
                if ($entry.row.initial_wait_result -ceq 'timeout-live') {
                    if (-not [ColayOwnedDescendantNativeApi]::TerminateProcess($entry.handle, 1)) {
                        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                        $errors.Add("candidate $($entry.row.process_id) TerminateProcess failed with Win32 error $errorCode")
                    } else {
                        $entry.row.terminate_called = $true
                        $evidence.terminate_call_count = [int]$evidence.terminate_call_count + 1
                    }
                }
                $remainingMs = [math]::Max(0, 2000 - [int]$waitWall.ElapsedMilliseconds)
                $waitResult = [ColayOwnedDescendantNativeApi]::WaitForSingleObject(
                    $entry.handle, [uint32]$remainingMs
                )
                if ($waitResult -eq 0) {
                    $entry.row.exit_confirmed = $true
                    $evidence.exit_confirmed_count = [int]$evidence.exit_confirmed_count + 1
                    $creationFileTime = [long]0
                    $exitFileTime = [long]0
                    $kernelFileTime = [long]0
                    $userFileTime = [long]0
                    if ([ColayOwnedDescendantNativeApi]::GetProcessTimes(
                            $entry.handle,
                            [ref]$creationFileTime,
                            [ref]$exitFileTime,
                            [ref]$kernelFileTime,
                            [ref]$userFileTime) -and $exitFileTime -gt 0) {
                        $nativeExit = ConvertTo-NormalizedProcessCreationUtc ([datetime]::FromFileTimeUtc($exitFileTime))
                        $entry.row.native_exit_time_utc = $nativeExit.ToString('o')
                        Set-OwnedProcessIdentityExit -Identity $entry.registered_identity -ExitTimeUtc $nativeExit
                    } else {
                        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                        $errors.Add("candidate $($entry.row.process_id) exit-time evidence read failed with Win32 error $errorCode")
                    }
                } elseif ($waitResult -eq [uint32]::MaxValue) {
                    $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                    $errors.Add("candidate $($entry.row.process_id) bounded wait failed with Win32 error $errorCode")
                } else {
                    $errors.Add("candidate $($entry.row.process_id) did not exit within the shared 2000ms limit")
                }
            }
        }
    } catch {
        $errors.Add("verified force cleanup failed: $($_.Exception.Message)")
    } finally {
        foreach ($entry in $opened) {
            if ([ColayOwnedDescendantNativeApi]::CloseHandle($entry.handle)) {
                $entry.row.handle_closed = $true
                $evidence.closed_handle_count = [int]$evidence.closed_handle_count + 1
            } else {
                $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $errors.Add("candidate $($entry.row.process_id) CloseHandle failed with Win32 error $errorCode")
                $evidence.close_error_count = [int]$evidence.close_error_count + 1
            }
        }
        $wall.Stop()
        $evidence.processes = $rows.ToArray()
        $evidence.handles_disposed = $evidence.opened_handle_count -eq $evidence.closed_handle_count -and
            $evidence.close_error_count -eq 0
        $evidence.wall_ms = [int64]$wall.ElapsedMilliseconds
        $evidence.errors = $errors.ToArray()
        $script:ForcedProcessCleanupEvidence.Add($evidence)
    }
    return $evidence
}

function Add-CleanupFailure {
    param(
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Summary,
        [Parameter(Mandatory = $true)][string]$Stage,
        [Parameter(Mandatory = $true)]$Failure
    )
    if ($Failure -is [System.Management.Automation.ErrorRecord]) {
        $message = $Failure.Exception.Message
        $category = [string]$Failure.CategoryInfo.Category
        $scriptStack = $Failure.ScriptStackTrace
    } elseif ($Failure -is [System.Exception]) {
        $message = $Failure.Message
        $category = $Failure.GetType().FullName
        $scriptStack = $null
    } else {
        $message = [string]$Failure
        $category = 'CleanupFailure'
        $scriptStack = $null
    }
    $script:CleanupErrors.Add([pscustomobject][ordered]@{
        stage = $Stage
        message = $message
        category = $category
        script_stack = $scriptStack
        observed_at_utc = [datetime]::UtcNow.ToString('o')
    })
    $Summary.cleanup_errors = $script:CleanupErrors.ToArray()
}

function Assert-VerifiedEarlyFailureInputEvidence {
    param(
        [Parameter(Mandatory = $true)]$CandidateSummary,
        [Parameter(Mandatory = $true)][string]$ExpectedHarnessPath,
        [Parameter(Mandatory = $true)][string]$ExpectedColayPath,
        [Parameter(Mandatory = $true)][string]$ExpectedFakeProviderPath,
        [Parameter(Mandatory = $true)][string]$ExpectedSourceCommit
    )
    if ($null -eq $CandidateSummary.harness) {
        throw 'synthetic early-failure evidence omitted the harness identity'
    }
    $expectedHarness = [System.IO.Path]::GetFullPath($ExpectedHarnessPath)
    $actualHarness = [System.IO.Path]::GetFullPath([string]$CandidateSummary.harness.path)
    if (-not $actualHarness.Equals($expectedHarness, [System.StringComparison]::OrdinalIgnoreCase) -or
        [string]$CandidateSummary.harness.sha256 -cne (Get-Sha256 $expectedHarness) -or
        [string]$CandidateSummary.harness.sha256 -cnotmatch '^[0-9a-f]{64}$') {
        throw 'synthetic early-failure evidence recorded the wrong harness identity'
    }
    if ($null -eq $CandidateSummary.binaries) {
        throw 'synthetic early-failure evidence omitted binary identities'
    }
    $expectedInputs = @(
        [pscustomobject]@{ name = 'colay'; path = $ExpectedColayPath },
        [pscustomobject]@{ name = 'fake_provider'; path = $ExpectedFakeProviderPath }
    )
    $verifiedInputs = [System.Collections.Generic.List[object]]::new()
    foreach ($expectedInput in $expectedInputs) {
        $actualInput = $CandidateSummary.binaries.([string]$expectedInput.name)
        if ($null -eq $actualInput) {
            throw "synthetic early-failure evidence omitted $($expectedInput.name) identity"
        }
        $expectedPath = [System.IO.Path]::GetFullPath([string]$expectedInput.path)
        $actualPath = [System.IO.Path]::GetFullPath([string]$actualInput.path)
        if (-not $actualPath.Equals($expectedPath, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "synthetic early-failure evidence recorded the wrong $($expectedInput.name) path"
        }
        $expectedSha256 = Get-Sha256 $expectedPath
        if ([string]$actualInput.sha256 -cne $expectedSha256 -or
            [string]$actualInput.sha256 -cnotmatch '^[0-9a-f]{64}$') {
            throw "synthetic early-failure evidence recorded the wrong $($expectedInput.name) SHA-256"
        }
        $verifiedInputs.Add([pscustomobject][ordered]@{
            name = [string]$expectedInput.name
            path = $actualPath
            sha256 = [string]$actualInput.sha256
        })
    }
    $sourceCommit = [string]$CandidateSummary.source_commit
    if ($sourceCommit -cne $ExpectedSourceCommit -or $sourceCommit -cnotmatch '^[0-9a-f]{40}$') {
        throw "synthetic early-failure evidence recorded a malformed source commit: $sourceCommit"
    }
    if ($null -eq $CandidateSummary.source_identity -or
        [string]$CandidateSummary.source_identity.intended_commit -cne $sourceCommit -or
        [string]$CandidateSummary.source_identity.actual_commit -cne $sourceCommit -or
        [string]$CandidateSummary.source_identity.verified_commit -cne $sourceCommit -or
        [string]$CandidateSummary.source_identity.verification_status -cne 'verified_clean') {
        throw 'synthetic early-failure evidence did not preserve exact intended, actual, and clean-verified source identity'
    }
    $workingTree = $CandidateSummary.source_identity.working_tree
    if ($null -eq $workingTree -or
        [string]$workingTree.status -cne 'clean' -or
        [int]$workingTree.git_status_exit_code -ne 0 -or
        [int]$workingTree.entry_count -ne 0 -or
        [string]$workingTree.porcelain_v1_sha256 -cne (Get-Utf8Sha256 '') -or
        -not [bool]$workingTree.tracked_changes_included -or
        -not [bool]$workingTree.staged_changes_included -or
        -not [bool]$workingTree.untracked_files_included -or
        [bool]$workingTree.ignored_files_included) {
        throw 'synthetic early-failure evidence did not preserve an exact clean-tree verification'
    }
    return [pscustomobject][ordered]@{
        source_commit = $sourceCommit
        source_identity = $CandidateSummary.source_identity
        harness = [pscustomobject][ordered]@{
            path = $actualHarness
            sha256 = [string]$CandidateSummary.harness.sha256
        }
        binaries = $verifiedInputs.ToArray()
    }
}

function Invoke-EarlyFailureInputEvidenceSelfTest {
    param(
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Summary,
        [Parameter(Mandatory = $true)][string]$ExpectedHarnessPath,
        [Parameter(Mandatory = $true)][string]$ExpectedColayPath,
        [Parameter(Mandatory = $true)][string]$ExpectedFakeProviderPath,
        [Parameter(Mandatory = $true)][string]$ExpectedSourceCommit
    )
    $verified = Assert-VerifiedEarlyFailureInputEvidence -CandidateSummary $Summary `
        -ExpectedHarnessPath $ExpectedHarnessPath -ExpectedColayPath $ExpectedColayPath `
        -ExpectedFakeProviderPath $ExpectedFakeProviderPath -ExpectedSourceCommit $ExpectedSourceCommit
    $syntheticFailure = [ordered]@{
        summary = [ordered]@{
            status = 'failed'
            failure = [ordered]@{
                message = 'synthetic early failure before the latency phase'
                category = 'SyntheticFailure'
                script_stack = $null
            }
            source_commit = $Summary.source_commit
            source_identity = $Summary.source_identity
            harness = $Summary.harness
            binaries = $Summary.binaries
        }
    }
    $serialized = $syntheticFailure | ConvertTo-Json -Depth 10 -Compress
    $roundTrip = $serialized | ConvertFrom-Json -Depth 10
    if ([string]$roundTrip.summary.status -cne 'failed' -or
        [string]$roundTrip.summary.failure.category -cne 'SyntheticFailure') {
        throw 'synthetic early-failure JSON round trip lost its failure envelope'
    }
    [void](Assert-VerifiedEarlyFailureInputEvidence -CandidateSummary $roundTrip.summary `
            -ExpectedHarnessPath $ExpectedHarnessPath -ExpectedColayPath $ExpectedColayPath `
            -ExpectedFakeProviderPath $ExpectedFakeProviderPath -ExpectedSourceCommit $ExpectedSourceCommit)

    $alternateCommit = if ($ExpectedSourceCommit[0] -ceq '0') {
        '1' + $ExpectedSourceCommit.Substring(1)
    } else {
        '0' + $ExpectedSourceCommit.Substring(1)
    }
    $zeroSha = ('0' * 64) -join ''
    $oneSha = ('1' * 64) -join ''
    $alternateHarnessSha = if ([string]$Summary.harness.sha256 -ceq $zeroSha) { $oneSha } else { $zeroSha }
    $alternateColaySha = if ([string]$Summary.binaries.colay.sha256 -ceq $zeroSha) { $oneSha } else { $zeroSha }
    $alternateFakeSha = if ([string]$Summary.binaries.fake_provider.sha256 -ceq $zeroSha) { $oneSha } else { $zeroSha }
    $alternateTreeSha = if ([string]$Summary.source_identity.working_tree.porcelain_v1_sha256 -ceq $zeroSha) {
        $oneSha
    } else {
        $zeroSha
    }
    $tamperCases = @(
        [pscustomobject]@{ name = 'harness_path'; value = $ExpectedColayPath; mutate = {
                param($Document, $Value) $Document.summary.harness.path = [string]$Value
            } },
        [pscustomobject]@{ name = 'harness_sha256'; value = $alternateHarnessSha; mutate = {
                param($Document, $Value) $Document.summary.harness.sha256 = [string]$Value
            } },
        [pscustomobject]@{ name = 'colay_path'; value = $ExpectedFakeProviderPath; mutate = {
                param($Document, $Value) $Document.summary.binaries.colay.path = [string]$Value
            } },
        [pscustomobject]@{ name = 'colay_sha256'; value = $alternateColaySha; mutate = {
                param($Document, $Value) $Document.summary.binaries.colay.sha256 = [string]$Value
            } },
        [pscustomobject]@{ name = 'fake_provider_path'; value = $ExpectedHarnessPath; mutate = {
                param($Document, $Value) $Document.summary.binaries.fake_provider.path = [string]$Value
            } },
        [pscustomobject]@{ name = 'fake_provider_sha256'; value = $alternateFakeSha; mutate = {
                param($Document, $Value) $Document.summary.binaries.fake_provider.sha256 = [string]$Value
            } },
        [pscustomobject]@{ name = 'source_commit'; value = $alternateCommit; mutate = {
                param($Document, $Value) $Document.summary.source_commit = [string]$Value
            } },
        [pscustomobject]@{ name = 'source_intended_commit'; value = $alternateCommit; mutate = {
                param($Document, $Value) $Document.summary.source_identity.intended_commit = [string]$Value
            } },
        [pscustomobject]@{ name = 'source_actual_commit'; value = $alternateCommit; mutate = {
                param($Document, $Value) $Document.summary.source_identity.actual_commit = [string]$Value
            } },
        [pscustomobject]@{ name = 'source_verified_commit'; value = $alternateCommit; mutate = {
                param($Document, $Value) $Document.summary.source_identity.verified_commit = [string]$Value
            } },
        [pscustomobject]@{ name = 'source_verification_status'; value = 'dirty'; mutate = {
                param($Document, $Value) $Document.summary.source_identity.verification_status = [string]$Value
            } },
        [pscustomobject]@{ name = 'working_tree_status'; value = 'dirty'; mutate = {
                param($Document, $Value) $Document.summary.source_identity.working_tree.status = [string]$Value
            } },
        [pscustomobject]@{ name = 'working_tree_entry_count'; value = 1; mutate = {
                param($Document, $Value) $Document.summary.source_identity.working_tree.entry_count = [int]$Value
            } },
        [pscustomobject]@{ name = 'working_tree_porcelain_sha256'; value = $alternateTreeSha; mutate = {
                param($Document, $Value) $Document.summary.source_identity.working_tree.porcelain_v1_sha256 = [string]$Value
            } }
    )
    $tamperResults = [System.Collections.Generic.List[object]]::new()
    foreach ($tamperCase in $tamperCases) {
        $tampered = $serialized | ConvertFrom-Json -Depth 10
        & $tamperCase.mutate $tampered $tamperCase.value
        $accepted = $false
        try {
            [void](Assert-VerifiedEarlyFailureInputEvidence -CandidateSummary $tampered.summary `
                    -ExpectedHarnessPath $ExpectedHarnessPath -ExpectedColayPath $ExpectedColayPath `
                    -ExpectedFakeProviderPath $ExpectedFakeProviderPath -ExpectedSourceCommit $ExpectedSourceCommit)
            $accepted = $true
        } catch { }
        if ($accepted) {
            throw "synthetic early-failure tamper case was accepted: $($tamperCase.name)"
        }
        $tamperResults.Add([pscustomobject][ordered]@{
            name = [string]$tamperCase.name
            status = 'rejected'
        })
    }
    return [pscustomobject][ordered]@{
        status = 'passed'
        synthetic_failure_json_round_trip = $true
        source_commit = $verified.source_commit
        source_identity = $verified.source_identity
        harness = $verified.harness
        binaries = $verified.binaries
        serialized_bytes = [System.Text.Encoding]::UTF8.GetByteCount($serialized)
        tamper_case_count = $tamperResults.Count
        all_tamper_cases_rejected = $tamperResults.Count -eq $tamperCases.Count
        tamper_cases = $tamperResults.ToArray()
    }
}

function Sync-CompletedTimingEvidence {
    param([Parameter(Mandatory = $true)][System.Collections.IDictionary]$Summary)
    $serialCommands = @($script:CommandEvidence | Where-Object { $_.label -match '^serial-register-[0-9]+$' })
    $concurrentCommands = @($script:CommandEvidence | Where-Object { $_.label -match '^concurrent-register-[0-9]+$' })
    $serial = @($serialCommands | ForEach-Object { [int64]$_.elapsed_ms })
    $concurrent = @($concurrentCommands | ForEach-Object { [int64]$_.elapsed_ms })
    $Summary.serial_times_ms = $serial
    $Summary.concurrent_times_ms = $concurrent
    if ($serial.Count -ne 0) {
        $Summary.serial_max_ms = [int64](($serial | Measure-Object -Maximum).Maximum)
        $sortedSerial = @($serial | Sort-Object)
        $nearestRankIndex = [int][math]::Ceiling(0.95 * $sortedSerial.Count) - 1
        $Summary.serial_p95_ms = [int64]$sortedSerial[$nearestRankIndex]
    }
    if ($concurrent.Count -ne 0) {
        $Summary.concurrent_max_ms = [int64](($concurrent | Measure-Object -Maximum).Maximum)
    }
    $acceptanceFailures = [System.Collections.Generic.List[object]]::new()
    if ($serial.Count -ne 0 -and [int64]$Summary.serial_p95_ms -gt $SerialP95LimitMs) {
        $acceptanceFailures.Add([pscustomobject][ordered]@{
            criterion = 'serial_p95_ms'
            command_label = $null
            observed_ms = [int64]$Summary.serial_p95_ms
            limit_ms = $SerialP95LimitMs
            message = "serial nearest-rank p95 $($Summary.serial_p95_ms)ms exceeded ${SerialP95LimitMs}ms"
        })
    }
    foreach ($command in $concurrentCommands) {
        if ([int64]$command.elapsed_ms -gt $ConcurrentLimitMs) {
            $acceptanceFailures.Add([pscustomobject][ordered]@{
                criterion = 'concurrent_command_elapsed_ms'
                command_label = [string]$command.label
                observed_ms = [int64]$command.elapsed_ms
                limit_ms = $ConcurrentLimitMs
                message = "$($command.label) took $($command.elapsed_ms)ms, exceeding ${ConcurrentLimitMs}ms"
            })
        }
    }
    $Summary.acceptance_failures = $acceptanceFailures.ToArray()
    $Summary.measurement_diagnostics.concurrent_start_cleanup = $script:ProcessBatchCleanupEvidence.ToArray()
    $Summary.measurement_diagnostics.timing_self_test_failure_cleanup = $script:TimingSelfTestFailureCleanupEvidence
    $Summary.measurement_diagnostics.command_timings = @($script:CommandEvidence | ForEach-Object {
        [pscustomobject][ordered]@{
            label = $_.label
            measurement_method = $_.measurement_method
            process_lifetime_ms = if ($_.measurement_method -ceq 'os-process-lifetime') { [int64]$_.elapsed_ms } else { $null }
            launch_overhead_ms = $_.launch_overhead_ms
            exit_detection_wall_ms = $_.exit_detection_wall_ms
            output_drain_wall_ms = $_.output_drain_wall_ms
            post_exit_total_wall_ms = $_.post_exit_total_wall_ms
            observer_wall_ms = $_.observer_wall_ms
            observer_deferred = $_.observer_deferred
        }
    })
}

function Assert-ExactStoppedStatus {
    param(
        [Parameter(Mandatory = $true)]$Result,
        [Parameter(Mandatory = $true)][string]$ExpectedCommand
    )
    if ($Result.exit_code -ne 0) {
        throw "$($Result.label) exited with code $($Result.exit_code)"
    }
    $document = Assert-StatusJson $Result
    if ([string]$document.schema_version -cne '1' -or
        [string]$document.command -cne $ExpectedCommand -or
        [string]$document.data.status.state -cne 'stopped') {
        throw "$($Result.label) did not return exact schema-v1 $ExpectedCommand stopped status"
    }
    return $document
}

function Assert-NoProductNativeProcessLaunchBypass {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)
    $cratesRoot = Join-Path $RepositoryRoot 'crates'
    $safeCreationFlagsRelativePath = 'crates/orchestrator-cli/src/ipc_client.rs'
    $safeCreationFlagsHash = '799fc38a5f8725fc65d2fe0b0b7d564e8e593678cf3404cfdce8e8ad0e4738e2'
    $patterns = @(
        '(?i)\bDEBUG_PROCESS\b',
        '(?i)\bDEBUG_ONLY_THIS_PROCESS\b',
        '(?i)\bCreateProcessA\b',
        '(?i)\bCreateProcessW\b',
        '(?i)\bCreateProcess\s*\('
    )
    $sourceFiles = @(Get-ChildItem -LiteralPath $cratesRoot -Recurse -File -Filter '*.rs' -ErrorAction Stop)
    $matches = [System.Collections.Generic.List[object]]::new()
    $creationFlagsIdentifiers = [System.Collections.Generic.List[object]]::new()
    foreach ($sourceFile in $sourceFiles) {
        foreach ($match in @(Select-String -LiteralPath $sourceFile.FullName -Pattern $patterns -AllMatches -ErrorAction Stop)) {
            $matches.Add([pscustomobject]@{
                path = $sourceFile.FullName
                line = $match.LineNumber
                text = $match.Line.Trim()
            })
        }
        $source = Get-Content -LiteralPath $sourceFile.FullName -Raw -ErrorAction Stop
        $relativePath = [System.IO.Path]::GetRelativePath($RepositoryRoot, $sourceFile.FullName).Replace('\', '/')
        $sourceLines = @($source -split "`r?`n")
        foreach ($identifierMatch in [regex]::Matches($source, '(?i)\bcreation_flags\b')) {
            $lineNumber = 1 + @($source.Substring(0, $identifierMatch.Index) -split "`n").Count - 1
            $identifierLine = $sourceLines[$lineNumber - 1].Trim()
            $priorNonempty = @()
            if ($lineNumber -gt 1) {
                $priorNonempty = @($sourceLines[0..($lineNumber - 2)] | ForEach-Object { $_.Trim() } |
                    Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
            }
            $siteLines = if ($priorNonempty.Count -ge 2) {
                @($priorNonempty[-2], $priorNonempty[-1], $identifierLine)
            } else {
                @($identifierLine)
            }
            $siteText = $siteLines -join "`n"
            $sha = [Security.Cryptography.SHA256]::Create()
            try {
                $siteHash = [Convert]::ToHexString(
                    $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($siteText))
                ).ToLowerInvariant()
            } finally {
                $sha.Dispose()
            }
            $allowlisted = $relativePath -ceq $safeCreationFlagsRelativePath -and
                $siteHash -ceq $safeCreationFlagsHash
            $identifierEvidence = [pscustomobject][ordered]@{
                path = $sourceFile.FullName
                relative_path = $relativePath
                line = $lineNumber
                text = $identifierLine
                normalized_site_sha256 = $siteHash
                allowlisted = $allowlisted
            }
            $creationFlagsIdentifiers.Add($identifierEvidence)
            if (-not $allowlisted) {
                $matches.Add([pscustomobject]@{
                    path = $sourceFile.FullName
                    line = $lineNumber
                    text = $identifierLine
                    reason = 'non-allowlisted creation_flags identifier; comments, aliases, macros, numeric, named, bitwise, and other indirection fail closed'
                    normalized_site_sha256 = $siteHash
                })
            }
        }
    }
    $allowlistedCreationFlagsIdentifiers = @($creationFlagsIdentifiers | Where-Object allowlisted -EQ $true)
    if ($creationFlagsIdentifiers.Count -ne 1 -or $allowlistedCreationFlagsIdentifiers.Count -ne 1) {
        $matches.Add([pscustomobject]@{
            path = $cratesRoot
            line = 0
            text = $null
            reason = 'expected exactly one creation_flags identifier and exactly one exact path-plus-site-hash allowlist match'
        })
    }
    if ($matches.Count -ne 0) {
        throw "product or fake-provider source contains a forbidden native process-launch/debug bypass: $($matches | ConvertTo-Json -Compress)"
    }
    return [pscustomobject]@{
        root = $cratesRoot
        rust_files_scanned = $sourceFiles.Count
        rejected_patterns = $patterns
        creation_flags_identifiers_scanned = $creationFlagsIdentifiers.Count
        creation_flags_calls_scanned = $creationFlagsIdentifiers.Count
        allowlisted_creation_flags_sites = $creationFlagsIdentifiers.ToArray()
        matches = @()
    }
}

function ConvertTo-ProcessAuditArgumentBase64 {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Value)
    $valueBytes = [System.Text.Encoding]::UTF8.GetBytes($Value)
    $framedBytes = [byte[]]::new($valueBytes.Length + 1)
    [Array]::Copy($valueBytes, 0, $framedBytes, 1, $valueBytes.Length)
    return [Convert]::ToBase64String($framedBytes)
}

function Write-ProcessAuditChildScript {
    param([Parameter(Mandatory = $true)][string]$Path)
    $scriptSource = @'
#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ColayExe,
    [Parameter(Mandatory = $true)][string]$PythonExe,
    [Parameter(Mandatory = $true)][string]$EmptyRepository,
    [Parameter(Mandatory = $true)][string]$LegacyRepository,
    [Parameter(Mandatory = $true)][string]$LegacyDatabase,
    [Parameter(Mandatory = $true)][string]$AuditColayHome,
    [Parameter(Mandatory = $true)][string]$MarkerDirectory,
    [Parameter(Mandatory = $true)][string]$ExpectedSourceHashesJson,
    [Parameter(Mandatory = $true)][string]$ExpectedConfigSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:ChildProcessLineFailureForTest = $null
$script:LastChildProcessCleanup = $null
$script:AuditDaemonReadinessTimeoutMs = 5000
$script:AuditDaemonReadinessPollIntervalMs = 50
$script:AuditDaemonReadinessExitWaitLimitMs = 400
$script:AuditDaemonReadinessOutputDrainLimitMs = 100
$script:AuditDaemonReadinessCleanupReserveMs = 500
$script:AuditDaemonReadinessInitialParseDelayForTestMs = 0

function ConvertTo-ComparablePath {
    param([Parameter(Mandatory = $true)][string]$Value)
    $full = [System.IO.Path]::GetFullPath($Value)
    if ($full.StartsWith('\\?\', [System.StringComparison]::Ordinal)) { $full = $full.Substring(4) }
    return $full.TrimEnd('\').ToLowerInvariant()
}

function Get-AuditElapsedCeilingMs {
    param([Parameter(Mandatory = $true)][System.Diagnostics.Stopwatch]$Stopwatch)
    return [int64][Math]::Ceiling($Stopwatch.Elapsed.TotalMilliseconds)
}

function Get-AuditPhaseWaitMs {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.Stopwatch]$Stopwatch,
        [Parameter(Mandatory = $true)][int64]$OverallDeadlineMs,
        [Parameter(Mandatory = $true)][int64]$PhaseDeadlineElapsedMs,
        [Parameter(Mandatory = $true)][int]$MaximumWaitMs
    )
    $elapsedMs = Get-AuditElapsedCeilingMs -Stopwatch $Stopwatch
    $remainingMs = [Math]::Min($OverallDeadlineMs - $elapsedMs, $PhaseDeadlineElapsedMs - $elapsedMs)
    if ($remainingMs -le 0 -or $MaximumWaitMs -le 0) { return 0 }
    return [int][Math]::Min([int64]$MaximumWaitMs, $remainingMs)
}

function ConvertTo-AuditChildJson {
    param([Parameter(Mandatory = $true)]$Value)
    return $Value | ConvertTo-Json -Compress -Depth 30 -WarningAction Stop
}

function Get-ProcessGenerationObservation {
    param(
        [Parameter(Mandatory = $true)][int]$ProcessId,
        [Parameter(Mandatory = $true)][long]$ExpectedCreationFileTimeUtc,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutablePath
    )
    $observation = [pscustomobject][ordered]@{
        process_id = $ProcessId
        expected_creation_file_time_utc = $ExpectedCreationFileTimeUtc
        expected_executable_path = [System.IO.Path]::GetFullPath($ExpectedExecutablePath)
        process_exists = $false
        observed_creation_file_time_utc = $null
        observed_executable_path = $null
        identity_verified = $false
        expected_generation_live = $false
        observation_error = $null
    }
    $candidate = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($null -eq $candidate) { return $observation }
    $observation.process_exists = $true
    try {
        if ($candidate.HasExited) {
            $observation.process_exists = $false
            return $observation
        }
        $observedCreation = $candidate.StartTime.ToUniversalTime().ToFileTimeUtc()
        $rawObservedPath = [string]$candidate.Path
        if ([string]::IsNullOrWhiteSpace($rawObservedPath)) {
            throw 'live process exposed no executable path'
        }
        $observedPath = [System.IO.Path]::GetFullPath($rawObservedPath)
        $observation.observed_creation_file_time_utc = $observedCreation
        $observation.observed_executable_path = $observedPath
        $observation.identity_verified = $true
        $observation.expected_generation_live = $observedCreation -eq $ExpectedCreationFileTimeUtc -and
            (ConvertTo-ComparablePath $observedPath) -ceq
            (ConvertTo-ComparablePath $ExpectedExecutablePath)
    } catch {
        $observation.observation_error = $_.Exception.Message
        try {
            if ($candidate.HasExited) {
                $observation.process_exists = $false
                $observation.observation_error = $null
            }
        } catch { }
    } finally {
        $candidate.Dispose()
    }
    return $observation
}

function Invoke-ChildProcessLine {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][string]$Label,
        [int]$TimeoutMs = 30000,
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000
    )
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    if ($boundDeadlineParameterCount -notin @(0, $deadlineParameterNames.Count)) {
        throw 'child process runner requires one atomic bounded deadline contract'
    }
    $deadlineAware = $boundDeadlineParameterCount -eq $deadlineParameterNames.Count
    if ($deadlineAware -and ($null -eq $OverallDeadlineStopwatch -or
        $OverallDeadlineMs -le 0 -or -not $OverallDeadlineStopwatch.IsRunning -or
        $ExitWaitLimitMs -lt 0 -or $OutputDrainLimitMs -lt 0 -or $TimeoutMs -le 0 -or
        ($ExitWaitLimitMs + $OutputDrainLimitMs) -ge $OverallDeadlineMs)) {
        throw 'child process runner received an invalid bounded deadline contract'
    }
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $false
    foreach ($argument in $Arguments) { $startInfo.ArgumentList.Add($argument) }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $processStarted = $false
    $stdoutTask = $null
    $line = $null
    $exitCode = $null
    $timedOut = $false
    $primaryFailure = $null
    $cleanupErrors = [System.Collections.Generic.List[string]]::new()
    $cleanup = [pscustomobject][ordered]@{
        label = $Label
        child_started = $false
        process_id = $null
        process_creation_file_time_utc = $null
        executable_path = $null
        timed_out = $false
        terminate_requested = $false
        kill_tree_attempted = $false
        kill_tree_error = $null
        tree_kill_request_succeeded = $null
        single_process_fallback_attempted = $false
        single_process_fallback_succeeded = $false
        exit_confirmed = $false
        deadline_aware = $deadlineAware
        overall_deadline_ms = if ($deadlineAware) { $OverallDeadlineMs } else { $null }
        remaining_at_launch_ms = $null
        command_timeout_ms = $TimeoutMs
        total_operation_budget_ms = $null
        exit_wait_limit_ms = $ExitWaitLimitMs
        exit_wait_applied_ms = $null
        exit_wait_consumed_ms = $null
        stdout_completed = $false
        output_drain_limit_ms = $OutputDrainLimitMs
        output_drain_consumed_ms = $null
        process_disposed = $false
        total_wall_ms = $null
        cleanup_errors = @()
    }
    $deadlineExecutionEndMs = $null
    $deadlineExitEndMs = $null
    $deadlineDrainEndMs = $null
    [int64]$deadlineExitWaitConsumedMs = 0
    [int64]$deadlineOutputDrainConsumedMs = 0
    try {
        if ($deadlineAware) {
            $launchElapsedMs = Get-AuditElapsedCeilingMs -Stopwatch $OverallDeadlineStopwatch
            $remainingAtLaunchMs = [int64]$OverallDeadlineMs - $launchElapsedMs
            $cleanupBudgetMs = [int64]$ExitWaitLimitMs + $OutputDrainLimitMs
            $effectiveTimeoutMs = [int][Math]::Min([int64]$TimeoutMs, $remainingAtLaunchMs - $cleanupBudgetMs)
            if ($effectiveTimeoutMs -le 0) {
                throw "child process deadline had no execution budget at launch (remaining=${remainingAtLaunchMs}ms, cleanup=${cleanupBudgetMs}ms)"
            }
            $deadlineExecutionEndMs = $launchElapsedMs + $effectiveTimeoutMs
            $deadlineExitEndMs = $deadlineExecutionEndMs + $ExitWaitLimitMs
            $deadlineDrainEndMs = $deadlineExitEndMs + $OutputDrainLimitMs
            if ($deadlineDrainEndMs -gt $OverallDeadlineMs -or
                ($effectiveTimeoutMs + $cleanupBudgetMs) -gt $remainingAtLaunchMs) {
                throw 'child process launch budget exceeded the shared overall deadline'
            }
            $cleanup.remaining_at_launch_ms = $remainingAtLaunchMs
            $cleanup.command_timeout_ms = $effectiveTimeoutMs
            $cleanup.total_operation_budget_ms = $effectiveTimeoutMs + $cleanupBudgetMs
        }
        $startReturned = if ($script:ChildProcessLineFailureForTest -ceq 'process-start-false') {
            $false
        } else {
            $process.Start()
        }
        if (-not $startReturned) { throw "failed to start $Label" }
        $processStarted = $true
        $cleanup.child_started = $true
        $cleanup.process_id = [int]$process.Id
        $cleanup.process_creation_file_time_utc = $process.StartTime.ToUniversalTime().ToFileTimeUtc()
        $cleanup.executable_path = [System.IO.Path]::GetFullPath($Executable)
        if ($script:ChildProcessLineFailureForTest -ceq 'stdout-read-start') {
            throw [System.InvalidOperationException]::new('injected generated-child stdout reader setup failure')
        }
        $stdoutTask = $process.StandardOutput.ReadLineAsync()
        $processExited = $false
        while (-not $processExited) {
            if ($deadlineAware) {
                $executionWaitMs = Get-AuditPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                    -OverallDeadlineMs $OverallDeadlineMs -PhaseDeadlineElapsedMs $deadlineExecutionEndMs `
                    -MaximumWaitMs 10
                if ($executionWaitMs -le 0) {
                    $timedOut = $true
                    throw "$Label exceeded $($cleanup.command_timeout_ms)ms"
                }
                $processExited = $process.WaitForExit($executionWaitMs)
            } else {
                $processExited = $process.WaitForExit(10)
                if (-not $processExited -and $stopwatch.ElapsedMilliseconds -gt $TimeoutMs) {
                    $timedOut = $true
                    throw "$Label exceeded ${TimeoutMs}ms"
                }
            }
        }
        $successExitWaitMs = if ($deadlineAware) {
            Get-AuditPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                -OverallDeadlineMs $OverallDeadlineMs -PhaseDeadlineElapsedMs $deadlineExitEndMs `
                -MaximumWaitMs ($ExitWaitLimitMs - [int]$deadlineExitWaitConsumedMs)
        } else { 5000 }
        $successExitWaitWall = [System.Diagnostics.Stopwatch]::StartNew()
        $successExitConfirmed = $process.WaitForExit($successExitWaitMs)
        $successExitWaitWall.Stop()
        if ($deadlineAware) {
            $deadlineExitWaitConsumedMs += [int64][Math]::Ceiling($successExitWaitWall.Elapsed.TotalMilliseconds)
        }
        if (-not $successExitConfirmed) {
            throw "$Label did not remain exit-confirmed during bounded finalization"
        }
        $successDrainStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
        while (-not $stdoutTask.IsCompleted) {
            $drainRemainingMs = if ($deadlineAware) {
                Get-AuditPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                    -OverallDeadlineMs $OverallDeadlineMs -PhaseDeadlineElapsedMs $deadlineDrainEndMs `
                    -MaximumWaitMs ($OutputDrainLimitMs - [int]$deadlineOutputDrainConsumedMs -
                        [int][Math]::Ceiling($successDrainStopwatch.Elapsed.TotalMilliseconds))
            } else {
                5000 - [int][Math]::Ceiling($successDrainStopwatch.Elapsed.TotalMilliseconds)
            }
            if ($drainRemainingMs -le 0) { break }
            Start-Sleep -Milliseconds ([int][Math]::Min(10, $drainRemainingMs))
        }
        $successDrainStopwatch.Stop()
        if ($deadlineAware) {
            $deadlineOutputDrainConsumedMs += [int64][Math]::Ceiling(
                $successDrainStopwatch.Elapsed.TotalMilliseconds
            )
        }
        if (-not $stdoutTask.IsCompleted) { throw "$Label did not emit a complete first stdout line" }
        $line = $stdoutTask.GetAwaiter().GetResult()
        $exitCode = [int]$process.ExitCode
    } catch {
        $primaryFailure = $_
    } finally {
        $cleanup.timed_out = $timedOut
        if ($processStarted) {
            $exitAlreadyConfirmed = $false
            try {
                $exitAlreadyConfirmed = $process.WaitForExit(0)
            } catch {
                $cleanupErrors.Add("exit-state query failed: $($_.Exception.Message)")
            }
            if ($null -ne $primaryFailure -and -not $exitAlreadyConfirmed) {
                $cleanup.terminate_requested = $true
                $cleanup.kill_tree_attempted = $true
                $cleanup.tree_kill_request_succeeded = $false
                try {
                    $process.Kill($true)
                    $cleanup.tree_kill_request_succeeded = $true
                } catch {
                    $treeKillError = $_.Exception.Message
                    $cleanup.kill_tree_error = $treeKillError
                    $cleanupErrors.Add("process tree termination failed: $treeKillError")
                    $cleanup.single_process_fallback_attempted = $true
                    try {
                        $process.Kill()
                        $cleanup.single_process_fallback_succeeded = $true
                    } catch {
                        $cleanupErrors.Add("direct child fallback termination failed: $($_.Exception.Message)")
                    }
                }
            }
            $cleanupExitWaitMs = if ($deadlineAware) {
                $phaseEndMs = $deadlineExitEndMs
                Get-AuditPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                    -OverallDeadlineMs $OverallDeadlineMs -PhaseDeadlineElapsedMs $phaseEndMs `
                    -MaximumWaitMs ($ExitWaitLimitMs - [int]$deadlineExitWaitConsumedMs)
            } else { 5000 }
            $cleanup.exit_wait_applied_ms = $cleanupExitWaitMs
            $cleanupExitWaitWall = [System.Diagnostics.Stopwatch]::StartNew()
            try {
                $cleanup.exit_confirmed = $process.WaitForExit($cleanupExitWaitMs)
            } catch {
                $cleanupErrors.Add("bounded process exit wait failed: $($_.Exception.Message)")
            } finally {
                $cleanupExitWaitWall.Stop()
                if ($deadlineAware) {
                    $deadlineExitWaitConsumedMs += [int64][Math]::Ceiling(
                        $cleanupExitWaitWall.Elapsed.TotalMilliseconds
                    )
                }
            }
            if (-not $cleanup.exit_confirmed) {
                $cleanupErrors.Add("process did not exit within the ${cleanupExitWaitMs}ms cleanup limit")
            }
        }
        if ($null -ne $stdoutTask) {
            $cleanupDrainStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
            while (-not $stdoutTask.IsCompleted) {
                $drainRemainingMs = if ($deadlineAware) {
                    $phaseEndMs = $deadlineDrainEndMs
                    Get-AuditPhaseWaitMs -Stopwatch $OverallDeadlineStopwatch `
                        -OverallDeadlineMs $OverallDeadlineMs -PhaseDeadlineElapsedMs $phaseEndMs `
                        -MaximumWaitMs ($OutputDrainLimitMs - [int]$deadlineOutputDrainConsumedMs -
                            [int][Math]::Ceiling($cleanupDrainStopwatch.Elapsed.TotalMilliseconds))
                } else {
                    2000 - [int][Math]::Ceiling($cleanupDrainStopwatch.Elapsed.TotalMilliseconds)
                }
                if ($drainRemainingMs -le 0) { break }
                Start-Sleep -Milliseconds ([int][Math]::Min(10, $drainRemainingMs))
            }
            $cleanupDrainStopwatch.Stop()
            if ($deadlineAware) {
                $deadlineOutputDrainConsumedMs += [int64][Math]::Ceiling(
                    $cleanupDrainStopwatch.Elapsed.TotalMilliseconds
                )
            }
            $cleanup.stdout_completed = [bool]$stdoutTask.IsCompleted
            if (-not $cleanup.stdout_completed) {
                $cleanupErrors.Add("redirected stdout did not drain within the ${OutputDrainLimitMs}ms cleanup limit")
            }
        }
        try {
            $process.Dispose()
            $cleanup.process_disposed = $true
        } catch {
            $cleanupErrors.Add("process dispose failed: $($_.Exception.Message)")
        }
        $stopwatch.Stop()
        $cleanup.total_wall_ms = [int64]$stopwatch.ElapsedMilliseconds
        $cleanup.exit_wait_consumed_ms = if ($deadlineAware) { $deadlineExitWaitConsumedMs } else { $null }
        $cleanup.output_drain_consumed_ms = if ($deadlineAware) { $deadlineOutputDrainConsumedMs } else { $null }
        $cleanup.cleanup_errors = $cleanupErrors.ToArray()
        $script:LastChildProcessCleanup = $cleanup
    }
    if ($null -ne $primaryFailure) {
        $message = $primaryFailure.Exception.Message
        if ($cleanupErrors.Count -ne 0) { $message += "; cleanup: $($cleanupErrors -join '; ')" }
        throw $message
    }
    if ($cleanupErrors.Count -ne 0) { throw "$Label cleanup failed: $($cleanupErrors -join '; ')" }
    if ($exitCode -ne 0) { throw "$Label exited with code $exitCode" }
    if ([string]::IsNullOrWhiteSpace($line)) { throw "$Label emitted empty stdout" }
    return $line
}

function Invoke-ChildProcessLineFailureSelfTest {
    $portablePowerShell = [System.IO.Path]::GetFullPath((Join-Path $PSHOME 'pwsh.exe'))
    $caseResults = [System.Collections.Generic.List[object]]::new()
    $startFalseFailure = $null
    try {
        $script:ChildProcessLineFailureForTest = 'process-start-false'
        Invoke-ChildProcessLine -Executable $portablePowerShell `
            -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'exit 0') `
            -WorkingDirectory $AuditColayHome -Label 'process-line-start-false-self-test' -TimeoutMs 5000 | Out-Null
    } catch {
        $startFalseFailure = $_.Exception.Message
    } finally {
        $script:ChildProcessLineFailureForTest = $null
    }
    $startFalseCleanup = $script:LastChildProcessCleanup
    if ($startFalseFailure -notmatch 'failed to start' -or $null -eq $startFalseCleanup -or
        $startFalseCleanup.child_started -ne $false -or $null -ne $startFalseCleanup.process_id -or
        -not $startFalseCleanup.process_disposed -or @($startFalseCleanup.cleanup_errors).Count -ne 0) {
        throw "process-line start-false self-test cleanup evidence was incomplete: $($startFalseCleanup | ConvertTo-Json -Compress -Depth 8)"
    }
    $caseResults.Add([pscustomobject][ordered]@{
        failure_stage = 'process-start-false'
        child_started = $false
        process_id = $null
        process_residue_count = 0
        incomplete_pipe_task_count = 0
        cleanup_error_count = 0
        process_disposed = [bool]$startFalseCleanup.process_disposed
    })

    $stdoutSetupFailure = $null
    try {
        $script:ChildProcessLineFailureForTest = 'stdout-read-start'
        Invoke-ChildProcessLine -Executable $portablePowerShell `
            -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30') `
            -WorkingDirectory $AuditColayHome -Label 'process-line-stdout-setup-self-test' -TimeoutMs 5000 | Out-Null
    } catch {
        $stdoutSetupFailure = $_.Exception.Message
    } finally {
        $script:ChildProcessLineFailureForTest = $null
    }
    $stdoutSetupCleanup = $script:LastChildProcessCleanup
    $stdoutSetupResidueObservation = if ($null -eq $stdoutSetupCleanup -or
        $null -eq $stdoutSetupCleanup.process_id -or
        $null -eq $stdoutSetupCleanup.process_creation_file_time_utc -or
        [string]::IsNullOrWhiteSpace([string]$stdoutSetupCleanup.executable_path)) {
        $null
    } else {
        Get-ProcessGenerationObservation -ProcessId ([int]$stdoutSetupCleanup.process_id) `
            -ExpectedCreationFileTimeUtc ([long]$stdoutSetupCleanup.process_creation_file_time_utc) `
            -ExpectedExecutablePath ([string]$stdoutSetupCleanup.executable_path)
    }
    if ($stdoutSetupFailure -notmatch 'injected generated-child stdout reader setup failure' -or
        $null -eq $stdoutSetupCleanup -or -not $stdoutSetupCleanup.child_started -or
        $null -eq $stdoutSetupCleanup.process_id -or
        $null -eq $stdoutSetupResidueObservation -or
        ($stdoutSetupResidueObservation.process_exists -and -not $stdoutSetupResidueObservation.identity_verified) -or
        $stdoutSetupResidueObservation.expected_generation_live -or
        -not $stdoutSetupCleanup.exit_confirmed -or -not $stdoutSetupCleanup.process_disposed -or
        @($stdoutSetupCleanup.cleanup_errors).Count -ne 0) {
        throw "process-line stdout-setup self-test cleanup evidence was incomplete: $($stdoutSetupCleanup | ConvertTo-Json -Compress -Depth 8)"
    }
    $caseResults.Add([pscustomobject][ordered]@{
        failure_stage = 'stdout-read-start'
        child_started = $true
        process_id = [int]$stdoutSetupCleanup.process_id
        process_residue_count = 0
        incomplete_pipe_task_count = 0
        cleanup_error_count = 0
        process_disposed = [bool]$stdoutSetupCleanup.process_disposed
        residue_observation = $stdoutSetupResidueObservation
    })

    $marker = Join-Path $AuditColayHome 'process-line-timeout-self-test.pid'
    if (Test-Path -LiteralPath $marker) { throw "process-line timeout self-test marker already exists: $marker" }
    $escapedMarker = $marker.Replace("'", "''")
    $markerWriter = "[System.IO.File]::WriteAllText('$escapedMarker', [string]`$PID); Start-Sleep -Seconds 30"
    $failureMessage = $null
    $processId = $null
    $residueObserved = $false
    $timeoutCleanup = $null
    $timeoutResidueObservation = $null
    $wall = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        Invoke-ChildProcessLine -Executable $portablePowerShell `
            -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $markerWriter) `
            -WorkingDirectory $AuditColayHome -Label 'process-line-timeout-self-test' -TimeoutMs 2000 | Out-Null
    } catch {
        $failureMessage = $_.Exception.Message
    } finally {
        $wall.Stop()
        $timeoutCleanup = $script:LastChildProcessCleanup
        if (Test-Path -LiteralPath $marker -PathType Leaf) {
            $processId = [int](Get-Content -LiteralPath $marker -Raw).Trim()
            if ($null -eq $timeoutCleanup -or [int]$timeoutCleanup.process_id -ne $processId -or
                $null -eq $timeoutCleanup.process_creation_file_time_utc -or
                [string]::IsNullOrWhiteSpace([string]$timeoutCleanup.executable_path)) {
                throw 'process-line timeout self-test could not bind its marker PID to an exact launched identity'
            }
            $timeoutResidueObservation = Get-ProcessGenerationObservation -ProcessId $processId `
                -ExpectedCreationFileTimeUtc ([long]$timeoutCleanup.process_creation_file_time_utc) `
                -ExpectedExecutablePath ([string]$timeoutCleanup.executable_path)
            if ($timeoutResidueObservation.process_exists -and -not $timeoutResidueObservation.identity_verified) {
                throw "process-line timeout self-test could not verify process $processId generation: $($timeoutResidueObservation.observation_error)"
            }
            $residueObserved = [bool]$timeoutResidueObservation.expected_generation_live
            Remove-Item -LiteralPath $marker -Force -ErrorAction Stop
        }
    }
    if ($failureMessage -notmatch 'exceeded 2000ms') {
        throw "process-line timeout self-test did not reach its injected timeout: $failureMessage"
    }
    if ($null -eq $processId) { throw 'process-line timeout self-test child did not publish its process id' }
    if ($residueObserved) { throw "process-line timeout self-test left process $processId alive" }
    $cleanup = $timeoutCleanup
    if ($null -eq $cleanup -or -not $cleanup.timed_out -or -not $cleanup.kill_tree_attempted -or
        -not $cleanup.exit_confirmed -or -not $cleanup.stdout_completed -or
        -not $cleanup.process_disposed -or @($cleanup.cleanup_errors).Count -ne 0) {
        throw "process-line timeout self-test cleanup evidence was incomplete: $($cleanup | ConvertTo-Json -Compress -Depth 8)"
    }
    if ($wall.ElapsedMilliseconds -gt 10000) {
        throw "process-line timeout self-test exceeded its bounded wall allowance: $($wall.ElapsedMilliseconds)ms"
    }
    $caseResults.Add([pscustomobject][ordered]@{
        failure_stage = 'hard-timeout'
        child_started = $true
        process_id = [int]$processId
        process_residue_count = 0
        residue_observation = $timeoutResidueObservation
        incomplete_pipe_task_count = 0
        cleanup_error_count = 0
        process_disposed = [bool]$cleanup.process_disposed
    })
    return [pscustomobject][ordered]@{
        status = 'passed'
        case_count = $caseResults.Count
        cases = $caseResults.ToArray()
        failure_stage = 'hard-timeout'
        wall_ms = [int64]$wall.ElapsedMilliseconds
        process_id = [int]$processId
        process_residue_count = 0
        incomplete_pipe_task_count = 0
        cleanup_error_count = 0
        exit_confirmed = [bool]$cleanup.exit_confirmed
        process_disposed = [bool]$cleanup.process_disposed
    }
}

function Invoke-ColayDocument {
    param(
        [string]$Repository,
        [string[]]$Arguments,
        [string]$Label,
        [int]$TimeoutMs = 30000,
        [AllowNull()][System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
        [int]$OverallDeadlineMs = 0,
        [int]$ExitWaitLimitMs = 5000,
        [int]$OutputDrainLimitMs = 2000
    )
    $deadlineParameterNames = @(
        'OverallDeadlineStopwatch', 'OverallDeadlineMs', 'ExitWaitLimitMs', 'OutputDrainLimitMs'
    )
    $boundDeadlineParameterCount = @($deadlineParameterNames | Where-Object {
        $PSBoundParameters.ContainsKey($_)
    }).Count
    if ($boundDeadlineParameterCount -notin @(0, $deadlineParameterNames.Count)) {
        throw 'Colay document invocation requires one atomic bounded deadline contract'
    }
    $deadlineArguments = @{}
    if ($boundDeadlineParameterCount -eq $deadlineParameterNames.Count) {
        $deadlineArguments = @{
            OverallDeadlineStopwatch = $OverallDeadlineStopwatch
            OverallDeadlineMs = $OverallDeadlineMs
            ExitWaitLimitMs = $ExitWaitLimitMs
            OutputDrainLimitMs = $OutputDrainLimitMs
        }
    }
    $line = Invoke-ChildProcessLine -Executable $ColayExe -Arguments $Arguments `
        -WorkingDirectory $Repository -Label $Label -TimeoutMs $TimeoutMs @deadlineArguments
    try { return $line | ConvertFrom-Json -Depth 30 }
    catch { throw "$Label did not emit valid JSON: $($_.Exception.Message)" }
}

function ConvertTo-AuditDaemonDocumentIdentity {
    param(
        [Parameter(Mandatory = $true)]$Document,
        [Parameter(Mandatory = $true)]
        [ValidateSet('daemon_start', 'daemon_status')]
        [string]$ExpectedCommand,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable
    )
    if ($null -eq $Document -or
        $Document.PSObject.Properties.Name -cnotcontains 'schema_version' -or
        $Document.PSObject.Properties.Name -cnotcontains 'command' -or
        $Document.PSObject.Properties.Name -cnotcontains 'data' -or
        $Document.schema_version -isnot [string] -or
        [string]$Document.schema_version -cne '1' -or
        $Document.command -isnot [string] -or
        [string]$Document.command -cne $ExpectedCommand) {
        throw "$ExpectedCommand did not return exact schema-v1 $ExpectedCommand JSON"
    }
    if ($null -eq $Document.data -or
        $Document.data.PSObject.Properties.Name -cnotcontains 'status' -or
        $null -eq $Document.data.status -or
        $Document.data.status.PSObject.Properties.Name -cnotcontains 'state' -or
        $Document.data.status.PSObject.Properties.Name -cnotcontains 'instance') {
        throw "$ExpectedCommand JSON has no exact status identity"
    }
    $status = $Document.data.status
    $instance = $status.instance
    if ($null -eq $instance) { throw "$ExpectedCommand JSON has no exact instance identity" }
    foreach ($propertyName in @('instance_id', 'pid', 'phase', 'executable_path')) {
        if ($instance.PSObject.Properties.Name -cnotcontains $propertyName) {
            throw "$ExpectedCommand instance is missing exact property: $propertyName"
        }
    }
    if ($status.state -isnot [string] -or
        $instance.phase -isnot [string] -or
        $instance.instance_id -isnot [string] -or
        $instance.executable_path -isnot [string]) {
        throw "$ExpectedCommand state, phase, instance id, and executable path must be exact JSON strings"
    }
    $state = [string]$status.state
    $phase = [string]$instance.phase
    if ([string]::IsNullOrWhiteSpace($state) -or
        [string]::IsNullOrWhiteSpace($phase) -or
        $state -cne $phase) {
        throw "$ExpectedCommand state/phase mismatch: state '$state', phase '$phase'"
    }

    $instanceIdText = [string]$instance.instance_id
    try {
        $instanceId = ([guid]::ParseExact($instanceIdText, 'D')).ToString('D')
    } catch {
        throw "$ExpectedCommand returned a malformed instance id: $instanceIdText"
    }
    if ($instanceIdText -cne $instanceId) {
        throw "$ExpectedCommand instance id is not canonical UUID text: $instanceIdText"
    }

    $integralPidTypes = @(
        [byte], [sbyte], [int16], [uint16], [int32], [uint32], [int64], [uint64]
    )
    if ($null -eq $instance.pid -or $integralPidTypes -notcontains $instance.pid.GetType()) {
        $actualPidType = if ($null -eq $instance.pid) { 'null' } else { $instance.pid.GetType().FullName }
        throw "$ExpectedCommand PID is not an exact JSON integer: $actualPidType"
    }
    $rawPid = [int64]$instance.pid
    if ($rawPid -le 0 -or $rawPid -gt [uint32]::MaxValue -or $rawPid -eq $PID) {
        throw "$ExpectedCommand returned an unsafe process id: $rawPid"
    }

    $executablePathText = [string]$instance.executable_path
    if ([string]::IsNullOrWhiteSpace($executablePathText) -or
        -not [System.IO.Path]::IsPathFullyQualified($executablePathText)) {
        throw "$ExpectedCommand executable path is not an exact absolute path: $executablePathText"
    }
    $jsonPath = ConvertTo-ComparablePath $executablePathText
    $expectedPath = ConvertTo-ComparablePath $ExpectedExecutable
    if (-not $jsonPath.Equals($expectedPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "$ExpectedCommand executable path mismatch: expected $expectedPath, found $jsonPath"
    }

    return [pscustomobject][ordered]@{
        Document = $Document
        Command = [string]$Document.command
        State = $state
        Phase = $phase
        InstanceId = $instanceId
        ProcessId = [uint32]$rawPid
        ExecutablePath = $jsonPath
    }
}

function Assert-AuditDaemonReadinessDeadline {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.Stopwatch]$Stopwatch,
        [Parameter(Mandatory = $true)][ValidateRange(1, [int]::MaxValue)][int]$OverallTimeoutMs
    )
    if ((Get-AuditElapsedCeilingMs -Stopwatch $Stopwatch) -ge $OverallTimeoutMs) {
        throw "audit daemon readiness timed out after ${OverallTimeoutMs}ms"
    }
}

function Wait-AuditDaemonReadiness {
    param(
        [Parameter(Mandatory = $true)]$DaemonStartDocument,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable,
        [Parameter(Mandatory = $true)][string]$Repository,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $evidenceKey = 'ColayStressAuditDaemonReadinessEvidence'
    $polls = [System.Collections.Generic.List[object]]::new()
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $cleanupBudgetMs = $script:AuditDaemonReadinessExitWaitLimitMs +
        $script:AuditDaemonReadinessOutputDrainLimitMs
    if ($cleanupBudgetMs -ne $script:AuditDaemonReadinessCleanupReserveMs) {
        throw 'audit daemon readiness cleanup reserve does not equal its exit and output-drain limits'
    }
    $evidence = [pscustomobject][ordered]@{
        readiness_status = 'failed'
        original_state = $null
        final_state = $null
        poll_count = 0
        elapsed_ms = 0
        overall_timeout_ms = $script:AuditDaemonReadinessTimeoutMs
        poll_interval_ms = $script:AuditDaemonReadinessPollIntervalMs
        cleanup_reserve_ms = $script:AuditDaemonReadinessCleanupReserveMs
        exit_wait_limit_ms = $script:AuditDaemonReadinessExitWaitLimitMs
        output_drain_limit_ms = $script:AuditDaemonReadinessOutputDrainLimitMs
        status_command = @('--json', 'daemon', 'status')
        anchored_identity = $null
        polls = @()
        online_document = $null
        failure = $null
    }
    try {
        if ($script:AuditDaemonReadinessInitialParseDelayForTestMs -gt 0) {
            Start-Sleep -Milliseconds $script:AuditDaemonReadinessInitialParseDelayForTestMs
        }
        [void](Assert-AuditDaemonReadinessDeadline -Stopwatch $stopwatch `
            -OverallTimeoutMs $script:AuditDaemonReadinessTimeoutMs)
        $anchor = ConvertTo-AuditDaemonDocumentIdentity -Document $DaemonStartDocument `
            -ExpectedCommand daemon_start -ExpectedExecutable $ExpectedExecutable
        $evidence.original_state = $anchor.State
        $evidence.final_state = $anchor.State
        $evidence.anchored_identity = [pscustomobject][ordered]@{
            instance_id = $anchor.InstanceId
            process_id = [int64]$anchor.ProcessId
            executable_path = $anchor.ExecutablePath
        }
        [void](Assert-AuditDaemonReadinessDeadline -Stopwatch $stopwatch `
            -OverallTimeoutMs $script:AuditDaemonReadinessTimeoutMs)
        if (@('booting', 'probing', 'online') -cnotcontains $anchor.State) {
            throw "audit daemon readiness start returned terminal or non-progress state '$($anchor.State)'"
        }
        if ($anchor.State -ceq 'online') {
            $evidence.readiness_status = 'online'
            $evidence.online_document = $DaemonStartDocument
            $evidence.elapsed_ms = Get-AuditElapsedCeilingMs -Stopwatch $stopwatch
            return [pscustomobject][ordered]@{
                Evidence = $evidence
                OnlineDocument = $DaemonStartDocument
            }
        }

        while ($true) {
            $remainingBeforeSleepMs = $script:AuditDaemonReadinessTimeoutMs -
                (Get-AuditElapsedCeilingMs -Stopwatch $stopwatch)
            $sleepBudgetMs = $remainingBeforeSleepMs - $script:AuditDaemonReadinessCleanupReserveMs
            if ($sleepBudgetMs -le 0) {
                throw "audit daemon readiness timed out after $($script:AuditDaemonReadinessTimeoutMs)ms"
            }
            $sleepMs = [int][Math]::Min($script:AuditDaemonReadinessPollIntervalMs, $sleepBudgetMs)
            Start-Sleep -Milliseconds $sleepMs

            $remainingMs = $script:AuditDaemonReadinessTimeoutMs -
                (Get-AuditElapsedCeilingMs -Stopwatch $stopwatch)
            $commandBudgetMs = [int]($remainingMs - $script:AuditDaemonReadinessCleanupReserveMs)
            if ($commandBudgetMs -le 0) {
                throw "audit daemon readiness timed out after $($script:AuditDaemonReadinessTimeoutMs)ms"
            }
            $pollNumber = $polls.Count + 1
            $commandLabel = "$Label-daemon-readiness-{0:D3}" -f $pollNumber
            $pollEvidence = [pscustomobject][ordered]@{
                poll = $pollNumber
                command_label = $commandLabel
                command_timeout_ms = $commandBudgetMs
                remaining_at_launch_ms = $remainingMs
                exit_wait_limit_ms = $script:AuditDaemonReadinessExitWaitLimitMs
                output_drain_limit_ms = $script:AuditDaemonReadinessOutputDrainLimitMs
                total_operation_budget_ms = $commandBudgetMs + $cleanupBudgetMs
                observed_elapsed_ms = Get-AuditElapsedCeilingMs -Stopwatch $stopwatch
                state = $null
                phase = $null
                instance_id = $null
                process_id = $null
                executable_path = $null
            }
            $polls.Add($pollEvidence)
            $evidence.poll_count = $polls.Count
            $evidence.polls = $polls.ToArray()
            try {
                $statusDocument = Invoke-ColayDocument -Repository $Repository `
                    -Arguments @('--json', 'daemon', 'status') -Label $commandLabel `
                    -TimeoutMs $commandBudgetMs -OverallDeadlineStopwatch $stopwatch `
                    -OverallDeadlineMs $script:AuditDaemonReadinessTimeoutMs `
                    -ExitWaitLimitMs $script:AuditDaemonReadinessExitWaitLimitMs `
                    -OutputDrainLimitMs $script:AuditDaemonReadinessOutputDrainLimitMs
            } catch {
                if ($null -ne $script:LastChildProcessCleanup -and
                    [string]$script:LastChildProcessCleanup.label -ceq $commandLabel -and
                    [bool]$script:LastChildProcessCleanup.deadline_aware) {
                    $pollEvidence.remaining_at_launch_ms = [int64]$script:LastChildProcessCleanup.remaining_at_launch_ms
                    $pollEvidence.command_timeout_ms = [int]$script:LastChildProcessCleanup.command_timeout_ms
                    $pollEvidence.exit_wait_limit_ms = [int]$script:LastChildProcessCleanup.exit_wait_limit_ms
                    $pollEvidence.output_drain_limit_ms = [int]$script:LastChildProcessCleanup.output_drain_limit_ms
                    $pollEvidence.total_operation_budget_ms = [int]$script:LastChildProcessCleanup.total_operation_budget_ms
                }
                throw
            }
            if ($null -ne $script:LastChildProcessCleanup -and
                [string]$script:LastChildProcessCleanup.label -ceq $commandLabel -and
                [bool]$script:LastChildProcessCleanup.deadline_aware) {
                $pollEvidence.remaining_at_launch_ms = [int64]$script:LastChildProcessCleanup.remaining_at_launch_ms
                $pollEvidence.command_timeout_ms = [int]$script:LastChildProcessCleanup.command_timeout_ms
                $pollEvidence.exit_wait_limit_ms = [int]$script:LastChildProcessCleanup.exit_wait_limit_ms
                $pollEvidence.output_drain_limit_ms = [int]$script:LastChildProcessCleanup.output_drain_limit_ms
                $pollEvidence.total_operation_budget_ms = [int]$script:LastChildProcessCleanup.total_operation_budget_ms
            }
            $pollEvidence.observed_elapsed_ms = Get-AuditElapsedCeilingMs -Stopwatch $stopwatch
            [void](Assert-AuditDaemonReadinessDeadline -Stopwatch $stopwatch `
                -OverallTimeoutMs $script:AuditDaemonReadinessTimeoutMs)
            $statusIdentity = ConvertTo-AuditDaemonDocumentIdentity -Document $statusDocument `
                -ExpectedCommand daemon_status -ExpectedExecutable $ExpectedExecutable
            $pollEvidence.state = $statusIdentity.State
            $pollEvidence.phase = $statusIdentity.Phase
            $pollEvidence.instance_id = $statusIdentity.InstanceId
            $pollEvidence.process_id = [int64]$statusIdentity.ProcessId
            $pollEvidence.executable_path = $statusIdentity.ExecutablePath
            if ($statusIdentity.InstanceId -cne $anchor.InstanceId -or
                $statusIdentity.ProcessId -ne $anchor.ProcessId -or
                -not $statusIdentity.ExecutablePath.Equals(
                    $anchor.ExecutablePath,
                    [System.StringComparison]::OrdinalIgnoreCase
                )) {
                throw "audit daemon readiness identity drift at status poll $pollNumber"
            }
            [void](Assert-AuditDaemonReadinessDeadline -Stopwatch $stopwatch `
                -OverallTimeoutMs $script:AuditDaemonReadinessTimeoutMs)

            $evidence.final_state = $statusIdentity.State
            if ($statusIdentity.State -ceq 'online') {
                $evidence.readiness_status = 'online'
                $evidence.online_document = $statusDocument
                $evidence.elapsed_ms = Get-AuditElapsedCeilingMs -Stopwatch $stopwatch
                return [pscustomobject][ordered]@{
                    Evidence = $evidence
                    OnlineDocument = $statusDocument
                }
            }
            if (@('booting', 'probing') -cnotcontains $statusIdentity.State) {
                throw "audit daemon readiness status poll $pollNumber returned terminal or non-progress state '$($statusIdentity.State)'"
            }
        }
    } catch {
        $evidence.poll_count = $polls.Count
        $evidence.polls = $polls.ToArray()
        $evidence.elapsed_ms = Get-AuditElapsedCeilingMs -Stopwatch $stopwatch
        $evidence.failure = $_.Exception.Message
        $_.Exception.Data[$evidenceKey] = $evidence
        throw
    } finally {
        $stopwatch.Stop()
    }
}

function Get-SqliteFamilyHashes {
    param([string]$Database)
    $hashes = [ordered]@{}
    foreach ($suffix in @('', '-wal', '-shm', '-journal')) {
        $candidate = $Database + $suffix
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $hashes[$suffix] = [ordered]@{
                bytes = (Get-Item -LiteralPath $candidate).Length
                sha256 = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        }
    }
    if (-not $hashes.Contains('')) { throw "source SQLite family is missing its primary database: $Database" }
    return $hashes
}

function Assert-JsonEquivalent {
    param($Expected, $Actual, [string]$Label)
    $expectedJson = $Expected | ConvertTo-Json -Depth 30 -Compress
    $actualJson = $Actual | ConvertTo-Json -Depth 30 -Compress
    if ($expectedJson -cne $actualJson) { throw "$Label mismatch: expected $expectedJson, found $actualJson" }
}

$pythonCode = @"
import json
import pathlib
import sqlite3
import sys

database = pathlib.Path(sys.argv[1]).resolve()
connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)
counts = {}
for table in ("workspaces", "workspace_paths", "legacy_imports", "sessions"):
    counts[table] = connection.execute("SELECT count(*) FROM " + table).fetchone()[0]
row = connection.execute(
    "SELECT wp.workspace_id, wp.canonical_path, li.source_fingerprint, li.manifest_hash, li.result_json "
    "FROM legacy_imports li JOIN workspace_paths wp ON wp.workspace_id = li.workspace_id "
    "WHERE wp.is_current = 1"
).fetchone()
payload = {
    "counts": counts,
    "integrity": connection.execute("PRAGMA integrity_check").fetchone()[0],
    "foreign_key_violations": len(connection.execute("PRAGMA foreign_key_check").fetchall()),
    "live_lease_count": connection.execute(
        "SELECT count(*) FROM daemon_instances WHERE released_at IS NULL"
    ).fetchone()[0],
    "import": None if row is None else {
        "workspace_id": row[0],
        "canonical_path": row[1],
        "source_fingerprint": row[2],
        "manifest_hash": row[3],
        "result": json.loads(row[4]),
    },
}
connection.close()
print(json.dumps(payload, separators=(",", ":")))
"@

function Get-DurableState {
    $line = Invoke-ChildProcessLine -Executable $PythonExe `
        -Arguments @('-I', '-c', $pythonCode, (Join-Path $AuditColayHome 'state/state.db')) `
        -WorkingDirectory $EmptyRepository -Label 'audit durable-state query' -TimeoutMs 30000
    return $line | ConvertFrom-Json -Depth 40
}

function Assert-AttributedMarker {
    param([Parameter(Mandatory = $true)][string]$ExpectedGroup)
    $root = Get-Item -LiteralPath $MarkerDirectory -Force -ErrorAction Stop
    if (-not $root.PSIsContainer -or ($root.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw 'audit marker root is not a regular directory'
    }
    $groups = @(Get-ChildItem -LiteralPath $MarkerDirectory -Force -ErrorAction Stop)
    if ($groups.Count -ne 1 -or -not $groups[0].PSIsContainer -or
        ($groups[0].Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
        $groups[0].Name -cne $ExpectedGroup) {
        throw 'audit marker root did not contain exactly the durable opaque source group'
    }
    $events = @(Get-ChildItem -LiteralPath $groups[0].FullName -Force -ErrorAction Stop)
    if ($events.Count -ne 2) { throw "audit marker group contained $($events.Count) events; expected 2" }
    $names = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::Ordinal)
    foreach ($event in $events) {
        if ($event.PSIsContainer -or ($event.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
            $event.Length -ne 0 -or $event.Name -cnotmatch '^event-[0-9]+-[0-9]+$' -or
            -not $names.Add($event.Name)) {
            throw "invalid audit marker event: $($event.FullName)"
        }
    }
}

$processLineFailureSelfTest = Invoke-ChildProcessLineFailureSelfTest
$primaryFailure = $null
$cleanupFailures = [System.Collections.Generic.List[string]]::new()
$durable = $null
$daemonReadiness = $null
try {
    if (Test-Path -LiteralPath (Join-Path $AuditColayHome 'state/state.db')) {
        throw 'audit COLAY_HOME had a pre-existing global database'
    }
    if (@(Get-ChildItem -LiteralPath $MarkerDirectory -Force -ErrorAction Stop).Count -ne 0) {
        throw 'audit marker directory was not empty before daemon start'
    }
    $started = Invoke-ColayDocument -Repository $EmptyRepository -Arguments @('--json', 'daemon', 'start') `
        -Label 'audit daemon start' -TimeoutMs 40000
    $readiness = Wait-AuditDaemonReadiness -DaemonStartDocument $started `
        -ExpectedExecutable $ColayExe -Repository $EmptyRepository -Label 'audit'
    $daemonReadiness = $readiness.Evidence

    $registered = Invoke-ColayDocument -Repository $LegacyRepository -Arguments @('--json', 'status') `
        -Label 'audit legacy registration' -TimeoutMs 40000
    if ([string]$registered.schema_version -cne '1' -or [string]$registered.command -cne 'status' -or
        -not [bool]$registered.data.database.integrity_ok -or
        [int]$registered.data.database.foreign_key_violations -ne 0) {
        throw 'audit legacy status did not return exact healthy schema-v1 status'
    }

    $durable = Get-DurableState
    foreach ($name in @('workspaces', 'workspace_paths')) {
        if ([int]$durable.counts.$name -ne 2) { throw "audit durable $name count was not 2" }
    }
    foreach ($name in @('legacy_imports', 'sessions')) {
        if ([int]$durable.counts.$name -ne 1) { throw "audit durable $name count was not 1" }
    }
    if ([string]$durable.integrity -cne 'ok' -or [int]$durable.foreign_key_violations -ne 0 -or
        $null -eq $durable.import -or -not [bool]$durable.import.result.imported) {
        throw 'audit durable import health or result was invalid'
    }
    $sourceRootHash = [string]$durable.import.result.source_root_hash
    if ($sourceRootHash -cnotmatch '^[0-9a-f]{64}$') { throw 'audit durable source_root_hash was malformed' }
    if ((ConvertTo-ComparablePath ([string]$durable.import.canonical_path)) -cne
        (ConvertTo-ComparablePath $LegacyRepository)) { throw 'audit durable canonical path did not match legacy repository' }
    $expectedPublication = Join-Path `
        (Join-Path (Join-Path (Join-Path $AuditColayHome 'data/workspaces') ([string]$durable.import.workspace_id)) 'imports') `
        ([string]$durable.import.source_fingerprint)
    if ((ConvertTo-ComparablePath ([string]$durable.import.result.published_path)) -cne
        (ConvertTo-ComparablePath $expectedPublication)) { throw 'audit durable publication path was not exact' }
    $expectedPublicationDatabase = Join-Path $expectedPublication 'legacy.db'
    $publicationChildren = @(Get-ChildItem -LiteralPath $expectedPublication -Force -ErrorAction Stop)
    if ($publicationChildren.Count -ne 1 -or $publicationChildren[0].Name -cne 'legacy.db' -or
        $publicationChildren[0].PSIsContainer -or
        ($publicationChildren[0].Attributes -band [System.IO.FileAttributes]::ReparsePoint) -or
        (ConvertTo-ComparablePath $publicationChildren[0].FullName) -cne
        (ConvertTo-ComparablePath $expectedPublicationDatabase)) {
        throw 'audit controlled-source publication did not contain exactly one regular legacy.db child'
    }
    Assert-AttributedMarker -ExpectedGroup $sourceRootHash
    $expectedSourceHashes = $ExpectedSourceHashesJson | ConvertFrom-Json -AsHashtable -Depth 30
    Assert-JsonEquivalent $expectedSourceHashes (Get-SqliteFamilyHashes $LegacyDatabase) 'audit source SQLite family'
    $actualConfigHash = (Get-FileHash -LiteralPath (Join-Path $LegacyRepository '.colay/config.toml') -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualConfigHash -cne $ExpectedConfigSha256) { throw 'audit legacy config hash changed' }
}
catch {
    if ($_.Exception.Data.Contains('ColayStressAuditDaemonReadinessEvidence')) {
        $daemonReadiness = $_.Exception.Data['ColayStressAuditDaemonReadinessEvidence']
    }
    $primaryFailure = $_
}
finally {
    try {
        $stopped = Invoke-ColayDocument -Repository $EmptyRepository -Arguments @('--json', 'daemon', 'stop') `
            -Label 'audit daemon stop' -TimeoutMs 30000
        if ([string]$stopped.schema_version -cne '1' -or [string]$stopped.command -cne 'daemon_stop' -or
            [string]$stopped.data.status.state -cne 'stopped') { throw 'audit daemon stop did not return exact stopped status' }
    } catch { $cleanupFailures.Add("daemon stop: $($_.Exception.Message)") }
    try {
        $status = Invoke-ColayDocument -Repository $EmptyRepository -Arguments @('--json', 'daemon', 'status') `
            -Label 'audit endpoint status after stop' -TimeoutMs 20000
        if ([string]$status.schema_version -cne '1' -or [string]$status.command -cne 'daemon_status' -or
            [string]$status.data.status.state -cne 'stopped') { throw 'audit endpoint remained present after stop' }
    } catch { $cleanupFailures.Add("endpoint status: $($_.Exception.Message)") }
    if (Test-Path -LiteralPath (Join-Path $AuditColayHome 'state/state.db') -PathType Leaf) {
        try {
            $deadline = [datetime]::UtcNow.AddSeconds(10)
            do {
                $durableAfterStop = Get-DurableState
                if ([int]$durableAfterStop.live_lease_count -eq 0) { break }
                Start-Sleep -Milliseconds 50
            } while ([datetime]::UtcNow -lt $deadline)
            if ([int]$durableAfterStop.live_lease_count -ne 0) { throw 'audit live lease remained after stop' }
        } catch { $cleanupFailures.Add("live lease: $($_.Exception.Message)") }
    }
}

if ($null -ne $primaryFailure -or $cleanupFailures.Count -ne 0) {
    $primaryFailureMessage = if ($null -eq $primaryFailure) { $null } else { $primaryFailure.Exception.Message }
    $cleanupFailureMessages = $cleanupFailures.ToArray()
    $failureMessage = if ($null -eq $primaryFailureMessage) {
        "audit cleanup failed: $($cleanupFailureMessages -join '; ')"
    } else {
        $primaryFailureMessage
    }
    if ($null -ne $primaryFailureMessage -and $cleanupFailureMessages.Count -ne 0) {
        $failureMessage += "; audit cleanup failed: $($cleanupFailureMessages -join '; ')"
    }
    $failureEvidence = [pscustomobject][ordered]@{
        schema_version = '1'
        status = 'failed'
        failure = $failureMessage
        primary_failure = $primaryFailureMessage
        cleanup_failures = $cleanupFailureMessages
        cleanup_state = if ($cleanupFailureMessages.Count -eq 0) { 'stopped' } else { 'failed' }
        daemon_readiness = $daemonReadiness
        process_line_failure_self_test = $processLineFailureSelfTest
    }
    try {
        [Console]::Out.WriteLine((ConvertTo-AuditChildJson -Value $failureEvidence))
        [Console]::Out.Flush()
    } catch {
        $failureMessage += "; audit failure evidence write failed: $($_.Exception.Message)"
    }
    throw $failureMessage
}
$successEvidence = [pscustomobject]@{
    schema_version = '1'
    status = 'passed'
    imported_workspace_id = [string]$durable.import.workspace_id
    source_root_hash = [string]$durable.import.result.source_root_hash
    daemon_readiness = $daemonReadiness
    cleanup_state = 'stopped'
    process_line_failure_self_test = $processLineFailureSelfTest
}
[Console]::Out.WriteLine((ConvertTo-AuditChildJson -Value $successEvidence))
[Console]::Out.Flush()
'@
    Set-Content -LiteralPath $Path -Value $scriptSource -Encoding utf8NoBOM
}

function Assert-StrongProcessAuditEvidence {
    param(
        [Parameter(Mandatory = $true)]$HelperResult,
        [Parameter(Mandatory = $true)]$Evidence,
        [Parameter(Mandatory = $true)][string]$ExpectedPowerShell,
        [Parameter(Mandatory = $true)][string]$ExpectedColay,
        [Parameter(Mandatory = $true)][string]$ExpectedPython,
        [Parameter(Mandatory = $true)][string]$ExpectedFakeProvider,
        [Parameter(Mandatory = $true)][string]$ExpectedWorkingDirectory,
        [Parameter(Mandatory = $true)][int]$ExpectedTimeoutMs,
        [Parameter(Mandatory = $true)][int]$ExpectedArgumentCount,
        [Parameter(Mandatory = $true)][string[]]$ExpectedEnvironmentNames
    )
    if ($HelperResult.exit_code -ne 0) { throw "process audit helper exited with code $($HelperResult.exit_code)" }
    if ([int]$Evidence.schema_version -ne 1 -or [string]$Evidence.status -cne 'success' -or
        $null -ne $Evidence.observer_error -or [int]$Evidence.child_exit_code -ne 0) {
        throw 'process audit helper did not report exact schema-v1 observer success and child exit 0'
    }
    if ([string]$Evidence.environment_mode -cne 'clear' -or
        (ConvertTo-ComparableWindowsPath ([string]$Evidence.executable)) -cne
        (ConvertTo-ComparableWindowsPath $ExpectedPowerShell)) {
        throw 'process audit helper did not root at exact portable pwsh with a cleared environment'
    }
    if ([int]$Evidence.argument_count -ne $ExpectedArgumentCount -or
        [int]$Evidence.timeout_ms -ne $ExpectedTimeoutMs -or
        (ConvertTo-ComparableWindowsPath ([string]$Evidence.working_directory)) -cne
        (ConvertTo-ComparableWindowsPath $ExpectedWorkingDirectory)) {
        throw 'process audit helper argument count, working directory, or timeout did not match the exact launch contract'
    }
    Assert-EquivalentJson @($ExpectedEnvironmentNames | Sort-Object) `
        @($Evidence.environment_override_names | Sort-Object) `
        'process audit explicit environment override names'
    $active = @($Evidence.active_process_ids_at_finish)
    if ($active.Count -ne 0) { throw "process audit helper active set was not empty: $($active -join ', ')" }
    $starts = @($Evidence.process_starts)
    $exits = @($Evidence.process_exits)
    if ($starts.Count -eq 0 -or $starts.Count -ne $exits.Count) {
        throw "process audit start/exit cardinality mismatch: starts=$($starts.Count), exits=$($exits.Count)"
    }
    $startCounts = @{}
    $exitCounts = @{}
    foreach ($record in $starts) {
        $key = [string][uint32]$record.process_id
        $startCounts[$key] = 1 + [int]$startCounts[$key]
        if ([string]::IsNullOrWhiteSpace([string]$record.path)) { throw "process audit start $key had no resolved image path" }
    }
    foreach ($record in $exits) {
        $key = [string][uint32]$record.process_id
        $exitCounts[$key] = 1 + [int]$exitCounts[$key]
    }
    Assert-EquivalentJson -Expected $startCounts -Actual $exitCounts `
        -Label 'process audit start/exit PID multiset'
    $startPaths = @($starts | ForEach-Object { [string]$_.path })
    if ((ConvertTo-ComparableWindowsPath $startPaths[0]) -cne (ConvertTo-ComparableWindowsPath $ExpectedPowerShell)) {
        throw 'process audit first observed process was not exact portable pwsh'
    }
    $expectedPathsByBasename = @{}
    foreach ($expectedPath in @($ExpectedPowerShell, $ExpectedColay, $ExpectedPython, $ExpectedFakeProvider)) {
        $basename = [System.IO.Path]::GetFileName($expectedPath).ToLowerInvariant()
        if ($expectedPathsByBasename.ContainsKey($basename)) {
            throw "process audit expected path basenames were not unique: $basename"
        }
        $expectedPathsByBasename[$basename] = $expectedPath
    }
    foreach ($startPath in $startPaths) {
        $basename = [System.IO.Path]::GetFileName($startPath).ToLowerInvariant()
        if ($expectedPathsByBasename.ContainsKey($basename) -and
            (ConvertTo-ComparableWindowsPath $startPath) -cne
            (ConvertTo-ComparableWindowsPath ([string]$expectedPathsByBasename[$basename]))) {
            throw "process audit observed $basename from a path other than its exact resolved candidate: $startPath"
        }
    }
    $namedColayStarts = @($startPaths | Where-Object {
        [System.IO.Path]::GetFileName($_).Equals('colay.exe', [System.StringComparison]::OrdinalIgnoreCase)
    })
    $colayStarts = @($namedColayStarts | Where-Object {
        (ConvertTo-ComparableWindowsPath $_) -ceq (ConvertTo-ComparableWindowsPath $ExpectedColay)
    })
    if ($namedColayStarts.Count -ne $colayStarts.Count) {
        throw 'process audit observed a colay.exe basename from a path other than the exact resolved candidate'
    }
    if ($colayStarts.Count -lt 4) {
        throw "process audit observed only $($colayStarts.Count) exact colay starts; expected client, daemon, registration, and cleanup coverage"
    }
    $namedFakeStarts = @($startPaths | Where-Object {
        [System.IO.Path]::GetFileName($_).Equals('colay-e2e-fake-provider.exe', [System.StringComparison]::OrdinalIgnoreCase)
    })
    $exactFakeStarts = @($namedFakeStarts | Where-Object {
        (ConvertTo-ComparableWindowsPath $_) -ceq (ConvertTo-ComparableWindowsPath $ExpectedFakeProvider)
    })
    if ($namedFakeStarts.Count -ne $exactFakeStarts.Count) {
        throw 'process audit observed a fake-provider basename from a path other than the exact test-support binary'
    }
    $pythonStarts = @($startPaths | Where-Object {
        [System.IO.Path]::GetFileName($_).Equals(
            [System.IO.Path]::GetFileName($ExpectedPython),
            [System.StringComparison]::OrdinalIgnoreCase
        )
    })
    if ($pythonStarts.Count -lt 1) {
        throw 'process audit did not observe the required exact Python SQLite child process'
    }
    $forbidden = @($startPaths | Where-Object {
        ([System.IO.Path]::GetFileName($_)).ToLowerInvariant() -in @('whoami.exe', 'icacls.exe')
    })
    if ($forbidden.Count -ne 0) { throw "process audit evidence contains forbidden utility starts: $($forbidden -join ', ')" }
    return [pscustomobject]@{
        status = 'passed'
        start_count = $starts.Count
        exit_count = $exits.Count
        colay_start_count = $colayStarts.Count
        python_start_count = $pythonStarts.Count
        python_start_required = $true
        fake_provider_start_count = $exactFakeStarts.Count
        fake_provider_start_required = $false
        observed_expected_basename_paths_exact = $true
        active_process_ids_at_finish = @()
        forbidden_utility_starts = @()
    }
}

$runStamp = [datetime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
$summary = [ordered]@{
    schema_version = 2
    run_id = $runStamp
    started_at_utc = [datetime]::UtcNow.ToString('o')
    completed_at_utc = $null
    status = 'failed'
    failure = $null
    source_commit = $null
    source_identity = [pscustomobject][ordered]@{
        intended_commit = $ExpectedSourceCommit
        actual_commit = $null
        verified_commit = $null
        verification_status = 'pending'
        working_tree = [pscustomobject][ordered]@{
            status = 'pending'
            git_status_exit_code = $null
            entry_count = $null
            porcelain_v1_sha256 = $null
            tracked_changes_included = $true
            staged_changes_included = $true
            untracked_files_included = $true
            ignored_files_included = $false
        }
    }
    harness = $null
    measurement_method = 'os-process-lifetime'
    response_timeout_ms = $ResponseTimeoutMs
    serial_max_limit_ms = $SerialMaxLimitMs
    serial_p95_limit_ms = $SerialP95LimitMs
    concurrent_limit_ms = $ConcurrentLimitMs
    serial_times_ms = @()
    serial_max_ms = $null
    serial_p95_ms = $null
    concurrent_times_ms = @()
    concurrent_max_ms = $null
    acceptance_failures = @()
    measurement_diagnostics = [pscustomobject][ordered]@{
        main_daemon_readiness = $null
        latency_source_preparation = $null
        source_clean_tree_command_evidence_redaction_self_test = $null
        early_failure_input_identity_self_test = $null
        timing_self_test = $null
        timing_self_test_failure_cleanup = $null
        failure_cleanup_self_test = $null
        concurrent_observer_wall_ms = $null
        concurrent_start_cleanup = @()
        command_timings = @()
    }
    inspection_count = $null
    marker_phase_policy = 'split-latency-marker-off-and-correctness-marker-on-phases'
    inspection_markers = [pscustomobject][ordered]@{
        latency_phase = [pscustomobject][ordered]@{
            marker_phase = 'LatencyAttributedOff'
            aggregate_file = $null
            aggregate_count = $null
            attributed_environment_key_present = $null
            attributed_sentinel_directory = $null
            attributed_group_count = $null
            attributed_event_count = $null
            groups = @()
            timing_included_in_latency_thresholds = $true
        }
        correctness_phase = [pscustomobject][ordered]@{
            marker_phase = 'CorrectnessAttributedOn'
            aggregate_file = $null
            aggregate_count = $null
            attributed_environment_key_present = $null
            attributed_directory = $null
            attributed_group_count = $null
            attributed_event_count = $null
            groups = @()
            source_root_hash = $null
            source_root_hash_matches_group = $null
            timing_included_in_latency_thresholds = $false
        }
    }
    forbidden_utility_launches = @()
    process_ownership_refusals = @()
    residual_processes = @()
    sqlite_integrity = $null
    sqlite_foreign_key_violations = $null
    zero_writable_rows = $null
    durable_state = $null
    sources = @()
    minimum_free_gib = $null
    disk_volumes = @()
    cleanup = [pscustomobject][ordered]@{
        daemon_stop = $null
        endpoint_status = $null
        live_lease_count = $null
        residual_processes_before_force = @()
        residual_processes_after_force = @()
        force_process_cleanup = $null
        audit_daemon_stop = $null
        audit_endpoint_status = $null
        audit_live_lease_count = $null
        observer_teardown = $null
    }
    cleanup_errors = @()
    process_audit = [pscustomobject][ordered]@{
        static_scan = $null
        helper_build = $null
        functional = $null
    }
    provider_key_names_cleared = $ProviderKeyNames
    fake_provider_only = $true
    sqlite_runtime = $null
    binaries = $null
    runtime_root = $null
}
$failureRecord = $null
$bodySucceeded = $false
$evidenceDirectory = $null
$environment = $null
$emptyRepository = $null
$globalDatabase = $null
$resolvedFake = $null
$auditEnvironment = $null
$auditRuntimeRoot = $null
$auditColayHome = $null
$auditEmptyRepository = $null
$auditGlobalDatabase = $null
$auditHelperExe = $null
$auditPowerShell = $null
$latencySeeds = $null

try {
    if ($ExpectedSourceCommit -cnotmatch '^[0-9a-f]{40}$') {
        $summary.source_identity.verification_status = 'invalid'
        throw "ExpectedSourceCommit must be one exact lowercase 40-hex Git commit: $ExpectedSourceCommit"
    }
    if (-not $IsWindows) { throw 'windows-state-acl-stress.ps1 must run on native Windows' }
    $script:RepoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
    $resolvedHarness = Resolve-RequiredFile $PSCommandPath 'Windows state ACL stress harness'
    $summary.harness = [pscustomobject]@{ path = $resolvedHarness; sha256 = Get-Sha256 $resolvedHarness }
    $script:ResolvedColay = Resolve-RequiredFile $ColayExe 'Colay executable'
    $resolvedFake = Resolve-RequiredFile $FakeProviderExe 'fake provider executable'
    if ([System.IO.Path]::GetFileName($resolvedFake) -cne 'colay-e2e-fake-provider.exe') {
        throw "only the colay-e2e-fake-provider.exe test-support binary is allowed"
    }
    $summary.binaries = [pscustomobject]@{
        colay = [pscustomobject]@{ path = $script:ResolvedColay; sha256 = Get-Sha256 $script:ResolvedColay }
        fake_provider = [pscustomobject]@{ path = $resolvedFake; sha256 = Get-Sha256 $resolvedFake }
    }
    $script:PythonExe = (Get-Command python.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $resolvedEvidenceRoot = [System.IO.Path]::GetFullPath($EvidenceRoot)
    $script:RunRoot = Join-Path ([System.IO.Path]::GetTempPath()) "colay-acl-$runStamp"
    if (Test-Path -LiteralPath $script:RunRoot) {
        throw "isolated runtime root already exists: $script:RunRoot"
    }
    Register-DiskVolume -Path $resolvedEvidenceRoot -Label 'evidence_root'
    Register-DiskVolume -Path $script:RunRoot -Label 'runtime_root'
    Assert-FreeDisk | Out-Null
    New-Item -ItemType Directory -Path $resolvedEvidenceRoot -Force | Out-Null
    $evidenceDirectory = Join-Path $resolvedEvidenceRoot "run-$runStamp"
    New-Item -ItemType Directory -Path $evidenceDirectory -Force | Out-Null
    $summary.runtime_root = $script:RunRoot
    $script:ColayHome = Join-Path $script:RunRoot 'colay-home'
    $workspaceRoot = Join-Path $script:RunRoot 'workspaces'
    $emptyRepository = Join-Path $workspaceRoot 'empty-incumbent'
    $inspectionMarker = Join-Path $script:RunRoot 'temp/legacy-inspections.log'
    $inspectionMarkerDirectory = Join-Path $script:RunRoot 'temp/legacy-inspection-groups'
    foreach ($directory in @($script:RunRoot, $workspaceRoot, $emptyRepository, $script:ColayHome)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $environment = New-IsolatedEnvironment -ColayHomePath $script:ColayHome -Root $script:RunRoot `
        -FakeProvider $resolvedFake -InspectionMarker $inspectionMarker `
        -InspectionMarkerDirectory $inspectionMarkerDirectory -MarkerPhase LatencyAttributedOff
    New-Item -ItemType Directory -Path $inspectionMarkerDirectory -ErrorAction Stop | Out-Null
    $summary.inspection_markers.latency_phase.aggregate_file = $inspectionMarker
    $summary.inspection_markers.latency_phase.attributed_environment_key_present = `
        $environment.Contains('COLAY_TEST_LEGACY_INSPECT_MARKER_DIR')
    $summary.inspection_markers.latency_phase.attributed_sentinel_directory = $inspectionMarkerDirectory
    if ($summary.inspection_markers.latency_phase.attributed_environment_key_present) {
        throw 'latency environment unexpectedly contains the attributed marker key'
    }
    New-FakeProviderConfig -ColayHomePath $script:ColayHome -FakeProvider $resolvedFake

    $timeoutSource = Get-Content -LiteralPath (Join-Path $script:RepoRoot 'crates/orchestrator-cli/src/ipc_client.rs') -Raw
    if ($timeoutSource -notmatch 'RESPONSE_TIMEOUT\s*:\s*Duration\s*=\s*Duration::from_secs\(10\)') {
        throw 'source RESPONSE_TIMEOUT is not exactly Duration::from_secs(10)'
    }
    if ($ResponseTimeoutMs -ne 10000) { throw 'harness response timeout invariant changed' }
    Assert-FreeDisk | Out-Null
    Start-AncestryObservation
    try {
        $gitExe = (Get-Command git.exe -CommandType Application -ErrorAction Stop |
            Select-Object -First 1).Source
        $gitResult = Invoke-HarnessProcess -Executable $gitExe `
            -ArgumentValues @('-C', $script:RepoRoot, 'rev-parse', 'HEAD') `
            -WorkingDirectory $script:RepoRoot -Environment $environment -Label 'source-commit' `
            -TimeoutMs 10000 -StandardInputText $null -AllowFailure
    } catch {
        $summary.source_identity.verification_status = 'invalid'
        throw 'git rev-parse HEAD preflight process failed'
    }
    if ([int]$gitResult.exit_code -ne 0) {
        $summary.source_identity.verification_status = 'invalid'
        throw "git rev-parse HEAD preflight failed with exit code $($gitResult.exit_code)"
    }
    $actualSourceCommit = if ($null -eq $gitResult.stdout) { '' } else { ([string]$gitResult.stdout).Trim() }
    $summary.source_identity.actual_commit = $actualSourceCommit
    if ($actualSourceCommit -cnotmatch '^[0-9a-f]{40}$') {
        $summary.source_identity.verification_status = 'invalid'
        throw "git rev-parse returned a malformed source commit: $actualSourceCommit"
    }
    if ($actualSourceCommit -cne $ExpectedSourceCommit) {
        $summary.source_identity.verification_status = 'mismatch'
        throw "source commit mismatch: expected $ExpectedSourceCommit, found $actualSourceCommit"
    }

    $gitStatusOutput = $null
    $gitStatusExitCode = $null
    try {
        $gitStatusResult = Invoke-HarnessProcess -Executable $gitExe `
            -ArgumentValues @(
                '-C', $script:RepoRoot, 'status', '--porcelain=v1', '--untracked-files=all',
                '--ignore-submodules=none', '--ignored=no'
            ) `
            -WorkingDirectory $script:RepoRoot -Environment $environment -Label 'source-clean-tree' `
            -TimeoutMs 10000 -StandardInputText $null -AllowFailure
        $gitStatusOutput = if ($null -eq $gitStatusResult.stdout) { '' } else { [string]$gitStatusResult.stdout }
        $gitStatusExitCode = [int]$gitStatusResult.exit_code
    } catch {
        $summary.source_identity.working_tree.status = 'invalid'
        $summary.source_identity.verification_status = 'invalid'
        throw
    } finally {
        try {
            [void](Clear-SourceCleanTreeCommandEvidenceOutput -CommandEvidence $script:CommandEvidence)
        } catch {
            Add-CleanupFailure -Summary $summary -Stage 'source-clean-tree-command-evidence-redaction' -Failure $_
        }
    }
    $summary.source_identity.working_tree = ConvertTo-GitWorkingTreeEvidence `
        -ExitCode $gitStatusExitCode -PorcelainV1Output $gitStatusOutput
    if ([string]$summary.source_identity.working_tree.status -ceq 'invalid') {
        $summary.source_identity.verification_status = 'invalid'
        throw "git clean-tree preflight failed with exit code $gitStatusExitCode"
    }
    if ([string]$summary.source_identity.working_tree.status -ceq 'dirty') {
        $summary.source_identity.verification_status = 'dirty'
        throw "repository working tree is dirty: entry_count=$($summary.source_identity.working_tree.entry_count), porcelain_v1_sha256=$($summary.source_identity.working_tree.porcelain_v1_sha256)"
    }
    $summary.source_identity.verified_commit = $actualSourceCommit
    $summary.source_identity.verification_status = 'verified_clean'
    $summary.source_commit = $actualSourceCommit

    $latencySeeds = @{}
    foreach ($index in 1..9) {
        if ($latencySeeds.ContainsKey($index)) {
            throw "duplicate latency fixture index: $index"
        }
        $latencySeeds[$index] = New-LegacyWorkspace -Index $index -Root $workspaceRoot `
            -Environment $environment
    }
    $summary.measurement_diagnostics.latency_source_preparation = `
        Assert-LatencySourcePreparationEvidence -Seeds $latencySeeds `
        -CommandEvidence $script:CommandEvidence.ToArray()
    $summary.measurement_diagnostics.source_clean_tree_command_evidence_redaction_self_test = `
        Invoke-SourceCleanTreeCommandEvidenceRedactionSelfTest
    $summary.measurement_diagnostics.early_failure_input_identity_self_test = `
        Invoke-EarlyFailureInputEvidenceSelfTest -Summary $summary `
        -ExpectedHarnessPath $resolvedHarness -ExpectedColayPath $script:ResolvedColay `
        -ExpectedFakeProviderPath $resolvedFake -ExpectedSourceCommit $ExpectedSourceCommit
    $summary.measurement_diagnostics.timing_self_test = Invoke-ProcessLifetimeMeasurementSelfTest `
        -WorkingDirectory $script:RunRoot -BaseEnvironment $environment
    $summary.measurement_diagnostics.failure_cleanup_self_test = Invoke-HarnessFailureCleanupSelfTest `
        -WorkingDirectory $script:RunRoot -BaseEnvironment $environment
    $sqliteVersionResult = Invoke-HarnessProcess -Executable $script:PythonExe `
        -ArgumentValues @('-I', '-c', 'import sqlite3; print(sqlite3.sqlite_version)') `
        -WorkingDirectory $script:RepoRoot -Environment $environment -Label 'sqlite-runtime-version' `
        -TimeoutMs 10000 -StandardInputText $null
    $sqliteVersion = [version]$sqliteVersionResult.stdout.Trim()
    if ($sqliteVersion -lt [version]'3.37.0') {
        throw "Python SQLite runtime $sqliteVersion cannot read schema-17 STRICT tables"
    }
    $summary.sqlite_runtime = [pscustomobject]@{ executable = $script:PythonExe; sqlite_version = $sqliteVersion.ToString() }
    $summary.process_audit.static_scan = Assert-NoProductNativeProcessLaunchBypass `
        -RepositoryRoot $script:RepoRoot

    $started = Invoke-Colay -Repository $emptyRepository -ArgumentValues @('--json', 'daemon', 'start') `
        -Environment $environment -Label 'start-empty-incumbent' -TimeoutMs 40000
    $startedDocument = Assert-StatusJson $started
    $mainReadiness = Wait-MainDaemonReadiness -DaemonStartDocument $startedDocument `
        -ExpectedExecutable $script:ResolvedColay -Repository $emptyRepository `
        -Environment $environment -Label 'main'
    $summary.measurement_diagnostics.main_daemon_readiness = $mainReadiness.Evidence
    $latencyMarkerState = Assert-LatencyInspectionMarkers -AggregateMarker $inspectionMarker `
        -AttributedDirectory $inspectionMarkerDirectory -ExpectedAggregateCount 0 `
        -Label 'empty incumbent latency phase'
    $seeds = [System.Collections.Generic.List[object]]::new()
    [void](Assert-DurableState -Seeds @() -ExpectedWorkspaceCount 1 -Environment $environment)

    $serialTimes = [System.Collections.Generic.List[int64]]::new()
    for ($index = 1; $index -le 5; $index++) {
        $seed = $latencySeeds[$index]
        $seeds.Add($seed)
        Add-SourceEvidence -Seed $seed -Summary $summary
        $result = Invoke-Colay -Repository $seed.repository -ArgumentValues @('--json', 'status') `
            -Environment $environment -Label "serial-register-$index" -TimeoutMs 12000
        [void](Assert-StatusJson $result)
        $serialTimes.Add([int64]$result.elapsed_ms)
        $summary.serial_times_ms = $serialTimes.ToArray()
        $latencyMarkerState = Assert-LatencyInspectionMarkers -AggregateMarker $inspectionMarker `
            -AttributedDirectory $inspectionMarkerDirectory -ExpectedAggregateCount (2 * $index) `
            -Label "serial workspace $index latency phase"
        $sourceHashesAfter = Get-SqliteFamilyHashes $seed.database
        $seed.source_hashes.after = $sourceHashesAfter
        Assert-EquivalentJson $seed.source_hashes.before $sourceHashesAfter "serial source $index SQLite family"
        [void](Assert-DurableState -Seeds $seeds.ToArray() -ExpectedWorkspaceCount (1 + $seeds.Count) -Environment $environment)
    }
    $summary.serial_times_ms = $serialTimes.ToArray()
    $serialMax = [int64](($serialTimes.ToArray() | Measure-Object -Maximum).Maximum)
    $summary.serial_max_ms = $serialMax

    $concurrentSeeds = [System.Collections.Generic.List[object]]::new()
    foreach ($index in 6..9) {
        $seed = $latencySeeds[$index]
        $seeds.Add($seed)
        $concurrentSeeds.Add($seed)
        Add-SourceEvidence -Seed $seed -Summary $summary
    }
    $concurrentRequests = [System.Collections.Generic.List[object]]::new()
    foreach ($seed in $concurrentSeeds) {
        $concurrentRequests.Add([pscustomobject]@{
            seed = $seed
            executable = $script:ResolvedColay
            argument_values = @('--json', 'status')
            working_directory = $seed.repository
            environment = $environment
            label = "concurrent-register-$($seed.index)"
            standard_input_text = $null
            capture_first_stdout_line = $true
            defer_observation = $true
        })
    }
    $running = @(Start-OwnedHarnessProcessBatch -Requests $concurrentRequests.ToArray())
    $concurrentTimes = [System.Collections.Generic.List[int64]]::new()
    $concurrentFailures = [System.Collections.Generic.List[string]]::new()
    $completedConcurrent = [System.Collections.Generic.List[object]]::new()
    try {
        foreach ($runningEntry in $running) {
            try {
                $result = Wait-HarnessProcess -Record $runningEntry.process -TimeoutMs 12000 -DeferObservation
                $completedConcurrent.Add([pscustomobject]@{
                    seed = $runningEntry.seed
                    result = $result
                })
            } catch {
                $concurrentFailures.Add("concurrent workspace $($runningEntry.seed.index) wait/finalization: $($_.Exception.Message)")
            }
        }
        foreach ($runningEntry in $running) {
            if ($null -ne $runningEntry.process.Process) {
                $concurrentFailures.Add("concurrent workspace $($runningEntry.seed.index) retained an owned process after its wait/finalization attempt")
                $lateCleanup = Complete-FailedHarnessProcess -Record $runningEntry.process `
                    -FailureStage 'concurrent-wait-finalization' -Terminate -DeferObservation
                foreach ($lateCleanupError in @($lateCleanup.cleanup_errors)) {
                    $concurrentFailures.Add("concurrent workspace $($runningEntry.seed.index) late cleanup: $lateCleanupError")
                }
            }
        }
        foreach ($completedEntry in $completedConcurrent) {
            try {
                $result = $completedEntry.result
                [void](Assert-StatusJson $result)
                $concurrentTimes.Add([int64]$result.elapsed_ms)
                $summary.concurrent_times_ms = $concurrentTimes.ToArray()
                $summary.concurrent_max_ms = [int64](($concurrentTimes.ToArray() | Measure-Object -Maximum).Maximum)
                $sourceHashesAfter = Get-SqliteFamilyHashes $completedEntry.seed.database
                $completedEntry.seed.source_hashes.after = $sourceHashesAfter
                Assert-EquivalentJson $completedEntry.seed.source_hashes.before $sourceHashesAfter "concurrent source $($completedEntry.seed.index) SQLite family"
            } catch {
                $concurrentFailures.Add("concurrent workspace $($completedEntry.seed.index) post-exit validation: $($_.Exception.Message)")
            }
        }
    } finally {
        $concurrentObserverWall = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            Update-ProcessObservation
        } catch {
            $concurrentFailures.Add("deferred concurrent process observation: $($_.Exception.Message)")
        } finally {
            $concurrentObserverWall.Stop()
            $summary.measurement_diagnostics.concurrent_observer_wall_ms = [int64]$concurrentObserverWall.ElapsedMilliseconds
        }
    }
    if ($concurrentFailures.Count -ne 0) {
        throw "concurrent registration failure(s): $($concurrentFailures -join '; ')"
    }
    $summary.concurrent_times_ms = $concurrentTimes.ToArray()
    $summary.concurrent_max_ms = [int64](($concurrentTimes.ToArray() | Measure-Object -Maximum).Maximum)

    $durableState = Assert-DurableState -Seeds $seeds.ToArray() -ExpectedWorkspaceCount 10 -Environment $environment
    $summary.durable_state = $durableState
    foreach ($seed in $seeds) {
        $after = Get-SqliteFamilyHashes $seed.database
        $seed.source_hashes.after = $after
        Assert-EquivalentJson $seed.source_hashes.before $after "source $($seed.index) SQLite family"
    }
    $latencyMarkerState = Assert-LatencyInspectionMarkers -AggregateMarker $inspectionMarker `
        -AttributedDirectory $inspectionMarkerDirectory -ExpectedAggregateCount 18 `
        -Label 'final latency phase'
    $summary.inspection_markers.latency_phase.aggregate_count = $latencyMarkerState.aggregate_count
    $summary.inspection_markers.latency_phase.attributed_group_count = $latencyMarkerState.attributed_group_count
    $summary.inspection_markers.latency_phase.attributed_event_count = $latencyMarkerState.attributed_event_count
    $summary.inspection_markers.latency_phase.groups = @($latencyMarkerState.groups)
    $summary.inspection_count = $latencyMarkerState.aggregate_count
    $globalDatabase = Join-Path $script:ColayHome 'state/state.db'
    $summary.zero_writable_rows = Assert-ZeroWritableRows -Database $globalDatabase -Environment $environment
    Assert-DatabaseHealth -Database $globalDatabase -Environment $environment
    $summary.sqlite_integrity = 'ok'
    $summary.sqlite_foreign_key_violations = 0

    $preAuditStopResult = Invoke-Colay -Repository $emptyRepository `
        -ArgumentValues @('--json', 'daemon', 'stop') -Environment $environment `
        -Label 'pre-audit-main-daemon-stop' -TimeoutMs 20000
    $preAuditStopDocument = Assert-ExactStoppedStatus -Result $preAuditStopResult `
        -ExpectedCommand 'daemon_stop'
    $preAuditStatusResult = Invoke-Colay -Repository $emptyRepository `
        -ArgumentValues @('--json', 'daemon', 'status') -Environment $environment `
        -Label 'pre-audit-main-endpoint-status' -TimeoutMs 10000
    $preAuditStatusDocument = Assert-ExactStoppedStatus -Result $preAuditStatusResult `
        -ExpectedCommand 'daemon_status'
    $preAuditDeadline = [datetime]::UtcNow.AddSeconds(10)
    do {
        $preAuditLeaseRows = Invoke-Sqlite -Database $globalDatabase `
            -Sql 'SELECT count(*) AS row_count FROM daemon_instances WHERE released_at IS NULL;' `
            -WorkingDirectory $script:RunRoot -Environment $environment -ReadOnly -Csv `
            -Label 'pre-audit-main-live-leases'
        if ($preAuditLeaseRows.Count -ne 1) {
            throw "pre-audit main live lease query returned $($preAuditLeaseRows.Count) rows; expected exactly one"
        }
        $preAuditResidual = @(Get-LiveAttributedProcesses)
        if ([int]$preAuditLeaseRows[0].row_count -eq 0 -and $preAuditResidual.Count -eq 0) { break }
        Start-Sleep -Milliseconds 50
    } while ([datetime]::UtcNow -lt $preAuditDeadline)
    if ([int]$preAuditLeaseRows[0].row_count -ne 0) {
        throw 'main daemon live lease remained before process audit'
    }
    if ($preAuditResidual.Count -ne 0) {
        throw "main attributable process residue remained before process audit: $($preAuditResidual | ConvertTo-Json -Compress)"
    }
    $mainShutdownBeforeAudit = [pscustomobject][ordered]@{
        daemon_stop_state = [string]$preAuditStopDocument.data.status.state
        endpoint_state = [string]$preAuditStatusDocument.data.status.state
        live_lease_count = [int]$preAuditLeaseRows[0].row_count
        residual_processes = @()
    }

    $auditPowerShell = Resolve-RequiredFile (Join-Path $PSHOME 'pwsh.exe') 'portable PowerShell executable'
    if ([System.IO.Path]::GetFileName($auditPowerShell) -cne 'pwsh.exe') {
        throw "process audit root is not pwsh.exe: $auditPowerShell"
    }
    $auditEvidenceDirectory = Join-Path $evidenceDirectory 'process-audit'
    New-Item -ItemType Directory -Path $auditEvidenceDirectory -ErrorAction Stop | Out-Null
    $helperBuildScript = Resolve-RequiredFile `
        (Join-Path $script:RepoRoot 'scripts/qa/build-windows-process-audit-helper.ps1') `
        'process audit helper build script'
    $helperSource = Resolve-RequiredFile `
        (Join-Path $script:RepoRoot 'scripts/qa/windows-process-audit-helper.cs') `
        'process audit helper source'
    $compiler = Resolve-RequiredFile `
        (Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319/csc.exe') `
        '64-bit inbox C# compiler'
    $buildResult = Invoke-HarnessProcess -Executable $auditPowerShell `
        -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $helperBuildScript, `
            '-OutputDirectory', $auditEvidenceDirectory) `
        -WorkingDirectory $script:RepoRoot -Environment $environment -Label 'process-audit-helper-build' `
        -TimeoutMs 60000 -StandardInputText $null
    $buildLines = @($buildResult.stdout -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($buildLines.Count -ne 1) {
        throw "process audit helper build emitted $($buildLines.Count) nonempty stdout lines; expected exactly one"
    }
    $auditHelperExe = Resolve-RequiredFile $buildLines[0] 'compiled process audit helper'
    $expectedHelperExe = Join-Path $auditEvidenceDirectory 'windows-process-audit-helper.exe'
    if ((ConvertTo-ComparableWindowsPath $auditHelperExe) -cne (ConvertTo-ComparableWindowsPath $expectedHelperExe)) {
        throw "process audit helper build escaped its run evidence directory: $auditHelperExe"
    }
    $auditChildScript = Join-Path $auditEvidenceDirectory 'windows-process-audit-child.ps1'
    Write-ProcessAuditChildScript -Path $auditChildScript
    $summary.process_audit.helper_build = [pscustomobject][ordered]@{
        build_script = [pscustomobject]@{ path = $helperBuildScript; sha256 = Get-Sha256 $helperBuildScript }
        helper_source = [pscustomobject]@{ path = $helperSource; sha256 = Get-Sha256 $helperSource }
        compiler = [pscustomobject]@{ path = $compiler; sha256 = Get-Sha256 $compiler }
        helper_binary = [pscustomobject]@{ path = $auditHelperExe; sha256 = Get-Sha256 $auditHelperExe }
        audit_child_source = [pscustomobject]@{ path = $auditChildScript; sha256 = Get-Sha256 $auditChildScript }
        powershell = [pscustomobject]@{
            path = $auditPowerShell
            sha256 = Get-Sha256 $auditPowerShell
            version = $PSVersionTable.PSVersion.ToString()
        }
        build_elapsed_ms = $buildResult.elapsed_ms
    }

    $auditRuntimeRoot = Join-Path $script:RunRoot 'process-audit-runtime'
    if (Test-Path -LiteralPath $auditRuntimeRoot) {
        throw "isolated process audit runtime root already exists: $auditRuntimeRoot"
    }
    $auditColayHome = Join-Path $auditRuntimeRoot 'colay-home'
    $auditWorkspaceRoot = Join-Path $auditRuntimeRoot 'workspaces'
    $auditEmptyRepository = Join-Path $auditWorkspaceRoot 'empty-incumbent'
    $auditMarkerFile = Join-Path $auditRuntimeRoot 'temp/legacy-inspections.log'
    $auditMarkerDirectory = Join-Path $auditRuntimeRoot 'temp/legacy-inspection-groups'
    foreach ($directory in @($auditRuntimeRoot, $auditColayHome, $auditWorkspaceRoot, $auditEmptyRepository)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $auditEnvironment = New-IsolatedEnvironment -ColayHomePath $auditColayHome -Root $auditRuntimeRoot `
        -FakeProvider $resolvedFake -InspectionMarker $auditMarkerFile `
        -InspectionMarkerDirectory $auditMarkerDirectory -MarkerPhase CorrectnessAttributedOn
    New-Item -ItemType Directory -Path $auditMarkerDirectory -ErrorAction Stop | Out-Null
    $summary.inspection_markers.correctness_phase.aggregate_file = $auditMarkerFile
    $summary.inspection_markers.correctness_phase.attributed_environment_key_present = `
        $auditEnvironment.Contains('COLAY_TEST_LEGACY_INSPECT_MARKER_DIR')
    $summary.inspection_markers.correctness_phase.attributed_directory = $auditMarkerDirectory
    if (-not $summary.inspection_markers.correctness_phase.attributed_environment_key_present -or
        [string]$auditEnvironment['COLAY_TEST_LEGACY_INSPECT_MARKER_DIR'] -cne $auditMarkerDirectory) {
        throw 'correctness environment omitted the exact attributed marker directory'
    }
    New-FakeProviderConfig -ColayHomePath $auditColayHome -FakeProvider $resolvedFake
    $auditGlobalDatabase = Join-Path $auditColayHome 'state/state.db'
    if (Test-Path -LiteralPath $auditGlobalDatabase) {
        throw "process audit COLAY_HOME unexpectedly had a pre-existing global database: $auditGlobalDatabase"
    }
    $auditSeed = New-LegacyWorkspace -Index 10 -Root $auditWorkspaceRoot -Environment $auditEnvironment
    $auditGroupsBefore = Get-AttributedInspectionSnapshot $auditMarkerDirectory
    if ($auditGroupsBefore.Count -ne 0 -or (Get-InspectionCount $auditMarkerFile) -ne 0) {
        throw 'process audit markers were not empty before DEBUG_PROCESS launch'
    }

    $auditChildArguments = @(
        '-NoLogo', '-NoProfile', '-NonInteractive', '-File', $auditChildScript,
        '-ColayExe', $script:ResolvedColay,
        '-PythonExe', $script:PythonExe,
        '-EmptyRepository', $auditEmptyRepository,
        '-LegacyRepository', $auditSeed.repository,
        '-LegacyDatabase', $auditSeed.database,
        '-AuditColayHome', $auditColayHome,
        '-MarkerDirectory', $auditMarkerDirectory,
        '-ExpectedSourceHashesJson', ($auditSeed.source_hashes.before | ConvertTo-Json -Depth 30 -Compress),
        '-ExpectedConfigSha256', $auditSeed.config_sha256
    )
    $auditHelperEvidencePath = Join-Path $auditEvidenceDirectory 'functional-audit.json'
    if (Test-Path -LiteralPath $auditHelperEvidencePath) {
        throw "process audit evidence already exists: $auditHelperEvidencePath"
    }
    $auditHelperArguments = [System.Collections.Generic.List[string]]::new()
    foreach ($argument in @('--evidence', $auditHelperEvidencePath, '--timeout-ms', '150000',
            '--working-directory', $auditRuntimeRoot, '--environment', 'clear')) {
        $auditHelperArguments.Add($argument)
    }
    $auditEnvironmentNames = @($auditEnvironment.Keys | ForEach-Object { [string]$_ } | Sort-Object)
    foreach ($name in $auditEnvironmentNames) {
        $auditHelperArguments.Add('--env')
        $auditHelperArguments.Add($name)
        $auditHelperArguments.Add([string]$auditEnvironment[$name])
    }
    foreach ($argument in $auditChildArguments) {
        $auditHelperArguments.Add('--child-argument-base64')
        $auditHelperArguments.Add((ConvertTo-ProcessAuditArgumentBase64 $argument))
    }
    $auditHelperArguments.Add('--')
    $auditHelperArguments.Add($auditPowerShell)
    $auditHelperResult = Invoke-HarnessProcess -Executable $auditHelperExe `
        -ArgumentValues $auditHelperArguments.ToArray() -WorkingDirectory $auditRuntimeRoot `
        -Environment $auditEnvironment -Label 'strong-process-audit-functional' -TimeoutMs 170000 `
        -StandardInputText $null -AllowFailure
    if (-not (Test-Path -LiteralPath $auditHelperEvidencePath -PathType Leaf)) {
        throw "process audit helper did not emit evidence (exit=$($auditHelperResult.exit_code)): $($auditHelperResult.stderr)"
    }
    $auditObserverEvidence = Get-Content -LiteralPath $auditHelperEvidencePath -Raw | ConvertFrom-Json -Depth 50
    $auditContract = Assert-StrongProcessAuditEvidence -HelperResult $auditHelperResult `
        -Evidence $auditObserverEvidence -ExpectedPowerShell $auditPowerShell `
        -ExpectedColay $script:ResolvedColay -ExpectedPython $script:PythonExe `
        -ExpectedFakeProvider $resolvedFake -ExpectedWorkingDirectory $auditRuntimeRoot `
        -ExpectedTimeoutMs 150000 -ExpectedArgumentCount $auditChildArguments.Count `
        -ExpectedEnvironmentNames $auditEnvironmentNames
    $auditChildOutputLines = @($auditHelperResult.stdout -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if (-not [string]::IsNullOrEmpty([string]$auditHelperResult.stderr)) {
        throw "process audit child/helper emitted stderr and readiness serialization is not trustworthy: $($auditHelperResult.stderr)"
    }
    if ($auditChildOutputLines.Count -ne 1) {
        throw "process audit child emitted $($auditChildOutputLines.Count) nonempty stdout lines; expected exactly one"
    }
    $auditChildResult = $auditChildOutputLines[0] | ConvertFrom-Json -Depth 30
    if ([string]$auditChildResult.schema_version -cne '1' -or
        [string]$auditChildResult.status -cne 'passed' -or
        [string]$auditChildResult.cleanup_state -cne 'stopped') {
        throw 'process audit child did not report exact passed/stopped schema-v1 result'
    }
    [void](Assert-AuditDaemonReadinessEvidence `
        -ReadinessEvidence $auditChildResult.daemon_readiness `
        -ExpectedExecutable $script:ResolvedColay -ExpectedLabel 'audit' `
        -ExpectedOverallTimeoutMs 5000 `
        -ExpectedPollIntervalMs 50 -ExpectedExitWaitLimitMs 400 `
        -ExpectedOutputDrainLimitMs 100)
    $processLineSelfTest = $auditChildResult.process_line_failure_self_test
    if ($null -eq $processLineSelfTest -or [string]$processLineSelfTest.status -cne 'passed' -or
        [int]$processLineSelfTest.case_count -ne 3 -or
        [int]$processLineSelfTest.process_residue_count -ne 0 -or
        [int]$processLineSelfTest.incomplete_pipe_task_count -ne 0 -or
        [int]$processLineSelfTest.cleanup_error_count -ne 0 -or
        -not [bool]$processLineSelfTest.exit_confirmed -or
        -not [bool]$processLineSelfTest.process_disposed) {
        throw 'process audit child process-line timeout cleanup self-test did not pass without residue'
    }

    $auditSourceHashesAfter = Get-SqliteFamilyHashes $auditSeed.database
    $auditSeed.source_hashes.after = $auditSourceHashesAfter
    Assert-EquivalentJson $auditSeed.source_hashes.before $auditSourceHashesAfter `
        'process audit source SQLite family'
    $auditDurableState = Assert-DurableState -Seeds @($auditSeed) -ExpectedWorkspaceCount 2 `
        -Environment $auditEnvironment -ColayHomePath $auditColayHome
    $correctnessMarkerState = Assert-CorrectnessInspectionMarkers -AggregateMarker $auditMarkerFile `
        -AttributedDirectory $auditMarkerDirectory -ExpectedGroup $auditSeed.source_root_hash `
        -Label 'process audit correctness phase'
    if ([string]$auditChildResult.source_root_hash -cne [string]$auditSeed.source_root_hash) {
        throw 'process audit opaque marker group did not match durable and child source identity'
    }
    $summary.inspection_markers.correctness_phase.aggregate_count = $correctnessMarkerState.aggregate_count
    $summary.inspection_markers.correctness_phase.attributed_group_count = $correctnessMarkerState.attributed_group_count
    $summary.inspection_markers.correctness_phase.attributed_event_count = $correctnessMarkerState.attributed_event_count
    $summary.inspection_markers.correctness_phase.groups = @($correctnessMarkerState.groups)
    $summary.inspection_markers.correctness_phase.source_root_hash = $auditSeed.source_root_hash
    $summary.inspection_markers.correctness_phase.source_root_hash_matches_group = $true
    $auditZeroWritableRows = Assert-ZeroWritableRows -Database $auditGlobalDatabase -Environment $auditEnvironment
    Assert-DatabaseHealth -Database $auditGlobalDatabase -Environment $auditEnvironment
    $auditLiveRows = Invoke-Sqlite -Database $auditGlobalDatabase `
        -Sql 'SELECT count(*) AS row_count FROM daemon_instances WHERE released_at IS NULL;' `
        -WorkingDirectory $auditRuntimeRoot -Environment $auditEnvironment -ReadOnly -Csv `
        -Label 'process-audit-final-live-leases'
    if ($auditLiveRows.Count -ne 1 -or [int]$auditLiveRows[0].row_count -ne 0) {
        throw 'process audit live lease residue remained after debugged cleanup'
    }
    $summary.process_audit.functional = [pscustomobject][ordered]@{
        status = 'passed'
        timing_excluded_from_latency_thresholds = $true
        main_daemon_before_audit = $mainShutdownBeforeAudit
        helper_command_elapsed_ms = $auditHelperResult.elapsed_ms
        helper_evidence_path = $auditHelperEvidencePath
        observer_contract = $auditContract
        observer_evidence = $auditObserverEvidence
        child_result = $auditChildResult
        durable_state = $auditDurableState
        source = [pscustomobject]@{
            database = $auditSeed.database
            sqlite_family_hashes = $auditSeed.source_hashes
            config_sha256 = $auditSeed.config_sha256
            source_root_hash = $auditSeed.source_root_hash
        }
        zero_writable_rows = $auditZeroWritableRows
        live_lease_count = [int]$auditLiveRows[0].row_count
    }
    Update-ProcessObservation
    $summary.forbidden_utility_launches = @($script:ForbiddenStarts)
    if ($summary.forbidden_utility_launches.Count -ne 0) { throw 'forbidden utility launch count was nonzero' }
    $bodySucceeded = $true
}
catch {
    $failureRecord = $_
    $summary.failure = [pscustomobject]@{
        message = $_.Exception.Message
        category = [string]$_.CategoryInfo.Category
        script_stack = $_.ScriptStackTrace
    }
}
finally {
    try {
        try {
            $auditDatabaseAvailable = $null -ne $auditGlobalDatabase -and `
                (Test-Path -LiteralPath $auditGlobalDatabase -PathType Leaf)
            if ($auditDatabaseAvailable) {
                if ($null -eq $auditEnvironment -or $null -eq $auditEmptyRepository) {
                    Add-CleanupFailure -Summary $summary -Stage 'audit-cleanup-preconditions' `
                        -Failure 'audit database exists, but its isolated environment or incumbent repository is unavailable'
                } else {
                    try {
                        $auditStopResult = Invoke-Colay -Repository $auditEmptyRepository `
                            -ArgumentValues @('--json', 'daemon', 'stop') -Environment $auditEnvironment `
                            -Label 'finally-audit-stop' -TimeoutMs 10000 -AllowFailure
                        $summary.cleanup.audit_daemon_stop = [pscustomobject]@{
                            exit_code = $auditStopResult.exit_code
                            state = $null
                        }
                        $auditStopDocument = Assert-ExactStoppedStatus -Result $auditStopResult `
                            -ExpectedCommand 'daemon_stop'
                        $summary.cleanup.audit_daemon_stop.state = [string]$auditStopDocument.data.status.state
                    } catch {
                        Add-CleanupFailure -Summary $summary -Stage 'audit-daemon-stop' -Failure $_
                    }
                    try {
                        $auditStatusResult = Invoke-Colay -Repository $auditEmptyRepository `
                            -ArgumentValues @('--json', 'daemon', 'status') -Environment $auditEnvironment `
                            -Label 'finally-audit-endpoint-status' -TimeoutMs 10000 -AllowFailure
                        $summary.cleanup.audit_endpoint_status = [pscustomobject]@{
                            exit_code = $auditStatusResult.exit_code
                            state = $null
                        }
                        $auditStatusDocument = Assert-ExactStoppedStatus -Result $auditStatusResult `
                            -ExpectedCommand 'daemon_status'
                        $summary.cleanup.audit_endpoint_status.state = [string]$auditStatusDocument.data.status.state
                    } catch {
                        Add-CleanupFailure -Summary $summary -Stage 'audit-endpoint-status' -Failure $_
                    }
                    try {
                        $auditLeaseDeadline = [datetime]::UtcNow.AddSeconds(10)
                        do {
                            $auditLeaseRows = Invoke-Sqlite -Database $auditGlobalDatabase `
                                -Sql 'SELECT count(*) AS row_count FROM daemon_instances WHERE released_at IS NULL;' `
                                -WorkingDirectory $auditRuntimeRoot -Environment $auditEnvironment -ReadOnly -Csv `
                                -Label 'finally-audit-live-leases'
                            if ($auditLeaseRows.Count -ne 1) {
                                throw "audit live lease query returned $($auditLeaseRows.Count) rows; expected exactly one"
                            }
                            $summary.cleanup.audit_live_lease_count = [int]$auditLeaseRows[0].row_count
                            if ([int]$auditLeaseRows[0].row_count -eq 0) { break }
                            Start-Sleep -Milliseconds 50
                        } while ([datetime]::UtcNow -lt $auditLeaseDeadline)
                        if ([int]$auditLeaseRows[0].row_count -ne 0) {
                            throw "audit live lease residue remained: $($auditLeaseRows[0].row_count)"
                        }
                    } catch {
                        Add-CleanupFailure -Summary $summary -Stage 'audit-live-lease-cleanup' -Failure $_
                    }
                }
            }
            if ($null -ne $script:ColayHome) {
                $globalDatabase = Join-Path $script:ColayHome 'state/state.db'
            }
            $databaseAvailable = $null -ne $globalDatabase -and (Test-Path -LiteralPath $globalDatabase -PathType Leaf)
            if ($databaseAvailable) {
                if ($null -eq $environment -or $null -eq $script:ResolvedColay -or $null -eq $emptyRepository) {
                    Add-CleanupFailure -Summary $summary -Stage 'cleanup-preconditions' `
                        -Failure 'global database exists, but the isolated environment, executable, or incumbent repository is unavailable'
                } else {
                    try {
                        $stopResult = Invoke-Colay -Repository $emptyRepository -ArgumentValues @('--json', 'daemon', 'stop') `
                            -Environment $environment -Label 'finally-stop' -TimeoutMs 10000 -AllowFailure
                        $summary.cleanup.daemon_stop = [pscustomobject]@{
                            exit_code = $stopResult.exit_code
                            state = $null
                        }
                        $stopDocument = Assert-ExactStoppedStatus -Result $stopResult -ExpectedCommand 'daemon_stop'
                        $summary.cleanup.daemon_stop.state = [string]$stopDocument.data.status.state
                    } catch {
                        Add-CleanupFailure -Summary $summary -Stage 'daemon-stop' -Failure $_
                    }
                    try {
                        $statusResult = Invoke-Colay -Repository $emptyRepository -ArgumentValues @('--json', 'daemon', 'status') `
                            -Environment $environment -Label 'endpoint-status-after-stop' -TimeoutMs 10000 -AllowFailure
                        $summary.cleanup.endpoint_status = [pscustomobject]@{
                            exit_code = $statusResult.exit_code
                            state = $null
                        }
                        $statusDocument = Assert-ExactStoppedStatus -Result $statusResult -ExpectedCommand 'daemon_status'
                        $summary.cleanup.endpoint_status.state = [string]$statusDocument.data.status.state
                    } catch {
                        Add-CleanupFailure -Summary $summary -Stage 'endpoint-status' -Failure $_
                    }
                }
            }

            $leaseCount = $null
            $residual = @()
            $pollChecksAvailable = $true
            $cleanupDeadline = [datetime]::UtcNow.AddSeconds(10)
            do {
                if ($databaseAvailable) {
                    try {
                        if ($null -eq $environment -or $null -eq $script:PythonExe) {
                            throw 'SQLite cleanup query dependencies are unavailable'
                        }
                        $liveRows = Invoke-Sqlite -Database $globalDatabase `
                            -Sql 'SELECT count(*) AS row_count FROM daemon_instances WHERE released_at IS NULL;' `
                            -WorkingDirectory $script:RunRoot -Environment $environment -ReadOnly -Csv -Label 'cleanup-live-leases'
                        if ($liveRows.Count -ne 1) {
                            throw "live daemon lease query returned $($liveRows.Count) rows; expected exactly one"
                        }
                        $leaseCount = [int]$liveRows[0].row_count
                        $summary.cleanup.live_lease_count = $leaseCount
                    } catch {
                        Add-CleanupFailure -Summary $summary -Stage 'live-lease-query' -Failure $_
                        $pollChecksAvailable = $false
                    }
                }
                try {
                    $residual = @(Get-LiveAttributedProcesses)
                } catch {
                    Add-CleanupFailure -Summary $summary -Stage 'process-residue-query' -Failure $_
                    $pollChecksAvailable = $false
                }
                $leaseReleased = -not $databaseAvailable -or ($null -ne $leaseCount -and $leaseCount -eq 0)
                if ($pollChecksAvailable -and $leaseReleased -and $residual.Count -eq 0) {
                    break
                }
                if (-not $pollChecksAvailable) {
                    break
                }
                Start-Sleep -Milliseconds 50
            } while ([datetime]::UtcNow -lt $cleanupDeadline)

            if ($databaseAvailable -and $null -ne $leaseCount -and $leaseCount -ne 0) {
                Add-CleanupFailure -Summary $summary -Stage 'live-lease-residue' `
                    -Failure "daemon live lease residue remained after bounded stop: $leaseCount"
            }
            $summary.cleanup.residual_processes_before_force = @($residual)
            if ($residual.Count -ne 0) {
                Add-CleanupFailure -Summary $summary -Stage 'process-residue' `
                    -Failure "exact-owned process residue remained after bounded stop: $($residual | ConvertTo-Json -Compress)"
                try {
                    $forceCleanup = Stop-AttributedProcesses -Processes $residual
                    $summary.cleanup.force_process_cleanup = $forceCleanup
                    if ($null -ne $forceCleanup.refused_reason) {
                        Add-CleanupFailure -Summary $summary -Stage 'process-force-stop-refused' `
                            -Failure $forceCleanup.refused_reason
                    }
                    foreach ($forceError in @($forceCleanup.errors)) {
                        Add-CleanupFailure -Summary $summary -Stage 'process-force-stop-native' `
                            -Failure $forceError
                    }
                    if (-not $forceCleanup.handles_disposed) {
                        Add-CleanupFailure -Summary $summary -Stage 'process-force-stop-handle-disposal' `
                            -Failure 'verified force cleanup did not close every retained native handle'
                    }
                } catch {
                    Add-CleanupFailure -Summary $summary -Stage 'process-force-stop' -Failure $_
                }
                $forceDeadline = [datetime]::UtcNow.AddSeconds(2)
                do {
                    try {
                        $residual = @(Get-LiveAttributedProcesses)
                    } catch {
                        Add-CleanupFailure -Summary $summary -Stage 'post-force-process-query' -Failure $_
                        break
                    }
                    if ($residual.Count -eq 0) { break }
                    Start-Sleep -Milliseconds 50
                } while ([datetime]::UtcNow -lt $forceDeadline)
            }
            $summary.cleanup.residual_processes_after_force = @($residual)
            $summary.residual_processes = @($residual)
            if ($residual.Count -ne 0) {
                Add-CleanupFailure -Summary $summary -Stage 'post-force-process-residue' `
                    -Failure "attributable process residue survived forced cleanup: $($residual | ConvertTo-Json -Compress)"
            }
        } catch {
            Add-CleanupFailure -Summary $summary -Stage 'cleanup-unhandled' -Failure $_
        }
    } finally {
        try {
            Update-ProcessObservation
        } catch {
            Add-CleanupFailure -Summary $summary -Stage 'observer-final-drain' -Failure $_
        } finally {
            try {
                Stop-ProcessObservation
                $summary.cleanup.observer_teardown = 'passed'
            } catch {
                $summary.cleanup.observer_teardown = 'failed'
                Add-CleanupFailure -Summary $summary -Stage 'observer-teardown' -Failure $_
            }
        }
    }

    try {
        Sync-CompletedTimingEvidence -Summary $summary
    } catch {
        Add-CleanupFailure -Summary $summary -Stage 'timing-evidence-finalization' -Failure $_
    }
    try {
        $summary.forbidden_utility_launches = @($script:ForbiddenStarts)
        $forbiddenAlreadyRecorded = @($script:CleanupErrors | Where-Object stage -EQ 'forbidden-utility-launch').Count -ne 0
        if ($summary.forbidden_utility_launches.Count -ne 0 -and -not $forbiddenAlreadyRecorded) {
            Add-CleanupFailure -Summary $summary -Stage 'forbidden-utility-launch' `
                -Failure "forbidden attributable utility launch observed: $($summary.forbidden_utility_launches | ConvertTo-Json -Compress)"
        }
    } catch {
        Add-CleanupFailure -Summary $summary -Stage 'forbidden-utility-finalization' -Failure $_
    }
    try {
        $summary.process_ownership_refusals = @($script:ProcessOwnershipRefusals)
        if ($summary.process_ownership_refusals.Count -ne 0) {
            Add-CleanupFailure -Summary $summary -Stage 'process-ownership-refusal' `
                -Failure "process identity attribution refused unsafe, ambiguous, or unverifiable snapshot input: $($summary.process_ownership_refusals | ConvertTo-Json -Compress)"
        }
    } catch {
        Add-CleanupFailure -Summary $summary -Stage 'process-ownership-refusal-finalization' -Failure $_
    }

    $safeToWriteEvidence = $true
    try {
        Assert-FreeDisk | Out-Null
    } catch {
        $safeToWriteEvidence = $false
        Add-CleanupFailure -Summary $summary -Stage 'final-free-space-check' -Failure $_
    }
    try {
        $summary.disk_volumes = @(Get-DiskVolumeEvidence)
        $minimums = @($summary.disk_volumes | Where-Object { $null -ne $_.minimum_free_gib } | ForEach-Object { [double]$_.minimum_free_gib })
        $summary.minimum_free_gib = if ($minimums.Count -eq 0) { $null } else { [double](($minimums | Measure-Object -Minimum).Minimum) }
    } catch {
        $safeToWriteEvidence = $false
        Add-CleanupFailure -Summary $summary -Stage 'disk-evidence-finalization' -Failure $_
    }
    $hasAcceptanceFailures = @($summary.acceptance_failures).Count -ne 0
    $summary.status = if ($bodySucceeded -and $script:CleanupErrors.Count -eq 0 -and
        -not $hasAcceptanceFailures) { 'passed' } else { 'failed' }
    if ($null -eq $failureRecord -and $script:CleanupErrors.Count -ne 0) {
        $summary.failure = [pscustomobject]@{
            message = "cleanup failed in $($script:CleanupErrors.Count) stage(s)"
            category = 'CleanupFailure'
            script_stack = $null
        }
    } elseif ($null -eq $failureRecord -and $hasAcceptanceFailures) {
        $summary.failure = [pscustomobject]@{
            message = "$(@($summary.acceptance_failures).Count) latency acceptance threshold(s) failed"
            category = 'AcceptanceFailure'
            script_stack = $null
        }
    }
    $summary.completed_at_utc = [datetime]::UtcNow.ToString('o')

    if ($null -ne $evidenceDirectory -and $safeToWriteEvidence) {
        try {
            $evidencePath = Join-Path (Split-Path -Parent $evidenceDirectory) "windows-state-acl-stress-$runStamp.json"
            $summaryPath = Join-Path (Split-Path -Parent $evidenceDirectory) 'summary.json'
            $evidence = [ordered]@{
                summary = $summary
                commands = $script:CommandEvidence
                process_setup_failures = $script:ProcessSetupFailureEvidence
                process_batch_cleanup = $script:ProcessBatchCleanupEvidence
                timing_self_test_failure_cleanup = $script:TimingSelfTestFailureCleanupEvidence
                attributable_process_ids = @($script:OwnedProcessIdentities | ForEach-Object process_id | Sort-Object -Unique)
                attributable_process_identities = @($script:OwnedProcessIdentities | ForEach-Object { Get-ProcessIdentityEvidence $_ })
                process_ownership_refusals = $script:ProcessOwnershipRefusals
                forced_process_cleanup = $script:ForcedProcessCleanupEvidence
                observed_process_starts = $script:ObservedStarts
                process_snapshots = $script:ProcessSnapshots
                process_observation_mode = $script:ProcessObservationMode
                process_event_subscription_failure = $script:WatcherFailure
            }
            $evidence | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
            $summary | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $summaryPath -Encoding utf8NoBOM
        } catch {
            Add-CleanupFailure -Summary $summary -Stage 'evidence-write' -Failure $_
            $summary.status = 'failed'
            if ($null -eq $failureRecord) {
                $summary.failure = [pscustomobject]@{
                    message = "cleanup or evidence finalization failed in $($script:CleanupErrors.Count) stage(s)"
                    category = 'CleanupFailure'
                    script_stack = $null
                }
            }
            $summary.completed_at_utc = [datetime]::UtcNow.ToString('o')
            try {
                $retryEvidencePath = Join-Path (Split-Path -Parent $evidenceDirectory) "windows-state-acl-stress-$runStamp.json"
                $retrySummaryPath = Join-Path (Split-Path -Parent $evidenceDirectory) 'summary.json'
                $retryEvidence = [ordered]@{
                    summary = $summary
                    commands = $script:CommandEvidence
                    process_setup_failures = $script:ProcessSetupFailureEvidence
                    process_batch_cleanup = $script:ProcessBatchCleanupEvidence
                    timing_self_test_failure_cleanup = $script:TimingSelfTestFailureCleanupEvidence
                    attributable_process_ids = @($script:OwnedProcessIdentities | ForEach-Object process_id | Sort-Object -Unique)
                    attributable_process_identities = @($script:OwnedProcessIdentities | ForEach-Object { Get-ProcessIdentityEvidence $_ })
                    process_ownership_refusals = $script:ProcessOwnershipRefusals
                    forced_process_cleanup = $script:ForcedProcessCleanupEvidence
                    observed_process_starts = $script:ObservedStarts
                    process_snapshots = $script:ProcessSnapshots
                    process_observation_mode = $script:ProcessObservationMode
                    process_event_subscription_failure = $script:WatcherFailure
                }
                $retryEvidence | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $retryEvidencePath -Encoding utf8NoBOM
                $summary | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $retrySummaryPath -Encoding utf8NoBOM
            } catch {
                Add-CleanupFailure -Summary $summary -Stage 'evidence-write-retry' -Failure $_
            }
        }
    }
}

if ($null -ne $failureRecord) {
    throw $failureRecord
}

if ($script:CleanupErrors.Count -ne 0) {
    throw "Windows state ACL stress cleanup failed in $($script:CleanupErrors.Count) stage(s): $($script:CleanupErrors.message -join '; ')"
}

if (@($summary.acceptance_failures).Count -ne 0) {
    throw "Windows state ACL stress acceptance failed: $($summary.acceptance_failures.message -join '; ')"
}

$summary | ConvertTo-Json -Depth 20
