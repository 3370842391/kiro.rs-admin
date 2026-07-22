# Dashed Account URL Auto-Detection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让账号管理 GUI 无需切换下拉模板即可识别 `账号----密码----HTTPS Start URL`。

**Architecture:** 在通用解析器增加默认关闭的逐行 Start URL 自动识别开关，保持 CLI 和现有调用方行为不变。账号管理服务为 GUI 导入显式打开该开关；自动格式先于当前模板尝试，并复用现有贪婪密码捕获与 URL 安全校验。

**Tech Stack:** Python 3、`unittest`、现有 `batch_login` 数据模型与解析器。

---

### Task 1: 固定 GUI 自动识别行为

**Files:**
- Modify: `tests/batch_login/test_account_manager_service.py`

- [ ] **Step 1: 写入失败测试**

在 `AccountManagerServiceTests` 中增加：

```python
def test_preview_auto_detects_dashed_per_line_start_urls(self):
    portal = "https://d-9066760521.awsapps.com/start"
    preview = self.service.preview_import(
        "\n".join(
            (
                f"NobleFlame1----#5P%<g)g@d80D>o03$OHKz----{portal}",
                f"NobleFlame2----part----two----{portal}",
            )
        ),
        "login = {account} / onetime password = {password}",
        LoginMode.ENTERPRISE,
    )

    self.assertEqual([], preview.issues)
    self.assertEqual(["NobleFlame1", "NobleFlame2"], [item.account for item in preview.entries])
    self.assertEqual("#5P%<g)g@d80D>o03$OHKz", preview.entries[0].password)
    self.assertEqual("part----two", preview.entries[1].password)
    self.assertEqual([portal, portal], [item.start_url for item in preview.entries])
```

- [ ] **Step 2: 运行测试并确认按预期失败**

Run: `python -m unittest tests.batch_login.test_account_manager_service.AccountManagerServiceTests.test_preview_auto_detects_dashed_per_line_start_urls -v`

Expected: FAIL，当前结果含两个 `format_mismatch`，没有有效账号。

### Task 2: 实现受控自动识别

**Files:**
- Modify: `scripts/batch_login/input_parser.py`
- Modify: `scripts/batch_login/account_manager_service.py`
- Test: `tests/batch_login/test_account_manager_service.py`
- Test: `tests/batch_login/test_input_parser.py`

- [ ] **Step 1: 扩展解析器入口**

在 `input_parser.py` 定义自动格式，并为 `parse_accounts` 增加仅限关键字的开关：

```python
AUTO_START_URL_FORMAT = "{account}----{password}----{start_url}"

def parse_accounts(
    text: str,
    template: str,
    mode: LoginMode,
    *,
    auto_detect_start_url: bool = False,
) -> ParseResult:
    compiled = compile_format(template)
    auto_compiled = (
        compile_format(AUTO_START_URL_FORMAT)
        if auto_detect_start_url and "{start_url}" not in template
        else None
    )
```

逐行匹配时先尝试自动格式，再回退当前模板：

```python
match = auto_compiled.pattern.fullmatch(line) if auto_compiled else None
if match is None:
    match = compiled.pattern.fullmatch(line)
```

- [ ] **Step 2: 只在账号管理 GUI 导入服务启用**

将 `AccountManagerService.preview_import` 的调用改为：

```python
result = parse_accounts(
    text,
    template,
    mode,
    auto_detect_start_url=True,
)
```

- [ ] **Step 3: 增加解析器兼容性断言**

在 `test_input_parser.py` 增加测试，证明默认不开启时两字段模板仍把尾部文本视为密码，开启后则提取 URL：

```python
def test_dashed_start_url_auto_detection_is_opt_in(self):
    raw = "user----part----two----https://portal.example/start"
    strict = parse_accounts(raw, "{account}----{password}", LoginMode.ENTERPRISE)
    detected = parse_accounts(
        raw,
        "{account}----{password}",
        LoginMode.ENTERPRISE,
        auto_detect_start_url=True,
    )

    self.assertIsNone(strict.entries[0].start_url)
    self.assertEqual("part----two----https://portal.example/start", strict.entries[0].password)
    self.assertEqual("part----two", detected.entries[0].password)
    self.assertEqual("https://portal.example/start", detected.entries[0].start_url)
```

- [ ] **Step 4: 运行聚焦测试**

Run: `python -m unittest tests.batch_login.test_input_parser tests.batch_login.test_account_manager_service -v`

Expected: PASS，两个模块所有测试通过。

- [ ] **Step 5: 运行全部批量登录测试**

Run: `python -m unittest discover -s tests/batch_login -p "test_*.py" -v`

Expected: PASS，无错误或失败。

- [ ] **Step 6: 提交实现**

```bash
git add scripts/batch_login/input_parser.py scripts/batch_login/account_manager_service.py tests/batch_login/test_input_parser.py tests/batch_login/test_account_manager_service.py
git commit -m "feat: auto-detect per-account start URLs"
```
