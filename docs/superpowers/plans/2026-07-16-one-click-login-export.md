# 一键登录与凭据同步 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 对账号管理表中选中的账号执行自动登录，可靠同步首次改密后的当前密码和 OIDC 凭据，并生成合并/逐账号/两种 JSON，可选继续导入 RS。

**Architecture:** `AccountRepository` 加密保存完整 `CredentialRecord`；`AccountLoginCoordinator` 读取已保存 GUI 配置，复用有效凭据并对 missing/stale 账号调用现有 `GuiRuntime`。每个登录批次使用临时完整 JSON，成功后先加密回写数据库，再调用 OIDC exporter；AccountManagerApp 只负责后台线程、事件和按钮状态。

**Tech Stack:** Python asyncio/threading、现有 Enterprise/Microsoft 登录模块、SQLite/DPAPI、Tkinter、unittest。

---

### Task 1: 账号库凭据与登录状态事务

**Files:**
- Modify: `scripts/batch_login/account_repository.py`
- Modify: `tests/batch_login/test_account_repository.py`

- [ ] 先写失败测试：完整凭据 JSON 经 protector 加密后数据库不含 token；读取能恢复 `CredentialRecord`；保存凭据原子设置 `credential_status=valid/login_status=success`；登录失败只保存脱敏 code/stage；批量 running/failed 遇缺失 ID 全回滚。
- [ ] 运行 `python -m unittest tests.batch_login.test_account_repository -v` 确认失败。
- [ ] 实现 `save_credential(account_id, CredentialRecord)`、`load_credential(account_id)`、`mark_login_running(ids)`、`mark_login_failed(id, code, stage)`；凭据使用 `as_add_request()` JSON 整体加密，历史不记录内容。
- [ ] 测试通过后提交 `feat(account-manager): 增加凭据与登录状态存储`。

### Task 2: 一键登录协调器

**Files:**
- Create: `scripts/batch_login/account_login_coordinator.py`
- Create: `tests/batch_login/test_account_login_coordinator.py`

- [ ] 先写失败测试：valid 凭据默认复用；stale/missing 才登录；force 全部登录；已售拒绝；企业与 Microsoft 分组；临时完整 JSON 成功后先写数据库再导出；登录失败继续；密码保险库 confirmed 密码同步；无保存配置给出明确错误；事件不含密码/token。
- [ ] 运行聚焦测试确认模块不存在。
- [ ] 实现 `form_from_saved_settings()`，保留 RS/SSH/超时配置，但将每个模式的 credential/checkpoint 指向临时目录，将企业密码库固定为已保存路径或账号数据库旁 `enterprise-passwords.sqlite3`。
- [ ] 实现 `AccountLoginCoordinator.run(ids, force_relogin=False)`：读取并校验 managed 账号；收集可复用凭据；按 mode 运行隔离 `GuiRuntime`；加载临时 CredentialStore；按 account + normalized start URL 回写；从 PasswordVault confirmed 记录同步当前密码；缺结果账号标记 failed；最后对“本次成功 + 复用”凭据调用 OIDC exporter。`GuiRuntime.close()` 必须在 finally。
- [ ] 运行测试并提交 `feat(account-manager): 增加一键登录协调器`。

### Task 3: 主界面按钮与后台事件

**Files:**
- Modify: `scripts/batch_login/account_manager_app.py`
- Modify: `scripts/kiro_batch_login_gui.py`
- Modify: `tests/batch_login/test_account_manager_app.py`

- [ ] 先写失败测试：“一键登录导出 JSON”按钮存在；无选择提示；运行期间禁用破坏性按钮；后台事件通过 `root.after` 回主线程；完成后刷新表格；失败消息脱敏；提供“强制重新登录”确认选项。
- [ ] 修改启动入口构造 `GuiSettingsStore` 和 `AccountLoginCoordinator` 注入 App。
- [ ] App 新增按钮和日志区；点击后后台线程 `asyncio.run(coordinator.run(...))`，事件仅展示账号掩码、阶段、成功/失败计数和导出目录。
- [ ] 聚焦测试、`--check`、compileall 通过后提交 `feat(account-manager): 接入一键登录导出`。

### Task 4: 完整验证

- [ ] Python 全量测试 0 failure。
- [ ] Bun 全量 0 failure 且生产构建成功。
- [ ] Rust 全量 1054 项或更多通过，不新增 warning。
- [ ] 使用 FakeAuth/FakeProtector 完成 100 账号协调器烟测；不读取真实账号文件、不访问真实登录服务。
- [ ] `git diff --check 8b1a335..HEAD` 和敏感信息扫描通过。
- [ ] 规格复核和代码质量复核无 Critical/Important 后，本地合并到 master，不 push。
