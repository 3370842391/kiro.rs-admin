# 统一批量并发 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 GUI 并发输入值实际控制登录、JSON、提 Key 和额度刷新全部批量任务。

**Architecture:** 保持协调器默认 `concurrency=5` 兼容非 GUI 调用方；GUI 五个入口显式读取并传递同一个值。提 Key 包装器把值转发给自动登录和提取阶段，额度刷新改为 semaphore 限制的逐账号异步任务，并用锁保护报告计数。

**Tech Stack:** Python 3、`asyncio`、Tkinter、`unittest.IsolatedAsyncioTestCase`。

---

### Task 1: 固定协调器并发契约

**Files:**
- Modify: `tests/batch_login/test_account_login_coordinator_apikey.py`
- Modify: `tests/batch_login/test_coordinator_quota.py`

- [ ] **Step 1: 写失败测试**

新增测试断言：

```python
async def test_login_and_extract_forwards_same_concurrency_to_both_stages(self):
    calls = []

    async def fake_run(ids, **kwargs):
        calls.append(("login", kwargs["concurrency"]))

    async def fake_extract(ids, **kwargs):
        calls.append(("extract", kwargs["concurrency"]))
        return ApiKeyExtractionReport(len(ids), len(ids), 0, 0, 0, 0, None)

    coordinator.run = fake_run
    coordinator.extract_api_keys = fake_extract
    await coordinator.login_and_extract_api_keys([self.account_id], concurrency=2)

    self.assertEqual([("login", 2), ("extract", 2)], calls)
```

另加额度刷新并发观测：用 `fake_usage` 在进入时递增 active、`await asyncio.sleep(0)` 后递减，传入 2 个以上账号和 `concurrency=2`，断言 `max_active == 2`。

- [ ] **Step 2: 运行失败测试**

Run: `python -m unittest tests.batch_login.test_account_login_coordinator_apikey tests.batch_login.test_coordinator_quota -v`

Expected: FAIL；当前包装器不接受 `concurrency`，额度刷新也没有并发参数。

### Task 2: 实现协调器传递与额度并发

**Files:**
- Modify: `scripts/batch_login/account_login_coordinator.py`
- Test: `tests/batch_login/test_account_login_coordinator_apikey.py`
- Test: `tests/batch_login/test_coordinator_quota.py`

- [ ] **Step 1: 扩展提 Key 包装器**

给 `login_and_extract_api_keys` 增加 `concurrency: int = 5`，并在两个调用点传递：

```python
await self.run(..., concurrency=concurrency)
return await self.extract_api_keys(..., concurrency=concurrency)
```

- [ ] **Step 2: 并发化额度刷新**

给 `refresh_quota` 增加 `concurrency: int = 5`，创建 `limiter = asyncio.Semaphore(max(1, int(concurrency)))` 和 `lock = asyncio.Lock()`；将单账号逻辑放进 `async def handle(account)`，入口使用 `async with limiter`，计数和进度终态在锁内更新，最后 `await asyncio.gather(*(handle(account) for account in accounts))`。

- [ ] **Step 3: 运行协调器测试**

Run: `python -m unittest tests.batch_login.test_account_login_coordinator_apikey tests.batch_login.test_coordinator_quota -v`

Expected: PASS，原有错误继续处理和报告统计测试也通过。

### Task 3: 让 GUI 五个入口传递并显示并发

**Files:**
- Modify: `scripts/batch_login/account_manager_app.py`
- Modify: `tests/batch_login/test_account_manager_app.py`

- [ ] **Step 1: 写入口传参测试**

使用 fake coordinator 和 fake task runner 调用五个入口，模拟 `_read_concurrency()` 返回 `3`，断言调用参数包含 `concurrency=3`；确认提示文本和开始日志包含 `并发 3`。

- [ ] **Step 2: 运行失败测试**

Run: `python -m unittest tests.batch_login.test_account_manager_app -v`

Expected: FAIL；当前只有 `start_pipeline` 传递并发。

- [ ] **Step 3: 实现入口传递**

在 `start_login_only`、`start_login_export`、`start_api_key_extraction`、`start_quota_refresh` 启动任务前调用 `_read_concurrency()`，传入对应 coordinator，并在确认/日志/状态中显示实际值。`start_pipeline` 保持现有传递。

- [ ] **Step 4: 运行 GUI 测试**

Run: `python -m unittest tests.batch_login.test_account_manager_app -v`

Expected: PASS。

### Task 4: 全量验证并提交

**Files:**
- Test: `tests/batch_login/` 全部测试

- [ ] **Step 1: 运行全量回归**

Run: `python -m unittest discover -s tests/batch_login -p "test_*.py"`

Expected: 全部 PASS，0 failures。

- [ ] **Step 2: 检查差异并提交**

```bash
git diff --check
git add scripts/batch_login/account_login_coordinator.py scripts/batch_login/account_manager_app.py tests/batch_login/test_account_login_coordinator_apikey.py tests/batch_login/test_coordinator_quota.py tests/batch_login/test_account_manager_app.py
git commit -m "fix: honor GUI concurrency across batch tasks"
```
