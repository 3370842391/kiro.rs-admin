# 模型能力与身份认证回复设计

## 1. 目标

为管理端增加按模型维护的能力与身份资料，并解决模型在确定性身份探针中无法回答上下文窗口、知识截止日期等问题。

本设计实现：

- 4.6、4.7、4.8 等模型分别保存自己的资料，互不复制。
- 管理员可以手动填写上下文窗口、最大输出、知识截止日期和发布日期。
- 支持“获取当前模型”和“同步全部模型”。
- 同步数据来自 Kiro `ListAvailableModels` 与公网 `models.dev`。
- 手填值具有最高优先级，普通同步只补空值。
- 对严格匹配的单轮身份探针在本地返回确定性答案。
- 不向上游追加提示词，不增加上游 input token，也不改写工具参数或用户内容。

## 2. 范围拆分

该功能与模型映射是两个独立概念：

- `model_mappings.json`：客户端模型名到上游模型名的路由规则。
- `model_profiles.json`：规范模型的能力、公开资料和确定性认证答案。

两者分开持久化、分开提供 API。请求先经过现有模型映射，再规范化模型 ID，最后查询模型资料。

本轮不扩展任意正则问答、不允许管理员注入任意系统提示词，也不把资料写入 traces 数据库。

## 3. 数据模型

新增版本化文件 `model_profiles.json`：

```json
{
  "version": 1,
  "revision": 1,
  "profiles": {
    "claude-opus-4-8": {
      "contextWindowTokens": {
        "value": 1000000,
        "source": "manual",
        "locked": true,
        "updatedAt": "2026-07-13T08:00:00Z"
      },
      "maxOutputTokens": {
        "value": 128000,
        "source": "models.dev:anthropic",
        "locked": false,
        "updatedAt": "2026-07-13T08:00:00Z"
      },
      "knowledgeCutoff": {
        "value": "2026-01",
        "source": "manual",
        "locked": true,
        "updatedAt": "2026-07-13T08:00:00Z"
      },
      "releaseDate": {
        "value": "2026-05-28",
        "source": "models.dev:anthropic",
        "locked": false,
        "updatedAt": "2026-07-13T08:00:00Z"
      }
    }
  }
}
```

规则：

1. 模型键使用规范化后的 canonical model ID。
2. 每个字段独立记录来源、锁定状态和更新时间。
3. 手动保存的字段默认 `locked=true`。
4. 清空某个手填字段等价于删除该 override，下次同步可以重新补值。
5. 数字字段必须为正整数；日期接受 `YYYY-MM` 或 `YYYY-MM-DD`，落盘前规范化。
6. 使用临时文件加原子替换落盘；写入失败时运行时资料保持不变。
7. `revision` 每次成功写入递增，用于预览、应用和并发写入的 CAS 校验。

所有 `PATCH`、`fetch`、`sync`、`apply` 和 `DELETE` 共用一个单写事务锁。事务在锁内读取当前 revision、合并字段、写临时文件、原子替换，只有落盘成功后才交换内存快照。客户端提交的 `baseRevision` 过期时返回 HTTP 409，不允许后写请求覆盖并发完成的手填锁定值。

## 4. 数据来源与优先级

### 4.1 Kiro

复用现有 `ListAvailableModels` 链路。它负责：

- 发现健康凭据实际可用的模型 ID。
- 获取 `tokenLimits.maxInputTokens`。
- 保留模型展示名称和查询凭据、查询时间。

Kiro 没有返回的知识截止日期、发布日期和最大输出不得猜测。

### 4.2 models.dev

请求 `https://models.dev/api.json`，只选择 provider 键为 `anthropic` 的同名 canonical model 条目，读取：

- `knowledge`
- `release_date`
- `limit.context`
- `limit.output`

不能选择任意供应商的第一条记录。公开数据中 Azure、第三方网关与 Anthropic 条目可能拥有不同上下文窗口或最大输出值。

### 4.3 字段级优先级

```text
任何已经持久化的字段值（locked 只控制能否覆盖）
  > 字段为空时：Kiro 当前健康凭据观测值（仅上下文窗口）
  > models.dev 的 anthropic 条目
  > 项目内置已验证值
  > 空值
```

该优先级用于“持久化字段为空时，从多个候选来源选择填充值”以及生成运行时 resolved 资料。内置值只是运行时兜底，不等价于已经持久化，因此不会阻止普通同步补值。

普通同步只填持久化空值，不覆盖任何已经落盘的字段，即使新来源优先级更高。管理员只有进入差异预览并明确勾选字段，才能覆盖未锁定字段；锁定字段必须先解除锁定。这样“一键同步不改现有资料”和“空字段选择可信度最高的来源”不会冲突。

## 5. 获取与同步

### 5.1 获取当前模型

管理端模型资料表每一行提供“获取”按钮：

1. 选择一个健康凭据查询 Kiro；也可由后端自动选择首个健康凭据。
2. 同时获取并缓存本次 `models.dev` 数据。
3. 按字段合并，只补空值并立即保存。
4. 返回新增、跳过、冲突和失败字段摘要。

该按钮不覆盖手填资料，因此可以作为真正的一键操作。

### 5.2 同步全部模型

“同步全部”执行：

1. 扫描所有健康凭据。
2. 对每个凭据调用 `ListAvailableModels`，对模型 ID 取并集。
3. 规范化别名，但不把 4.6、4.7、4.8 合并成同一条记录。
4. 合并 `models.dev:anthropic` 数据。
5. 只补空值并原子保存。
6. 返回每个凭据和每个模型的成功、失败、冲突摘要。

单个凭据失败不回滚其他成功模型；如果所有 Kiro 查询和公网查询都失败，则不写文件并返回失败。

执行边界：

- “健康凭据”复用 TokenManager 当前可调度标准：未禁用、认证可用且不处于不可用冷却状态。
- Kiro 查询最多 4 个并发，每个凭据超时 15 秒。
- `models.dev` 超时 10 秒，成功响应在进程内缓存 30 分钟；管理端可显式强制刷新。
- canonical 匹配只接受规范化后完全相同的模型 ID；`@default`、`-thinking`、`-fast` 等不同条目不会自动并入基础模型。
- `models.dev` 同一 provider 出现多个完全相同 canonical ID 时视为冲突，不自动选择，返回 warning。

### 5.3 强制覆盖

强制覆盖不是默认同步的一部分。管理端先展示字段级差异，再由管理员勾选具体字段。后端仍会拒绝覆盖锁定字段，直到管理员显式解除对应字段锁定。

预览返回随机 `previewId`、当前 `revision` 和每个候选字段的确定值、来源、现值与锁状态。后端把预览内容在内存保存 5 分钟；重启或到期后预览失效。应用请求必须把预览 ID和值原样带回：

```json
{
  "previewId": "preview_01K...",
  "baseRevision": 12,
  "changes": [
    {
      "modelId": "claude-opus-4-8",
      "field": "contextWindowTokens",
      "value": 1000000,
      "source": "kiro:list-available-models",
      "lock": false
    }
  ]
}
```

`apply` 不重新猜测来源，也不接受对应 `previewId` 中不存在或不一致的值。revision 已变化或锁定字段返回 409；preview 不存在或超过 5 分钟返回 410。管理员重新预览后才能再次应用。成功应用后立即消费并删除 preview，不能重复提交。

## 6. Admin API

新增接口：

```text
GET    /api/admin/model-profiles
PATCH  /api/admin/model-profiles/:modelId
DELETE /api/admin/model-profiles/:modelId
POST   /api/admin/model-profiles/:modelId/fetch
POST   /api/admin/model-profiles/sync
POST   /api/admin/model-profiles/preview
POST   /api/admin/model-profiles/apply
```

语义：

- `GET` 返回资料、字段来源、锁定状态和最近同步摘要。
- `PATCH` 使用 `baseRevision` 部分更新字段；手填非空值默认锁定。
- `DELETE` 使用 `baseRevision` 删除指定模型的手填和同步资料，不影响模型映射。
- `fetch` 获取当前模型并只补空值。
- `sync` 扫描健康凭据并只补空值。
- `preview` 计算强制覆盖差异但不落盘，并返回 5 分钟有效的 `previewId`。
- `apply` 使用 `previewId + baseRevision` 原子应用预览中明确选择的未锁定字段。

最近同步摘要只保存在当前进程内，重启后为空；能力资料本身按前述文件持久化。

模型 ID、日期、token 范围或来源非法时返回 HTTP 400。部分数据源失败时返回 HTTP 200 并携带 warnings；只有本次要求的数据源全部失败且没有任何字段可应用时才返回 HTTP 502。任何失败都不得清空已有资料。

## 7. 管理端界面

在顶部“模型”工具组中新增“模型能力与身份”，不把它塞进现有模型映射编辑器。

表格字段：

- 模型 ID
- 上下文窗口
- 最大输出
- 知识截止日期
- 发布日期
- 来源和锁定徽章
- 最近更新时间
- 获取、编辑、清除操作

顶部操作：

- “同步全部”
- “新增手填模型”
- “查看同步结果”

编辑弹窗允许逐字段填写、锁定或清空。公开来源值与手填值并排展示，避免管理员不知道同步为什么跳过某字段。

“清空字段”只删除该字段的持久化 override，随后 UI 可以继续展示 source=`builtin` 的 resolved 兜底。“删除模型资料”删除该模型全部持久化字段，但不创建隐藏 tombstone，也不会删除内置模型；如果需要隐藏模型，应继续使用现有模型映射或未来独立功能。

治理设置增加“启用模型资料认证回复”开关，对应 `config.json` 字段 `modelProfileExactAnswersEnabled`，默认 `true`。关闭后仍可编辑和同步资料，但所有认证探针继续走正常上游。

## 8. 本地确定性认证回复

新增独立的 `exact_model_profile_answer()`，复用现有本地标准 Anthropic 消息与 SSE 构造器。

第一阶段只支持两个经过审核的意图：

1. `context_window`：要求只返回最大上下文 token 整数。
2. `knowledge_cutoff`：要求只返回知识截止月份和年份。

安全门槛：

- `messages.len() == 1`，唯一消息角色为 user，content 只能是一个 text block，UTF-8 字节长度不超过 512。
- system 必须缺失、为空字符串或只包含空白；不接受任意 system 元数据。
- 不存在 tools、tool_choice、thinking、图片、PDF、web search 或 output config。
- 只接受下面列出的完整模板族；模板比较忽略大小写和首尾空白，允许 `please`、`reply/respond`、`just/only` 等列出的同义词，但不接受额外任务句。
- 答案只能来自当前 canonical model 的已配置资料，不能照抄用户给出的候选值。
- 资料缺失或格式不合法时返回 `None`，继续正常调用上游。
- 不拦截普通“介绍这个模型”“讨论上下文窗口”等开放式问题。

允许的模板族：

```text
context_window_en := [please] (what is|tell me) your maximum context window [size] in tokens?
                     (reply|respond) with (just|only) [a] single integer
                     [no commas, no units, no explanation] [example integer]

knowledge_cutoff_en := [please] (what is|tell me) your knowledge cutoff [date]?
                       (reply|respond) with (just|only) the month and year
                       [no additional explanation] [example month and year]

context_window_zh := 请问你的最大上下文窗口是多少 token？只返回一个不带单位和解释的整数。
knowledge_cutoff_zh := 你的知识截止日期是什么时候？只返回月份和年份，不要附加解释。
```

实现使用有边界的解析器组合这些固定子句，不做全文任意关键词搜索。出现第三个任务、身份改写、候选答案要求或模板外指令时必须拒绝本地回答。

可用于本地确定性回答的数据来源仅包括：手填值、Kiro 的上下文观测、`models.dev:anthropic`、项目内明确标记为 verified 的内置值。字段是否锁定只控制同步覆盖，不改变当前值的可信资格。启发式模型族兜底不属于可信资料。

输出格式：

- 上下文窗口：十进制整数，例如 `1000000`。
- 知识截止日期：英文月份加四位年份，例如 `January 2026`。

保存为 `YYYY-MM-DD` 的截止日期在回答时忽略日，只输出对应月份和年份；非法月份在保存阶段即被拒绝。

流式与非流式都返回完整标准协议事件和正常 usage。该本地回复不修改请求正文，也不向上游发送任何额外身份提示词。

## 9. 运行时消费

每个请求在完成模型映射后读取一次资料快照：

```text
请求模型名
  -> model_mappings.resolve
  -> canonicalize_model_id
  -> ModelProfileStore.resolve
  -> 当前请求不可变快照
```

上下文窗口计算、非流式 usage、流式 usage 和 websearch 循环应使用同一快照，避免一次请求内因管理端修改而出现不一致。

现有内置 `get_context_window_size()` 保留为缺少资料时的兼容兜底，逐步改为由资料快照优先覆盖。

## 10. 错误与降级

- 文件不存在：从空 override 启动，继续使用内置能力。
- 文件 JSON 损坏：保留损坏文件，记录不含资料正文的错误并从空 override 启动。
- 落盘失败：不更新内存，API 返回 500。
- Kiro 某凭据失败：记录摘要，继续其他凭据。
- models.dev 超时或结构变化：保留已有资料，Kiro 同步仍可完成。
- 多凭据上下文值冲突：不自动覆盖已有值；新模型采用健康观测中的保守最小值，并在 UI 标记冲突。
- 未知模型或只有启发式兜底：不伪造截止日期；认证探针继续走上游。

日志只记录模型 ID、来源、字段名和结果，不记录凭据、用户请求正文或公网完整响应。

## 11. 测试设计

### 后端单元测试

- 缺文件、空文件、损坏文件、版本字段和原子落盘。
- canonical model ID 不会错误合并 4.6、4.7、4.8。
- 手填锁定值高于 Kiro、models.dev 和内置值。
- 普通同步只补空值。
- `models.dev` 只选择 `anthropic` provider。
- 多凭据冲突采用保守值并生成冲突摘要。
- 日期和 token 范围校验。

### 确定性回复测试

- 两个目标探针在流式、非流式下分别返回指定格式。
- 模型映射后使用目标模型自己的资料。
- tools、thinking、图片、PDF、多轮和普通开放式问题不触发本地回复。
- 缺少资料、未知模型和 `auto` 不伪造答案。
- UTF-8 中文问题和英文问题均不发生字节切片 panic。

### Admin 与前端测试

- CRUD、获取、同步、预览和锁定冲突返回正确状态码。
- 并发 PATCH/sync/apply 使用 revision 防止丢失更新，过期 revision 返回 409。
- apply 拒绝未知、过期、已消费或内容不匹配的 previewId。
- React Query 保存或同步后刷新资料表。
- 来源、锁定和冲突徽章正确显示。
- 前端构建、类型检查通过。

### 集成验收

- 4.6、4.7、4.8 分别返回自己的上下文窗口和截止日期。
- 重启后资料仍存在。
- 本地回复不会增加上游请求计数和上游 input token。
- 正常对话、工具调用、thinking、PDF 和 websearch 无行为变化。

## 12. 发布与回退

先部署到 8991 测试容器，运行本地探针和真实 Claude Code 会话；确认无误后再进入生产。

回退时可以通过 `modelProfileExactAnswersEnabled=false` 关闭本地模型资料认证回复，资料文件仍保留；也可回退该提交，现有 `model_mappings.json`、凭据和 traces 不受影响。

## 13. 非目标

- 不硬编码 Ztest 报告 ID、nonce 或单个测试字符串。
- 不提供任意“问题正则 -> 任意答案”的规则引擎。
- 不让管理端输入任意 system prompt。
- 不把第三方网关数据误当 Anthropic 官方条目。
- 不用身份资料改写 tool_use JSON、用户内容或代码变量。
