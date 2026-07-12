# Thinking 降级与 Docker 发布可靠性设计

## 背景

生产容器在 Kiro 上游已返回 HTTP 200 和有效正文后，因为响应缺少
`thinking`/`redacted_thinking`，被中转层改判为协议错误，导致 Opus 4.6、4.7
出现大量 502。生产日志还在 `identity.rs` 的 UTF-8 字节边界处 panic，而当前
master 已包含字符边界修复，证明生产 `beta` 镜像并非当前源码的可靠构建。

Ztest 报告 `01KXAHA6MA1NGRJP56JQHQGXYF` 得分 74。D10、D17、D19、S5 已通过；
主要剩余项是 D5 Canary、D7 结构化输出、S3 system 服从率和 Kiro 身份痕迹。

## 目标

1. 缺少真实 thinking 时保留有效正文和工具调用，不再制造 502。
2. 不生成、补写或伪造 thinking 内容。
3. 让生产 Docker 镜像能追溯到明确 Git commit，避免 `beta` 指向旧代码。
4. 部署当前 master 后重新验证 D5、D7、S3，只修复通用协议或数据流问题。
5. 保持 UTF-8、流式工具调用、PDF、usage 和 SSE 事件顺序的现有兼容性。

## 非目标与安全边界

- 不识别 Ztest 报告 ID、nonce 或固定提示词。
- 不伪造 token usage、缓存命中率、模型身份或上游 reasoning。
- 不承诺固定分数；分数只作为通用协议回归的外部观测。
- 不通过全量文本替换修改用户输入、工具 JSON 或工具结果。

## 方案

### 1. Thinking 缺失采用诚实降级

新增配置项控制缺少 reasoning 时的处理策略：

- 默认兼容模式：上游有正文或工具调用时正常返回，只记录结构化警告。
- 严格模式：维持当前协议错误行为，供专门的合规测试使用。
- 上游完全没有正文、工具调用或 reasoning 时，仍返回空响应错误。

非流式响应直接保留原始 content。流式响应继续发送已经生成的内容块、
`message_delta` 和 `message_stop`，不得在流尾追加 error 事件。

### 2. Docker 构建可追溯

调整 Docker 工作流：

- master 推送即使 commit 同时带 release tag，也必须生成对应 beta 镜像。
- 镜像写入 `org.opencontainers.image.revision`、version 和 created 标签。
- 同时发布不可变 commit 标签，部署时优先使用该标签或 digest。
- release 流程发布语义化版本 Docker 标签，避免只有二进制 Release、没有容器。

### 3. 分数回归策略

先部署包含 UTF-8、tool_choice、thinking、PDF 和动态模型校验的当前 master，之后
重新运行本地探针、ARA 和 Ztest。D7 若随正确镜像部署恢复，不再追加修改。

对 D5/S3 只接受以下通用改进：

- 修复并发请求串扰、历史拼接、缓存污染或 system 内容丢失。
- 在 Kiro 无原生 system 槽的约束下，提高普通 system 指令的稳定传递。
- 保持用户 system 原文，不附加检测专用指令。

如果新报告仍只剩 Kiro 上游自身拒答或身份包装，则如实记录为上游限制，不通过
响应伪造掩盖。

## 测试与验收

### 自动化测试

- 非流式：请求 thinking、上游仅返回 text 时成功，不产生伪 thinking。
- 流式：请求 thinking、上游仅返回 text/tool_use 时完整结束，无 error 尾帧。
- 严格模式：缺 reasoning 时仍返回原有协议错误。
- 空响应：兼容模式下仍失败。
- UTF-8：中文跨 chunk 不 panic。
- Docker 工作流：tagged master 不再跳过 beta；标签包含 commit revision。

### 运行时验收

- 本地 `anthropic_probe` 的 thinking、tool_choice、parallel_canary、stream 全通过。
- 连续执行流式工具调用并回传 `tool_result`，对话正常以 `end_turn` 结束。
- 服务器日志不再出现 `identity.rs` UTF-8 panic。
- Opus 4.6/4.7 缺 thinking 的有效响应不再计为请求失败。
- 重新生成外部报告，逐项记录改善与仍受上游限制的项目。

## 发布与回滚

先构建不可变 commit 镜像，再在服务器拉取并重建单个 `kiro-rs-admin` 容器。
保留旧镜像 digest；若健康检查、工具调用或错误率异常，立即回滚旧 digest。
配置文件、凭据文件和 usage 数据不进入镜像或 Git 提交。
