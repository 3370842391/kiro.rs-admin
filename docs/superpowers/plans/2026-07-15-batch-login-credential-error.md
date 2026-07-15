# Batch Login Credential Error Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让密码输入过程可观察，并让 AWS 凭证错误页面立即结束当前账号、继续下一账号。

**Architecture:** 在 `AccountBrowserSession` 内修复密码输入和提交后页面监控，不改变协议层、凭据存储或批次编排接口。页面错误转换为现有 `BrowserFlowError`，由现有 runner 负责记录并继续。

**Tech Stack:** Python 3.11+、asyncio、Playwright async API、unittest

---

### Task 1：锁定提交后凭证错误与可观察输入

**Files:**
- Modify: `tests/batch_login/test_browser_contract.py`

- [ ] **Step 1：增加 AWS 中文凭证错误页面和回归测试**

在 `PAGES` 增加从密码表单提交到中文错误页的 fixture，并断言 `complete_enterprise()` 抛出 `BrowserFlowError`，错误码为 `invalid_credentials`、`retryable` 为 `False`。

- [ ] **Step 2：增加逐字符输入测试**

创建带 `input` 事件计数器的密码页面，调用 `_fill_password("secret")` 后断言至少产生 6 次输入事件；旧实现的 `fill()` 只产生一次事件。

- [ ] **Step 3：运行测试确认 RED**

Run: `python -m unittest tests.batch_login.test_browser_contract.BrowserContractTests.test_aws_chinese_invalid_credentials_close_current_account tests.batch_login.test_browser_contract.BrowserContractTests.test_password_is_entered_sequentially -v`

Expected: 中文错误测试未抛异常，逐字符输入事件数量不足。

### Task 2：修复页面驱动状态机

**Files:**
- Modify: `scripts/batch_login/browser_flows.py`
- Test: `tests/batch_login/test_browser_contract.py`

- [ ] **Step 1：扩展成功与失败文案**

扩展 `INVALID_TEXT`，覆盖 `无法验证.*登录凭证`、`couldn.t verify.*sign-in credentials`；增加 `SUCCESS_TEXT`，覆盖授权成功、请求已批准和可以关闭窗口等中英文提示。

- [ ] **Step 2：改为顺序输入并保留提交前观察时间**

`_fill_password()` 先清空输入框，再用 `press_sequentially(password, delay=35)`；密码输入完成后等待 0.8 秒再点击登录。

- [ ] **Step 3：提交后继续监控**

删除 `password_filled and callback_future is None` 的立即返回；仅在匹配成功文案、回调完成或外部 IdC token 任务成功取消页面任务时结束。

- [ ] **Step 4：运行浏览器与 runner 回归**

Run: `python -m unittest tests.batch_login.test_browser_contract tests.batch_login.test_local_auth tests.batch_login.test_local_runner -v`

Expected: PASS。

- [ ] **Step 5：运行全量验证并提交**

Run: `python -m compileall -q scripts/batch_login`

Run: `python -m unittest discover -s tests/batch_login -q`

Expected: 0 failures。

Commit: `fix(batch-login): 凭证错误后关闭并继续下一账号`
