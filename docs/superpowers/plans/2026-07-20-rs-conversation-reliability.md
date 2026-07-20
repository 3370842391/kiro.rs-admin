# RS 客户对话工具可靠性修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 安全修复工具参数等价别名和同消息内完全重复的 `tool_use`，消除生产日志中可由 RS 解决的客户续轮中断。

**Architecture:** 在既有 `tool_schema` 事务式校验边界内加入 Schema 感知的受限别名搬移；在既有 `tool_history` ID 规范化前，只读记录完全相同块的索引，全部唯一性和配对校验成功后再原地提交去重。两处修改均失败关闭，不复制整段对话，不猜测缺失业务值，并沿用现有流式、非流式和错误快照路径。

**Tech Stack:** Rust 2024、serde/serde_json、现有单元测试、Cargo、Docker BuildKit、隔离公网 8991。

---

## 文件结构

- `src/anthropic/tool_schema.rs`：声明受限字段别名并在对象 Schema 校验前安全搬移现有值；同文件单元测试覆盖成功和失败关闭边界。
- `src/anthropic/tool_history.rs`：只读扫描同消息内完全重复的工具块，校验成功后按索引原地删除，保留冲突/跨消息重复的严格拒绝；同文件单元测试覆盖配对行为。
- `src/anthropic/converter.rs`：消费规范化报告，只记录去重数量，不记录工具输入正文。
- `docs/superpowers/specs/2026-07-20-rs-conversation-reliability-design.md`：已批准设计，不再扩大范围。

### Task 1: Schema 感知的工具字段别名搬移

**Files:**
- Modify: `src/anthropic/tool_schema.rs`
- Test: `src/anthropic/tool_schema.rs`

- [ ] **Step 1: 写新增别名成功搬移的失败测试**

在现有 `repairs_file_path_alias_when_path_is_required` 后加入：

```rust
#[test]
fn repairs_observed_aliases_only_when_target_is_required_by_schema() {
    for (source, target) in [
        ("name_path", "name_path_pattern"),
        ("content", "contents"),
        ("pattern", "glob_pattern"),
        ("query", "pattern"),
    ] {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [target.to_string()],
            "additionalProperties": false
        });
        schema["properties"][target] = serde_json::json!({"type": "string"});
        let mut input = serde_json::Value::Object(serde_json::Map::from_iter([(
            source.to_string(),
            serde_json::json!("customer-value"),
        )]));

        assert_eq!(
            validate_and_repair(&schema, &mut input),
            ToolInputOutcome::Repaired {
                paths: vec![format!("$.{target}")]
            }
        );
        assert_eq!(
            input,
            serde_json::Value::Object(serde_json::Map::from_iter([(
                target.to_string(),
                serde_json::json!("customer-value"),
            )]))
        );
    }
}
```

- [ ] **Step 2: 运行测试并确认 RED**

Run:

```powershell
cargo test anthropic::tool_schema::tests::repairs_observed_aliases_only_when_target_is_required_by_schema -- --exact
```

Expected: FAIL；返回 `MissingRequired`/`AdditionalProperty`，证明四个新别名尚未实现。

- [ ] **Step 3: 写失败关闭边界测试**

加入三个独立测试：

```rust
#[test]
fn alias_repair_never_overwrites_target_or_declared_source() {
    let target_schema = serde_json::json!({
        "type": "object",
        "properties": {"contents": {"type": "string"}},
        "required": ["contents"],
        "additionalProperties": false
    });
    let mut conflict = serde_json::json!({
        "content": "source",
        "contents": "target"
    });
    let original_conflict = conflict.clone();
    assert!(matches!(
        validate_and_repair(&target_schema, &mut conflict),
        ToolInputOutcome::Invalid { .. }
    ));
    assert_eq!(conflict, original_conflict);

    let both_declared = serde_json::json!({
        "type": "object",
        "properties": {
            "content": {"type": "string"},
            "contents": {"type": "string"}
        },
        "required": ["contents"],
        "additionalProperties": false
    });
    let mut declared_source = serde_json::json!({"content": "source"});
    let original_declared_source = declared_source.clone();
    assert!(matches!(
        validate_and_repair(&both_declared, &mut declared_source),
        ToolInputOutcome::Invalid { .. }
    ));
    assert_eq!(declared_source, original_declared_source);
}

#[test]
fn alias_repair_requires_matching_declared_type_and_required_target() {
    let required_string = serde_json::json!({
        "type": "object",
        "properties": {"contents": {"type": "string"}},
        "required": ["contents"],
        "additionalProperties": false
    });
    let mut wrong_type = serde_json::json!({"content": 7});
    let original_wrong_type = wrong_type.clone();
    assert!(matches!(
        validate_and_repair(&required_string, &mut wrong_type),
        ToolInputOutcome::Invalid { .. }
    ));
    assert_eq!(wrong_type, original_wrong_type);

    let optional_target = serde_json::json!({
        "type": "object",
        "properties": {"contents": {"type": "string"}},
        "required": [],
        "additionalProperties": false
    });
    let mut optional = serde_json::json!({"content": "source"});
    let original_optional = optional.clone();
    assert!(matches!(
        validate_and_repair(&optional_target, &mut optional),
        ToolInputOutcome::Invalid { .. }
    ));
    assert_eq!(optional, original_optional);
}

#[test]
fn alias_repair_is_transactional_when_target_constraints_fail() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "glob_pattern": {"type": "string", "pattern": "^src/"}
        },
        "required": ["glob_pattern"],
        "additionalProperties": false
    });
    let mut input = serde_json::json!({"pattern": "private/*.txt"});
    let original = input.clone();

    assert!(matches!(
        validate_and_repair(&schema, &mut input),
        ToolInputOutcome::Invalid { violations }
            if violations.iter().any(|violation| {
                matches!(violation, ToolInputViolation::ConstraintViolation {
                    path,
                    keyword: "pattern"
                } if path == "$.glob_pattern")
            })
    ));
    assert_eq!(input, original);
}
```

- [ ] **Step 4: 实现最小通用别名搬移**

在常量区加入：

```rust
const SAFE_REQUIRED_PROPERTY_ALIASES: &[(&str, &str)] = &[
    ("file_path", "path"),
    ("name_path", "name_path_pattern"),
    ("content", "contents"),
    ("pattern", "glob_pattern"),
    ("query", "pattern"),
];
```

在 `validate_object` 前增加：

```rust
fn repair_required_property_aliases(
    properties: &serde_json::Map<String, serde_json::Value>,
    required: &std::collections::HashSet<&str>,
    object: &mut serde_json::Map<String, serde_json::Value>,
    path: &str,
    repairs: &mut Vec<String>,
) {
    for &(source, target) in SAFE_REQUIRED_PROPERTY_ALIASES {
        if !required.contains(target)
            || object.contains_key(target)
            || properties.contains_key(source)
        {
            continue;
        }
        let Some(target_schema) = properties.get(target) else {
            continue;
        };
        let Some(source_value) = object.get(source) else {
            continue;
        };
        let Some(declared_type) = target_schema.get("type") else {
            continue;
        };
        if !matches_declared_type(declared_type, source_value) {
            continue;
        }

        let value = object
            .remove(source)
            .expect("source alias was checked before removal");
        object.insert(target.to_string(), value);
        repairs.push(property_path(path, target));
    }
}
```

用以下调用替换 `validate_object` 中现有的 `file_path` 特例：

```rust
if let Some(properties) = properties {
    repair_required_property_aliases(properties, &required, object, path, repairs);
}
```

- [ ] **Step 5: 运行聚焦测试并确认 GREEN**

Run:

```powershell
cargo test anthropic::tool_schema
```

Expected: 所有 `tool_schema` 测试 PASS，现有 `file_path -> path` 测试保持通过。

- [ ] **Step 6: 提交 Task 1**

```powershell
git add -- src/anthropic/tool_schema.rs
git diff --cached --check
git commit -m "fix(tool): 安全兼容已观测参数别名"
```

### Task 2: 完全重复历史工具块去重

**Files:**
- Modify: `src/anthropic/tool_history.rs`
- Modify: `src/anthropic/converter.rs`
- Test: `src/anthropic/tool_history.rs`

- [ ] **Step 1: 写完全相同重复块的失败测试**

在 `tool_history.rs` 测试模块加入：

```rust
#[test]
fn deduplicates_identical_tool_uses_within_one_assistant_message() {
    let tool_use = ToolUseEntry::new("duplicate:1", "get_weather")
        .with_input(serde_json::json!({"city": "Paris"}));
    let mut history = vec![Message::Assistant(HistoryAssistantMessage {
        assistant_response_message: AssistantMessage::new("calling tool")
            .with_tool_uses(vec![tool_use.clone(), tool_use]),
    })];
    let mut current = vec![ToolResult::success("duplicate:1", "sunny")];

    let report = normalize_tool_history_ids(&mut history, &mut current).unwrap();

    let Message::Assistant(message) = &history[0] else {
        panic!("expected assistant message");
    };
    assert_eq!(
        message
            .assistant_response_message
            .tool_uses
            .as_ref()
            .expect("tool uses")
            .len(),
        1
    );
    assert_eq!(report.deduplicated_tool_uses, 1);
    assert_eq!(current[0].tool_use_id, tool_use_id(&history[0], 0));
}
```

- [ ] **Step 2: 运行测试并确认 RED**

Run:

```powershell
cargo test anthropic::tool_history::tests::deduplicates_identical_tool_uses_within_one_assistant_message -- --exact
```

Expected: 编译失败（报告字段不存在）或返回 `DuplicateToolUseId`。

- [ ] **Step 3: 写冲突和跨消息重复的严格拒绝测试**

加入：

```rust
#[test]
fn rejects_same_message_duplicate_id_with_different_name_or_input() {
    for second in [
        ToolUseEntry::new("duplicate:1", "other_tool")
            .with_input(serde_json::json!({"city": "Paris"})),
        ToolUseEntry::new("duplicate:1", "get_weather")
            .with_input(serde_json::json!({"city": "London"})),
    ] {
        let first = ToolUseEntry::new("duplicate:1", "get_weather")
            .with_input(serde_json::json!({"city": "Paris"}));
        let mut history = vec![Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: AssistantMessage::new("calling tool")
                .with_tool_uses(vec![first, second]),
        })];

        assert_eq!(
            normalize_tool_history_ids(&mut history, &mut []).unwrap_err(),
            ToolHistoryError::DuplicateToolUseId("duplicate:1".into())
        );
    }
}

#[test]
fn rejects_identical_tool_use_id_reused_across_assistant_messages() {
    let mut history = vec![
        assistant_with_tool_uses(&["duplicate:1"]),
        assistant_with_tool_uses(&["duplicate:1"]),
    ];

    assert_eq!(
        normalize_tool_history_ids(&mut history, &mut []).unwrap_err(),
        ToolHistoryError::DuplicateToolUseId("duplicate:1".into())
    );
}
```

- [ ] **Step 4: 实现事务式同消息去重**

扩展报告：

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolIdNormalization {
    pub(crate) rewritten_ids: HashMap<String, String>,
    pub(crate) deduplicated_tool_uses: usize,
}
```

增加只读索引扫描函数：

```rust
fn identical_tool_use_duplicate_indices(
    history: &[Message],
) -> Result<Vec<Vec<usize>>, ToolHistoryError> {
    let mut duplicate_indices = Vec::with_capacity(history.len());
    for message in history {
        let Message::Assistant(message) = message else {
            duplicate_indices.push(Vec::new());
            continue;
        };
        let Some(tool_uses) = &message.assistant_response_message.tool_uses else {
            duplicate_indices.push(Vec::new());
            continue;
        };
        let mut seen = HashMap::<&str, (&str, &serde_json::Value)>::new();
        let mut message_duplicates = Vec::new();
        for (index, tool_use) in tool_uses.iter().enumerate() {
            match seen.get(tool_use.tool_use_id.as_str()) {
                Some(&(name, input))
                    if name == tool_use.name.as_str() && input == &tool_use.input =>
                {
                    message_duplicates.push(index);
                }
                Some(_) => {
                    return Err(ToolHistoryError::DuplicateToolUseId(
                        tool_use.tool_use_id.clone(),
                    ));
                }
                None => {
                    seen.insert(
                        tool_use.tool_use_id.as_str(),
                        (tool_use.name.as_str(), &tool_use.input),
                    );
                }
            }
        }
        duplicate_indices.push(message_duplicates);
    }
    Ok(duplicate_indices)
}
```

在 `normalize_tool_history_ids` 起始处只读扫描：

```rust
let duplicate_indices = identical_tool_use_duplicate_indices(history)?;
let deduplicated_tool_uses = duplicate_indices.iter().map(Vec::len).sum();
```

首轮全局 ID 校验按 `message_index/tool_index` 跳过已确认的完全重复索引；现有 `tool_result` 校验全部成功后，再原地删除这些索引：

```rust
for (message, duplicates) in history.iter_mut().zip(&duplicate_indices) {
    if duplicates.is_empty() {
        continue;
    }
    let Message::Assistant(message) = message else {
        continue;
    };
    let Some(tool_uses) = &mut message.assistant_response_message.tool_uses else {
        continue;
    };
    let mut index = 0;
    tool_uses.retain(|_| {
        let keep = !duplicates.contains(&index);
        index += 1;
        keep
    });
}

Ok(ToolIdNormalization {
    rewritten_ids,
    deduplicated_tool_uses,
})
```

确保任何后续孤立结果或冲突错误都不会部分修改调用方的 `history`，同时避免复制超长对话正文和图片。

- [ ] **Step 5: 在转换入口记录安全计数**

把 `converter.rs` 中忽略返回值的调用改为：

```rust
let normalization =
    super::tool_history::normalize_tool_history_ids(&mut history, &mut tool_results)
        .map_err(|error| ConversionError::InvalidToolHistory(error.to_string()))?;
if normalization.deduplicated_tool_uses > 0 {
    tracing::warn!(
        count = normalization.deduplicated_tool_uses,
        "去重同一助手消息内完全相同的历史工具调用"
    );
}
```

日志不写 ID、name 或 input 正文。

- [ ] **Step 6: 运行聚焦测试并确认 GREEN**

Run:

```powershell
cargo test anthropic::tool_history
cargo test anthropic::converter::tests::convert_request_rejects_duplicate_tool_use_id_locally -- --exact
```

Expected: 新增完全重复用例 PASS；既有冲突重复、孤立结果、重复结果和非法 ID 测试全部 PASS。

- [ ] **Step 7: 提交 Task 2**

```powershell
git add -- src/anthropic/tool_history.rs src/anthropic/converter.rs
git diff --cached --check
git commit -m "fix(tool): 去重完全相同的历史工具调用"
```

### Task 3: 全量回归和变更审计

**Files:**
- Verify: `src/anthropic/tool_schema.rs`
- Verify: `src/anthropic/tool_history.rs`
- Verify: `src/anthropic/converter.rs`

- [ ] **Step 1: 格式化并检查差异**

```powershell
cargo fmt
git diff --check
git diff --stat master...HEAD
```

Expected: 无空白错误；源码差异只包含三份目标文件。

- [ ] **Step 2: 运行完整测试**

```powershell
cargo test
```

Expected: 至少基线 `1062` 个既有测试加新增测试全部 PASS，0 failed。

- [ ] **Step 3: 运行编译门禁**

```powershell
cargo check --all-targets
cargo build --release
```

Expected: 两条命令退出码 0；只允许保留基线已有的两个 Rust warning。

- [ ] **Step 4: 审计非目标行为未变**

```powershell
git diff master...HEAD -- src/anthropic/handlers.rs src/anthropic/stream.rs src/anthropic/cache_metering.rs src/model/config.rs
```

Expected: 无输出，证明首字、SSE、缓存和运行配置未被修改。

### Task 4: 部署隔离 8991 并验证

**Files:**
- Use: `scripts/test-deploy.sh`
- Server checkout: `/opt/kiro-rs-test`
- Server test data: `/opt/kiro-rs-test/data-test`

- [ ] **Step 1: 记录生产和测试容器边界**

```powershell
ssh -p 18792 root@43.225.196.10 "docker inspect kiro-rs-admin kiro-rs-test --format '{{.Name}} {{.Config.Image}} {{.State.Status}} {{.State.Health.Status}}'; git -C /opt/kiro-rs-test status --short"
```

Expected: `kiro-rs-admin` 和 `kiro-rs-test` 是不同容器，测试 checkout 无 tracked changes。

- [ ] **Step 2: 不推送 GitHub 地传输已验证提交**

在本地创建只包含当前分支提交的 bundle：

```powershell
git bundle create "$env:TEMP\rs-conversation-reliability.bundle" HEAD
scp -P 18792 "$env:TEMP\rs-conversation-reliability.bundle" root@43.225.196.10:/tmp/rs-conversation-reliability.bundle
```

在服务器导入为测试专用引用：

```powershell
ssh -p 18792 root@43.225.196.10 "git -C /opt/kiro-rs-test fetch /tmp/rs-conversation-reliability.bundle HEAD:refs/heads/test/rs-conversation-reliability && rm -f /tmp/rs-conversation-reliability.bundle"
```

Expected: 服务器本地分支指向本轮验证提交，不修改 GitHub remote。

- [ ] **Step 3: 构建并替换 8991 测试容器**

```powershell
$sha = (git rev-parse HEAD).Trim()
ssh -p 18792 root@43.225.196.10 "cd /opt/kiro-rs-test && ./scripts/test-deploy.sh $sha"
```

Expected: 输出准确 `commit=<sha>`、`image=kiro-rs-test:<sha>`、`url=http://127.0.0.1:8991/admin`；生产 `kiro-rs-admin` 未重启。

- [ ] **Step 4: 验证公开入口与隔离边界**

```powershell
ssh -p 18792 root@43.225.196.10 "curl -fsS http://127.0.0.1:8991/admin >/dev/null && docker inspect kiro-rs-test --format '{{.Config.Image}} {{.RestartCount}} {{.State.Health.Status}}'"
curl.exe -fsS https://rs-test.43-225-196-10.sslip.io/admin -o NUL
```

Expected: 内外健康检查成功，测试容器 healthy，公开 HTTPS 管理端返回 200。

- [ ] **Step 5: 验证日志中无新增可修复中断**

使用测试 Key 执行普通流式文本和一轮真实工具调用续轮；随后只查询 `kiro-rs-test` 的 trace/错误快照，确认：

- 普通对话和首字握手保持正常。
- 完全相同的历史重复块被安全去重且续轮成功。
- 等价字段别名满足 Schema 时交付目标字段和值。
- 同 ID 冲突、缺少路径等不可推断值仍被拒绝。
- 8990 生产容器的镜像、启动时间和重启计数不变。
