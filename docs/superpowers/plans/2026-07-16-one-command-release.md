# 一键准备并确认发布 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用一个 PowerShell 命令自动准备版本、同步 lock、预览说明，并在最终确认后安全提交、推送 master 和发布 tag。

**Architecture:** `scripts/release.ps1` 把纯版本计算/摘要函数与有副作用的 Git/Cargo 编排分开；PowerShell 单元测试覆盖纯函数，Bun 源码合约测试固定危险操作的保护条件与顺序。GitHub workflow 在没有版本 CHANGELOG 时调用 Generate Release Notes API；功能分支验证通过后更新主工作区的本地私有发布文档，但该文件不进入公开仓库。

**Tech Stack:** Windows PowerShell 5.1、Git、Cargo、GitHub Actions Bash、GitHub CLI/API、Bun Test

---

## 文件结构

- Create: `scripts/release.ps1` — 用户唯一发布入口，包含版本计算、预检、准备、确认、提交和推送。
- Create: `tests/release/release_script_tests.ps1` — 不依赖 Pester 的 PowerShell 纯函数测试。
- Create: `scripts/release.contract.test.ts` — 读取脚本/workflow 源码，固定发布安全顺序和 Notes 兜底。
- Modify: `.github/workflows/release.yaml:220-257` — CHANGELOG 缺失时调用 GitHub Generate Release Notes API。
- Modify locally only after branch verification: `D:/kiro2api/kiro-rs2/kiro.rs-admin/docs/更新发布流程.md` — 增加一键命令并精简人工步骤；不暂存、不提交，命令在功能分支合并后生效。

## 执行约束

- 功能工作目录：`D:/kiro2api/kiro-rs2/kiro.rs-admin/.worktrees/release-one-command`。
- 只在测试中运行纯函数和 `-DryRun`；开发阶段禁止输入 `RELEASE`，禁止真实 push/tag。
- PowerShell 实现必须兼容本机 Windows PowerShell 5.1，不使用 `pwsh` 专属语法。
- 不使用 `git add -A`、`git reset --hard`、force push 或自动 rebase。
- `docs/更新发布流程.md` 含服务器信息，必须保持 ignored；任何提交中都不能出现该文件。

---

### Task 1: 版本计算与发布摘要纯函数

**Files:**
- Create: `tests/release/release_script_tests.ps1`
- Create: `scripts/release.ps1`

- [ ] **Step 1: 写纯函数失败测试**

创建 `tests/release/release_script_tests.ps1`：

```powershell
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

Write-Host 'release_script_tests: PASS'
```

- [ ] **Step 2: 运行测试并确认脚本缺失**

Run:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests/release/release_script_tests.ps1
```

Expected: FAIL，错误包含找不到 `scripts/release.ps1` 或 `Get-TargetVersion`。

- [ ] **Step 3: 创建最小纯函数脚本**

创建 `scripts/release.ps1`：

```powershell
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
```

- [ ] **Step 4: 运行纯函数测试并确认通过**

Run: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests/release/release_script_tests.ps1`

Expected: 输出 `release_script_tests: PASS`，退出码 0。

- [ ] **Step 5: 提交纯函数与测试**

```powershell
git add -- scripts/release.ps1 tests/release/release_script_tests.ps1
git diff --cached --check
git commit -m "feat(release): 增加版本计算与摘要函数"
```

---

### Task 2: 安全发布编排与 DryRun

**Files:**
- Create: `scripts/release.contract.test.ts`
- Modify: `scripts/release.ps1`

- [ ] **Step 1: 写安全顺序失败测试**

创建 `scripts/release.contract.test.ts`：

```ts
import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function read(path: string): Promise<string> {
  return readFile(path, 'utf8').catch(() => '')
}

describe('one-command release contract', () => {
  test('release script enforces branch, remote, clean tree and explicit confirmation', async () => {
    const script = await read('scripts/release.ps1')

    expect(script).toContain("当前分支必须是 master")
    expect(script).toContain("status', '--porcelain', '--untracked-files=no")
    expect(script).toContain("remote', 'get-url', '--push', 'deploy")
    expect(script).toContain('3370842391/kiro.rs-admin.git')
    expect(script).toContain("merge-base', '--is-ancestor', 'refs/remotes/deploy/master', 'HEAD")
    expect(script).toContain('Read-Host "输入 RELEASE 确认发布')
    expect(script).toContain("$answer -cne 'RELEASE'")
    expect(script).toContain('ReadAllBytes')
    expect(script).toContain('WriteAllBytes')
    expect(script).toContain("restore', '--staged', '--', 'Cargo.toml', 'Cargo.lock")
    expect(script).not.toContain('git add -A')
    expect(script).not.toContain('reset --hard')
    expect(script).not.toContain('--force')
  })

  test('release script pushes master before creating and pushing the tag', async () => {
    const script = await read('scripts/release.ps1')
    const commit = script.indexOf("commit', '-m', \"chore(release): 发布 $tag\"")
    const pushMaster = script.indexOf("push', 'deploy', 'master'")
    const createTag = script.indexOf("tag', '-a', $tag")
    const pushTag = script.indexOf("push', 'deploy', $tag")

    expect(commit).toBeGreaterThan(-1)
    expect(pushMaster).toBeGreaterThan(commit)
    expect(createTag).toBeGreaterThan(pushMaster)
    expect(pushTag).toBeGreaterThan(createTag)
  })

  test('dry-run exits before file preparation and all remote writes', async () => {
    const script = await read('scripts/release.ps1')
    const dryRunExit = script.indexOf("if ($DryRun) {")
    const backup = script.indexOf('[System.IO.File]::ReadAllBytes')
    const pushMaster = script.indexOf("push', 'deploy', 'master'")

    expect(dryRunExit).toBeGreaterThan(-1)
    expect(backup).toBeGreaterThan(dryRunExit)
    expect(pushMaster).toBeGreaterThan(backup)
  })
})
```

- [ ] **Step 2: 运行合约测试并确认编排缺失**

Run: `bun test scripts/release.contract.test.ts`

Expected: 3 tests FAIL，缺少 master/remote/确认/push 顺序字符串。

- [ ] **Step 3: 用完整安全编排替换脚本尾部**

保留 Task 1 的参数和四个纯函数，在 `Get-ReleaseSummary` 后、dot-source guard 前加入以下函数：

```powershell
function Invoke-NativeChecked {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [switch]$Capture
    )

    if ($Capture) {
        $output = @(& $FilePath @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
        if ($exitCode -ne 0) {
            throw "$FilePath $($Arguments -join ' ') 失败（exit=$exitCode）`n$($output -join [Environment]::NewLine)"
        }
        return @($output | ForEach-Object { $_.ToString() })
    }

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath $($Arguments -join ' ') 失败（exit=$LASTEXITCODE）"
    }
}

function Invoke-Git {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)
    Invoke-NativeChecked -FilePath 'git' -Arguments $Arguments
}

function Invoke-GitCapture {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)
    return @(Invoke-NativeChecked -FilePath 'git' -Arguments $Arguments -Capture)
}

function Get-CargoVersion {
    param([Parameter(Mandatory = $true)][string]$CargoText)
    $match = [regex]::Match($CargoText, '(?m)^version\s*=\s*"(?<version>[0-9]+\.[0-9]+\.[0-9]+)"')
    if (-not $match.Success) { throw 'Cargo.toml 未找到根 package 稳定版本号' }
    return $match.Groups['version'].Value
}

function Get-LockVersion {
    param([Parameter(Mandatory = $true)][string]$LockText)
    $match = [regex]::Match(
        $LockText,
        '(?ms)\[\[package\]\]\s*name\s*=\s*"kiro-rs"\s*version\s*=\s*"(?<version>[0-9]+\.[0-9]+\.[0-9]+)"'
    )
    if (-not $match.Success) { throw 'Cargo.lock 未找到 kiro-rs package 版本' }
    return $match.Groups['version'].Value
}

function Set-CargoVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Current,
        [Parameter(Mandatory = $true)][string]$Target
    )

    $text = [System.IO.File]::ReadAllText($Path)
    $match = [regex]::Match($text, '(?m)^version\s*=\s*"(?<version>[0-9]+\.[0-9]+\.[0-9]+)"')
    if (-not $match.Success -or $match.Groups['version'].Value -ne $Current) {
        throw "Cargo.toml 当前版本与预期不一致：$Current"
    }

    $group = $match.Groups['version']
    $updated = $text.Substring(0, $group.Index) + $Target + $text.Substring($group.Index + $group.Length)
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $updated, $utf8NoBom)
}

function Restore-ReleaseFiles {
    param(
        [Parameter(Mandatory = $true)][string]$CargoPath,
        [Parameter(Mandatory = $true)][byte[]]$CargoBytes,
        [Parameter(Mandatory = $true)][string]$LockPath,
        [Parameter(Mandatory = $true)][byte[]]$LockBytes
    )

    & git restore --staged -- Cargo.toml Cargo.lock 2>$null
    [System.IO.File]::WriteAllBytes($CargoPath, $CargoBytes)
    [System.IO.File]::WriteAllBytes($LockPath, $LockBytes)
}

function Show-ReleasePreview {
    param(
        [string]$Current,
        [string]$Target,
        [string]$LastTag,
        [int]$AheadCount,
        [string[]]$Summary,
        [switch]$Prepared
    )

    Write-Host ''
    Write-Host '========== 发布预览 ==========' -ForegroundColor Cyan
    Write-Host "当前版本：$Current"
    Write-Host "目标版本：$Target"
    Write-Host '发布远端：deploy'
    Write-Host "上一个 tag：$LastTag"
    Write-Host "将推送的本地提交数：$AheadCount"
    Write-Host "版本文件状态：$(if ($Prepared) { '已准备，等待确认' } else { 'DryRun，未修改' })"
    Write-Host '发布摘要：'
    foreach ($line in $Summary) { Write-Host "- $line" }
    Write-Host '==============================' -ForegroundColor Cyan
    Write-Host ''
}

function Invoke-Release {
    param(
        [ValidateSet('patch', 'minor', 'major')][string]$Bump = 'patch',
        [string]$ExplicitVersion,
        [switch]$DryRun
    )

    $repoRoot = (Invoke-GitCapture -Arguments @('rev-parse', '--show-toplevel') | Select-Object -First 1).Trim()
    Set-Location -LiteralPath $repoRoot

    $branch = (Invoke-GitCapture -Arguments @('branch', '--show-current') | Select-Object -First 1).Trim()
    if ($branch -ne 'master') { throw "当前分支必须是 master，实际为：$branch" }

    $trackedChanges = @(Invoke-GitCapture -Arguments @('status', '--porcelain', '--untracked-files=no'))
    if ($trackedChanges.Count -gt 0) {
        throw "存在已跟踪未提交改动，请先提交或处理：`n$($trackedChanges -join [Environment]::NewLine)"
    }

    $remoteUrl = (Invoke-GitCapture -Arguments @('remote', 'get-url', '--push', 'deploy') | Select-Object -First 1).Trim()
    if ($remoteUrl -notmatch 'github\.com[/:]3370842391/kiro\.rs-admin\.git$') {
        throw "deploy push URL 不正确，拒绝发布：$remoteUrl"
    }

    Invoke-Git -Arguments @('fetch', '--tags', '--prune', 'deploy', '+master:refs/remotes/deploy/master')
    & git merge-base --is-ancestor refs/remotes/deploy/master HEAD
    if ($LASTEXITCODE -eq 1) {
        throw 'deploy/master 不是当前 HEAD 的祖先，请先同步远端再发布'
    }
    if ($LASTEXITCODE -ne 0) { throw '无法验证 deploy/master 与 HEAD 的祖先关系' }

    $cargoPath = Join-Path $repoRoot 'Cargo.toml'
    $lockPath = Join-Path $repoRoot 'Cargo.lock'
    $cargoText = [System.IO.File]::ReadAllText($cargoPath)
    $lockText = [System.IO.File]::ReadAllText($lockPath)
    $currentVersion = Get-CargoVersion -CargoText $cargoText
    $lockVersion = Get-LockVersion -LockText $lockText
    if ($currentVersion -ne $lockVersion) {
        throw "Cargo.toml ($currentVersion) 与 Cargo.lock ($lockVersion) 版本不一致"
    }

    $targetVersion = Get-TargetVersion -Current $currentVersion -Bump $Bump -ExplicitVersion $ExplicitVersion
    $tag = "v$targetVersion"

    & git show-ref --verify --quiet "refs/tags/$tag"
    if ($LASTEXITCODE -eq 0) { throw "本地 tag 已存在：$tag" }
    if ($LASTEXITCODE -ne 1) { throw "无法检查本地 tag：$tag" }

    & git ls-remote --exit-code --tags deploy "refs/tags/$tag" 1>$null 2>$null
    if ($LASTEXITCODE -eq 0) { throw "远端 tag 已存在：$tag" }
    if ($LASTEXITCODE -ne 2) { throw "无法检查远端 tag：$tag" }

    $lastTag = @(Invoke-GitCapture -Arguments @('tag', '--merged', 'HEAD', '--list', 'v*', '--sort=-v:refname') | Select-Object -First 1)
    $lastTagValue = if ($lastTag.Count -gt 0 -and $lastTag[0]) { $lastTag[0].Trim() } else { '(无历史 tag)' }
    $subjects = if ($lastTagValue -eq '(无历史 tag)') {
        @(Invoke-GitCapture -Arguments @('log', '--format=%s', 'HEAD'))
    }
    else {
        @(Invoke-GitCapture -Arguments @('log', '--format=%s', "$lastTagValue..HEAD"))
    }
    $summary = @(Get-ReleaseSummary -Subjects $subjects)
    $aheadCountText = (Invoke-GitCapture -Arguments @('rev-list', '--count', 'refs/remotes/deploy/master..HEAD') | Select-Object -First 1).Trim()
    $aheadCount = [int]$aheadCountText + 1

    if ($DryRun) {
        Show-ReleasePreview -Current $currentVersion -Target $targetVersion -LastTag $lastTagValue -AheadCount $aheadCount -Summary $summary
        Write-Host 'DryRun 完成：未修改文件、未提交、未推送、未创建 tag。' -ForegroundColor Green
        return
    }

    $cargoBytes = [System.IO.File]::ReadAllBytes($cargoPath)
    $lockBytes = [System.IO.File]::ReadAllBytes($lockPath)
    $releaseCommitted = $false

    try {
        Set-CargoVersion -Path $cargoPath -Current $currentVersion -Target $targetVersion
        Invoke-NativeChecked -FilePath 'cargo' -Arguments @('update', '-p', 'kiro-rs')
        Invoke-NativeChecked -FilePath 'cargo' -Arguments @('metadata', '--locked', '--no-deps', '--format-version', '1') -Capture | Out-Null

        $changedFiles = @(Invoke-GitCapture -Arguments @('diff', '--name-only'))
        $unexpected = @($changedFiles | Where-Object { $_ -notin @('Cargo.toml', 'Cargo.lock') })
        if ($unexpected.Count -gt 0) {
            throw "准备发布时出现范围外改动：$($unexpected -join ', ')"
        }
        if ($changedFiles -notcontains 'Cargo.toml' -or $changedFiles -notcontains 'Cargo.lock') {
            throw '版本准备后 Cargo.toml/Cargo.lock 未同时产生预期改动'
        }

        Show-ReleasePreview -Current $currentVersion -Target $targetVersion -LastTag $lastTagValue -AheadCount $aheadCount -Summary $summary -Prepared
        $answer = Read-Host "输入 RELEASE 确认发布 $tag，其他输入均取消"
        if ($answer -cne 'RELEASE') {
            Restore-ReleaseFiles -CargoPath $cargoPath -CargoBytes $cargoBytes -LockPath $lockPath -LockBytes $lockBytes
            $remaining = @(Invoke-GitCapture -Arguments @('status', '--porcelain', '--untracked-files=no'))
            if ($remaining.Count -gt 0) { throw '取消发布后 tracked worktree 未恢复干净' }
            Write-Host '已取消发布，版本文件已还原。' -ForegroundColor Yellow
            return
        }

        Invoke-Git -Arguments @('add', '--', 'Cargo.toml', 'Cargo.lock')
        Invoke-Git -Arguments @('diff', '--cached', '--check')
        Invoke-Git -Arguments @('commit', '-m', "chore(release): 发布 $tag")
        $releaseCommitted = $true

        Invoke-Git -Arguments @('push', 'deploy', 'master')
        Invoke-Git -Arguments @('tag', '-a', $tag, '-m', "Kiro.rs $tag")
        try {
            Invoke-Git -Arguments @('push', 'deploy', $tag)
        }
        catch {
            throw "tag 推送失败，本地 tag 已保留。修复网络后运行：git push deploy $tag`n$($_.Exception.Message)"
        }

        Write-Host "发布已触发：$tag" -ForegroundColor Green
        Write-Host 'Actions: https://github.com/3370842391/kiro.rs-admin/actions'
    }
    catch {
        if (-not $releaseCommitted) {
            Restore-ReleaseFiles -CargoPath $cargoPath -CargoBytes $cargoBytes -LockPath $lockPath -LockBytes $lockBytes
        }
        throw
    }
}
```

将脚本末尾替换为：

```powershell
if ($MyInvocation.InvocationName -eq '.') {
    return
}

$explicitVersion = if ($PSCmdlet.ParameterSetName -eq 'Version') { $Version } else { $null }
Invoke-Release -Bump $Bump -ExplicitVersion $explicitVersion -DryRun:$DryRun
```

- [ ] **Step 4: 运行 PowerShell 与源码合约测试**

Run:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests/release/release_script_tests.ps1
bun test scripts/release.contract.test.ts
```

Expected: PowerShell 输出 PASS；Bun 3 tests PASS，0 FAIL。

- [ ] **Step 5: 静态检查危险命令和语法**

Run:

```powershell
powershell.exe -NoProfile -Command "[void][scriptblock]::Create([IO.File]::ReadAllText('scripts/release.ps1')); 'PowerShell parse: PASS'"
rg -n "git add -A|reset --hard|push.*--force|origin" scripts/release.ps1
```

Expected: PowerShell parse PASS；`rg` 无输出。

- [ ] **Step 6: 提交发布编排**

```powershell
git add -- scripts/release.ps1 scripts/release.contract.test.ts
git diff --cached --check
git commit -m "feat(release): 增加一键确认发布脚本"
```

---

### Task 3: GitHub 自动生成 Release Notes

**Files:**
- Modify: `scripts/release.contract.test.ts`
- Modify: `.github/workflows/release.yaml:220-257`

- [ ] **Step 1: 写 workflow 兜底顺序失败测试**

在 `scripts/release.contract.test.ts` 的 describe 内加入：

```ts
test('release workflow prefers changelog, then GitHub generated notes, then fixed fallback', async () => {
  const workflow = await read('.github/workflows/release.yaml')
  const changelog = workflow.indexOf('CHANGELOG.md')
  const generated = workflow.indexOf('releases/generate-notes')
  const fallback = workflow.indexOf('Kiro.rs ${VERSION}')
  const footer = workflow.indexOf('在线更新：admin 面板')

  expect(changelog).toBeGreaterThan(-1)
  expect(generated).toBeGreaterThan(changelog)
  expect(fallback).toBeGreaterThan(generated)
  expect(footer).toBeGreaterThan(fallback)
  expect(workflow).toContain("--jq '.body // empty'")
  expect(workflow).toContain('GH_TOKEN: ${{ github.token }}')
})
```

- [ ] **Step 2: 运行测试并确认 Generate Notes 尚不存在**

Run: `bun test scripts/release.contract.test.ts`

Expected: 新增测试 FAIL，`generated` 为 `-1`。

- [ ] **Step 3: 在 CHANGELOG 与固定说明之间加入 GitHub 自动说明**

先给 `Write release notes` step 增加 GitHub CLI 鉴权：

```yaml
      - name: Write release notes
        env:
          GH_TOKEN: ${{ github.token }}
        shell: bash
```

在 `.github/workflows/release.yaml` 的 `if [ ! -s RELEASE_NOTES.md ]; then` 固定兜底块之前加入：

```yaml
          if [ ! -s RELEASE_NOTES.md ]; then
            AUTO_NOTES_FILE="RELEASE_NOTES.generated.md"
            if gh api --method POST \
              "repos/${GITHUB_REPOSITORY}/releases/generate-notes" \
              -f tag_name="${{ needs.prepare.outputs.tag }}" \
              -f target_commitish="${{ needs.prepare.outputs.target }}" \
              --jq '.body // empty' > "$AUTO_NOTES_FILE"; then
              if [ -s "$AUTO_NOTES_FILE" ]; then
                mv "$AUTO_NOTES_FILE" RELEASE_NOTES.md
              else
                rm -f "$AUTO_NOTES_FILE"
              fi
            else
              rm -f "$AUTO_NOTES_FILE"
              echo "::warning::GitHub automatic release notes failed; using fixed fallback"
            fi
          fi

          if [ ! -s RELEASE_NOTES.md ]; then
```

保留现有固定 `cat > RELEASE_NOTES.md` 内容和最后追加在线更新/footer 的逻辑不变。

- [ ] **Step 4: 运行合约测试并检查 YAML 差异**

Run:

```powershell
bun test scripts/release.contract.test.ts
git diff --check -- .github/workflows/release.yaml scripts/release.contract.test.ts
```

Expected: 4 tests PASS，0 FAIL；diff check 无输出。

- [ ] **Step 5: 提交 workflow 说明生成**

```powershell
git add -- .github/workflows/release.yaml scripts/release.contract.test.ts
git diff --cached --check
git commit -m "feat(release): 自动生成GitHub发布说明"
```

---

### Task 4: 更新本地私有发布文档

**Files:**
- Modify locally only: `D:/kiro2api/kiro-rs2/kiro.rs-admin/docs/更新发布流程.md`

执行本任务前必须确认 Task 1-3 的脚本、测试和 workflow 已在功能分支通过验证。文档会先于功能分支合并出现在主工作区，因此交付时必须提示“一键命令需合并后才能使用”。

- [ ] **Step 1: 确认文档仍被 ignore 且旧速查存在错误**

Run:

```powershell
$repo='D:/kiro2api/kiro-rs2/kiro.rs-admin'
git -C $repo check-ignore -q docs/更新发布流程.md
if ($LASTEXITCODE -ne 0) { throw '私有发布文档未被 ignore，停止操作' }
rg -n "步骤 2：升版本号|cargo update -p kiro-rs|^3$" "$repo/docs/更新发布流程.md"
```

Expected: ignore 检查通过；输出旧手工步骤以及错误的单独数字 `3`。

- [ ] **Step 2: 用一键流程替换日常发布章节**

在主工作区本地文档中，将“## 二、日常更新流程”到“## 三、四条铁律”之前替换为：

````markdown
## 二、日常更新流程（一条命令）

在 PowerShell 中运行：

```powershell
cd D:\kiro2api\kiro-rs2\kiro.rs-admin
.\scripts\release.ps1
```

脚本会自动完成：

1. 检查 `master`、`deploy` 远端、工作区和远端同步状态。
2. 默认把 patch 版本加一，例如 `0.9.8 → 0.9.9`。
3. 更新 `Cargo.toml` 并同步 `Cargo.lock`。
4. 汇总上一个 tag 以来的代码改动，显示发布预览。
5. 等待最终确认。

确认预览无误后输入大写：

```text
RELEASE
```

脚本随后自动提交版本、推送 `deploy master`、创建并推送 tag。其他任何输入都会取消，并还原版本文件。

### 常用参数

```powershell
# 只预览，不改文件、不提交、不推送
.\scripts\release.ps1 -DryRun

# 升 minor：0.9.8 → 0.10.0
.\scripts\release.ps1 -Bump minor

# 升 major：0.9.8 → 1.0.0
.\scripts\release.ps1 -Bump major

# 指定版本
.\scripts\release.ps1 -Version 1.2.3
```

### 推送完成后

1. 打开 `https://github.com/3370842391/kiro.rs-admin/actions`，等待 `release: vX.Y.Z` 变成绿勾。
2. 打开 RS 管理端的「在线更新」，点击「立即检查」。
3. 显示新版本后点击「更新并重启」。
4. 等约 10 秒刷新页面，确认“当前版本”已更新。

GitHub Release Notes 会优先读取对应版本 CHANGELOG；没有时由 GitHub 自动生成，不需要手写发布说明。

````

- [ ] **Step 3: 将“命令速查”改成真正的一键命令**

将“## 四、命令速查”章节内容替换为：

````markdown
## 四、命令速查

```powershell
cd D:\kiro2api\kiro-rs2\kiro.rs-admin
.\scripts\release.ps1
```

想先确认版本和改动说明时：

```powershell
.\scripts\release.ps1 -DryRun
```

如果 master 已成功推送、只有 tag 推送失败，按脚本提示重试：

```powershell
git push deploy vX.Y.Z
```

````

- [ ] **Step 4: 检查本地文档内容和隐私边界**

Run:

```powershell
$repo='D:/kiro2api/kiro-rs2/kiro.rs-admin'
rg -n "\.\\scripts\\release\.ps1|-DryRun|-Bump minor|-Version 1\.2\.3|RELEASE|git push deploy vX\.Y\.Z" "$repo/docs/更新发布流程.md"
rg -n "^3$" "$repo/docs/更新发布流程.md"
git -C $repo status --short
```

Expected: 第一条输出所有一键发布关键词；第二条无输出；Git status 不出现 `docs/更新发布流程.md`。

- [ ] **Step 5: 不提交私有文档**

不要对该文件运行 `git add -f`。在任务记录中注明：

```text
本地私有文档已更新；受 /docs/ ignore 保护，未进入公开提交。
```

---

### Task 5: 全量验证与 DryRun 烟雾测试

**Files:**
- Verify: `scripts/release.ps1`
- Verify: `tests/release/release_script_tests.ps1`
- Verify: `scripts/release.contract.test.ts`
- Verify: `.github/workflows/release.yaml`

- [ ] **Step 1: 运行所有发布相关测试**

Run:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests/release/release_script_tests.ps1
bun test scripts/release.contract.test.ts
```

Expected: PowerShell PASS；Bun 4 tests PASS，0 FAIL。

- [ ] **Step 2: 运行项目基线回归**

Run:

```powershell
cargo metadata --locked --no-deps --format-version 1 | Out-Null
cd admin-ui
bun test
cd ..
```

Expected: Cargo metadata 退出码 0；现有 91 项前端测试全部通过。

- [ ] **Step 3: 在临时 master 克隆中运行真实 DryRun**

Run:

```powershell
$source=(Resolve-Path '.').Path
$tempRoot=Join-Path ([System.IO.Path]::GetTempPath()) ('kiro-release-dryrun-' + [guid]::NewGuid().ToString('N'))
try {
    git clone --shared --branch feat/release-one-command $source $tempRoot
    if ($LASTEXITCODE -ne 0) { throw '临时 clone 失败' }
    git -C $tempRoot branch -M master
    git -C $tempRoot remote remove origin
    git -C $tempRoot remote add deploy https://github.com/3370842391/kiro.rs-admin.git

    $beforeCargo=[System.IO.File]::ReadAllBytes((Join-Path $tempRoot 'Cargo.toml'))
    $beforeLock=[System.IO.File]::ReadAllBytes((Join-Path $tempRoot 'Cargo.lock'))
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $tempRoot 'scripts/release.ps1') -DryRun
    if ($LASTEXITCODE -ne 0) { throw 'DryRun 失败' }

    $afterCargo=[System.IO.File]::ReadAllBytes((Join-Path $tempRoot 'Cargo.toml'))
    $afterLock=[System.IO.File]::ReadAllBytes((Join-Path $tempRoot 'Cargo.lock'))
    if ([Convert]::ToBase64String($beforeCargo) -ne [Convert]::ToBase64String($afterCargo)) { throw 'DryRun 修改了 Cargo.toml' }
    if ([Convert]::ToBase64String($beforeLock) -ne [Convert]::ToBase64String($afterLock)) { throw 'DryRun 修改了 Cargo.lock' }
    if (git -C $tempRoot status --porcelain --untracked-files=no) { throw 'DryRun 产生 tracked 改动' }
    Write-Host 'DryRun smoke: PASS'
}
finally {
    if (Test-Path -LiteralPath $tempRoot) {
        $resolved=[System.IO.Path]::GetFullPath($tempRoot)
        $allowed=[System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
        if (-not $resolved.StartsWith($allowed, [StringComparison]::OrdinalIgnoreCase)) { throw "拒绝删除临时目录：$resolved" }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
```

Expected: 输出发布预览和 `DryRun smoke: PASS`；没有版本文件变化、commit、tag 或 push。

- [ ] **Step 4: 审计最终差异范围**

Run:

```powershell
git diff master...HEAD --check
git diff master...HEAD --name-only
git status --short --branch
```

Expected:

- diff check 无输出。
- 功能分支只包含 `scripts/release.ps1`、两份发布测试、`release.yaml` 和设计/计划文档。
- 工作树干净。
- 主工作区私有 `docs/更新发布流程.md` 已更新但因 ignore 不出现在 Git status。

- [ ] **Step 5: 记录交付摘要**

最终报告必须包含：

```text
新增的一键命令：.\scripts\release.ps1
默认行为：patch +1，输入 RELEASE 后才提交/推送/tag
可选参数：-DryRun、-Bump minor/major、-Version
GitHub Notes：CHANGELOG → 自动生成 → 固定兜底
私有文档：已更新，未进入公开提交
客户影响：无运行时、对话、Token 或首字节变化
```
