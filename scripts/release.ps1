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
$Script:ExpectedDeployRepo = '3370842391/kiro.rs-admin.git'

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
    $utf8NoBom = New-Object System.Text.UTF8Encoding
    [System.IO.File]::WriteAllText($Path, $updated, $utf8NoBom)
}

function Restore-ReleaseFiles {
    param(
        [Parameter(Mandatory = $true)][string]$CargoPath,
        [Parameter(Mandatory = $true)][byte[]]$CargoBytes,
        [Parameter(Mandatory = $true)][string]$LockPath,
        [Parameter(Mandatory = $true)][byte[]]$LockBytes
    )

    Invoke-Git -Arguments @('restore', '--staged', '--', 'Cargo.toml', 'Cargo.lock')
    [System.IO.File]::WriteAllBytes($CargoPath, $CargoBytes)
    [System.IO.File]::WriteAllBytes($LockPath, $LockBytes)
}

function Show-ReleasePreview {
    param(
        [Parameter(Mandatory = $true)][string]$Current,
        [Parameter(Mandatory = $true)][string]$Target,
        [Parameter(Mandatory = $true)][string]$LastTag,
        [Parameter(Mandatory = $true)][int]$AheadCount,
        [Parameter(Mandatory = $true)][string[]]$Summary,
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
    $expectedRemotePattern = 'github\.com[/:]' + [regex]::Escape($Script:ExpectedDeployRepo) + '$'
    if ($remoteUrl -notmatch $expectedRemotePattern) {
        throw "deploy push URL 不正确，拒绝发布：$remoteUrl"
    }

    Invoke-Git -Arguments @('fetch', '--tags', 'deploy', '+refs/heads/master:refs/remotes/deploy/master')
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

if ($MyInvocation.InvocationName -eq '.') {
    return
}

$explicitVersion = if ($PSCmdlet.ParameterSetName -eq 'Version') { $Version } else { $null }
Invoke-Release -Bump $Bump -ExplicitVersion $explicitVersion -DryRun:$DryRun
