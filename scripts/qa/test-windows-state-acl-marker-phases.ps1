#requires -Version 7.2

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$stressPath = Join-Path $scriptRoot 'windows-state-acl-stress.ps1'
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("colay-marker-phase-test-" + [guid]::NewGuid().ToString('N'))
$failures = [System.Collections.Generic.List[string]]::new()

function Assert-Equal {
    param($Expected, $Actual, [Parameter(Mandatory = $true)][string]$Message)
    if ($Expected -cne $Actual) {
        throw "$Message (expected '$Expected', actual '$Actual')"
    }
}

function Assert-True {
    param([bool]$Condition, [Parameter(Mandatory = $true)][string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Throws {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string]$MessagePattern,
        [Parameter(Mandatory = $true)][string]$Message
    )
    $failure = $null
    try { & $Action } catch { $failure = $_ }
    if ($null -eq $failure) { throw "$Message (no failure was raised)" }
    if ($failure.Exception.Message -notmatch $MessagePattern) {
        throw "$Message (unexpected failure '$($failure.Exception.Message)')"
    }
}

function Get-ThrownFailure {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string]$MessagePattern,
        [Parameter(Mandatory = $true)][string]$Message
    )
    $failure = $null
    try { & $Action } catch { $failure = $_ }
    if ($null -eq $failure) { throw "$Message (no failure was raised)" }
    if ($failure.Exception.Message -notmatch $MessagePattern) {
        throw "$Message (unexpected failure '$($failure.Exception.Message)')"
    }
    return $failure
}

function Assert-StructuredReadinessFailure {
    param(
        [Parameter(Mandatory = $true)]$Failure,
        [Parameter(Mandatory = $true)][string]$EvidenceKey,
        [Parameter(Mandatory = $true)][string]$Label
    )
    Assert-True ($Failure.Exception.Data.Contains($EvidenceKey)) "$Label omitted structured readiness evidence"
    $evidence = $Failure.Exception.Data[$EvidenceKey]
    Assert-Equal failed $evidence.readiness_status "$Label readiness status"
    Assert-True (-not [string]::IsNullOrWhiteSpace([string]$evidence.failure)) "$Label failure detail"
    Assert-True ($null -eq $evidence.online_document) "$Label unexpectedly retained an online document"
    return $evidence
}

function Invoke-TestCase {
    param([Parameter(Mandatory = $true)][string]$Name, [Parameter(Mandatory = $true)][scriptblock]$Body)
    try {
        & $Body
        Write-Output "PASS $Name"
    } catch {
        $script:failures.Add("${Name}: $($_.Exception.Message)")
        Write-Output "FAIL $Name`: $($_.Exception.Message)"
    }
}

function Get-FunctionAst {
    param(
        [Parameter(Mandatory = $true)][System.Management.Automation.Language.Ast]$Ast,
        [Parameter(Mandatory = $true)][string]$Name
    )
    $matches = @($Ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -ceq $Name
    }, $true))
    if ($matches.Count -ne 1) {
        throw "expected exactly one function '$Name', found $($matches.Count)"
    }
    return $matches[0]
}

function New-DaemonDocument {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [Parameter(Mandatory = $true)][string]$State,
        [string]$Phase = $State,
        [string]$InstanceId = '019f8b42-8e29-7c2d-9d6f-9f48c593b9d1',
        $ProcessId = [int]4242,
        [Parameter(Mandatory = $true)][string]$ExecutablePath
    )
    return [pscustomobject]@{
        schema_version = '1'
        command = $Command
        data = [pscustomobject]@{
            status = [pscustomobject]@{
                state = $State
                instance = [pscustomobject]@{
                    instance_id = $InstanceId
                    pid = $ProcessId
                    phase = $Phase
                    executable_path = $ExecutablePath
                }
            }
        }
    }
}

function Set-EmptyFile {
    param([Parameter(Mandatory = $true)][string]$Path)
    [System.IO.File]::WriteAllBytes($Path, [byte[]]::new(0))
}

$tokens = $null
$parseErrors = $null
$stressAst = [System.Management.Automation.Language.Parser]::ParseFile(
    $stressPath,
    [ref]$tokens,
    [ref]$parseErrors
)
if ($parseErrors.Count -ne 0) {
    throw "stress harness has parser errors: $($parseErrors.Message -join '; ')"
}

$requiredStressFunctions = @(
    'New-IsolatedEnvironment',
    'Get-InspectionCount',
    'Get-AttributedInspectionSnapshot',
    'Assert-LatencyInspectionMarkers',
    'Assert-CorrectnessInspectionMarkers',
    'ConvertTo-NormalizedProcessCreationUtc',
    'ConvertTo-NormalizedExecutablePath',
    'Assert-HarnessDeadlineContract',
    'Get-MonotonicElapsedCeilingMs',
    'Get-BoundedPhaseWaitMs',
    'ConvertTo-StressDaemonDocumentIdentity',
    'Assert-StressDaemonReadinessDeadline',
    'Wait-MainDaemonReadiness',
    'Assert-AuditDaemonReadinessEvidence',
    'Assert-StatusJson',
    'Get-ProcessGenerationObservation',
    'Get-ProcessLivenessObservation',
    'Test-JsonElementStructuralEquality',
    'Assert-EquivalentJson',
    'Assert-LatencySourcePreparationEvidence',
    'Complete-FailedHarnessProcess',
    'Start-HarnessProcess',
    'Wait-HarnessProcess',
    'Invoke-HarnessProcess',
    'Write-ProcessAuditChildScript'
)
$availableStressFunctions = @($stressAst.FindAll({
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
}, $true) | ForEach-Object Name)

Invoke-TestCase 'stress harness exposes the marker phase helpers' {
    foreach ($name in $requiredStressFunctions) {
        Assert-True ($availableStressFunctions -ccontains $name) "stress harness is missing function $name"
    }
}

Invoke-TestCase 'latency source fixtures are fully prepared before daemon timing' {
    $stressText = $stressAst.Extent.Text
    $seedPattern = 'New-LegacyWorkspace\s+-Index\s+\$index\s+-Root\s+\$workspaceRoot'
    $seedMatches = [regex]::Matches($stressText, $seedPattern)
    Assert-Equal 1 $seedMatches.Count `
        'latency fixture creation must have one exact pre-timing call site'
    $verifiedCleanOffset = $stressText.IndexOf(
        '$summary.source_identity.verification_status = ''verified_clean''',
        [System.StringComparison]::Ordinal
    )
    $timingSelfTestOffset = $stressText.IndexOf(
        '$summary.measurement_diagnostics.timing_self_test =',
        [System.StringComparison]::Ordinal
    )
    $daemonStartOffset = $stressText.IndexOf(
        '$started = Invoke-Colay -Repository',
        [System.StringComparison]::Ordinal
    )
    Assert-True ($verifiedCleanOffset -ge 0) 'verified-clean statement was not found'
    Assert-True ($timingSelfTestOffset -ge 0) 'timing self-test statement was not found'
    Assert-True ($daemonStartOffset -ge 0) 'main daemon start statement was not found'
    Assert-True (
        $verifiedCleanOffset -lt $seedMatches[0].Index -and
        $seedMatches[0].Index -lt $timingSelfTestOffset -and
        $timingSelfTestOffset -lt $daemonStartOffset
    ) 'latency fixture ordering is not verified-clean, seed, timing self-test, daemon start'
    Assert-True ($stressText -match '\$latencySeeds\s*=\s*@\{\}') `
        'latency fixture preparation does not retain an exact seed map'
    Assert-True ($stressText -match 'foreach\s*\(\$index\s+in\s+1\.\.9\)') `
        'latency fixture preparation does not cover all nine sources before timing'
    $retainedSeedPattern = '\$seed\s*=\s*\$latencySeeds\[\$index\]'
    Assert-Equal 2 ([regex]::Matches($stressText, $retainedSeedPattern).Count) `
        'serial and concurrent registration must each consume the retained seed map exactly once'
    Assert-True ([regex]::IsMatch(
            $stressText,
            'for\s*\(\$index\s*=\s*1;\s*\$index\s*-le\s*5;\s*\$index\+\+\)\s*\{\s*\$seed\s*=\s*\$latencySeeds\[\$index\]',
            [System.Text.RegularExpressions.RegexOptions]::Singleline
        )) 'serial registration does not consume retained seed indexes 1 through 5'
    Assert-True ([regex]::IsMatch(
            $stressText,
            'foreach\s*\(\$index\s+in\s+6\.\.9\)\s*\{\s*\$seed\s*=\s*\$latencySeeds\[\$index\]',
            [System.Text.RegularExpressions.RegexOptions]::Singleline
        )) 'concurrent registration does not consume retained seed indexes 6 through 9'
    Assert-True ($stressText -match 'Assert-LatencySourcePreparationEvidence\s+-Seeds\s+\$latencySeeds') `
        'latency fixture preparation does not validate retained sources and command evidence'
    Assert-True ($stressText -match 'latency_source_preparation\s*=') `
        'latency fixture preparation evidence is not published'
    Assert-True ($stressText -match 'timing_included_in_latency_thresholds\s*=\s*\$false') `
        'latency fixture preparation evidence does not explicitly exclude setup timing'
}

Invoke-TestCase 'failure cleanup residue checks use exit-aware fail-closed observers' {
    $selfTestText = (Get-FunctionAst -Ast $stressAst `
            -Name 'Invoke-HarnessFailureCleanupSelfTest').Extent.Text
    Assert-Equal 0 ([regex]::Matches($selfTestText, 'Get-Process\s+-Id').Count) `
        'failure cleanup self-test still uses raw process-null liveness checks'
    Assert-Equal 2 ([regex]::Matches($selfTestText, 'Get-ProcessLivenessObservation').Count) `
        'failure cleanup self-test does not use the shared liveness observer at both sites'

    $batchText = (Get-FunctionAst -Ast $stressAst -Name 'Start-OwnedHarnessProcessBatch').Extent.Text
    Assert-True ($batchText -notmatch 'GetProcessById') `
        'batch rollback still uses a fail-open direct process identity read'
    Assert-True ($batchText -match 'Get-ProcessGenerationObservation') `
        'batch rollback does not use the shared generation observer'
    Assert-True ([regex]::IsMatch(
            $batchText,
            'process_exists\s+-and\s*-not\s+\$generationObservation\.identity_verified',
            [System.Text.RegularExpressions.RegexOptions]::Singleline
        )) 'batch rollback does not fail closed on a live unverified process'
}

foreach ($name in $requiredStressFunctions) {
    if ($availableStressFunctions -ccontains $name) {
        . ([scriptblock]::Create((Get-FunctionAst -Ast $stressAst -Name $name).Extent.Text))
    }
}

if ($availableStressFunctions -ccontains 'Get-ProcessGenerationObservation') {
    Invoke-TestCase 'process generation observation separates exited empty-path candidates from live ambiguity' {
        function Get-Process {
            return $script:processObservationCandidate
        }
        $expectedStartedAt = [datetime]::UtcNow
        $expectedExecutable = [System.IO.Path]::GetFullPath((Join-Path $PSHOME 'pwsh.exe'))

        $script:processObservationCandidate = [pscustomobject]@{
            HasExited = $true
            StartTime = $null
            Path = $null
        }
        $script:processObservationCandidate | Add-Member -MemberType ScriptMethod `
            -Name Dispose -Value { }
        $exited = Get-ProcessGenerationObservation -ProcessId 4242 `
            -ExpectedCreationTimeUtc $expectedStartedAt -ExpectedExecutablePath $expectedExecutable
        Assert-True (-not $exited.process_exists) `
            'exited empty-path candidate was reported as a live process'
        Assert-True (-not $exited.identity_verified) `
            'exited empty-path candidate was reported as identity-verified'
        Assert-True ($null -eq $exited.observation_error) `
            'exited empty-path candidate retained an observation error'
        $exitedLiveness = Get-ProcessLivenessObservation -ProcessId 4242
        Assert-True (-not $exitedLiveness.process_exists) `
            'liveness observer reported an exited candidate as live'
        Assert-True ($null -eq $exitedLiveness.observation_error) `
            'liveness observer retained an error for an exited candidate'

        $script:processObservationCandidate = [pscustomobject]@{
            HasExited = $false
            StartTime = $expectedStartedAt.ToLocalTime()
            Path = $null
        }
        $script:processObservationCandidate | Add-Member -MemberType ScriptMethod `
            -Name Dispose -Value { }
        $ambiguous = Get-ProcessGenerationObservation -ProcessId 4242 `
            -ExpectedCreationTimeUtc $expectedStartedAt -ExpectedExecutablePath $expectedExecutable
        Assert-True $ambiguous.process_exists 'live empty-path candidate was reported as absent'
        Assert-True (-not $ambiguous.identity_verified) `
            'live empty-path candidate was reported as identity-verified'
        Assert-True (-not [string]::IsNullOrWhiteSpace([string]$ambiguous.observation_error)) `
            'live empty-path candidate did not fail closed with an observation error'
        $liveLiveness = Get-ProcessLivenessObservation -ProcessId 4242
        Assert-True $liveLiveness.process_exists `
            'liveness observer reported a live candidate as absent'
        Assert-True ($null -eq $liveLiveness.observation_error) `
            'liveness observer reported an unexpected error for a readable live candidate'
    }
}

if ($availableStressFunctions -ccontains 'Assert-LatencySourcePreparationEvidence') {
    Invoke-TestCase 'latency source preparation evidence fails closed on labels and command state' {
        $newFixture = {
            $seeds = @{}
            $commands = @()
            foreach ($index in 1..9) {
                $seeds[$index] = [pscustomobject]@{ index = [int]$index }
                $commands += [pscustomobject]@{
                    label = "seed-schema-v8-$index"
                    measurement_method = 'os-process-lifetime'
                    exit_code = [int]0
                    timed_out = $false
                    elapsed_ms = [int64](10 * $index)
                }
            }
            return [pscustomobject]@{ seeds = $seeds; commands = $commands }
        }

        $valid = & $newFixture
        $evidence = Assert-LatencySourcePreparationEvidence -Seeds $valid.seeds `
            -CommandEvidence $valid.commands
        Assert-Equal 9 $evidence.source_count 'latency source preparation source count'
        Assert-Equal 10 $evidence.minimum_ms 'latency source preparation minimum'
        Assert-Equal 50 $evidence.median_ms 'latency source preparation median'
        Assert-Equal 90 $evidence.maximum_ms 'latency source preparation maximum'
        Assert-True $evidence.completed_before_timing_self_test `
            'latency source preparation did not record timing self-test exclusion'
        Assert-True (-not $evidence.timing_included_in_latency_thresholds) `
            'latency source preparation included fixture writes in product latency'

        $reordered = & $newFixture
        [array]::Reverse($reordered.commands)
        Assert-Throws -Action {
            Assert-LatencySourcePreparationEvidence -Seeds $reordered.seeds `
                -CommandEvidence $reordered.commands
        } -MessagePattern 'label mismatch' -Message 'latency source preparation accepted reordered labels'

        $missingSeed = & $newFixture
        [void]$missingSeed.seeds.Remove(9)
        Assert-Throws -Action {
            Assert-LatencySourcePreparationEvidence -Seeds $missingSeed.seeds `
                -CommandEvidence $missingSeed.commands
        } -MessagePattern 'retained 8 sources' -Message 'latency source preparation accepted a missing seed'

        $wrongIndex = & $newFixture
        $wrongIndex.seeds[9].index = '9'
        Assert-Throws -Action {
            Assert-LatencySourcePreparationEvidence -Seeds $wrongIndex.seeds `
                -CommandEvidence $wrongIndex.commands
        } -MessagePattern 'invalid source at index 9' `
            -Message 'latency source preparation accepted a non-integral seed index'

        foreach ($case in @(
                [pscustomobject]@{ property = 'measurement_method'; value = 'wall-clock'; pattern = 'lacks OS process lifetime' },
                [pscustomobject]@{ property = 'exit_code'; value = '0'; pattern = 'did not exit successfully' },
                [pscustomobject]@{ property = 'timed_out'; value = $true; pattern = 'invalid timeout evidence' },
                [pscustomobject]@{ property = 'timed_out'; value = 'false'; pattern = 'invalid timeout evidence' },
                [pscustomobject]@{ property = 'elapsed_ms'; value = -1; pattern = 'invalid elapsed time evidence' },
                [pscustomobject]@{ property = 'elapsed_ms'; value = '10'; pattern = 'invalid elapsed time evidence' }
            )) {
            $invalid = & $newFixture
            $invalid.commands[0].($case.property) = $case.value
            Assert-Throws -Action {
                Assert-LatencySourcePreparationEvidence -Seeds $invalid.seeds `
                    -CommandEvidence $invalid.commands
            } -MessagePattern $case.pattern `
                -Message "latency source preparation accepted invalid $($case.property)"
        }
    }
}

if ($availableStressFunctions -ccontains 'Assert-EquivalentJson') {
    Invoke-TestCase 'process audit PID multiset comparison ignores map insertion order and rejects mismatches' {
        $starts = [ordered]@{}
        $starts['30472'] = 1
        $starts['16344'] = 2
        $starts['16164'] = 1
        $exits = [ordered]@{}
        $exits['16164'] = 1
        $exits['16344'] = 2
        $exits['30472'] = 1
        Assert-True (($starts | ConvertTo-Json -Compress) -cne ($exits | ConvertTo-Json -Compress)) `
            'PID multiset fixture did not preserve its deliberate JSON property-order difference'
        Assert-EquivalentJson -Expected $starts -Actual $exits `
            -Label 'reordered fixture'

        $wrongCount = [ordered]@{}
        $wrongCount['16164'] = 1
        $wrongCount['16344'] = 1
        $wrongCount['30472'] = 1
        Assert-Throws -Action {
            Assert-EquivalentJson -Expected $starts -Actual $wrongCount `
                -Label 'count mismatch fixture'
        } -MessagePattern 'count mismatch fixture changed' `
            -Message 'process audit PID multiset accepted a changed occurrence count'

        $wrongPid = [ordered]@{}
        $wrongPid['16164'] = 1
        $wrongPid['16344'] = 2
        $wrongPid['99999'] = 1
        Assert-Throws -Action {
            Assert-EquivalentJson -Expected $starts -Actual $wrongPid `
                -Label 'PID mismatch fixture'
        } -MessagePattern 'PID mismatch fixture changed' `
            -Message 'process audit PID multiset accepted a changed PID key'

        foreach ($invalidCount in @('1', $true, [double]1.5)) {
            $wrongType = [ordered]@{}
            $wrongType['16164'] = 1
            $wrongType['16344'] = 2
            $wrongType['30472'] = $invalidCount
            Assert-Throws -Action {
                Assert-EquivalentJson -Expected $starts -Actual $wrongType `
                    -Label 'PID count type fixture'
            } -MessagePattern 'PID count type fixture changed' `
                -Message "process audit PID multiset accepted count value '$invalidCount'"
        }
    }
}

if ($availableStressFunctions -ccontains 'Assert-EquivalentJson') {
    Invoke-TestCase 'JSON equivalence ignores object order but preserves arrays and scalar types' {
        $equivalenceText = (Get-FunctionAst -Ast $stressAst -Name 'Assert-EquivalentJson').Extent.Text
        $structuralText = (Get-FunctionAst -Ast $stressAst `
                -Name 'Test-JsonElementStructuralEquality').Extent.Text
        Assert-True (($equivalenceText + $structuralText) -notmatch 'JsonNode.*DeepEquals') `
            'JSON equivalence uses JsonNode.DeepEquals, which is unavailable on PowerShell 7.2/.NET 6'
        $expectedFamily = [ordered]@{
            '' = [ordered]@{ bytes = 4096; sha256 = ('a' * 64) }
            '-wal' = [ordered]@{ bytes = 0; sha256 = ('b' * 64) }
        }
        $reorderedFamily = [ordered]@{
            '-wal' = [ordered]@{ sha256 = ('b' * 64); bytes = 0 }
            '' = [ordered]@{ sha256 = ('a' * 64); bytes = 4096 }
        }
        Assert-EquivalentJson -Expected $expectedFamily -Actual $reorderedFamily `
            -Label 'reordered object fixture'

        $orderedArray = @(1, 2)
        $reorderedArray = @(2, 1)
        Assert-Throws -Action {
            Assert-EquivalentJson -Expected $orderedArray -Actual $reorderedArray `
                -Label 'array order fixture'
        } -MessagePattern 'array order fixture changed' `
            -Message 'JSON equivalence ignored array order'

        $singletonArray = @('PATH')
        Assert-Throws -Action {
            Assert-EquivalentJson -Expected $singletonArray -Actual 'PATH' `
                -Label 'singleton array fixture'
        } -MessagePattern 'singleton array fixture changed' `
            -Message 'JSON equivalence collapsed a singleton array to its scalar'

        foreach ($scalarCase in @(
                [pscustomobject]@{ expected = 4096; actual = '4096'; label = 'number type fixture' },
                [pscustomobject]@{ expected = $true; actual = 'true'; label = 'boolean type fixture' }
            )) {
            Assert-Throws -Action {
                Assert-EquivalentJson -Expected $scalarCase.expected -Actual $scalarCase.actual `
                    -Label $scalarCase.label
            } -MessagePattern ([regex]::Escape("$($scalarCase.label) changed")) `
                -Message "JSON equivalence ignored $($scalarCase.label)"
        }

        $changedHash = [ordered]@{
            '' = [ordered]@{ bytes = 4096; sha256 = ('c' * 64) }
            '-wal' = [ordered]@{ bytes = 0; sha256 = ('b' * 64) }
        }
        Assert-Throws -Action {
            Assert-EquivalentJson -Expected $expectedFamily -Actual $changedHash `
                -Label 'hash fixture'
        } -MessagePattern 'hash fixture changed' `
            -Message 'JSON equivalence ignored a changed source hash'

        $missingSuffix = [ordered]@{
            '' = [ordered]@{ bytes = 4096; sha256 = ('a' * 64) }
        }
        Assert-Throws -Action {
            Assert-EquivalentJson -Expected $expectedFamily -Actual $missingSuffix `
                -Label 'suffix fixture'
        } -MessagePattern 'suffix fixture changed' `
            -Message 'JSON equivalence ignored a missing SQLite suffix'
    }
}

$providerKeyNames = @('OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'GOOGLE_API_KEY', 'AGY_API_KEY', 'CODEX_API_KEY', 'CLAUDE_API_KEY')

New-Item -ItemType Directory -Path $tempRoot -ErrorAction Stop | Out-Null
try {
    $fakeProvider = Join-Path $tempRoot 'fake/colay-e2e-fake-provider.exe'
    $aggregateMarker = Join-Path $tempRoot 'latency/legacy-inspections.log'
    $attributedSentinel = Join-Path $tempRoot 'latency/legacy-inspection-groups'
    $latencyRoot = Join-Path $tempRoot 'latency'
    $latencyHome = Join-Path $latencyRoot 'colay-home'

    if ($availableStressFunctions -ccontains 'New-IsolatedEnvironment') {
        Invoke-TestCase 'marker phase is mandatory and rejects unknown modes' {
            Assert-Throws -Action {
                New-IsolatedEnvironment -ColayHomePath $latencyHome -Root $latencyRoot `
                    -FakeProvider $fakeProvider -InspectionMarker $aggregateMarker `
                    -InspectionMarkerDirectory $attributedSentinel
            } -MessagePattern 'MarkerPhase|mandatory parameter' -Message 'environment accepted no marker phase'
            Assert-Throws -Action {
                New-IsolatedEnvironment -ColayHomePath $latencyHome -Root $latencyRoot `
                    -FakeProvider $fakeProvider -InspectionMarker $aggregateMarker `
                    -InspectionMarkerDirectory $attributedSentinel -MarkerPhase InvalidMode
            } -MessagePattern 'MarkerPhase|ValidateSet|valid values' -Message 'environment accepted an unknown marker phase'
            foreach ($wrongCase in @('latencyAttributedOff', 'correctnessAttributedOn')) {
                Assert-Throws -Action {
                    New-IsolatedEnvironment -ColayHomePath $latencyHome -Root $latencyRoot `
                        -FakeProvider $fakeProvider -InspectionMarker $aggregateMarker `
                        -InspectionMarkerDirectory $attributedSentinel -MarkerPhase $wrongCase
                } -MessagePattern 'MarkerPhase|ValidateSet|valid values' `
                    -Message "environment accepted wrong-case marker phase $wrongCase"
            }
        }

        Invoke-TestCase 'latency mode omits the attributed marker environment key' {
            $environment = New-IsolatedEnvironment -ColayHomePath $latencyHome -Root $latencyRoot `
                -FakeProvider $fakeProvider -InspectionMarker $aggregateMarker `
                -InspectionMarkerDirectory $attributedSentinel -MarkerPhase LatencyAttributedOff
            Assert-Equal $aggregateMarker $environment['COLAY_TEST_LEGACY_INSPECT_MARKER'] 'latency aggregate marker path'
            Assert-True (-not $environment.Contains('COLAY_TEST_LEGACY_INSPECT_MARKER_DIR')) `
                'latency environment included the attributed marker key'
        }

        Invoke-TestCase 'correctness mode includes the exact attributed marker directory' {
            $correctnessRoot = Join-Path $tempRoot 'correctness'
            $correctnessMarker = Join-Path $correctnessRoot 'legacy-inspections.log'
            $correctnessDirectory = Join-Path $correctnessRoot 'legacy-inspection-groups'
            $environment = New-IsolatedEnvironment -ColayHomePath (Join-Path $correctnessRoot 'colay-home') `
                -Root $correctnessRoot -FakeProvider $fakeProvider -InspectionMarker $correctnessMarker `
                -InspectionMarkerDirectory $correctnessDirectory -MarkerPhase CorrectnessAttributedOn
            Assert-Equal $correctnessMarker $environment['COLAY_TEST_LEGACY_INSPECT_MARKER'] 'correctness aggregate marker path'
            Assert-True ($environment.Contains('COLAY_TEST_LEGACY_INSPECT_MARKER_DIR')) `
                'correctness environment omitted the attributed marker key'
            Assert-Equal $correctnessDirectory $environment['COLAY_TEST_LEGACY_INSPECT_MARKER_DIR'] `
                'correctness attributed marker directory'
        }
    }

    if (@($requiredStressFunctions | Where-Object { $availableStressFunctions -cnotcontains $_ }).Count -eq 0) {
        Invoke-TestCase 'latency marker contract preserves cumulative 2/4/6/8/10 and final 18/0' {
            New-Item -ItemType Directory -Path $attributedSentinel -Force -ErrorAction Stop | Out-Null
            $initial = Assert-LatencyInspectionMarkers -AggregateMarker $aggregateMarker `
                -AttributedDirectory $attributedSentinel -ExpectedAggregateCount 0 -Label 'initial latency fixture'
            Assert-Equal 0 $initial.aggregate_count 'initial aggregate count'
            Assert-Equal 0 $initial.attributed_group_count 'initial attributed group count'
            foreach ($expected in @(2, 4, 6, 8, 10)) {
                [System.IO.File]::AppendAllLines(
                    $aggregateMarker,
                    [string[]]@('legacy-inspect', 'legacy-inspect')
                )
                $snapshot = Assert-LatencyInspectionMarkers -AggregateMarker $aggregateMarker `
                    -AttributedDirectory $attributedSentinel -ExpectedAggregateCount $expected `
                    -Label "serial aggregate $expected"
                Assert-Equal $expected $snapshot.aggregate_count "serial aggregate $expected"
                Assert-Equal 0 $snapshot.attributed_event_count "serial attributed events $expected"
            }
            [System.IO.File]::AppendAllLines($aggregateMarker, [string[]]@(
                'legacy-inspect', 'legacy-inspect', 'legacy-inspect', 'legacy-inspect',
                'legacy-inspect', 'legacy-inspect', 'legacy-inspect', 'legacy-inspect'
            ))
            $final = Assert-LatencyInspectionMarkers -AggregateMarker $aggregateMarker `
                -AttributedDirectory $attributedSentinel -ExpectedAggregateCount 18 -Label 'final latency fixture'
            Assert-Equal 18 $final.aggregate_count 'final aggregate count'
            Assert-Equal 0 $final.attributed_group_count 'final attributed group count'
            Assert-Equal 0 $final.attributed_event_count 'final attributed event count'
        }

        Invoke-TestCase 'correctness marker contract requires aggregate 2 and exact durable hash group' {
            $correctnessRoot = Join-Path $tempRoot 'correctness-fixture'
            $correctnessMarker = Join-Path $correctnessRoot 'legacy-inspections.log'
            $correctnessDirectory = Join-Path $correctnessRoot 'legacy-inspection-groups'
            $expectedGroup = '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
            $groupDirectory = Join-Path $correctnessDirectory $expectedGroup
            New-Item -ItemType Directory -Path $groupDirectory -Force -ErrorAction Stop | Out-Null
            [System.IO.File]::AppendAllLines(
                $correctnessMarker,
                [string[]]@('legacy-inspect', 'legacy-inspect')
            )
            Set-EmptyFile (Join-Path $groupDirectory 'event-100-1')
            Set-EmptyFile (Join-Path $groupDirectory 'event-100-2')
            $snapshot = Assert-CorrectnessInspectionMarkers -AggregateMarker $correctnessMarker `
                -AttributedDirectory $correctnessDirectory -ExpectedGroup $expectedGroup `
                -Label 'correctness fixture'
            Assert-Equal 2 $snapshot.aggregate_count 'correctness aggregate count'
            Assert-Equal 1 $snapshot.attributed_group_count 'correctness attributed group count'
            Assert-Equal 2 $snapshot.attributed_event_count 'correctness attributed event count'
            Assert-Equal $expectedGroup $snapshot.groups[0].group_id 'correctness durable group equality'
            Assert-Equal 2 (@($snapshot.groups[0].event_names | Sort-Object -Unique).Count) `
                'correctness distinct event names'
        }
    }

    Invoke-TestCase 'main and audit paths use explicit phase modes and schema-2 evidence' {
        $environmentCalls = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.CommandAst] -and
                $node.GetCommandName() -ceq 'New-IsolatedEnvironment'
        }, $true))
        Assert-Equal 2 $environmentCalls.Count 'isolated environment call count'
        Assert-True ($environmentCalls[0].Extent.Text -match '-MarkerPhase\s+LatencyAttributedOff') `
            'main environment does not select latency-attributed-off mode'
        Assert-True ($environmentCalls[1].Extent.Text -match '-MarkerPhase\s+CorrectnessAttributedOn') `
            'audit environment does not select correctness-attributed-on mode'

        $summaryAssignments = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                $node.Left.Extent.Text -ceq '$summary'
        }, $true))
        Assert-Equal 1 $summaryAssignments.Count 'summary assignment count'
        $summaryText = $summaryAssignments[0].Right.Extent.Text
        Assert-True ($summaryText -match 'schema_version\s*=\s*2(?:\D|$)') 'stress evidence schema is not 2'
        Assert-True ($summaryText -match "marker_phase_policy\s*=\s*'split-latency-marker-off-and-correctness-marker-on-phases'") `
            'stress evidence omitted the reviewed split policy'
        Assert-True ($summaryText -match 'latency_phase\s*=') 'stress evidence omitted latency marker phase'
        Assert-True ($summaryText -match 'correctness_phase\s*=') 'stress evidence omitted correctness marker phase'
        Assert-True ($summaryText -match 'main_daemon_readiness\s*=') `
            'measurement diagnostics omitted main daemon readiness evidence'

        $inspectionCountAssignments = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                $node.Left.Extent.Text -ceq '$summary.inspection_count'
        }, $true))
        Assert-Equal 1 $inspectionCountAssignments.Count 'inspection_count assignment count'
        Assert-Equal '$latencyMarkerState.aggregate_count' `
            $inspectionCountAssignments[0].Right.Extent.Text 'inspection_count latency aggregate source'

        $auditAssignments = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                $node.Left.Extent.Text -ceq '$summary.process_audit.functional'
        }, $true))
        Assert-Equal 1 $auditAssignments.Count 'process audit functional evidence assignment count'
        Assert-True ($auditAssignments[0].Right.Extent.Text -match 'timing_excluded_from_latency_thresholds\s*=\s*\$true') `
            'process audit timing is not explicitly excluded'
    }

    Invoke-TestCase 'main latency path asserts aggregate milestones and empty attributed sentinel' {
        $latencyCalls = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.CommandAst] -and
                $node.GetCommandName() -ceq 'Assert-LatencyInspectionMarkers'
        }, $true))
        Assert-Equal 3 $latencyCalls.Count 'latency marker assertion call count'
        $texts = @($latencyCalls | ForEach-Object { $_.Extent.Text })
        Assert-True (@($texts | Where-Object { $_ -match '-ExpectedAggregateCount\s+0(?:\D|$)' }).Count -eq 1) `
            'latency path omitted initial aggregate zero assertion'
        Assert-True (@($texts | Where-Object { $_ -match '-ExpectedAggregateCount\s+\(2\s*\*\s*\$index\)' }).Count -eq 1) `
            'latency path omitted cumulative serial aggregate assertion'
        Assert-True (@($texts | Where-Object { $_ -match '-ExpectedAggregateCount\s+18(?:\D|$)' }).Count -eq 1) `
            'latency path omitted final aggregate 18 assertion'
    }

    Invoke-TestCase 'main daemon reaches identity-stable online before any timed registration' {
        $mainReadinessCalls = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.CommandAst] -and
                $node.GetCommandName() -ceq 'Wait-MainDaemonReadiness'
        }, $true))
        Assert-Equal 1 $mainReadinessCalls.Count 'main readiness call count'
        $serialCalls = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.CommandAst] -and
                $node.GetCommandName() -ceq 'Invoke-Colay' -and
                $node.Extent.Text -match 'serial-register-'
        }, $true))
        Assert-Equal 1 $serialCalls.Count 'serial registration call-site count'
        Assert-True ($mainReadinessCalls[0].Extent.EndOffset -lt $serialCalls[0].Extent.StartOffset) `
            'timed serial registration can precede main daemon readiness'
        $mainWait = Get-FunctionAst -Ast $stressAst -Name 'Wait-MainDaemonReadiness'
        $mainWaitText = $mainWait.Extent.Text
        Assert-True ($mainWaitText -match "-ArgumentValues\s+@\('--json',\s*'daemon',\s*'status'\)") `
            'main readiness does not use exact separated status arguments'
        foreach ($forbidden in @('Get-CimInstance', 'OpenProcess', 'Invoke-Sqlite', 'New-LegacyWorkspace')) {
            Assert-True ($mainWaitText -notmatch [regex]::Escape($forbidden)) `
                "main readiness contains forbidden operation $forbidden"
        }
    }

    $requiredMainReadinessFunctions = @(
        'ConvertTo-StressDaemonDocumentIdentity',
        'Assert-StressDaemonReadinessDeadline',
        'Wait-MainDaemonReadiness'
    )
    $mainReadinessReady = @($requiredMainReadinessFunctions | Where-Object {
        $availableStressFunctions -cnotcontains $_
    }).Count -eq 0
    if ($mainReadinessReady) {
        $expectedMainColay = Join-Path $tempRoot 'main-bin/colay.exe'
        $mainRepository = Join-Path $tempRoot 'main-repository'
        New-Item -ItemType Directory -Path $mainRepository -Force | Out-Null
        $script:ResolvedColay = $expectedMainColay
        $script:MainDaemonReadinessTimeoutMs = 250
        $script:MainDaemonReadinessPollIntervalMs = 1
        $script:MainDaemonReadinessExitWaitLimitMs = 15
        $script:MainDaemonReadinessOutputDrainLimitMs = 5
        $script:MainDaemonReadinessInitialParseDelayForTestMs = 0
        $script:mainReadinessDocuments = [System.Collections.Generic.Queue[object]]::new()
        $script:mainReadinessCalls = [System.Collections.Generic.List[object]]::new()
        $script:mainPortablePowerShell = [System.IO.Path]::GetFullPath((Join-Path $PSHOME 'pwsh.exe'))
        $script:mainHangMarker = Join-Path $tempRoot 'main-readiness-hang.pid'
        $script:CommandEvidence = [System.Collections.Generic.List[object]]::new()
        $script:ProcessSetupFailureEvidence = [System.Collections.Generic.List[object]]::new()
        $script:ProcessSetupFailureForTest = $null
        $script:ProcessExitTimeFailureForTest = $false
        $script:ProcessFinalizeFailureForTest = $null
        $script:HarnessProcessIdentity = [pscustomobject]@{ identity_key = 'focused-test-parent' }
        $script:processObservationCalls = 0

        function Assert-FreeDisk { return @() }
        function Register-OwnedProcessIdentity {
            param(
                [int]$ProcessId,
                [int]$ParentProcessId,
                [datetime]$CreationTimeUtc,
                [string]$ExecutablePath,
                [string]$Name,
                [string]$Source,
                $ParentIdentity,
                [string]$Label
            )
            return [pscustomobject]@{
                identity_key = "$ProcessId`:$($CreationTimeUtc.ToFileTimeUtc())"
                process_id = $ProcessId
                creation_time_utc = $CreationTimeUtc
                executable_path = $ExecutablePath
            }
        }
        function Set-OwnedProcessIdentityExit { param($Identity, [datetime]$ExitTimeUtc) }
        function Update-ProcessObservation { $script:processObservationCalls++ }

        function Invoke-Colay {
            param(
                [string]$Repository,
                [string[]]$ArgumentValues,
                [System.Collections.IDictionary]$Environment,
                [string]$Label,
                [int]$TimeoutMs = 12000,
                [switch]$AllowFailure,
                [System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
                [int]$OverallDeadlineMs = 0,
                [int]$ExitWaitLimitMs = 5000,
                [int]$OutputDrainLimitMs = 2000,
                [switch]$DeferObservation
            )
            $script:mainReadinessCalls.Add([pscustomobject]@{
                repository = $Repository
                arguments = @($ArgumentValues)
                label = $Label
                timeout_ms = $TimeoutMs
                overall_deadline_ms = $OverallDeadlineMs
                exit_wait_limit_ms = $ExitWaitLimitMs
                output_drain_limit_ms = $OutputDrainLimitMs
                observer_deferred = [bool]$DeferObservation
            })
            if ($script:mainReadinessDocuments.Count -eq 0) {
                $document = New-DaemonDocument -Command daemon_status -State booting `
                    -ExecutablePath $expectedMainColay
            } else {
                $next = $script:mainReadinessDocuments.Dequeue()
                if ($next -is [System.Exception]) { throw $next }
                if ($next.PSObject.Properties.Name -contains 'hang_command' -and [bool]$next.hang_command) {
                    $escapedMarker = $script:mainHangMarker.Replace("'", "''")
                    $hangCommand = "[System.IO.File]::WriteAllText('$escapedMarker', [string]`$PID); Start-Sleep -Seconds 30"
                    return Invoke-HarnessProcess -Executable $script:mainPortablePowerShell `
                        -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $hangCommand) `
                        -WorkingDirectory $Repository -Environment $Environment -Label $Label `
                        -TimeoutMs $TimeoutMs -StandardInputText $null -CaptureFirstStdoutLine `
                        -DeferObservation:$DeferObservation -OverallDeadlineStopwatch $OverallDeadlineStopwatch `
                        -OverallDeadlineMs $OverallDeadlineMs -ExitWaitLimitMs $ExitWaitLimitMs `
                        -OutputDrainLimitMs $OutputDrainLimitMs
                }
                if ($next.PSObject.Properties.Name -contains 'delay_ms') {
                    Start-Sleep -Milliseconds ([int]$next.delay_ms)
                    $document = $next.document
                } else {
                    $document = $next
                }
            }
            return [pscustomobject]@{
                label = $Label
                stdout = ($document | ConvertTo-Json -Compress -Depth 30)
                stderr = ''
                exit_code = 0
                deadline = [pscustomobject]@{
                    remaining_at_launch_ms = [Math]::Max(1, $OverallDeadlineMs)
                    command_timeout_ms = $TimeoutMs
                    exit_wait_limit_ms = $ExitWaitLimitMs
                    output_drain_limit_ms = $OutputDrainLimitMs
                    total_operation_budget_ms = $TimeoutMs + $ExitWaitLimitMs + $OutputDrainLimitMs
                }
            }
        }

        Invoke-TestCase 'main readiness accepts exact immediate online without polling' {
            $script:mainReadinessCalls.Clear()
            $start = New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedMainColay
            $result = Wait-MainDaemonReadiness -DaemonStartDocument $start -ExpectedExecutable $expectedMainColay `
                -Repository $mainRepository -Environment ([ordered]@{}) -Label 'main-immediate'
            Assert-Equal online $result.Evidence.readiness_status 'main immediate readiness status'
            Assert-Equal 0 $result.Evidence.poll_count 'main immediate readiness poll count'
            Assert-Equal 0 $script:mainReadinessCalls.Count 'main immediate status command count'
            Assert-True (-not [bool]$result.Evidence.timing_included_in_latency_thresholds) `
                'main readiness was not explicitly excluded from latency thresholds'
        }

        Invoke-TestCase 'main readiness permits identity-stable booting probing online only' {
            $script:mainReadinessCalls.Clear()
            $script:mainReadinessDocuments.Clear()
            $script:mainReadinessDocuments.Enqueue((New-DaemonDocument -Command daemon_status -State probing `
                    -ExecutablePath $expectedMainColay))
            $script:mainReadinessDocuments.Enqueue((New-DaemonDocument -Command daemon_status -State online `
                    -ExecutablePath $expectedMainColay))
            $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedMainColay
            $result = Wait-MainDaemonReadiness -DaemonStartDocument $start -ExpectedExecutable $expectedMainColay `
                -Repository $mainRepository -Environment ([ordered]@{}) -Label 'main-delayed'
            Assert-Equal 'probing,online' (@($result.Evidence.polls | ForEach-Object state) -join ',') `
                'main readiness transition sequence'
            foreach ($call in $script:mainReadinessCalls) {
                Assert-Equal '--json,daemon,status' (@($call.arguments) -join ',') `
                    'main readiness invoked a non-status command before readiness'
                Assert-True $call.observer_deferred 'main readiness did not defer process observation'
            }
        }

        Invoke-TestCase 'main readiness rejects status-side malformed terminal and drift fixtures' {
            $fixtures = [System.Collections.Generic.List[object]]::new()
            $wrongSchema = New-DaemonDocument -Command daemon_status -State online -ExecutablePath $expectedMainColay
            $wrongSchema.schema_version = '2'
            $fixtures.Add($wrongSchema)
            $fixtures.Add((New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedMainColay))
            $fixtures.Add((New-DaemonDocument -Command daemon_status -State online `
                    -InstanceId '019F8B42-8E29-7C2D-9D6F-9F48C593B9D1' -ExecutablePath $expectedMainColay))
            $fixtures.Add((New-DaemonDocument -Command daemon_status -State online -ProcessId 4242.5 `
                    -ExecutablePath $expectedMainColay))
            $fixtures.Add((New-DaemonDocument -Command daemon_status -State online `
                    -ExecutablePath (Join-Path $tempRoot 'wrong-main/colay.exe')))
            $fixtures.Add((New-DaemonDocument -Command daemon_status -State probing -Phase booting `
                    -ExecutablePath $expectedMainColay))
            $fixtures.Add((New-DaemonDocument -Command daemon_status -State failed -ExecutablePath $expectedMainColay))
            $fixtures.Add((New-DaemonDocument -Command daemon_status -State mystery -ExecutablePath $expectedMainColay))
            foreach ($fixture in $fixtures) {
                $script:mainReadinessCalls.Clear()
                $script:mainReadinessDocuments.Clear()
                $script:mainReadinessDocuments.Enqueue($fixture)
                $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedMainColay
                $failure = Get-ThrownFailure -Action {
                    Wait-MainDaemonReadiness -DaemonStartDocument $start -ExpectedExecutable $expectedMainColay `
                        -Repository $mainRepository -Environment ([ordered]@{}) -Label 'main-invalid'
                } -MessagePattern 'schema-v1|canonical UUID|PID|path|state/phase|terminal|non-progress|identity drift' `
                    -Message 'main readiness accepted an invalid polled status'
                [void](Assert-StructuredReadinessFailure -Failure $failure `
                        -EvidenceKey 'ColayStressMainDaemonReadinessEvidence' -Label 'main invalid poll')
                Assert-Equal 1 $script:mainReadinessCalls.Count 'main invalid poll command count'
                Assert-Equal '--json,daemon,status' (@($script:mainReadinessCalls[0].arguments) -join ',') `
                    'main invalid poll reached a registration command'
            }
        }

        Invoke-TestCase 'main readiness rejects command failure and late online with structured evidence' {
            foreach ($fixture in @(
                    [System.InvalidOperationException]::new('injected daemon status command failure'),
                    [pscustomobject]@{
                        delay_ms = 120
                        document = New-DaemonDocument -Command daemon_status -State online `
                            -ExecutablePath $expectedMainColay
                    }
                )) {
                $script:mainReadinessCalls.Clear()
                $script:mainReadinessDocuments.Clear()
                $script:MainDaemonReadinessTimeoutMs = 100
                $script:MainDaemonReadinessExitWaitLimitMs = 15
                $script:MainDaemonReadinessOutputDrainLimitMs = 5
                $script:mainReadinessDocuments.Enqueue($fixture)
                $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedMainColay
                $failure = Get-ThrownFailure -Action {
                    Wait-MainDaemonReadiness -DaemonStartDocument $start -ExpectedExecutable $expectedMainColay `
                        -Repository $mainRepository -Environment ([ordered]@{}) -Label 'main-command-failure'
                } -MessagePattern 'failure|timed out after 100ms' `
                    -Message 'main readiness accepted command failure or late online'
                [void](Assert-StructuredReadinessFailure -Failure $failure `
                        -EvidenceKey 'ColayStressMainDaemonReadinessEvidence' -Label 'main command failure')
                Assert-Equal 1 $script:mainReadinessCalls.Count 'main command failure status call count'
            }
        }

        Invoke-TestCase 'main hanging status uses cleanup-inclusive generic deadline without residue' {
            if (Test-Path -LiteralPath $script:mainHangMarker) {
                Remove-Item -LiteralPath $script:mainHangMarker -Force
            }
            $script:mainReadinessCalls.Clear()
            $script:mainReadinessDocuments.Clear()
            $script:CommandEvidence.Clear()
            $script:MainDaemonReadinessTimeoutMs = 1200
            $script:MainDaemonReadinessPollIntervalMs = 1
            $script:MainDaemonReadinessExitWaitLimitMs = 150
            $script:MainDaemonReadinessOutputDrainLimitMs = 50
            $script:mainReadinessDocuments.Enqueue([pscustomobject]@{ hang_command = $true })
            $hangEnvironment = [ordered]@{
                SystemRoot = $env:SystemRoot
                WINDIR = $env:WINDIR
                TEMP = [System.IO.Path]::GetTempPath()
                TMP = [System.IO.Path]::GetTempPath()
                PATH = (Split-Path -Parent $script:mainPortablePowerShell)
            }
            $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedMainColay
            $wall = [System.Diagnostics.Stopwatch]::StartNew()
            $failure = Get-ThrownFailure -Action {
                Wait-MainDaemonReadiness -DaemonStartDocument $start -ExpectedExecutable $expectedMainColay `
                    -Repository $mainRepository -Environment $hangEnvironment -Label 'main-hang'
            } -MessagePattern 'exceeded|timed out' -Message 'main hanging status did not fail'
            $wall.Stop()
            $evidence = Assert-StructuredReadinessFailure -Failure $failure `
                -EvidenceKey 'ColayStressMainDaemonReadinessEvidence' -Label 'main hanging status'
            Assert-True ($wall.ElapsedMilliseconds -lt 1275) `
                "main hanging status exceeded overall deadline plus 75ms tolerance: $($wall.ElapsedMilliseconds)ms"
            Assert-Equal 1 $script:mainReadinessCalls.Count 'main hanging status command count'
            Assert-Equal '--json,daemon,status' (@($script:mainReadinessCalls[0].arguments) -join ',') `
                'main hanging status reached a registration command'
            Assert-Equal 1 $evidence.poll_count 'main hanging status evidence poll count'
            Assert-True (Test-Path -LiteralPath $script:mainHangMarker -PathType Leaf) `
                'main hanging status child did not publish its PID'
            $publishedPid = [int](Get-Content -LiteralPath $script:mainHangMarker -Raw).Trim()
            $commandRows = @($script:CommandEvidence | Where-Object { [string]$_.label -match '^main-hang-daemon-readiness-' })
            Assert-Equal 1 $commandRows.Count 'main hanging status command evidence count'
            $commandRow = $commandRows[0]
            Assert-Equal $publishedPid ([int]$commandRow.process_id) 'main hanging status command evidence PID'
            $cleanup = $commandRow.failure_cleanup
            Assert-True $cleanup.exit_confirmed 'main hanging status exit was not confirmed'
            Assert-True $cleanup.stdout_completed 'main hanging status stdout did not drain'
            Assert-True $cleanup.stderr_completed 'main hanging status stderr did not drain'
            Assert-True $cleanup.process_disposed 'main hanging status process was not disposed'
            Assert-Equal 0 @($cleanup.cleanup_errors).Count 'main hanging status cleanup error count'
            Assert-True ($cleanup.exit_wait_consumed_ms -le $cleanup.exit_wait_limit_ms) `
                'main hanging status reused its exit cleanup budget'
            Assert-True ($cleanup.output_drain_consumed_ms -le $cleanup.output_drain_limit_ms) `
                'main hanging status reused its output-drain cleanup budget'
            Assert-True (($commandRow.deadline.command_timeout_ms + $commandRow.deadline.exit_wait_limit_ms +
                    $commandRow.deadline.output_drain_limit_ms) -le $commandRow.deadline.remaining_at_launch_ms) `
                'main hanging status launch budget exceeded actual remaining time'
            $residue = Get-ProcessGenerationObservation -ProcessId $publishedPid `
                -ExpectedCreationTimeUtc ([datetime]$commandRow.process_started_at_utc) `
                -ExpectedExecutablePath $script:mainPortablePowerShell
            Assert-True (-not ($residue.process_exists -and -not $residue.identity_verified)) `
                "main hanging status could not verify process residue: $($residue.observation_error)"
            Assert-True (-not $residue.expected_generation_live) `
                'main hanging status left exact process residue'
            Remove-Item -LiteralPath $script:mainHangMarker -Force
        }

        Invoke-TestCase 'generic process runner preserves non-readiness default behavior' {
            $defaultEnvironment = [ordered]@{
                SystemRoot = $env:SystemRoot
                WINDIR = $env:WINDIR
                TEMP = [System.IO.Path]::GetTempPath()
                TMP = [System.IO.Path]::GetTempPath()
                PATH = (Split-Path -Parent $script:mainPortablePowerShell)
            }
            $script:CommandEvidence.Clear()
            $script:processObservationCalls = 0
            $result = Invoke-HarnessProcess -Executable $script:mainPortablePowerShell `
                -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', "'default-ok'") `
                -WorkingDirectory $mainRepository -Environment $defaultEnvironment `
                -Label 'generic-default-success' -TimeoutMs 5000 -StandardInputText $null
            Assert-Equal 0 $result.exit_code 'generic default success exit code'
            Assert-True ($null -eq $result.deadline) 'generic default success unexpectedly used deadline evidence'
            Assert-True (-not $result.observer_deferred) 'generic default success unexpectedly deferred observation'
            Assert-Equal os-process-lifetime $result.measurement_method `
                'generic default success changed OS-lifetime measurement'
            Assert-Equal 1 $script:processObservationCalls `
                'generic default success changed process-observation behavior'
            $script:processObservationCalls = 0
            $failure = Get-ThrownFailure -Action {
                Invoke-HarnessProcess -Executable $script:mainPortablePowerShell `
                    -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30') `
                    -WorkingDirectory $mainRepository -Environment $defaultEnvironment `
                    -Label 'generic-default-timeout' -TimeoutMs 50 -StandardInputText $null
            } -MessagePattern 'exceeded hard process timeout 50ms' `
                -Message 'generic default timeout path did not retain its requested limit'
            $timeoutRow = @($script:CommandEvidence | Where-Object label -CEQ 'generic-default-timeout')
            Assert-Equal 1 $timeoutRow.Count 'generic default timeout evidence count'
            Assert-True ($null -eq $timeoutRow[0].deadline) 'generic default timeout unexpectedly used deadline evidence'
            Assert-Equal 5000 $timeoutRow[0].failure_cleanup.exit_wait_limit_ms `
                'generic default exit cleanup limit changed'
            Assert-Equal 2000 $timeoutRow[0].failure_cleanup.output_drain_limit_ms `
                'generic default output drain limit changed'
            Assert-True (-not $timeoutRow[0].failure_cleanup.deadline_aware) `
                'generic default cleanup unexpectedly became deadline-aware'
            Assert-Equal 0 @($timeoutRow[0].failure_cleanup.cleanup_errors).Count `
                'generic default timeout cleanup error count'
            Assert-Equal 1 $script:processObservationCalls `
                'generic default timeout changed process-observation behavior'
            $waitFunctionText = (Get-FunctionAst -Ast $stressAst -Name 'Wait-HarnessProcess').Extent.Text
            Assert-True ($waitFunctionText -match '\.WaitForExit\(10\)') `
                'generic default wait polling is not exactly 10ms'
        }

        Invoke-TestCase 'deadline contract rejects partial cleanup limits before process launch' {
            Assert-Throws -Action {
                Assert-HarnessDeadlineContract -OverallDeadlineStopwatch $null -OverallDeadlineMs 0 `
                    -ExitWaitLimitMs 17 -OutputDrainLimitMs 19 -RequestedExecutionTimeoutMs 100
            } -MessagePattern 'atomic|partial|deadline contract' `
                -Message 'partial cleanup-only deadline contract was accepted'
            Assert-Throws -Action {
                Assert-HarnessDeadlineContract -OverallDeadlineStopwatch $null -OverallDeadlineMs 0 `
                    -ExitWaitLimitMs 5000 -OutputDrainLimitMs 2000 -RequestedExecutionTimeoutMs 100
            } -MessagePattern 'atomic|invalid|deadline contract' `
                -Message 'explicit null/default deadline quartet silently downgraded to non-deadline mode'
            $deadlineEnvironment = [ordered]@{
                SystemRoot = $env:SystemRoot
                WINDIR = $env:WINDIR
                TEMP = [System.IO.Path]::GetTempPath()
                TMP = [System.IO.Path]::GetTempPath()
                PATH = (Split-Path -Parent $script:mainPortablePowerShell)
            }
            $script:partialDeadlineRecord = $null
            $partialLaunchMarker = Join-Path $tempRoot 'partial-deadline-launch.marker'
            $escapedPartialLaunchMarker = $partialLaunchMarker.Replace("'", "''")
            $partialLaunchCommand = "[System.IO.File]::WriteAllText('$escapedPartialLaunchMarker', 'launched'); Start-Sleep -Seconds 30"
            $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
            try {
                Assert-Throws -Action {
                    $script:partialDeadlineRecord = Start-HarnessProcess `
                        -Executable $script:mainPortablePowerShell `
                        -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $partialLaunchCommand) `
                        -WorkingDirectory $mainRepository -Environment $deadlineEnvironment `
                        -Label 'partial-prelaunch-deadline' -StandardInputText $null `
                        -OverallDeadlineStopwatch $stopwatch -OverallDeadlineMs 10000 `
                        -RequestedExecutionTimeoutMs 1000 -DeferObservation
                } -MessagePattern 'atomic|partial|deadline contract' `
                    -Message 'partial stopwatch/overall deadline contract reached process launch'
            } finally {
                if ($null -ne $script:partialDeadlineRecord -and
                    $null -ne $script:partialDeadlineRecord.Process) {
                    try { $script:partialDeadlineRecord.Process.Kill($true) } catch { }
                    try { [void]$script:partialDeadlineRecord.Process.WaitForExit(1000) } catch { }
                    try { $script:partialDeadlineRecord.Process.Dispose() } catch { }
                    $script:partialDeadlineRecord.Process = $null
                }
            }
            Assert-True (-not (Test-Path -LiteralPath $partialLaunchMarker)) `
                'partial stopwatch/overall deadline contract launched a child before rejection'

            $nullDeadlineMarker = Join-Path $tempRoot 'null-deadline-launch.marker'
            $escapedNullDeadlineMarker = $nullDeadlineMarker.Replace("'", "''")
            $nullDeadlineCommand = "[System.IO.File]::WriteAllText('$escapedNullDeadlineMarker', 'launched'); Start-Sleep -Seconds 30"
            $script:nullDeadlineRecord = $null
            try {
                Assert-Throws -Action {
                    $script:nullDeadlineRecord = Start-HarnessProcess `
                        -Executable $script:mainPortablePowerShell `
                        -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $nullDeadlineCommand) `
                        -WorkingDirectory $mainRepository -Environment $deadlineEnvironment `
                        -Label 'null-prelaunch-deadline' -StandardInputText $null `
                        -OverallDeadlineStopwatch $null -OverallDeadlineMs 0 `
                        -ExitWaitLimitMs 5000 -OutputDrainLimitMs 2000 `
                        -RequestedExecutionTimeoutMs 100 -DeferObservation
                } -MessagePattern 'atomic|invalid|deadline contract' `
                    -Message 'explicit null/default deadline quartet reached process launch'
            } finally {
                if ($null -ne $script:nullDeadlineRecord -and
                    $null -ne $script:nullDeadlineRecord.Process) {
                    try { $script:nullDeadlineRecord.Process.Kill($true) } catch { }
                    try { [void]$script:nullDeadlineRecord.Process.WaitForExit(1000) } catch { }
                    try { $script:nullDeadlineRecord.Process.Dispose() } catch { }
                    $script:nullDeadlineRecord.Process = $null
                }
            }
            Assert-True (-not (Test-Path -LiteralPath $nullDeadlineMarker)) `
                'explicit null/default deadline quartet launched a child before rejection'

            $partialWrapperMarker = Join-Path $tempRoot 'partial-wrapper-launch.marker'
            $escapedPartialWrapperMarker = $partialWrapperMarker.Replace("'", "''")
            $partialWrapperCommand = "[System.IO.File]::WriteAllText('$escapedPartialWrapperMarker', 'launched'); Start-Sleep -Seconds 30"
            $wrapperStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
            Assert-Throws -Action {
                Invoke-HarnessProcess -Executable $script:mainPortablePowerShell `
                    -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $partialWrapperCommand) `
                    -WorkingDirectory $mainRepository -Environment $deadlineEnvironment `
                    -Label 'partial-wrapper-deadline' -TimeoutMs 1000 -StandardInputText $null `
                    -OverallDeadlineStopwatch $wrapperStopwatch -OverallDeadlineMs 10000 `
                    -DeferObservation
            } -MessagePattern 'atomic|partial|deadline contract' `
                -Message 'partial wrapper deadline contract reached process launch'
            Assert-True (-not (Test-Path -LiteralPath $partialWrapperMarker)) `
                'partial wrapper deadline contract launched a child before rejection'

            $nullWrapperMarker = Join-Path $tempRoot 'null-wrapper-launch.marker'
            $escapedNullWrapperMarker = $nullWrapperMarker.Replace("'", "''")
            $nullWrapperCommand = "[System.IO.File]::WriteAllText('$escapedNullWrapperMarker', 'launched'); Start-Sleep -Seconds 30"
            Assert-Throws -Action {
                Invoke-HarnessProcess -Executable $script:mainPortablePowerShell `
                    -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $nullWrapperCommand) `
                    -WorkingDirectory $mainRepository -Environment $deadlineEnvironment `
                    -Label 'null-wrapper-deadline' -TimeoutMs 100 -StandardInputText $null `
                    -OverallDeadlineStopwatch $null -OverallDeadlineMs 0 `
                    -ExitWaitLimitMs 5000 -OutputDrainLimitMs 2000 -DeferObservation
            } -MessagePattern 'atomic|invalid|deadline contract' `
                -Message 'explicit null/default wrapper deadline quartet reached process launch'
            Assert-True (-not (Test-Path -LiteralPath $nullWrapperMarker)) `
                'explicit null/default wrapper deadline quartet launched a child before rejection'
        }

        Invoke-TestCase 'deadline cleanup uses only launch-sealed absolute endpoints' {
            $startText = (Get-FunctionAst -Ast $stressAst -Name 'Start-HarnessProcess').Extent.Text
            $startContractOffset = $startText.IndexOf('$boundDeadlineParameterCount', [StringComparison]::Ordinal)
            $processStartOffset = $startText.IndexOf('$process.Start()', [StringComparison]::Ordinal)
            Assert-True ($startContractOffset -ge 0 -and $processStartOffset -gt $startContractOffset) `
                'parent atomic deadline validation does not dominate Process.Start'
            foreach ($wrapperContract in @(
                    [pscustomobject]@{ name = 'Invoke-HarnessProcess'; downstream = 'Start-HarnessProcess' },
                    [pscustomobject]@{ name = 'Invoke-Colay'; downstream = 'Invoke-HarnessProcess' }
                )) {
                $wrapperText = (Get-FunctionAst -Ast $stressAst -Name $wrapperContract.name).Extent.Text
                $wrapperContractOffset = $wrapperText.IndexOf(
                    '$boundDeadlineParameterCount',
                    [StringComparison]::Ordinal
                )
                $downstreamOffset = $wrapperText.IndexOf(
                    [string]$wrapperContract.downstream,
                    [StringComparison]::Ordinal
                )
                Assert-True ($wrapperContractOffset -ge 0 -and $downstreamOffset -gt $wrapperContractOffset) `
                    "$($wrapperContract.name) atomic deadline validation does not dominate its downstream launch"
            }
            $cleanupText = (Get-FunctionAst -Ast $stressAst `
                    -Name 'Complete-FailedHarnessProcess').Extent.Text
            Assert-True ($cleanupText -match '\$Record\.DeadlineExitEndMs') `
                'failure cleanup does not use the launch-sealed exit endpoint'
            Assert-True ($cleanupText -match '\$Record\.DeadlineDrainEndMs') `
                'failure cleanup does not use the launch-sealed drain endpoint'
            Assert-True ($cleanupText -notmatch `
                    'Get-MonotonicElapsedCeilingMs[\s\S]{0,160}\+\s*\$(?:ExitWaitLimitMs|OutputDrainLimitMs)') `
                'failure cleanup can synthesize a new elapsed-plus-limit endpoint'
            $batchText = (Get-FunctionAst -Ast $stressAst `
                    -Name 'Start-OwnedHarnessProcessBatch').Extent.Text
            Assert-True ($batchText -match `
                    '-DeferObservation:\(\[bool\]\$request\.defer_observation\)') `
                'batch launch does not seal the request observation policy'
            Assert-True ($batchText -match `
                    '-DeferObservation:\(\[bool\]\$record\.DeferObservation\)') `
                'batch rollback cleanup does not reuse the sealed observation policy'
            Assert-True ($stressAst.Extent.Text -match 'defer_observation\s*=\s*\$true') `
                'batch request fixtures omit their explicit observation policy'
        }

        Invoke-TestCase 'deadline-aware continuations fail closed and clean their exact process generation' {
            $deadlineEnvironment = [ordered]@{
                SystemRoot = $env:SystemRoot
                WINDIR = $env:WINDIR
                TEMP = [System.IO.Path]::GetTempPath()
                TMP = [System.IO.Path]::GetTempPath()
                PATH = (Split-Path -Parent $script:mainPortablePowerShell)
            }
            $cases = @(
                [pscustomobject]@{ name = 'omitted'; defer = $true; invoke = {
                        param($record, $sealedStopwatch)
                        Wait-HarnessProcess -Record $record -TimeoutMs 900 -DeferObservation
                    } },
                [pscustomobject]@{ name = 'partial'; defer = $true; invoke = {
                        param($record, $sealedStopwatch)
                        Wait-HarnessProcess -Record $record -TimeoutMs 900 -DeferObservation `
                            -OverallDeadlineStopwatch $sealedStopwatch
                    } },
                [pscustomobject]@{ name = 'mismatched'; defer = $true; invoke = {
                        param($record, $sealedStopwatch)
                        $differentStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
                        Wait-HarnessProcess -Record $record -TimeoutMs 900 -DeferObservation `
                            -OverallDeadlineStopwatch $differentStopwatch -OverallDeadlineMs 1200 `
                            -ExitWaitLimitMs 150 -OutputDrainLimitMs 50
                    } },
                [pscustomobject]@{ name = 'mismatched-nondeferred'; defer = $false; invoke = {
                        param($record, $sealedStopwatch)
                        $differentStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
                        Wait-HarnessProcess -Record $record -TimeoutMs 900 `
                            -OverallDeadlineStopwatch $differentStopwatch -OverallDeadlineMs 1200 `
                            -ExitWaitLimitMs 150 -OutputDrainLimitMs 50
                    } }
            )
            foreach ($case in $cases) {
                $script:processObservationCalls = 0
                $sealedStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
                $record = Start-HarnessProcess -Executable $script:mainPortablePowerShell `
                    -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30') `
                    -WorkingDirectory $mainRepository -Environment $deadlineEnvironment `
                    -Label "deadline-continuation-$($case.name)" -StandardInputText $null `
                    -OverallDeadlineStopwatch $sealedStopwatch -OverallDeadlineMs 1200 `
                    -ExitWaitLimitMs 150 -OutputDrainLimitMs 50 -RequestedExecutionTimeoutMs 900 `
                    -DeferObservation:$case.defer
                $processId = [int]$record.ProcessId
                $processStartedAt = [datetime]$record.ProcessStartedAt
                $wall = [System.Diagnostics.Stopwatch]::StartNew()
                try {
                    $failure = Get-ThrownFailure -Action {
                        & $case.invoke $record $sealedStopwatch
                    } -MessagePattern 'deadline contract|exact shared launch deadline' `
                        -Message "deadline continuation $($case.name) did not fail closed"
                    Assert-True ($failure.Exception.Data.Contains('ColayHarnessDeadlineContractCleanup')) `
                        "deadline continuation $($case.name) omitted cleanup evidence"
                    $cleanup = $failure.Exception.Data['ColayHarnessDeadlineContractCleanup']
                    Assert-True $cleanup.deadline_aware `
                        "deadline continuation $($case.name) cleanup lost deadline mode"
                    Assert-Equal 1200 $cleanup.overall_deadline_ms `
                        "deadline continuation $($case.name) cleanup overall deadline"
                    Assert-True ($cleanup.exit_wait_consumed_ms -le $cleanup.exit_wait_limit_ms) `
                        "deadline continuation $($case.name) reused its exit budget"
                    Assert-True ($cleanup.output_drain_consumed_ms -le $cleanup.output_drain_limit_ms) `
                        "deadline continuation $($case.name) reused its drain budget"
                    Assert-True $cleanup.exit_confirmed `
                        "deadline continuation $($case.name) did not confirm exit"
                    Assert-True $cleanup.stdout_completed `
                        "deadline continuation $($case.name) did not drain stdout"
                    Assert-True $cleanup.stderr_completed `
                        "deadline continuation $($case.name) did not drain stderr"
                    Assert-True $cleanup.process_disposed `
                        "deadline continuation $($case.name) did not dispose the process"
                    Assert-Equal 0 @($cleanup.cleanup_errors).Count `
                        "deadline continuation $($case.name) cleanup error count"
                    Assert-Equal ([bool]$case.defer) ([bool]$cleanup.observer_deferred) `
                        "deadline continuation $($case.name) changed sealed observation policy"
                    Assert-True ($null -eq $record.Process) `
                        "deadline continuation $($case.name) retained its process object"
                    $expectedObservationCalls = if ($case.defer) { 0 } else { 1 }
                    Assert-Equal $expectedObservationCalls $script:processObservationCalls `
                        "deadline continuation $($case.name) changed sealed process observation"
                } finally {
                    $wall.Stop()
                    if ($null -ne $record.Process) {
                        try { $record.Process.Kill($true) } catch { }
                        try { [void]$record.Process.WaitForExit(1000) } catch { }
                        try { $record.Process.Dispose() } catch { }
                        $record.Process = $null
                    }
                }
                Assert-True ($wall.ElapsedMilliseconds -lt 1275) `
                    "deadline continuation $($case.name) exceeded original budget plus tolerance"
                Assert-True ($sealedStopwatch.ElapsedMilliseconds -lt 1200) `
                    "deadline continuation $($case.name) exceeded its original absolute endpoint"
                $residue = Get-ProcessGenerationObservation -ProcessId $processId `
                    -ExpectedCreationTimeUtc $processStartedAt `
                    -ExpectedExecutablePath $script:mainPortablePowerShell
                Assert-True (-not ($residue.process_exists -and -not $residue.identity_verified)) `
                    "deadline continuation $($case.name) could not verify process residue: $($residue.observation_error)"
                Assert-True (-not $residue.expected_generation_live) `
                    "deadline continuation $($case.name) left exact process residue"
            }
        }

        Invoke-TestCase 'deadline-aware direct cleanup rejects omitted partial and mismatched contracts after cleanup' {
            $deadlineEnvironment = [ordered]@{
                SystemRoot = $env:SystemRoot
                WINDIR = $env:WINDIR
                TEMP = [System.IO.Path]::GetTempPath()
                TMP = [System.IO.Path]::GetTempPath()
                PATH = (Split-Path -Parent $script:mainPortablePowerShell)
            }
            foreach ($caseName in @('omitted', 'partial', 'mismatched', 'mismatched-nondeferred')) {
                $script:processObservationCalls = 0
                $sealedStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
                $deferObservation = $caseName -cne 'mismatched-nondeferred'
                $record = Start-HarnessProcess -Executable $script:mainPortablePowerShell `
                    -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 30') `
                    -WorkingDirectory $mainRepository -Environment $deadlineEnvironment `
                    -Label "deadline-direct-cleanup-$caseName" -StandardInputText $null `
                    -OverallDeadlineStopwatch $sealedStopwatch -OverallDeadlineMs 1200 `
                    -ExitWaitLimitMs 150 -OutputDrainLimitMs 50 -RequestedExecutionTimeoutMs 900 `
                    -DeferObservation:$deferObservation
                $processId = [int]$record.ProcessId
                $processStartedAt = [datetime]$record.ProcessStartedAt
                $wall = [System.Diagnostics.Stopwatch]::StartNew()
                try {
                    $failure = Get-ThrownFailure -Action {
                        if ($caseName -ceq 'omitted') {
                            Complete-FailedHarnessProcess -Record $record `
                                -FailureStage 'focused-contract-omission' -Terminate -DeferObservation
                        } elseif ($caseName -ceq 'partial') {
                            Complete-FailedHarnessProcess -Record $record `
                                -FailureStage 'focused-contract-partial' -Terminate -DeferObservation `
                                -OverallDeadlineStopwatch $sealedStopwatch
                        } elseif ($caseName -ceq 'mismatched') {
                            $differentStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
                            Complete-FailedHarnessProcess -Record $record `
                                -FailureStage 'focused-contract-mismatch' -Terminate -DeferObservation `
                                -OverallDeadlineStopwatch $differentStopwatch -OverallDeadlineMs 1200 `
                                -ExitWaitLimitMs 150 -OutputDrainLimitMs 50
                        } else {
                            $differentStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
                            Complete-FailedHarnessProcess -Record $record `
                                -FailureStage 'focused-contract-mismatch-nondeferred' `
                                -OverallDeadlineStopwatch $differentStopwatch -OverallDeadlineMs 1200 `
                                -ExitWaitLimitMs 150 -OutputDrainLimitMs 50
                        }
                    } -MessagePattern 'deadline contract|exact shared launch deadline' `
                        -Message "direct cleanup accepted a $caseName deadline contract"
                    Assert-True ($failure.Exception.Data.Contains('ColayHarnessDeadlineContractCleanup')) `
                        "direct cleanup $caseName did not return cleanup evidence"
                    $cleanup = $failure.Exception.Data['ColayHarnessDeadlineContractCleanup']
                    Assert-True $cleanup.exit_confirmed "direct cleanup $caseName did not confirm exit"
                    Assert-True $cleanup.stdout_completed "direct cleanup $caseName did not drain stdout"
                    Assert-True $cleanup.stderr_completed "direct cleanup $caseName did not drain stderr"
                    Assert-True $cleanup.process_disposed "direct cleanup $caseName did not dispose process"
                    Assert-True $cleanup.deadline_aware "direct cleanup $caseName lost deadline mode"
                    Assert-Equal 1200 $cleanup.overall_deadline_ms `
                        "direct cleanup $caseName overall deadline"
                    Assert-True ($cleanup.exit_wait_consumed_ms -le $cleanup.exit_wait_limit_ms) `
                        "direct cleanup $caseName reused its exit budget"
                    Assert-True ($cleanup.output_drain_consumed_ms -le $cleanup.output_drain_limit_ms) `
                        "direct cleanup $caseName reused its drain budget"
                    Assert-Equal 0 @($cleanup.cleanup_errors).Count `
                        "direct cleanup $caseName cleanup error count"
                    Assert-Equal $deferObservation ([bool]$cleanup.observer_deferred) `
                        "direct cleanup $caseName changed sealed observation policy"
                    Assert-True ($null -eq $record.Process) `
                        "direct cleanup $caseName retained process object"
                    $expectedObservationCalls = if ($deferObservation) { 0 } else { 1 }
                    Assert-Equal $expectedObservationCalls $script:processObservationCalls `
                        "direct cleanup $caseName changed sealed process observation"
                    Assert-True (-not $record.Stopwatch.IsRunning) `
                        "direct cleanup $caseName observed before stopping OS-lifetime timing"
                } finally {
                    $wall.Stop()
                    if ($null -ne $record.Process) {
                        try { $record.Process.Kill($true) } catch { }
                        try { [void]$record.Process.WaitForExit(1000) } catch { }
                        try { $record.Process.Dispose() } catch { }
                        $record.Process = $null
                    }
                    $safetyCandidate = Get-Process -Id $processId -ErrorAction SilentlyContinue
                    if ($null -ne $safetyCandidate) {
                        try {
                            $sameSafetyGeneration = $false
                            if (-not $safetyCandidate.HasExited) {
                                try {
                                    $safetyPath = [string]$safetyCandidate.Path
                                    if ([string]::IsNullOrWhiteSpace($safetyPath)) {
                                        throw 'live safety-cleanup candidate exposed no executable path'
                                    }
                                    $sameSafetyGeneration = $safetyCandidate.StartTime.ToUniversalTime() -eq
                                        $processStartedAt -and
                                        ([System.IO.Path]::GetFullPath($safetyPath)).Equals(
                                            $script:mainPortablePowerShell,
                                            [System.StringComparison]::OrdinalIgnoreCase
                                        )
                                } catch {
                                    $identityFailure = $_
                                    $exitedAfterFailure = $false
                                    try { $exitedAfterFailure = [bool]$safetyCandidate.HasExited } catch { }
                                    if (-not $exitedAfterFailure) { throw $identityFailure }
                                }
                            }
                            if ($sameSafetyGeneration) {
                                try { $safetyCandidate.Kill($true) } catch { }
                                try { [void]$safetyCandidate.WaitForExit(1000) } catch { }
                            }
                        } finally {
                            $safetyCandidate.Dispose()
                        }
                    }
                }
                Assert-True ($wall.ElapsedMilliseconds -lt 1275) `
                    "direct cleanup $caseName exceeded original budget plus tolerance"
                Assert-True ($sealedStopwatch.ElapsedMilliseconds -lt 1200) `
                    "direct cleanup $caseName exceeded its original absolute endpoint"
                $residue = Get-ProcessGenerationObservation -ProcessId $processId `
                    -ExpectedCreationTimeUtc $processStartedAt `
                    -ExpectedExecutablePath $script:mainPortablePowerShell
                Assert-True (-not ($residue.process_exists -and -not $residue.identity_verified)) `
                    "direct cleanup $caseName could not verify process residue: $($residue.observation_error)"
                Assert-True (-not $residue.expected_generation_live) `
                    "direct cleanup $caseName left exact process residue"
            }
        }

        Invoke-TestCase 'main immediate online cannot bypass a slow initial parser deadline' {
            $script:MainDaemonReadinessTimeoutMs = 20
            $script:MainDaemonReadinessExitWaitLimitMs = 5
            $script:MainDaemonReadinessOutputDrainLimitMs = 2
            $script:MainDaemonReadinessInitialParseDelayForTestMs = 30
            $start = New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedMainColay
            $failure = Get-ThrownFailure -Action {
                Wait-MainDaemonReadiness -DaemonStartDocument $start -ExpectedExecutable $expectedMainColay `
                    -Repository $mainRepository -Environment ([ordered]@{}) -Label 'main-slow-initial'
            } -MessagePattern 'timed out after 20ms' -Message 'main slow initial parser bypassed the deadline'
            [void](Assert-StructuredReadinessFailure -Failure $failure `
                    -EvidenceKey 'ColayStressMainDaemonReadinessEvidence' -Label 'main slow initial parser')
            $script:MainDaemonReadinessInitialParseDelayForTestMs = 0
            $script:MainDaemonReadinessTimeoutMs = 250
            $script:MainDaemonReadinessExitWaitLimitMs = 15
            $script:MainDaemonReadinessOutputDrainLimitMs = 5
        }
    }

    $childPath = Join-Path $tempRoot 'windows-process-audit-child.ps1'
    if ($availableStressFunctions -ccontains 'Write-ProcessAuditChildScript') {
        Write-ProcessAuditChildScript -Path $childPath
    }
    $childAst = $null
    $childFunctionNames = @()
    if (Test-Path -LiteralPath $childPath -PathType Leaf) {
        $childTokens = $null
        $childParseErrors = $null
        $childAst = [System.Management.Automation.Language.Parser]::ParseFile(
            $childPath,
            [ref]$childTokens,
            [ref]$childParseErrors
        )
        Invoke-TestCase 'generated audit child parses without errors' {
            Assert-Equal 0 $childParseErrors.Count 'generated audit child parser error count'
        }
        $childFunctionNames = @($childAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst]
        }, $true) | ForEach-Object Name)
    }

    $requiredChildFunctions = @(
        'ConvertTo-ComparablePath',
        'Get-AuditElapsedCeilingMs',
        'Get-AuditPhaseWaitMs',
        'Get-ProcessGenerationObservation',
        'Invoke-ChildProcessLine',
        'Invoke-ColayDocument',
        'ConvertTo-AuditDaemonDocumentIdentity',
        'Assert-AuditDaemonReadinessDeadline',
        'Wait-AuditDaemonReadiness',
        'ConvertTo-AuditChildJson'
    )
    Invoke-TestCase 'generated audit child exposes strict bounded readiness helpers' {
        foreach ($name in $requiredChildFunctions) {
            Assert-True ($childFunctionNames -ccontains $name) "generated audit child is missing function $name"
        }
        $childText = Get-Content -Raw -LiteralPath $childPath
        Assert-True ($childText -match '\$script:AuditDaemonReadinessTimeoutMs\s*=\s*5000(?:\D|$)') `
            'generated audit child readiness deadline is not exactly 5000ms'
        Assert-True ($childText -match '\$script:AuditDaemonReadinessCleanupReserveMs\s*=\s*[1-9][0-9]*(?:\D|$)') `
            'generated audit child has no positive cleanup reserve'
        $serializer = Get-FunctionAst -Ast $childAst -Name 'ConvertTo-AuditChildJson'
        Assert-True ($serializer.Extent.Text -match 'ConvertTo-Json\s+-Compress\s+-Depth\s+30\s+-WarningAction\s+Stop') `
            'generated audit child serializer does not preserve nested evidence fail closed'
        Assert-True ($childText -match 'ConvertTo-AuditChildJson\s+-Value\s+\$failureEvidence') `
            'generated audit child failure output bypasses the deep serializer'
        Assert-True ($childText -match 'ConvertTo-AuditChildJson\s+-Value\s+\$successEvidence') `
            'generated audit child success output bypasses the deep serializer'
        $childRunner = Get-FunctionAst -Ast $childAst -Name 'Invoke-ChildProcessLine'
        $childContractOffset = $childRunner.Extent.Text.IndexOf(
            '$boundDeadlineParameterCount',
            [StringComparison]::Ordinal
        )
        $childProcessStartOffset = $childRunner.Extent.Text.IndexOf(
            '$process.Start()',
            [StringComparison]::Ordinal
        )
        Assert-True ($childContractOffset -ge 0 -and $childProcessStartOffset -gt $childContractOffset) `
            'generated audit child atomic deadline validation does not dominate Process.Start'
        $childDocumentWrapper = Get-FunctionAst -Ast $childAst -Name 'Invoke-ColayDocument'
        $childDocumentContractOffset = $childDocumentWrapper.Extent.Text.IndexOf(
            '$boundDeadlineParameterCount',
            [StringComparison]::Ordinal
        )
        $childDocumentDownstreamOffset = $childDocumentWrapper.Extent.Text.IndexOf(
            'Invoke-ChildProcessLine',
            [StringComparison]::Ordinal
        )
        Assert-True (
            $childDocumentContractOffset -ge 0 -and
            $childDocumentDownstreamOffset -gt $childDocumentContractOffset
        ) 'generated audit child document wrapper deadline validation does not dominate its downstream launch'
        Assert-True ($childRunner.Extent.Text -notmatch `
                'Get-AuditElapsedCeilingMs[\s\S]{0,160}\+\s*\$(?:ExitWaitLimitMs|OutputDrainLimitMs)') `
            'generated audit child can synthesize a new elapsed-plus-limit endpoint'
        Assert-True ($childRunner.Extent.Text -match `
                'redirected stdout did not drain within the \$\{OutputDrainLimitMs\}ms cleanup limit') `
            'generated audit child drain failure message does not report its configured limit'
    }

    if ($null -ne $childAst) {
        Invoke-TestCase 'generated audit child reaches exact online before legacy registration' {
            $readinessAssignments = @($childAst.FindAll({
                param($node)
                $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                    $node.Left.Extent.Text -ceq '$readiness'
            }, $true))
            $registrationAssignments = @($childAst.FindAll({
                param($node)
                $node -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                    $node.Left.Extent.Text -ceq '$registered'
            }, $true))
            Assert-Equal 1 $readinessAssignments.Count 'audit readiness assignment count'
            Assert-Equal 1 $registrationAssignments.Count 'audit registration assignment count'
            Assert-True ($readinessAssignments[0].Extent.StartOffset -lt $registrationAssignments[0].Extent.StartOffset) `
                'audit registration precedes bounded readiness'

            $waitAst = Get-FunctionAst -Ast $childAst -Name 'Wait-AuditDaemonReadiness'
            $statusCalls = @($waitAst.FindAll({
                param($node)
                $node -is [System.Management.Automation.Language.CommandAst] -and
                    $node.GetCommandName() -ceq 'Invoke-ColayDocument'
            }, $true))
            Assert-Equal 1 $statusCalls.Count 'readiness status command call count'
            Assert-True ($statusCalls[0].Extent.Text -match "-Arguments\s+@\('--json',\s*'daemon',\s*'status'\)") `
                'readiness does not use exact separated status arguments'
            Assert-True ($statusCalls[0].Extent.Text -match '-TimeoutMs\s+\$commandBudgetMs') `
                'readiness status command does not use the remaining deadline budget'
            Assert-True ($statusCalls[0].Extent.Text -match '-OverallDeadlineStopwatch\s+\$stopwatch') `
                'readiness status command does not share the monotonic overall deadline'
            Assert-True ($statusCalls[0].Extent.Text -match '-ExitWaitLimitMs\s+\$script:AuditDaemonReadinessExitWaitLimitMs') `
                'readiness status command has no explicit bounded exit cleanup'
            Assert-True ($statusCalls[0].Extent.Text -match '-OutputDrainLimitMs\s+\$script:AuditDaemonReadinessOutputDrainLimitMs') `
                'readiness status command has no explicit bounded output cleanup'
        }
    }

    $childReady = $null -ne $childAst -and
        @($requiredChildFunctions | Where-Object { $childFunctionNames -cnotcontains $_ }).Count -eq 0
    if ($childReady) {
        foreach ($name in $requiredChildFunctions) {
            . ([scriptblock]::Create((Get-FunctionAst -Ast $childAst -Name $name).Extent.Text))
        }
        $expectedColay = Join-Path $tempRoot 'bin/colay.exe'
        $repository = Join-Path $tempRoot 'audit-repository'
        New-Item -ItemType Directory -Path $repository -Force | Out-Null
        $script:AuditDaemonReadinessTimeoutMs = 250
        $script:AuditDaemonReadinessPollIntervalMs = 1
        $script:AuditDaemonReadinessCleanupReserveMs = 20
        $script:AuditDaemonReadinessExitWaitLimitMs = 15
        $script:AuditDaemonReadinessOutputDrainLimitMs = 5
        $script:AuditDaemonReadinessInitialParseDelayForTestMs = 0
        $script:LastChildProcessCleanup = $null
        $script:ChildProcessLineFailureForTest = $null
        $script:auditPortablePowerShell = [System.IO.Path]::GetFullPath((Join-Path $PSHOME 'pwsh.exe'))
        $script:auditHangMarker = Join-Path $tempRoot 'audit-readiness-hang.pid'
        $script:readinessDocuments = [System.Collections.Generic.Queue[object]]::new()
        $script:readinessCalls = [System.Collections.Generic.List[object]]::new()

        Invoke-TestCase 'generated audit child document wrapper rejects a partial deadline before launch' {
            $script:ColayExe = $script:auditPortablePowerShell
            $script:LastChildProcessCleanup = $null
            $partialDocumentMarker = Join-Path $tempRoot 'audit-partial-document-wrapper-launch.marker'
            $escapedPartialDocumentMarker = $partialDocumentMarker.Replace("'", "''")
            $partialDocumentCommand = "[System.IO.File]::WriteAllText('$escapedPartialDocumentMarker', 'launched'); [Console]::WriteLine('{`"ok`":true}')"
            $documentStopwatch = [System.Diagnostics.Stopwatch]::StartNew()
            Assert-Throws -Action {
                Invoke-ColayDocument -Repository $repository `
                    -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $partialDocumentCommand) `
                    -Label 'audit-partial-document-wrapper' -TimeoutMs 100 `
                    -OverallDeadlineStopwatch $documentStopwatch -OverallDeadlineMs 10000
            } -MessagePattern 'atomic|partial|deadline contract' `
                -Message 'generated audit child document wrapper accepted a partial deadline contract'
            Assert-True ($null -eq $script:LastChildProcessCleanup) `
                'generated audit child document wrapper reached process cleanup after prelaunch rejection'
            Assert-True (-not (Test-Path -LiteralPath $partialDocumentMarker)) `
                'generated audit child document wrapper launched before partial deadline rejection'

            $nullDocumentMarker = Join-Path $tempRoot 'audit-null-document-wrapper-launch.marker'
            $escapedNullDocumentMarker = $nullDocumentMarker.Replace("'", "''")
            $nullDocumentCommand = "[System.IO.File]::WriteAllText('$escapedNullDocumentMarker', 'launched'); [Console]::WriteLine('{`"ok`":true}')"
            Assert-Throws -Action {
                Invoke-ColayDocument -Repository $repository `
                    -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $nullDocumentCommand) `
                    -Label 'audit-null-document-wrapper' -TimeoutMs 100 `
                    -OverallDeadlineStopwatch $null -OverallDeadlineMs 0 `
                    -ExitWaitLimitMs 5000 -OutputDrainLimitMs 2000
            } -MessagePattern 'atomic|invalid|deadline contract' `
                -Message 'generated audit child document wrapper accepted a null/default deadline quartet'
            Assert-True (-not (Test-Path -LiteralPath $nullDocumentMarker)) `
                'generated audit child document wrapper launched after a null/default deadline quartet'
        }

        function Invoke-ColayDocument {
            param(
                [string]$Repository,
                [string[]]$Arguments,
                [string]$Label,
                [int]$TimeoutMs = 30000,
                [System.Diagnostics.Stopwatch]$OverallDeadlineStopwatch,
                [int]$OverallDeadlineMs = 0,
                [int]$ExitWaitLimitMs = 5000,
                [int]$OutputDrainLimitMs = 2000
            )
            $script:readinessCalls.Add([pscustomobject]@{
                repository = $Repository
                arguments = @($Arguments)
                label = $Label
                timeout_ms = $TimeoutMs
                overall_deadline_ms = $OverallDeadlineMs
                exit_wait_limit_ms = $ExitWaitLimitMs
                output_drain_limit_ms = $OutputDrainLimitMs
            })
            if ($script:readinessDocuments.Count -eq 0) {
                return New-DaemonDocument -Command daemon_status -State booting -ExecutablePath $expectedColay
            }
            $next = $script:readinessDocuments.Dequeue()
            if ($next -is [System.Exception]) { throw $next }
            if ($next.PSObject.Properties.Name -contains 'hang_command' -and [bool]$next.hang_command) {
                $escapedMarker = $script:auditHangMarker.Replace("'", "''")
                $hangCommand = "[System.IO.File]::WriteAllText('$escapedMarker', [string]`$PID); Start-Sleep -Seconds 30"
                return Invoke-ChildProcessLine -Executable $script:auditPortablePowerShell `
                    -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $hangCommand) `
                    -WorkingDirectory $Repository -Label $Label -TimeoutMs $TimeoutMs `
                    -OverallDeadlineStopwatch $OverallDeadlineStopwatch -OverallDeadlineMs $OverallDeadlineMs `
                    -ExitWaitLimitMs $ExitWaitLimitMs -OutputDrainLimitMs $OutputDrainLimitMs
            }
            if ($next.PSObject.Properties.Name -contains 'delay_ms') {
                Start-Sleep -Milliseconds ([int]$next.delay_ms)
                return $next.document
            }
            return $next
        }

        Invoke-TestCase 'generated audit child rejects partial cleanup-only deadline contracts' {
            $script:LastChildProcessCleanup = $null
            $partialChildMarker = Join-Path $tempRoot 'audit-partial-deadline-launch.marker'
            $escapedPartialChildMarker = $partialChildMarker.Replace("'", "''")
            $partialChildCommand = "[System.IO.File]::WriteAllText('$escapedPartialChildMarker', 'launched')"
            Assert-Throws -Action {
                Invoke-ChildProcessLine -Executable $script:auditPortablePowerShell `
                    -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $partialChildCommand) `
                    -WorkingDirectory $repository -Label 'audit-partial-deadline' -TimeoutMs 100 `
                    -ExitWaitLimitMs 17 -OutputDrainLimitMs 19
            } -MessagePattern 'atomic|invalid bounded deadline contract' `
                -Message 'generated audit child accepted partial cleanup-only deadline limits'
            Assert-True ($null -eq $script:LastChildProcessCleanup) `
                'generated audit child reached process cleanup after prelaunch contract rejection'
            Assert-True (-not (Test-Path -LiteralPath $partialChildMarker)) `
                'generated audit child launched before partial deadline rejection'

            $nullChildMarker = Join-Path $tempRoot 'audit-null-deadline-launch.marker'
            $escapedNullChildMarker = $nullChildMarker.Replace("'", "''")
            $nullChildCommand = "[System.IO.File]::WriteAllText('$escapedNullChildMarker', 'launched')"
            Assert-Throws -Action {
                Invoke-ChildProcessLine -Executable $script:auditPortablePowerShell `
                    -Arguments @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', $nullChildCommand) `
                    -WorkingDirectory $repository -Label 'audit-null-deadline' -TimeoutMs 100 `
                    -OverallDeadlineStopwatch $null -OverallDeadlineMs 0 `
                    -ExitWaitLimitMs 5000 -OutputDrainLimitMs 2000
            } -MessagePattern 'atomic|invalid|deadline contract' `
                -Message 'generated audit child accepted a null/default deadline quartet'
            Assert-True (-not (Test-Path -LiteralPath $nullChildMarker)) `
                'generated audit child launched after a null/default deadline quartet'
        }

        Invoke-TestCase 'audit readiness accepts immediate exact online without polling' {
            $script:readinessCalls.Clear()
            $start = New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedColay
            $result = Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                -ExpectedExecutable $expectedColay -Repository $repository -Label 'immediate'
            Assert-Equal online $result.Evidence.readiness_status 'immediate readiness status'
            Assert-Equal 0 $result.Evidence.poll_count 'immediate readiness poll count'
            Assert-Equal 0 $script:readinessCalls.Count 'immediate status command count'
            Assert-True ([object]::ReferenceEquals($start, $result.OnlineDocument)) `
                'immediate readiness did not return the anchored online document'
        }

        Invoke-TestCase 'audit readiness permits only identity-stable booting/probing until online' {
            $script:readinessCalls.Clear()
            $script:readinessDocuments.Clear()
            $script:readinessDocuments.Enqueue((New-DaemonDocument -Command daemon_status -State probing -ExecutablePath $expectedColay))
            $script:readinessDocuments.Enqueue((New-DaemonDocument -Command daemon_status -State online -ExecutablePath $expectedColay))
            $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
            $result = Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                -ExpectedExecutable $expectedColay -Repository $repository -Label 'delayed'
            Assert-Equal online $result.Evidence.readiness_status 'delayed readiness status'
            Assert-Equal 2 $result.Evidence.poll_count 'delayed readiness poll count'
            Assert-Equal 'probing,online' (@($result.Evidence.polls | ForEach-Object state) -join ',') `
                'delayed readiness transition sequence'
            foreach ($poll in $result.Evidence.polls) {
                Assert-Equal $result.Evidence.anchored_identity.instance_id $poll.instance_id `
                    'readiness poll instance id'
                Assert-Equal $result.Evidence.anchored_identity.process_id $poll.process_id `
                    'readiness poll process id'
                Assert-Equal $result.Evidence.anchored_identity.executable_path $poll.executable_path `
                    'readiness poll executable path'
                Assert-Equal $poll.state $poll.phase 'readiness poll state/phase equality'
            }
            foreach ($call in $script:readinessCalls) {
                Assert-Equal '--json,daemon,status' (@($call.arguments) -join ',') 'readiness separated arguments'
                Assert-True ($call.timeout_ms -gt 0) 'readiness status command timeout was not positive'
                $cleanupBudget = $call.exit_wait_limit_ms + $call.output_drain_limit_ms
                Assert-True (($call.timeout_ms + $cleanupBudget) -le $result.Evidence.polls[$script:readinessCalls.IndexOf($call)].remaining_at_launch_ms) `
                    'readiness execution plus cleanup exceeded remaining launch budget'
            }
        }

        foreach ($drift in @('instance', 'pid', 'path')) {
            Invoke-TestCase "audit readiness rejects $drift identity drift" {
                $script:readinessCalls.Clear()
                $script:readinessDocuments.Clear()
                $parameters = @{
                    Command = 'daemon_status'
                    State = 'online'
                    ExecutablePath = $expectedColay
                }
                if ($drift -ceq 'instance') { $parameters.InstanceId = '019f8b42-8e29-7c2d-9d6f-9f48c593b9d2' }
                if ($drift -ceq 'pid') { $parameters.ProcessId = [int]4243 }
                if ($drift -ceq 'path') { $parameters.ExecutablePath = Join-Path $tempRoot 'other/colay.exe' }
                $script:readinessDocuments.Enqueue((New-DaemonDocument @parameters))
                $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
                $failure = Get-ThrownFailure -Action {
                    Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                        -ExpectedExecutable $expectedColay -Repository $repository -Label "drift-$drift"
                } -MessagePattern 'identity drift|executable path mismatch' `
                    -Message "readiness accepted $drift identity drift"
                [void](Assert-StructuredReadinessFailure -Failure $failure `
                        -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label "$drift drift")
            }
        }

        Invoke-TestCase 'audit readiness rejects state and phase mismatch' {
            $start = New-DaemonDocument -Command daemon_start -State booting -Phase probing `
                -ExecutablePath $expectedColay
            $failure = Get-ThrownFailure -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'state-phase'
            } -MessagePattern 'state/phase mismatch' -Message 'readiness accepted state/phase mismatch'
            [void](Assert-StructuredReadinessFailure -Failure $failure `
                    -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'state/phase mismatch')
        }

        Invoke-TestCase 'audit readiness rejects terminal state before registration' {
            $start = New-DaemonDocument -Command daemon_start -State failed -ExecutablePath $expectedColay
            $failure = Get-ThrownFailure -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'terminal'
            } -MessagePattern 'terminal|non-progress' -Message 'readiness accepted terminal state'
            [void](Assert-StructuredReadinessFailure -Failure $failure `
                    -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'terminal start')
        }

        Invoke-TestCase 'audit readiness rejects malformed unsafe PID' {
            $start = New-DaemonDocument -Command daemon_start -State online -ProcessId 4242.5 `
                -ExecutablePath $expectedColay
            $failure = Get-ThrownFailure -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'unsafe-pid'
            } -MessagePattern 'PID|integer' -Message 'readiness accepted fractional PID'
            [void](Assert-StructuredReadinessFailure -Failure $failure `
                    -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'unsafe PID')
        }

        Invoke-TestCase 'audit readiness rejects malformed schema, command, and UUID documents' {
            $wrongSchema = New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedColay
            $wrongSchema.schema_version = '2'
            $wrongCommand = New-DaemonDocument -Command daemon_status -State online -ExecutablePath $expectedColay
            $nonCanonicalUuid = New-DaemonDocument -Command daemon_start -State online `
                -InstanceId '019F8B42-8E29-7C2D-9D6F-9F48C593B9D1' -ExecutablePath $expectedColay
            foreach ($fixture in @($wrongSchema, $wrongCommand, $nonCanonicalUuid)) {
                $failure = Get-ThrownFailure -Action {
                    Wait-AuditDaemonReadiness -DaemonStartDocument $fixture `
                        -ExpectedExecutable $expectedColay -Repository $repository -Label 'malformed'
                } -MessagePattern 'schema-v1|canonical UUID' -Message 'readiness accepted a malformed start document'
                [void](Assert-StructuredReadinessFailure -Failure $failure `
                        -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'malformed start')
            }
        }

        Invoke-TestCase 'audit readiness rejects every malformed polled status before registration' {
            $wrongSchema = New-DaemonDocument -Command daemon_status -State online -ExecutablePath $expectedColay
            $wrongSchema.schema_version = '2'
            $fixtures = @(
                $wrongSchema,
                (New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedColay),
                (New-DaemonDocument -Command daemon_status -State online `
                    -InstanceId '019F8B42-8E29-7C2D-9D6F-9F48C593B9D1' -ExecutablePath $expectedColay),
                (New-DaemonDocument -Command daemon_status -State online -ProcessId 4242.5 `
                    -ExecutablePath $expectedColay),
                (New-DaemonDocument -Command daemon_status -State online `
                    -ExecutablePath (Join-Path $tempRoot 'wrong-audit/colay.exe')),
                (New-DaemonDocument -Command daemon_status -State probing -Phase booting `
                    -ExecutablePath $expectedColay),
                (New-DaemonDocument -Command daemon_status -State failed -ExecutablePath $expectedColay),
                (New-DaemonDocument -Command daemon_status -State mystery -ExecutablePath $expectedColay)
            )
            foreach ($fixture in $fixtures) {
                $script:readinessCalls.Clear()
                $script:readinessDocuments.Clear()
                $script:AuditDaemonReadinessTimeoutMs = 250
                $script:AuditDaemonReadinessCleanupReserveMs = 20
                $script:AuditDaemonReadinessExitWaitLimitMs = 15
                $script:AuditDaemonReadinessOutputDrainLimitMs = 5
                $script:readinessDocuments.Enqueue($fixture)
                $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
                $failure = Get-ThrownFailure -Action {
                    Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                        -ExpectedExecutable $expectedColay -Repository $repository -Label 'audit-invalid-poll'
                } -MessagePattern 'schema-v1|canonical UUID|PID|path|state/phase|terminal|non-progress|identity drift' `
                    -Message 'audit readiness accepted an invalid polled status'
                [void](Assert-StructuredReadinessFailure -Failure $failure `
                        -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'audit invalid poll')
                Assert-Equal 1 $script:readinessCalls.Count 'audit invalid poll command count'
                Assert-Equal '--json,daemon,status' (@($script:readinessCalls[0].arguments) -join ',') `
                    'audit invalid poll reached a registration command'
            }
        }

        Invoke-TestCase 'audit readiness rejects status command failure and late online' {
            foreach ($fixture in @(
                    [System.InvalidOperationException]::new('injected audit status command failure'),
                    [pscustomobject]@{
                        delay_ms = 120
                        document = New-DaemonDocument -Command daemon_status -State online `
                            -ExecutablePath $expectedColay
                    }
                )) {
                $script:readinessCalls.Clear()
                $script:readinessDocuments.Clear()
                $script:AuditDaemonReadinessTimeoutMs = 100
                $script:AuditDaemonReadinessCleanupReserveMs = 20
                $script:AuditDaemonReadinessExitWaitLimitMs = 15
                $script:AuditDaemonReadinessOutputDrainLimitMs = 5
                $script:readinessDocuments.Enqueue($fixture)
                $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
                $failure = Get-ThrownFailure -Action {
                    Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                        -ExpectedExecutable $expectedColay -Repository $repository -Label 'audit-command-failure'
                } -MessagePattern 'failure|timed out after 100ms' `
                    -Message 'audit readiness accepted command failure or late online'
                [void](Assert-StructuredReadinessFailure -Failure $failure `
                        -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'audit command failure')
                Assert-Equal 1 $script:readinessCalls.Count 'audit command failure status call count'
            }
        }

        Invoke-TestCase 'audit immediate online cannot bypass a slow initial parser deadline' {
            $script:AuditDaemonReadinessTimeoutMs = 20
            $script:AuditDaemonReadinessCleanupReserveMs = 7
            $script:AuditDaemonReadinessExitWaitLimitMs = 5
            $script:AuditDaemonReadinessOutputDrainLimitMs = 2
            $script:AuditDaemonReadinessInitialParseDelayForTestMs = 30
            $start = New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedColay
            $failure = Get-ThrownFailure -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'audit-slow-initial'
            } -MessagePattern 'timed out after 20ms' -Message 'audit slow initial parser bypassed the deadline'
            [void](Assert-StructuredReadinessFailure -Failure $failure `
                    -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'audit slow initial parser')
            $script:AuditDaemonReadinessInitialParseDelayForTestMs = 0
        }

        Invoke-TestCase 'audit hanging status is cleanup-inclusive and leaves no exact process residue' {
            if (Test-Path -LiteralPath $script:auditHangMarker) {
                Remove-Item -LiteralPath $script:auditHangMarker -Force
            }
            $script:readinessCalls.Clear()
            $script:readinessDocuments.Clear()
            $script:LastChildProcessCleanup = $null
            $script:AuditDaemonReadinessTimeoutMs = 1200
            $script:AuditDaemonReadinessPollIntervalMs = 1
            $script:AuditDaemonReadinessCleanupReserveMs = 200
            $script:AuditDaemonReadinessExitWaitLimitMs = 150
            $script:AuditDaemonReadinessOutputDrainLimitMs = 50
            $script:readinessDocuments.Enqueue([pscustomobject]@{ hang_command = $true })
            $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
            $wall = [System.Diagnostics.Stopwatch]::StartNew()
            $failure = Get-ThrownFailure -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'audit-hang'
            } -MessagePattern 'exceeded|timed out' -Message 'audit hanging status did not fail'
            $wall.Stop()
            $evidence = Assert-StructuredReadinessFailure -Failure $failure `
                -EvidenceKey 'ColayStressAuditDaemonReadinessEvidence' -Label 'audit hanging status'
            Assert-True ($wall.ElapsedMilliseconds -lt 1275) `
                "audit hanging status exceeded overall deadline plus 75ms tolerance: $($wall.ElapsedMilliseconds)ms"
            Assert-Equal 1 $script:readinessCalls.Count 'audit hanging status command count'
            Assert-Equal 1 $evidence.poll_count 'audit hanging status evidence poll count'
            Assert-True (Test-Path -LiteralPath $script:auditHangMarker -PathType Leaf) `
                'audit hanging status child did not publish its PID'
            $publishedPid = [int](Get-Content -LiteralPath $script:auditHangMarker -Raw).Trim()
            $cleanup = $script:LastChildProcessCleanup
            Assert-Equal $publishedPid ([int]$cleanup.process_id) 'audit hanging status cleanup PID'
            Assert-True $cleanup.exit_confirmed 'audit hanging status exit was not confirmed'
            Assert-True $cleanup.stdout_completed 'audit hanging status stdout did not drain'
            Assert-True $cleanup.process_disposed 'audit hanging status process was not disposed'
            Assert-Equal 0 @($cleanup.cleanup_errors).Count 'audit hanging status cleanup error count'
            Assert-True ($cleanup.exit_wait_consumed_ms -le $cleanup.exit_wait_limit_ms) `
                'audit hanging status reused its exit cleanup budget'
            Assert-True ($cleanup.output_drain_consumed_ms -le $cleanup.output_drain_limit_ms) `
                'audit hanging status reused its output-drain cleanup budget'
            Assert-True (($cleanup.command_timeout_ms + $cleanup.exit_wait_limit_ms +
                    $cleanup.output_drain_limit_ms) -le $cleanup.remaining_at_launch_ms) `
                'audit hanging status launch budget exceeded actual remaining time'
            $residue = Get-ProcessGenerationObservation -ProcessId $publishedPid `
                -ExpectedCreationFileTimeUtc ([long]$cleanup.process_creation_file_time_utc) `
                -ExpectedExecutablePath ([string]$cleanup.executable_path)
            Assert-True (-not ($residue.process_exists -and -not $residue.identity_verified)) `
                "audit hanging status could not verify process residue: $($residue.observation_error)"
            Assert-True (-not $residue.expected_generation_live) 'audit hanging status left exact process residue'
            Remove-Item -LiteralPath $script:auditHangMarker -Force
        }

        Invoke-TestCase 'audit readiness JSON round trip preserves nested evidence and validates fail closed' {
            $script:AuditDaemonReadinessTimeoutMs = 250
            $script:AuditDaemonReadinessPollIntervalMs = 1
            $script:AuditDaemonReadinessCleanupReserveMs = 20
            $script:AuditDaemonReadinessExitWaitLimitMs = 15
            $script:AuditDaemonReadinessOutputDrainLimitMs = 5
            $immediateStart = New-DaemonDocument -Command daemon_start -State online `
                -ExecutablePath $expectedColay
            $immediate = Wait-AuditDaemonReadiness -DaemonStartDocument $immediateStart `
                -ExpectedExecutable $expectedColay -Repository $repository -Label 'audit-round-trip-immediate'
            $immediateWarnings = @()
            $immediateJson = ConvertTo-AuditChildJson -Value ([pscustomobject]@{
                schema_version = '1'
                status = 'passed'
                daemon_readiness = $immediate.Evidence
            }) -WarningVariable immediateWarnings
            Assert-Equal 0 @($immediateWarnings).Count 'immediate audit readiness serialization warning count'
            $immediateParsed = $immediateJson | ConvertFrom-Json -Depth 30
            Assert-True ($immediateParsed.daemon_readiness.online_document.data.status.instance -isnot [string]) `
                'immediate audit readiness online identity was truncated to a string'
            [void](Assert-AuditDaemonReadinessEvidence -ReadinessEvidence $immediateParsed.daemon_readiness `
                    -ExpectedExecutable $expectedColay -ExpectedLabel 'audit-round-trip-immediate' `
                    -ExpectedOverallTimeoutMs 250 `
                    -ExpectedPollIntervalMs 1 -ExpectedExitWaitLimitMs 15 -ExpectedOutputDrainLimitMs 5)

            $script:readinessCalls.Clear()
            $script:readinessDocuments.Clear()
            $script:readinessDocuments.Enqueue((New-DaemonDocument -Command daemon_status -State probing `
                    -ExecutablePath $expectedColay))
            $script:readinessDocuments.Enqueue((New-DaemonDocument -Command daemon_status -State online `
                    -ExecutablePath $expectedColay))
            $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
            $polled = Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                -ExpectedExecutable $expectedColay -Repository $repository -Label 'audit-round-trip'
            $serializationWarnings = @()
            $json = ConvertTo-AuditChildJson -Value ([pscustomobject]@{
                schema_version = '1'
                status = 'passed'
                daemon_readiness = $polled.Evidence
            }) -WarningVariable serializationWarnings
            Assert-Equal 0 @($serializationWarnings).Count 'audit readiness serialization warning count'
            $parsed = $json | ConvertFrom-Json -Depth 30
            Assert-True ($parsed.daemon_readiness.polls[0] -isnot [string]) `
                'audit readiness poll was truncated to a string'
            Assert-True ($parsed.daemon_readiness.online_document.data.status.instance -isnot [string]) `
                'audit readiness online identity was truncated to a string'
            [void](Assert-AuditDaemonReadinessEvidence -ReadinessEvidence $parsed.daemon_readiness `
                    -ExpectedExecutable $expectedColay -ExpectedLabel 'audit-round-trip' `
                    -ExpectedOverallTimeoutMs 250 `
                    -ExpectedPollIntervalMs 1 -ExpectedExitWaitLimitMs 15 -ExpectedOutputDrainLimitMs 5)

            $invalidEvidence = [System.Collections.Generic.List[object]]::new()
            $invalidEvidence.Add('System.Management.Automation.PSCustomObject')
            $missing = $json | ConvertFrom-Json -Depth 30
            $missing.daemon_readiness.PSObject.Properties.Remove('online_document')
            $invalidEvidence.Add($missing.daemon_readiness)
            $truncatedPoll = $json | ConvertFrom-Json -Depth 30
            $truncatedPoll.daemon_readiness.polls = @('System.Management.Automation.PSCustomObject')
            $invalidEvidence.Add($truncatedPoll.daemon_readiness)
            $truncatedOnline = $json | ConvertFrom-Json -Depth 30
            $truncatedOnline.daemon_readiness.online_document.data = 'System.Management.Automation.PSCustomObject'
            $invalidEvidence.Add($truncatedOnline.daemon_readiness)
            $missingPollInterval = $json | ConvertFrom-Json -Depth 30
            $missingPollInterval.daemon_readiness.PSObject.Properties.Remove('poll_interval_ms')
            $invalidEvidence.Add($missingPollInterval.daemon_readiness)
            $objectPolls = $json | ConvertFrom-Json -Depth 30
            $objectPolls.daemon_readiness.polls = $objectPolls.daemon_readiness.polls[0]
            $invalidEvidence.Add($objectPolls.daemon_readiness)
            $objectStatusCommand = $json | ConvertFrom-Json -Depth 30
            $objectStatusCommand.daemon_readiness.status_command = [pscustomobject]@{ command = 'daemon status' }
            $invalidEvidence.Add($objectStatusCommand.daemon_readiness)
            $wrongStatusCommand = $json | ConvertFrom-Json -Depth 30
            $wrongStatusCommand.daemon_readiness.status_command = @('--json', 'daemon', 'start')
            $invalidEvidence.Add($wrongStatusCommand.daemon_readiness)
            $wrongIntegerType = $json | ConvertFrom-Json -Depth 30
            $wrongIntegerType.daemon_readiness.poll_interval_ms = '1'
            $invalidEvidence.Add($wrongIntegerType.daemon_readiness)
            $wrongConstants = $json | ConvertFrom-Json -Depth 30
            $wrongConstants.daemon_readiness.exit_wait_limit_ms = 16
            $wrongConstants.daemon_readiness.cleanup_reserve_ms = 21
            $invalidEvidence.Add($wrongConstants.daemon_readiness)
            $wrongCleanupArithmetic = $json | ConvertFrom-Json -Depth 30
            $wrongCleanupArithmetic.daemon_readiness.cleanup_reserve_ms = 19
            $invalidEvidence.Add($wrongCleanupArithmetic.daemon_readiness)
            $reversedElapsed = $json | ConvertFrom-Json -Depth 30
            $reversedElapsed.daemon_readiness.elapsed_ms = 1
            $reversedElapsed.daemon_readiness.polls[0].observed_elapsed_ms = 2
            $invalidEvidence.Add($reversedElapsed.daemon_readiness)
            $reversedPollSequence = $json | ConvertFrom-Json -Depth 30
            $reversedPollSequence.daemon_readiness.polls[0].observed_elapsed_ms = 2
            $reversedPollSequence.daemon_readiness.polls[1].observed_elapsed_ms = 1
            $reversedPollSequence.daemon_readiness.elapsed_ms = 3
            $invalidEvidence.Add($reversedPollSequence.daemon_readiness)
            $nonSequentialLabel = $json | ConvertFrom-Json -Depth 30
            $nonSequentialLabel.daemon_readiness.polls[0].command_label = 'audit-round-trip-daemon-readiness-999'
            $invalidEvidence.Add($nonSequentialLabel.daemon_readiness)
            $wrongLabelPrefix = $json | ConvertFrom-Json -Depth 30
            $wrongLabelPrefix.daemon_readiness.polls[0].command_label = 'forged-daemon-readiness-001'
            $invalidEvidence.Add($wrongLabelPrefix.daemon_readiness)
            foreach ($invalid in $invalidEvidence) {
                Assert-Throws -Action {
                    Assert-AuditDaemonReadinessEvidence -ReadinessEvidence $invalid `
                        -ExpectedExecutable $expectedColay -ExpectedLabel 'audit-round-trip' `
                        -ExpectedOverallTimeoutMs 250 `
                        -ExpectedPollIntervalMs 1 -ExpectedExitWaitLimitMs 15 -ExpectedOutputDrainLimitMs 5
                } -MessagePattern 'readiness|missing|truncated|schema-v1|status identity|array|elapsed|sequence|command' `
                    -Message 'parent validator accepted missing or truncated readiness evidence'
            }
        }

        Invoke-TestCase 'audit readiness enforces its monotonic overall deadline' {
            $script:readinessCalls.Clear()
            $script:readinessDocuments.Clear()
            $script:AuditDaemonReadinessTimeoutMs = 500
            $script:AuditDaemonReadinessPollIntervalMs = 1
            $script:AuditDaemonReadinessCleanupReserveMs = 50
            $script:AuditDaemonReadinessExitWaitLimitMs = 40
            $script:AuditDaemonReadinessOutputDrainLimitMs = 10
            $script:readinessDocuments.Enqueue([pscustomobject]@{
                    delay_ms = 600
                    document = New-DaemonDocument -Command daemon_status -State booting `
                        -ExecutablePath $expectedColay
                })
            $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
            Assert-Throws -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'timeout'
            } -MessagePattern 'timed out after 500ms' -Message 'readiness exceeded its overall deadline without timeout'
            Assert-Equal 1 $script:readinessCalls.Count 'timeout fixture status poll count'
        }
    }

    Invoke-TestCase 'durable source evidence uses source_root_hash and not a latency marker group name' {
        $memberNames = @($stressAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.MemberExpressionAst]
        }, $true) | ForEach-Object { $_.Member.Extent.Text.Trim("'`"") })
        Assert-True ($memberNames -ccontains 'source_root_hash') 'durable seed evidence omitted source_root_hash'
        Assert-True ($memberNames -cnotcontains 'inspection_group_id') `
            'durable seed evidence still conflates source_root_hash with an inspection group id'
    }

    Invoke-TestCase 'marker diagnostic imports the amended stress closure under its static contract' {
        $diagnosticPath = (Resolve-Path (Join-Path $scriptRoot `
                    '../../artifacts/qa/windows-state-acl/marker-attribution-ab-diagnostic.ps1')).Path
        $diagnosticTokens = $null
        $diagnosticParseErrors = $null
        $diagnosticAst = [System.Management.Automation.Language.Parser]::ParseFile(
            $diagnosticPath,
            [ref]$diagnosticTokens,
            [ref]$diagnosticParseErrors
        )
        Assert-Equal 0 $diagnosticParseErrors.Count 'marker diagnostic parser error count'
        $diagnosticSeedOverride = Get-FunctionAst -Ast $diagnosticAst -Name 'New-LegacyWorkspace'
        $diagnosticSeedOverrideText = $diagnosticSeedOverride.Extent.Text
        Assert-True ($diagnosticSeedOverrideText -match '(?m)^\s*source_root_hash\s*=\s*\$null\s*$') `
            'marker diagnostic seed object omitted its source_root_hash slot'
        Assert-True ($diagnosticSeedOverrideText -notmatch '(?m)^\s*inspection_group_id\s*=') `
            'marker diagnostic seed object still exposes obsolete inspection_group_id'
        $diagnosticMemberNames = @($diagnosticAst.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.MemberExpressionAst]
        }, $true) | ForEach-Object { $_.Member.Extent.Text.Trim("'`"") })
        Assert-True ($diagnosticMemberNames -ccontains 'source_root_hash') `
            'marker diagnostic seed contract omitted source_root_hash'
        Assert-True ($diagnosticMemberNames -cnotcontains 'inspection_group_id') `
            'marker diagnostic still uses the obsolete inspection_group_id seed contract'
        $mainStart = @($diagnosticAst.EndBlock.Statements | Where-Object {
            $_ -is [System.Management.Automation.Language.AssignmentStatementAst] -and
                $_.Left.Extent.Text -ceq '$script:ResolvedColay' -and
                $_.Right.Extent.Text -match '^Resolve-AbFile\b'
        })
        Assert-Equal 1 $mainStart.Count 'marker diagnostic main entry assignment count'
        $diagnosticText = Get-Content -Raw -LiteralPath $diagnosticPath
        $prefix = $diagnosticText.Substring(0, $mainStart[0].Extent.StartOffset)
        $escapedDiagnosticPath = $diagnosticPath.Replace("'", "''")
        $escapedStressPath = $stressPath.Replace("'", "''")
        $contractTail = @"
`$staticContract = Assert-AbStaticContract -DiagnosticPath '$escapedDiagnosticPath'
`$importContract = Import-StressHarnessFunctions '$escapedStressPath'
`$expectedProbe = [ordered]@{ first = 1; nested = [ordered]@{ alpha = `$true; beta = 'value' } }
`$actualProbe = [ordered]@{ nested = [ordered]@{ beta = 'value'; alpha = `$true }; first = 1 }
Assert-EquivalentJson -Expected `$expectedProbe -Actual `$actualProbe -Label 'imported comparer probe'
return [pscustomobject]@{ static = `$staticContract; import = `$importContract; comparer_probe = 'passed' }
"@
        $contractFixture = [scriptblock]::Create($prefix + $contractTail)
        $stressHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $stressPath).Hash.ToLowerInvariant()
        $diagnosticHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $diagnosticPath).Hash.ToLowerInvariant()
        $contract = & $contractFixture -ColayExe 'unused-colay.exe' `
            -FakeProviderExe 'unused-fake-provider.exe' -StressHarness $stressPath `
            -EvidenceRoot $tempRoot -ExpectedColaySha256 ('0' * 64) `
            -ExpectedFakeProviderSha256 ('0' * 64) -ExpectedStressHarnessSha256 $stressHash `
            -ExpectedDiagnosticSha256 $diagnosticHash
        Assert-Equal 0 @($contract.import.unqualified_free_variables).Count `
            'marker stress import unqualified free variable count'
        Assert-Equal 0 @($contract.import.ast_contract_violations).Count `
            'marker stress import AST violation count'
        Assert-Equal 0 $contract.import.wait_active_timing_cim_command_count `
            'marker stress import pre-stop timing CIM count'
        Assert-True ($contract.import.function_count -gt 0) `
            'marker stress import closure was empty'
        Assert-True ($contract.import.function_names -ccontains 'Test-JsonElementStructuralEquality') `
            'marker stress import closure omitted the structural JSON helper'
        Assert-Equal passed $contract.comparer_probe `
            'marker stress imported comparer did not accept reordered objects'

        $mutatedStressPath = Join-Path $tempRoot 'unapproved-free-variable-stress.ps1'
        $stressSource = Get-Content -Raw -LiteralPath $stressPath
        $deadlineFunctionNeedle = '    $deadlineParameterNames = @('
        $mutatedStressSource = $stressSource.Replace(
            $deadlineFunctionNeedle,
            "    [void]`$unapprovedDeadlineGlobal`n$deadlineFunctionNeedle"
        )
        Assert-True ($mutatedStressSource -cne $stressSource) `
            'marker negative fixture did not mutate the imported stress closure'
        [System.IO.File]::WriteAllText(
            $mutatedStressPath,
            $mutatedStressSource,
            [System.Text.UTF8Encoding]::new($false)
        )
        $mutatedStressHash = (Get-FileHash -Algorithm SHA256 `
                -LiteralPath $mutatedStressPath).Hash.ToLowerInvariant()
        $escapedMutatedStressPath = $mutatedStressPath.Replace("'", "''")
        $negativeTail = @"
[void](Import-StressHarnessFunctions '$escapedMutatedStressPath')
"@
        $negativeFixture = [scriptblock]::Create($prefix + $negativeTail)
        Assert-Throws -Action {
            & $negativeFixture -ColayExe 'unused-colay.exe' `
                -FakeProviderExe 'unused-fake-provider.exe' -StressHarness $mutatedStressPath `
                -EvidenceRoot $tempRoot -ExpectedColaySha256 ('0' * 64) `
                -ExpectedFakeProviderSha256 ('0' * 64) `
                -ExpectedStressHarnessSha256 $mutatedStressHash `
                -ExpectedDiagnosticSha256 $diagnosticHash
        } -MessagePattern 'unqualified free variables.*unapprovedDeadlineGlobal' `
            -Message 'marker import allowed an arbitrary unqualified variable'
    }
}
finally {
    if (Test-Path -LiteralPath $tempRoot) {
        $resolvedTemp = [System.IO.Path]::GetFullPath($tempRoot)
        $systemTemp = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
        if (-not $resolvedTemp.StartsWith($systemTemp, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "refusing to clean unexpected test path: $resolvedTemp"
        }
        Remove-Item -LiteralPath $resolvedTemp -Recurse -Force
    }
}

if ($failures.Count -ne 0) {
    throw "Windows marker phase tests failed ($($failures.Count)): $($failures -join '; ')"
}

Write-Output 'windows state ACL marker phase tests passed'
