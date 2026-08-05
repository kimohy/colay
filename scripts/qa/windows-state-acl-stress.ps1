#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ColayExe,

    [Parameter(Mandatory = $true)]
    [string]$FakeProviderExe,

    [Parameter(Mandatory = $true)]
    [string]$EvidenceRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ResponseTimeoutMs = 10000
$SerialP95LimitMs = 5000
$ConcurrentLimitMs = 8000
$MinimumFreeGiB = 5
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

$script:RunPids = [System.Collections.Generic.HashSet[int]]::new()
$script:ObservedStarts = [System.Collections.Generic.List[object]]::new()
$script:ProcessSnapshots = [System.Collections.Generic.List[object]]::new()
$script:ForbiddenStarts = [System.Collections.Generic.List[object]]::new()
$script:CommandEvidence = [System.Collections.Generic.List[object]]::new()
$script:MinimumObservedFreeGiB = [double]::PositiveInfinity
$script:ProcessEventQueue = [System.Collections.Concurrent.ConcurrentQueue[object]]::new()
$script:Watcher = $null
$script:WatcherSubscription = $null
$script:ProcessObservationMode = 'win32-process-polling'
$script:WatcherFailure = $null
$script:RepoRoot = $null
$script:RunRoot = $null
$script:ColayHome = $null
$script:ResolvedColay = $null
$script:PythonExe = $null

function Assert-FreeDisk {
    $freeGiB = [math]::Round((Get-PSDrive -Name C).Free / 1GB, 3)
    if ($freeGiB -lt $script:MinimumObservedFreeGiB) {
        $script:MinimumObservedFreeGiB = $freeGiB
    }
    if ($freeGiB -lt $MinimumFreeGiB) {
        throw "C: free space fell below the ${MinimumFreeGiB}GiB safety floor: ${freeGiB}GiB"
    }
    return $freeGiB
}

function Resolve-RequiredFile {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Label)
    $resolved = Resolve-Path -LiteralPath $Path -ErrorAction Stop
    if (-not (Test-Path -LiteralPath $resolved.Path -PathType Leaf)) {
        throw "$Label is not a file: $($resolved.Path)"
    }
    return [System.IO.Path]::GetFullPath($resolved.Path)
}

function Start-AncestryObservation {
    [void]$script:RunPids.Add($PID)
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
                })
            }
        $script:Watcher.Start()
        $script:ProcessObservationMode = 'win32-process-events-and-polling'
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
    $started = $null
    while ($script:ProcessEventQueue.TryDequeue([ref]$started)) {
        $script:ObservedStarts.Add($started)
        $started = $null
    }

    $snapshot = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop | ForEach-Object {
        [pscustomobject]@{
            process_id = [int]$_.ProcessId
            parent_process_id = [int]$_.ParentProcessId
            name = [string]$_.Name
        }
    })
    $candidates = @($script:ObservedStarts) + $snapshot
    do {
        $added = $false
        foreach ($candidate in $candidates) {
            if ($script:RunPids.Contains([int]$candidate.parent_process_id)) {
                if ($script:RunPids.Add([int]$candidate.process_id)) {
                    $added = $true
                }
            }
        }
    } while ($added)

    $attributableSnapshot = @($snapshot | Where-Object {
        $script:RunPids.Contains([int]$_.process_id)
    })
    $script:ProcessSnapshots.Add([pscustomobject]@{
        observed_at_utc = [datetime]::UtcNow.ToString('o')
        processes = $attributableSnapshot
    })

    foreach ($candidate in $attributableSnapshot) {
        if (-not ($script:ObservedStarts | Where-Object process_id -EQ $candidate.process_id)) {
            $script:ObservedStarts.Add([pscustomobject]@{
                process_id = [int]$candidate.process_id
                parent_process_id = [int]$candidate.parent_process_id
                name = [string]$candidate.name
                observed_at_utc = [datetime]::UtcNow.ToString('o')
                source = 'Win32_Process'
            })
        }
        if ($ForbiddenUtilityNames -contains ([string]$candidate.name).ToLowerInvariant()) {
            if (-not ($script:ForbiddenStarts | Where-Object process_id -EQ $candidate.process_id)) {
                $script:ForbiddenStarts.Add($candidate)
            }
        }
    }

    foreach ($candidate in $script:ObservedStarts) {
        if (-not $script:RunPids.Contains([int]$candidate.process_id)) {
            continue
        }
        if ($ForbiddenUtilityNames -contains ([string]$candidate.name).ToLowerInvariant()) {
            if (-not ($script:ForbiddenStarts | Where-Object process_id -EQ $candidate.process_id)) {
                $script:ForbiddenStarts.Add($candidate)
            }
        }
    }
    if ($script:ForbiddenStarts.Count -ne 0) {
        $names = ($script:ForbiddenStarts | ForEach-Object { "$($_.name) pid=$($_.process_id)" }) -join ', '
        throw "forbidden attributable Windows utility launch observed: $names"
    }
}

function Stop-ProcessObservation {
    if ($null -ne $script:Watcher) {
        try { $script:Watcher.Stop() } catch { }
    }
    if ($null -ne $script:WatcherSubscription) {
        try { Unregister-Event -SourceIdentifier $script:WatcherSubscription.Name -ErrorAction SilentlyContinue } catch { }
        try { Remove-Job -Id $script:WatcherSubscription.Id -Force -ErrorAction SilentlyContinue } catch { }
    }
    if ($null -ne $script:Watcher) {
        $script:Watcher.Dispose()
    }
}

function New-IsolatedEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$ColayHomePath,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$FakeProvider,
        [Parameter(Mandatory = $true)][string]$InspectionMarker
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
        'PATH' = (Split-Path -Parent $FakeProvider)
        'PATHEXT' = '.EXE;.CMD'
        'RUST_BACKTRACE' = '1'
    }
    foreach ($key in $ProviderKeyNames) {
        [void]$environment.Remove($key)
    }
    return $environment
}

function Start-HarnessProcess {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$ArgumentValues,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label,
        [AllowNull()][string]$StandardInputText,
        [switch]$CaptureFirstStdoutLine
    )
    Assert-FreeDisk | Out-Null
    Update-ProcessObservation
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
    $startedAt = [datetime]::UtcNow
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    if (-not $process.Start()) {
        throw "failed to start $Label"
    }
    [void]$script:RunPids.Add([int]$process.Id)
    $stdoutTask = if ($CaptureFirstStdoutLine) {
        $process.StandardOutput.ReadLineAsync()
    } else {
        $process.StandardOutput.ReadToEndAsync()
    }
    $stderrTask = if ($startInfo.RedirectStandardError) {
        $process.StandardError.ReadToEndAsync()
    } else {
        $null
    }
    if ($null -ne $StandardInputText) {
        $process.StandardInput.Write($StandardInputText)
        $process.StandardInput.Close()
    }
    return [pscustomobject]@{
        Process = $process
        StdoutTask = $stdoutTask
        StderrTask = $stderrTask
        Stopwatch = $stopwatch
        StartedAt = $startedAt
        Label = $Label
        ExecutableName = [System.IO.Path]::GetFileName($Executable)
        ArgumentCount = $ArgumentValues.Count
        CaptureFirstStdoutLine = [bool]$CaptureFirstStdoutLine
    }
}

function Wait-HarnessProcess {
    param(
        [Parameter(Mandatory = $true)]$Record,
        [Parameter(Mandatory = $true)][int]$TimeoutMs,
        [switch]$AllowFailure
    )
    while (-not $Record.Process.WaitForExit(10)) {
        Update-ProcessObservation
        if ($Record.Stopwatch.ElapsedMilliseconds -gt $TimeoutMs) {
            try { $Record.Process.Kill($true) } catch { try { $Record.Process.Kill() } catch { } }
            throw "$($Record.Label) exceeded hard process timeout ${TimeoutMs}ms"
        }
    }
    $Record.Process.WaitForExit()
    $Record.Stopwatch.Stop()
    Update-ProcessObservation
    $drainDeadline = [datetime]::UtcNow.AddSeconds(2)
    while ((-not $Record.StdoutTask.IsCompleted -or ($null -ne $Record.StderrTask -and -not $Record.StderrTask.IsCompleted)) -and [datetime]::UtcNow -lt $drainDeadline) {
        Start-Sleep -Milliseconds 10
        Update-ProcessObservation
    }
    if (-not $Record.StdoutTask.IsCompleted -or ($null -ne $Record.StderrTask -and -not $Record.StderrTask.IsCompleted)) {
        $stderrCompleted = $null -eq $Record.StderrTask -or $Record.StderrTask.IsCompleted
        throw "$($Record.Label) left redirected output handles open after process exit (stdout_completed=$($Record.StdoutTask.IsCompleted), stderr_completed=$stderrCompleted)"
    }
    $stdout = $Record.StdoutTask.GetAwaiter().GetResult()
    $stderr = if ($null -eq $Record.StderrTask) { '' } else { $Record.StderrTask.GetAwaiter().GetResult() }
    $result = [pscustomobject]@{
        label = $Record.Label
        executable = $Record.ExecutableName
        argument_count = $Record.ArgumentCount
        started_at_utc = $Record.StartedAt.ToString('o')
        elapsed_ms = [int64]$Record.Stopwatch.ElapsedMilliseconds
        exit_code = [int]$Record.Process.ExitCode
        stdout = $stdout
        stderr = $stderr
    }
    $script:CommandEvidence.Add($result)
    $Record.Process.Dispose()
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
        [switch]$AllowFailure
    )
    $record = Start-HarnessProcess -Executable $Executable -ArgumentValues $ArgumentValues `
        -WorkingDirectory $WorkingDirectory -Environment $Environment -Label $Label `
        -StandardInputText $StandardInputText -CaptureFirstStdoutLine:$CaptureFirstStdoutLine
    return Wait-HarnessProcess -Record $record -TimeoutMs $TimeoutMs -AllowFailure:$AllowFailure
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

function Assert-EquivalentJson {
    param($Expected, $Actual, [string]$Label)
    $expectedJson = $Expected | ConvertTo-Json -Depth 20 -Compress
    $actualJson = $Actual | ConvertTo-Json -Depth 20 -Compress
    if ($expectedJson -cne $actualJson) {
        throw "$Label changed: expected $expectedJson, found $actualJson"
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
        source_hashes_before = $hashes
        config_sha256 = Get-Sha256 (Join-Path $state 'config.toml')
    }
}

function Get-InspectionCount {
    param([Parameter(Mandatory = $true)][string]$Marker)
    if (-not (Test-Path -LiteralPath $Marker -PathType Leaf)) { return 0 }
    return @((Get-Content -LiteralPath $Marker) | Where-Object { $_ -ceq 'legacy-inspect' }).Count
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
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment
    )
    $database = Join-Path $script:ColayHome 'state/state.db'
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

    $seedEvidence = [System.Collections.Generic.List[object]]::new()
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
        $publishedPath = [string]$result.published_path
        $publishedDatabase = Join-Path $publishedPath 'legacy.db'
        if (-not (Test-Path -LiteralPath $publishedDatabase -PathType Leaf)) {
            throw "workspace $($seed.index) publication is missing legacy.db"
        }
        $seedEvidence.Add([pscustomobject]@{
            index = $seed.index
            workspace_id = [string]$rows[0].workspace_id
            canonical_path = [string]$rows[0].canonical_path
            source_fingerprint = [string]$rows[0].source_fingerprint
            manifest_hash = [string]$rows[0].manifest_hash
            published_path = $publishedPath
            published_hashes = Get-SqliteFamilyHashes $publishedDatabase
        })
    }
    return [pscustomobject]@{
        counts = $countMap
        seeds = $seedEvidence
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
        [switch]$AllowFailure
    )
    return Invoke-HarnessProcess -Executable $script:ResolvedColay -ArgumentValues $ArgumentValues `
        -WorkingDirectory $Repository -Environment $Environment -Label $Label -TimeoutMs $TimeoutMs `
        -StandardInputText $null -CaptureFirstStdoutLine -AllowFailure:$AllowFailure
}

function Assert-StatusJson {
    param($Result)
    if ([string]::IsNullOrWhiteSpace($Result.stdout)) {
        throw "$($Result.label) emitted empty stdout"
    }
    try { return $Result.stdout | ConvertFrom-Json -Depth 30 }
    catch { throw "$($Result.label) did not emit valid JSON: $($_.Exception.Message)" }
}

function Get-LiveAttributedProcesses {
    Update-ProcessObservation
    $interesting = @('colay.exe', 'colay-e2e-fake-provider.exe', 'whoami.exe', 'icacls.exe')
    return @(Get-CimInstance -ClassName Win32_Process | Where-Object {
        $script:RunPids.Contains([int]$_.ProcessId) -and $interesting -contains ([string]$_.Name).ToLowerInvariant()
    } | ForEach-Object {
        [pscustomobject]@{ process_id = [int]$_.ProcessId; parent_process_id = [int]$_.ParentProcessId; name = [string]$_.Name }
    })
}

function Stop-AttributedProcessesBestEffort {
    foreach ($processInfo in @(Get-LiveAttributedProcesses)) {
        if ([int]$processInfo.process_id -eq $PID) { continue }
        try { Stop-Process -Id ([int]$processInfo.process_id) -Force -ErrorAction SilentlyContinue } catch { }
    }
}

$runStamp = [datetime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
$summary = [ordered]@{
    schema_version = 1
    run_id = $runStamp
    started_at_utc = [datetime]::UtcNow.ToString('o')
    completed_at_utc = $null
    status = 'failed'
    failure = $null
    source_commit = $null
    response_timeout_ms = $ResponseTimeoutMs
    serial_limit_ms = $SerialP95LimitMs
    concurrent_limit_ms = $ConcurrentLimitMs
    serial_times_ms = @()
    serial_p95_ms = $null
    concurrent_times_ms = @()
    concurrent_max_ms = $null
    inspection_count = $null
    forbidden_utility_launches = @()
    residual_processes = @()
    sqlite_integrity = $null
    sqlite_foreign_key_violations = $null
    zero_writable_rows = $null
    durable_state = $null
    sources = @()
    minimum_free_gib = $null
    provider_key_names_cleared = $ProviderKeyNames
    fake_provider_only = $true
    sqlite_runtime = $null
    binaries = $null
    runtime_root = $null
}
$failureRecord = $null
$evidenceDirectory = $null
$environment = $null

try {
    if (-not $IsWindows) { throw 'windows-state-acl-stress.ps1 must run on native Windows' }
    $script:RepoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
    $script:ResolvedColay = Resolve-RequiredFile $ColayExe 'Colay executable'
    $resolvedFake = Resolve-RequiredFile $FakeProviderExe 'fake provider executable'
    if ([System.IO.Path]::GetFileName($resolvedFake) -cne 'colay-e2e-fake-provider.exe') {
        throw "only the colay-e2e-fake-provider.exe test-support binary is allowed"
    }
    $script:PythonExe = (Get-Command python.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $resolvedEvidenceRoot = [System.IO.Path]::GetFullPath($EvidenceRoot)
    New-Item -ItemType Directory -Path $resolvedEvidenceRoot -Force | Out-Null
    $evidenceDirectory = Join-Path $resolvedEvidenceRoot "run-$runStamp"
    New-Item -ItemType Directory -Path $evidenceDirectory -Force | Out-Null
    $script:RunRoot = Join-Path ([System.IO.Path]::GetTempPath()) "colay-acl-$runStamp"
    if (Test-Path -LiteralPath $script:RunRoot) {
        throw "isolated runtime root already exists: $script:RunRoot"
    }
    $summary.runtime_root = $script:RunRoot
    $script:ColayHome = Join-Path $script:RunRoot 'colay-home'
    $workspaceRoot = Join-Path $script:RunRoot 'workspaces'
    $emptyRepository = Join-Path $workspaceRoot 'empty-incumbent'
    $inspectionMarker = Join-Path $script:RunRoot 'temp/legacy-inspections.log'
    foreach ($directory in @($script:RunRoot, $workspaceRoot, $emptyRepository, $script:ColayHome)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $environment = New-IsolatedEnvironment -ColayHomePath $script:ColayHome -Root $script:RunRoot `
        -FakeProvider $resolvedFake -InspectionMarker $inspectionMarker
    New-FakeProviderConfig -ColayHomePath $script:ColayHome -FakeProvider $resolvedFake

    $timeoutSource = Get-Content -LiteralPath (Join-Path $script:RepoRoot 'crates/orchestrator-cli/src/ipc_client.rs') -Raw
    if ($timeoutSource -notmatch 'RESPONSE_TIMEOUT\s*:\s*Duration\s*=\s*Duration::from_secs\(10\)') {
        throw 'source RESPONSE_TIMEOUT is not exactly Duration::from_secs(10)'
    }
    if ($ResponseTimeoutMs -ne 10000) { throw 'harness response timeout invariant changed' }
    Assert-FreeDisk | Out-Null
    Start-AncestryObservation
    $sqliteVersionResult = Invoke-HarnessProcess -Executable $script:PythonExe `
        -ArgumentValues @('-I', '-c', 'import sqlite3; print(sqlite3.sqlite_version)') `
        -WorkingDirectory $script:RepoRoot -Environment $environment -Label 'sqlite-runtime-version' `
        -TimeoutMs 10000 -StandardInputText $null
    $sqliteVersion = [version]$sqliteVersionResult.stdout.Trim()
    if ($sqliteVersion -lt [version]'3.37.0') {
        throw "Python SQLite runtime $sqliteVersion cannot read schema-17 STRICT tables"
    }
    $summary.sqlite_runtime = [pscustomobject]@{ executable = $script:PythonExe; sqlite_version = $sqliteVersion.ToString() }
    $summary.binaries = [pscustomobject]@{
        colay = [pscustomobject]@{ path = $script:ResolvedColay; sha256 = Get-Sha256 $script:ResolvedColay }
        fake_provider = [pscustomobject]@{ path = $resolvedFake; sha256 = Get-Sha256 $resolvedFake }
    }
    $gitExe = (Get-Command git.exe -CommandType Application -ErrorAction Stop).Source
    $gitResult = Invoke-HarnessProcess -Executable $gitExe -ArgumentValues @('-C', $script:RepoRoot, 'rev-parse', 'HEAD') `
        -WorkingDirectory $script:RepoRoot -Environment $environment -Label 'source-commit' -TimeoutMs 10000 `
        -StandardInputText $null
    $summary.source_commit = $gitResult.stdout.Trim()

    $started = Invoke-Colay -Repository $emptyRepository -ArgumentValues @('--json', 'daemon', 'start') `
        -Environment $environment -Label 'start-empty-incumbent' -TimeoutMs 40000
    [void](Assert-StatusJson $started)
    if ((Get-InspectionCount $inspectionMarker) -ne 0) {
        throw 'empty incumbent unexpectedly inspected a legacy source'
    }
    $seeds = [System.Collections.Generic.List[object]]::new()
    [void](Assert-DurableState -Seeds @() -ExpectedWorkspaceCount 1 -Environment $environment)

    $serialTimes = [System.Collections.Generic.List[int64]]::new()
    for ($index = 1; $index -le 5; $index++) {
        $beforeMarkers = Get-InspectionCount $inspectionMarker
        $seed = New-LegacyWorkspace -Index $index -Root $workspaceRoot -Environment $environment
        $seeds.Add($seed)
        $result = Invoke-Colay -Repository $seed.repository -ArgumentValues @('--json', 'status') `
            -Environment $environment -Label "serial-register-$index" -TimeoutMs 12000
        [void](Assert-StatusJson $result)
        $serialTimes.Add([int64]$result.elapsed_ms)
        $afterMarkers = Get-InspectionCount $inspectionMarker
        if (($afterMarkers - $beforeMarkers) -ne 2) {
            throw "serial workspace $index produced $($afterMarkers - $beforeMarkers) inspections; expected exactly 2"
        }
        [void](Assert-DurableState -Seeds $seeds.ToArray() -ExpectedWorkspaceCount (1 + $seeds.Count) -Environment $environment)
        Assert-EquivalentJson $seed.source_hashes_before (Get-SqliteFamilyHashes $seed.database) "serial source $index SQLite family"
    }
    $sortedSerial = @($serialTimes.ToArray() | Sort-Object)
    $p95Index = [math]::Ceiling(0.95 * $sortedSerial.Count) - 1
    $serialP95 = [int64]$sortedSerial[$p95Index]
    if ($serialP95 -gt $SerialP95LimitMs) {
        throw "nearest-rank serial p95 ${serialP95}ms exceeded ${SerialP95LimitMs}ms"
    }
    $summary.serial_times_ms = $serialTimes.ToArray()
    $summary.serial_p95_ms = $serialP95

    $concurrentSeeds = [System.Collections.Generic.List[object]]::new()
    foreach ($index in 6..9) {
        $seed = New-LegacyWorkspace -Index $index -Root $workspaceRoot -Environment $environment
        $seeds.Add($seed)
        $concurrentSeeds.Add($seed)
    }
    $concurrentMarkerStart = Get-InspectionCount $inspectionMarker
    $running = [System.Collections.Generic.List[object]]::new()
    foreach ($seed in $concurrentSeeds) {
        $running.Add((Start-HarnessProcess -Executable $script:ResolvedColay -ArgumentValues @('--json', 'status') `
            -WorkingDirectory $seed.repository -Environment $environment -Label "concurrent-register-$($seed.index)" `
            -StandardInputText $null -CaptureFirstStdoutLine))
    }
    $concurrentTimes = [System.Collections.Generic.List[int64]]::new()
    foreach ($record in $running) {
        $result = Wait-HarnessProcess -Record $record -TimeoutMs 12000
        [void](Assert-StatusJson $result)
        if ([int64]$result.elapsed_ms -gt $ConcurrentLimitMs) {
            throw "$($result.label) took $($result.elapsed_ms)ms, exceeding ${ConcurrentLimitMs}ms"
        }
        $concurrentTimes.Add([int64]$result.elapsed_ms)
    }
    $concurrentMarkerEnd = Get-InspectionCount $inspectionMarker
    if (($concurrentMarkerEnd - $concurrentMarkerStart) -ne 8) {
        throw "concurrent workspaces produced $($concurrentMarkerEnd - $concurrentMarkerStart) inspections; expected exactly 8"
    }
    $summary.concurrent_times_ms = $concurrentTimes.ToArray()
    $summary.concurrent_max_ms = [int64](($concurrentTimes.ToArray() | Measure-Object -Maximum).Maximum)
    $summary.inspection_count = $concurrentMarkerEnd

    $durableState = Assert-DurableState -Seeds $seeds.ToArray() -ExpectedWorkspaceCount 10 -Environment $environment
    $summary.durable_state = $durableState
    foreach ($seed in $seeds) {
        $after = Get-SqliteFamilyHashes $seed.database
        Assert-EquivalentJson $seed.source_hashes_before $after "source $($seed.index) SQLite family"
        $summary.sources += [pscustomobject]@{
            index = $seed.index
            session_id = $seed.session_id
            database = $seed.database
            sqlite_family_hashes = $after
            config_sha256 = $seed.config_sha256
        }
    }
    $globalDatabase = Join-Path $script:ColayHome 'state/state.db'
    $summary.zero_writable_rows = Assert-ZeroWritableRows -Database $globalDatabase -Environment $environment
    Assert-DatabaseHealth -Database $globalDatabase -Environment $environment
    $summary.sqlite_integrity = 'ok'
    $summary.sqlite_foreign_key_violations = 0

    $stopped = Invoke-Colay -Repository $emptyRepository -ArgumentValues @('--json', 'daemon', 'stop') `
        -Environment $environment -Label 'stop-incumbent' -TimeoutMs 20000
    [void](Assert-StatusJson $stopped)
    $cleanupDeadline = [datetime]::UtcNow.AddSeconds(10)
    do {
        $liveRows = Invoke-Sqlite -Database $globalDatabase `
            -Sql 'SELECT count(*) AS row_count FROM daemon_instances WHERE released_at IS NULL;' `
            -WorkingDirectory $script:RunRoot -Environment $environment -ReadOnly -Csv -Label 'cleanup-live-leases'
        $residual = @(Get-LiveAttributedProcesses)
        if ([int]$liveRows[0].row_count -eq 0 -and $residual.Count -eq 0) { break }
        Start-Sleep -Milliseconds 50
    } while ([datetime]::UtcNow -lt $cleanupDeadline)
    if ([int]$liveRows[0].row_count -ne 0) { throw 'daemon live lease residue remained after stop' }
    if ($residual.Count -ne 0) { throw "attributable process residue remained after stop: $($residual | ConvertTo-Json -Compress)" }
    $statusAfterStop = Invoke-Colay -Repository $emptyRepository -ArgumentValues @('--json', 'daemon', 'status') `
        -Environment $environment -Label 'endpoint-status-after-stop' -TimeoutMs 10000
    $statusDocument = Assert-StatusJson $statusAfterStop
    if (($statusDocument | ConvertTo-Json -Depth 20 -Compress) -notmatch 'stopped') {
        throw 'post-stop daemon status did not prove the endpoint stopped'
    }
    $summary.residual_processes = @(Get-LiveAttributedProcesses)
    if ($summary.residual_processes.Count -ne 0) { throw 'post-status attributable process residue remained' }
    Update-ProcessObservation
    $summary.forbidden_utility_launches = @($script:ForbiddenStarts)
    if ($summary.forbidden_utility_launches.Count -ne 0) { throw 'forbidden utility launch count was nonzero' }
    $summary.status = 'passed'
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
    if ($null -ne $environment -and $null -ne $script:ResolvedColay -and $null -ne $script:RunRoot) {
        try {
            if (Test-Path -LiteralPath (Join-Path $script:ColayHome 'state/state.db')) {
                $empty = Join-Path $script:RunRoot 'workspaces/empty-incumbent'
                [void](Invoke-Colay -Repository $empty -ArgumentValues @('--json', 'daemon', 'stop') `
                    -Environment $environment -Label 'finally-stop' -TimeoutMs 10000 -AllowFailure)
            }
        } catch { }
    }
    try { Update-ProcessObservation } catch {
        if ($null -eq $summary.failure) {
            $summary.failure = [pscustomobject]@{ message = $_.Exception.Message; category = 'process-observation'; script_stack = $_.ScriptStackTrace }
        }
    }
    try { Stop-AttributedProcessesBestEffort } catch { }
    try { Update-ProcessObservation } catch { }
    $summary.forbidden_utility_launches = @($script:ForbiddenStarts)
    $summary.residual_processes = @(Get-LiveAttributedProcesses)
    $summary.minimum_free_gib = if ([double]::IsPositiveInfinity($script:MinimumObservedFreeGiB)) { $null } else { $script:MinimumObservedFreeGiB }
    $summary.completed_at_utc = [datetime]::UtcNow.ToString('o')
    if ($null -ne $evidenceDirectory) {
        $evidence = [ordered]@{
            summary = $summary
            commands = $script:CommandEvidence
            attributable_process_ids = @($script:RunPids | Sort-Object)
            observed_process_starts = $script:ObservedStarts
            process_snapshots = $script:ProcessSnapshots
            process_observation_mode = $script:ProcessObservationMode
            process_event_subscription_failure = $script:WatcherFailure
        }
        $evidencePath = Join-Path (Split-Path -Parent $evidenceDirectory) "windows-state-acl-stress-$runStamp.json"
        $summaryPath = Join-Path (Split-Path -Parent $evidenceDirectory) 'summary.json'
        $evidence | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
        $summary | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $summaryPath -Encoding utf8NoBOM
    }
    Stop-ProcessObservation
}

if ($null -ne $failureRecord) {
    throw $failureRecord
}

$summary | ConvertTo-Json -Depth 20
