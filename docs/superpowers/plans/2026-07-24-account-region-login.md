# Account Region Login Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让账号导入时可保存 `eu-central-1`，并让批量自动登录按每个账号保存的 Region 使用正确 AWS 端点。

**Architecture:** Region 仍作为账号仓库字段保存，导入界面只负责采集，账号服务负责规范化与校验。登录协调器按 `(login_mode, region)` 分组创建运行时，使同一批中的不同 Region 互不污染；底层企业 HTTP 客户端继续使用已有 Region 端点构造逻辑。

**Tech Stack:** Python 3、Tkinter、asyncio、unittest、SQLite

---

### Task 1: 导入并保存账号 Region

**Files:**
- Modify: `scripts/batch_login/account_manager_service.py`
- Modify: `scripts/batch_login/account_manager_app.py`
- Test: `tests/batch_login/test_account_manager_service.py`
- Test: `tests/batch_login/test_account_manager_app.py`

- [ ] **Step 1: 写入失败测试**

在服务测试中断言 `confirm_import(..., region=" EU-CENTRAL-1 ")` 保存为 `eu-central-1`，并断言空值或含非法字符的 Region 在写库前抛出 `AccountManagerServiceError`。在应用测试中用假的设置存储断言 `_default_import_region()` 返回保存值或默认 `us-east-1`。

- [ ] **Step 2: 运行测试确认 RED**

Run: `python -m pytest tests/batch_login/test_account_manager_service.py tests/batch_login/test_account_manager_app.py -q`

Expected: FAIL，因为服务尚未规范化/校验 Region，应用也没有 `_default_import_region()`。

- [ ] **Step 3: 最小实现**

在账号服务中增加 Region 规范化：

```python
@staticmethod
def _validated_region(region: str) -> str:
    normalized = region.strip().lower()
    if not normalized or any(
        character not in "abcdefghijklmnopqrstuvwxyz0123456789-"
        for character in normalized
    ):
        raise AccountManagerServiceError("Region 格式无效")
    return normalized
```

`confirm_import()` 使用规范化结果调用仓库。导入窗口增加 `Region` 输入框，初始值由 `_default_import_region()` 从 `GuiSettingsStore` 读取，确认时调用：

```python
self.service.confirm_import(result, region=region.get())
```

- [ ] **Step 4: 运行测试确认 GREEN**

Run: `python -m pytest tests/batch_login/test_account_manager_service.py tests/batch_login/test_account_manager_app.py -q`

Expected: PASS。

### Task 2: 批量登录按账号 Region 分组

**Files:**
- Modify: `scripts/batch_login/account_login_coordinator.py`
- Test: `tests/batch_login/test_account_login_coordinator.py`
- Test: `tests/batch_login/test_account_login_coordinator_apikey.py`

- [ ] **Step 1: 写入失败测试**

创建一个 `us-east-1` 账号和一个 `eu-central-1` 账号，记录 `runtime_factory` 收到的 `form.region`。分别测试普通 `run()` 和 `login_and_extract_pipeline()`，期望各创建两个运行时且 Region 集合为：

```python
{"us-east-1", "eu-central-1"}
```

- [ ] **Step 2: 运行测试确认 RED**

Run: `python -m pytest tests/batch_login/test_account_login_coordinator.py tests/batch_login/test_account_login_coordinator_apikey.py -q`

Expected: FAIL，因为当前仅按登录模式分组且运行时统一使用保存配置中的 Region。

- [ ] **Step 3: 最小实现**

让 `form_from_saved_settings()` 接受 `region_override`。增加协调器分组函数，按账号顺序生成 `(login_mode, region, accounts)`：

```python
@staticmethod
def _login_batches(accounts):
    grouped = {}
    for account in accounts:
        key = (account.login_mode, account.region.strip().lower())
        grouped.setdefault(key, []).append(account)
    return [(mode, region, batch) for (mode, region), batch in grouped.items()]
```

普通登录与流水线登录都遍历该分组，创建独立表单和运行时，并用序号构造临时文件名，避免把 Region 直接拼入文件路径。

- [ ] **Step 4: 运行测试确认 GREEN**

Run: `python -m pytest tests/batch_login/test_account_login_coordinator.py tests/batch_login/test_account_login_coordinator_apikey.py -q`

Expected: PASS。

### Task 3: 德国区端点回归与全量验证

**Files:**
- Test: `tests/batch_login/test_enterprise_http.py`

- [ ] **Step 1: 增加端点回归测试**

使用假响应执行 `EnterpriseHttpClient.login()`，传入 `https://d-99674db463.awsapps.com/start` 和 `eu-central-1`，断言请求使用：

```text
https://oidc.eu-central-1.amazonaws.com/client/register
https://portal.sso.eu-central-1.amazonaws.com/login
https://eu-central-1.signin.aws/platform/d-99674db463/api/execute
```

- [ ] **Step 2: 运行聚焦测试**

Run: `python -m pytest tests/batch_login/test_enterprise_http.py tests/batch_login/test_account_manager_service.py tests/batch_login/test_account_manager_app.py tests/batch_login/test_account_login_coordinator.py tests/batch_login/test_account_login_coordinator_apikey.py -q`

Expected: PASS。

- [ ] **Step 3: 运行批量登录全量回归**

Run: `python -m pytest tests/batch_login -q`

Expected: PASS。

- [ ] **Step 4: 检查并提交**

Run: `git diff --check`，只暂存本计划涉及的源码、测试和文档文件，然后使用中文提交信息 `fix(login): 支持按账号地区自动登录` 创建本地提交；不暂存 `.cargo-target/`。
