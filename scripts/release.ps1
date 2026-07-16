[CmdletBinding(DefaultParameterSetName = 'Bump')]
param(
    [Parameter(ParameterSetName = 'Bump')]
    [ValidateSet('patch', 'minor', 'major')]
    [string]$Bump = 'patch',

    [Parameter(Mandatory = $true, ParameterSetName = 'Version')]
    [string]$Version,

    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function ConvertTo-VersionParts {
    param([Parameter(Mandatory = $true)][string]$Value)

    if ($Value -notmatch '^([0-9]+)\.([0-9]+)\.([0-9]+)$') {
        throw "版本号必须是稳定三段式语义版本，例如 0.9.9：$Value"
    }

    return @([int]$Matches[1], [int]$Matches[2], [int]$Matches[3])
}

function Compare-SemVer {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )

    $leftParts = @(ConvertTo-VersionParts -Value $Left)
    $rightParts = @(ConvertTo-VersionParts -Value $Right)
    for ($index = 0; $index -lt 3; $index += 1) {
        if ($leftParts[$index] -lt $rightParts[$index]) { return -1 }
        if ($leftParts[$index] -gt $rightParts[$index]) { return 1 }
    }
    return 0
}

function Get-TargetVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Current,
        [ValidateSet('patch', 'minor', 'major')][string]$Bump = 'patch',
        [string]$ExplicitVersion
    )

    $parts = @(ConvertTo-VersionParts -Value $Current)
    if ($ExplicitVersion) {
        [void](ConvertTo-VersionParts -Value $ExplicitVersion)
        if ((Compare-SemVer -Left $ExplicitVersion -Right $Current) -le 0) {
            throw "目标版本必须大于当前版本 $Current：$ExplicitVersion"
        }
        return $ExplicitVersion
    }

    switch ($Bump) {
        'patch' { $parts[2] += 1 }
        'minor' { $parts[1] += 1; $parts[2] = 0 }
        'major' { $parts[0] += 1; $parts[1] = 0; $parts[2] = 0 }
    }
    return ($parts -join '.')
}

function Get-ReleaseSummary {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [string[]]$Subjects
    )

    $result = New-Object System.Collections.Generic.List[string]
    foreach ($subject in $Subjects) {
        if ($subject -match '^docs(?:\([^)]*\))?:' -or $subject -match '^chore\(release\):') {
            continue
        }

        $label = '其他'
        if ($subject -match '^feat(?:\([^)]*\))?:') { $label = '新功能' }
        elseif ($subject -match '^fix(?:\([^)]*\))?:') { $label = '修复' }
        elseif ($subject -match '^merge(?:\([^)]*\))?:') { $label = '合并' }
        $result.Add("$label`：$subject")
    }

    if ($result.Count -eq 0) {
        $result.Add('其他：仅版本维护')
    }
    return $result.ToArray()
}

if ($MyInvocation.InvocationName -eq '.') {
    return
}

throw '发布编排尚未实现'
