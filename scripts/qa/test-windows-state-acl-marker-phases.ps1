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
        'ConvertTo-AuditDaemonDocumentIdentity',
        'Assert-AuditDaemonReadinessDeadline',
        'Wait-AuditDaemonReadiness'
    )
    Invoke-TestCase 'generated audit child exposes strict bounded readiness helpers' {
        foreach ($name in $requiredChildFunctions) {
            Assert-True ($childFunctionNames -ccontains $name) "generated audit child is missing function $name"
        }
        $childText = Get-Content -Raw -LiteralPath $childPath
        Assert-True ($childText -match '\$script:AuditDaemonReadinessTimeoutMs\s*=\s*5000(?:\D|$)') `
            'generated audit child readiness deadline is not exactly 5000ms'
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
        $script:AuditDaemonReadinessCleanupReserveMs = 10
        $script:readinessDocuments = [System.Collections.Generic.Queue[object]]::new()
        $script:readinessCalls = [System.Collections.Generic.List[object]]::new()

        function Invoke-ColayDocument {
            param([string]$Repository, [string[]]$Arguments, [string]$Label, [int]$TimeoutMs = 30000)
            $script:readinessCalls.Add([pscustomobject]@{
                repository = $Repository
                arguments = @($Arguments)
                label = $Label
                timeout_ms = $TimeoutMs
            })
            if ($script:readinessDocuments.Count -eq 0) {
                return New-DaemonDocument -Command daemon_status -State booting -ExecutablePath $expectedColay
            }
            return $script:readinessDocuments.Dequeue()
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
                Assert-True ($call.timeout_ms -gt 0 -and $call.timeout_ms -lt 5000) `
                    'readiness status command did not receive a bounded remaining timeout'
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
                Assert-Throws -Action {
                    Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                        -ExpectedExecutable $expectedColay -Repository $repository -Label "drift-$drift"
                } -MessagePattern 'identity drift|executable path mismatch' `
                    -Message "readiness accepted $drift identity drift"
            }
        }

        Invoke-TestCase 'audit readiness rejects state and phase mismatch' {
            $start = New-DaemonDocument -Command daemon_start -State booting -Phase probing `
                -ExecutablePath $expectedColay
            Assert-Throws -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'state-phase'
            } -MessagePattern 'state/phase mismatch' -Message 'readiness accepted state/phase mismatch'
        }

        Invoke-TestCase 'audit readiness rejects terminal state before registration' {
            $start = New-DaemonDocument -Command daemon_start -State failed -ExecutablePath $expectedColay
            Assert-Throws -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'terminal'
            } -MessagePattern 'terminal|non-progress' -Message 'readiness accepted terminal state'
        }

        Invoke-TestCase 'audit readiness rejects malformed unsafe PID' {
            $start = New-DaemonDocument -Command daemon_start -State online -ProcessId 4242.5 `
                -ExecutablePath $expectedColay
            Assert-Throws -Action {
                Wait-AuditDaemonReadiness -DaemonStartDocument $start `
                    -ExpectedExecutable $expectedColay -Repository $repository -Label 'unsafe-pid'
            } -MessagePattern 'PID|integer' -Message 'readiness accepted fractional PID'
        }

        Invoke-TestCase 'audit readiness rejects malformed schema, command, and UUID documents' {
            $wrongSchema = New-DaemonDocument -Command daemon_start -State online -ExecutablePath $expectedColay
            $wrongSchema.schema_version = '2'
            $wrongCommand = New-DaemonDocument -Command daemon_status -State online -ExecutablePath $expectedColay
            $nonCanonicalUuid = New-DaemonDocument -Command daemon_start -State online `
                -InstanceId '019F8B42-8E29-7C2D-9D6F-9F48C593B9D1' -ExecutablePath $expectedColay
            foreach ($fixture in @($wrongSchema, $wrongCommand, $nonCanonicalUuid)) {
                Assert-Throws -Action {
                    Wait-AuditDaemonReadiness -DaemonStartDocument $fixture `
                        -ExpectedExecutable $expectedColay -Repository $repository -Label 'malformed'
                } -MessagePattern 'schema-v1|canonical UUID' -Message 'readiness accepted a malformed start document'
            }
        }

        Invoke-TestCase 'audit readiness enforces its monotonic overall deadline' {
            $script:readinessCalls.Clear()
            $script:readinessDocuments.Clear()
            $script:AuditDaemonReadinessTimeoutMs = 35
            $script:AuditDaemonReadinessPollIntervalMs = 1
            $script:AuditDaemonReadinessCleanupReserveMs = 5
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
