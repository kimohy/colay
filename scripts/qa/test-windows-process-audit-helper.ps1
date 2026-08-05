#requires -Version 5.1

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$buildScript = Join-Path $scriptRoot 'build-windows-process-audit-helper.ps1'
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("colay-process-audit-test-" + [guid]::NewGuid().ToString('N'))
$helper = Join-Path $tempRoot 'windows-process-audit-helper.exe'
$testChild = Join-Path $tempRoot 'windows-process-audit-test-child.exe'

function Assert-Equal {
    param($Expected, $Actual, [string]$Message)
    if ($Expected -ne $Actual) {
        throw "$Message (expected '$Expected', actual '$Actual')"
    }
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) {
        throw $Message
    }
}

function Decode-Value {
    param([string]$Value)
    return [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Value))
}

function Assert-NoRecordedResidue {
    param($Evidence, [string]$Label)
    Start-Sleep -Milliseconds 250
    foreach ($record in @($Evidence.process_starts)) {
        $current = Get-CimInstance Win32_Process -Filter "ProcessId=$([int]$record.process_id)" -ErrorAction SilentlyContinue
        if ($null -eq $current) {
            continue
        }
        $recordedPath = ([string]$record.path) -replace '^\\\\\?\\', ''
        $currentPath = ([string]$current.ExecutablePath) -replace '^\\\\\?\\', ''
        if ($recordedPath.Equals($currentPath, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "$Label left recorded pid $($record.process_id) running at $currentPath"
        }
    }
    $tempResidual = @(Get-CimInstance Win32_Process | Where-Object {
        $_.Name -in @('windows-process-audit-helper.exe', 'windows-process-audit-test-child.exe') -and
        ([string]$_.ExecutablePath).StartsWith($tempRoot, [System.StringComparison]::OrdinalIgnoreCase)
    })
    Assert-Equal 0 $tempResidual.Count "$Label temp-root residual process count"
}

function Invoke-Audit {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyString()][string[]]$ChildArguments,
        [int]$TimeoutMs = 30000,
        [string]$EnvironmentMode = 'inherit',
        [hashtable]$EnvironmentOverrides = @{},
        [string[]]$AdditionalAuditArguments = @()
    )

    $caseId = [guid]::NewGuid().ToString('N')
    $evidence = Join-Path $tempRoot "$caseId.json"
    $stdout = Join-Path $tempRoot "$caseId.stdout"
    $stderr = Join-Path $tempRoot "$caseId.stderr"
    $arguments = @(
        '--evidence', $evidence,
        '--timeout-ms', [string]$TimeoutMs,
        '--working-directory', $tempRoot,
        '--environment', $EnvironmentMode
    )
    foreach ($name in @($EnvironmentOverrides.Keys | Sort-Object)) {
        $arguments += @('--env', [string]$name, [string]$EnvironmentOverrides[$name])
    }
    foreach ($childArgument in $ChildArguments) {
        $argumentBytes = [System.Text.Encoding]::UTF8.GetBytes($childArgument)
        $framedBytes = New-Object byte[] ($argumentBytes.Length + 1)
        [Array]::Copy($argumentBytes, 0, $framedBytes, 1, $argumentBytes.Length)
        $encoded = [Convert]::ToBase64String($framedBytes)
        $arguments += @('--child-argument-base64', $encoded)
    }
    $arguments += $AdditionalAuditArguments
    $arguments += @('--', $testChild)

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $helper @arguments 1> $stdout 2> $stderr
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    Assert-True (Test-Path -LiteralPath $evidence -PathType Leaf) 'audit evidence was not written'
    return [pscustomobject]@{
        ExitCode = $exitCode
        Evidence = Get-Content -Raw -LiteralPath $evidence | ConvertFrom-Json
        Stdout = $stdout
        Stderr = $stderr
    }
}

New-Item -ItemType Directory -Path $tempRoot | Out-Null
try {
    & $buildScript -OutputDirectory $tempRoot -IncludeTestChild
    Assert-True (Test-Path -LiteralPath $helper -PathType Leaf) 'helper build did not produce an executable'
    Assert-True (Test-Path -LiteralPath $testChild -PathType Leaf) 'test-child build did not produce an executable'

    $contractArguments = @('', 'plain', 'with spaces', 'quote"tail', 'slash\', 'slashes\\"quote')
    $contract = Invoke-Audit `
        -ChildArguments (@('echo-contract') + $contractArguments) `
        -EnvironmentMode clear `
        -EnvironmentOverrides @{
            SystemRoot = $env:SystemRoot
            PROCESS_AUDIT_TEST = 'value with spaces'
        }
    Assert-Equal 0 $contract.ExitCode 'contract audit exit code'
    Assert-Equal 'success' $contract.Evidence.status 'contract audit status'
    Assert-Equal 0 $contract.Evidence.child_exit_code 'contract child exit code'
    Assert-Equal 0 @($contract.Evidence.active_process_ids_at_finish).Count 'contract final active set'
    $lines = @(Get-Content -LiteralPath $contract.Stdout)
    $cwdLine = @($lines | Where-Object { $_ -like 'cwd=*' })[0]
    $envLine = @($lines | Where-Object { $_ -like 'env=*' })[0]
    Assert-Equal $tempRoot (Decode-Value ($cwdLine.Substring(4))) 'working directory round trip'
    Assert-Equal 'value with spaces' (Decode-Value ($envLine.Substring(4))) 'environment round trip'
    $actualArguments = @($lines | Where-Object { $_ -like 'arg=*' } | ForEach-Object { Decode-Value (($_).Substring(4)) })
    Assert-Equal $contractArguments.Count $actualArguments.Count 'argument count round trip'
    for ($index = 0; $index -lt $contractArguments.Count; $index++) {
        Assert-Equal $contractArguments[$index] $actualArguments[$index] "argument $index round trip"
    }

    $descendant = Invoke-Audit -ChildArguments @('spawn-where')
    Assert-Equal 0 $descendant.ExitCode 'descendant audit exit code'
    $startNames = @($descendant.Evidence.process_starts | ForEach-Object { [System.IO.Path]::GetFileName([string]$_.path).ToLowerInvariant() })
    Assert-True ($startNames -contains 'windows-process-audit-test-child.exe') 'root test process was not audited'
    Assert-True ($startNames -contains 'where.exe') 'short-lived descendant was not audited'
    Assert-Equal @($descendant.Evidence.process_starts).Count @($descendant.Evidence.process_exits).Count 'start/exit evidence cardinality'

    $rejected = Invoke-Audit -ChildArguments @('spawn-where') -AdditionalAuditArguments @('--forbid-image', 'where.exe')
    Assert-Equal 125 $rejected.ExitCode 'benign forbidden-image rejection exit code'
    Assert-Equal 'failed' $rejected.Evidence.status 'benign forbidden-image rejection status'
    Assert-True ([string]$rejected.Evidence.observer_error -match 'forbidden process image observed: where.exe') 'benign forbidden-image rejection reason'
    Assert-Equal 0 @($rejected.Evidence.active_process_ids_at_finish).Count 'rejected audit final active set'
    Assert-NoRecordedResidue $rejected.Evidence 'rejected audit'

    $exactExit = Invoke-Audit -ChildArguments @('exit', '7')
    Assert-Equal 7 $exactExit.ExitCode 'successful observation must preserve child exit code'
    Assert-Equal 'success' $exactExit.Evidence.status 'nonzero child exit is not an observer failure'
    Assert-Equal 7 $exactExit.Evidence.child_exit_code 'nonzero child exit evidence'

    $timedOut = Invoke-Audit -ChildArguments @('sleep', '5000') -TimeoutMs 100
    Assert-Equal 125 $timedOut.ExitCode 'timeout must use observer-failure exit code'
    Assert-Equal 'failed' $timedOut.Evidence.status 'timeout evidence status'
    Assert-True ([string]$timedOut.Evidence.observer_error -match 'timeout') 'timeout evidence reason'
    Assert-NoRecordedResidue $timedOut.Evidence 'timed-out audit'

    $flood = Invoke-Audit -ChildArguments @('flood', '1048576')
    Assert-Equal 0 $flood.ExitCode 'stdout/stderr flood audit exit code'
    Assert-True ((Get-Item -LiteralPath $flood.Stdout).Length -ge 1048576) 'stdout drain lost flood output'
    Assert-True ((Get-Item -LiteralPath $flood.Stderr).Length -ge 1048576) 'stderr drain lost flood output'

    $source = Get-Content -Raw -LiteralPath (Join-Path $scriptRoot 'windows-process-audit-helper.cs')
    Assert-True (-not $source.Contains('DEBUG_ONLY_THIS_PROCESS')) 'helper must never name or use DEBUG_ONLY_THIS_PROCESS'
    Assert-True ($source.Contains('DEBUG_PROCESS')) 'helper must enable DEBUG_PROCESS'
    Assert-True ($source.Contains('CREATE_SUSPENDED')) 'helper must assign the cleanup job before child execution'
    Assert-True ($source.Contains('EXTENDED_STARTUPINFO_PRESENT')) 'helper must use STARTUPINFOEX'
    Assert-True ($source.Contains('PROC_THREAD_ATTRIBUTE_HANDLE_LIST')) 'helper must restrict inherited handles'
    Assert-True ($source.Contains('InitializeProcThreadAttributeList')) 'helper must initialize an attribute list'
    Assert-True ($source.Contains('UpdateProcThreadAttribute')) 'helper must install the explicit handle list'
    Assert-True ($source.Contains('DeleteProcThreadAttributeList')) 'helper must delete the attribute list'
    Assert-True ($source.Contains('initialBreakpoints.Remove(debugEvent.ProcessId)')) 'helper must forget exited PIDs before PID reuse'
    Assert-True ($source.Contains('TerminateProcess(rootProcess')) 'helper must terminate an unassigned suspended root directly'

    Write-Output 'windows process audit helper tests passed'
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
