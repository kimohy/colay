#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory,

    [switch]$IncludeTestChild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($env:OS -ne 'Windows_NT') {
    throw 'windows-process-audit-helper can only be compiled on Windows'
}

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$helperSource = Join-Path $scriptRoot 'windows-process-audit-helper.cs'
$testChildSource = Join-Path $scriptRoot 'windows-process-audit-test-child.cs'
foreach ($required in @($helperSource)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "required source file is missing: $required"
    }
}

$compilerCandidates = @(
    (Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'),
    (Join-Path $env:WINDIR 'Microsoft.NET\Framework\v4.0.30319\csc.exe')
)
$compiler = $compilerCandidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if ($null -eq $compiler) {
    throw 'inbox .NET Framework C# compiler not found; expected Framework64 or Framework v4.0.30319 csc.exe'
}
if ($compiler -notlike '*\Framework64\*') {
    throw "64-bit inbox C# compiler is required for the native DEBUG_EVENT layout: $compiler"
}

$resolvedOutput = [System.IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $resolvedOutput -Force | Out-Null

function Invoke-CSharpCompiler {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Output,
        [string[]]$AdditionalCompilerArguments = @()
    )

    $compilerArguments = @(
        '/nologo',
        '/target:exe',
        '/platform:x64',
        '/checked+',
        '/warnaserror+',
        '/optimize+',
        "/out:$Output",
        $Source
    )
    $compilerArguments += $AdditionalCompilerArguments
    & $compiler @compilerArguments
    if ($LASTEXITCODE -ne 0) {
        throw "C# compiler failed with exit code $LASTEXITCODE for $Source"
    }
    if (-not (Test-Path -LiteralPath $Output -PathType Leaf)) {
        throw "C# compiler reported success without producing $Output"
    }
}

$helperOutput = Join-Path $resolvedOutput 'windows-process-audit-helper.exe'
Invoke-CSharpCompiler -Source $helperSource -Output $helperOutput

if ($IncludeTestChild) {
    if (-not (Test-Path -LiteralPath $testChildSource -PathType Leaf)) {
        throw "required test source file is missing: $testChildSource"
    }
    $testHelperOutput = Join-Path $resolvedOutput 'windows-process-audit-helper-test.exe'
    Invoke-CSharpCompiler `
        -Source $helperSource `
        -Output $testHelperOutput `
        -AdditionalCompilerArguments @('/define:PROCESS_AUDIT_TESTING')
    $testChildOutput = Join-Path $resolvedOutput 'windows-process-audit-test-child.exe'
    Invoke-CSharpCompiler -Source $testChildSource -Output $testChildOutput
}

Write-Output $helperOutput
