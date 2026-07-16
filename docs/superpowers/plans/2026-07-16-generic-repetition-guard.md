# 通用流式复读熔断 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Anthropic 实时流和 Claude Code 缓冲流中识别普通文本/Thinking 的通用连续复读，并以 `max_tokens` 完整收尾且立即停止读取上游。

**Architecture:** 扩展现有 `StreamContext` 复读状态，将固定 `call/count/card` 检测改为按通道比较任意不超过 512 字节的完全相同短行/分片；Thinking 通过统一受保护发出口生成 delta。handler 在每个上游 chunk 处理完成后检查跳闸状态，若已跳闸则把该轮当作受控 EOF 收尾，使 response stream 被丢弃并取消。

**Tech Stack:** Rust、Tokio/Futures、Anthropic SSE、现有 `StreamContext`/`BufferedStreamContext`、Cargo test

---

## 文件结构

- Modify: `src/anthropic/stream.rs` — 通用复读状态、Text/Thinking 过滤、公开跳闸状态及单元测试。
- Modify: `src/anthropic/handlers.rs` — realtime/buffered 上游循环在跳闸后停止读取，并记录诊断。
- Create: `scripts/repetition-guard.contract.test.ts` — 固定两个 handler 都接入主动终止检查，防止后续重构漏掉其中一路。

### Task 1: 用失败测试固定 Text 与 Thinking 复读行为

**Files:**
- Modify: `src/anthropic/stream.rs`

- [ ] **Step 1: 添加普通文本 `}` 洪水失败测试**

在 `stream.rs` 现有 repeat guard 测试区增加测试。循环发送 100 个 `}\n\n`，期望最多保留阈值前的重复、`repetition_guard_tripped()` 为 true，最终 `message_delta.delta.stop_reason == "max_tokens"`：

```rust
#[test]
fn repeat_guard_trips_on_generic_brace_flood() {
    let mut ctx = StreamContext::new_with_thinking(
        "test-model",
        1,
        false,
        HashMap::new(),
        test_known_tools(),
    );
    let _ = ctx.generate_initial_events();
    let mut events = Vec::new();
    for _ in 0..100 {
        events.extend(ctx.process_assistant_response("}\n\n"));
    }
    events.extend(ctx.generate_final_events());

    let text = collect_text_content(&events);
    assert!(text.matches('}').count() < 32, "generic flood was not stopped: {text:?}");
    assert!(ctx.repetition_guard_tripped());
    let message_delta = events.iter().find(|event| event.event == "message_delta").unwrap();
    assert_eq!(message_delta.data["delta"]["stop_reason"], "max_tokens");
}
```

- [ ] **Step 2: 添加原生 Thinking 洪水失败测试**

创建启用 thinking 的 context，循环发送 `ReasoningContentEvent { text: Some("}\n\n"), ... }`，断言可见 `thinking_delta` 中的 `}` 少于 32、guard 跳闸并以 `max_tokens` 收尾：

```rust
#[test]
fn repeat_guard_trips_on_native_thinking_flood() {
    let mut ctx = StreamContext::new_with_thinking(
        "test-model",
        1,
        true,
        HashMap::new(),
        test_known_tools(),
    );
    let _ = ctx.generate_initial_events();
    let mut events = Vec::new();
    for _ in 0..100 {
        events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("}\n\n".into()),
                signature: None,
                redacted_content: None,
            },
        )));
    }
    events.extend(ctx.generate_final_events());
    let thinking = collect_thinking_content(&events);
    assert!(thinking.matches('}').count() < 32);
    assert!(ctx.repetition_guard_tripped());
    let message_delta = events.iter().find(|event| event.event == "message_delta").unwrap();
    assert_eq!(message_delta.data["delta"]["stop_reason"], "max_tokens");
}
```

- [ ] **Step 3: 添加边界与不误伤测试**

增加两项：15 次完全相同的 `}\n` 不跳闸；不同前导缩进的闭合括号连续循环不跳闸且内容完整。

```rust
#[test]
fn repeat_guard_allows_fifteen_identical_lines() {
    let mut ctx = StreamContext::new_with_thinking(
        "test-model",
        1,
        false,
        HashMap::new(),
        test_known_tools(),
    );
    let _ = ctx.generate_initial_events();
    let mut events = Vec::new();
    for _ in 0..15 {
        events.extend(ctx.process_assistant_response("}\n"));
    }
    assert!(!ctx.repetition_guard_tripped());
    assert_eq!(collect_text_content(&events).matches('}').count(), 15);
}

#[test]
fn repeat_guard_preserves_differently_indented_braces() {
    let mut ctx = StreamContext::new_with_thinking(
        "test-model",
        1,
        false,
        HashMap::new(),
        test_known_tools(),
    );
    let _ = ctx.generate_initial_events();
    let mut events = Vec::new();
    for _ in 0..40 {
        events.extend(ctx.process_assistant_response("}\n  }\n    }\n"));
    }
    assert!(!ctx.repetition_guard_tripped());
    assert_eq!(collect_text_content(&events).matches('}').count(), 120);
}
```

- [ ] **Step 4: 运行测试并确认红灯**

Run:

```powershell
cargo test repeat_guard -- --nocapture
```

Expected: 新增 generic brace / native thinking 测试失败；既有 4 项测试继续通过。

### Task 2: 实现通用 Text/Thinking 复读状态

**Files:**
- Modify: `src/anthropic/stream.rs`

- [ ] **Step 1: 把固定 token 状态扩展为通用候选状态**

增加常量和通道字段：

```rust
const REPEAT_GUARD_TRIP_THRESHOLD: u32 = 16;
const REPEAT_GUARD_MAX_UNIT_BYTES: usize = 512;

repeat_guard_last_channel: &'static str,
repeat_guard_last_line: String,
repeat_guard_run: u32,
repeat_guard_tripped: bool,
```

构造函数将 channel 初始化为 `""`。

- [ ] **Step 2: 改造过滤函数**

签名改为：

```rust
fn repeat_guard_filter(&mut self, text: &str, channel: &'static str) -> String
```

逐个 `split_inclusive('\n')` 处理：行尾 `\r/\n/空格/tab` 被移除但行首缩进保留；空行忽略且不重置；1..=512 字节的相同候选在同一 channel 连续累计；不同非空候选、超长候选或 channel 变化重置。达到 16 时设置：

```rust
self.repeat_guard_tripped = true;
self.state_manager.set_stop_reason("max_tokens");
tracing::warn!(channel, repeat_count = self.repeat_guard_run, unit_bytes = candidate.len(), "upstream repetition guard tripped");
```

并返回阈值前已保留内容。

- [ ] **Step 3: 为 Thinking 建立受保护发出口**

增加：

```rust
fn create_guarded_thinking_delta_event(
    &mut self,
    index: i32,
    thinking: &str,
) -> Option<SseEvent> {
    if thinking.is_empty() {
        return Some(self.create_thinking_delta_event(index, ""));
    }
    let kept = self.repeat_guard_filter(thinking, "thinking");
    (!kept.is_empty()).then(|| self.create_thinking_delta_event(index, &kept))
}
```

所有非空 Thinking delta（XML thinking、原生 reasoning、final flush）改走该方法；关闭块所需的空 delta 仍原样发送。

- [ ] **Step 4: Text 调用通用过滤并公开跳闸状态**

`emit_text_delta_raw` 改为 `repeat_guard_filter(text, "text")`，并增加：

```rust
pub fn repetition_guard_tripped(&self) -> bool {
    self.repeat_guard_tripped
}
```

`BufferedStreamContext` 增加同名委托方法。

- [ ] **Step 5: 运行 repeat guard 与 Thinking 测试**

Run:

```powershell
cargo test repeat_guard -- --nocapture
cargo test thinking -- --nocapture
```

Expected: 新旧复读测试全部通过，Thinking 协议测试无回归。

- [ ] **Step 6: 提交流状态修复**

```powershell
git add -- src/anthropic/stream.rs
git diff --cached --check
git commit -m "fix(stream): 熔断通用文本和Thinking复读"
```

### Task 3: 让 realtime 与 buffered handler 主动终止上游

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Create: `scripts/repetition-guard.contract.test.ts`

- [ ] **Step 1: 写 handler 接线失败合约**

读取 `handlers.rs`，断言 `ctx.repetition_guard_tripped()` 至少出现两次，并固定诊断类型和受控 EOF：

```ts
import { expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

test('realtime and buffered streams stop upstream after repetition guard trips', async () => {
  const source = await readFile('src/anthropic/handlers.rs', 'utf8')
  expect(source.match(/ctx\.repetition_guard_tripped\(\)/g)?.length ?? 0).toBeGreaterThanOrEqual(2)
  expect(source).toContain('upstream_repetition_guard')
  expect(source).toContain('break AttemptTermination::Eof')
})
```

- [ ] **Step 2: 运行合约并确认红灯**

Run: `bun test scripts/repetition-guard.contract.test.ts`

Expected: FAIL，当前 handler 尚未检查 guard。

- [ ] **Step 3: 接入实时 handler**

在 realtime chunk 的事件发送成功后检查 guard；跳闸时记录无正文诊断并 `break AttemptTermination::Eof`：

```rust
if ctx.repetition_guard_tripped() {
    tracing::warn!(attempt = attempt_index + 1, received_bytes, "upstream repetition guard ended realtime stream");
    tracer.record_protocol_error("upstream_repetition_guard", "repeated upstream output was truncated");
    break AttemptTermination::Eof;
}
```

- [ ] **Step 4: 接入 buffered handler**

在 buffered chunk 解码完成后执行同样检查和受控 EOF。`BufferedStreamContext` 已在 Task 2 暴露委托方法。

- [ ] **Step 5: 运行 handler 合约和定向 Rust 测试**

```powershell
bun test scripts/repetition-guard.contract.test.ts
cargo test repeat_guard -- --nocapture
```

Expected: 全部通过。

- [ ] **Step 6: 提交 handler 主动终止**

```powershell
git add -- src/anthropic/handlers.rs scripts/repetition-guard.contract.test.ts
git diff --cached --check
git commit -m "fix(stream): 复读后主动结束上游流"
```

### Task 4: 全量验证与本地集成

**Files:**
- Verify: `src/anthropic/stream.rs`
- Verify: `src/anthropic/handlers.rs`
- Verify: `scripts/repetition-guard.contract.test.ts`

- [ ] **Step 1: 运行格式化与差异检查**

```powershell
cargo fmt --check
git diff --check
```

- [ ] **Step 2: 运行 Rust 全量测试**

```powershell
cargo test
```

Expected: 现有测试与新增测试全部通过；允许保留项目已有的两项编译 warning，不新增 warning。

- [ ] **Step 3: 运行前端与脚本合约测试**

```powershell
cd admin-ui
bun test
cd ..
bun test scripts/repetition-guard.contract.test.ts scripts/release.contract.test.ts
```

- [ ] **Step 4: 审计客户影响与改动范围**

```powershell
git diff master...HEAD --check
git diff master...HEAD --name-status
git status --short --branch
```

Expected: 只包含设计/计划、`stream.rs`、`handlers.rs` 和新合约测试；工作树干净。

- [ ] **Step 5: 本地合并回 master**

确认主工作区无未提交改动后，以非快进合并提交接入 `master`，合并后重新运行 repeat guard 与合约测试。不得推送、发布或部署，除非用户另行明确要求。
