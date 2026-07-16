# 一键准备并确认发布设计

## 1. 背景

当前发布流程要求人工完成版本号递增、Cargo.lock 同步、提交说明、推送 master、创建并推送 tag。步骤本身稳定，但重复输入多，容易出现版本号/tag 不一致、忘记更新 lock、推错远端或漏写说明。

仓库现状：

- 发布远端是 `deploy`，`origin` 指向上游仓库，不能用于发布。
- 当前版本来自根目录 `Cargo.toml`，`Cargo.lock` 中的 `kiro-rs` 版本必须同步。
- `.github/workflows/release.yaml` 在 `v*` tag 推送后构建并发布 Linux musl x64 资产。
- Release workflow 已支持从对应版本的 `CHANGELOG.md` 章节读取说明；不存在该章节时目前只生成固定兜底文本。
- `docs/更新发布流程.md` 含服务器信息且受 `/docs/` ignore 规则保护，只作为本地运维文档，不进入公开仓库。

## 2. 目标

日常发布只需执行：

```powershell
.\scripts\release.ps1
```

脚本自动准备 patch 版本、同步 lock、汇总变更并显示发布预览。只有用户在最后输入精确确认词 `RELEASE` 后，脚本才允许执行提交、push 和 tag 操作。

同时支持：

```powershell
# 仅预览，不改文件、不提交、不推送
.\scripts\release.ps1 -DryRun

# 0.9.8 -> 0.10.0
.\scripts\release.ps1 -Bump minor

# 0.9.8 -> 1.0.0
.\scripts\release.ps1 -Bump major

# 指定版本
.\scripts\release.ps1 -Version 1.2.3
```

`-Bump` 与 `-Version` 互斥；不传时等价于 `-Bump patch`。

## 3. 变更范围

### 3.1 进入公开仓库

- 新增 `scripts/release.ps1`：一键发布入口。
- 修改 `.github/workflows/release.yaml`：无对应 CHANGELOG 章节时，优先调用 GitHub Generate Release Notes API。
- 新增发布脚本/工作流合约测试，固定危险操作的顺序和关键保护条件。

### 3.2 仅保留在本地

- 更新 `docs/更新发布流程.md`，将日常流程精简为“一条命令 → 等待 Actions → 面板更新”。
- 文档首页直接展示 `.\scripts\release.ps1`、`-DryRun`、`-Bump` 和 `-Version` 示例。
- 该文件继续受 `/docs/` ignore 保护，不使用 `git add -f`，避免服务器信息进入公开 GitHub 仓库。
- ignored 文件不会自动出现在 feature worktree；因此脚本和 workflow 在功能分支验证通过后，再更新主工作区中的这份本地文档。该更新不进入功能分支提交；在功能分支尚未合并前，交付报告必须明确提示“一键命令需合并到 master 后才能使用”。

## 4. 脚本接口与版本计算

脚本使用 PowerShell 参数集保证调用无歧义：

- 默认参数集：`-Bump patch|minor|major`，默认 `patch`。
- 显式版本参数集：`-Version <semver>`。
- 公共开关：`-DryRun`。

版本只允许三段式稳定语义版本 `X.Y.Z`，三个分量必须是非负十进制整数；本次脚本不生成 prerelease/build metadata。显式版本必须严格大于当前 `Cargo.toml` 版本。

递增规则：

- patch：`0.9.8 → 0.9.9`
- minor：`0.9.8 → 0.10.0`
- major：`0.9.8 → 1.0.0`

脚本只替换根 `Cargo.toml` 的 `[package]` 首个 `version = "..."`，不修改依赖版本。文件写入使用无 BOM UTF-8，并保留原有换行风格。

## 5. 发布前检查

脚本在修改任何文件前依次检查：

1. 当前目录位于 Git 仓库根目录，当前分支必须是 `master`。
2. 已跟踪文件必须干净；未跟踪/被 ignore 文件不会被暂存，也不阻止发布。
3. `deploy` 远端必须存在，push URL 必须指向 `3370842391/kiro.rs-admin.git`；拒绝使用 `origin`。
4. 执行 `git fetch deploy master --tags --prune`，刷新只读远端状态。
5. `deploy/master` 必须是当前 HEAD 的祖先；否则 push 会非快进，脚本停止且不修改文件。
6. 当前 `Cargo.toml` 版本必须与 `Cargo.lock` 中 `kiro-rs` package 版本一致。
7. 目标 tag 在本地和 `deploy` 远端都不能存在。
8. 上一个版本 tag 必须能解析；若仓库没有历史 tag，摘要范围退化为当前可达提交。

`-DryRun` 仍执行上述只读检查和 fetch，但不写版本文件、不运行提交、不 push、不创建 tag。

## 6. 准备阶段

非 DryRun 模式在内存中保存 `Cargo.toml` 和 `Cargo.lock` 的原始字节，然后：

1. 更新 `Cargo.toml` 目标版本。
2. 运行 `cargo update -p kiro-rs` 同步根 package lock 版本。
3. 运行 `cargo metadata --locked --no-deps --format-version 1` 验证 lock 可被 `--locked` 使用。
4. 校验实际 tracked diff 只能包含 `Cargo.toml` 和 `Cargo.lock`；发现其他文件变化立即停止。
5. 从上一个 tag 到当前 HEAD 读取提交主题，生成本地发布预览：忽略纯 `docs` 和旧 `chore(release)` 提交，其余按 `feat`、`fix`、`merge`、其他分组；若过滤后为空，显示“仅版本维护”。

脚本显示：

```text
当前版本：0.9.8
目标版本：0.9.9
远端：deploy
将推送的本地提交数：N
发布摘要：
- 新功能：...
- 修复：...
- 合并：...
待提交文件：Cargo.toml、Cargo.lock
```

此摘要只用于确认和终端记录，不要求用户编辑。

## 7. 确认、提交与推送顺序

脚本提示：

```text
输入 RELEASE 确认发布 v0.9.9，其他输入均取消：
```

### 7.1 取消

未输入精确大写 `RELEASE` 时：

- 使用准备前保存的原始字节还原 `Cargo.toml` 和 `Cargo.lock`。
- 再次检查 tracked worktree 干净。
- 退出码为 0，且没有 commit、push 或 tag。

### 7.2 确认

确认后严格按以下顺序执行：

1. `git add -- Cargo.toml Cargo.lock`
2. `git diff --cached --check`
3. `git commit -m "chore(release): 发布 vX.Y.Z"`
4. `git push deploy master`
5. `git tag -a vX.Y.Z -m "Kiro.rs vX.Y.Z"`
6. `git push deploy vX.Y.Z`

必须先成功 push master，之后才创建 tag，防止 Release tag 指向 GitHub 上不存在的提交。

## 8. 错误处理与恢复

- 准备阶段任一步失败：还原两个版本文件，保持进入脚本前的 tracked 状态。
- commit 失败：不 push、不创建 tag；执行 `git restore --staged -- Cargo.toml Cargo.lock` 后还原尚未提交的版本文件，避免留下半完成的暂存区。
- master push 失败：不创建 tag；保留本地 release commit，输出远端同步排查命令，不自动 reset 或 rebase。
- 本地 tag 创建失败：不 push tag，输出失败原因。
- tag push 失败：保留本地 tag，输出精确重试命令 `git push deploy vX.Y.Z`；不删除已经成功推送的 master 或 release commit。
- Ctrl+C/异常退出：在 release commit 创建前执行同一份文件还原逻辑；commit 创建后不做破坏性自动回滚。

脚本不使用 `git add -A`、`git reset --hard`、force push 或自动删除用户文件。

## 9. GitHub Release Notes

`release.yaml` 的说明生成顺序调整为：

1. 如果 `CHANGELOG.md` 存在 `## [X.Y.Z]`，继续使用该章节。
2. 否则调用 GitHub `POST /repos/{owner}/{repo}/releases/generate-notes`，传入 tag 和 target commit，从上一个 Release 自动汇总提交/PR。
3. API 调用失败或返回空内容时，使用现有固定说明兜底，不能因此让构建资产失败。
4. 无论说明来源为何，最后追加管理端在线更新指引和校验和说明。

脚本不自动重写 `CHANGELOG.md` 的 `[Unreleased]` 内容，避免损坏已有人工维护的详细说明。

## 10. 本地发布文档

`docs/更新发布流程.md` 的日常部分改为：

```powershell
cd D:\kiro2api\kiro-rs2\kiro.rs-admin
.\scripts\release.ps1
```

随后只保留两个人工动作：

1. 等 GitHub Actions 的 `release: vX.Y.Z` 变绿。
2. 管理端点击“立即检查”→“更新并重启”。

文档保留进阶参数、失败恢复、服务器兜底部署和四条安全边界，并修复现有“打 tag”速查区误写为单独数字 `3` 的错误。

## 11. 测试策略

实施阶段先写失败测试，再实现脚本/工作流：

- 版本计算测试：patch/minor/major、显式版本、非法版本、非递增版本。
- 源码合约测试：只能使用 `deploy`、禁止 `git add -A`、确认词为 `RELEASE`、master push 位于 tag 创建之前、tag push 位于最后。
- 取消/异常策略测试：脚本包含原始字节备份和 release commit 前的恢复路径。
- workflow 合约测试：CHANGELOG 优先、Generate Release Notes 次之、固定说明兜底。
- 本地文档检查（合并后在主工作区执行）：一键命令、DryRun、版本参数和失败重试命令全部出现，旧的数字 `3` 不再存在于打 tag 步骤。
- 基线回归：`cargo metadata --locked --no-deps --format-version 1` 与现有 `bun test` 均通过。

发布脚本首次落地后只执行 `-DryRun` 验证，不在开发测试阶段真实 push/tag。

## 12. 用户与客户影响

- 管理员每次发布不再手写版本号、lock 同步命令、提交说明或 tag 命令。
- 用户仍保留最终一次明确确认，所有外部写操作都在确认后发生。
- GitHub Release 说明自动生成，不要求手工维护当前版本 CHANGELOG。
- 本改动只影响开发/发布流程，不修改 RS 运行时、客户对话、SSE 首字节、Token、计费、缓存、工具调用或账号调度。

## 13. 验收标准

- 默认命令能从当前稳定版本正确计算下一个 patch 版本。
- `-DryRun` 不产生文件、commit、tag 或远端写入。
- 取消确认后两个版本文件逐字节还原，tracked worktree 干净。
- 只有输入 `RELEASE` 才执行外部写操作。
- 发布顺序固定为 commit → push master → create tag → push tag。
- Release workflow 在没有版本 CHANGELOG 时生成有内容的自动说明，并有固定兜底。
- 本地发布文档首页包含可复制的一键命令，且不进入公开仓库。
