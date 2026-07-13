# 严格 JSON 长上下文误判修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 阻止超长代码/历史上下文中的零散 `json`、`exactly one` 和 no-extra 文本误触发严格 JSON 恢复，同时保留真实 JSON-only 请求与 Ztest 探针兼容性。

**Architecture:** `strict_json_requested()` 只分析最新 user 文本尾部 4096 字节，并围绕每个 `json` 构造最大约 512 字节的 UTF-8 安全局部窗口。JSON 目标、输出命令、单值约束和 no-extra 约束必须在同一窗口共现；handlers 的工具/thinking/document guard、JSON 提取、字段验证和一次重试保持不变。

**Tech Stack:** Rust 2024、serde_json、现有 Anthropic converter/handler 单元测试、Dockerized Rust 1.92 Alpine 服务器验证

**Scope decision:** 本热修只修改请求检测器并增加回归，不在同一提交重构 JSON 提取结果枚举。规格中的详细失败原因诊断属于后续独立增强，避免扩大生产对话中断修复的风险面。

---

### Task 1: 在服务器准备无凭据的 Rust 测试工作区

**Files on server:**
- Create checkout: `/opt/kiro-rs-json-fix`

- [ ] **Step 1: Verify the scratch target is separate from production**

```powershell
$remote = @'
set -eu
docker inspect kiro-rs-admin --format 'production={{.Name}} image={{.Config.Image}} mounts={{range .Mounts}}{{.Source}}:{{.Destination}}{{end}}'
test ! -e /opt/kiro-rs-json-fix
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: production remains `/kiro-rs-admin` with `/opt/kiro-rs-admin/config:/app/config`; scratch path does not exist.

- [ ] **Step 2: Clone the exact master baseline**

```powershell
ssh -p 18792 root@43.225.196.10 "git clone --branch master --single-branch https://github.com/3370842391/kiro.rs-admin.git /opt/kiro-rs-json-fix"
```

Expected: checkout HEAD is `63c49359375227737b1d996a0b289425c67cc32a` before local test files are copied.

- [ ] **Step 3: Build the embedded admin UI once**

```powershell
$remote = @'
docker run --rm \
  -v /opt/kiro-rs-json-fix/admin-ui:/app \
  -w /app oven/bun:1-alpine \
  sh -lc 'bun install --frozen-lockfile && bun run build'
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: `/opt/kiro-rs-json-fix/admin-ui/dist/index.html` exists.

### Task 2: 用生产形状建立 RED 回归

**Files:**
- Modify: `src/anthropic/exact_output.rs`

- [ ] **Step 1: Add the long-context false-positive test**

在 `strict_json_requires_exact_and_no_extra_cues` 后新增：

```rust
#[test]
fn strict_json_ignores_distant_cues_in_large_code_context() {
    let noisy_code = r#"
        value, err := json.Marshal(payload)
        // Exactly one purchase should succeed.
        const historical_instruction: &str = "no markdown";
    "#
    .repeat(4_000);
    let prompt = format!(
        "{noisy_code}\n\nThe implementation is ready. Build the project and run the tests."
    );

    assert!(!strict_json_requested(&request(None, &prompt)));
}
```

- [ ] **Step 2: Copy only the changed Rust file to the server scratch checkout**

```powershell
scp -P 18792 src/anthropic/exact_output.rs root@43.225.196.10:/opt/kiro-rs-json-fix/src/anthropic/exact_output.rs
```

- [ ] **Step 3: Run RED on the server**

```powershell
$remote = @'
docker run --rm \
  -v /opt/kiro-rs-json-fix:/app \
  -v kiro-json-cargo-registry:/usr/local/cargo/registry \
  -v kiro-json-cargo-git:/usr/local/cargo/git \
  -v kiro-json-target:/app/target \
  -w /app rust:1.92-alpine \
  sh -lc 'apk add --no-cache musl-dev perl make >/dev/null && cargo test --no-default-features -j 8 strict_json_ignores_distant_cues_in_large_code_context'
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: FAIL because current whole-message `contains()` returns true.

### Task 3: 实现 UTF-8 安全尾窗口与局部共现检测

**Files:**
- Modify: `src/anthropic/exact_output.rs`

- [ ] **Step 1: Add constants and UTF-8 boundary helpers**

```rust
const STRICT_JSON_TAIL_BYTES: usize = 4 * 1024;
const STRICT_JSON_LOCAL_RADIUS_BYTES: usize = 256;

fn utf8_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn utf8_local_window(text: &str, offset: usize, needle_len: usize) -> &str {
    let mut start = offset.saturating_sub(STRICT_JSON_LOCAL_RADIUS_BYTES);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (offset + needle_len + STRICT_JSON_LOCAL_RADIUS_BYTES).min(text.len());
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[start..end]
}
```

- [ ] **Step 2: Add local cue helpers**

```rust
fn has_json_output_command_cue(text: &str) -> bool {
    [
        "return", "reply", "respond", "output", "provide",
        "只返回", "仅返回", "回复", "输出",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

fn has_single_json_value_cue(text: &str) -> bool {
    [
        "exactly one", "exactly a single", "single minified", "one minified",
        "只返回", "仅返回",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}
```

- [ ] **Step 3: Replace the global detector**

```rust
pub(crate) fn strict_json_requested(req: &MessagesRequest) -> bool {
    let latest_user_text = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message_text(&message.content))
        .unwrap_or_default();
    let normalized = utf8_tail(&latest_user_text, STRICT_JSON_TAIL_BYTES)
        .to_ascii_lowercase();

    normalized.match_indices("json").any(|(offset, cue)| {
        let window = utf8_local_window(&normalized, offset, cue.len());
        has_json_output_command_cue(window)
            && has_single_json_value_cue(window)
            && has_no_extra_cue(window)
    })
}
```

- [ ] **Step 4: Copy implementation and run GREEN on the server**

```powershell
scp -P 18792 src/anthropic/exact_output.rs root@43.225.196.10:/opt/kiro-rs-json-fix/src/anthropic/exact_output.rs
$remote = @'
docker run --rm \
  -v /opt/kiro-rs-json-fix:/app \
  -v kiro-json-cargo-registry:/usr/local/cargo/registry \
  -v kiro-json-cargo-git:/usr/local/cargo/git \
  -v kiro-json-target:/app/target \
  -w /app rust:1.92-alpine \
  sh -lc 'apk add --no-cache musl-dev perl make >/dev/null && cargo test --no-default-features -j 8 strict_json_ignores_distant_cues_in_large_code_context'
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: PASS.

### Task 4: 增加兼容边界回归

**Files:**
- Modify: `src/anthropic/exact_output.rs`

- [ ] **Step 1: Add explicit compatibility tests**

```rust
#[test]
fn strict_json_accepts_explicit_instruction_at_end_of_large_context() {
    let context = "let value = json.Marshal(payload);\n".repeat(20_000);
    let prompt = format!(
        "{context}\nReply with exactly one minified JSON object and no markdown or explanation."
    );
    assert!(strict_json_requested(&request(None, &prompt)));
}

#[test]
fn strict_json_requires_cues_in_one_local_window() {
    let prompt = format!(
        "Reply with exactly one result.{}JSON is mentioned here.{}No markdown.",
        "x".repeat(700),
        "y".repeat(700),
    );
    assert!(!strict_json_requested(&request(None, &prompt)));
}

#[test]
fn strict_json_accepts_explicit_chinese_instruction() {
    assert!(strict_json_requested(&request(
        None,
        "仅返回一个 JSON 对象，不要解释。"
    )));
}
```

- [ ] **Step 2: Run the complete exact-output test module on the server**

```powershell
scp -P 18792 src/anthropic/exact_output.rs root@43.225.196.10:/opt/kiro-rs-json-fix/src/anthropic/exact_output.rs
$remote = @'
docker run --rm \
  -v /opt/kiro-rs-json-fix:/app \
  -v kiro-json-cargo-registry:/usr/local/cargo/registry \
  -v kiro-json-cargo-git:/usr/local/cargo/git \
  -v kiro-json-target:/app/target \
  -w /app rust:1.92-alpine \
  sh -lc 'apk add --no-cache musl-dev perl make >/dev/null && cargo test --no-default-features -j 8 anthropic::exact_output::tests'
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: all exact-output tests pass.

- [ ] **Step 3: Commit the detector fix**

```powershell
git add -- src/anthropic/exact_output.rs
git diff --cached --check
git commit -m "fix(json): 避免长上下文误触发严格输出"
```

### Task 5: 运行 handler 回归与服务器全量验证

**Files:** none

- [ ] **Step 1: Run focused handler recovery tests on the server**

```powershell
$remote = @'
docker run --rm \
  -v /opt/kiro-rs-json-fix:/app \
  -v kiro-json-cargo-registry:/usr/local/cargo/registry \
  -v kiro-json-cargo-git:/usr/local/cargo/git \
  -v kiro-json-target:/app/target \
  -w /app rust:1.92-alpine \
  sh -lc 'apk add --no-cache musl-dev perl make >/dev/null && cargo test --no-default-features -j 8 strict_json_from_events && cargo test --no-default-features -j 8 strict_json_recovery'
'@
ssh -p 18792 root@43.225.196.10 $remote
```

- [ ] **Step 2: Run the complete Rust suite**

```powershell
$remote = @'
docker run --rm \
  -v /opt/kiro-rs-json-fix:/app \
  -v kiro-json-cargo-registry:/usr/local/cargo/registry \
  -v kiro-json-cargo-git:/usr/local/cargo/git \
  -v kiro-json-target:/app/target \
  -w /app rust:1.92-alpine \
  sh -lc 'apk add --no-cache musl-dev perl make >/dev/null && cargo test --no-default-features -j 8 --quiet && cargo check --no-default-features -j 8 --quiet'
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: all Rust tests and cargo check pass.

- [ ] **Step 3: Run local format and scope checks**

```powershell
rustfmt --edition 2024 --check src/anthropic/exact_output.rs
git diff --check
git status --short --branch
git diff master --stat
```

Inspect `git diff master --` and confirm no request body, credential, `csk_` value, profile ARN, production log or trace database content was added.

### Task 6: 运行隔离 HTTP 复现验收

**Files on server:**
- Create runtime snapshot: `/opt/kiro-rs-json-data`
- Create temporary container: `kiro-rs-json-fix`

- [ ] **Step 1: Build the fixed test image**

```powershell
ssh -p 18792 root@43.225.196.10 "cd /opt/kiro-rs-json-fix && docker build -t kiro-rs-json-fix:test ."
```

- [ ] **Step 2: Create an isolated runtime snapshot without logs or databases**

```powershell
$remote = @'
set -eu
install -d -m 700 /opt/kiro-rs-json-data
for f in config.json credentials.json client_api_keys.json model_mappings.json; do
  if [ -f "/opt/kiro-rs-admin/config/$f" ]; then
    install -m 600 "/opt/kiro-rs-admin/config/$f" "/opt/kiro-rs-json-data/$f"
  fi
done
'@
ssh -p 18792 root@43.225.196.10 $remote
```

- [ ] **Step 3: Start the isolated container on loopback 18991**

```powershell
$remote = @'
docker rm -f kiro-rs-json-fix 2>/dev/null || true
docker run -d \
  --name kiro-rs-json-fix \
  -p 127.0.0.1:18991:8990 \
  -v /opt/kiro-rs-json-data:/app/config \
  --restart unless-stopped \
  kiro-rs-json-fix:test
for _ in $(seq 1 30); do
  curl -fsS http://127.0.0.1:18991/ >/dev/null && exit 0
  sleep 1
done
docker logs --tail 100 kiro-rs-json-fix
exit 1
'@
ssh -p 18792 root@43.225.196.10 $remote
```

- [ ] **Step 4: Send a false-positive-shaped request without exposing the key**

在服务器内执行以下 Python；Key 只在进程内存中使用且不输出：

```powershell
$py = @'
import json
import urllib.request

with open('/opt/kiro-rs-json-data/client_api_keys.json', encoding='utf-8') as f:
    keys = json.load(f)
key = next(item['key'] for item in keys if not item.get('disabled', False))

noise = (
    'value, err := json.Marshal(payload)\n'
    '// Exactly one purchase should succeed.\n'
    'const historical_instruction = "no markdown";\n'
) * 200
payload = {
    'model': 'claude-opus-4-8',
    'max_tokens': 16,
    'messages': [{
        'role': 'user',
        'content': noise + '\nWhat is 2 + 2? Answer briefly.',
    }],
}
request = urllib.request.Request(
    'http://127.0.0.1:18991/v1/messages',
    data=json.dumps(payload).encode(),
    headers={
        'content-type': 'application/json',
        'anthropic-version': '2023-06-01',
        'x-api-key': key,
    },
    method='POST',
)
with urllib.request.urlopen(request, timeout=60) as response:
    body = json.load(response)
    assert response.status == 200
    assert body.get('type') == 'message', body
    assert body.get('error', {}).get('type') != 'upstream_json_protocol_error', body
    print(json.dumps({
        'status': response.status,
        'type': body.get('type'),
        'stop_reason': body.get('stop_reason'),
    }))
'@
$b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($py))
ssh -p 18792 root@43.225.196.10 "echo $b64 | base64 -d | python3"
```

验收条件：

```text
HTTP 200
response type = message
response error.type != upstream_json_protocol_error
test-container logs do not contain "strict JSON recovery exhausted"
```

- [ ] **Step 5: Confirm production was untouched**

```powershell
ssh -p 18792 root@43.225.196.10 "docker inspect kiro-rs-admin --format '{{.State.Status}} {{.Config.Image}} {{json .HostConfig.PortBindings}}'"
```

Expected: `running`, original production image, and only `127.0.0.1:8990` binding.

- [ ] **Step 6: Remove the temporary runtime container and copied credentials**

```powershell
$remote = @'
set -eu
docker rm -f kiro-rs-json-fix 2>/dev/null || true
path=/opt/kiro-rs-json-data
test "$path" = /opt/kiro-rs-json-data
rm -rf -- "$path"
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Keep `/opt/kiro-rs-json-fix` only until branch integration is complete; it contains source/build output but no runtime credentials.

### Task 7: 完成交付分支

**Files:** none

- [ ] **Step 1: Review commits and clean status**

```powershell
git status --short --branch
git log --oneline master..HEAD
git diff master --check
```

- [ ] **Step 2: Enter branch-finishing workflow**

Use `superpowers:verification-before-completion` and `superpowers:finishing-a-development-branch`. Offer local merge, PR, keep branch or discard. Do not push or replace production without explicit user authorization.
