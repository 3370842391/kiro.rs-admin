# OIDC 精简导出与 RS 兼容 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 保留内部完整凭据文件，同时让 Python GUI 可生成 Kiro Account Manager/RS 兼容的 OIDC 精简 JSON，并让 RS 前端直接识别旧的 `credentials` 数组包装。

**Architecture:** `OidcCredentialExporter` 使用显式字段白名单把完整 `CredentialRecord` 投影为精简字段并原子写文件；不修改 `CredentialRecord.as_add_request()` 和 `CredentialStore` 内部格式。GUI 提供手动转换入口。前端新增纯函数 `unwrapCredentialImportPayload()`，批量导入对话框复用它，不再把顶层 `credentials` 数组误判为单账号对象。

**Tech Stack:** Python 3.14、`unittest`、Tkinter、TypeScript、Bun test、React、现有 Rust 管理后台。

---

### Task 1: 独立 OIDC 文件导出器

**Files:**
- Create: `scripts/batch_login/oidc_exporter.py`
- Create: `tests/batch_login/test_oidc_exporter.py`
- Modify: `tests/batch_login/test_credential_store.py`

- [ ] **Step 1: 写精简字段和三种导出模式的失败测试**

```python
def enterprise_record():
    return CredentialRecord(
        email="admin-user",
        auth_method="IdC",
        provider="Enterprise",
        refresh_token="refresh-secret",
        access_token="must-not-export",
        client_id="client-id",
        client_secret="client-secret",
        start_url="https://portal.example/start",
        region="us-east-1",
    )


def test_oidc_projection_matches_kam_minimal_shape(self):
    payload = OidcCredentialExporter().project(enterprise_record())
    self.assertEqual("idc", payload["authMethod"])
    self.assertEqual("refresh-secret", payload["refreshToken"])
    self.assertNotIn("accessToken", payload)
    self.assertNotIn("sourceChannel", payload)


def test_both_mode_writes_merged_array_and_one_item_arrays(self):
    result = OidcCredentialExporter(now=fixed_clock).export(
        [enterprise_record(), enterprise_record_for("second")],
        output_directory=Path(tmp),
        mode=OidcExportMode.BOTH,
    )
    self.assertEqual(2, result.record_count)
    self.assertIsNotNone(result.merged_path)
    self.assertEqual(2, len(result.account_paths))
    self.assertIsInstance(json.loads(result.merged_path.read_text("utf-8")), list)
```

同时测试：缺少/空 `refreshToken` 拒绝整批且错误不含其他 Token；合并、逐账号、两者；文件名不含斜杠和 Token；临时文件清理；逐账号文件为单元素数组；权限警告不改变成功结果。

- [ ] **Step 2: 运行测试并确认因 API 不存在而失败**

Run: `python -m unittest tests.batch_login.test_oidc_exporter -v`

Expected: FAIL，提示 `batch_login.oidc_exporter` 尚不存在。

- [ ] **Step 3: 实现显式字段白名单投影**

```python
def project(self, record: CredentialRecord) -> dict[str, str]:
    payload: dict[str, str | None] = {
        "email": record.email,
        "authMethod": record.auth_method.lower(),
        "provider": record.provider,
        "region": record.region,
        "startUrl": record.start_url,
        "refreshToken": record.refresh_token,
        "clientId": record.client_id,
        "clientSecret": record.client_secret,
        "profileArn": record.profile_arn,
        "tokenEndpoint": record.token_endpoint,
        "scopes": record.scopes,
        "issuerUrl": record.issuer_url,
    }
    return {
        key: value.strip() if key != "refreshToken" else value
        for key, value in payload.items()
        if isinstance(value, str) and value.strip()
    }
```

这个白名单不得通过 `record.as_add_request()` 后删除键来实现，避免未来新增内部敏感字段时意外泄漏。`refreshToken` 的必填校验放在 `export()`，投影方法不记录敏感值。

- [ ] **Step 4: 实现原子导出器**

```python
class OidcExportMode(str, Enum):
    MERGED = "merged"
    PER_ACCOUNT = "per_account"
    BOTH = "both"


@dataclass(frozen=True, slots=True)
class OidcExportReport:
    record_count: int
    merged_path: Path | None
    account_paths: tuple[Path, ...]


class OidcCredentialExporter:
    def export(
        self,
        records: Sequence[CredentialRecord],
        *,
        output_directory: Path,
        mode: OidcExportMode,
    ) -> OidcExportReport:
        items = list(records)
        invalid = [item for item in items if not (item.refresh_token or "").strip()]
        if invalid:
            masked = ", ".join(mask_account(item.email) for item in invalid)
            raise OidcExportError(f"以下账号缺少 refreshToken：{masked}")
        payloads = [self.project(item) for item in items]
        stamp = self.now().strftime("%Y%m%d-%H%M%S")
        merged_path = None
        account_paths: list[Path] = []
        if mode in {OidcExportMode.MERGED, OidcExportMode.BOTH}:
            merged_path = self._unused_path(
                output_directory / f"kiro-accounts-{stamp}.oidc.json"
            )
            self._atomic_write(merged_path, payloads)
        if mode in {OidcExportMode.PER_ACCOUNT, OidcExportMode.BOTH}:
            for index, (record, payload) in enumerate(zip(items, payloads), 1):
                name = self._account_filename(index, record)
                path = self._unused_path(output_directory / name)
                self._atomic_write(path, [payload])
                account_paths.append(path)
        return OidcExportReport(len(items), merged_path, tuple(account_paths))
```

导出前先验证全部记录并计算全部目标文件名，避免缺 token 时产生部分输出。逐账号文件按输入顺序加三位序号，账号部分只保留字母、数字、点、下划线和短横线并限制长度，再追加基于 `dedupe_key()` 的 8 位 SHA-256；处理 Windows 保留名、尾随点/空格和大小写碰撞。`_unused_path()` 在同秒重名时追加 `-2/-3`，不静默覆盖旧批次。`_atomic_write()` 在目标同目录创建唯一临时文件，依次执行 JSON dump、flush、fsync、尽力 chmod、`os.replace`，失败时清理临时文件并保留旧目标。

- [ ] **Step 5: 运行聚焦测试和凭据模型回归测试**

Run: `python -m unittest tests.batch_login.test_oidc_exporter tests.batch_login.test_credential_store -v`

Expected: PASS。

- [ ] **Step 6: 本地提交**

```powershell
git add -- scripts/batch_login/oidc_exporter.py tests/batch_login/test_oidc_exporter.py tests/batch_login/test_credential_store.py
git commit -m "feat(batch-login): 增加 OIDC 精简导出器"
```

### Task 2: GUI 导出配置与已有 JSON 转换

**Files:**
- Modify: `scripts/batch_login/gui_controller.py`
- Modify: `scripts/batch_login/gui_settings.py`
- Modify: `scripts/batch_login/gui_runtime.py`
- Modify: `scripts/batch_login/gui_app.py`
- Modify: `tests/batch_login/test_gui_controller.py`
- Modify: `tests/batch_login/test_gui_settings.py`
- Create: `tests/batch_login/test_gui_runtime.py`

- [ ] **Step 1: 写表单、配置和手动转换的失败测试**

```python
def test_export_settings_round_trip(self):
    saved = GuiSavedSettings(
        oidc_export_mode="both",
        oidc_export_dir="C:/exports",
    )
    store.save(saved)
    self.assertEqual(saved, store.load())


def test_export_existing_does_not_require_portal_or_rs(self):
    form = valid_form(
        start_url="",
        rs_url="",
        admin_key="",
        oidc_export_mode=OidcExportMode.BOTH,
        oidc_export_directory="C:/exports",
    )
    controller.export_existing(form)
    controller.thread.join(timeout=2)
    self.assertEqual(["export_existing"], runtime.calls)
```

控制器测试还要覆盖任务并发保护、run/import/export 三种 action 的显式路由和运行时错误脱敏；GUI 运行时测试覆盖空完整文件报错、只调用 exporter、不连接 RS/Playwright、完成事件不含凭据内容。

- [ ] **Step 2: 运行测试并确认缺少字段/方法而失败**

Run: `python -m unittest tests.batch_login.test_gui_settings tests.batch_login.test_gui_controller tests.batch_login.test_gui_runtime -v`

Expected: FAIL，提示 OIDC 配置字段或 `export_existing` 不存在。

- [ ] **Step 3: 扩展表单、运行设置和保存配置**

```python
@dataclass(slots=True)
class GuiFormState:
    oidc_export_mode: OidcExportMode = OidcExportMode.MERGED
    oidc_export_directory: str = ""

    def oidc_output_dir(self) -> Path:
        raw = self.oidc_export_directory.strip()
        return Path(raw) if raw else Path(self.credential_path).resolve().parent
```

`GuiSavedSettings` 保存字符串模式和目录；`from_mapping()` 对旧 version 1 配置缺字段使用 `merged / ""` 默认值，不提升配置版本；非法模式和非字符串目录抛 `GuiSettingsError`。

- [ ] **Step 4: 注入导出器并实现运行/转换数据流**

```python
records = CredentialStore(Path(self.form.credential_path)).load()
if not records:
    raise ValueError("完整凭据 JSON 中没有可导出的账号")
report = OidcCredentialExporter(
    warning_sink=lambda message: self.emit(
        WorkerEvent("security_warning", {"message": message})
    )
).export(
    records,
    output_directory=self.form.oidc_output_dir(),
    mode=self.form.oidc_export_mode,
)
self.emit(WorkerEvent("oidc_exported", {
    "count": report.record_count,
    "fileCount": int(report.merged_path is not None) + len(report.account_paths),
    "directory": str(self.form.oidc_output_dir()),
}))
```

`GuiRuntime.export_existing()` 只执行上述读取、转换和安全事件发送，不连接 RS。`GuiController` 的线程 action 增加 `export`，调用 runtime 的 `export_existing()`；将现有二分支改为显式 `run/import/export` 分派，避免把 export 误路由成 import。

- [ ] **Step 5: 增加 GUI 控件并接好配置**

在“登录与结果”区域增加只读组合框（合并 JSON、逐账号 JSON、两者）和“O﻿IDC 导出目录”目录选择行，目录选择必须使用 `filedialog.askdirectory()`。底部增加“转换已有完整 JSON”按钮：选择内部完整 JSON 后选择/沿用输出目录，调用 controller 的 export action；不得把结果模式强制改成 `save_and_import`。确认提示明确 OIDC 文件包含 refresh token/client secret。

`_build_variables()`、`_collect_form()`、`_snapshot_settings()`、`_apply_saved_settings()` 全部使用相同字段名，避免只保存不恢复。

- [ ] **Step 6: 运行 GUI 和运行器聚焦测试**

Run: `python -m unittest tests.batch_login.test_gui_settings tests.batch_login.test_gui_controller tests.batch_login.test_gui_runtime -v`

Expected: PASS。

- [ ] **Step 7: 本地提交**

```powershell
git add -- scripts/batch_login/gui_controller.py scripts/batch_login/gui_settings.py scripts/batch_login/gui_runtime.py scripts/batch_login/gui_app.py tests/batch_login/test_gui_controller.py tests/batch_login/test_gui_settings.py tests/batch_login/test_gui_runtime.py
git commit -m "feat(batch-login): 接入 GUI OIDC 导出"
```

### Task 3: RS 前端识别完整凭据包装

**Files:**
- Create: `admin-ui/src/lib/credential-import.ts`
- Create: `admin-ui/src/lib/credential-import.test.ts`
- Modify: `admin-ui/src/components/batch-import-dialog.tsx`

- [ ] **Step 1: 写真实解析行为的失败测试**

```typescript
import { describe, expect, test } from 'bun:test'
import { unwrapCredentialImportPayload } from './credential-import'

describe('unwrapCredentialImportPayload', () => {
  test('unwraps internal credentials array instead of treating it as one account', () => {
    const items = [{ email: 'user', refreshToken: 'refresh' }]
    expect(unwrapCredentialImportPayload({ version: 1, credentials: items })).toEqual(items)
  })

  test('keeps arrays, KAM accounts, flat objects and nested single accounts compatible', () => {
    expect(unwrapCredentialImportPayload([{ refreshToken: 'a' }])).toHaveLength(1)
    expect(unwrapCredentialImportPayload({ accounts: [{ refreshToken: 'b' }] })).toHaveLength(1)
    expect(unwrapCredentialImportPayload({ refreshToken: 'c' })).toHaveLength(1)
    expect(unwrapCredentialImportPayload({ credentials: { refreshToken: 'd' } })).toHaveLength(1)
  })
})
```

- [ ] **Step 2: 运行测试并确认模块不存在**

Run: `bun test src/lib/credential-import.test.ts`

Expected: FAIL，提示 `credential-import` 模块不存在。

- [ ] **Step 3: 实现纯解析函数并替换组件内分支**

```typescript
export function unwrapCredentialImportPayload(parsed: unknown): unknown[] {
  if (Array.isArray(parsed)) return parsed
  if (!parsed || typeof parsed !== 'object') throw new Error('无法识别的 JSON 格式')
  const obj = parsed as Record<string, unknown>
  if (Array.isArray(obj.accounts)) return obj.accounts
  if (Array.isArray(obj.credentials)) return obj.credentials
  if (
    (obj.credentials && typeof obj.credentials === 'object') ||
    typeof obj.refreshToken === 'string' ||
    typeof obj.refresh_token === 'string' ||
    typeof obj.kiroApiKey === 'string' ||
    typeof obj.kiro_api_key === 'string'
  ) return [obj]
  throw new Error('无法识别的导入格式')
}
```

`batch-import-dialog.tsx` 删除旧 `parseImportEntries()`，导入并调用该纯函数；`normalizeImportEntry()` 保持不变。

- [ ] **Step 4: 运行前端测试、类型检查和生产构建**

Run: `bun test`

Expected: 现有 91 项加新增测试全部 PASS。

Run: `bun run build`

Expected: TypeScript 和 Vite 构建退出码 0。

- [ ] **Step 5: 本地提交**

```powershell
git add -- admin-ui/src/lib/credential-import.ts admin-ui/src/lib/credential-import.test.ts admin-ui/src/components/batch-import-dialog.tsx
git commit -m "fix(admin-ui): 兼容完整凭据数组导入"
```

### Task 4: 阶段一整体回归和安全检查

**Files:**
- Verify only; only fix files already in Tasks 1-3 when a regression is found.

- [ ] **Step 1: 运行 Python 全量测试**

Run: `python -m unittest discover -s tests/batch_login -t .`

Expected: 现有 218 项加新增测试全部 PASS。

- [ ] **Step 2: 运行前端全量测试与构建**

Run: `bun test`（目录 `admin-ui`）

Expected: 0 fail。

Run: `bun run build`（目录 `admin-ui`）

Expected: 退出码 0。

- [ ] **Step 3: 运行 Rust 全量测试**

Run: `cargo test`

Expected: 1054 项或更多测试 PASS；允许基线已有的两条 warning，不新增 warning。

- [ ] **Step 4: 检查敏感信息和 Git 差异**

```powershell
rg -n "refresh-secret|access-secret|client-secret|plain-admin-key" scripts tests admin-ui/src
git diff --check master...HEAD
git status --short
```

测试夹具只能出现在测试文件中；生产代码、日志和错误信息不得含测试 Secret。工作区只允许计划内源码和测试改动，账号文件、数据库、导出 JSON、`admin-ui/dist` 与 `node_modules` 必须未跟踪或被忽略。

- [ ] **Step 5: 请求规格符合性和代码质量复核**

先让独立复核者逐条核对阶段一规格，再进行代码质量、安全和测试完整性复核。所有 Critical/Important 问题修复并重新验证后才进入阶段二。
