# 利润计费账本准确性实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让利润报表以 usage 账本中的上游真实 Credits 计算总成本，并用 `traceId` 精确归属新请求，同时把旧数据的未归属成本完整计入总利润。

**Architecture:** 扩展现有 `usage_log.YYYY-MM-DD.jsonl` 为权威轻量计费账本，不新增高频数据库。利润处理器并行读取 NewAPI 消费日志、usage 范围记录和 legacy trace；先从精确匹配识别 RS 渠道与 Key，再用同 Key 全部 usage 计算总成本，trace 仅作为旧记录归属补充。

**Tech Stack:** Rust 2024、Axum、Serde JSONL、Chrono、React 19、TypeScript、Bun Test。

---

### Task 1: 扩展 usage 账本与精确范围读取

**Files:**
- Modify: `src/admin/usage_stats.rs`
- Test: `src/admin/usage_stats.rs`

- [ ] **Step 1: 写旧格式兼容与范围读取失败测试**

在 `usage_stats.rs` 测试模块新增：

```rust
#[test]
fn usage_record_trace_id_is_backward_compatible() {
    let legacy: UsageRecord = serde_json::from_value(serde_json::json!({
        "ts": "2026-07-23T01:00:00Z",
        "keyId": 3,
        "credentialId": 9,
        "model": "claude-opus-4-8",
        "inputTokens": 10,
        "outputTokens": 5,
        "status": "success"
    })).unwrap();
    assert_eq!(legacy.trace_id, None);

    let mut current = legacy;
    current.trace_id = Some("trace-1".to_string());
    assert_eq!(serde_json::to_value(current).unwrap()["traceId"], "trace-1");
}

#[test]
fn usage_recorder_reads_exact_window_and_reports_invalid_lines() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = UsageRecorder::with_retention(dir.path().to_path_buf(), 31);
    recorder.record(&usage_test_record("2026-07-23T01:00:00Z", Some("a"), 1.0));
    recorder.record(&usage_test_record("2026-07-23T02:00:00Z", None, 2.0));
    recorder.record(&usage_test_record("2026-07-23T03:00:00Z", Some("c"), 3.0));

    let result = recorder.query_range(
        DateTime::parse_from_rfc3339("2026-07-23T01:30:00Z").unwrap().timestamp(),
        DateTime::parse_from_rfc3339("2026-07-23T03:00:00Z").unwrap().timestamp(),
    ).unwrap();
    assert_eq!(result.records.len(), 2);
    assert_eq!(result.records[0].credits, 2.0);
    assert_eq!(result.records[1].credits, 3.0);
    assert_eq!(result.invalid_lines, 0);
}
```

- [ ] **Step 2: 运行测试确认字段和接口缺失**

Run:

```powershell
cargo test admin::usage_stats::tests::usage_record_trace_id_is_backward_compatible -- --nocapture
cargo test admin::usage_stats::tests::usage_recorder_reads_exact_window -- --nocapture
```

Expected: FAIL，提示 `trace_id`、`query_range` 或 `UsageRangeRead` 不存在。

- [ ] **Step 3: 实现账本字段与流式范围读取**

给 `UsageRecord` 增加：

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub trace_id: Option<String>,
```

新增：

```rust
#[derive(Debug, Default)]
pub struct UsageRangeRead {
    pub records: Vec<UsageRecord>,
    pub invalid_lines: u64,
}

impl UsageRecorder {
    pub fn query_range(&self, start_epoch: i64, end_epoch: i64) -> std::io::Result<UsageRangeRead> {
        if start_epoch > end_epoch {
            return Ok(UsageRangeRead::default());
        }
        if let Some(writer) = self.inner.lock().writer.as_mut() {
            writer.flush()?;
        }
        let mut result = UsageRangeRead::default();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if parse_usage_log_filename(&name).is_none() { continue; }
            let reader = BufReader::new(File::open(entry.path())?);
            for line in reader.lines() {
                let line = line?;
                let Ok(record) = serde_json::from_str::<UsageRecord>(&line) else {
                    result.invalid_lines += 1;
                    continue;
                };
                let Ok(ts) = DateTime::parse_from_rfc3339(&record.ts) else {
                    result.invalid_lines += 1;
                    continue;
                };
                if ts.timestamp() >= start_epoch && ts.timestamp() <= end_epoch {
                    result.records.push(record);
                }
            }
        }
        result.records.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(result)
    }
}
```

- [ ] **Step 4: 更新现有 UsageRecord 构造并运行 usage 测试**

所有现有测试构造显式增加 `trace_id: None`，然后运行：

```powershell
cargo test admin::usage_stats::tests -- --nocapture
```

Expected: PASS。

- [ ] **Step 5: 提交 usage 账本基础**

```powershell
git add -- src/admin/usage_stats.rs
git commit -m "feat(profit): 扩展真实用量账本"
```

### Task 2: 把请求 traceId 写入每条 usage

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Test: `src/anthropic/handlers.rs`

- [ ] **Step 1: 写 UsageRecordHook traceId 失败测试**

新增一个仅构造记录的私有方法并先写期望测试：

```rust
#[test]
fn usage_hook_carries_request_trace_id() {
    let hook = UsageRecordHook::test_hook("trace-ledger-1");
    let record = hook.build_record(7, 10, 5, 0, 0, 0.25, "success");
    assert_eq!(record.trace_id.as_deref(), Some("trace-ledger-1"));
    assert_eq!(record.key_id, hook.key_id);
}
```

- [ ] **Step 2: 运行测试确认 hook 尚未保存 traceId**

Run:

```powershell
cargo test anthropic::handlers::tests::usage_hook_carries_request_trace_id -- --nocapture
```

Expected: FAIL。

- [ ] **Step 3: 调整 hook 和两个消息入口**

给 `UsageRecordHook` 增加：

```rust
pub trace_id: Option<String>,
```

把记录构造提取为 `build_record`，`record` 继续负责写 recorder/aggregator/client key。`post_messages` 和 `post_messages_cc` 均先创建 `RequestTracer`，再创建：

```rust
let hook = UsageRecordHook::from_state(
    &state,
    &key_ctx,
    payload.model.clone(),
    Some(tracer.trace_id().to_string()),
);
```

保证 OpenAI `/chat/completions` 和 `/responses` 继续把相同 headers 传入 `post_messages`，无需另建 ID。

- [ ] **Step 4: 运行 handler 与 OpenAI 回归测试**

```powershell
cargo test anthropic::handlers::tests::usage_hook_carries_request_trace_id -- --nocapture
cargo test openai::handlers::tests -- --nocapture
```

Expected: PASS。

- [ ] **Step 5: 提交 traceId 写入**

```powershell
git add -- src/anthropic/handlers.rs
git commit -m "feat(profit): 记录请求计费关联ID"
```

### Task 3: 实现真实账本利润聚合

**Files:**
- Modify: `src/admin/profit.rs`
- Test: `src/admin/profit.rs`

- [ ] **Step 1: 写总成本、渠道过滤和旧数据兜底失败测试**

新增测试覆盖：

```rust
#[test]
fn ledger_report_uses_all_usage_cost_for_observed_rs_keys() {
    let logs = vec![
        newapi_log("matched", 19, 500_000),
        newapi_log("legacy-missing", 19, 500_000),
        newapi_log("gpt", 1, 500_000),
    ];
    let usage = vec![
        usage("matched", 3, 1.0, "success"),
        usage_without_trace(3, 9.0, "success"),
        usage_without_trace(3, 2.0, "error"),
        usage_without_trace(99, 100.0, "success"),
    ];
    let report = aggregate_ledger_report(logs, usage, Vec::new(), vec![key(3, "rs", "新号")], ProfitConfig::default());
    assert!(report.ledger_scope_confirmed);
    assert_eq!(report.observed_channel_ids, vec![19]);
    assert_eq!(report.revenue, 2.0);
    assert_eq!(report.credits, 12.0);
    assert_eq!(report.attributed_credits, 1.0);
    assert_eq!(report.unattributed_credits, 11.0);
    assert!((report.cost - 0.27).abs() < 1e-9);
    assert!((report.profit - 1.73).abs() < 1e-9);
}

#[test]
fn ledger_report_fails_closed_without_observed_channel_or_key() {
    let report = aggregate_ledger_report(
        vec![newapi_log("missing", 19, 500_000)],
        vec![usage_without_trace(3, 10.0, "success")],
        Vec::new(),
        vec![key(3, "rs", "新号")],
        ProfitConfig::default(),
    );
    assert!(!report.ledger_scope_confirmed);
    assert_eq!(report.cost, 0.0);
    assert_eq!(report.profit, 0.0);
}
```

- [ ] **Step 2: 运行测试确认新聚合接口缺失**

```powershell
cargo test admin::profit::tests::ledger_report_ -- --nocapture
```

Expected: FAIL。

- [ ] **Step 3: 扩展 NewAPI 和报告类型**

给 `NewapiLogItem` 增加 `channel_id`；给 `ProfitReport` 增加设计规格中的 attributed/unattributed、观察 ID 与 `ledger_scope_confirmed` 字段。新增 `LedgerMatchedRow`，把“识别范围”和“汇总”拆为两个纯函数，避免在 handler 内写业务算法。

- [ ] **Step 4: 实现两阶段聚合**

`aggregate_ledger_report` 必须：

1. 优先用 `UsageRecord.trace_id` 匹配；缺失时用 `ProfitTraceRecord`；
2. 从匹配项收集 `channel_id` 与 `key_id`；
3. 只统计观察到的 RS 渠道收入；
4. 只统计观察到的 RS Key usage 成本；
5. 所有 `credits > 0` 均计入成本，不因 `status=error` 丢弃；
6. `profit = revenue - cost`，未确认范围时利润 fail closed；
7. 分组表只使用精确归属行。

- [ ] **Step 5: 运行全部利润纯函数测试**

```powershell
cargo test admin::profit::tests -- --nocapture
```

Expected: PASS。

- [ ] **Step 6: 提交利润算法**

```powershell
git add -- src/admin/profit.rs
git commit -m "fix(profit): 按真实账本计算总成本"
```

### Task 4: 接通利润 API 的 usage 范围读取

**Files:**
- Modify: `src/admin/service.rs`
- Modify: `src/admin/handlers.rs`
- Test: `src/admin/handlers.rs`

- [ ] **Step 1: 写 handler 使用 usage 账本失败测试**

扩展利润 handler 测试，创建临时 `UsageRecorder`，写入一条无 trace 的旧成本和一条带 trace 的新成本，断言 JSON 响应：

```rust
assert_eq!(body["ledgerScopeConfirmed"], true);
assert_eq!(body["credits"], 10.0);
assert_eq!(body["unattributedCredits"], 9.0);
assert_eq!(body["observedChannelIds"], serde_json::json!([19]));
```

- [ ] **Step 2: 运行测试确认 handler 尚未读取 usage**

```powershell
cargo test admin::handlers::tests::profit_report_uses_usage_ledger -- --nocapture
```

Expected: FAIL。

- [ ] **Step 3: 暴露 AdminService 查询接口并接线**

在 `AdminService` 新增：

```rust
pub fn query_usage_records(
    &self,
    start_epoch: i64,
    end_epoch: i64,
) -> Result<UsageRangeRead, AdminServiceError>
```

没有 recorder 时返回明确 `InternalError("usage 计费账本未启用")`。利润 handler 在 blocking task 中读取 usage，再调用 `aggregate_ledger_report`。坏行数量写入响应 warning 字段或服务端 warning 日志，不静默忽略。

- [ ] **Step 4: 运行 handler、service 与路由测试**

```powershell
cargo test admin::handlers::tests::profit_report -- --nocapture
cargo test admin::service::tests -- --nocapture
cargo test admin::router::tests::profit_report -- --nocapture
```

Expected: PASS。

- [ ] **Step 5: 提交 API 接线**

```powershell
git add -- src/admin/service.rs src/admin/handlers.rs
git commit -m "fix(profit): 接通真实用量账本"
```

### Task 5: 更新管理端利润口径与警示

**Files:**
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/components/profit-page.tsx`
- Modify: `admin-ui/src/components/profit-page-ui.contract.test.ts`

- [ ] **Step 1: 写 UI 合同失败测试**

新增断言：

```ts
test('利润页展示真实账本与未归属成本', async () => {
  const page = await readSource('src/components/profit-page.tsx')
  const types = await readSource('src/types/api.ts')
  expect(types).toContain('unattributedCost')
  expect(types).toContain('ledgerScopeConfirmed')
  expect(page).toContain('未归属成本')
  expect(page).toContain('未归属 Credits')
  expect(page).toContain('顶部总成本来自 RS 实际 metering 账本')
  expect(page).toContain('范围未确认')
  expect(page).toContain('归属率')
})
```

- [ ] **Step 2: 运行测试确认 UI 字段缺失**

```powershell
Set-Location admin-ui
bun test src/components/profit-page-ui.contract.test.ts
```

Expected: FAIL。

- [ ] **Step 3: 扩展类型和展示**

前端 `ProfitReport` 增加后端同名 camelCase 字段。KPI 将“匹配率”改为“归属率”；警示条展示未归属收入、Credits、成本。`ledgerScopeConfirmed=false` 时显示红色阻断卡片并把利润结论标记为不可用，避免继续展示误导数据。

- [ ] **Step 4: 运行前端测试与构建**

```powershell
bun test
bun run build
```

Expected: PASS。

- [ ] **Step 5: 提交前端展示**

```powershell
git add -- admin-ui/src/types/api.ts admin-ui/src/components/profit-page.tsx admin-ui/src/components/profit-page-ui.contract.test.ts
git commit -m "fix(admin): 展示真实利润与未归属成本"
```

### Task 6: 全量验证与本地合并

**Files:**
- Verify only.

- [ ] **Step 1: 运行 Rust 全量测试**

```powershell
cargo test --all-targets --all-features
```

Expected: 0 failures。

- [ ] **Step 2: 运行前端全量测试和生产构建**

```powershell
Set-Location admin-ui
bun test
bun run build
```

Expected: 0 failures，构建退出码 0。

- [ ] **Step 3: 审查差异**

```powershell
git diff master --stat
git diff master --check
git status --short
```

Expected: 仅包含设计、计划、usage 账本、利润后端和利润前端文件，无凭据、构建产物或无关修改。

- [ ] **Step 4: 本地合并并复验重点测试**

在主工作树执行：

```powershell
git merge --no-ff fix/profit-ledger-accuracy -m "fix(profit): 修复利润成本漏算"
cargo test admin::profit::tests -- --nocapture
Set-Location admin-ui
bun test src/components/profit-page-ui.contract.test.ts
```

Expected: 合并成功，重点测试全部 PASS，不执行远程 push。
