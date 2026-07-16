$ErrorActionPreference = 'Stop'

$scriptPath = Join-Path $PSScriptRoot '..\..\scripts\release.ps1'
. $scriptPath

function Assert-Equal {
    param(
        [Parameter(Mandatory = $true)]$Actual,
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)][string]$Message
    )
    if ($Actual -ne $Expected) {
        throw "$Message`nExpected: $Expected`nActual: $Actual"
    }
}

function Assert-Throws {
    param(
        [Parameter(Mandatory = $true)][scriptblock]$Action,
        [Parameter(Mandatory = $true)][string]$Message
    )
    try {
        & $Action
    }
    catch {
        return
    }
    throw $Message
}

Assert-Equal (Get-TargetVersion -Current '0.9.8' -Bump 'patch') '0.9.9' 'patch 递增失败'
Assert-Equal (Get-TargetVersion -Current '0.9.8' -Bump 'minor') '0.10.0' 'minor 递增失败'
Assert-Equal (Get-TargetVersion -Current '0.9.8' -Bump 'major') '1.0.0' 'major 递增失败'
Assert-Equal (Get-TargetVersion -Current '0.9.8' -ExplicitVersion '1.2.3') '1.2.3' '显式版本失败'

Assert-Throws { Get-TargetVersion -Current '0.9.8' -ExplicitVersion '0.9.8' } '相同版本应失败'
Assert-Throws { Get-TargetVersion -Current '0.9.8' -ExplicitVersion '0.9.7' } '降级版本应失败'
Assert-Throws { Get-TargetVersion -Current '0.9.8' -ExplicitVersion 'v1.0.0' } '带 v 的版本应失败'
Assert-Throws { Get-TargetVersion -Current '0.9.8' -ExplicitVersion '1.0.0-beta' } '预发布版本应失败'

$summary = @(Get-ReleaseSummary -Subjects @(
    'feat(admin): 增加批量操作',
    'fix(api): 修复429重试',
    'merge(admin): 合并管理端优化',
    'refactor(core): 收敛解析逻辑',
    'docs(release): 记录设计',
    'chore(release): 发布 v0.9.8'
))

Assert-Equal $summary.Count 4 '摘要应过滤 docs 和旧 release 提交'
Assert-Equal $summary[0] '新功能：feat(admin): 增加批量操作' 'feat 分类错误'
Assert-Equal $summary[1] '修复：fix(api): 修复429重试' 'fix 分类错误'
Assert-Equal $summary[2] '合并：merge(admin): 合并管理端优化' 'merge 分类错误'
Assert-Equal $summary[3] '其他：refactor(core): 收敛解析逻辑' '其他分类错误'

$emptySummary = @(Get-ReleaseSummary -Subjects @())
Assert-Equal $emptySummary.Count 1 '空提交范围应生成维护说明'
Assert-Equal $emptySummary[0] '其他：仅版本维护' '空提交范围说明错误'

$nativeWarnings = @()
$nativeOutput = @(Invoke-NativeChecked `
    -FilePath 'powershell.exe' `
    -Arguments @(
        '-NoProfile',
        '-Command',
        "[Console]::Out.WriteLine('stdout-ok'); [Console]::Error.WriteLine('stderr-warning'); exit 0"
    ) `
    -Capture `
    -WarningVariable nativeWarnings)
Assert-Equal $nativeOutput.Count 1 '成功命令的 stderr 不应污染 stdout 捕获结果'
Assert-Equal $nativeOutput[0] 'stdout-ok' '成功命令 stdout 捕获错误'
Assert-Equal $nativeWarnings.Count 1 '成功命令的 stderr 应保留为警告'
Assert-Equal $nativeWarnings[0].ToString() 'stderr-warning' '成功命令警告内容错误'

Assert-Throws {
    Invoke-NativeChecked `
        -FilePath 'powershell.exe' `
        -Arguments @('-NoProfile', '-Command', "[Console]::Error.WriteLine('fatal-error'); exit 7") `
        -Capture
} '非零退出码仍应失败'

Write-Host 'release_script_tests: PASS'
