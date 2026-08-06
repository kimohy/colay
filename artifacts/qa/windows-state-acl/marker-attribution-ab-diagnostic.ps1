#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ColayExe,
    [Parameter(Mandatory = $true)][string]$FakeProviderExe,
    [Parameter(Mandatory = $true)][string]$StressHarness,
    [Parameter(Mandatory = $true)][string]$EvidenceRoot,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{64}$')][string]$ExpectedColaySha256,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{64}$')][string]$ExpectedFakeProviderSha256,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{64}$')][string]$ExpectedStressHarnessSha256,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-f]{64}$')][string]$ExpectedDiagnosticSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$MinimumFreeGiB = 5
$CimOperationTimeoutSec = 5
$DaemonReadinessTimeoutMs = 5000
$DaemonReadinessPollIntervalMs = 50
$DaemonReadinessCleanupReserveMs = 100
$ExpectedObservationCount = 8
$ExpectedPairCount = 4
$ExpectedRetryCount = 0
$AbsoluteDeltaFloorMs = 100
$RelativeDeltaFraction = 0.05
$MaximumPairExceedanceCount = 1
$ProviderKeyNames = @(
    'OPENAI_API_KEY',
    'ANTHROPIC_API_KEY',
    'GEMINI_API_KEY',
    'GOOGLE_API_KEY',
    'AGY_API_KEY',
    'CODEX_API_KEY',
    'CLAUDE_API_KEY'
)
$PairOrders = @(
    [pscustomobject]@{ variants = @('aggregate_only', 'attributed') },
    [pscustomobject]@{ variants = @('attributed', 'aggregate_only') },
    [pscustomobject]@{ variants = @('aggregate_only', 'attributed') },
    [pscustomobject]@{ variants = @('attributed', 'aggregate_only') }
)
$ExpectedHashCheckpointLabels = [System.Collections.Generic.List[string]]::new()
$ExpectedHashCheckpointLabels.Add('initial-pre-mutation')
for ($pairNumber = 1; $pairNumber -le $ExpectedPairCount; $pairNumber++) {
    $ExpectedHashCheckpointLabels.Add(("before-pair-{0:D2}" -f $pairNumber))
    $ExpectedHashCheckpointLabels.Add(("after-pair-{0:D2}" -f $pairNumber))
}
$ExpectedHashCheckpointLabels.Add('final')
$ExpectedHashCheckpointLabels = $ExpectedHashCheckpointLabels.ToArray()
$SchemaV8SeedMigrationNames = @(
    'core',
    'execution',
    'audit_and_control',
    'durable_sessions',
    'chat_workspace_state',
    'approved_task_graphs',
    'parallel_execution',
    'result_integration'
)
$StressFunctionRoots = @(
    'Resolve-RequiredFile',
    'Start-HarnessProcess',
    'Wait-HarnessProcess',
    'Invoke-HarnessProcess',
    'ConvertTo-TomlPath',
    'New-FakeProviderConfig',
    'Invoke-Sqlite',
    'Get-Sha256',
    'Get-SqliteFamilyHashes',
    'Assert-EquivalentJson',
    'ConvertTo-ComparableWindowsPath',
    'Assert-ControlledPublicationContents',
    'Assert-GlobalPublicationLayout',
    'New-LegacyWorkspace',
    'Assert-DatabaseHealth',
    'Assert-DurableState',
    'Assert-ZeroWritableRows',
    'Invoke-Colay',
    'Assert-StatusJson',
    'Assert-ExactStoppedStatus',
    'Get-AttributedInspectionSnapshot'
)
$StressFunctionOverrides = @('Assert-FreeDisk', 'New-LegacyWorkspace', 'Update-ProcessObservation')
$AllowedImportedScriptVariables = @(
    'script:ColayHome',
    'script:CommandEvidence',
    'script:HarnessProcessIdentity',
    'script:OwnedProcessIdentities',
    'script:ProcessExitTimeFailureForTest',
    'script:ProcessFinalizeFailureForTest',
    'script:ProcessSetupFailureEvidence',
    'script:ProcessSetupFailureForTest',
    'script:PythonExe',
    'script:ResolvedColay',
    'script:RunRoot'
)

$script:AbVolumeRoots = [ordered]@{}
$script:MinimumFreeGiBByRoot = [ordered]@{}
$script:CommandEvidence = [System.Collections.Generic.List[object]]::new()
$script:OwnedProcessIdentities = [System.Collections.Generic.List[object]]::new()
$script:ProcessSetupFailureEvidence = [System.Collections.Generic.List[object]]::new()
$script:HarnessProcessIdentity = $null
$script:ProcessExitTimeFailureForTest = $false
$script:ProcessFinalizeFailureForTest = $null
$script:ProcessSetupFailureForTest = $null
$script:RepoRoot = $null
$script:RunRoot = $null
$script:ColayHome = $null
$script:ResolvedColay = $null
$script:ResolvedFake = $null
$script:ResolvedStress = $null
$script:ResolvedDiagnostic = $null
$script:PythonExe = $null

function Resolve-AbFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $resolved = [System.IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path)
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        throw "$Label is not a file: $resolved"
    }
    return $resolved
}

function ConvertTo-AbComparableWindowsPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    $full = [System.IO.Path]::GetFullPath($Path)
    if ($full.StartsWith('\\?\UNC\', [System.StringComparison]::OrdinalIgnoreCase)) {
        $full = '\\' + $full.Substring(8)
    } elseif ($full.StartsWith('\\?\', [System.StringComparison]::OrdinalIgnoreCase)) {
        $full = $full.Substring(4)
    }
    return $full.TrimEnd('\')
}

function ConvertTo-AbNormalizedProcessCreationUtc {
    param([Parameter(Mandatory = $true)]$Value)
    $createdAt = if ($Value -is [datetime]) {
        ([datetime]$Value).ToUniversalTime()
    } else {
        $text = [string]$Value
        if ($text -match '^\d{14}\.\d{6}[+-]\d{3}$') {
            [System.Management.ManagementDateTimeConverter]::ToDateTime($text).ToUniversalTime()
        } else {
            [datetime]::Parse(
                $text,
                [Globalization.CultureInfo]::InvariantCulture,
                [Globalization.DateTimeStyles]::AssumeUniversal
            ).ToUniversalTime()
        }
    }
    return [datetime]::new(
        $createdAt.Ticks - ($createdAt.Ticks % 10),
        [DateTimeKind]::Utc
    )
}

function ConvertTo-AbTomlString {
    param([Parameter(Mandatory = $true)][string]$Value)
    return '"' + $Value.Replace('\', '\\').Replace('"', '\"') + '"'
}

function Get-AbSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
}

function Assert-AbEquivalentJson {
    param(
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)]$Actual,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $expectedJson = $Expected | ConvertTo-Json -Depth 30 -Compress
    $actualJson = $Actual | ConvertTo-Json -Depth 30 -Compress
    if ($expectedJson -cne $actualJson) {
        throw "$Label changed: expected $expectedJson, found $actualJson"
    }
}

function Get-AbCommandLeafName {
    param([AllowNull()][string]$Name)
    if ([string]::IsNullOrWhiteSpace($Name)) { return $null }
    $separator = $Name.LastIndexOf('\')
    if ($separator -ge 0) { return $Name.Substring($separator + 1) }
    return $Name
}

function Test-AbTrustedStressFunctionImportMutation {
    param([Parameter(Mandatory = $true)]$Command)
    if ([string]$Command.Extent.Text -cne
        'Set-Item -Path "Function:\script:$name" -Value $definitions[$name].Body.GetScriptBlock() -Force') {
        return $false
    }
    $owner = $Command.Parent
    while ($null -ne $owner -and
        $owner -isnot [System.Management.Automation.Language.FunctionDefinitionAst]) {
        $owner = $owner.Parent
    }
    return $null -ne $owner -and $owner.Name -ceq 'Import-StressHarnessFunctions'
}

function Register-AbVolume {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $root = [System.IO.Path]::GetPathRoot([System.IO.Path]::GetFullPath($Path))
    if ([string]::IsNullOrWhiteSpace($root)) {
        throw "cannot resolve volume for ${Label}: $Path"
    }
    $drive = [System.IO.DriveInfo]::new($root)
    if (-not $drive.IsReady) {
        throw "volume is not ready for ${Label}: $root"
    }
    $key = $drive.RootDirectory.FullName.TrimEnd('\').ToLowerInvariant()
    if (-not $script:AbVolumeRoots.Contains($key)) {
        $script:AbVolumeRoots[$key] = [pscustomobject]@{
            root = $drive.RootDirectory.FullName
            labels = [System.Collections.Generic.List[string]]::new()
        }
        $script:MinimumFreeGiBByRoot[$key] = [double]::PositiveInfinity
    }
    if (-not $script:AbVolumeRoots[$key].labels.Contains($Label)) {
        $script:AbVolumeRoots[$key].labels.Add($Label)
    }
}

function Assert-FreeDisk {
    if ($script:AbVolumeRoots.Count -eq 0) {
        throw 'no diagnostic volume was registered for free-space checks'
    }
    $observed = [System.Collections.Generic.List[object]]::new()
    foreach ($entry in $script:AbVolumeRoots.GetEnumerator()) {
        $drive = [System.IO.DriveInfo]::new([string]$entry.Value.root)
        if (-not $drive.IsReady) {
            throw "registered volume is not ready: $($entry.Value.root)"
        }
        $freeGiB = [math]::Round($drive.AvailableFreeSpace / 1GB, 3)
        if ($freeGiB -lt [double]$script:MinimumFreeGiBByRoot[$entry.Key]) {
            $script:MinimumFreeGiBByRoot[$entry.Key] = $freeGiB
        }
        $observed.Add([pscustomobject]@{
            root = [string]$entry.Value.root
            labels = @($entry.Value.labels)
            free_gib = $freeGiB
        })
        if ($freeGiB -lt $MinimumFreeGiB) {
            throw "free space fell below ${MinimumFreeGiB}GiB on $($entry.Value.root): ${freeGiB}GiB"
        }
    }
    return $observed.ToArray()
}

function Get-AbDiskEvidence {
    return @($script:AbVolumeRoots.GetEnumerator() | ForEach-Object {
        $minimum = [double]$script:MinimumFreeGiBByRoot[$_.Key]
        [pscustomobject]@{
            root = $_.Value.root
            labels = @($_.Value.labels)
            minimum_free_gib = if ([double]::IsPositiveInfinity($minimum)) { $null } else { $minimum }
        }
    })
}

function Update-ProcessObservation {
    # Deliberate override: the A/B latency path has no synchronous CIM observation.
}

function New-LegacyWorkspace {
    param(
        [Parameter(Mandatory = $true)][int]$Index,
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment
    )
    $repository = Join-Path $Root ("legacy-workspace-{0:D2}" -f $Index)
    $state = Join-Path $repository '.colay'
    [void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetFullPath($state))
    $configPath = Join-Path $state 'config.toml'
    [System.IO.File]::WriteAllText(
        [System.IO.Path]::GetFullPath($configPath),
        "config_version = 4`n",
        [System.Text.UTF8Encoding]::new($false)
    )
    $database = Join-Path $state 'orchestrator.db'
    $names = @(
        'core',
        'execution',
        'audit_and_control',
        'durable_sessions',
        'chat_workspace_state',
        'approved_task_graphs',
        'parallel_execution',
        'result_integration'
    )
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
        inspection_group_id = $null
        source_evidence = $null
        config_sha256 = Get-Sha256 $configPath
    }
}

function Get-AbSchemaV8SeedMigrationHashes {
    $hashes = [System.Collections.Generic.List[object]]::new()
    for ($index = 0; $index -lt $SchemaV8SeedMigrationNames.Count; $index++) {
        $version = $index + 1
        $relative = "migrations/{0:D4}_{1}.sql" -f $version, $SchemaV8SeedMigrationNames[$index]
        $path = Join-Path $script:RepoRoot $relative
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "missing diagnostic seed migration: $path"
        }
        $hashes.Add([pscustomobject][ordered]@{
            version = $version
            relative_path = $relative.Replace('\', '/')
            bytes = (Get-Item -LiteralPath $path -ErrorAction Stop).Length
            sha256 = Get-AbSha256 $path
        })
    }
    if ($hashes.Count -ne 8) {
        throw "schema-v8 diagnostic seed migration hash count was $($hashes.Count); expected exactly 8"
    }
    return [pscustomobject][ordered]@{
        scope = 'schema-v8-seed-inputs-only'
        rationale = '0001..0008 are read directly by New-LegacyWorkspace; later embedded migrations are pinned by the colay binary SHA-256'
        expected_count = 8
        actual_count = $hashes.Count
        files = $hashes.ToArray()
    }
}

function Get-AbInputHashCheckpoint {
    param(
        [Parameter(Mandatory = $true)][string]$Label,
        [AllowNull()]$ExpectedMigrationHashes
    )
    $actualColay = Get-AbSha256 $script:ResolvedColay
    $actualFake = Get-AbSha256 $script:ResolvedFake
    $actualStress = Get-AbSha256 $script:ResolvedStress
    $actualDiagnostic = Get-AbSha256 $script:ResolvedDiagnostic
    foreach ($check in @(
        @('colay', $actualColay, $ExpectedColaySha256),
        @('fake provider', $actualFake, $ExpectedFakeProviderSha256),
        @('stress harness', $actualStress, $ExpectedStressHarnessSha256),
        @('diagnostic script', $actualDiagnostic, $ExpectedDiagnosticSha256)
    )) {
        if ([string]$check[1] -cne [string]$check[2]) {
            throw "$Label $($check[0]) SHA-256 mismatch: expected $($check[2]), found $($check[1])"
        }
    }
    $migrations = Get-AbSchemaV8SeedMigrationHashes
    if ($null -ne $ExpectedMigrationHashes) {
        Assert-AbEquivalentJson $ExpectedMigrationHashes $migrations "$Label migration inputs"
    }
    return [pscustomobject][ordered]@{
        label = $Label
        observed_at_utc = [datetime]::UtcNow.ToString('o')
        colay_sha256 = $actualColay
        fake_provider_sha256 = $actualFake
        stress_harness_sha256 = $actualStress
        diagnostic_script_sha256 = $actualDiagnostic
        migrations = $migrations
    }
}

function Get-AbAstContractViolations {
    param(
        [Parameter(Mandatory = $true)]$Ast,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $violations = [System.Collections.Generic.List[string]]::new()
    $stopProcessNames = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($name in @('Stop-Process', 'spps', 'kill')) { [void]$stopProcessNames.Add($name) }
    $cimCommandNames = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($name in @('Get-CimInstance', 'gcim')) { [void]$cimCommandNames.Add($name) }
    $aliasMutationNames = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($name in @('Set-Alias', 'sal', 'New-Alias', 'nal', 'Import-Alias', 'ipal')) {
        [void]$aliasMutationNames.Add($name)
    }
    $dynamicEvaluationNames = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($name in @('Invoke-Expression', 'iex')) { [void]$dynamicEvaluationNames.Add($name) }
    $aliasProviderMutationNames = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($name in @(
        'Set-Item', 'si',
        'New-Item', 'ni',
        'Remove-Item', 'ri', 'rm', 'rmdir', 'del', 'erase', 'rd',
        'Clear-Item', 'cli',
        'Copy-Item', 'cpi', 'cp', 'copy',
        'Move-Item', 'mi', 'mv', 'move',
        'Rename-Item', 'rni', 'ren'
    )) {
        [void]$aliasProviderMutationNames.Add($name)
    }
    foreach ($command in @($Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.CommandAst]
    }, $true))) {
        if ($command.InvocationOperator -ne [System.Management.Automation.Language.TokenKind]::Unknown) {
            $violations.Add("$Label uses forbidden invocation operator '$($command.InvocationOperator)' at line $($command.Extent.StartLineNumber)")
        }
        $name = Get-AbCommandLeafName ($command.GetCommandName())
        if ([string]::IsNullOrWhiteSpace($name)) {
            $violations.Add("$Label uses a dynamic command invocation at line $($command.Extent.StartLineNumber)")
            continue
        }
        if ($stopProcessNames.Contains($name)) {
            $violations.Add("$Label uses forbidden PID-only Stop-Process or alias '$name' at line $($command.Extent.StartLineNumber)")
        }
        if ($aliasMutationNames.Contains($name)) {
            $violations.Add("$Label mutates command aliases with '$name' at line $($command.Extent.StartLineNumber)")
        }
        if ($dynamicEvaluationNames.Contains($name)) {
            $violations.Add("$Label uses forbidden dynamic evaluation '$name' at line $($command.Extent.StartLineNumber)")
        }
        if ($aliasProviderMutationNames.Contains($name)) {
            $trustedFunctionImport = Test-AbTrustedStressFunctionImportMutation $command
            if (-not $trustedFunctionImport) {
                $violations.Add("$Label uses forbidden alias-capable provider mutator '$name' outside the exact stress-function import site at line $($command.Extent.StartLineNumber)")
            }
        }
        if ($cimCommandNames.Contains($name) -and
            $command.Extent.Text -cnotmatch '(?i)-OperationTimeoutSec\b') {
            $violations.Add("$Label has unbounded Get-CimInstance or alias '$name' at line $($command.Extent.StartLineNumber)")
        }
    }
    foreach ($assignment in @($Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
            $node.Left -is [System.Management.Automation.Language.VariableExpressionAst] -and
            $node.Left.VariablePath.UserPath -match '(?i)^Alias:'
    }, $true))) {
        $violations.Add("$Label mutates the Alias: provider through a scoped variable at line $($assignment.Extent.StartLineNumber)")
    }
    foreach ($member in @($Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.MemberExpressionAst] -and
            $node.Member -isnot [System.Management.Automation.Language.StringConstantExpressionAst]
    }, $true))) {
        $kind = if ($member -is [System.Management.Automation.Language.InvokeMemberExpressionAst]) {
            'invocation'
        } else {
            'access'
        }
        $violations.Add("$Label uses forbidden dynamic member $kind at line $($member.Extent.StartLineNumber)")
    }
    foreach ($member in @($Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.MemberExpressionAst] -and
            $node.Member -is [System.Management.Automation.Language.StringConstantExpressionAst] -and
            [string]$node.Member.Value -ieq 'HasExited'
    }, $true))) {
        $violations.Add("$Label uses forbidden HasExited at line $($member.Extent.StartLineNumber)")
    }
    foreach ($invocation in @($Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.InvokeMemberExpressionAst] -and
            $node.Member -is [System.Management.Automation.Language.StringConstantExpressionAst] -and
            [string]$node.Member.Value -ieq 'WaitForExit' -and
            ($null -eq $node.Arguments -or $node.Arguments.Count -eq 0)
    }, $true))) {
        $violations.Add("$Label uses parameterless WaitForExit at line $($invocation.Extent.StartLineNumber)")
    }
    return $violations.ToArray()
}

function Test-AbStandaloneWaitStopStatement {
    param([Parameter(Mandatory = $true)]$Statement)
    if ($Statement -isnot [System.Management.Automation.Language.PipelineAst] -or
        $Statement.PipelineElements.Count -ne 1) {
        return $false
    }
    $commandExpression = $Statement.PipelineElements[0]
    if ($commandExpression -isnot [System.Management.Automation.Language.CommandExpressionAst] -or
        $commandExpression.Redirections.Count -ne 0) {
        return $false
    }
    $stopInvocation = $commandExpression.Expression
    if ($stopInvocation -isnot [System.Management.Automation.Language.InvokeMemberExpressionAst] -or
        $stopInvocation.Static -or
        $stopInvocation.Member -isnot [System.Management.Automation.Language.StringConstantExpressionAst] -or
        [string]$stopInvocation.Member.Value -ine 'Stop' -or
        ($null -ne $stopInvocation.Arguments -and $stopInvocation.Arguments.Count -ne 0)) {
        return $false
    }
    $stopwatchMember = $stopInvocation.Expression
    if ($stopwatchMember -isnot [System.Management.Automation.Language.MemberExpressionAst] -or
        $stopwatchMember.Static -or
        $stopwatchMember.Member -isnot [System.Management.Automation.Language.StringConstantExpressionAst] -or
        [string]$stopwatchMember.Member.Value -ine 'Stopwatch') {
        return $false
    }
    $recordVariable = $stopwatchMember.Expression
    return $recordVariable -is [System.Management.Automation.Language.VariableExpressionAst] -and
        -not $recordVariable.VariablePath.IsDriveQualified -and
        [string]$recordVariable.VariablePath.UserPath -ieq 'Record'
}

function Test-AbAstIsDescendantOf {
    param(
        [Parameter(Mandatory = $true)]$Node,
        [Parameter(Mandatory = $true)]$Ancestor
    )
    $cursor = $Node
    while ($null -ne $cursor) {
        if ([object]::ReferenceEquals($cursor, $Ancestor)) { return $true }
        $cursor = $cursor.Parent
    }
    return $false
}

function Test-AbStatementDominatesAst {
    param(
        [Parameter(Mandatory = $true)]$Statement,
        [Parameter(Mandatory = $true)]$Target,
        [Parameter(Mandatory = $true)]$Scope
    )
    $ancestor = $Target.Parent
    while ($null -ne $ancestor) {
        if (($ancestor -is [System.Management.Automation.Language.StatementBlockAst] -or
                $ancestor -is [System.Management.Automation.Language.NamedBlockAst]) -and
            [object]::ReferenceEquals($Statement.Parent, $ancestor) -and
            $Statement.Extent.EndOffset -le $Target.Extent.StartOffset) {
            return $true
        }
        if ([object]::ReferenceEquals($ancestor, $Scope)) { break }
        $ancestor = $ancestor.Parent
    }
    return $false
}

function Test-AbCommandRunsAfterWaitStop {
    param(
        [Parameter(Mandatory = $true)]$Command,
        [Parameter(Mandatory = $true)]$WaitFunction
    )
    $commandScope = $Command.Parent
    while ($null -ne $commandScope -and
        $commandScope -isnot [System.Management.Automation.Language.ScriptBlockAst]) {
        $commandScope = $commandScope.Parent
    }
    if ($null -eq $commandScope) { return $false }

    $latestStopOffset = -1
    $ancestor = $Command.Parent
    while ($null -ne $ancestor) {
        if ($ancestor -is [System.Management.Automation.Language.StatementBlockAst] -or
            $ancestor -is [System.Management.Automation.Language.NamedBlockAst]) {
            foreach ($statement in @($ancestor.Statements)) {
                if ($statement.Extent.EndOffset -gt $Command.Extent.StartOffset -or
                    -not (Test-AbStandaloneWaitStopStatement $statement)) {
                    continue
                }
                if ((Test-AbStatementDominatesAst -Statement $statement -Target $Command -Scope $commandScope) -and
                    $statement.Extent.EndOffset -gt $latestStopOffset) {
                    $latestStopOffset = $statement.Extent.EndOffset
                }
            }
        }
        if ([object]::ReferenceEquals($ancestor, $commandScope)) { break }
        $ancestor = $ancestor.Parent
    }
    if ($latestStopOffset -lt 0) { return $false }

    $restartInvocations = @($WaitFunction.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.InvokeMemberExpressionAst] -and
            $node.Member -is [System.Management.Automation.Language.StringConstantExpressionAst] -and
            ([string]$node.Member.Value -ieq 'Start' -or [string]$node.Member.Value -ieq 'Restart') -and
            [string]$node.Expression.Extent.Text -ieq '$Record.Stopwatch'
    }, $true) | Where-Object {
        $_.Extent.StartOffset -gt $latestStopOffset -and
            $_.Extent.StartOffset -lt $Command.Extent.StartOffset
    })
    return $restartInvocations.Count -eq 0
}

function Get-AbNearestScriptBlockAst {
    param([Parameter(Mandatory = $true)]$Node)
    $cursor = $Node
    while ($null -ne $cursor) {
        if ($cursor -is [System.Management.Automation.Language.ScriptBlockAst]) { return $cursor }
        $cursor = $cursor.Parent
    }
    return $null
}

function Get-AbNearestFunctionDefinitionAst {
    param([Parameter(Mandatory = $true)]$Node)
    $cursor = $Node
    while ($null -ne $cursor) {
        if ($cursor -is [System.Management.Automation.Language.FunctionDefinitionAst]) { return $cursor }
        $cursor = $cursor.Parent
    }
    return $null
}

function Get-AbOuterLexicalScriptBlockAst {
    param(
        [Parameter(Mandatory = $true)]$Scope,
        [Parameter(Mandatory = $true)]$Definition
    )
    $cursor = $Scope.Parent
    while ($null -ne $cursor) {
        if ($cursor -is [System.Management.Automation.Language.FunctionDefinitionAst]) {
            return $null
        }
        if ($cursor -is [System.Management.Automation.Language.ScriptBlockAst]) {
            $owner = Get-AbNearestFunctionDefinitionAst $cursor
            if ([object]::ReferenceEquals($owner, $Definition)) { return $cursor }
            return $null
        }
        $cursor = $cursor.Parent
    }
    return $null
}

function Test-AbLexicalScopeIsAncestor {
    param(
        [Parameter(Mandatory = $true)]$AncestorScope,
        [Parameter(Mandatory = $true)]$Scope,
        [Parameter(Mandatory = $true)]$Definition
    )
    $cursor = $Scope
    while ($null -ne $cursor) {
        if ([object]::ReferenceEquals($cursor, $AncestorScope)) { return $true }
        $cursor = Get-AbOuterLexicalScriptBlockAst -Scope $cursor -Definition $Definition
    }
    return $false
}

function Test-AbStatementPreventsFallthrough {
    param([Parameter(Mandatory = $true)]$Statement)
    return $Statement -is [System.Management.Automation.Language.ThrowStatementAst] -or
        $Statement -is [System.Management.Automation.Language.ReturnStatementAst] -or
        $Statement -is [System.Management.Automation.Language.ContinueStatementAst] -or
        $Statement -is [System.Management.Automation.Language.BreakStatementAst] -or
        $Statement -is [System.Management.Automation.Language.ExitStatementAst]
}

function Test-AbVariableBindingDominatesUse {
    param(
        [Parameter(Mandatory = $true)]$Binding,
        [Parameter(Mandatory = $true)]$Use,
        [Parameter(Mandatory = $true)]$UseScope,
        [Parameter(Mandatory = $true)]$Definition
    )
    if (-not (Test-AbLexicalScopeIsAncestor -AncestorScope $Binding.scope -Scope $UseScope `
            -Definition $Definition)) {
        return $false
    }
    if ($Binding.kind -ceq 'parameter') { return $true }

    $target = $Use
    $targetScope = $UseScope
    while (-not [object]::ReferenceEquals($targetScope, $Binding.scope)) {
        $target = $targetScope.Parent
        $targetScope = Get-AbOuterLexicalScriptBlockAst -Scope $targetScope -Definition $Definition
        if ($null -eq $targetScope) { return $false }
    }

    if ($Binding.kind -ceq 'foreach') {
        return Test-AbAstIsDescendantOf -Node $target -Ancestor $Binding.ast.Body
    }
    if ($Binding.kind -cne 'assignment') { return $false }

    $forAncestor = $Binding.ast.Parent
    while ($null -ne $forAncestor -and
        $forAncestor -isnot [System.Management.Automation.Language.ForStatementAst] -and
        -not [object]::ReferenceEquals($forAncestor, $Binding.scope)) {
        $forAncestor = $forAncestor.Parent
    }
    if ($forAncestor -is [System.Management.Automation.Language.ForStatementAst] -and
        $null -ne $forAncestor.Initializer -and
        (Test-AbAstIsDescendantOf -Node $Binding.ast -Ancestor $forAncestor.Initializer) -and
        (Test-AbAstIsDescendantOf -Node $target -Ancestor $forAncestor) -and
        $Binding.ast.Extent.EndOffset -le $target.Extent.StartOffset) {
        return $true
    }
    $tryAncestor = $Binding.ast.Parent
    while ($null -ne $tryAncestor -and
        $tryAncestor -isnot [System.Management.Automation.Language.TryStatementAst] -and
        -not [object]::ReferenceEquals($tryAncestor, $Binding.scope)) {
        $tryAncestor = $tryAncestor.Parent
    }
    if ($tryAncestor -is [System.Management.Automation.Language.TryStatementAst] -and
        [object]::ReferenceEquals($Binding.ast.Parent, $tryAncestor.Body) -and
        (Test-AbStatementDominatesAst -Statement $tryAncestor -Target $target -Scope $Binding.scope)) {
        $catchCanFallThrough = $false
        foreach ($catchClause in @($tryAncestor.CatchClauses)) {
            $catchStatements = @($catchClause.Body.Statements)
            if ($catchStatements.Count -eq 0 -or
                -not (Test-AbStatementPreventsFallthrough $catchStatements[-1])) {
                $catchCanFallThrough = $true
                break
            }
        }
        if (-not $catchCanFallThrough) { return $true }
    }
    return Test-AbStatementDominatesAst -Statement $Binding.ast -Target $target -Scope $Binding.scope
}

function Get-AbFunctionVariableContract {
    param(
        [Parameter(Mandatory = $true)]$Definition,
        [Parameter(Mandatory = $true)]$AutomaticVariables
    )
    $bindings = [System.Collections.Generic.Dictionary[
        string,
        System.Collections.Generic.List[object]
    ]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $declared = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )

    $scopes = @($Definition.FindAll({
        param($node)
        if ($node -isnot [System.Management.Automation.Language.ScriptBlockAst]) { return $false }
        $owner = Get-AbNearestFunctionDefinitionAst $node
        return [object]::ReferenceEquals($owner, $Definition)
    }, $true))
    foreach ($scope in $scopes) {
        if ($null -eq $scope.ParamBlock) { continue }
        foreach ($parameter in @($scope.ParamBlock.Parameters)) {
            $name = $parameter.Name.VariablePath.UserPath
            [void]$declared.Add($name)
            if (-not $bindings.ContainsKey($name)) {
                $bindings[$name] = [System.Collections.Generic.List[object]]::new()
            }
            $bindings[$name].Add([pscustomobject]@{
                name = $name
                kind = 'parameter'
                ast = $parameter
                scope = $scope
            })
        }
    }

    foreach ($forEachStatement in @($Definition.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.ForEachStatementAst]
    }, $true))) {
        $owner = Get-AbNearestFunctionDefinitionAst $forEachStatement
        if (-not [object]::ReferenceEquals($owner, $Definition)) { continue }
        $name = $forEachStatement.Variable.VariablePath.UserPath
        if ($name.Contains(':')) { continue }
        $scope = Get-AbNearestScriptBlockAst $forEachStatement
        [void]$declared.Add($name)
        if (-not $bindings.ContainsKey($name)) {
            $bindings[$name] = [System.Collections.Generic.List[object]]::new()
        }
        $bindings[$name].Add([pscustomobject]@{
            name = $name
            kind = 'foreach'
            ast = $forEachStatement
            scope = $scope
        })
    }

    foreach ($assignment in @($Definition.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.AssignmentStatementAst]
    }, $true))) {
        $owner = Get-AbNearestFunctionDefinitionAst $assignment
        if (-not [object]::ReferenceEquals($owner, $Definition) -or
            $assignment.Left -isnot [System.Management.Automation.Language.VariableExpressionAst] -or
            $assignment.Operator -ne [System.Management.Automation.Language.TokenKind]::Equals) {
            continue
        }
        $name = $assignment.Left.VariablePath.UserPath
        if ($name.Contains(':')) { continue }
        $scope = Get-AbNearestScriptBlockAst $assignment
        [void]$declared.Add($name)
        if (-not $bindings.ContainsKey($name)) {
            $bindings[$name] = [System.Collections.Generic.List[object]]::new()
        }
        $bindings[$name].Add([pscustomobject]@{
            name = $name
            kind = 'assignment'
            ast = $assignment
            scope = $scope
        })
    }

    $freeVariables = [System.Collections.Generic.List[object]]::new()
    foreach ($variable in @($Definition.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.VariableExpressionAst]
    }, $true))) {
        $owner = Get-AbNearestFunctionDefinitionAst $variable
        if (-not [object]::ReferenceEquals($owner, $Definition)) { continue }
        $name = $variable.VariablePath.UserPath
        if ($name.Contains(':') -or $AutomaticVariables.Contains($name)) { continue }
        if ($variable.Parent -is [System.Management.Automation.Language.ParameterAst] -and
            [object]::ReferenceEquals($variable.Parent.Name, $variable)) {
            continue
        }
        if ($variable.Parent -is [System.Management.Automation.Language.ForEachStatementAst] -and
            [object]::ReferenceEquals($variable.Parent.Variable, $variable)) {
            continue
        }
        if ($variable.Parent -is [System.Management.Automation.Language.AssignmentStatementAst] -and
            [object]::ReferenceEquals($variable.Parent.Left, $variable) -and
            $variable.Parent.Operator -eq [System.Management.Automation.Language.TokenKind]::Equals) {
            continue
        }

        $scope = Get-AbNearestScriptBlockAst $variable
        $accessibleBindings = @(
            if ($bindings.ContainsKey($name)) {
                $bindings[$name] | Where-Object {
                    Test-AbLexicalScopeIsAncestor -AncestorScope $_.scope -Scope $scope -Definition $Definition
                }
            }
        )
        $isBound = $false
        foreach ($binding in $accessibleBindings) {
            if (Test-AbVariableBindingDominatesUse -Binding $binding -Use $variable `
                    -UseScope $scope -Definition $Definition) {
                $isBound = $true
                break
            }
        }
        if ($isBound) { continue }
        $freeVariables.Add([pscustomobject][ordered]@{
            function = $Definition.Name
            variable = $name
            line = $variable.Extent.StartLineNumber
            reason = if ($accessibleBindings.Count -eq 0) {
                'undeclared-in-lexical-scope'
            } else {
                'not-dominated-by-local-binding'
            }
        })
    }
    return [pscustomobject][ordered]@{
        declared_names = @($declared | Sort-Object)
        free_variables = $freeVariables.ToArray()
    }
}

function Import-StressHarnessFunctions {
    param([Parameter(Mandatory = $true)][string]$Path)
    $hashBeforeParse = Get-AbSha256 $Path
    if ($hashBeforeParse -cne $ExpectedStressHarnessSha256) {
        throw "stress harness changed before dependency import: expected $ExpectedStressHarnessSha256, found $hashBeforeParse"
    }
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile(
        $Path,
        [ref]$tokens,
        [ref]$parseErrors
    )
    if ($parseErrors.Count -ne 0) {
        throw "stress harness AST is invalid: $($parseErrors.Message -join '; ')"
    }
    $hashAfterParse = Get-AbSha256 $Path
    if ($hashAfterParse -cne $ExpectedStressHarnessSha256 -or $hashAfterParse -cne $hashBeforeParse) {
        throw "stress harness changed during dependency import: before $hashBeforeParse, after $hashAfterParse"
    }

    $definitions = @{}
    foreach ($definition in @($ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
    }, $true))) {
        if ($definitions.ContainsKey($definition.Name)) {
            throw "stress harness has duplicate function definition: $($definition.Name)"
        }
        $definitions[$definition.Name] = $definition
    }

    $closure = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $queue = [System.Collections.Generic.Queue[string]]::new()
    foreach ($root in $StressFunctionRoots) { $queue.Enqueue($root) }
    while ($queue.Count -ne 0) {
        $name = $queue.Dequeue()
        if ($StressFunctionOverrides -contains $name) { continue }
        if (-not $closure.Add($name)) { continue }
        if (-not $definitions.ContainsKey($name)) {
            throw "stress dependency closure is missing required function: $name"
        }
        foreach ($command in @($definitions[$name].FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.CommandAst]
        }, $true))) {
            $dependency = Get-AbCommandLeafName ($command.GetCommandName())
            if (-not [string]::IsNullOrWhiteSpace($dependency) -and
                $definitions.ContainsKey($dependency) -and
                $StressFunctionOverrides -notcontains $dependency -and
                -not $closure.Contains($dependency)) {
                $queue.Enqueue($dependency)
            }
        }
    }

    $closureAst = @($closure | ForEach-Object { $definitions[$_] })
    $violations = [System.Collections.Generic.List[string]]::new()
    foreach ($definition in $closureAst) {
        foreach ($violation in @(Get-AbAstContractViolations -Ast $definition -Label "stress::$($definition.Name)")) {
            $violations.Add($violation)
        }
    }
    $waitFunction = $definitions['Wait-HarnessProcess']
    if ($waitFunction.Extent.Text -cnotmatch "measurement_method\s*=\s*'os-process-lifetime'") {
        $violations.Add('stress::Wait-HarnessProcess is not OS-process-lifetime based')
    }

    $cimReachableFunctions = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($definition in @($definitions.Values)) {
        $hasDirectCim = @($definition.FindAll({
            param($node)
            if ($node -isnot [System.Management.Automation.Language.CommandAst]) { return $false }
            $leaf = Get-AbCommandLeafName ($node.GetCommandName())
            return $leaf -ieq 'Get-CimInstance' -or $leaf -ieq 'gcim'
        }, $true)).Count -ne 0
        if ($hasDirectCim) { [void]$cimReachableFunctions.Add($definition.Name) }
    }
    $reachabilityChanged = $true
    while ($reachabilityChanged) {
        $reachabilityChanged = $false
        foreach ($definition in @($definitions.Values)) {
            if ($cimReachableFunctions.Contains($definition.Name)) { continue }
            $reachesCim = @($definition.FindAll({
                param($node)
                if ($node -isnot [System.Management.Automation.Language.CommandAst]) { return $false }
                $leaf = Get-AbCommandLeafName ($node.GetCommandName())
                return -not [string]::IsNullOrWhiteSpace($leaf) -and
                    $StressFunctionOverrides -notcontains $leaf -and
                    $cimReachableFunctions.Contains($leaf)
            }, $true)).Count -ne 0
            if ($reachesCim) {
                [void]$cimReachableFunctions.Add($definition.Name)
                $reachabilityChanged = $true
            }
        }
    }

    $waitLoops = @($waitFunction.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.LoopStatementAst]
    }, $true))
    $waitSensitiveCommands = [System.Collections.Generic.List[object]]::new()
    foreach ($command in @($waitFunction.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.CommandAst]
    }, $true))) {
        $leaf = Get-AbCommandLeafName ($command.GetCommandName())
        $directCim = $leaf -ieq 'Get-CimInstance' -or $leaf -ieq 'gcim'
        $helperCim = -not [string]::IsNullOrWhiteSpace($leaf) -and
            $StressFunctionOverrides -notcontains $leaf -and
            $cimReachableFunctions.Contains($leaf)
        if (-not $directCim -and -not $helperCim) { continue }

        $loopAncestor = $command.Parent
        while ($null -ne $loopAncestor -and
            $loopAncestor -isnot [System.Management.Automation.Language.LoopStatementAst] -and
            $loopAncestor -ne $waitFunction) {
            $loopAncestor = $loopAncestor.Parent
        }
        $afterStop = Test-AbCommandRunsAfterWaitStop -Command $command -WaitFunction $waitFunction
        $waitSensitiveCommands.Add([pscustomobject][ordered]@{
            command = $leaf
            line = $command.Extent.StartLineNumber
            direct_cim = $directCim
            helper_reaches_cim = $helperCim
            inside_loop = $loopAncestor -is [System.Management.Automation.Language.LoopStatementAst]
            loop_kind = if ($loopAncestor -is [System.Management.Automation.Language.LoopStatementAst]) {
                $loopAncestor.GetType().Name
            } else {
                $null
            }
            measurement_stop_dominates = $afterStop
        })
        if (-not $afterStop) {
            $violations.Add("stress::Wait-HarnessProcess can reach CIM before OS-lifetime timing stops through '$leaf' at line $($command.Extent.StartLineNumber)")
        }
    }

    $scriptVariables = @($closureAst | ForEach-Object {
        $_.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.VariableExpressionAst] -and
                $node.VariablePath.UserPath -like 'script:*'
        }, $true) | ForEach-Object { $_.VariablePath.UserPath }
    } | Sort-Object -Unique)
    $unexpectedVariables = @($scriptVariables | Where-Object { $AllowedImportedScriptVariables -notcontains $_ })
    $missingVariables = @($AllowedImportedScriptVariables | Where-Object { $scriptVariables -notcontains $_ })
    if ($unexpectedVariables.Count -ne 0 -or $missingVariables.Count -ne 0) {
        $violations.Add("stress dependency script-variable contract changed; unexpected=[$($unexpectedVariables -join ', ')], missing=[$($missingVariables -join ', ')]")
    }

    $scopedVariables = @($closureAst | ForEach-Object {
        $_.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.VariableExpressionAst] -and
                $node.VariablePath.UserPath.Contains(':')
        }, $true) | ForEach-Object { $_.VariablePath.UserPath }
    } | Sort-Object -Unique)
    $unexpectedScopedVariables = @($scopedVariables | Where-Object {
        $AllowedImportedScriptVariables -notcontains $_
    })
    if ($unexpectedScopedVariables.Count -ne 0) {
        $violations.Add("stress dependency uses forbidden scoped variables: $($unexpectedScopedVariables -join ', ')")
    }

    $automaticVariables = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($name in @('null', 'true', 'false', 'PID', 'PSItem', '_')) {
        [void]$automaticVariables.Add($name)
    }
    $declaredVariableNames = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    $unqualifiedFreeVariables = [System.Collections.Generic.List[object]]::new()
    foreach ($definition in $closureAst) {
        $variableContract = Get-AbFunctionVariableContract -Definition $definition `
            -AutomaticVariables $automaticVariables
        foreach ($declaredName in $variableContract.declared_names) {
            [void]$declaredVariableNames.Add($declaredName)
        }
        foreach ($freeVariable in $variableContract.free_variables) {
            $unqualifiedFreeVariables.Add($freeVariable)
        }
    }
    if ($unqualifiedFreeVariables.Count -ne 0) {
        $freeLabels = @($unqualifiedFreeVariables | ForEach-Object {
            "$($_.function)::$($_.variable)@L$($_.line)"
        } | Sort-Object -Unique)
        $violations.Add("stress dependency has unqualified free variables: $($freeLabels -join ', ')")
    }

    if ($violations.Count -ne 0) {
        throw "stress dependency closure violates the timing/cleanup/global contract: $($violations -join '; ')"
    }

    $functionHashes = [System.Collections.Generic.List[object]]::new()
    foreach ($name in @($closure | Sort-Object)) {
        $bodyBytes = [System.Text.Encoding]::UTF8.GetBytes($definitions[$name].Body.Extent.Text)
        $bodyHash = [Convert]::ToHexString(
            [System.Security.Cryptography.SHA256]::HashData($bodyBytes)
        ).ToLowerInvariant()
        $functionHashes.Add([pscustomobject][ordered]@{
            name = $name
            body_sha256 = $bodyHash
        })
        Set-Item -Path "Function:\script:$name" -Value $definitions[$name].Body.GetScriptBlock() -Force
    }
    return [pscustomobject][ordered]@{
        stress_harness_sha256_before_parse = $hashBeforeParse
        stress_harness_sha256_after_parse = $hashAfterParse
        function_count = $closure.Count
        function_names = @($closure | Sort-Object)
        function_body_hashes = $functionHashes.ToArray()
        override_names = @($StressFunctionOverrides | Sort-Object)
        script_variables = $scriptVariables
        scoped_variables = $scopedVariables
        declared_unqualified_variables = @($declaredVariableNames | Sort-Object)
        unqualified_free_variables = @()
        wait_loop_count = $waitLoops.Count
        wait_sensitive_commands = $waitSensitiveCommands.ToArray()
        wait_active_timing_cim_command_count = @($waitSensitiveCommands | Where-Object {
            -not $_.measurement_stop_dominates
        }).Count
        main_executed = $false
        ast_contract_violations = @()
    }
}

function Assert-AbStaticContract {
    param([Parameter(Mandatory = $true)][string]$DiagnosticPath)
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile(
        $DiagnosticPath,
        [ref]$tokens,
        [ref]$parseErrors
    )
    if ($parseErrors.Count -ne 0) {
        throw "diagnostic AST is invalid: $($parseErrors.Message -join '; ')"
    }
    $violations = @(Get-AbAstContractViolations -Ast $ast -Label 'diagnostic')
    $cimCommands = @($ast.FindAll({
        param($node)
        if ($node -isnot [System.Management.Automation.Language.CommandAst]) { return $false }
        $leaf = Get-AbCommandLeafName ($node.GetCommandName())
        return $leaf -ieq 'Get-CimInstance' -or $leaf -ieq 'gcim'
    }, $true))
    $cimFunctionNames = @($cimCommands | ForEach-Object {
        $ancestor = $_.Parent
        while ($null -ne $ancestor -and
            $ancestor -isnot [System.Management.Automation.Language.FunctionDefinitionAst]) {
            $ancestor = $ancestor.Parent
        }
        if ($null -eq $ancestor) { '<main>' } else { [string]$ancestor.Name }
    } | Sort-Object -Unique)
    $expectedCimFunctions = @('Get-AbExactCandidateProcesses', 'Open-AbDaemonIdentity')
    try {
        Assert-AbEquivalentJson $expectedCimFunctions $cimFunctionNames `
            'diagnostic bounded-CIM function ownership'
    } catch {
        $violations += $_.Exception.Message
    }
    foreach ($command in $cimCommands) {
        $ancestor = $command.Parent
        while ($null -ne $ancestor) {
            if ($ancestor -is [System.Management.Automation.Language.LoopStatementAst]) {
                $violations += "diagnostic CIM command is nested in a loop at line $($command.Extent.StartLineNumber)"
                break
            }
            $ancestor = $ancestor.Parent
        }
    }

    $diagnosticDefinitions = @($ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
    }, $true))
    foreach ($overrideName in $StressFunctionOverrides) {
        $overrideDefinitions = @($diagnosticDefinitions | Where-Object { $_.Name -ieq $overrideName })
        if ($overrideDefinitions.Count -ne 1) {
            $violations += "diagnostic override '$overrideName' has $($overrideDefinitions.Count) definitions; expected exactly one"
        }
    }
    $observationOverrides = @($diagnosticDefinitions | Where-Object {
        $_.Name -ieq 'Update-ProcessObservation'
    })
    $observationOverrideStatementCount = -1
    if ($observationOverrides.Count -eq 1) {
        $observationOverrideStatements = @($observationOverrides[0].Body.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.StatementAst]
        }, $true))
        $observationOverrideStatementCount = $observationOverrideStatements.Count
        if ($observationOverrideStatementCount -ne 0) {
            $violations += 'diagnostic Update-ProcessObservation override is not an AST-empty no-op'
        }
    }
    if ($violations.Count -ne 0) {
        throw "diagnostic violates the timing/cleanup contract: $($violations -join '; ')"
    }
    return [pscustomobject][ordered]@{
        parse_error_count = 0
        stop_process_command_count = 0
        has_exited_member_count = 0
        parameterless_wait_for_exit_count = 0
        unbounded_cim_command_count = 0
        bounded_cim_command_count = $cimCommands.Count
        bounded_cim_function_names = $cimFunctionNames
        cim_nested_in_loop_count = 0
        observation_override_statement_count = $observationOverrideStatementCount
        imported_override_definition_count = $StressFunctionOverrides.Count
    }
}

function Initialize-AbHarnessProcessIdentity {
    $current = [System.Diagnostics.Process]::GetCurrentProcess()
    try {
        $creation = ConvertTo-AbNormalizedProcessCreationUtc $current.StartTime.ToUniversalTime()
        $path = ConvertTo-NormalizedExecutablePath $current.MainModule.FileName
        $key = New-ProcessIdentityKey -ProcessId $PID -CreationTimeUtc $creation
        $script:HarnessProcessIdentity = [pscustomobject][ordered]@{
            identity_key = $key
            process_id = $PID
            parent_process_id = 0
            parent_identity_key = $null
            parent_chain = @($key)
            creation_time_utc = $creation
            exit_time_utc = $null
            executable_path = $path
            name = [System.IO.Path]::GetFileName($path)
            source = 'marker-ab-harness-root'
            label = 'marker-attribution-ab-diagnostic'
            depth = 0
        }
    } finally {
        $current.Dispose()
    }
}

function Initialize-AbNativeProcessApi {
    if ($null -ne ('ColayMarkerAbNativeProcessApi' -as [type])) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class ColayMarkerAbNativeProcessApi
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

function Get-AbExactCandidateProcesses {
    param([Parameter(Mandatory = $true)][string[]]$ExecutablePaths)
    $expected = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($path in $ExecutablePaths) {
        [void]$expected.Add((ConvertTo-AbComparableWindowsPath $path))
    }
    return @(Get-CimInstance -ClassName Win32_Process `
        -Property ProcessId, ParentProcessId, CreationDate, Name, ExecutablePath `
        -OperationTimeoutSec $CimOperationTimeoutSec -ErrorAction Stop | Where-Object {
        if ([string]::IsNullOrWhiteSpace([string]$_.ExecutablePath)) { return $false }
        try {
            return $expected.Contains((ConvertTo-AbComparableWindowsPath ([string]$_.ExecutablePath)))
        } catch {
            return $false
        }
    } | ForEach-Object {
        $creation = ConvertTo-AbNormalizedProcessCreationUtc $_.CreationDate
        [pscustomobject][ordered]@{
            process_id = [int]$_.ProcessId
            parent_process_id = [int]$_.ParentProcessId
            creation_time_utc = $creation.ToString('o')
            name = [string]$_.Name
            executable_path = ConvertTo-AbComparableWindowsPath ([string]$_.ExecutablePath)
        }
    })
}

function ConvertTo-AbDaemonDocumentIdentity {
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
    if ($null -eq $instance) {
        throw "$ExpectedCommand JSON has no exact instance identity"
    }
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
    $jsonPath = ConvertTo-AbComparableWindowsPath $executablePathText
    $expectedPath = ConvertTo-AbComparableWindowsPath $ExpectedExecutable
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

function Assert-AbDaemonReadinessDeadline {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.Stopwatch]$Stopwatch,
        [Parameter(Mandatory = $true)]
        [ValidateRange(1, [int]::MaxValue)]
        [int]$OverallTimeoutMs
    )
    if ([int64]$Stopwatch.ElapsedMilliseconds -ge $OverallTimeoutMs) {
        throw "daemon readiness timed out after ${OverallTimeoutMs}ms"
    }
}

function Wait-AbDaemonReadiness {
    param(
        [Parameter(Mandatory = $true)]$DaemonStartDocument,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable,
        [Parameter(Mandatory = $true)][string]$Repository,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $evidenceKey = 'ColayMarkerAbDaemonReadinessEvidence'
    $polls = [System.Collections.Generic.List[object]]::new()
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $anchor = $null
    $evidence = [pscustomobject][ordered]@{
        readiness_status = 'failed'
        original_state = $null
        final_state = $null
        poll_count = 0
        elapsed_ms = 0
        overall_timeout_ms = $DaemonReadinessTimeoutMs
        poll_interval_ms = $DaemonReadinessPollIntervalMs
        cleanup_reserve_ms = $DaemonReadinessCleanupReserveMs
        status_command = @('--json', 'daemon', 'status')
        anchored_identity = $null
        polls = @()
        online_document = $null
        failure = $null
    }
    try {
        $anchor = ConvertTo-AbDaemonDocumentIdentity -Document $DaemonStartDocument `
            -ExpectedCommand daemon_start -ExpectedExecutable $ExpectedExecutable
        $evidence.original_state = $anchor.State
        $evidence.final_state = $anchor.State
        $evidence.anchored_identity = [pscustomobject][ordered]@{
            instance_id = $anchor.InstanceId
            process_id = [int]$anchor.ProcessId
            executable_path = $anchor.ExecutablePath
        }
        [void](Assert-AbDaemonReadinessDeadline -Stopwatch $stopwatch `
            -OverallTimeoutMs $DaemonReadinessTimeoutMs)
        if (@('booting', 'probing', 'online') -cnotcontains $anchor.State) {
            throw "daemon readiness start returned terminal or non-progress state '$($anchor.State)'"
        }
        if ($anchor.State -ceq 'online') {
            $evidence.readiness_status = 'online'
            $evidence.online_document = $DaemonStartDocument
            $evidence.elapsed_ms = [int64]$stopwatch.ElapsedMilliseconds
            return [pscustomobject][ordered]@{
                Evidence = $evidence
                OnlineDocument = $DaemonStartDocument
            }
        }

        while ($true) {
            $remainingBeforeSleepMs = $DaemonReadinessTimeoutMs - [int64]$stopwatch.ElapsedMilliseconds
            $sleepBudgetMs = $remainingBeforeSleepMs - $DaemonReadinessCleanupReserveMs
            if ($sleepBudgetMs -le 0) {
                throw "daemon readiness timed out after ${DaemonReadinessTimeoutMs}ms"
            }
            $sleepMs = [int][Math]::Min($DaemonReadinessPollIntervalMs, $sleepBudgetMs)
            Start-Sleep -Milliseconds $sleepMs

            $remainingMs = $DaemonReadinessTimeoutMs - [int64]$stopwatch.ElapsedMilliseconds
            $commandBudgetMs = [int]($remainingMs - $DaemonReadinessCleanupReserveMs)
            if ($commandBudgetMs -le 0) {
                throw "daemon readiness timed out after ${DaemonReadinessTimeoutMs}ms"
            }
            $pollNumber = $polls.Count + 1
            $commandLabel = "$Label-daemon-readiness-{0:D3}" -f $pollNumber
            $pollEvidence = [pscustomobject][ordered]@{
                poll = $pollNumber
                command_label = $commandLabel
                command_timeout_ms = $commandBudgetMs
                command_elapsed_ms = $null
                command_exit_code = $null
                command_timed_out = $null
                observed_elapsed_ms = [int64]$stopwatch.ElapsedMilliseconds
                state = $null
                phase = $null
                instance_id = $null
                process_id = $null
                executable_path = $null
            }
            $polls.Add($pollEvidence)
            $evidence.poll_count = $polls.Count
            $evidence.polls = $polls.ToArray()
            $statusResult = Invoke-Colay -Repository $Repository `
                -ArgumentValues @('--json', 'daemon', 'status') `
                -Environment $Environment -Label $commandLabel `
                -TimeoutMs $commandBudgetMs
            $pollEvidence.command_elapsed_ms = [int64]$statusResult.elapsed_ms
            $pollEvidence.command_exit_code = [int]$statusResult.exit_code
            $pollEvidence.command_timed_out = [bool]$statusResult.timed_out
            $pollEvidence.observed_elapsed_ms = [int64]$stopwatch.ElapsedMilliseconds
            [void](Assert-AbDaemonReadinessDeadline -Stopwatch $stopwatch `
                -OverallTimeoutMs $DaemonReadinessTimeoutMs)
            if ([bool]$statusResult.timed_out -or [int]$statusResult.exit_code -ne 0) {
                throw "daemon readiness status poll $pollNumber did not exit successfully"
            }
            $statusDocument = Assert-StatusJson $statusResult
            $statusIdentity = ConvertTo-AbDaemonDocumentIdentity -Document $statusDocument `
                -ExpectedCommand daemon_status -ExpectedExecutable $ExpectedExecutable
            $pollEvidence.state = $statusIdentity.State
            $pollEvidence.phase = $statusIdentity.Phase
            $pollEvidence.instance_id = $statusIdentity.InstanceId
            $pollEvidence.process_id = [int]$statusIdentity.ProcessId
            $pollEvidence.executable_path = $statusIdentity.ExecutablePath
            if ($statusIdentity.InstanceId -cne $anchor.InstanceId -or
                $statusIdentity.ProcessId -ne $anchor.ProcessId -or
                -not $statusIdentity.ExecutablePath.Equals(
                    $anchor.ExecutablePath,
                    [System.StringComparison]::OrdinalIgnoreCase
                )) {
                throw "daemon readiness identity drift at status poll $pollNumber"
            }
            [void](Assert-AbDaemonReadinessDeadline -Stopwatch $stopwatch `
                -OverallTimeoutMs $DaemonReadinessTimeoutMs)

            $evidence.final_state = $statusIdentity.State
            if ($statusIdentity.State -ceq 'online') {
                $evidence.readiness_status = 'online'
                $evidence.online_document = $statusDocument
                $evidence.elapsed_ms = [int64]$stopwatch.ElapsedMilliseconds
                return [pscustomobject][ordered]@{
                    Evidence = $evidence
                    OnlineDocument = $statusDocument
                }
            }
            if (@('booting', 'probing') -cnotcontains $statusIdentity.State) {
                throw "daemon readiness status poll $pollNumber returned terminal or non-progress state '$($statusIdentity.State)'"
            }
        }
    } catch {
        $evidence.poll_count = $polls.Count
        $evidence.polls = $polls.ToArray()
        $evidence.elapsed_ms = [int64]$stopwatch.ElapsedMilliseconds
        $evidence.failure = $_.Exception.Message
        $_.Exception.Data[$evidenceKey] = $evidence
        throw
    } finally {
        $stopwatch.Stop()
    }
}

function Open-AbDaemonIdentity {
    param(
        [Parameter(Mandatory = $true)]$DaemonDocument,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable
    )
    if ($null -eq $DaemonDocument -or
        $DaemonDocument.PSObject.Properties.Name -cnotcontains 'command' -or
        $DaemonDocument.command -isnot [string]) {
        throw 'retained daemon identity requires exact daemon_start or daemon_status JSON'
    }
    $daemonCommand = [string]$DaemonDocument.command
    if ($daemonCommand -cne 'daemon_start' -and $daemonCommand -cne 'daemon_status') {
        throw 'retained daemon identity requires exact daemon_start or daemon_status JSON'
    }
    $documentIdentity = ConvertTo-AbDaemonDocumentIdentity -Document $DaemonDocument `
        -ExpectedCommand $daemonCommand -ExpectedExecutable $ExpectedExecutable
    $daemonState = $documentIdentity.State
    if ($daemonState -cne 'online') {
        throw "retained daemon identity expected exact state 'online', found '$daemonState'"
    }
    $instanceId = $documentIdentity.InstanceId
    $processId = $documentIdentity.ProcessId
    $rawPid = [int64]$processId
    $jsonPath = $documentIdentity.ExecutablePath
    $expectedPath = ConvertTo-AbComparableWindowsPath $ExpectedExecutable

    $rows = @(Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $processId" `
        -Property ProcessId, ParentProcessId, CreationDate, Name, ExecutablePath `
        -OperationTimeoutSec $CimOperationTimeoutSec -ErrorAction Stop)
    if ($rows.Count -ne 1) {
        throw "daemon identity query returned $($rows.Count) rows for exact pid $processId"
    }
    $row = $rows[0]
    if ([int64]$row.ProcessId -ne $rawPid -or
        [string]::IsNullOrWhiteSpace([string]$row.ExecutablePath)) {
        throw "daemon identity query did not return a usable exact identity for pid $processId"
    }
    $cimPath = ConvertTo-AbComparableWindowsPath ([string]$row.ExecutablePath)
    $cimCreation = ConvertTo-AbNormalizedProcessCreationUtc $row.CreationDate
    if (-not $cimPath.Equals($expectedPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "daemon CIM executable path mismatch: expected $expectedPath, found $cimPath"
    }

    Initialize-AbNativeProcessApi
    $processTerminate = [uint32]0x0001
    $processQueryLimitedInformation = [uint32]0x1000
    $synchronize = [uint32]0x00100000
    $handle = [ColayMarkerAbNativeProcessApi]::OpenProcess(
        $processTerminate -bor $processQueryLimitedInformation -bor $synchronize,
        $false,
        $processId
    )
    if ($handle -eq [IntPtr]::Zero) {
        $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "cannot retain daemon process handle for pid ${processId}: Win32 error $errorCode"
    }

    $captureEvidenceKey = 'ColayMarkerAbDaemonCaptureEvidence'
    $captureCleanupErrors = [System.Collections.Generic.List[string]]::new()
    $captureFailureEvidence = [pscustomobject][ordered]@{
        capture_status = 'failed-after-handle-open'
        primary_failure = $null
        refusal_reason = $null
        process_id = [int]$processId
        executable_path = $expectedPath
        direct_mutation_allowed = $false
        handle_opened = $true
        handle_close_attempted = $false
        handle_closed = $false
        close_error = $null
        handle_balance = [pscustomobject][ordered]@{
            opened = 1
            closed = 0
            outstanding = 1
        }
        cleanup_errors = @()
    }
    $identity = $null
    try {
        [int64]$creationTicks = 0
        [int64]$exitTicks = 0
        [int64]$kernelTicks = 0
        [int64]$userTicks = 0
        if (-not [ColayMarkerAbNativeProcessApi]::GetProcessTimes(
            $handle,
            [ref]$creationTicks,
            [ref]$exitTicks,
            [ref]$kernelTicks,
            [ref]$userTicks
        )) {
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            throw "cannot read retained daemon creation time for pid ${processId}: Win32 error $errorCode"
        }
        $nativeCreation = ConvertTo-AbNormalizedProcessCreationUtc ([datetime]::FromFileTimeUtc($creationTicks))
        $pathBuffer = [System.Text.StringBuilder]::new(32768)
        [uint32]$pathLength = $pathBuffer.Capacity
        if (-not [ColayMarkerAbNativeProcessApi]::QueryFullProcessImageName(
            $handle,
            0,
            $pathBuffer,
            [ref]$pathLength
        )) {
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            throw "cannot read retained daemon executable path for pid ${processId}: Win32 error $errorCode"
        }
        $nativePath = ConvertTo-AbComparableWindowsPath $pathBuffer.ToString()
        if ($nativeCreation -ne $cimCreation -or
            -not $nativePath.Equals($expectedPath, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "retained daemon identity mismatch for pid $processId"
        }
        $identity = [pscustomobject][ordered]@{
            Handle = $handle
            Evidence = [pscustomobject][ordered]@{
                capture_status = 'verified-retained-handle'
                daemon_command = $documentIdentity.Command
                instance_id = $instanceId
                daemon_state = $daemonState
                process_id = [int]$processId
                parent_process_id = [int]$row.ParentProcessId
                creation_time_utc = $nativeCreation.ToString('o')
                executable_path = $nativePath
                json_executable_path = $jsonPath
                cim_creation_time_utc = $cimCreation.ToString('o')
                cim_executable_path = $cimPath
                direct_mutation_allowed = $true
                handle_opened = $true
                handle_closed = $false
                close_error = $null
                handle_balance = [pscustomobject][ordered]@{
                    opened = 1
                    closed = 0
                    outstanding = 1
                }
                cleanup_errors = @()
            }
        }
        $handle = [IntPtr]::Zero
        return $identity
    } catch {
        $captureFailureEvidence.primary_failure = $_.Exception.Message
        $captureFailureEvidence.refusal_reason = $_.Exception.Message
        $_.Exception.Data[$captureEvidenceKey] = $captureFailureEvidence
        throw
    } finally {
        if ($handle -ne [IntPtr]::Zero) {
            $captureFailureEvidence.handle_close_attempted = $true
            $handleClosed = $false
            try {
                $handleClosed = [ColayMarkerAbNativeProcessApi]::CloseHandle($handle)
                if (-not $handleClosed) {
                    $closeErrorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                    $captureFailureEvidence.close_error = "Win32 error $closeErrorCode"
                }
            } catch {
                $captureFailureEvidence.close_error = "exception: $($_.Exception.Message)"
            }
            if ($handleClosed) {
                $captureFailureEvidence.handle_closed = $true
                $captureFailureEvidence.handle_balance.closed = 1
                $captureFailureEvidence.handle_balance.outstanding = 0
            } else {
                $captureCleanupErrors.Add(
                    "retained daemon capture handle close failed: $($captureFailureEvidence.close_error)"
                )
            }
            $captureFailureEvidence.cleanup_errors = $captureCleanupErrors.ToArray()
            $handle = [IntPtr]::Zero
        }
    }
}

function Complete-AbDaemonIdentity {
    param([Parameter(Mandatory = $true)]$Identity)
    $waitObject0 = [uint32]0
    $waitTimeout = [uint32]258
    $waitFailed = [uint32]::MaxValue
    $errors = [System.Collections.Generic.List[string]]::new()
    $cleanup = [pscustomobject][ordered]@{
        retained_handle_identity = $Identity.Evidence
        initial_wait_result = $null
        fallback_terminate_attempted = $false
        fallback_terminate_succeeded = $false
        fallback_exit_race = $false
        final_wait_result = $null
        handle_closed = $false
        handle_balance = $Identity.Evidence.handle_balance
        errors = @()
    }
    try {
        $initial = [ColayMarkerAbNativeProcessApi]::WaitForSingleObject($Identity.Handle, 0)
        $cleanup.initial_wait_result = [uint64]$initial
        if ($initial -eq $waitTimeout) {
            $cleanup.fallback_terminate_attempted = $true
            if ([ColayMarkerAbNativeProcessApi]::TerminateProcess($Identity.Handle, 86)) {
                $cleanup.fallback_terminate_succeeded = $true
            } else {
                $terminateError = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $raceWait = [ColayMarkerAbNativeProcessApi]::WaitForSingleObject($Identity.Handle, 0)
                if ($raceWait -eq $waitObject0) {
                    $cleanup.fallback_exit_race = $true
                } else {
                    $errors.Add("retained-handle TerminateProcess failed: Win32 error $terminateError")
                }
            }
            $final = [ColayMarkerAbNativeProcessApi]::WaitForSingleObject($Identity.Handle, 5000)
            $cleanup.final_wait_result = [uint64]$final
            if ($final -eq $waitFailed) {
                $waitError = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $errors.Add("retained-handle final wait failed: Win32 error $waitError")
            } elseif ($final -ne $waitObject0) {
                $errors.Add("retained-handle final wait returned $final instead of signaled")
            }
        } elseif ($initial -eq $waitFailed) {
            $waitError = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            $errors.Add("retained-handle initial wait failed: Win32 error $waitError")
        } elseif ($initial -ne $waitObject0) {
            $errors.Add("retained-handle initial wait returned unexpected value $initial")
        }
    } finally {
        if ([ColayMarkerAbNativeProcessApi]::CloseHandle($Identity.Handle)) {
            $cleanup.handle_closed = $true
            $Identity.Evidence.handle_closed = $true
            $Identity.Evidence.handle_balance.closed = 1
            $Identity.Evidence.handle_balance.outstanding = 0
        } else {
            $closeError = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            $Identity.Evidence.close_error = "Win32 error $closeError"
            $errors.Add("retained daemon handle close failed: Win32 error $closeError")
        }
        $Identity.Handle = [IntPtr]::Zero
    }
    $cleanup.errors = $errors.ToArray()
    return $cleanup
}

function New-AbEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$Root,
        [Parameter(Mandatory = $true)][string]$ColayHome,
        [Parameter(Mandatory = $true)][string]$FakeProvider,
        [Parameter(Mandatory = $true)][ValidateSet('aggregate_only', 'attributed')][string]$Variant
    )
    $userHome = Join-Path $Root 'user-home'
    $temp = Join-Path $Root 'temp'
    $appData = Join-Path $userHome 'AppData/Roaming'
    $localAppData = Join-Path $userHome 'AppData/Local'
    $aggregateMarker = Join-Path $temp 'legacy-inspections.log'
    $markerDirectoryA = Join-Path $temp 'marker-groups-a'
    $markerDirectoryB = Join-Path $temp 'marker-groups-b'
    foreach ($directory in @(
        $Root,
        $ColayHome,
        $userHome,
        $temp,
        $appData,
        $localAppData,
        $markerDirectoryA,
        $markerDirectoryB
    )) {
        [void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetFullPath($directory))
    }

    $environment = [ordered]@{
        'COLAY_HOME' = $ColayHome
        'COLAY_TEST_FAKE_PROVIDERS_ONLY' = '1'
        'COLAY_TEST_LEGACY_INSPECT_MARKER' = $aggregateMarker
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
    if ($Variant -ceq 'attributed') {
        $environment['COLAY_TEST_LEGACY_INSPECT_MARKER_DIR'] = $markerDirectoryB
    } else {
        # _PAD has exactly the same length as _DIR and is intentionally ignored by colay.
        $environment['COLAY_TEST_LEGACY_INSPECT_MARKER_PAD'] = $markerDirectoryA
    }
    foreach ($key in $ProviderKeyNames) { [void]$environment.Remove($key) }
    return [pscustomobject][ordered]@{
        values = $environment
        aggregate_marker = $aggregateMarker
        active_attributed_marker = $markerDirectoryB
        padding_marker = $markerDirectoryA
    }
}

function Assert-AbEnvironment {
    param(
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][ValidateSet('aggregate_only', 'attributed')][string]$Variant,
        [Parameter(Mandatory = $true)][string]$FakeProvider,
        [Parameter(Mandatory = $true)][string]$RuntimeRoot
    )
    foreach ($key in $ProviderKeyNames) {
        if ($Environment.Contains($key)) {
            throw "isolated diagnostic environment contains credential key: $key"
        }
    }
    if ([string]$Environment['OS'] -cne 'Windows_NT' -or
        [string]$Environment['COLAY_TEST_FAKE_PROVIDERS_ONLY'] -cne '1') {
        throw 'isolated diagnostic environment is not exact Windows_NT fake-only mode'
    }
    $expectedPath = ConvertTo-AbComparableWindowsPath (Split-Path -Parent $FakeProvider)
    $actualPath = ConvertTo-AbComparableWindowsPath ([string]$Environment['PATH'])
    if (-not $actualPath.Equals($expectedPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "isolated diagnostic PATH is not the exact fake-provider directory: $actualPath"
    }
    $variantKey = if ($Variant -ceq 'attributed') {
        'COLAY_TEST_LEGACY_INSPECT_MARKER_DIR'
    } else {
        'COLAY_TEST_LEGACY_INSPECT_MARKER_PAD'
    }
    $oppositeKey = if ($Variant -ceq 'attributed') {
        'COLAY_TEST_LEGACY_INSPECT_MARKER_PAD'
    } else {
        'COLAY_TEST_LEGACY_INSPECT_MARKER_DIR'
    }
    if (-not $Environment.Contains($variantKey) -or $Environment.Contains($oppositeKey)) {
        throw "$Variant environment does not contain its one exact marker selector"
    }
    $expectedNames = @(
        'APPDATA',
        'COLAY_HOME',
        'COLAY_TEST_DAEMON_CHILD_RESOLUTION',
        'COLAY_TEST_DAEMON_STDERR',
        'COLAY_TEST_FAKE_PROVIDERS_ONLY',
        'COLAY_TEST_LEGACY_INSPECT_MARKER',
        'HOME',
        'LOCALAPPDATA',
        'OS',
        'PATH',
        'PATHEXT',
        'RUST_BACKTRACE',
        'SystemRoot',
        'TEMP',
        'TMP',
        'USERPROFILE',
        'WINDIR',
        $variantKey
    ) | Sort-Object
    $actualNames = @($Environment.Keys | ForEach-Object { [string]$_ } | Sort-Object)
    Assert-AbEquivalentJson $expectedNames $actualNames "$Variant isolated environment names"
    $serializedCharacters = 0
    $neutralEntries = [System.Collections.Generic.List[object]]::new()
    foreach ($entry in $Environment.GetEnumerator()) {
        $serializedCharacters += ([string]$entry.Key).Length + 1 + ([string]$entry.Value).Length + 1
        $neutralKey = if ([string]$entry.Key -ceq $variantKey) {
            'COLAY_TEST_LEGACY_INSPECT_MARKER_SELECTOR'
        } else {
            [string]$entry.Key
        }
        $neutralValue = if ([string]$entry.Key -ceq $variantKey) {
            '<MARKER_DIRECTORY>'
        } else {
            ([string]$entry.Value).Replace(
                $RuntimeRoot,
                '<RUNTIME_ROOT>',
                [System.StringComparison]::OrdinalIgnoreCase
            )
        }
        $neutralEntries.Add([pscustomobject][ordered]@{
            name = $neutralKey
            value = $neutralValue
        })
    }
    return [pscustomobject][ordered]@{
        variable_count = $Environment.Count
        serialized_character_count = $serializedCharacters
        credential_key_count = 0
        os = [string]$Environment['OS']
        fake_provider_only = [string]$Environment['COLAY_TEST_FAKE_PROVIDERS_ONLY']
        path = [string]$Environment['PATH']
        semantic_selector_key = $variantKey
        selector_key_length = $variantKey.Length
        selector_value_length = ([string]$Environment[$variantKey]).Length
        neutral_entries = @($neutralEntries | Sort-Object name)
    }
}

function Assert-AbFakeProviderConfig {
    param(
        [Parameter(Mandatory = $true)][string]$ColayHomePath,
        [Parameter(Mandatory = $true)][string]$FakeProvider
    )
    $path = Join-Path $ColayHomePath 'config.toml'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "fake-only provider config is missing: $path"
    }
    $escaped = ConvertTo-AbTomlString $FakeProvider
    $expectedLines = @(
        'config_version = 4',
        '[orchestrator.providers.codex]',
        "executable = $escaped",
        '[orchestrator.providers.claude]',
        "executable = $escaped",
        '[orchestrator.providers.gemini]',
        "executable = $escaped",
        '[orchestrator.providers.agy]',
        "executable = $escaped"
    )
    $actualLines = @(Get-Content -LiteralPath $path -ErrorAction Stop)
    Assert-AbEquivalentJson $expectedLines $actualLines 'exact fake-only provider config lines'
    $rawBytes = [System.IO.File]::ReadAllBytes($path)
    $hasUtf8Bom = $rawBytes.Length -ge 3 -and
        $rawBytes[0] -eq 0xEF -and $rawBytes[1] -eq 0xBB -and $rawBytes[2] -eq 0xBF
    if ($hasUtf8Bom) {
        throw "fake-only provider config unexpectedly contains a UTF-8 BOM: $path"
    }
    return [pscustomobject][ordered]@{
        path = $path
        bytes = $rawBytes.Length
        sha256 = Get-AbSha256 $path
        provider_names = @('codex', 'claude', 'gemini', 'agy')
        executable = ConvertTo-AbComparableWindowsPath $FakeProvider
        exact_line_count = $actualLines.Count
        utf8_bom_present = $false
    }
}

function Get-AggregateMarkerCount {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return 0 }
    return @((Get-Content -LiteralPath $Path -ErrorAction Stop) | Where-Object { $_ -ceq 'legacy-inspect' }).Count
}

function Assert-AbEmptyDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    if (-not $item.PSIsContainer -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw "$Label is not a regular directory: $Path"
    }
    $children = @(Get-ChildItem -LiteralPath $Path -Force -ErrorAction Stop)
    if ($children.Count -ne 0) {
        throw "$Label contains $($children.Count) unexpected item(s)"
    }
}

function Get-LiveLeaseCount {
    param(
        [Parameter(Mandatory = $true)][string]$Database,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label
    )
    $rows = Invoke-Sqlite -Database $Database `
        -Sql 'SELECT count(*) AS row_count FROM daemon_instances WHERE released_at IS NULL;' `
        -WorkingDirectory $script:RunRoot -Environment $Environment -ReadOnly -Csv -Label $Label
    if ($rows.Count -ne 1) {
        throw "$Label returned $($rows.Count) rows"
    }
    return [int]$rows[0].row_count
}

function Get-AbDatabaseHealthEvidence {
    param(
        [Parameter(Mandatory = $true)][string]$Database,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$Environment,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)]
        [ValidateSet('ActiveDaemon', 'PostStopStable')]
        [string]$Phase,
        [Parameter(Mandatory = $true)][bool]$PostStopQuiescenceConfirmed
    )
    if ($Phase -eq 'PostStopStable' -and -not $PostStopQuiescenceConfirmed) {
        throw "$Label PostStopStable database health requires confirmed quiescence"
    }
    $integrity = Invoke-Sqlite -Database $Database -Sql 'PRAGMA integrity_check;' `
        -WorkingDirectory $script:RunRoot -Environment $Environment -ReadOnly -Csv `
        -Label "$Label-integrity"
    $foreignKeys = Invoke-Sqlite -Database $Database -Sql 'PRAGMA foreign_key_check;' `
        -WorkingDirectory $script:RunRoot -Environment $Environment -ReadOnly -Csv `
        -Label "$Label-foreign-keys"
    if ($integrity.Count -ne 1 -or [string]$integrity[0].integrity_check -cne 'ok') {
        throw "$Label SQLite integrity_check was not exactly ok"
    }
    if ($foreignKeys.Count -ne 0) {
        throw "$Label SQLite foreign_key_check found $($foreignKeys.Count) violation(s)"
    }
    $canonicalPhase = if ($Phase -eq 'PostStopStable') { 'PostStopStable' } else { 'ActiveDaemon' }
    $databaseFamilyHashScope = if ($canonicalPhase -ceq 'PostStopStable') {
        'post-stop-stable-sqlite-family'
    } else {
        'intentionally-omitted-active-daemon'
    }
    $databaseFamilyHashes = if ($canonicalPhase -ceq 'PostStopStable') {
        Get-SqliteFamilyHashes $Database
    } else {
        $null
    }
    return [pscustomobject][ordered]@{
        integrity_check = 'ok'
        foreign_key_violation_count = 0
        health_phase = $canonicalPhase
        post_stop_quiescence_confirmed = $PostStopQuiescenceConfirmed
        database_family_hash_scope = $databaseFamilyHashScope
        database_family_hashes = $databaseFamilyHashes
    }
}

function Get-AbMedian {
    param([Parameter(Mandatory = $true)][double[]]$Values)
    if ($Values.Count -eq 0) { throw 'cannot calculate a median from an empty sample' }
    $ordered = @($Values | Sort-Object)
    $middle = [math]::Floor($ordered.Count / 2)
    if (($ordered.Count % 2) -eq 1) { return [double]$ordered[$middle] }
    return ([double]$ordered[$middle - 1] + [double]$ordered[$middle]) / 2.0
}

function Get-AbDeltaLimit {
    param([Parameter(Mandatory = $true)][double]$BaselineMs)
    return [int64][math]::Max($AbsoluteDeltaFloorMs, [math]::Ceiling($BaselineMs * $RelativeDeltaFraction))
}

function Get-AbDeltaDecision {
    param([Parameter(Mandatory = $true)][object[]]$Observations)
    if ($Observations.Count -ne $ExpectedObservationCount) {
        throw "A/B result count was $($Observations.Count); expected exactly $ExpectedObservationCount"
    }
    $pairDeltas = [System.Collections.Generic.List[object]]::new()
    for ($pair = 1; $pair -le $ExpectedPairCount; $pair++) {
        $pairRows = @($Observations | Where-Object pair -EQ $pair)
        $aggregate = @($pairRows | Where-Object variant -EQ 'aggregate_only')
        $attributed = @($pairRows | Where-Object variant -EQ 'attributed')
        if ($pairRows.Count -ne 2 -or $aggregate.Count -ne 1 -or $attributed.Count -ne 1) {
            throw "A/B pair $pair is not one exact aggregate/attributed pair"
        }
        $aggregateMs = [int64]$aggregate[0].registration.elapsed_ms
        $attributedMs = [int64]$attributed[0].registration.elapsed_ms
        $deltaMs = $attributedMs - $aggregateMs
        $limitMs = Get-AbDeltaLimit $aggregateMs
        $aggregateSeedMs = [int64]$aggregate[0].seed_timing.elapsed_ms
        $attributedSeedMs = [int64]$attributed[0].seed_timing.elapsed_ms
        $seedDeltaMs = $attributedSeedMs - $aggregateSeedMs
        $seedLimitMs = Get-AbDeltaLimit $aggregateSeedMs
        $pairDeltas.Add([pscustomobject][ordered]@{
            pair = $pair
            attributed_order = [int]$attributed[0].order
            aggregate_only_ms = $aggregateMs
            attributed_ms = $attributedMs
            attributed_minus_aggregate_ms = $deltaMs
            pair_limit_ms = $limitMs
            exceeds_pair_limit = $deltaMs -gt $limitMs
            aggregate_seed_ms = $aggregateSeedMs
            attributed_seed_ms = $attributedSeedMs
            attributed_minus_aggregate_seed_ms = $seedDeltaMs
            seed_pair_limit_ms = $seedLimitMs
            exceeds_seed_pair_limit = [math]::Abs($seedDeltaMs) -gt $seedLimitMs
        })
    }

    $medianAggregate = Get-AbMedian ([double[]]@($pairDeltas | ForEach-Object aggregate_only_ms))
    $medianDelta = Get-AbMedian ([double[]]@($pairDeltas | ForEach-Object attributed_minus_aggregate_ms))
    $medianLimit = Get-AbDeltaLimit $medianAggregate
    $medianAggregateSeed = Get-AbMedian ([double[]]@($pairDeltas | ForEach-Object aggregate_seed_ms))
    $medianSeedDelta = Get-AbMedian ([double[]]@($pairDeltas | ForEach-Object attributed_minus_aggregate_seed_ms))
    $medianSeedLimit = Get-AbDeltaLimit $medianAggregateSeed
    $attributedFirstMedian = Get-AbMedian ([double[]]@(
        $pairDeltas | Where-Object attributed_order -EQ 1 | ForEach-Object attributed_minus_aggregate_ms
    ))
    $attributedSecondMedian = Get-AbMedian ([double[]]@(
        $pairDeltas | Where-Object attributed_order -EQ 2 | ForEach-Object attributed_minus_aggregate_ms
    ))
    $orderBiasMs = $attributedFirstMedian - $attributedSecondMedian
    $orderBiasLimitMs = Get-AbDeltaLimit $medianAggregate
    $pairExceedances = @($pairDeltas | Where-Object exceeds_pair_limit).Count
    $seedExceedances = @($pairDeltas | Where-Object exceeds_seed_pair_limit).Count
    $reasons = [System.Collections.Generic.List[string]]::new()
    if ($pairExceedances -gt $MaximumPairExceedanceCount) {
        $reasons.Add("registration pair exceedances $pairExceedances exceeded $MaximumPairExceedanceCount")
    }
    if ($medianDelta -gt $medianLimit) {
        $reasons.Add("registration median delta ${medianDelta}ms exceeded ${medianLimit}ms")
    }
    if ([math]::Abs($orderBiasMs) -gt $orderBiasLimitMs) {
        $reasons.Add("registration order bias ${orderBiasMs}ms exceeded +/-${orderBiasLimitMs}ms")
    }
    if ($seedExceedances -gt $MaximumPairExceedanceCount) {
        $reasons.Add("seed pair exceedances $seedExceedances exceeded $MaximumPairExceedanceCount")
    }
    if ([math]::Abs($medianSeedDelta) -gt $medianSeedLimit) {
        $reasons.Add("seed median drift ${medianSeedDelta}ms exceeded +/-${medianSeedLimit}ms")
    }
    $retainCombinedPhase = $reasons.Count -eq 0
    return [pscustomobject][ordered]@{
        criterion = [pscustomobject][ordered]@{
            absolute_floor_ms = $AbsoluteDeltaFloorMs
            relative_fraction = $RelativeDeltaFraction
            limit_formula = 'max(100ms, ceil(aggregate_baseline_ms * 5%))'
            maximum_pair_exceedance_count = $MaximumPairExceedanceCount
            median_delta_must_not_exceed_limit = $true
            absolute_order_bias_must_not_exceed_limit = $true
            seed_pair_exceedance_count_must_not_exceed = $MaximumPairExceedanceCount
            absolute_seed_median_drift_must_not_exceed_limit = $true
        }
        raw_pair_deltas = $pairDeltas.ToArray()
        registration_pair_exceedance_count = $pairExceedances
        registration_median_aggregate_ms = $medianAggregate
        registration_median_delta_ms = $medianDelta
        registration_median_limit_ms = $medianLimit
        attributed_first_median_delta_ms = $attributedFirstMedian
        attributed_second_median_delta_ms = $attributedSecondMedian
        order_bias_ms = $orderBiasMs
        order_bias_limit_ms = $orderBiasLimitMs
        seed_pair_exceedance_count = $seedExceedances
        seed_median_aggregate_ms = $medianAggregateSeed
        seed_median_delta_ms = $medianSeedDelta
        seed_median_limit_ms = $medianSeedLimit
        retain_combined_latency_and_correctness_phase = $retainCombinedPhase
        decision = if ($retainCombinedPhase) {
            'retain-attributed-markers-in-latency-phase'
        } else {
            'split-latency-marker-off-and-correctness-marker-on-phases'
        }
        reasons = $reasons.ToArray()
    }
}

$script:ResolvedColay = Resolve-AbFile $ColayExe 'colay executable'
$script:ResolvedFake = Resolve-AbFile $FakeProviderExe 'fake provider executable'
$script:ResolvedStress = Resolve-AbFile $StressHarness 'stress harness'
$script:ResolvedDiagnostic = Resolve-AbFile $PSCommandPath 'A/B diagnostic script'
$resolvedEvidenceRoot = [System.IO.Path]::GetFullPath($EvidenceRoot)
if (-not (Test-Path -LiteralPath $resolvedEvidenceRoot -PathType Container)) {
    throw "evidence root does not exist: $resolvedEvidenceRoot"
}
if ([System.IO.Path]::GetFileName($script:ResolvedFake) -cne 'colay-e2e-fake-provider.exe') {
    throw 'A/B diagnostic permits only the orchestrator-test-support fake provider binary'
}

$script:RepoRoot = [System.IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $script:ResolvedStress) '../..'))
if (-not (Test-Path -LiteralPath (Join-Path $script:RepoRoot 'Cargo.toml') -PathType Leaf)) {
    throw "stress harness does not resolve to the expected repository layout: $($script:ResolvedStress)"
}

$staticContract = Assert-AbStaticContract -DiagnosticPath $script:ResolvedDiagnostic
$initialHashCheckpoint = Get-AbInputHashCheckpoint -Label 'initial-pre-mutation' -ExpectedMigrationHashes $null
$migrationBaseline = $initialHashCheckpoint.migrations

Register-AbVolume -Path $resolvedEvidenceRoot -Label 'evidence_root'
Register-AbVolume -Path ([System.IO.Path]::GetTempPath()) -Label 'runtime_root'
[void](Assert-FreeDisk)
$preexisting = @(
    Get-AbExactCandidateProcesses @($script:ResolvedColay, $script:ResolvedFake)
)
if ($preexisting.Count -ne 0) {
    throw "candidate process residue exists before A/B diagnostic: $($preexisting | ConvertTo-Json -Depth 10 -Compress)"
}

$importContract = Import-StressHarnessFunctions $script:ResolvedStress
Initialize-AbHarnessProcessIdentity
$script:PythonExe = Resolve-AbFile (
    (Get-Command python.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
) 'Python executable'

$runStamp = [datetime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
$evidencePath = Join-Path $resolvedEvidenceRoot "marker-attribution-ab-$runStamp.json"
if (Test-Path -LiteralPath $evidencePath) {
    throw "fresh A/B evidence path already exists: $evidencePath"
}
$summary = [ordered]@{
    schema_version = 2
    diagnostic = 'non-authoritative-marker-attribution-ab'
    run_id = $runStamp
    started_at_utc = [datetime]::UtcNow.ToString('o')
    completed_at_utc = $null
    status = 'failed'
    failure = $null
    decision = $null
    pair_order = $PairOrders
    expected_pair_count = $ExpectedPairCount
    expected_observation_count = $ExpectedObservationCount
    expected_retry_count = $ExpectedRetryCount
    actual_pair_count = 0
    actual_observation_count = 0
    actual_retry_count = 0
    fresh_runtime_root_count = 0
    equal_length_runtime_roots = $false
    variant_neutral_environment_shape = $false
    measurement_method = 'os-process-lifetime'
    synchronous_wait_loop_cim = $false
    provider_key_names_cleared = $ProviderKeyNames
    provider_credential_key_count = 0
    fake_provider_only = $true
    exact_os = 'Windows_NT'
    input_hash_checkpoints = [System.Collections.Generic.List[object]]::new()
    expected_input_hash_checkpoint_count = $ExpectedHashCheckpointLabels.Count
    expected_input_hash_checkpoint_labels = $ExpectedHashCheckpointLabels
    exact_input_hash_checkpoint_labels = $false
    imported_stress_contract = $importContract
    static_contract = $staticContract
    hashes = [pscustomobject][ordered]@{
        colay = [pscustomobject]@{
            path = $script:ResolvedColay
            sha256 = Get-AbSha256 $script:ResolvedColay
        }
        fake_provider = [pscustomobject]@{
            path = $script:ResolvedFake
            sha256 = Get-AbSha256 $script:ResolvedFake
        }
        stress_harness = [pscustomobject]@{
            path = $script:ResolvedStress
            sha256 = Get-AbSha256 $script:ResolvedStress
        }
        diagnostic_script = [pscustomobject]@{
            path = $script:ResolvedDiagnostic
            sha256 = Get-AbSha256 $script:ResolvedDiagnostic
        }
        schema_v8_seed_migrations = $migrationBaseline
    }
    observations = [System.Collections.Generic.List[object]]::new()
    delta_analysis = $null
    disk_volumes = @()
    final_residual_processes = @()
    total_cleanup_error_count = 0
}
$summary.input_hash_checkpoints.Add($initialHashCheckpoint)
$freshRoots = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
$fatal = $null
$finalHashCheckpointCaptured = $false

try {
    for ($pairIndex = 0; $pairIndex -lt $PairOrders.Count; $pairIndex++) {
        $pairNumber = $pairIndex + 1
        $summary.input_hash_checkpoints.Add((Get-AbInputHashCheckpoint `
            -Label ("before-pair-{0:D2}" -f $pairNumber) `
            -ExpectedMigrationHashes $migrationBaseline))

        for ($orderIndex = 0; $orderIndex -lt 2; $orderIndex++) {
            $orderNumber = $orderIndex + 1
            $variant = [string]$PairOrders[$pairIndex].variants[$orderIndex]
            $armLabel = "p{0:D2}-o{1:D2}-arm" -f $pairNumber, $orderNumber
            $runtimeRoot = Join-Path ([System.IO.Path]::GetTempPath()) "colay-marker-ab-$runStamp-$armLabel"
            if (Test-Path -LiteralPath $runtimeRoot) {
                throw "fresh A/B runtime already exists: $runtimeRoot"
            }
            if (-not $freshRoots.Add((ConvertTo-AbComparableWindowsPath $runtimeRoot))) {
                throw "duplicate A/B runtime root was selected: $runtimeRoot"
            }

            $script:RunRoot = $runtimeRoot
            $script:ColayHome = Join-Path $runtimeRoot 'colay-home'
            $workspaceRoot = Join-Path $runtimeRoot 'workspaces'
            $emptyRepository = Join-Path $workspaceRoot 'empty-incumbent'
            foreach ($directory in @($runtimeRoot, $workspaceRoot, $emptyRepository, $script:ColayHome)) {
                [void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetFullPath($directory))
            }
            Register-AbVolume -Path $runtimeRoot -Label $armLabel

            $environmentRecord = New-AbEnvironment -Root $runtimeRoot -ColayHome $script:ColayHome `
                -FakeProvider $script:ResolvedFake -Variant $variant
            $environment = $environmentRecord.values
            $environmentShape = Assert-AbEnvironment -Environment $environment -Variant $variant `
                -FakeProvider $script:ResolvedFake -RuntimeRoot $runtimeRoot
            New-FakeProviderConfig -ColayHomePath $script:ColayHome -FakeProvider $script:ResolvedFake
            $providerConfigBefore = Assert-AbFakeProviderConfig -ColayHomePath $script:ColayHome `
                -FakeProvider $script:ResolvedFake

            $observation = [pscustomobject][ordered]@{
                pair = $pairNumber
                order = $orderNumber
                variant = $variant
                arm_label = $armLabel
                runtime_root = $runtimeRoot
                runtime_root_length = $runtimeRoot.Length
                environment_shape = $environmentShape
                provider_config = [pscustomobject][ordered]@{
                    before = $providerConfigBefore
                    after = $null
                }
                seed_timing = $null
                daemon_start = $null
                daemon_readiness = [pscustomobject][ordered]@{
                    readiness_status = 'not-attempted'
                    original_state = $null
                    final_state = $null
                    poll_count = 0
                    elapsed_ms = 0
                    overall_timeout_ms = $DaemonReadinessTimeoutMs
                    poll_interval_ms = $DaemonReadinessPollIntervalMs
                    cleanup_reserve_ms = $DaemonReadinessCleanupReserveMs
                    status_command = @('--json', 'daemon', 'status')
                    anchored_identity = $null
                    polls = @()
                    online_document = $null
                    failure = $null
                }
                daemon_identity = [pscustomobject][ordered]@{
                    capture_status = 'not-attempted'
                    primary_failure = $null
                    direct_mutation_allowed = $false
                    refusal_reason = $null
                    handle_opened = $false
                    handle_close_attempted = $false
                    handle_closed = $false
                    close_error = $null
                    handle_balance = [pscustomobject][ordered]@{
                        opened = 0
                        closed = 0
                        outstanding = 0
                    }
                    cleanup_errors = @()
                }
                registration = $null
                source_hashes = $null
                source_config = $null
                durable = $null
                database_health = $null
                zero_writable_rows = $null
                markers = $null
                cleanup = [pscustomobject][ordered]@{
                    daemon_stop = $null
                    retained_handle = $null
                    endpoint_status = $null
                    live_lease_count = $null
                    database_health_after_cleanup = $null
                    zero_writable_rows_after_cleanup = $null
                    source_after_cleanup = $null
                    provider_config_after_cleanup = $null
                    markers_after_cleanup = $null
                    residual_processes = @()
                    cleanup_error_count = 0
                    cleanup_errors = @()
                }
                failure = $null
            }
            $summary.observations.Add($observation)
            $armFailure = $null
            $daemonIdentity = $null
            $seed = $null

            try {
                [void](Assert-FreeDisk)
                $commandOffset = $script:CommandEvidence.Count
                $seed = New-LegacyWorkspace -Index 1 -Root $workspaceRoot -Environment $environment
                $seedCommands = @($script:CommandEvidence | Select-Object -Skip $commandOffset |
                    Where-Object label -EQ 'seed-schema-v8-1')
                if ($seedCommands.Count -ne 1) {
                    throw "$armLabel did not record exactly one independent Python seed command"
                }
                $observation.seed_timing = $seedCommands[0]
                if ([string]$seedCommands[0].measurement_method -cne 'os-process-lifetime') {
                    throw "$armLabel seed command did not use OS process lifetime"
                }
                $observation.source_hashes = $seed.source_hashes
                $sourceConfigPath = Join-Path $seed.repository '.colay/config.toml'
                $observation.source_config = [pscustomobject][ordered]@{
                    path = $sourceConfigPath
                    before_sha256 = [string]$seed.config_sha256
                    after_sha256 = $null
                }

                $started = Invoke-Colay -Repository $emptyRepository `
                    -ArgumentValues @('--json', 'daemon', 'start') `
                    -Environment $environment -Label "$armLabel-daemon-start" -TimeoutMs 40000
                $startDocument = Assert-StatusJson $started
                $observation.daemon_start = $started
                $readiness = $null
                try {
                    $readiness = Wait-AbDaemonReadiness -DaemonStartDocument $startDocument `
                        -ExpectedExecutable $script:ResolvedColay -Repository $emptyRepository `
                        -Environment $environment -Label $armLabel
                    $observation.daemon_readiness = $readiness.Evidence
                } catch {
                    $readinessFailureEvidence = `
                        $_.Exception.Data['ColayMarkerAbDaemonReadinessEvidence']
                    if ($null -ne $readinessFailureEvidence) {
                        $observation.daemon_readiness = $readinessFailureEvidence
                    } else {
                        $observation.daemon_readiness.failure = $_.Exception.Message
                    }
                    throw
                }
                try {
                    $daemonIdentity = Open-AbDaemonIdentity `
                        -DaemonDocument $readiness.OnlineDocument `
                        -ExpectedExecutable $script:ResolvedColay
                    $observation.daemon_identity = $daemonIdentity.Evidence
                } catch {
                    $captureFailureEvidence = $_.Exception.Data['ColayMarkerAbDaemonCaptureEvidence']
                    if ($null -eq $captureFailureEvidence) {
                        $captureFailureEvidence = [pscustomobject][ordered]@{
                            capture_status = 'refused-before-handle-open'
                            primary_failure = $_.Exception.Message
                            refusal_reason = $_.Exception.Message
                            direct_mutation_allowed = $false
                            handle_opened = $false
                            handle_close_attempted = $false
                            handle_closed = $false
                            close_error = $null
                            handle_balance = [pscustomobject][ordered]@{
                                opened = 0
                                closed = 0
                                outstanding = 0
                            }
                            cleanup_errors = @()
                        }
                    }
                    $observation.daemon_identity = $captureFailureEvidence
                    throw
                }

                [void](Assert-DurableState -Seeds @() -ExpectedWorkspaceCount 1 -Environment $environment)

                $registration = Invoke-Colay -Repository $seed.repository `
                    -ArgumentValues @('--json', 'status') `
                    -Environment $environment -Label "$armLabel-registration" -TimeoutMs 12000
                [void](Assert-StatusJson $registration)
                if ([string]$registration.measurement_method -cne 'os-process-lifetime' -or
                    [bool]$registration.timed_out -or
                    [int]$registration.exit_code -ne 0) {
                    throw "$armLabel registration did not satisfy the exact timing/exit contract"
                }
                $observation.registration = $registration

                $sourceHashesAfter = Get-SqliteFamilyHashes $seed.database
                $seed.source_hashes.after = $sourceHashesAfter
                Assert-EquivalentJson $seed.source_hashes.before $sourceHashesAfter `
                    "$armLabel source SQLite family"
                $sourceConfigAfter = Get-AbSha256 $sourceConfigPath
                $observation.source_config.after_sha256 = $sourceConfigAfter
                if ($sourceConfigAfter -cne [string]$seed.config_sha256) {
                    throw "$armLabel source config changed"
                }

                $observation.durable = Assert-DurableState -Seeds @($seed) -ExpectedWorkspaceCount 2 `
                    -Environment $environment
                $globalDatabase = Join-Path $script:ColayHome 'state/state.db'
                $observation.database_health = Get-AbDatabaseHealthEvidence -Database $globalDatabase `
                    -Environment $environment -Label $armLabel -Phase ActiveDaemon `
                    -PostStopQuiescenceConfirmed $false
                $observation.zero_writable_rows = Assert-ZeroWritableRows -Database $globalDatabase `
                    -Environment $environment

                $providerConfigAfter = Assert-AbFakeProviderConfig -ColayHomePath $script:ColayHome `
                    -FakeProvider $script:ResolvedFake
                $observation.provider_config.after = $providerConfigAfter
                Assert-AbEquivalentJson $providerConfigBefore $providerConfigAfter `
                    "$armLabel fake-only provider config"

                $aggregateCount = Get-AggregateMarkerCount $environmentRecord.aggregate_marker
                if ($aggregateCount -ne 2) {
                    throw "$armLabel aggregate marker count was $aggregateCount; expected exactly 2"
                }
                Assert-AbEmptyDirectory -Path $environmentRecord.padding_marker `
                    -Label "$armLabel padding marker directory"
                if ($variant -ceq 'aggregate_only') {
                    Assert-AbEmptyDirectory -Path $environmentRecord.active_attributed_marker `
                        -Label "$armLabel inactive attributed marker directory"
                    $observation.markers = [pscustomobject][ordered]@{
                        aggregate_count = 2
                        attributed_group_count = 0
                        attributed_event_count = 0
                        groups = @()
                    }
                } else {
                    $groups = Get-AttributedInspectionSnapshot $environmentRecord.active_attributed_marker
                    if ($groups.Count -ne 1) {
                        throw "$armLabel attributed group count was $($groups.Count); expected exactly 1"
                    }
                    $group = @($groups.Values)[0]
                    if ([string]$seed.inspection_group_id -cne [string]$group.group_id -or
                        [int]$group.event_count -ne 2) {
                        throw "$armLabel durable source_root_hash or event count does not match its marker group"
                    }
                    $observation.markers = [pscustomobject][ordered]@{
                        aggregate_count = 2
                        attributed_group_count = 1
                        attributed_event_count = 2
                        groups = @($group)
                    }
                }
            } catch {
                $armFailure = $_
                $observation.failure = [pscustomobject][ordered]@{
                    message = $_.Exception.Message
                    category = [string]$_.CategoryInfo.Category
                    script_stack = $_.ScriptStackTrace
                }
            } finally {
                $cleanupErrors = [System.Collections.Generic.List[string]]::new()
                try {
                    $stop = Invoke-Colay -Repository $emptyRepository `
                        -ArgumentValues @('--json', 'daemon', 'stop') `
                        -Environment $environment -Label "$armLabel-daemon-stop" -TimeoutMs 20000
                    $observation.cleanup.daemon_stop = Assert-ExactStoppedStatus -Result $stop `
                        -ExpectedCommand 'daemon_stop'
                } catch {
                    $cleanupErrors.Add("daemon stop: $($_.Exception.Message)")
                }

                if ($null -ne $daemonIdentity) {
                    try {
                        $retainedCleanup = Complete-AbDaemonIdentity -Identity $daemonIdentity
                        $observation.cleanup.retained_handle = $retainedCleanup
                        foreach ($errorMessage in @($retainedCleanup.errors)) {
                            $cleanupErrors.Add("retained daemon identity: $errorMessage")
                        }
                    } catch {
                        $cleanupErrors.Add("retained daemon identity cleanup: $($_.Exception.Message)")
                    }
                } else {
                    $captureCleanupErrors = @($observation.daemon_identity.cleanup_errors)
                    $observation.cleanup.retained_handle = [pscustomobject][ordered]@{
                        direct_mutation_attempted = $false
                        reason = 'no verified retained handle; direct process mutation refused'
                        capture_failure = $observation.daemon_identity
                        handle_balance = $observation.daemon_identity.handle_balance
                        errors = $captureCleanupErrors
                    }
                    foreach ($captureCleanupError in $captureCleanupErrors) {
                        $cleanupErrors.Add("retained daemon identity capture: $captureCleanupError")
                    }
                }

                try {
                    $status = Invoke-Colay -Repository $emptyRepository `
                        -ArgumentValues @('--json', 'daemon', 'status') `
                        -Environment $environment -Label "$armLabel-daemon-status" -TimeoutMs 10000
                    $observation.cleanup.endpoint_status = Assert-ExactStoppedStatus -Result $status `
                        -ExpectedCommand 'daemon_status'
                } catch {
                    $cleanupErrors.Add("endpoint status: $($_.Exception.Message)")
                }

                $globalDatabase = Join-Path $script:ColayHome 'state/state.db'
                if (Test-Path -LiteralPath $globalDatabase -PathType Leaf) {
                    try {
                        $observation.cleanup.live_lease_count = Get-LiveLeaseCount -Database $globalDatabase `
                            -Environment $environment -Label "$armLabel-live-leases"
                        if ($observation.cleanup.live_lease_count -ne 0) {
                            $cleanupErrors.Add("live lease count: $($observation.cleanup.live_lease_count)")
                        }
                    } catch {
                        $cleanupErrors.Add("live lease query: $($_.Exception.Message)")
                    }
                    $retainedHandleEvidence = $observation.cleanup.retained_handle
                    $retainedWaitSignaled = $false
                    if ($null -ne $retainedHandleEvidence) {
                        $initialWaitProperty = `
                            $retainedHandleEvidence.PSObject.Properties['initial_wait_result']
                        $finalWaitProperty = `
                            $retainedHandleEvidence.PSObject.Properties['final_wait_result']
                        $retainedWaitSignaled = (
                            $null -ne $initialWaitProperty -and
                            $null -ne $initialWaitProperty.Value -and
                            [uint64]$initialWaitProperty.Value -eq 0
                        ) -or (
                            $null -ne $finalWaitProperty -and
                            $null -ne $finalWaitProperty.Value -and
                            [uint64]$finalWaitProperty.Value -eq 0
                        )
                    }
                    $postStopQuiescenceConfirmed = (
                        $null -ne $observation.cleanup.daemon_stop -and
                        [string]$observation.cleanup.daemon_stop.schema_version -ceq '1' -and
                        [string]$observation.cleanup.daemon_stop.command -ceq 'daemon_stop' -and
                        [string]$observation.cleanup.daemon_stop.data.status.state -ceq 'stopped' -and
                        $retainedWaitSignaled -and
                        @($retainedHandleEvidence.errors).Count -eq 0 -and
                        $null -ne $observation.cleanup.endpoint_status -and
                        [string]$observation.cleanup.endpoint_status.schema_version -ceq '1' -and
                        [string]$observation.cleanup.endpoint_status.command -ceq 'daemon_status' -and
                        [string]$observation.cleanup.endpoint_status.data.status.state -ceq 'stopped' -and
                        $null -ne $observation.cleanup.live_lease_count -and
                        [int]$observation.cleanup.live_lease_count -eq 0
                    )
                    try {
                        $observation.cleanup.database_health_after_cleanup = `
                            Get-AbDatabaseHealthEvidence -Database $globalDatabase `
                                -Environment $environment -Label "$armLabel-after-cleanup" `
                                -Phase PostStopStable `
                                -PostStopQuiescenceConfirmed $postStopQuiescenceConfirmed
                        $observation.cleanup.zero_writable_rows_after_cleanup = `
                            Assert-ZeroWritableRows -Database $globalDatabase -Environment $environment
                    } catch {
                        $cleanupErrors.Add("database evidence after cleanup: $($_.Exception.Message)")
                    }
                }

                try {
                    if ($null -eq $seed) {
                        throw 'seed evidence is unavailable'
                    }
                    $sourceHashesAfterCleanup = Get-SqliteFamilyHashes $seed.database
                    Assert-EquivalentJson $seed.source_hashes.before $sourceHashesAfterCleanup `
                        "$armLabel source SQLite family after cleanup"
                    $sourceConfigAfterCleanup = Get-AbSha256 (Join-Path $seed.repository '.colay/config.toml')
                    if ($sourceConfigAfterCleanup -cne [string]$seed.config_sha256) {
                        throw 'source config changed during cleanup'
                    }
                    $observation.cleanup.source_after_cleanup = [pscustomobject][ordered]@{
                        sqlite_family_hashes = $sourceHashesAfterCleanup
                        config_sha256 = $sourceConfigAfterCleanup
                    }
                    $providerConfigAfterCleanup = Assert-AbFakeProviderConfig `
                        -ColayHomePath $script:ColayHome -FakeProvider $script:ResolvedFake
                    Assert-AbEquivalentJson $providerConfigBefore $providerConfigAfterCleanup `
                        "$armLabel fake-only provider config after cleanup"
                    $observation.cleanup.provider_config_after_cleanup = $providerConfigAfterCleanup
                } catch {
                    $cleanupErrors.Add("source/config evidence after cleanup: $($_.Exception.Message)")
                }

                try {
                    if ($null -eq $observation.markers) {
                        throw 'pre-cleanup marker evidence is unavailable'
                    }
                    $aggregateAfterCleanup = Get-AggregateMarkerCount $environmentRecord.aggregate_marker
                    if ($aggregateAfterCleanup -ne 2) {
                        throw "aggregate marker count after cleanup was $aggregateAfterCleanup; expected exactly 2"
                    }
                    Assert-AbEmptyDirectory -Path $environmentRecord.padding_marker `
                        -Label "$armLabel padding marker directory after cleanup"
                    $markerEvidenceAfterCleanup = if ($variant -ceq 'aggregate_only') {
                        Assert-AbEmptyDirectory -Path $environmentRecord.active_attributed_marker `
                            -Label "$armLabel inactive attributed marker directory after cleanup"
                        [pscustomobject][ordered]@{
                            aggregate_count = 2
                            attributed_group_count = 0
                            attributed_event_count = 0
                            groups = @()
                        }
                    } else {
                        $groupsAfterCleanup = Get-AttributedInspectionSnapshot `
                            $environmentRecord.active_attributed_marker
                        [pscustomobject][ordered]@{
                            aggregate_count = 2
                            attributed_group_count = $groupsAfterCleanup.Count
                            attributed_event_count = [int](@(
                                $groupsAfterCleanup.Values | Measure-Object -Property event_count -Sum
                            )[0].Sum)
                            groups = @($groupsAfterCleanup.Values)
                        }
                    }
                    Assert-AbEquivalentJson $observation.markers $markerEvidenceAfterCleanup `
                        "$armLabel marker evidence after cleanup"
                    $observation.cleanup.markers_after_cleanup = $markerEvidenceAfterCleanup
                } catch {
                    $cleanupErrors.Add("marker evidence after cleanup: $($_.Exception.Message)")
                }

                try {
                    $observation.cleanup.residual_processes = @(
                        Get-AbExactCandidateProcesses @($script:ResolvedColay, $script:ResolvedFake)
                    )
                    if ($observation.cleanup.residual_processes.Count -ne 0) {
                        $cleanupErrors.Add(
                            "candidate process residue: $($observation.cleanup.residual_processes | ConvertTo-Json -Depth 10 -Compress)"
                        )
                    }
                } catch {
                    $cleanupErrors.Add("process residue query: $($_.Exception.Message)")
                }
                try {
                    [void](Assert-FreeDisk)
                } catch {
                    $cleanupErrors.Add("free space: $($_.Exception.Message)")
                }

                $observation.cleanup.cleanup_error_count = $cleanupErrors.Count
                $observation.cleanup.cleanup_errors = $cleanupErrors.ToArray()
                if ($cleanupErrors.Count -ne 0) {
                    $cleanupMessage = $cleanupErrors -join '; '
                    if ($null -eq $armFailure) {
                        $armFailure = [System.Management.Automation.ErrorRecord]::new(
                            [System.Exception]::new($cleanupMessage),
                            'AbCleanupFailure',
                            [System.Management.Automation.ErrorCategory]::OperationStopped,
                            $runtimeRoot
                        )
                    }
                    $observation.failure = [pscustomobject][ordered]@{
                        message = if ($null -eq $observation.failure) {
                            $cleanupMessage
                        } else {
                            "$($observation.failure.message); cleanup: $cleanupMessage"
                        }
                        category = 'CleanupFailure'
                        script_stack = if ($null -eq $observation.failure) {
                            $null
                        } else {
                            $observation.failure.script_stack
                        }
                    }
                }
            }

            if ($null -ne $armFailure) { throw $armFailure }
        }
        $summary.input_hash_checkpoints.Add((Get-AbInputHashCheckpoint `
            -Label ("after-pair-{0:D2}" -f $pairNumber) `
            -ExpectedMigrationHashes $migrationBaseline))
    }

    if ($summary.observations.Count -ne $ExpectedObservationCount -or
        $freshRoots.Count -ne $ExpectedObservationCount) {
        throw "A/B did not produce exactly $ExpectedObservationCount fresh-root observations"
    }
    $rootLengths = @($summary.observations | ForEach-Object runtime_root_length | Sort-Object -Unique)
    if ($rootLengths.Count -ne 1) {
        throw "A/B runtime roots were not equal length: $($rootLengths -join ', ')"
    }
    $environmentCounts = @(
        $summary.observations | ForEach-Object { $_.environment_shape.variable_count } | Sort-Object -Unique
    )
    $environmentCharacters = @(
        $summary.observations | ForEach-Object { $_.environment_shape.serialized_character_count } |
            Sort-Object -Unique
    )
    $selectorKeyLengths = @(
        $summary.observations | ForEach-Object { $_.environment_shape.selector_key_length } | Sort-Object -Unique
    )
    $selectorValueLengths = @(
        $summary.observations | ForEach-Object { $_.environment_shape.selector_value_length } | Sort-Object -Unique
    )
    $neutralEnvironmentSignatures = @(
        $summary.observations | ForEach-Object {
            $_.environment_shape.neutral_entries | ConvertTo-Json -Depth 10 -Compress
        } | Sort-Object -Unique
    )
    if ($environmentCounts.Count -ne 1 -or $environmentCharacters.Count -ne 1 -or
        $selectorKeyLengths.Count -ne 1 -or $selectorValueLengths.Count -ne 1 -or
        $neutralEnvironmentSignatures.Count -ne 1) {
        throw 'A/B isolated environments were not equal-length and variant-neutral in shape'
    }

    $summary.actual_pair_count = $ExpectedPairCount
    $summary.actual_observation_count = $summary.observations.Count
    $summary.fresh_runtime_root_count = $freshRoots.Count
    $summary.equal_length_runtime_roots = $true
    $summary.variant_neutral_environment_shape = $true
    $summary.delta_analysis = Get-AbDeltaDecision -Observations $summary.observations.ToArray()
    $summary.decision = $summary.delta_analysis.decision
    $summary.status = 'passed'
} catch {
    $fatal = $_
    $summary.failure = [pscustomobject][ordered]@{
        message = $_.Exception.Message
        category = [string]$_.CategoryInfo.Category
        script_stack = $_.ScriptStackTrace
    }
} finally {
    try {
        $summary.input_hash_checkpoints.Add((Get-AbInputHashCheckpoint -Label 'final' `
            -ExpectedMigrationHashes $migrationBaseline))
        $finalHashCheckpointCaptured = $true
    } catch {
        $summary.status = 'failed'
        if ($null -eq $summary.failure) {
            $summary.failure = [pscustomobject][ordered]@{
                message = $_.Exception.Message
                category = 'InputHashFailure'
                script_stack = $_.ScriptStackTrace
            }
        }
    }
    try {
        $actualHashCheckpointLabels = @($summary.input_hash_checkpoints | ForEach-Object label)
        Assert-AbEquivalentJson $ExpectedHashCheckpointLabels $actualHashCheckpointLabels `
            'exact input hash checkpoint labels'
        $summary.exact_input_hash_checkpoint_labels = $true
    } catch {
        $summary.status = 'failed'
        if ($null -eq $summary.failure) {
            $summary.failure = [pscustomobject][ordered]@{
                message = $_.Exception.Message
                category = 'InputHashCheckpointFailure'
                script_stack = $_.ScriptStackTrace
            }
        }
    }
    try {
        $summary.final_residual_processes = @(
            Get-AbExactCandidateProcesses @($script:ResolvedColay, $script:ResolvedFake)
        )
        if ($summary.final_residual_processes.Count -ne 0) {
            $summary.status = 'failed'
            if ($null -eq $summary.failure) {
                $summary.failure = [pscustomobject][ordered]@{
                    message = 'exact candidate process residue remained after A/B diagnostic'
                    category = 'CleanupFailure'
                    script_stack = $null
                }
            }
        }
    } catch {
        $summary.status = 'failed'
        if ($null -eq $summary.failure) {
            $summary.failure = [pscustomobject][ordered]@{
                message = $_.Exception.Message
                category = 'CleanupFailure'
                script_stack = $_.ScriptStackTrace
            }
        }
    }
    try {
        [void](Assert-FreeDisk)
    } catch {
        $summary.status = 'failed'
        if ($null -eq $summary.failure) {
            $summary.failure = [pscustomobject][ordered]@{
                message = $_.Exception.Message
                category = 'DiskFloorFailure'
                script_stack = $_.ScriptStackTrace
            }
        }
    }
    $summary.actual_observation_count = $summary.observations.Count
    $summary.fresh_runtime_root_count = $freshRoots.Count
    $summary.total_cleanup_error_count = [int](@(
        $summary.observations | ForEach-Object { $_.cleanup.cleanup_error_count } |
            Measure-Object -Sum
    )[0].Sum)
    if ($summary.status -ceq 'passed' -and (
        -not $finalHashCheckpointCaptured -or
        $summary.input_hash_checkpoints.Count -ne $summary.expected_input_hash_checkpoint_count -or
        -not $summary.exact_input_hash_checkpoint_labels -or
        $summary.actual_observation_count -ne $ExpectedObservationCount -or
        $summary.actual_pair_count -ne $ExpectedPairCount -or
        $summary.actual_retry_count -ne $ExpectedRetryCount -or
        $summary.fresh_runtime_root_count -ne $ExpectedObservationCount -or
        $summary.total_cleanup_error_count -ne 0 -or
        $summary.final_residual_processes.Count -ne 0
    )) {
        $summary.status = 'failed'
        $summary.failure = [pscustomobject][ordered]@{
            message = 'final exact-count, hash, cleanup, or residue contract was not satisfied'
            category = 'AcceptanceContractFailure'
            script_stack = $null
        }
    }
    $summary.disk_volumes = Get-AbDiskEvidence
    $summary.completed_at_utc = [datetime]::UtcNow.ToString('o')
    [ordered]@{
        summary = $summary
        commands = $script:CommandEvidence
        process_setup_failures = $script:ProcessSetupFailureEvidence
        owned_process_identities = @($script:OwnedProcessIdentities | ForEach-Object {
            Get-ProcessIdentityEvidence $_
        })
    } | ConvertTo-Json -Depth 60 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
}

if ($null -ne $fatal) { throw $fatal }
if ($summary.status -cne 'passed') {
    throw "marker attribution A/B diagnostic failed; evidence: $evidencePath"
}
$summary | ConvertTo-Json -Depth 40
