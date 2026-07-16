# 本地账号库与管理界面 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 Python 软件主界面改为本地账号管理器，支持批量粘贴入库、表格多选、密码查看/更新、备注与已售状态、账号密码文本导出，同时保留旧批量登录窗口。

**Architecture:** 新增 SQLite `AccountRepository`，敏感字段通过可注入的 `SecretProtector` 加密；`AccountManagerService` 负责业务规则和原子状态变化；`AccountManagerApp` 只负责 Tkinter 交互。现有 `BatchLoginApp` 不承担数据库职责，作为“自动登录设置”辅助窗口继续使用。

**Tech Stack:** Python 3.14、stdlib `sqlite3`、Windows DPAPI、Tkinter/ttk、`unittest`。

---

### Task 1: SQLite 账号仓库与加密数据模型

**Files:**
- Create: `scripts/batch_login/account_repository.py`
- Create: `tests/batch_login/test_account_repository.py`

- [ ] **Step 1: 写建库、去重合并、加密和事务失败测试**

```python
class FakeProtector:
    def protect(self, value: bytes) -> bytes:
        return b"protected:" + value
    def unprotect(self, value: bytes) -> bytes:
        assert value.startswith(b"protected:")
        return value.removeprefix(b"protected:")


def test_upsert_preserves_sold_status_note_and_current_password(self):
    repo = AccountRepository(path, protector=FakeProtector())
    saved = repo.upsert_entries([entry("user", "one-time", url)])
    repo.update_current_passwords([saved[0].id], "current")
    repo.mark_sold([saved[0].id], "客户 A")

    repo.upsert_entries([entry("USER", "new-one-time", url + "/")])
    account = repo.get(saved[0].id, include_secrets=True)

    assert account.lifecycle_status is LifecycleStatus.SOLD
    assert account.note == "客户 A"
    assert account.initial_password == "new-one-time"
    assert account.current_password == "current"
```

同时覆盖：schema version、未知版本拒绝写入、`login_mode + casefold account + normalized URL` 唯一键、100 条单事务入库、数据库只出现密文不出现明文、列表默认不解密、解密失败不泄漏密文、批量更新密码将凭据状态设为 `stale`、批量销售状态失败时整批回滚、操作历史不含密码。

- [ ] **Step 2: 运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_account_repository -v`

Expected: FAIL，提示 `account_repository` 尚不存在。

- [ ] **Step 3: 实现枚举和只读视图模型**

```python
class LoginStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    SUCCESS = "success"
    FAILED = "failed"

class CredentialStatus(str, Enum):
    MISSING = "missing"
    VALID = "valid"
    STALE = "stale"

class LifecycleStatus(str, Enum):
    MANAGED = "managed"
    SOLD = "sold"

@dataclass(frozen=True, slots=True)
class ManagedAccount:
    id: int
    login_mode: LoginMode
    account: str
    start_url: str | None
    region: str
    login_status: LoginStatus
    credential_status: CredentialStatus
    lifecycle_status: LifecycleStatus
    note: str
    initial_password: str | None = field(default=None, repr=False)
    current_password: str | None = field(default=None, repr=False)
    updated_at: str = ""
```

- [ ] **Step 4: 实现版本化 schema 和事务仓库**

数据库含 `metadata`、`accounts`、`operation_history` 三张表。账号表用规范化账号/URL列建立唯一索引；密码列为 BLOB。`upsert_entries()` 先加密全部输入，再在一个 `BEGIN IMMEDIATE` 事务中 insert/update；更新只覆盖新的一次性密码、非空 URL/Region和 `updated_at`，保留当前密码、备注、已售状态。所有连接开启 foreign keys 和 busy timeout。

`list()` 默认 `include_secrets=False`；`get(..., include_secrets=True)` 才调用 protector。`update_current_passwords()` 加密一次后批量写入并设置 `credential_status=stale`。`mark_sold()` 与 `restore_managed()` 单事务更新全部 ID 并追加脱敏历史。

- [ ] **Step 5: 运行仓库测试**

Run: `python -m unittest tests.batch_login.test_account_repository -v`

Expected: PASS。

- [ ] **Step 6: 提交**

```powershell
git add -- scripts/batch_login/account_repository.py tests/batch_login/test_account_repository.py
git commit -m "feat(account-manager): 增加本地账号仓库"
```

### Task 2: 账号管理服务、选择状态与文本导出

**Files:**
- Create: `scripts/batch_login/account_manager_service.py`
- Create: `tests/batch_login/test_account_manager_service.py`

- [ ] **Step 1: 写业务规则失败测试**

```python
def test_confirm_import_upserts_only_valid_preview_entries(self):
    preview = service.preview_import(raw_text, template, LoginMode.ENTERPRISE)
    result = service.confirm_import(preview)
    self.assertEqual(100, result.saved)
    self.assertEqual(2, len(preview.issues))

def test_export_then_mark_sold_changes_state_only_after_writer_succeeds(self):
    report = service.export_text(
        ids,
        template="{account}----{password}----{start_url}",
        writer=lambda text: captured.append(text),
        note="客户 A",
        mark_sold=True,
    )
    self.assertEqual(2, report.exported)
    self.assertTrue(all(item.lifecycle_status is LifecycleStatus.SOLD for item in repo.list()))
```

覆盖：搜索账号/URL/备注、状态筛选、已售默认排除、选择集合在筛选后保留、全选/反选/清空、缺当前密码整批拒绝且不回退一次性密码、模板必须含 account/password/start_url、writer 失败不标记已售、成功后单事务写备注和已售状态、日志/异常不含密码。

- [ ] **Step 2: 运行测试确认服务不存在**

Run: `python -m unittest tests.batch_login.test_account_manager_service -v`

Expected: FAIL。

- [ ] **Step 3: 实现服务接口**

`AccountManagerService` 必须提供以下明确接口：`preview_import(text, template, mode) -> ParseResult` 只解析不写库；`confirm_import(preview, region) -> ImportReport` 只写有效项；`list_accounts(query, status) -> list[ManagedAccount]` 完成搜索与状态筛选；`set_selected/toggle_selected/select_visible/invert_visible/clear_selected` 维护 ID 集合；`update_password(ids, password) -> int` 调用仓库批量更新；`export_text(ids, template, writer, note, mark_sold) -> TextExportReport` 执行验证、渲染、写出和可选销售状态提交。

选择只保存账号 ID，不依赖 Treeview 行号。`export_text()` 先加载并验证全部当前密码，渲染完整文本，调用 writer 成功后才执行 `mark_sold()`；writer 由 GUI 注入为剪贴板写入或原子 TXT 写入。

- [ ] **Step 4: 运行服务测试并提交**

Run: `python -m unittest tests.batch_login.test_account_manager_service -v`

Expected: PASS。

```powershell
git add -- scripts/batch_login/account_manager_service.py tests/batch_login/test_account_manager_service.py
git commit -m "feat(account-manager): 增加账号管理服务"
```

### Task 3: 单页账号表格、粘贴预览和密码查看器

**Files:**
- Create: `scripts/batch_login/account_manager_app.py`
- Create: `tests/batch_login/test_account_manager_app.py`
- Modify: `scripts/kiro_batch_login_gui.py`

- [ ] **Step 1: 写 GUI 控制契约失败测试**

测试不启动真实主循环，使用 FakeVar/FakeTree/FakeDialog 验证：

- 主表列包含勾选、账号、当前密码遮罩、Start URL、登录状态、凭据状态、销售状态、备注、更新时间。
- 搜索/筛选调用 service 并保持已选择 ID。
- 单击勾选、Ctrl、Shift 和拖动连续行只改变 service selection。
- 粘贴窗口先 preview，只有确认才调用 `confirm_import()`。
- 密码查看器分别显示一次性/当前密码，关闭时两个变量都清空。
- 更新密码只接受非空值并支持多个选中账号。
- 已售账号默认不可执行批量更新；恢复管理后可操作。
- 文本导出取消/失败不改变状态，成功后刷新表格。
- “自动登录设置”打开现有 `BatchLoginApp` Toplevel。

- [ ] **Step 2: 运行测试确认新主界面不存在**

Run: `python -m unittest tests.batch_login.test_account_manager_app -v`

Expected: FAIL。

- [ ] **Step 3: 实现 AccountManagerApp 布局**

窗口顶部为搜索框、状态筛选和按钮栏；中间全宽 `ttk.Treeview(selectmode="extended")`；底部为选中数量、状态和操作日志。按钮：粘贴并识别、全选、反选、取消选择、查看密码、更新密码、导出账号密码、标记已售、恢复管理、自动登录设置。

勾选列使用 `☐/☑` 文本和 service ID 集合；`<Button-1>` 识别行/列，`<B1-Motion>` 以起始/当前行组成连续范围；Treeview 自带 Ctrl/Shift 选择同步到 service。刷新表格时按账号 ID 恢复勾选状态。

- [ ] **Step 4: 实现三个模态窗口**

- 粘贴预览：Text、模板 Combobox、登录方式、预览表和有效/重复/错误计数；确认后事务入库。
- 密码查看：两个默认遮罩 Entry、分别的眼睛和复制按钮；销毁事件先把变量设为空。
- 文本导出：模板、只读预览、备注、“成功后标记已售出”、复制和保存 TXT。保存使用同目录临时文件、flush、fsync、`os.replace`。

- [ ] **Step 5: 修改启动入口**

`kiro_batch_login_gui.py` 默认构造 `AccountRepository(default_account_db_path(), protector=WindowsDpapiProtector())`、`AccountManagerService` 和 `AccountManagerApp`。旧 `BatchLoginApp` 仅在用户点击“自动登录设置”时创建 Toplevel；依赖检查保持不变。账号数据库默认位于 `%LOCALAPPDATA%/KiroBatchLogin/accounts.sqlite3`。

- [ ] **Step 6: 运行 GUI 测试、依赖检查和语法检查**

Run: `python -m unittest tests.batch_login.test_account_manager_app tests.batch_login.test_gui_controller tests.batch_login.test_gui_settings -v`

Run: `python scripts/kiro_batch_login_gui.py --check`

Run: `python -m compileall -q scripts`

Expected: 全部退出码 0。

- [ ] **Step 7: 提交**

```powershell
git add -- scripts/batch_login/account_manager_app.py tests/batch_login/test_account_manager_app.py scripts/kiro_batch_login_gui.py
git commit -m "feat(account-manager): 增加账号管理主界面"
```

### Task 4: 阶段二完整验证与复核

**Files:** Verify only.

- [ ] **Step 1:** `python -m unittest discover -s tests/batch_login -t .`，预期 0 failure。
- [ ] **Step 2:** `bun test && bun run build`，预期 0 failure、构建成功。
- [ ] **Step 3:** `cargo test`，预期 1054 项或更多通过，不新增 warning。
- [ ] **Step 4:** `git diff --check 3850840..HEAD` 与 `git status --short`，预期干净。
- [ ] **Step 5:** 在临时目录用 FakeProtector 做 100 账号入库、筛选、多选、文本导出烟测；不得读取真实账号文件。
- [ ] **Step 6:** 规格复核先确认账号库、密码规则、已售规则和导出事务，再做代码质量/安全复核；修复全部 Critical/Important 问题后进入阶段三。
