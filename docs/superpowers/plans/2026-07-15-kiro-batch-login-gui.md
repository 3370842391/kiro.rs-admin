# Kiro 批量登录桌面助手实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增独立 Tkinter 桌面助手，在本机解析账号、完成企业/Microsoft 自动登录、原子保存完整凭据 JSON，并可通过直接连接或自动 SSH 隧道导入 RS。

**Architecture:** 保留现有 RS 绑定 CLI，扩展共享解析器和浏览器驱动；新增纯 Python 的 IdC、Kiro Portal/Microsoft 协议层、敏感凭据存储、稳定 checkpoint、SSE 导入、SSH 子进程和本地批次编排。Tkinter 主线程只负责视图，后台线程持有 asyncio 事件循环、Playwright、网络连接和取消任务，双方通过结构化事件队列通信。

**Tech Stack:** Python 3.11+、Tkinter 8.6、`asyncio`、`httpx>=0.28`、Playwright async API、系统 OpenSSH、`unittest`、`httpx.MockTransport`。

---

## 文件结构

- `scripts/batch_login/input_parser.py`：支持固定前后缀的模板编译、账号解析和统一格式渲染。
- `scripts/batch_login/credential_models.py`：完整凭据的强类型模型、camelCase 导入结构和稳定去重键。
- `scripts/batch_login/credential_store.py`：敏感 JSON bundle 的加载、原子写入、权限收紧和去重追加。
- `scripts/batch_login/local_checkpoint.py`：不依赖行号的脱敏恢复记录。
- `scripts/batch_login/worker_events.py`：GUI/后台之间的事件、运行设置和批次摘要模型。
- `scripts/batch_login/browser_flows.py`：保留现有页面驱动，新增结构化人工接管事件。
- `scripts/batch_login/local_idc.py`：AWS IAM Identity Center 客户端注册、设备授权和 token 轮询。
- `scripts/batch_login/local_microsoft.py`：Kiro Portal PKCE、social token 兑换、Entra discovery/二段 PKCE 和端点安全校验。
- `scripts/batch_login/local_auth.py`：把协议层与 `BrowserFlows` 组合为企业/Microsoft 单账号认证后端。
- `scripts/batch_login/rs_import.py`：读取 RS batch-import SSE 流并产生逐项事件。
- `scripts/batch_login/ssh_tunnel.py`：安全构造并管理系统 `ssh -N -L` 子进程。
- `scripts/batch_login/local_runner.py`：串行批次、保存优先、checkpoint、恢复、取消和可选导入。
- `scripts/batch_login/gui_controller.py`：纯 Python 表单状态、校验、工作线程和事件消费。
- `scripts/batch_login/gui_runtime.py`：组装 Playwright、协议客户端、存储、SSH、RS 导入与释放顺序。
- `scripts/batch_login/gui_app.py`：Tkinter 控件树、布局、文件对话框和用户交互。
- `scripts/kiro_batch_login_gui.py`：桌面程序薄入口。
- `scripts/requirements-batch-login.txt`：继续只声明 `httpx` 和 `playwright`；Tkinter 与 OpenSSH 作为系统能力检查。
- `tests/batch_login/test_input_parser.py`：丰富模板和统一输出测试。
- `tests/batch_login/test_credential_store.py`：凭据模型、原子写入、去重和敏感内容测试。
- `tests/batch_login/test_local_checkpoint.py`：稳定恢复键和脱敏测试。
- `tests/batch_login/test_browser_contract.py`：人工接管事件与现有页面流程回归。
- `tests/batch_login/test_local_idc.py`：IdC 协议状态测试。
- `tests/batch_login/test_local_microsoft.py`：Microsoft/social/external IdP 协议测试。
- `tests/batch_login/test_local_auth.py`：协议与浏览器组合测试。
- `tests/batch_login/test_rs_import.py`：SSE 解析、鉴权和取消测试。
- `tests/batch_login/test_ssh_tunnel.py`：命令构造、端口重试和进程清理测试。
- `tests/batch_login/test_local_runner.py`：保存优先、恢复、继续执行和取消测试。
- `tests/batch_login/test_gui_controller.py`：无显示器环境下的控制器状态测试。
- `tests/batch_login/test_gui_entrypoint.py`：入口导入、依赖提示和参数边界测试。
- `README.md`：GUI 安装、启动、本地 JSON、SSH、导入和安全说明。

## 任务依赖与并行边界

- Task 1、Task 2、Task 3 可由独立子代理并行实施，复核后再进入协议任务。
- Task 4 与 Task 5 依赖 Task 2 的凭据模型，但彼此可并行。
- Task 6 依赖现有浏览器驱动，能与 Task 4/5 并行，但必须在 Task 7 前合并。
- Task 8 与 Task 9 彼此独立，可并行。
- Task 7 依赖 Task 4、5、6；Task 10 依赖 Task 2、3、7、8、9。
- Task 11、12、13 按顺序执行，避免 GUI、文档和最终验证在共享文件上冲突。

### Task 1：扩展账号模板解析与统一输出

**Files:**
- Modify: `scripts/batch_login/input_parser.py:12-78`
- Modify: `tests/batch_login/test_input_parser.py:64-118`

- [ ] **Step 1：写固定前后缀与特殊密码的失败测试**

在 `tests/batch_login/test_input_parser.py` 增加：

```python
from batch_login.input_parser import parse_accounts, render_accounts


def test_prefixed_template_preserves_special_password(self):
    raw = r"login = admin-user30 / onetime password = ^_S!Ibq1xcU*EwBD$\_AsY8/Oo)"
    result = parse_accounts(
        raw,
        "login = {account} / onetime password = {password}",
        LoginMode.ENTERPRISE,
    )

    self.assertEqual([], result.issues)
    self.assertEqual("admin-user30", result.entries[0].account)
    self.assertEqual(r"^_S!Ibq1xcU*EwBD$\_AsY8/Oo)", result.entries[0].password)


def test_render_accounts_supports_custom_output_template(self):
    parsed = parse_accounts(
        "login = alice / onetime password = p/a#s<s>",
        "login = {account} / onetime password = {password}",
        LoginMode.ENTERPRISE,
    )

    self.assertEqual(
        "alice----p/a#s<s>",
        render_accounts(parsed.entries, "{account}----{password}"),
    )
```

- [ ] **Step 2：运行测试并确认旧编译器拒绝前后缀**

Run: `python -m unittest tests.batch_login.test_input_parser.InputParserTests.test_prefixed_template_preserves_special_password tests.batch_login.test_input_parser.InputParserTests.test_render_accounts_supports_custom_output_template -v`

Expected: FAIL，前者抛出模板限制 `ValueError`，后者因 `render_accounts` 不存在而失败。

- [ ] **Step 3：把模板编译为整行正则并增加渲染函数**

将 `CompiledFormat` 和 `compile_format` 改为：

```python
@dataclass(frozen=True, slots=True)
class CompiledFormat:
    pattern: re.Pattern[str]


def _validate_placeholders(template: str) -> None:
    if template.count("{account}") != 1 or template.count("{password}") != 1:
        raise ValueError("格式模板必须恰好包含一次 {account} 和一次 {password}")


def compile_format(template: str) -> CompiledFormat:
    _validate_placeholders(template)
    cursor = 0
    pieces = ["^"]
    for match in re.finditer(r"\{(account|password)\}", template):
        pieces.append(re.escape(template[cursor : match.start()]))
        name = match.group(1)
        pieces.append(
            r"(?P<account>.*?)" if name == "account" else r"(?P<password>.*)"
        )
        cursor = match.end()
    pieces.append(re.escape(template[cursor:]))
    pieces.append("$")
    return CompiledFormat(re.compile("".join(pieces)))


def render_accounts(entries: list[AccountEntry], template: str) -> str:
    _validate_placeholders(template)
    return "\n".join(
        template.replace("{account}", entry.account).replace(
            "{password}", entry.password
        )
        for entry in entries
    )
```

在 `parse_accounts` 中用 `compiled.pattern.fullmatch(line)` 取得命名分组；匹配失败记录 `format_mismatch`，其余账号校验和去重逻辑保持不变。

- [ ] **Step 4：更新旧模板限制测试并运行完整解析测试**

旧测试只保留缺少、重复占位符为非法；固定前后缀和字面量花括号改为合法用例。

Run: `python -m unittest tests.batch_login.test_input_parser -v`

Expected: PASS，所有解析与渲染测试通过。

- [ ] **Step 5：提交解析器变更**

```powershell
git add -- scripts/batch_login/input_parser.py tests/batch_login/test_input_parser.py
git commit -m "feat(batch-login): 支持完整账号模板转换"
```

### Task 2：建立完整凭据模型与原子存储

**Files:**
- Create: `scripts/batch_login/credential_models.py`
- Create: `scripts/batch_login/credential_store.py`
- Create: `tests/batch_login/test_credential_store.py`

- [ ] **Step 1：写 camelCase、去重和敏感写盘失败测试**

创建 `tests/batch_login/test_credential_store.py`：

```python
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.credential_models import CredentialRecord
from batch_login.credential_store import CredentialStore


class CredentialStoreTests(unittest.TestCase):
    def idc_record(self):
        return CredentialRecord(
            email="Admin-User",
            auth_method="idc",
            provider="Enterprise",
            refresh_token="refresh-secret",
            access_token="access-secret",
            client_id="client",
            client_secret="client-secret",
            start_url="https://example.awsapps.com/start",
            region="us-east-1",
            expires_at="2026-07-15T01:00:00Z",
        )

    def test_as_add_request_uses_rs_camel_case(self):
        payload = self.idc_record().as_add_request()
        self.assertEqual("refresh-secret", payload["refreshToken"])
        self.assertEqual("client-secret", payload["clientSecret"])
        self.assertEqual("https://example.awsapps.com/start", payload["startUrl"])
        self.assertNotIn("refresh_token", payload)

    def test_append_is_atomic_and_deduplicates_casefolded_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "credentials.json"
            store = CredentialStore(path)
            self.assertTrue(store.append(self.idc_record()))
            duplicate = self.idc_record()
            duplicate.email = "admin-user"
            self.assertFalse(store.append(duplicate))
            bundle = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(1, bundle["version"])
            self.assertEqual(1, len(bundle["credentials"]))
            self.assertNotIn("password", path.read_text(encoding="utf-8").casefold())
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_credential_store -v`

Expected: FAIL with `ModuleNotFoundError: batch_login.credential_models`。

- [ ] **Step 3：实现强类型凭据与稳定去重键**

创建 `scripts/batch_login/credential_models.py`：

```python
from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass(slots=True)
class CredentialRecord:
    email: str
    auth_method: str
    provider: str
    refresh_token: str | None = None
    access_token: str | None = None
    profile_arn: str | None = None
    expires_at: str | None = None
    client_id: str | None = None
    client_secret: str | None = None
    start_url: str | None = None
    token_endpoint: str | None = None
    issuer_url: str | None = None
    scopes: str | None = None
    region: str | None = None
    priority: int = 0
    rpm_limit: int = 10
    source_channel: str = "batch-login-gui"

    def dedupe_key(self) -> tuple[str, str, str]:
        scope = self.start_url or self.issuer_url or ""
        return self.auth_method, self.email.casefold(), scope.casefold().rstrip("/")

    def as_add_request(self) -> dict[str, Any]:
        mapping = {
            "email": self.email,
            "authMethod": self.auth_method,
            "provider": self.provider,
            "refreshToken": self.refresh_token,
            "accessToken": self.access_token,
            "profileArn": self.profile_arn,
            "expiresAt": self.expires_at,
            "clientId": self.client_id,
            "clientSecret": self.client_secret,
            "startUrl": self.start_url,
            "tokenEndpoint": self.token_endpoint,
            "issuerUrl": self.issuer_url,
            "scopes": self.scopes,
            "region": self.region,
            "priority": self.priority,
            "rpmLimit": self.rpm_limit,
            "sourceChannel": self.source_channel,
        }
        return {key: value for key, value in mapping.items() if value is not None}

    @classmethod
    def from_add_request(cls, payload: dict[str, Any]) -> "CredentialRecord":
        return cls(
            email=str(payload.get("email") or ""),
            auth_method=str(payload.get("authMethod") or "social"),
            provider=str(payload.get("provider") or ""),
            refresh_token=payload.get("refreshToken"),
            access_token=payload.get("accessToken"),
            profile_arn=payload.get("profileArn"),
            expires_at=payload.get("expiresAt"),
            client_id=payload.get("clientId"),
            client_secret=payload.get("clientSecret"),
            start_url=payload.get("startUrl"),
            token_endpoint=payload.get("tokenEndpoint"),
            issuer_url=payload.get("issuerUrl"),
            scopes=payload.get("scopes"),
            region=payload.get("region"),
            priority=int(payload.get("priority", 0)),
            rpm_limit=int(payload.get("rpmLimit", 10)),
            source_channel=str(payload.get("sourceChannel") or "batch-login-gui"),
        )
```

- [ ] **Step 4：实现同目录临时文件、fsync、权限和原子替换**

创建 `scripts/batch_login/credential_store.py`，实现以下公开边界：

```python
import json
import os
import stat
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

from .credential_models import CredentialRecord


class CredentialStoreError(RuntimeError):
    pass


class CredentialStore:
    def __init__(self, path: Path, *, warning_sink=lambda _message: None):
        self.path = path
        self.warning_sink = warning_sink

    def load(self) -> list[CredentialRecord]:
        if not self.path.exists():
            return []
        payload = json.loads(self.path.read_text(encoding="utf-8"))
        if payload.get("version") != 1 or not isinstance(payload.get("credentials"), list):
            raise CredentialStoreError("凭据文件格式无效")
        return [CredentialRecord.from_add_request(item) for item in payload["credentials"]]

    def append(self, record: CredentialRecord) -> bool:
        records = self.load()
        if record.dedupe_key() in {item.dedupe_key() for item in records}:
            return False
        records.append(record)
        self._write(records)
        return True

    def _write(self, records: list[CredentialRecord]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temp = self.path.with_name(f".{self.path.name}.{uuid4().hex}.tmp")
        payload = {
            "version": 1,
            "generatedAt": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            "credentials": [item.as_add_request() for item in records],
        }
        try:
            with temp.open("x", encoding="utf-8", newline="\n") as handle:
                json.dump(payload, handle, ensure_ascii=False, indent=2)
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
            try:
                os.chmod(temp, stat.S_IRUSR | stat.S_IWUSR)
            except OSError:
                self.warning_sink("无法确认凭据文件权限，请手动限制为仅当前用户可读写")
            os.replace(temp, self.path)
        except Exception as error:
            temp.unlink(missing_ok=True)
            raise CredentialStoreError("完整凭据 JSON 写入失败") from error
```

不得在异常消息中拼接 record 或 payload。

- [ ] **Step 5：运行存储测试并检查敏感序列化边界**

Run: `python -m unittest tests.batch_login.test_credential_store -v`

Expected: PASS。

Run: `python -m unittest tests.batch_login.test_redaction tests.batch_login.test_checkpoint -v`

Expected: PASS，现有脱敏 checkpoint 不受影响。

- [ ] **Step 6：提交凭据模型与存储**

```powershell
git add -- scripts/batch_login/credential_models.py scripts/batch_login/credential_store.py tests/batch_login/test_credential_store.py
git commit -m "feat(batch-login): 添加完整凭据原子存储"
```

### Task 3：新增稳定 checkpoint 与后台事件模型

**Files:**
- Create: `scripts/batch_login/local_checkpoint.py`
- Create: `scripts/batch_login/worker_events.py`
- Create: `tests/batch_login/test_local_checkpoint.py`

- [ ] **Step 1：写账号重排仍能恢复的失败测试**

创建 `tests/batch_login/test_local_checkpoint.py`：

```python
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from batch_login.local_checkpoint import LocalCheckpointStore, LocalRunRecord


class LocalCheckpointTests(unittest.TestCase):
    def test_resume_key_does_not_depend_on_line_number(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = LocalCheckpointStore(Path(tmp) / "run.jsonl")
            store.append(LocalRunRecord.success(
                run_id="run-1",
                line_number=2,
                account="Admin-User",
                mode="enterprise",
                scope="https://example.awsapps.com/start",
                credential_saved=True,
            ))

            self.assertFalse(store.should_run(
                account="admin-user",
                mode="enterprise",
                scope="https://example.awsapps.com/start/",
                resume=True,
            ))
            raw = store.path.read_text(encoding="utf-8")
            self.assertNotIn("Admin-User", raw)
            self.assertNotIn("password", raw.casefold())
            self.assertNotIn("token", raw.casefold())
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_local_checkpoint -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：实现稳定恢复键和追加记录**

`scripts/batch_login/local_checkpoint.py` 公开以下接口：

```python
def account_hash(account: str) -> str:
    return sha256(account.casefold().encode("utf-8")).hexdigest()


def resume_key(account: str, mode: str, scope: str) -> tuple[str, str, str]:
    return mode, account_hash(account), scope.casefold().rstrip("/")


@dataclass(slots=True)
class LocalRunRecord:
    run_id: str
    line_number: int
    account_hash: str
    account_masked: str
    mode: str
    scope: str
    status: str
    stage: str
    timestamp: str
    retryable: bool
    credential_saved: bool
    code: str | None = None
    message: str | None = None
    import_status: str | None = None
    credential_id: int | None = None

    @classmethod
    def success(cls, *, run_id, line_number, account, mode, scope, credential_saved):
        return cls(
            run_id=run_id,
            line_number=line_number,
            account_hash=account_hash(account),
            account_masked=mask_account(account),
            mode=mode,
            scope=scope.casefold().rstrip("/"),
            status="success",
            stage="saved",
            timestamp=datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            retryable=False,
            credential_saved=credential_saved,
        )
```

`LocalCheckpointStore` 在初始化时读取 JSONL 最新记录，索引键只使用 `(mode, accountHash, scope)`；`should_run` 对 `success + credentialSaved` 返回 false，对 `manual_required`、`cancelled` 和 `retryable=true` 返回 true。`append` 使用 `redact_text` 清洗 code/message，逐行 `flush + fsync`。

增加 `append_import_result(previous, *, import_status, credential_id, message=None)`，以同一恢复键追加 `stage="import"` 记录；只允许 `imported/verified/duplicate/failed`，message 先脱敏。这样重新导入已有 JSON 时无需改写登录成功记录，也能在 GUI 中恢复导入结果。

- [ ] **Step 4：定义 GUI/工作器共享事件与设置**

创建 `scripts/batch_login/worker_events.py`：

```python
class ResultMode(str, Enum):
    SAVE_ONLY = "save_only"
    SAVE_AND_IMPORT = "save_and_import"


@dataclass(slots=True, frozen=True)
class WorkerEvent:
    kind: str
    payload: dict[str, Any]


@dataclass(slots=True, frozen=True)
class LocalRunSettings:
    mode: LoginMode
    region: str
    start_url: str | None
    headless: bool
    timeout_seconds: float
    mfa_timeout_seconds: float
    result_mode: ResultMode
    credential_path: Path
    checkpoint_path: Path
    resume: bool = False


@dataclass(slots=True)
class BatchSummary:
    total: int
    succeeded: int = 0
    duplicate: int = 0
    failed: int = 0
    manual_required: int = 0
    cancelled: int = 0
    imported: int = 0
```

- [ ] **Step 5：运行 checkpoint 测试**

Run: `python -m unittest tests.batch_login.test_local_checkpoint -v`

Expected: PASS。

- [ ] **Step 6：提交恢复与事件模型**

```powershell
git add -- scripts/batch_login/local_checkpoint.py scripts/batch_login/worker_events.py tests/batch_login/test_local_checkpoint.py
git commit -m "feat(batch-login): 添加稳定恢复与任务事件"
```

### Task 4：实现本地 AWS IdC 协议客户端

**Files:**
- Create: `scripts/batch_login/local_idc.py`
- Create: `tests/batch_login/test_local_idc.py`

- [ ] **Step 1：写注册、pending、slow_down 和成功测试**

创建 `tests/batch_login/test_local_idc.py`，使用 `httpx.MockTransport` 按 path 返回：

```python
class LocalIdcTests(unittest.IsolatedAsyncioTestCase):
    async def test_start_and_poll_returns_token(self):
        replies = iter([
            httpx.Response(200, json={
                "clientId": "client",
                "clientSecret": "secret",
            }),
            httpx.Response(200, json={
                "deviceCode": "device",
                "userCode": "ABCD-EFGH",
                "verificationUri": "https://device.example/start",
                "verificationUriComplete": "https://device.example/start?user_code=ABCD-EFGH",
                "expiresIn": 600,
                "interval": 0,
            }),
            httpx.Response(400, json={"error": "authorization_pending"}),
            httpx.Response(400, json={"error": "slow_down"}),
            httpx.Response(200, json={
                "accessToken": "access",
                "refreshToken": "refresh",
                "expiresIn": 3600,
            }),
        ])

        async def handler(_request):
            return next(replies)

        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            client = LocalIdcClient(http, sleep=lambda _seconds: asyncio.sleep(0))
            session = await client.start("https://example.awsapps.com/start", "us-east-1")
            token = await client.poll(session)

        self.assertEqual("client", session.client_id)
        self.assertEqual("refresh", token.refresh_token)
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_local_idc -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：实现 IdC 会话、token 和错误类型**

创建 `scripts/batch_login/local_idc.py`：

```python
IDC_SCOPES = [
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
]


@dataclass(slots=True, frozen=True)
class IdcSession:
    region: str
    start_url: str
    client_id: str
    client_secret: str
    device_code: str
    user_code: str
    verification_url: str
    expires_at: float
    interval: float


@dataclass(slots=True, frozen=True)
class IdcToken:
    access_token: str
    refresh_token: str | None
    expires_in: int | None


@dataclass(slots=True)
class LocalAuthError(Exception):
    code: str
    stage: str
    retryable: bool
    message: str

    def __str__(self):
        return self.message
```

- [ ] **Step 4：实现注册、设备授权与轮询**

`LocalIdcClient.start` 必须向 `/client/register` 发送 `clientName="kiro-rs"`、`clientType="public"`、device-code/refresh grant、scopes 和 `issuerUrl`，再向 `/device_authorization` 发送 client/start URL。`poll` 向 `/token` 发送 device-code grant：

```python
class LocalIdcClient:
    def __init__(self, http: httpx.AsyncClient, *, sleep=asyncio.sleep, now=time.monotonic):
        self.http = http
        self.sleep = sleep
        self.now = now

    def endpoint(self, region: str) -> str:
        if not re.fullmatch(r"[a-z0-9-]+", region):
            raise LocalAuthError("invalid_region", "config", False, "Region 格式无效")
        return f"https://oidc.{region}.amazonaws.com"

    async def start(self, start_url: str, region: str) -> IdcSession:
        start_parts = urlsplit(start_url.strip())
        if start_parts.scheme != "https" or not start_parts.hostname:
            raise LocalAuthError("invalid_start_url", "config", False, "Start URL 必须是 HTTPS 地址")
        base = self.endpoint(region)
        registered = await self.http.post(base + "/client/register", json={
            "clientName": "kiro-rs",
            "clientType": "public",
            "scopes": IDC_SCOPES,
            "grantTypes": [
                "urn:ietf:params:oauth:grant-type:device_code",
                "refresh_token",
            ],
            "issuerUrl": start_url,
        })
        if not registered.is_success:
            raise LocalAuthError("idc_register_failed", "idc_register", registered.status_code >= 500, "注册 IdC 客户端失败")
        reg = registered.json()
        client_id = reg.get("clientId")
        client_secret = reg.get("clientSecret")
        if not isinstance(client_id, str) or not isinstance(client_secret, str):
            raise LocalAuthError("invalid_idc_response", "idc_register", False, "IdC 注册响应格式无效")

        started = await self.http.post(base + "/device_authorization", json={
            "clientId": client_id,
            "clientSecret": client_secret,
            "startUrl": start_url,
        })
        if not started.is_success:
            raise LocalAuthError("idc_start_failed", "idc_start", started.status_code >= 500, "发起设备授权失败")
        body = started.json()
        verification_url = body.get("verificationUriComplete") or body.get("verificationUri")
        if not all(isinstance(body.get(key), str) for key in ("deviceCode", "userCode")) or not isinstance(verification_url, str):
            raise LocalAuthError("invalid_idc_response", "idc_start", False, "设备授权响应格式无效")
        expires_in = int(body.get("expiresIn", 600))
        interval = float(body.get("interval", 5))
        return IdcSession(
            region=region,
            start_url=start_url,
            client_id=client_id,
            client_secret=client_secret,
            device_code=body["deviceCode"],
            user_code=body["userCode"],
            verification_url=verification_url,
            expires_at=self.now() + expires_in,
            interval=interval,
        )

    async def poll(self, session: IdcSession) -> IdcToken:
        interval = max(session.interval, 0.2)
        while self.now() < session.expires_at:
            response = await self.http.post(
                self.endpoint(session.region) + "/token",
                json={
                    "clientId": session.client_id,
                    "clientSecret": session.client_secret,
                    "grantType": "urn:ietf:params:oauth:grant-type:device_code",
                    "deviceCode": session.device_code,
                },
            )
            if response.is_success:
                body = response.json()
                return IdcToken(body["accessToken"], body.get("refreshToken"), body.get("expiresIn"))
            error = response.json().get("error")
            if error == "authorization_pending":
                await self.sleep(interval)
                continue
            if error == "slow_down":
                interval += 5
                await self.sleep(interval)
                continue
            if error == "expired_token":
                raise LocalAuthError("session_expired", "idc_poll", False, "设备授权已过期")
            if error == "access_denied":
                raise LocalAuthError("access_denied", "idc_poll", False, "用户拒绝授权")
            raise LocalAuthError("idc_token_failed", "idc_poll", response.status_code >= 500, "IdC token 请求失败")
        raise LocalAuthError("session_expired", "idc_poll", False, "设备授权已过期")
```

`poll` 成功响应同样验证 `accessToken` 为非空字符串，`refreshToken` 为字符串或 null，`expiresIn` 为整数或 null。异常不得包含原始响应正文、client secret 或 device code。

- [ ] **Step 5：运行 IdC 协议测试**

Run: `python -m unittest tests.batch_login.test_local_idc -v`

Expected: PASS，pending、slow_down、expired 和拒绝分支均通过。

- [ ] **Step 6：提交 IdC 客户端**

```powershell
git add -- scripts/batch_login/local_idc.py tests/batch_login/test_local_idc.py
git commit -m "feat(batch-login): 添加本地企业 IdC 协议"
```

### Task 5：实现本地 Kiro Portal 与 Microsoft/Entra 协议

**Files:**
- Create: `scripts/batch_login/local_microsoft.py`
- Create: `tests/batch_login/test_local_microsoft.py`

- [ ] **Step 1：写 URL、安全校验和两类 token 兑换失败测试**

创建测试覆盖：

```python
class LocalMicrosoftTests(unittest.IsolatedAsyncioTestCase):
    def test_signin_url_contains_pkce_state_and_fixed_redirect(self):
        session = MicrosoftProtocol.new_session(region="us-east-1")
        parts = urlsplit(session.signin_url)
        query = parse_qs(parts.query)
        self.assertEqual("https", parts.scheme)
        self.assertEqual("app.kiro.dev", parts.hostname)
        self.assertEqual([session.state], query["state"])
        self.assertEqual(["http://localhost:3128"], query["redirect_uri"])
        self.assertEqual(["S256"], query["code_challenge_method"])

    def test_external_idp_rejects_non_allowlisted_endpoint(self):
        with self.assertRaises(LocalAuthError) as raised:
            validate_external_endpoint("https://login.microsoftonline.com.evil.example/token")
        self.assertEqual("unsafe_idp_endpoint", raised.exception.code)

    async def test_social_exchange_reads_camel_case_token(self):
        async def handler(_request):
            return httpx.Response(200, json={
                "accessToken": "access",
                "refreshToken": "refresh",
                "expiresIn": 3600,
                "profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/p",
            })
        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
            result = await MicrosoftProtocol(http).exchange_social("code", "verifier")
        self.assertEqual("refresh", result.refresh_token)
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_local_microsoft -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：实现 PKCE、Portal 会话和回调描述符**

创建以下模型与纯函数：

```python
SIGNIN_URL = "https://app.kiro.dev/signin"
REDIRECT_URI = "http://localhost:3128"
SOCIAL_TOKEN_URL = "https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token"
ALLOWED_IDP_SUFFIXES = (
    ".microsoftonline.com",
    ".microsoftonline.us",
    ".microsoftonline.cn",
)


def random_urlsafe(size: int) -> str:
    return base64.urlsafe_b64encode(secrets.token_bytes(size)).rstrip(b"=").decode()


def pkce_challenge(verifier: str) -> str:
    digest = hashlib.sha256(verifier.encode()).digest()
    return base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


@dataclass(slots=True, frozen=True)
class MicrosoftSession:
    state: str
    verifier: str
    signin_url: str
    region: str


@dataclass(slots=True, frozen=True)
class PortalCallback:
    kind: str
    code: str | None = None
    issuer_url: str | None = None
    client_id: str | None = None
    scopes: str = ""
    login_hint: str = ""


def validate_external_endpoint(raw_url: str) -> str:
    parts = urlsplit(raw_url.strip())
    host = (parts.hostname or "").casefold()
    if parts.scheme != "https" or not host:
        raise LocalAuthError(
            "unsafe_idp_endpoint", "microsoft_discovery", False,
            "外部身份端点必须使用 HTTPS 域名",
        )
    try:
        ipaddress.ip_address(host)
    except ValueError:
        pass
    else:
        raise LocalAuthError(
            "unsafe_idp_endpoint", "microsoft_discovery", False,
            "外部身份端点不能使用 IP 地址",
        )
    if not any(host.endswith(suffix) for suffix in ALLOWED_IDP_SUFFIXES):
        raise LocalAuthError(
            "unsafe_idp_endpoint", "microsoft_discovery", False,
            "外部身份端点不在 Microsoft 白名单中",
        )
    return raw_url.strip()


def _callback_values(raw_url: str) -> dict[str, str]:
    parts = urlsplit(raw_url.strip())
    values = parse_qs(parts.query, keep_blank_values=True)
    for key, items in parse_qs(parts.fragment, keep_blank_values=True).items():
        values.setdefault(key, items)
    return {key: items[0] for key, items in values.items() if items}


def parse_portal_callback(raw_url: str, expected_state: str) -> PortalCallback:
    values = _callback_values(raw_url)
    if values.get("state") != expected_state:
        raise LocalAuthError("state_mismatch", "microsoft_callback", False, "OAuth state 不匹配")
    if values.get("error"):
        raise LocalAuthError("access_denied", "microsoft_callback", False, "Microsoft 授权失败")
    if values.get("code"):
        return PortalCallback(kind="social", code=values["code"])
    issuer = values.get("issuer_url") or values.get("issuerUrl")
    client_id = values.get("client_id") or values.get("clientId")
    if issuer and client_id:
        return PortalCallback(
            kind="external_idp",
            issuer_url=issuer,
            client_id=client_id,
            scopes=values.get("scopes") or values.get("scope") or "",
            login_hint=values.get("login_hint") or values.get("loginHint") or "",
        )
    raise LocalAuthError("invalid_callback", "microsoft_callback", False, "登录回调缺少授权信息")
```

导入 `ipaddress`、`parse_qs`、`urlsplit` 和 Task 4 的 `LocalAuthError`。Portal 回调解析不保留完整 URL，也不把 query 写入错误文本。

- [ ] **Step 4：实现 discovery、外部授权 URL与两种 token 兑换**

`MicrosoftProtocol` 公开：

```python
class MicrosoftProtocol:
    def __init__(self, http: httpx.AsyncClient):
        self.http = http

    parse_portal_callback = staticmethod(parse_portal_callback)

    @staticmethod
    def new_session(region: str) -> MicrosoftSession:
        verifier = random_urlsafe(96)
        state = random_urlsafe(32)
        query = urlencode({
            "state": state,
            "code_challenge": pkce_challenge(verifier),
            "code_challenge_method": "S256",
            "redirect_uri": REDIRECT_URI,
            "redirect_from": "KiroIDE",
        })
        return MicrosoftSession(state, verifier, f"{SIGNIN_URL}?{query}", region or "us-east-1")

    async def discover(self, issuer_url: str) -> tuple[str, str]:
        issuer = validate_external_endpoint(issuer_url)
        response = await self.http.get(issuer.rstrip("/") + "/.well-known/openid-configuration")
        response.raise_for_status()
        body = response.json()
        return (
            validate_external_endpoint(body["authorization_endpoint"]),
            validate_external_endpoint(body["token_endpoint"]),
        )

    async def exchange_social(self, code: str, verifier: str) -> MicrosoftToken:
        response = await self.http.post(SOCIAL_TOKEN_URL, json={
            "code": code,
            "code_verifier": verifier,
            "redirect_uri": REDIRECT_URI,
        })
        response.raise_for_status()
        body = response.json()
        return MicrosoftToken(
            access_token=body["accessToken"],
            refresh_token=body.get("refreshToken"),
            expires_in=body.get("expiresIn"),
            profile_arn=body.get("profileArn"),
        )
```

同一模块加入：

```python
@dataclass(slots=True, frozen=True)
class MicrosoftToken:
    access_token: str
    refresh_token: str | None
    expires_in: int | None
    profile_arn: str | None = None


@dataclass(slots=True, frozen=True)
class ExternalLeg:
    state: str
    verifier: str
    authorize_url: str
    token_endpoint: str
    issuer_url: str
    client_id: str
    scopes: str
    redirect_uri: str


async def prepare_external(self, callback: PortalCallback) -> ExternalLeg:
    if callback.issuer_url is None or callback.client_id is None:
        raise LocalAuthError("invalid_callback", "microsoft_callback", False, "外部身份描述符不完整")
    auth_endpoint, token_endpoint = await self.discover(callback.issuer_url)
    verifier = random_urlsafe(96)
    state = random_urlsafe(32)
    redirect_uri = REDIRECT_URI + "/oauth/callback"
    authorize_url = auth_endpoint + "?" + urlencode({
        "client_id": callback.client_id,
        "response_type": "code",
        "redirect_uri": redirect_uri,
        "scope": callback.scopes,
        "code_challenge": pkce_challenge(verifier),
        "code_challenge_method": "S256",
        "response_mode": "query",
        "state": state,
        **({"login_hint": callback.login_hint} if callback.login_hint else {}),
    })
    return ExternalLeg(
        state=state,
        verifier=verifier,
        authorize_url=authorize_url,
        token_endpoint=token_endpoint,
        issuer_url=callback.issuer_url,
        client_id=callback.client_id,
        scopes=callback.scopes,
        redirect_uri=redirect_uri,
    )


async def exchange_external(self, leg: ExternalLeg, callback_url: str) -> MicrosoftToken:
    values = _callback_values(callback_url)
    if values.get("state") != leg.state:
        raise LocalAuthError("state_mismatch", "external_callback", False, "OAuth state 不匹配")
    if values.get("error") or not values.get("code"):
        raise LocalAuthError("access_denied", "external_callback", False, "Entra 授权失败")
    response = await self.http.post(
        leg.token_endpoint,
        data={
            "client_id": leg.client_id,
            "grant_type": "authorization_code",
            "code": values["code"],
            "redirect_uri": leg.redirect_uri,
            "code_verifier": leg.verifier,
            "scope": leg.scopes,
        },
        headers={"content-type": "application/x-www-form-urlencoded"},
    )
    response.raise_for_status()
    body = response.json()
    if not body.get("access_token"):
        raise LocalAuthError("token_missing", "external_token", False, "Entra token 响应缺少 access_token")
    return MicrosoftToken(body["access_token"], body.get("refresh_token"), body.get("expires_in"))


def email_from_jwt(token: str) -> str:
    try:
        segment = token.split(".")[1]
        segment += "=" * (-len(segment) % 4)
        claims = json.loads(base64.urlsafe_b64decode(segment).decode("utf-8"))
    except (IndexError, ValueError, UnicodeDecodeError, json.JSONDecodeError):
        return ""
    for key in ("preferred_username", "email", "upn", "unique_name", "name"):
        value = claims.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ""
```

把 `prepare_external` 和 `exchange_external` 定义为 `MicrosoftProtocol` 方法；`email_from_jwt` 为模块级纯函数。

- [ ] **Step 5：运行 Microsoft 协议测试**

Run: `python -m unittest tests.batch_login.test_local_microsoft -v`

Expected: PASS，包含 social、external_idp、state mismatch、恶意 host、无 token 和 JWT 邮箱解析测试。

- [ ] **Step 6：提交 Microsoft 协议层**

```powershell
git add -- scripts/batch_login/local_microsoft.py tests/batch_login/test_local_microsoft.py
git commit -m "feat(batch-login): 添加本地微软认证协议"
```

### Task 6：让浏览器流程报告人工接管事件

**Files:**
- Modify: `scripts/batch_login/browser_flows.py:28-318`
- Modify: `tests/batch_login/test_browser_contract.py:16-146`

- [ ] **Step 1：写 MFA 事件失败测试**

在 fixture 页面增加：

```python
"/mfa": """
  <p>verification code required</p>
  <script>setTimeout(() => document.body.innerText = 'approved', 100)</script>
""",
```

增加测试：

```python
async def test_manual_step_reports_structured_event(self):
    events = []
    driver = BrowserFlows(
        self.browser,
        timeout_seconds=1,
        mfa_timeout_seconds=1,
        event_sink=events.append,
    )
    async with driver.account_context() as session:
        await session.page.goto(self.base_url + "/mfa")
        await session._wait_for_manual_step(
            "verification code required",
            "mfa_timeout",
        )

    self.assertEqual("manual_action_required", events[0]["kind"])
    self.assertEqual("mfa", events[0]["manualKind"])
    self.assertNotIn("verification code required", str(events[0]))
```

- [ ] **Step 2：运行测试并确认构造器不接受 event_sink**

Run: `python -m unittest tests.batch_login.test_browser_contract.BrowserContractTests.test_manual_step_reports_structured_event -v`

Expected: FAIL with unexpected keyword argument `event_sink`。

- [ ] **Step 3：把事件回调传入每个账号会话**

在 `browser_flows.py` 增加：

```python
from collections.abc import Callable
from typing import Any

BrowserEventSink = Callable[[dict[str, Any]], None]


class BrowserFlows:
    def __init__(
        self,
        browser: Browser,
        *,
        timeout_seconds: float,
        mfa_timeout_seconds: float,
        event_sink: BrowserEventSink | None = None,
    ):
        if timeout_seconds <= 0 or mfa_timeout_seconds <= 0:
            raise ValueError("浏览器超时必须大于 0")
        self.browser = browser
        self.timeout_seconds = timeout_seconds
        self.mfa_timeout_seconds = mfa_timeout_seconds
        self.event_sink = event_sink
```

`account_context()` 构造 `AccountBrowserSession` 时传入 `event_sink`。会话新增：

```python
def _emit(self, kind: str, **payload: Any) -> None:
    if self.event_sink is not None:
        self.event_sink({"kind": kind, **payload})
```

把 `_wait_for_manual_step` 中的 `print` 替换为：

```python
self._emit(
    "manual_action_required",
    manualKind="captcha" if code == "captcha_required" else "mfa",
    message="请在当前浏览器窗口完成验证",
)
```

事件不得包含 body、账号、密码或 URL。

同时让 `complete_enterprise` 接受可选设备码，兼容 AWS 未返回 `verificationUriComplete` 的情况：

```python
async def _fill_device_code(self, user_code: str) -> bool:
    locator = await self._first_visible([
        self.page.get_by_label(re.compile(r"设备码|用户码|device code|user code", re.I)),
        self.page.locator("input[name='userCode'], input[name='user_code'], input[autocomplete='one-time-code']"),
    ])
    if locator is None:
        return False
    await locator.fill(user_code)
    await self._click_primary(False)
    return True


async def complete_enterprise(self, url, account, password, user_code=None):
    try:
        await self.page.goto(url, wait_until="domcontentloaded", timeout=self.timeout_ms)
        if user_code:
            await self._fill_device_code(user_code)
        await self._drive_login(account, password)
    except BrowserFlowError:
        raise
    except PlaywrightError as error:
        raise BrowserFlowError(
            "browser_navigation_failed", "browser_login", True,
            "无法打开登录页面",
        ) from error
```

现有三参数调用保持兼容。为 `/device-code` fixture 增加输入框和测试，断言提交后继续账号密码页。

- [ ] **Step 4：运行浏览器合约回归**

Run: `python -m unittest tests.batch_login.test_browser_contract -v`

Expected: PASS；原企业填写、回调捕获、错误分类和同 context 两段登录测试保持通过。

- [ ] **Step 5：提交浏览器事件变更**

```powershell
git add -- scripts/batch_login/browser_flows.py tests/batch_login/test_browser_contract.py
git commit -m "feat(batch-login): 暴露浏览器人工接管事件"
```

### Task 7：组合协议与浏览器为本地认证后端

**Files:**
- Create: `scripts/batch_login/local_auth.py`
- Create: `tests/batch_login/test_local_auth.py`

- [ ] **Step 1：写企业保存字段与 Microsoft 两段流程失败测试**

创建 `tests/batch_login/test_local_auth.py`，使用假的 BrowserFactory、IdC 与 MicrosoftProtocol：

```python
from contextlib import asynccontextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
from types import SimpleNamespace

FIXED_NOW = datetime(2026, 7, 15, tzinfo=timezone.utc)


@dataclass
class FakeIdcSession:
    client_id: str
    client_secret: str
    verification_url: str
    user_code: str = "ABCD-EFGH"


@dataclass
class FakeIdcToken:
    access_token: str
    refresh_token: str | None
    expires_in: int | None


class FakeIdc:
    def __init__(self, session, token):
        self.session = session
        self.token = token

    async def start(self, _start_url, _region):
        return self.session

    async def poll(self, _session):
        return self.token


class FakeBrowserSession:
    def __init__(self, owner):
        self.owner = owner

    async def complete_enterprise(self, url, _account, _password, _user_code=None):
        self.owner.enterprise_urls.append(url)

    async def capture_callback(self, _url, _account, _password, *, expected_path):
        self.owner.expected_paths.append(expected_path)
        return self.owner.callbacks.pop(0)


class FakeBrowserFactory:
    def __init__(self, callbacks=None):
        self.callbacks = list(callbacks or [])
        self.enterprise_urls = []
        self.expected_paths = []
        self.context_count = 0

    @asynccontextmanager
    async def account_context(self):
        self.context_count += 1
        yield FakeBrowserSession(self)


class FakeMicrosoftProtocol:
    @classmethod
    def external_idp(cls):
        return cls()

    def new_session(self, _region):
        return SimpleNamespace(state="s", signin_url="https://app.kiro.dev/signin")

    def parse_portal_callback(self, _url, _state):
        return PortalCallback(
            kind="external_idp",
            issuer_url="https://login.microsoftonline.com/t",
            client_id="c",
        )

    async def prepare_external(self, _callback):
        return SimpleNamespace(
            authorize_url="https://login.microsoftonline.com/t/authorize",
            token_endpoint="https://login.microsoftonline.com/t/token",
            issuer_url="https://login.microsoftonline.com/t",
            client_id="c",
            scopes="openid offline_access",
        )

    async def exchange_external(self, _leg, _url):
        return MicrosoftToken("access", "refresh", 3600)

    def external_record(self, input_email, region, leg, token, _now):
        return CredentialRecord(
            email=input_email,
            auth_method="external_idp",
            provider="Enterprise",
            access_token=token.access_token,
            refresh_token=token.refresh_token,
            client_id=leg.client_id,
            token_endpoint=leg.token_endpoint,
            issuer_url=leg.issuer_url,
            scopes=leg.scopes,
            region=region,
        )


class LocalAuthTests(unittest.IsolatedAsyncioTestCase):
    async def test_enterprise_returns_importable_idc_credential(self):
        idc = FakeIdc(
            session=FakeIdcSession(
                client_id="client",
                client_secret="secret",
                verification_url="https://verify",
            ),
            token=FakeIdcToken("access", "refresh", 3600),
        )
        browser = FakeBrowserFactory()
        backend = LocalEnterpriseAuth(idc, browser, now=lambda: FIXED_NOW)

        record = await backend.login(
            AccountEntry(1, "admin-user", "one-time-password"),
            EnterpriseSettings("https://example.awsapps.com/start", "us-east-1"),
        )

        self.assertEqual("idc", record.auth_method)
        self.assertEqual("refresh", record.refresh_token)
        self.assertEqual("client", record.client_id)
        self.assertEqual("https://verify", browser.enterprise_urls[0])

    async def test_microsoft_external_idp_reuses_one_account_context(self):
        protocol = FakeMicrosoftProtocol.external_idp()
        browser = FakeBrowserFactory(
            callbacks=["http://localhost:3128?issuer_url=https%3A%2F%2Flogin.microsoftonline.com%2Ft&client_id=c&state=s",
                       "http://localhost:3128/oauth/callback?code=final&state=s2"]
        )
        record = await LocalMicrosoftAuth(protocol, browser, now=lambda: FIXED_NOW).login(
            AccountEntry(1, "user@example.com", "password"),
            MicrosoftSettings(region="us-east-1"),
        )
        self.assertEqual(1, browser.context_count)
        self.assertEqual("external_idp", record.auth_method)
        self.assertEqual("https://login.microsoftonline.com/t/token", record.token_endpoint)
```

- [ ] **Step 2：运行测试并确认本地认证组合层不存在**

Run: `python -m unittest tests.batch_login.test_local_auth -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：实现企业认证组合**

创建 `scripts/batch_login/local_auth.py` 的公共设置与企业类：

```python
@dataclass(slots=True, frozen=True)
class EnterpriseSettings:
    start_url: str
    region: str


@dataclass(slots=True, frozen=True)
class MicrosoftSettings:
    region: str = "us-east-1"


class LocalEnterpriseAuth:
    def __init__(self, idc, browser_factory, *, now=lambda: datetime.now(timezone.utc)):
        self.idc = idc
        self.browser_factory = browser_factory
        self.now = now

    async def login(self, entry: AccountEntry, settings: EnterpriseSettings) -> CredentialRecord:
        session = await self.idc.start(settings.start_url, settings.region)
        async with self.browser_factory.account_context() as browser:
            browser_task = asyncio.create_task(
                browser.complete_enterprise(
                    session.verification_url,
                    entry.account,
                    entry.password,
                    session.user_code,
                )
            )
            token_task = asyncio.create_task(self.idc.poll(session))
            tasks = {browser_task, token_task}
            try:
                done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_EXCEPTION)
                for task in done:
                    task.result()
                token = await token_task
            finally:
                for task in tasks:
                    if not task.done():
                        task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)

        expires_at = None
        if token.expires_in is not None:
            expires_at = (self.now() + timedelta(seconds=token.expires_in)).isoformat().replace("+00:00", "Z")
        return CredentialRecord(
            email=entry.account,
            auth_method="idc",
            provider="Enterprise",
            refresh_token=token.refresh_token,
            access_token=token.access_token,
            client_id=session.client_id,
            client_secret=session.client_secret,
            start_url=settings.start_url,
            region=settings.region,
            expires_at=expires_at,
        )
```

- [ ] **Step 4：实现 Microsoft social/external_idp 组合**

```python
class LocalMicrosoftAuth:
    def __init__(self, protocol, browser_factory, *, now=lambda: datetime.now(timezone.utc)):
        self.protocol = protocol
        self.browser_factory = browser_factory
        self.now = now

    async def login(self, entry: AccountEntry, settings: MicrosoftSettings) -> CredentialRecord:
        session = self.protocol.new_session(settings.region)
        async with self.browser_factory.account_context() as browser:
            first_url = await browser.capture_callback(
                session.signin_url,
                entry.account,
                entry.password,
                expected_path="/",
            )
            callback = self.protocol.parse_portal_callback(first_url, session.state)
            if callback.kind == "social":
                token = await self.protocol.exchange_social(callback.code, session.verifier)
                return self.protocol.social_record(entry.account, settings.region, token, self.now())

            leg = await self.protocol.prepare_external(callback)
            final_url = await browser.capture_callback(
                leg.authorize_url,
                entry.account,
                entry.password,
                expected_path="/oauth/callback",
            )
            token = await self.protocol.exchange_external(leg, final_url)
            return self.protocol.external_record(entry.account, settings.region, leg, token, self.now())
```

在 `MicrosoftProtocol` 中增加确定的转换方法：

```python
@staticmethod
def _expires_at(now: datetime, expires_in: int | None) -> str | None:
    if expires_in is None:
        return None
    return (now + timedelta(seconds=expires_in)).isoformat().replace("+00:00", "Z")


def social_record(
    self,
    input_email: str,
    region: str,
    token: MicrosoftToken,
    now: datetime,
) -> CredentialRecord:
    return CredentialRecord(
        email=email_from_jwt(token.access_token) or input_email,
        auth_method="social",
        provider="Microsoft",
        refresh_token=token.refresh_token,
        access_token=token.access_token,
        profile_arn=token.profile_arn,
        region=region,
        expires_at=self._expires_at(now, token.expires_in),
    )


def external_record(
    self,
    input_email: str,
    region: str,
    leg: ExternalLeg,
    token: MicrosoftToken,
    now: datetime,
) -> CredentialRecord:
    return CredentialRecord(
        email=email_from_jwt(token.access_token) or input_email,
        auth_method="external_idp",
        provider="Enterprise",
        refresh_token=token.refresh_token,
        access_token=token.access_token,
        client_id=leg.client_id,
        token_endpoint=leg.token_endpoint,
        issuer_url=leg.issuer_url,
        scopes=leg.scopes,
        region=region,
        expires_at=self._expires_at(now, token.expires_in),
    )
```

- [ ] **Step 5：运行组合层与浏览器测试**

Run: `python -m unittest tests.batch_login.test_local_auth tests.batch_login.test_browser_contract -v`

Expected: PASS。

- [ ] **Step 6：提交本地认证后端**

```powershell
git add -- scripts/batch_login/local_auth.py tests/batch_login/test_local_auth.py
git commit -m "feat(batch-login): 组合本地企业与微软登录"
```

### Task 8：实现 RS 批量导入 SSE 客户端

**Files:**
- Create: `scripts/batch_login/rs_import.py`
- Create: `tests/batch_login/test_rs_import.py`

- [ ] **Step 1：写 SSE 分块、汇总与鉴权失败测试**

创建测试：

```python
class RsImportTests(unittest.IsolatedAsyncioTestCase):
    async def test_batch_import_parses_split_sse_events(self):
        body = (
            b'data: {"index":0,"status":"verified","credentialId":9}\n\n'
            b'data: {"status":"summary","summary":{"total":1,"imported":0,'
            b'"verified":1,"duplicate":0,"failed":0,"rolledBack":0}}\n\n'
        )

        async def handler(request):
            self.assertEqual("admin-key", request.headers["x-api-key"])
            self.assertEqual("/api/admin/credentials/batch-import", request.url.path)
            return httpx.Response(200, content=body)

        events = []
        async with RsImportClient(
            "https://rs.example",
            "admin-key",
            transport=httpx.MockTransport(handler),
        ) as client:
            summary = await client.batch_import(
                [{"email": "user@example.com", "refreshToken": "secret"}],
                events.append,
            )

        self.assertEqual("verified", events[0]["status"])
        self.assertEqual(1, summary["verified"])
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_rs_import -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：实现安全 URL、SSE 解析和流式导入**

创建 `scripts/batch_login/rs_import.py`：

```python
def parse_sse(buffer: str) -> tuple[list[dict[str, Any]], str]:
    events = []
    while "\n\n" in buffer:
        raw, buffer = buffer.split("\n\n", 1)
        data = next((line[5:].strip() for line in raw.splitlines() if line.startswith("data:")), "")
        if not data:
            continue
        try:
            value = json.loads(data)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            events.append(value)
    return events, buffer


class RsImportClient:
    def __init__(self, base_url, admin_key, *, transport=None, timeout=60):
        self.base_url = _normalize_base_url(base_url)
        self.client = httpx.AsyncClient(
            headers={"x-api-key": admin_key, "accept": "text/event-stream"},
            transport=transport,
            timeout=timeout,
        )

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_args):
        await self.aclose()

    async def aclose(self):
        await self.client.aclose()

    async def preflight(self):
        response = await self.client.get(self.base_url + "/credentials")
        response.raise_for_status()

    async def batch_import(self, credentials, on_event, *, verify=True, concurrency=8):
        summary = None
        buffer = ""
        async with self.client.stream(
            "POST",
            self.base_url + "/credentials/batch-import",
            json={"credentials": credentials, "verify": verify, "concurrency": concurrency},
        ) as response:
            response.raise_for_status()
            async for chunk in response.aiter_text():
                buffer += chunk.replace("\r\n", "\n")
                events, buffer = parse_sse(buffer)
                for event in events:
                    if event.get("status") == "summary":
                        summary = event.get("summary")
                    else:
                        if isinstance(event.get("error"), str):
                            event["error"] = redact_text(event["error"])
                        if isinstance(event.get("email"), str):
                            event["email"] = mask_account(event["email"])
                        on_event(event)
        if not isinstance(summary, dict):
            raise RsApiError("invalid_rs_response", "batch_import", False, 0, "导入响应缺少汇总")
        return summary
```

复用现有 `_normalize_base_url` 和 `RsApiError`。HTTP 错误映射时只保留状态码与稳定错误码，不把响应正文或请求 credentials 写入异常。

- [ ] **Step 4：运行 RS 导入测试**

Run: `python -m unittest tests.batch_login.test_rs_import tests.batch_login.test_rs_client -v`

Expected: PASS。

- [ ] **Step 5：提交导入客户端**

```powershell
git add -- scripts/batch_login/rs_import.py tests/batch_login/test_rs_import.py
git commit -m "feat(batch-login): 添加 RS 凭据流式导入"
```

### Task 9：实现自动 SSH 本地转发

**Files:**
- Create: `scripts/batch_login/ssh_tunnel.py`
- Create: `tests/batch_login/test_ssh_tunnel.py`

- [ ] **Step 1：写安全参数、无 shell 和清理测试**

创建测试：

```python
class FakeStream:
    async def read(self, _size):
        return b""


class FakeProcess:
    def __init__(self):
        self.returncode = None
        self.stderr = FakeStream()
        self.terminated = False
        self.killed = False

    def terminate(self):
        self.terminated = True
        self.returncode = 0

    def kill(self):
        self.killed = True
        self.returncode = -9

    async def wait(self):
        return self.returncode


class FakeFactory:
    def __init__(self, process):
        self.process = process
        self.calls = []

    async def __call__(self, *args, **kwargs):
        self.calls.append((args, kwargs))
        return self.process


async def always_ready(_port):
    return True


def valid_settings():
    return SshTunnelSettings(
        host="server.example",
        user="deploy",
        remote_port=8990,
        local_port=18080,
    )


class SshTunnelTests(unittest.IsolatedAsyncioTestCase):
    def test_build_command_uses_argument_list_and_safe_options(self):
        settings = SshTunnelSettings(
            host="server.example",
            user="deploy",
            ssh_port=22,
            remote_host="127.0.0.1",
            remote_port=8990,
            local_port=18080,
            identity_file=Path("C:/keys/rs key"),
        )
        command = build_ssh_command(settings, 18080)
        self.assertEqual("ssh", command[0])
        self.assertIn("ExitOnForwardFailure=yes", command)
        self.assertIn("StrictHostKeyChecking=accept-new", command)
        self.assertIn("BatchMode=yes", command)
        self.assertIn("127.0.0.1:18080:127.0.0.1:8990", command)
        self.assertNotIn("shell=True", str(command))

    async def test_stop_terminates_only_owned_process(self):
        process = FakeProcess()
        tunnel = SshTunnel(process_factory=FakeFactory(process), probe=always_ready)
        await tunnel.start(valid_settings())
        await tunnel.stop()
        self.assertTrue(process.terminated)
```

- [ ] **Step 2：运行测试并确认模块不存在**

Run: `python -m unittest tests.batch_login.test_ssh_tunnel -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：实现设置校验与命令构造**

```python
@dataclass(slots=True, frozen=True)
class SshTunnelSettings:
    host: str
    user: str
    ssh_port: int = 22
    remote_host: str = "127.0.0.1"
    remote_port: int = 8990
    local_port: int | None = None
    identity_file: Path | None = None


def build_ssh_command(settings: SshTunnelSettings, local_port: int) -> list[str]:
    if not settings.host.strip() or not settings.user.strip():
        raise ValueError("SSH 主机和用户不能为空")
    for port in (settings.ssh_port, settings.remote_port, local_port):
        if not 1 <= port <= 65535:
            raise ValueError("SSH 端口必须位于 1..65535")
    command = [
        "ssh", "-N",
        "-L", f"127.0.0.1:{local_port}:{settings.remote_host}:{settings.remote_port}",
        "-p", str(settings.ssh_port),
        "-o", "ExitOnForwardFailure=yes",
        "-o", "ServerAliveInterval=30",
        "-o", "StrictHostKeyChecking=accept-new",
        "-o", "BatchMode=yes",
    ]
    if settings.identity_file is not None:
        command.extend(["-i", str(settings.identity_file)])
    command.append(f"{settings.user}@{settings.host}")
    return command
```

- [ ] **Step 4：实现端口选择、启动探测和幂等清理**

实现：

```python
def choose_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


async def probe_port(port: int) -> bool:
    try:
        _reader, writer = await asyncio.wait_for(
            asyncio.open_connection("127.0.0.1", port),
            timeout=0.25,
        )
    except (OSError, TimeoutError):
        return False
    writer.close()
    await writer.wait_closed()
    return True


class SshTunnel:
    def __init__(self, *, process_factory=asyncio.create_subprocess_exec, port_factory=choose_port, probe=probe_port):
        self.process_factory = process_factory
        self.port_factory = port_factory
        self.probe = probe
        self.process = None
        self.local_port = None

    async def start(self, settings: SshTunnelSettings) -> str:
        if self.process is not None:
            raise SshTunnelError("SSH 隧道已经启动")
        for _attempt in range(3):
            port = settings.local_port or self.port_factory()
            command = build_ssh_command(settings, port)
            kwargs = {
                "stdout": asyncio.subprocess.PIPE,
                "stderr": asyncio.subprocess.PIPE,
            }
            if os.name == "nt":
                kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW
            process = await self.process_factory(*command, **kwargs)
            for _ in range(40):
                if process.returncode is not None:
                    stderr = await process.stderr.read(4096)
                    message = redact_text(stderr.decode("utf-8", errors="replace"))
                    raise SshTunnelError(message or "SSH 隧道启动失败")
                if await self.probe(port):
                    self.process = process
                    self.local_port = port
                    return f"http://127.0.0.1:{port}"
                await asyncio.sleep(0.1)
            process.terminate()
            await process.wait()
            if settings.local_port is not None:
                break
        raise SshTunnelError("SSH 隧道未在超时内就绪")

    async def stop(self) -> None:
        process, self.process = self.process, None
        self.local_port = None
        if process is None or process.returncode is not None:
            return
        process.terminate()
        try:
            await asyncio.wait_for(process.wait(), timeout=3)
        except TimeoutError:
            process.kill()
            await process.wait()
```

只能使用参数列表调用 `create_subprocess_exec`，不得使用 shell。只操作实例保存的 process。

- [ ] **Step 5：运行 SSH 测试**

Run: `python -m unittest tests.batch_login.test_ssh_tunnel -v`

Expected: PASS，包含自动端口重试、提前退出、超时 kill 和重复 stop。

- [ ] **Step 6：提交 SSH 隧道模块**

```powershell
git add -- scripts/batch_login/ssh_tunnel.py tests/batch_login/test_ssh_tunnel.py
git commit -m "feat(batch-login): 添加自动 SSH 隧道"
```

### Task 10：实现保存优先的本地批次编排

**Files:**
- Create: `scripts/batch_login/local_runner.py`
- Create: `tests/batch_login/test_local_runner.py`

- [ ] **Step 1：写“先保存后导入”和失败继续测试**

创建测试：

```python
def record():
    return CredentialRecord(
        email="admin-user",
        auth_method="idc",
        provider="Enterprise",
        refresh_token="refresh",
        access_token="access",
        client_id="client",
        client_secret="secret",
        start_url="https://example.awsapps.com/start",
        region="us-east-1",
    )


def settings_for(mode):
    return LocalRunSettings(
        mode=LoginMode.ENTERPRISE,
        region="us-east-1",
        start_url="https://example.awsapps.com/start",
        headless=True,
        timeout_seconds=10,
        mfa_timeout_seconds=10,
        result_mode=mode,
        credential_path=Path("unused-credentials.json"),
        checkpoint_path=Path("unused-checkpoint.jsonl"),
    )


class FakeAuth:
    def __init__(self, results):
        self.results = list(results)

    @classmethod
    def success(cls, value):
        return cls([value])

    @classmethod
    def sequence(cls, values):
        return cls(values)

    async def login(self, _entry, _settings):
        value = self.results.pop(0)
        if isinstance(value, Exception):
            raise value
        return value


class FakeStore:
    def __init__(self, calls=None):
        self.calls = calls if calls is not None else []

    def append(self, _record):
        self.calls.append("store.append")
        return True


class FakeCheckpoint:
    def should_run(self, **_kwargs):
        return True

    def append(self, _record):
        return None

    def append_import_result(self, *_args, **_kwargs):
        return None


class FakeImporter:
    def __init__(self, calls=None):
        self.calls = calls if calls is not None else []

    async def batch_import(self, _credentials, on_event):
        self.calls.append("import.start")
        on_event({"index": 0, "status": "verified", "credentialId": 1})
        return {"total": 1, "imported": 0, "verified": 1, "duplicate": 0, "failed": 0, "rolledBack": 0}


def runner_with(auth):
    return LocalBatchRunner(
        enterprise=auth,
        microsoft=auth,
        store=FakeStore(),
        checkpoint=FakeCheckpoint(),
        importer=None,
    )


class LocalRunnerTests(unittest.IsolatedAsyncioTestCase):
    async def test_credentials_are_saved_before_import_starts(self):
        calls = []
        store = FakeStore(calls)
        importer = FakeImporter(calls)
        runner = LocalBatchRunner(
            enterprise=FakeAuth.success(record()),
            microsoft=FakeAuth.success(record()),
            store=store,
            checkpoint=FakeCheckpoint(),
            importer=importer,
            emit=lambda event: calls.append(event.kind),
        )
        settings = settings_for(ResultMode.SAVE_AND_IMPORT)

        summary = await runner.run([AccountEntry(1, "admin-user", "password")], settings)

        self.assertLess(calls.index("store.append"), calls.index("import.start"))
        self.assertEqual(1, summary.succeeded)
        self.assertEqual(1, summary.imported)

    async def test_one_account_failure_does_not_stop_next_account(self):
        auth = FakeAuth.sequence([LocalAuthError("invalid_credentials", "browser", False, "登录失败"), record()])
        runner = runner_with(auth=auth)
        summary = await runner.run([
            AccountEntry(1, "bad-user", "bad"),
            AccountEntry(2, "good-user", "good"),
        ], settings_for(ResultMode.SAVE_ONLY))
        self.assertEqual(1, summary.failed)
        self.assertEqual(1, summary.succeeded)
```

- [ ] **Step 2：运行测试并确认 runner 不存在**

Run: `python -m unittest tests.batch_login.test_local_runner -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：实现逐账号认证、保存和 checkpoint**

创建 `LocalBatchRunner`：

```python
class LocalBatchRunner:
    def __init__(self, *, enterprise, microsoft, store, checkpoint, importer=None, emit=lambda _event: None):
        self.enterprise = enterprise
        self.microsoft = microsoft
        self.store = store
        self.checkpoint = checkpoint
        self.importer = importer
        self.emit = emit

    async def run(self, entries: list[AccountEntry], settings: LocalRunSettings) -> BatchSummary:
        summary = BatchSummary(total=len(entries))
        saved_this_run: list[tuple[AccountEntry, CredentialRecord, LocalRunRecord]] = []
        self.emit(WorkerEvent("batch_started", {"total": len(entries)}))
        try:
            for index, entry in enumerate(entries, start=1):
                scope = settings.start_url or ""
                if not self.checkpoint.should_run(
                    account=entry.account,
                    mode=settings.mode.value,
                    scope=scope,
                    resume=settings.resume,
                ):
                    continue
                self.emit(WorkerEvent("account_started", {
                    "index": index,
                    "total": len(entries),
                    "accountMasked": mask_account(entry.account),
                    "mode": settings.mode.value,
                }))
                try:
                    backend = self.enterprise if settings.mode is LoginMode.ENTERPRISE else self.microsoft
                    credential = await backend.login(entry, self._auth_settings(settings))
                    added = self.store.append(credential)
                    if added:
                        summary.succeeded += 1
                    else:
                        summary.duplicate += 1
                    success_record = self._success_record(entry, settings, added)
                    self.checkpoint.append(success_record)
                    if added:
                        saved_this_run.append((entry, credential, success_record))
                    self.emit(WorkerEvent("account_finished", {
                        "status": "success" if added else "duplicate_credential",
                        "credentialSaved": True,
                    }))
                except (LocalAuthError, BrowserFlowError) as error:
                    self._record_failure(summary, entry, settings, error)

            if settings.result_mode is ResultMode.SAVE_AND_IMPORT and saved_this_run:
                if self.importer is None:
                    raise RuntimeError("保存并导入模式缺少 RS 导入客户端")
                def on_import(event):
                    index = event.get("index")
                    if isinstance(index, int) and 0 <= index < len(saved_this_run):
                        previous = saved_this_run[index][2]
                        self.checkpoint.append_import_result(
                            previous,
                            import_status=str(event.get("status") or "failed"),
                            credential_id=event.get("credentialId"),
                            message=event.get("error"),
                        )
                    self.emit(WorkerEvent("import_event", event))
                import_summary = await self.importer.batch_import(
                    [item[1].as_add_request() for item in saved_this_run],
                    on_import,
                )
                summary.imported = int(import_summary.get("imported", 0)) + int(import_summary.get("verified", 0))
            self.emit(WorkerEvent("batch_finished", asdict(summary)))
            return summary
        except asyncio.CancelledError:
            summary.cancelled += 1
            self.emit(WorkerEvent("batch_cancelled", asdict(summary)))
            raise

    def _auth_settings(self, settings):
        if settings.mode is LoginMode.ENTERPRISE:
            return EnterpriseSettings(settings.start_url or "", settings.region)
        return MicrosoftSettings(settings.region)

    def _success_record(self, entry, settings, _added):
        return LocalRunRecord.success(
            run_id=self.run_id,
            line_number=entry.line_number,
            account=entry.account,
            mode=settings.mode.value,
            scope=settings.start_url or "microsoft",
            credential_saved=True,
        )

    def _record_failure(self, summary, entry, settings, error):
        manual = error.code in {"mfa_timeout", "captcha_required"}
        if manual:
            summary.manual_required += 1
            status = "manual_required"
        else:
            summary.failed += 1
            status = "failed"
        record = LocalRunRecord(
            run_id=self.run_id,
            line_number=entry.line_number,
            account_hash=account_hash(entry.account),
            account_masked=mask_account(entry.account),
            mode=settings.mode.value,
            scope=(settings.start_url or "microsoft").casefold().rstrip("/"),
            status=status,
            stage=error.stage,
            timestamp=datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            retryable=error.retryable,
            credential_saved=False,
            code=error.code,
            message=redact_text(str(error)),
        )
        self.checkpoint.append(record)
        self.emit(WorkerEvent("account_finished", {
            "status": status,
            "code": error.code,
            "credentialSaved": False,
        }))
```

构造器设置 `self.run_id = uuid4().hex`。`CredentialStoreError` 不进入单账号失败分支，必须作为批次级致命错误立即停止。Microsoft 在登录前无法获知 issuer，checkpoint 恢复作用域固定为 `microsoft`；凭据文件自身仍使用实际 issuer 做去重。

- [ ] **Step 4：增加取消、恢复和写盘致命错误测试**

测试断言：取消会重新抛出 `CancelledError` 并产生 `batch_cancelled`；恢复跳过已保存账号；`CredentialStoreError` 阻止后续账号且从未调用 importer；事件中没有原账号、密码和 token。

Run: `python -m unittest tests.batch_login.test_local_runner -v`

Expected: PASS。

- [ ] **Step 5：提交本地批次编排**

```powershell
git add -- scripts/batch_login/local_runner.py tests/batch_login/test_local_runner.py
git commit -m "feat(batch-login): 编排本地登录保存与导入"
```

### Task 11：实现无显示器可测的 GUI 控制器

**Files:**
- Create: `scripts/batch_login/gui_controller.py`
- Create: `scripts/batch_login/gui_runtime.py`
- Create: `tests/batch_login/test_gui_controller.py`

- [ ] **Step 1：写表单校验、线程启动和取消失败测试**

创建 `tests/batch_login/test_gui_controller.py`：

```python
def valid_form(**overrides):
    values = {
        "mode": LoginMode.ENTERPRISE,
        "start_url": "https://example.awsapps.com/start",
        "credential_path": "credentials.json",
        "checkpoint_path": "checkpoint.jsonl",
        "result_mode": ResultMode.SAVE_ONLY,
    }
    values.update(overrides)
    return GuiFormState(**values)


def valid_entries():
    return [AccountEntry(1, "admin-user", "password")]


class FakeRuntime:
    def __init__(self, block=False):
        self.block = block
        self.started = threading.Event()
        self.cancelled = threading.Event()
        self.closed = threading.Event()

    async def run(self, _entries):
        self.started.set()
        if self.block:
            try:
                await asyncio.Future()
            except asyncio.CancelledError:
                self.cancelled.set()
                raise

    async def import_existing(self):
        self.started.set()

    async def close(self):
        self.closed.set()


class GuiControllerTests(unittest.TestCase):
    def test_save_only_does_not_require_rs_or_ssh_fields(self):
        form = valid_form(result_mode=ResultMode.SAVE_ONLY)
        form.rs_url = ""
        form.admin_key = ""
        form.use_ssh = False
        self.assertEqual([], form.validate())

    def test_enterprise_requires_start_url(self):
        form = valid_form(mode=LoginMode.ENTERPRISE)
        form.start_url = ""
        self.assertIn("企业模式必须填写 Start URL", form.validate())

    def test_start_and_cancel_are_marshaled_to_worker_loop(self):
        runtime = FakeRuntime(block=True)
        controller = GuiController(runtime_factory=lambda _form, _emit: runtime)
        controller.start(valid_entries(), valid_form())
        runtime.started.wait(timeout=1)
        controller.cancel()
        runtime.cancelled.wait(timeout=1)
        self.assertTrue(runtime.cancelled.is_set())
```

- [ ] **Step 2：运行测试并确认控制器不存在**

Run: `python -m unittest tests.batch_login.test_gui_controller -v`

Expected: FAIL with `ModuleNotFoundError`。

- [ ] **Step 3：定义表单状态与完整校验**

创建 `scripts/batch_login/gui_controller.py`：

```python
@dataclass(slots=True)
class GuiFormState:
    mode: LoginMode = LoginMode.ENTERPRISE
    input_template: str = "login = {account} / onetime password = {password}"
    output_template: str = "{account}----{password}"
    start_url: str = ""
    region: str = "us-east-1"
    headless: bool = False
    timeout_seconds: float = 180
    mfa_timeout_seconds: float = 300
    result_mode: ResultMode = ResultMode.SAVE_ONLY
    input_path: str = ""
    credential_path: str = ""
    checkpoint_path: str = ""
    resume: bool = False
    rs_url: str = ""
    admin_key: str = ""
    use_ssh: bool = False
    ssh_host: str = ""
    ssh_user: str = ""
    ssh_port: int = 22
    identity_file: str = ""
    remote_host: str = "127.0.0.1"
    remote_port: int = 8990
    local_port: int | None = None

    def validate(self) -> list[str]:
        errors = []
        try:
            compile_format(self.input_template)
            _validate_placeholders(self.output_template)
        except ValueError as error:
            errors.append(str(error))
        if self.mode is LoginMode.ENTERPRISE and not self.start_url.strip():
            errors.append("企业模式必须填写 Start URL")
        if not self.region.strip():
            errors.append("Region 不能为空")
        if self.timeout_seconds <= 0 or self.mfa_timeout_seconds <= 0:
            errors.append("超时必须大于 0")
        if not self.credential_path.strip():
            errors.append("必须选择完整凭据 JSON 路径")
        if self.input_path and Path(self.input_path).resolve() == Path(self.credential_path).resolve():
            errors.append("完整凭据 JSON 不能覆盖账号输入文件")
        if self.result_mode is ResultMode.SAVE_AND_IMPORT:
            if not self.admin_key.strip():
                errors.append("导入 RS 必须填写 Admin Key")
            if self.use_ssh:
                if not self.ssh_host.strip() or not self.ssh_user.strip():
                    errors.append("SSH 模式必须填写主机和用户")
            elif not self.rs_url.strip():
                errors.append("直接连接必须填写 RS URL")
            else:
                try:
                    parts = urlsplit(self.rs_url.strip())
                    _ = parts.port
                except ValueError:
                    errors.append("RS URL 无效")
                else:
                    if parts.scheme not in {"http", "https"} or not parts.hostname:
                        errors.append("RS URL 必须是 HTTP(S) 地址")
                    if parts.scheme == "http" and parts.hostname not in {"127.0.0.1", "::1", "localhost"}:
                        errors.append("远程 RS 必须使用 HTTPS")
        return errors

    def to_run_settings(self) -> LocalRunSettings:
        checkpoint = self.checkpoint_path.strip() or (self.credential_path.strip() + ".checkpoint.jsonl")
        return LocalRunSettings(
            mode=self.mode,
            region=self.region.strip(),
            start_url=self.start_url.strip() or None,
            headless=self.headless,
            timeout_seconds=self.timeout_seconds,
            mfa_timeout_seconds=self.mfa_timeout_seconds,
            result_mode=self.result_mode,
            credential_path=Path(self.credential_path),
            checkpoint_path=Path(checkpoint),
            resume=self.resume,
        )
```

- [ ] **Step 4：实现后台线程、事件队列和线程安全取消**

`GuiController` 保存 `queue.Queue[WorkerEvent]`、后台线程、loop 和 task：

```python
class GuiController:
    def __init__(self, runtime_factory):
        self.runtime_factory = runtime_factory
        self.events = queue.Queue()
        self.thread = None
        self.loop = None
        self.task = None

    def start(self, entries, form: GuiFormState):
        errors = form.validate()
        if errors:
            raise ValueError("\n".join(errors))
        if self.thread is not None and self.thread.is_alive():
            raise RuntimeError("已有任务正在运行")
        self.thread = threading.Thread(
            target=self._thread_main,
            args=("run", entries, form),
            daemon=False,
            name="kiro-batch-login-worker",
        )
        self.thread.start()

    def import_existing(self, form: GuiFormState):
        errors = form.validate()
        if errors:
            raise ValueError("\n".join(errors))
        if form.result_mode is not ResultMode.SAVE_AND_IMPORT:
            raise ValueError("导入已有 JSON 必须选择保存并导入 RS")
        if self.thread is not None and self.thread.is_alive():
            raise RuntimeError("已有任务正在运行")
        self.thread = threading.Thread(
            target=self._thread_main,
            args=("import", [], form),
            daemon=False,
            name="kiro-batch-import-worker",
        )
        self.thread.start()

    def _thread_main(self, action, entries, form):
        loop = asyncio.new_event_loop()
        self.loop = loop
        asyncio.set_event_loop(loop)
        runtime = self.runtime_factory(form, self.events.put)
        coroutine = runtime.run(entries) if action == "run" else runtime.import_existing()
        self.task = loop.create_task(coroutine)
        try:
            loop.run_until_complete(self.task)
        except asyncio.CancelledError:
            pass
        except Exception as error:
            self.events.put(WorkerEvent("fatal_error", {
                "code": "runtime_failed",
                "message": redact_text(str(error)),
            }))
        finally:
            loop.run_until_complete(runtime.close())
            loop.close()
            self.task = None
            self.loop = None

    def cancel(self):
        if self.loop is not None and self.task is not None:
            self.loop.call_soon_threadsafe(self.task.cancel)

    def drain_events(self) -> list[WorkerEvent]:
        items = []
        while True:
            try:
                items.append(self.events.get_nowait())
            except queue.Empty:
                return items
```

runtime factory 负责创建 Playwright、浏览器、HTTP 客户端、CredentialStore、checkpoint、可选 SSH 和 importer，并在 `close()` 中按逆序释放。`import_existing(form)` 使用同一工作线程边界：读取 `CredentialStore.load()` 后直接调用 importer，不启动浏览器。

在 `scripts/batch_login/gui_runtime.py` 实现默认依赖装配：

```python
class GuiRuntime:
    def __init__(self, form: GuiFormState, emit):
        self.form = form
        self.emit = emit
        self.http = None
        self.playwright = None
        self.browser = None
        self.importer = None
        self.tunnel = None

    async def _connect_importer(self) -> RsImportClient | None:
        if self.form.result_mode is ResultMode.SAVE_ONLY:
            return None
        base_url = self.form.rs_url
        if self.form.use_ssh:
            self.tunnel = SshTunnel()
            base_url = await self.tunnel.start(SshTunnelSettings(
                host=self.form.ssh_host,
                user=self.form.ssh_user,
                ssh_port=self.form.ssh_port,
                remote_host=self.form.remote_host,
                remote_port=self.form.remote_port,
                local_port=self.form.local_port,
                identity_file=Path(self.form.identity_file) if self.form.identity_file else None,
            ))
        self.importer = RsImportClient(base_url, self.form.admin_key)
        await self.importer.preflight()
        return self.importer

    async def run(self, entries: list[AccountEntry]):
        store = CredentialStore(
            Path(self.form.credential_path),
            warning_sink=lambda message: self.emit(WorkerEvent("security_warning", {"message": message})),
        )
        checkpoint = LocalCheckpointStore(Path(self.form.checkpoint_path))
        importer = await self._connect_importer()
        self.http = httpx.AsyncClient(timeout=30)
        self.playwright = await async_playwright().start()
        self.browser = await self.playwright.chromium.launch(headless=self.form.headless)
        browser_flows = BrowserFlows(
            self.browser,
            timeout_seconds=self.form.timeout_seconds,
            mfa_timeout_seconds=self.form.mfa_timeout_seconds,
            event_sink=lambda event: self.emit(WorkerEvent(event.pop("kind"), event)),
        )
        idc = LocalIdcClient(self.http)
        microsoft = MicrosoftProtocol(self.http)
        runner = LocalBatchRunner(
            enterprise=LocalEnterpriseAuth(idc, browser_flows),
            microsoft=LocalMicrosoftAuth(microsoft, browser_flows),
            store=store,
            checkpoint=checkpoint,
            importer=importer,
            emit=self.emit,
        )
        return await runner.run(entries, self.form.to_run_settings())

    async def import_existing(self):
        records = CredentialStore(Path(self.form.credential_path)).load()
        if not records:
            raise ValueError("完整凭据 JSON 中没有可导入账号")
        importer = await self._connect_importer()
        if importer is None:
            raise ValueError("导入已有 JSON 必须选择 RS 导入模式")
        self.emit(WorkerEvent("batch_started", {"total": len(records), "importOnly": True}))
        summary = await importer.batch_import(
            [record.as_add_request() for record in records],
            lambda event: self.emit(WorkerEvent("import_event", event)),
        )
        self.emit(WorkerEvent("batch_finished", {"importOnly": True, **summary}))
        return summary

    async def close(self):
        if self.browser is not None:
            await self.browser.close()
            self.browser = None
        if self.playwright is not None:
            await self.playwright.stop()
            self.playwright = None
        if self.http is not None:
            await self.http.aclose()
            self.http = None
        if self.importer is not None:
            await self.importer.aclose()
            self.importer = None
        if self.tunnel is not None:
            await self.tunnel.stop()
            self.tunnel = None


def build_default_controller() -> GuiController:
    return GuiController(runtime_factory=GuiRuntime)
```

`GuiFormState.to_run_settings()` 构造 Task 3 的 `LocalRunSettings`。`close()` 即使某一步失败也必须通过独立 `try/finally` 继续释放后续资源；测试使用 fake 断言释放顺序为 browser → playwright → HTTP → importer → SSH。

- [ ] **Step 5：运行控制器测试**

Run: `python -m unittest tests.batch_login.test_gui_controller -v`

Expected: PASS，包含重复启动、fatal_error 脱敏、已有 JSON 导入不创建浏览器、关闭时 runtime.close 测试。

- [ ] **Step 6：提交 GUI 控制器**

```powershell
git add -- scripts/batch_login/gui_controller.py scripts/batch_login/gui_runtime.py tests/batch_login/test_gui_controller.py
git commit -m "feat(batch-login): 添加桌面任务控制器"
```

### Task 12：实现 Tkinter 界面与独立入口

**Files:**
- Create: `scripts/batch_login/gui_app.py`
- Create: `scripts/kiro_batch_login_gui.py`
- Create: `tests/batch_login/test_gui_entrypoint.py`

- [ ] **Step 1：写入口检查模式与 GUI 模块导入测试**

创建 `tests/batch_login/test_gui_entrypoint.py`：

```python
class GuiEntrypointTests(unittest.TestCase):
    def test_check_mode_does_not_create_tk_window(self):
        module = importlib.import_module("kiro_batch_login_gui")
        result = module.main(["--check"], checker=lambda: [])
        self.assertEqual(0, result)

    def test_check_mode_reports_missing_dependencies(self):
        module = importlib.import_module("kiro_batch_login_gui")
        result = module.main(["--check"], checker=lambda: ["当前 Python 未安装 Tkinter"])
        self.assertEqual(1, result)

    def test_check_mode_treats_missing_ssh_as_non_fatal(self):
        module = importlib.import_module("kiro_batch_login_gui")
        result = module.main(["--check"], checker=lambda: ["未找到系统 OpenSSH；SSH 模式不可用"])
        self.assertEqual(0, result)

    def test_gui_app_import_has_no_window_side_effect(self):
        module = importlib.import_module("batch_login.gui_app")
        self.assertTrue(hasattr(module, "BatchLoginApp"))
```

- [ ] **Step 2：运行测试并确认入口不存在**

Run: `python -m unittest tests.batch_login.test_gui_entrypoint -v`

Expected: FAIL with `ModuleNotFoundError: kiro_batch_login_gui`。

- [ ] **Step 3：建立 Tkinter 单窗口布局**

`scripts/batch_login/gui_app.py` 使用 `ttk.PanedWindow` 和 `ttk.LabelFrame`，公开：

```python
class BatchLoginApp:
    POLL_MS = 100

    def __init__(self, root: tk.Tk, controller: GuiController, *, ssh_available: bool = True):
        self.root = root
        self.controller = controller
        self.ssh_available = ssh_available
        self.form = GuiFormState(
            admin_key=os.environ.get("KIRO_RS_ADMIN_KEY", ""),
        )
        self.entries = []
        self._build_variables()
        self._build_layout()
        self._apply_mode_visibility()
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)
        self.root.after(self.POLL_MS, self._poll_events)

    def _build_layout(self):
        self.root.title("Kiro 批量登录助手")
        self.root.minsize(1050, 720)
        outer = ttk.Frame(self.root, padding=12)
        outer.pack(fill="both", expand=True)
        self._build_rule_bar(outer)
        self._build_input_preview(outer)
        self._build_login_settings(outer)
        self._build_rs_settings(outer)
        self._build_log_progress(outer)
        self._build_actions(outer)
```

各构建函数必须使用以下控件语义：

- 规则栏：input template `Combobox`、output template `Entry`、打开文件、转换预览。
- 双栏：左侧 `Text` 原文；右侧 `Treeview` 列 `line/account/password/status/reason`；显示密码 `Checkbutton`；复制/保存按钮。
- 登录区：企业/Microsoft `Radiobutton`；Start URL、Region、headless、结果模式、凭据 JSON、checkpoint、resume。
- RS 区：直接/SSH `Radiobutton`；RS URL、Admin Key 遮罩；SSH host/user/port/key/remote/local 字段；只在保存并导入时启用。
- 日志区：只读 `Text`、`Progressbar`、当前状态 Label。
- 操作区：导入已有 JSON、停止、开始批量登录。

按以下方式创建核心控件并保存引用，其他表单字段使用同样的 `Label + Entry/Combobox` 网格模式：

```python
def _build_variables(self):
    self.input_template_var = tk.StringVar(value=self.form.input_template)
    self.output_template_var = tk.StringVar(value=self.form.output_template)
    self.mode_var = tk.StringVar(value=self.form.mode.value)
    self.start_url_var = tk.StringVar()
    self.region_var = tk.StringVar(value="us-east-1")
    self.headless_var = tk.BooleanVar(value=False)
    self.timeout_var = tk.DoubleVar(value=180)
    self.mfa_timeout_var = tk.DoubleVar(value=300)
    self.result_mode_var = tk.StringVar(value=ResultMode.SAVE_ONLY.value)
    self.credential_path_var = tk.StringVar()
    self.checkpoint_path_var = tk.StringVar()
    self.resume_var = tk.BooleanVar(value=False)
    self.rs_url_var = tk.StringVar()
    self.admin_key_var = tk.StringVar(value=self.form.admin_key)
    self.use_ssh_var = tk.BooleanVar(value=False)
    self.ssh_host_var = tk.StringVar()
    self.ssh_user_var = tk.StringVar()
    self.ssh_port_var = tk.IntVar(value=22)
    self.identity_file_var = tk.StringVar()
    self.remote_host_var = tk.StringVar(value="127.0.0.1")
    self.remote_port_var = tk.IntVar(value=8990)
    self.local_port_var = tk.StringVar()
    self.status_var = tk.StringVar(value="准备就绪")
    self.progress_var = tk.DoubleVar(value=0)
    self.show_password_var = tk.BooleanVar(value=False)
    self.last_result = ParseResult([], [])


def _build_rule_bar(self, parent):
    frame = ttk.LabelFrame(parent, text="解析规则", padding=8)
    frame.pack(fill="x", pady=(0, 8))
    ttk.Combobox(
        frame,
        textvariable=self.input_template_var,
        values=["login = {account} / onetime password = {password}", "{account}----{password}"],
    ).grid(row=0, column=0, sticky="ew", padx=(0, 6))
    ttk.Entry(frame, textvariable=self.output_template_var, width=28).grid(row=0, column=1, sticky="ew", padx=6)
    ttk.Button(frame, text="打开文件", command=self._open_input_file).grid(row=0, column=2, padx=6)
    ttk.Button(frame, text="转换并预览", command=self._convert_preview).grid(row=0, column=3)
    frame.columnconfigure(0, weight=3)
    frame.columnconfigure(1, weight=2)


def _build_input_preview(self, parent):
    pane = ttk.PanedWindow(parent, orient="horizontal")
    pane.pack(fill="both", expand=True, pady=(0, 8))
    left = ttk.LabelFrame(pane, text="原始账号文本", padding=6)
    right = ttk.LabelFrame(pane, text="转换预览", padding=6)
    pane.add(left, weight=1)
    pane.add(right, weight=1)
    self.input_text = tk.Text(left, wrap="none", undo=True, height=14)
    self.input_text.pack(fill="both", expand=True)
    columns = ("line", "account", "password", "status", "reason")
    self.preview = ttk.Treeview(right, columns=columns, show="headings", height=12)
    for name, title, width in (
        ("line", "行", 48), ("account", "账号", 180), ("password", "密码", 150),
        ("status", "状态", 85), ("reason", "原因", 180),
    ):
        self.preview.heading(name, text=title)
        self.preview.column(name, width=width, anchor="w")
    self.preview.pack(fill="both", expand=True)
    actions = ttk.Frame(right)
    actions.pack(fill="x", pady=(6, 0))
    ttk.Checkbutton(actions, text="显示密码", variable=self.show_password_var, command=self._render_last_preview).pack(side="left")
    ttk.Button(actions, text="复制统一格式", command=self._copy_output).pack(side="right")
    ttk.Button(actions, text="保存账号 TXT", command=self._save_output).pack(side="right", padx=6)


def _entry_row(self, frame, row, label, variable, *, show=None):
    ttk.Label(frame, text=label).grid(row=row, column=0, sticky="w", padx=(0, 6), pady=3)
    entry = ttk.Entry(frame, textvariable=variable, show=show)
    entry.grid(row=row, column=1, sticky="ew", pady=3)
    frame.columnconfigure(1, weight=1)
    self.run_sensitive.append(entry)
    return entry


def _build_login_settings(self, parent):
    frame = ttk.LabelFrame(parent, text="登录与结果", padding=8)
    frame.pack(fill="x", pady=(0, 8))
    modes = ttk.Frame(frame)
    modes.grid(row=0, column=0, columnspan=4, sticky="w")
    for text, value in (("企业账号", "enterprise"), ("Microsoft", "microsoft")):
        button = ttk.Radiobutton(modes, text=text, value=value, variable=self.mode_var, command=self._apply_mode_visibility)
        button.pack(side="left", padx=(0, 8))
        self.run_sensitive.append(button)
    self.start_url_entry = self._entry_row(frame, 1, "Start URL", self.start_url_var)
    self._entry_row(frame, 2, "Region", self.region_var)
    self._entry_row(frame, 3, "完整凭据 JSON", self.credential_path_var)
    self._entry_row(frame, 4, "Checkpoint", self.checkpoint_path_var)
    ttk.Checkbutton(frame, text="无头浏览器", variable=self.headless_var).grid(row=1, column=2, sticky="w", padx=12)
    ttk.Checkbutton(frame, text="恢复运行", variable=self.resume_var).grid(row=2, column=2, sticky="w", padx=12)
    ttk.Label(frame, text="结果方式").grid(row=3, column=2, sticky="w", padx=12)
    result = ttk.Combobox(
        frame,
        state="readonly",
        textvariable=self.result_mode_var,
        values=[ResultMode.SAVE_ONLY.value, ResultMode.SAVE_AND_IMPORT.value],
    )
    result.grid(row=3, column=3, sticky="ew")
    result.bind("<<ComboboxSelected>>", lambda _event: self._apply_mode_visibility())
    self.run_sensitive.append(result)


def _build_rs_settings(self, parent):
    self.rs_frame = ttk.LabelFrame(parent, text="RS 连接", padding=8)
    self.rs_frame.pack(fill="x", pady=(0, 8))
    ssh_toggle = ttk.Checkbutton(
        self.rs_frame,
        text="使用 SSH 隧道",
        variable=self.use_ssh_var,
        command=self._apply_mode_visibility,
    )
    ssh_toggle.grid(row=0, column=0, columnspan=2, sticky="w")
    if not self.ssh_available:
        ssh_toggle.configure(state="disabled")
        self.use_ssh_var.set(False)
    self.direct_widgets = [
        self._entry_row(self.rs_frame, 1, "RS URL", self.rs_url_var),
    ]
    self._entry_row(self.rs_frame, 2, "Admin Key", self.admin_key_var, show="•")
    self.ssh_widgets = [
        self._entry_row(self.rs_frame, 3, "SSH 主机", self.ssh_host_var),
        self._entry_row(self.rs_frame, 4, "SSH 用户", self.ssh_user_var),
        self._entry_row(self.rs_frame, 5, "SSH 端口", self.ssh_port_var),
        self._entry_row(self.rs_frame, 6, "私钥路径", self.identity_file_var),
        self._entry_row(self.rs_frame, 7, "远端主机", self.remote_host_var),
        self._entry_row(self.rs_frame, 8, "远端端口", self.remote_port_var),
        self._entry_row(self.rs_frame, 9, "本地端口", self.local_port_var),
    ]


def _apply_mode_visibility(self):
    if self.mode_var.get() == LoginMode.ENTERPRISE.value:
        self.start_url_entry.grid()
    else:
        self.start_url_entry.grid_remove()
    if self.result_mode_var.get() == ResultMode.SAVE_ONLY.value:
        self.rs_frame.pack_forget()
    else:
        self.rs_frame.pack(fill="x", pady=(0, 8), before=self.log_text.master)
    for widget in self.direct_widgets:
        widget.grid_remove() if self.use_ssh_var.get() else widget.grid()
    for widget in self.ssh_widgets:
        widget.grid() if self.use_ssh_var.get() else widget.grid_remove()


def _build_log_progress(self, parent):
    frame = ttk.LabelFrame(parent, text="运行日志", padding=6)
    frame.pack(fill="both", pady=(0, 8))
    self.log_text = tk.Text(frame, height=7, state="disabled", wrap="word")
    self.log_text.pack(fill="both", expand=True)
    ttk.Progressbar(frame, variable=self.progress_var, maximum=100).pack(fill="x", pady=(6, 0))
    ttk.Label(frame, textvariable=self.status_var).pack(anchor="w", pady=(4, 0))


def _build_actions(self, parent):
    frame = ttk.Frame(parent)
    frame.pack(fill="x")
    import_button = ttk.Button(frame, text="导入已有 JSON", command=self._import_existing)
    import_button.pack(side="left")
    self.stop_button = ttk.Button(frame, text="停止", command=self.controller.cancel, state="disabled")
    self.stop_button.pack(side="right")
    start_button = ttk.Button(frame, text="开始批量登录", command=self._start)
    start_button.pack(side="right", padx=8)
    self.run_sensitive.extend([import_button, start_button])
```

在 `_build_layout` 开头初始化 `self.run_sensitive = []`。登录区和 RS 区的每个输入框保存到 `self.field_widgets`，并加入 `run_sensitive`；`_apply_mode_visibility` 根据 mode/result/use_ssh 调用 `grid()` 或 `grid_remove()`；`_set_running(True)` 禁用所有会改变语义的控件，只保留日志复制、显示密码和停止按钮。

文件、预览和表单方法完整边界：

```python
def _open_input_file(self):
    selected = filedialog.askopenfilename(
        title="打开账号文本",
        filetypes=[("文本文件", "*.txt"), ("所有文件", "*.*")],
        parent=self.root,
    )
    if not selected:
        return
    try:
        text = Path(selected).read_text(encoding="utf-8-sig")
    except OSError as error:
        messagebox.showerror("读取失败", redact_text(str(error)), parent=self.root)
        return
    self.input_path = selected
    self.input_text.delete("1.0", "end")
    self.input_text.insert("1.0", text)


def _render_preview(self, result: ParseResult):
    self.preview.delete(*self.preview.get_children())
    issues = {issue.line_number: issue for issue in result.issues}
    by_line = {entry.line_number: entry for entry in result.entries}
    for line_number in sorted(set(issues) | set(by_line)):
        entry = by_line.get(line_number)
        issue = issues.get(line_number)
        password = ""
        if entry is not None:
            password = entry.password if self.show_password_var.get() else "•" * min(max(len(entry.password), 6), 16)
        self.preview.insert("", "end", values=(
            line_number,
            entry.account if entry else "",
            password,
            "有效" if entry else issue.code,
            "" if issue is None else issue.message,
        ))


def _copy_output(self):
    text = render_accounts(self.entries, self.output_template_var.get())
    self.root.clipboard_clear()
    self.root.clipboard_append(text)


def _save_output(self):
    selected = filedialog.asksaveasfilename(
        title="保存统一账号文本",
        defaultextension=".txt",
        filetypes=[("文本文件", "*.txt")],
        parent=self.root,
    )
    if selected:
        Path(selected).write_text(
            render_accounts(self.entries, self.output_template_var.get()) + "\n",
            encoding="utf-8",
            newline="\n",
        )


def _collect_form(self) -> GuiFormState:
    local_port = self.local_port_var.get().strip()
    return GuiFormState(
        mode=LoginMode(self.mode_var.get()),
        input_template=self.input_template_var.get(),
        output_template=self.output_template_var.get(),
        start_url=self.start_url_var.get(),
        region=self.region_var.get(),
        headless=self.headless_var.get(),
        timeout_seconds=self.timeout_var.get(),
        mfa_timeout_seconds=self.mfa_timeout_var.get(),
        result_mode=ResultMode(self.result_mode_var.get()),
        input_path=getattr(self, "input_path", ""),
        credential_path=self.credential_path_var.get(),
        checkpoint_path=self.checkpoint_path_var.get(),
        resume=self.resume_var.get(),
        rs_url=self.rs_url_var.get(),
        admin_key=self.admin_key_var.get(),
        use_ssh=self.use_ssh_var.get(),
        ssh_host=self.ssh_host_var.get(),
        ssh_user=self.ssh_user_var.get(),
        ssh_port=self.ssh_port_var.get(),
        identity_file=self.identity_file_var.get(),
        remote_host=self.remote_host_var.get(),
        remote_port=self.remote_port_var.get(),
        local_port=int(local_port) if local_port else None,
    )
```

- [ ] **Step 4：实现转换预览、启动、停止和事件消费**

关键方法使用以下边界：

```python
def _convert_preview(self):
    try:
        result = parse_accounts(
            self.input_text.get("1.0", "end-1c"),
            self.input_template_var.get(),
            LoginMode(self.mode_var.get()),
        )
    except ValueError as error:
        messagebox.showerror("解析规则无效", str(error), parent=self.root)
        return
    self.last_result = result
    self.entries = result.entries
    self._render_preview(result)


def _render_last_preview(self):
    self._render_preview(self.last_result)


def _start(self):
    self._convert_preview()
    fatal = [issue for issue in self.last_result.issues if issue.code != "duplicate_input"]
    if fatal or not self.entries:
        messagebox.showerror("无法开始", "请先修正账号解析错误", parent=self.root)
        return
    credential_path = Path(self.credential_path_var.get())
    if credential_path.exists():
        choice = messagebox.askyesnocancel(
            "凭据文件已存在",
            "选择“是”将读取现有文件并去重追加；选择“否”可另存新文件。不会静默覆盖。",
            parent=self.root,
        )
        if choice is None:
            return
        if choice is False:
            selected = filedialog.asksaveasfilename(
                title="选择新的完整凭据 JSON",
                defaultextension=".json",
                filetypes=[("JSON", "*.json")],
                parent=self.root,
            )
            if not selected:
                return
            self.credential_path_var.set(selected)
    if not messagebox.askokcancel(
        "敏感文件提示",
        "完整凭据 JSON 将包含 access/refresh token。请勿上传、截图或提交 Git。",
        parent=self.root,
    ):
        return
    try:
        self.controller.start(self.entries, self._collect_form())
    except (ValueError, RuntimeError) as error:
        messagebox.showerror("无法开始", str(error), parent=self.root)
        return
    self._set_running(True)


def _import_existing(self):
    try:
        form = self._collect_form()
        self.controller.import_existing(form)
    except (ValueError, RuntimeError) as error:
        messagebox.showerror("无法导入", str(error), parent=self.root)
        return
    self._set_running(True)


def _poll_events(self):
    for event in self.controller.drain_events():
        self._handle_event(event)
    self.root.after(self.POLL_MS, self._poll_events)


def _handle_event(self, event: WorkerEvent):
    payload = event.payload
    if event.kind == "account_started":
        index, total = int(payload["index"]), int(payload["total"])
        self.progress_var.set(index * 100 / max(total, 1))
        self.status_var.set(f"正在处理 {payload['accountMasked']}（{index}/{total}）")
    elif event.kind == "manual_action_required":
        self.status_var.set(str(payload.get("message") or "等待人工验证"))
    elif event.kind == "security_warning":
        self._append_log(str(payload.get("message") or "敏感文件权限需要检查"))
    elif event.kind in {"account_finished", "import_event"}:
        self._append_log(json.dumps(payload, ensure_ascii=False))
    elif event.kind in {"batch_finished", "batch_cancelled"}:
        self._append_log(json.dumps(payload, ensure_ascii=False))
        self.status_var.set("任务完成" if event.kind == "batch_finished" else "任务已取消")
        self._set_running(False)
    elif event.kind == "fatal_error":
        self._append_log(str(payload.get("message") or "任务失败"))
        self.status_var.set("任务失败")
        self._set_running(False)


def _append_log(self, message: str):
    self.log_text.configure(state="normal")
    self.log_text.insert("end", redact_text(message) + "\n")
    self.log_text.see("end")
    self.log_text.configure(state="disabled")


def _set_running(self, running: bool):
    state = "disabled" if running else "normal"
    for widget in self.run_sensitive:
        widget.configure(state=state)
    self.stop_button.configure(state="normal" if running else "disabled")


def _on_close(self):
    if self.controller.thread is not None and self.controller.thread.is_alive():
        if not messagebox.askyesno("任务仍在运行", "停止任务并退出？", parent=self.root):
            return
        self.controller.cancel()
        self._wait_worker_then_close()
        return
    self.root.destroy()


def _wait_worker_then_close(self):
    thread = self.controller.thread
    if thread is not None and thread.is_alive():
        self.root.after(100, self._wait_worker_then_close)
    else:
        self.root.destroy()
```

`_render_preview` 永远默认显示 `•` 遮罩；显示密码开关只影响当前 Treeview 文本。`_handle_event` 仅处理结构化 payload，不打印未知对象。`fatal_error`、`batch_finished` 和 `batch_cancelled` 恢复控件可用状态。

关闭窗口时：若运行中先弹确认；确认后调用 `controller.cancel()`，轮询线程结束再 `destroy()`；不得直接强杀后台线程。

- [ ] **Step 5：实现薄入口与依赖检查**

创建 `scripts/kiro_batch_login_gui.py`：

```python
def dependency_errors() -> list[str]:
    errors = []
    try:
        import tkinter
    except ImportError:
        errors.append("当前 Python 未安装 Tkinter")
    try:
        import httpx
        import playwright
    except ImportError:
        errors.append("请安装 scripts/requirements-batch-login.txt")
    if shutil.which("ssh") is None:
        errors.append("未找到系统 OpenSSH；仅保存 JSON 和直接 RS 模式仍可使用")
    return errors


def main(argv=None, *, checker=dependency_errors) -> int:
    parser = argparse.ArgumentParser(description="Kiro 批量登录桌面助手")
    parser.add_argument("--check", action="store_true", help="只检查运行依赖")
    args = parser.parse_args(argv)
    errors = checker()
    if args.check:
        for error in errors:
            print(error, file=sys.stderr)
        return 1 if any("Tkinter" in error or "requirements" in error for error in errors) else 0
    fatal = [error for error in errors if "Tkinter" in error or "requirements" in error]
    if fatal:
        raise SystemExit("；".join(fatal))
    root = tk.Tk()
    controller = build_default_controller()
    BatchLoginApp(root, controller, ssh_available=shutil.which("ssh") is not None)
    root.mainloop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

OpenSSH 缺失只禁用 SSH 控件，不阻止仅保存或直接连接。

- [ ] **Step 6：运行入口和控制器测试**

Run: `python -m unittest tests.batch_login.test_gui_entrypoint tests.batch_login.test_gui_controller -v`

Expected: PASS。

Run: `python scripts/kiro_batch_login_gui.py --check`

Expected: 当前机器退出码 0；若 OpenSSH 缺失只显示非致命提示。

- [ ] **Step 7：提交 Tkinter 界面和入口**

```powershell
git add -- scripts/batch_login/gui_app.py scripts/kiro_batch_login_gui.py tests/batch_login/test_gui_entrypoint.py
git commit -m "feat(batch-login): 交付桌面批量登录界面"
```

### Task 13：补充文档、回归和安全验收

**Files:**
- Modify: `README.md:740-830`
- Verify: `scripts/requirements-batch-login.txt`
- Verify: all files under `scripts/batch_login/` and `tests/batch_login/`

- [ ] **Step 1：更新 README 的 GUI 安装与启动说明**

在现有批量登录章节前增加：

```markdown
### 桌面批量登录助手

```powershell
python -m pip install -r scripts/requirements-batch-login.txt
python -m playwright install chromium
python scripts/kiro_batch_login_gui.py --check
python scripts/kiro_batch_login_gui.py
```

默认输入模板支持：

```text
login = {account} / onetime password = {password}
```

“仅保存完整 JSON”完全不连接 RS。“保存并导入 RS”会先原子保存完整凭据文件，再通过直接 URL 或 GUI 管理的 SSH 隧道调用 RS。完整凭据 JSON 含 refresh/access token，属于高敏感文件；不要发送、截图、提交 Git，也不要与账号密码文件使用同一路径。
```

补充 SSH 字段示例、`KIRO_RS_ADMIN_KEY` 环境变量、直接 HTTPS/loopback HTTP 规则、“导入已有 JSON”用法、MFA 人工接管、取消恢复和凭据文件权限说明。

- [ ] **Step 2：运行 Python 语法与完整测试套件**

Run: `python -m compileall -q scripts/batch_login scripts/kiro_batch_login.py scripts/kiro_batch_login_gui.py`

Expected: exit 0，无语法错误。

Run: `python -m unittest discover -s tests/batch_login -v`

Expected: PASS；真实 Playwright 合约测试若 Chromium 未安装，应先运行 `python -m playwright install chromium`，不得用跳过掩盖失败。

- [ ] **Step 3：运行现有 CLI 回归**

Run: `python scripts/kiro_batch_login.py --help`

Expected: exit 0，仍显示 `enterprise` 和 `microsoft`。

Run: `python -m unittest tests.batch_login.test_cli tests.batch_login.test_runner tests.batch_login.test_rs_client tests.batch_login.test_checkpoint -v`

Expected: PASS，现有 RS 绑定 CLI 行为未改变。

- [ ] **Step 4：执行敏感信息静态扫描**

Run:

```powershell
rg -n "print\(.*(password|token|admin_key)|logger\..*(password|token|admin_key)|message=.*(callback_url|device_code|client_secret)" scripts/batch_login scripts/kiro_batch_login_gui.py
```

Expected: 无把敏感值传入输出的匹配；常量字段名或脱敏测试匹配需逐条人工确认。

Run:

```powershell
rg -n "accounts\.txt|csk_|refresh-secret|access-secret|one-time-password" --glob '!tests/**' --glob '!docs/**' .
```

Expected: 不出现真实凭据或本地 `accounts.txt` 引用；示例常量只允许出现在测试与文档。

- [ ] **Step 5：执行本机 GUI 烟雾测试**

1. 运行 `python scripts/kiro_batch_login_gui.py`。
2. 粘贴 `login = demo-user01 / onetime password = Sample-Password-01` 和 `login = demo-user02 / onetime password = Sample-Password-02`，确认转换预览与密码遮罩。
3. 切换企业/Microsoft、仅保存/导入、直接/SSH，确认相关字段启用状态。
4. 选择不存在的结果目录，确认启动前给出可执行错误。
5. 使用测试替身或用户提供的非生产测试账号执行一次可见浏览器登录；确认成功后完整 JSON 存在且 checkpoint 无 token。
6. 在运行中点击停止；确认浏览器、HTTP 流和 GUI 启动的 SSH 进程关闭，已保存 JSON 未损坏。

- [ ] **Step 6：检查差异并提交最终文档**

```powershell
git status --short
git diff --check
git diff --stat
git add -- README.md scripts/requirements-batch-login.txt
git diff --cached --check
git commit -m "docs(batch-login): 补充桌面助手使用说明"
```

仅在 `scripts/requirements-batch-login.txt` 实际发生必要变化时暂存该文件。不得暂存 `accounts.txt`、凭据 JSON、checkpoint、浏览器缓存或其他用户文件。

- [ ] **Step 7：最终全量验证与交付检查点**

Run:

```powershell
python -m compileall -q scripts/batch_login scripts/kiro_batch_login.py scripts/kiro_batch_login_gui.py
python -m unittest discover -s tests/batch_login -v
python scripts/kiro_batch_login_gui.py --check
git status --short
```

Expected: 编译、测试和依赖检查通过；`git status --short` 只允许显示用户原有且未纳入任务的文件，例如未跟踪的 `accounts.txt`。不执行 `git push`。
