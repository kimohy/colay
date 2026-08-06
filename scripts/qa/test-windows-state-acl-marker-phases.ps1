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
    'ConvertTo-NormalizedExecutablePath',
    'Assert-HarnessDeadlineContract',
    'Get-MonotonicElapsedCeilingMs',
    'Get-BoundedPhaseWaitMs',
    'ConvertTo-StressDaemonDocumentIdentity',
    'Assert-StressDaemonReadinessDeadline',
    'Wait-MainDaemonReadiness',
    'Assert-AuditDaemonReadinessEvidence',
    'Assert-StatusJson',
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

foreach ($name in $requiredStressFunctions) {
    if ($availableStressFunctions -ccontains $name) {
        . ([scriptblock]::Create((Get-FunctionAst -Ast $stressAst -Name $name).Extent.Text))
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

    $mainReadinessReady = @(
        'ConvertTo-StressDaemonDocumentIdentity',
        'Assert-StressDaemonReadinessDeadline',
        'Wait-MainDaemonReadiness'
    | Where-Object { $availableStressFunctions -cnotcontains $_ }).Count -eq 0
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
        function Update-ProcessObservation { }

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
            $candidate = Get-Process -Id $publishedPid -ErrorAction SilentlyContinue
            if ($null -ne $candidate) {
                try {
                    $sameGeneration = $candidate.StartTime.ToUniversalTime() -eq
                        ([datetime]$commandRow.process_started_at_utc).ToUniversalTime() -and
                        ([System.IO.Path]::GetFullPath($candidate.Path)).Equals(
                            $script:mainPortablePowerShell,
                            [System.StringComparison]::OrdinalIgnoreCase
                        )
                    Assert-True (-not $sameGeneration) 'main hanging status left exact process residue'
                } finally {
                    $candidate.Dispose()
                }
            }
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
            $result = Invoke-HarnessProcess -Executable $script:mainPortablePowerShell `
                -ArgumentValues @('-NoLogo', '-NoProfile', '-NonInteractive', '-Command', "'default-ok'") `
                -WorkingDirectory $mainRepository -Environment $defaultEnvironment `
                -Label 'generic-default-success' -TimeoutMs 5000 -StandardInputText $null
            Assert-Equal 0 $result.exit_code 'generic default success exit code'
            Assert-True ($null -eq $result.deadline) 'generic default success unexpectedly used deadline evidence'
            Assert-True (-not $result.observer_deferred) 'generic default success unexpectedly deferred observation'
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
                    -ExpectedExecutable $expectedColay -ExpectedOverallTimeoutMs 250)

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
                    -ExpectedExecutable $expectedColay -ExpectedOverallTimeoutMs 250)

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
            foreach ($invalid in $invalidEvidence) {
                Assert-Throws -Action {
                    Assert-AuditDaemonReadinessEvidence -ReadinessEvidence $invalid `
                        -ExpectedExecutable $expectedColay -ExpectedOverallTimeoutMs 250
                } -MessagePattern 'readiness|missing|truncated|schema-v1|status identity' `
                    -Message 'parent validator accepted missing or truncated readiness evidence'
            }
        }

        Invoke-TestCase 'audit readiness enforces its monotonic overall deadline' {
            $script:readinessCalls.Clear()
            $script:readinessDocuments.Clear()
            $script:AuditDaemonReadinessTimeoutMs = 35
            $script:AuditDaemonReadinessPollIntervalMs = 1
            $script:AuditDaemonReadinessCleanupReserveMs = 5
            $script:AuditDaemonReadinessExitWaitLimitMs = 4
            $script:AuditDaemonReadinessOutputDrainLimitMs = 1
            $start = New-DaemonDocument -Command daemon_start -State booting -ExecutablePath $expectedColay
            Assert-Throws -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'timeout'
            } -MessagePattern 'timed out after 35ms' -Message 'readiness exceeded its overall deadline without timeout'
            Assert-True ($script:readinessCalls.Count -gt 0) 'timeout fixture did not exercise a status poll'
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
