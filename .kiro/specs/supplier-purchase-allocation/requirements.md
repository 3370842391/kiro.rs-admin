# Requirements Document

## Introduction

当前多供货商采购是**每家各自独立**的：每家有自己的 webhook、自己的 `minPurchase` / `maxPurchase`、自己的补货闸（`restockOnlyWhenExhausted` 只统计该家名下凭据）。三家同时推「到货了」，三家会各自判定「该补货了」，各买一批。全库唯一的全局上限是 `MAX_SUPPLIERS = 32`，**不存在任何跨供货商的数量或金额封顶**。

本特性引入一个**全局号池目标存量 N**：所有采购来的可用凭据合计不得超过 N。任一供货商推来到货通知时，系统按「N 减去当前全局可用数」算出缺口，只向**推送方那一家**下单补齐缺口；缺口为 0 就不买。供货商之间**不设优先级**，谁的到货事件先被处理谁先消耗缺口额度——现有 `claim_next` 已是全局 FIFO（`ORDER BY id ASC`），先到先得是它的天然行为，本特性不改动排序。

为什么 N 必须是存量而不是「每次触发上限」：先到先得的语义下，若 N 是每次触发的上限，三家各推一次就买 3N，与逐家把 `maxPurchase` 设成 N 完全等价，等于没做这个功能。存量口径下三家抢的是同一个缺口，N=3 时池子稳定保持 3 个号、三家先后补位，长期表现即用户所述的「轮询」——这不是配置出来的轮询，是先到先得叠加固定水位的自然结果。

**「哪些号算在池子里」是本特性最容易出错的地方，因此单独立为一条需求。** 已发布版本（v0.9.40）的采购流程只写 `source_channel`（默认「Webhook 自动采购」）与昵称前缀，**不写 `supplier_id`**——那个字段是本轮多供货商改造才新增、尚未发版。若只按 `supplier_id` 统计，升级瞬间所有现存采购号都不被计入，缺口等于目标存量全额，会立刻重复买一批。因此识别规则必须同时覆盖新版的 `supplier_id` 与旧版的备注。

这是**花钱的路径**。因此本文档把「任意一次触发后全局可用数不超过 N」当作硬不变式，把「配置缺失或非法时宁可少买不可多买」当作默认失效方向，并要求重放与重启路径全程幂等。

本特性**不修**已知的相关缺陷（事件队头阻塞、无退避重试、事件表零清理、`config.json` 全量读改写无共享锁），但所有需求都必须与这些缺陷共存而不放大它们。

## Glossary

- **Pool_Config**：全局号池采购配置。保存在 `Config` 顶层（与 `keySuppliers` 并列），全实例一份，不是每家一份。
- **Pool_Target**：Pool_Config 中的全局号池目标存量 N，单位为「个 key」。
- **Purchased_Credential**：由自动采购流程导入的凭据，**不区分来自哪一家供货商**。判定为两级：`supplier_id` 非空（新版采购写入的机器可判定标记）；或 `supplier_id` 为空但 `source_channel` 与任一已配置供货商的 `sourceChannel` 完全相等（升级前由旧版采购导入的遗留凭据，那时 `supplier_id` 字段尚不存在）。
- **Legacy_Purchased_Credential**：上述第二级判定命中的凭据，即只能靠备注认出来的旧版采购号。区分这一类不是为了归属到某家，而是因为备注匹配有已知弱点（`sourceChannel` 被改过就不再命中），分开计数才能在失效时看出来。
- **Global_Usable_Count**：当前全部 Purchased_Credential 中**可用**者的合计数量。可用性判定沿用现有 `SupplierCredentialHealth` 的 `usable` 语义：**已判死的号不算**，额度耗尽的不算，剩余额度跌到水位以下的不算；手动禁用仍算可用（那是人主动暂停）。
- **Deficit**：缺口，等于 `Pool_Target - Global_Usable_Count`，下界截断到 0。
- **Trigger_Event**：触发一次补货判定的事件。本特性只承认 `new_keys_available` 这一种自动触发来源。
- **Trigger_Supplier**：推送 Trigger_Event 的那一家供货商，也是本次唯一的下单对象。
- **Pool_Engine**：识别 Purchased_Credential、计算 Global_Usable_Count 与 Deficit、决定本次采购量的组件（新增）。
- **Purchase_Executor**：执行单家单笔采购与导入的现有组件（`execute_claimed` 所在链路）。
- **Global_Restock_Gate**：全局补货闸。以 Global_Usable_Count 与 Pool_Config 中的目标存量比较，取代现有的逐家 `restockOnlyWhenExhausted` 判定。
- **Unfulfillable_Signal**：明确表示「这家这次买不到」的信号，包含 `OutOfStock`、`InsufficientBalance`、`OrderConflict`、供货商 API 故障。
- **Event_Store**：供货商事件库（`supplier_events` 表及其访问层）。
- **Quota_Source**：余额数据源（`AdminService` 的余额缓存，后台每 5 分钟刷新），供额度水位判定使用。
- **Admin_UI**：`admin-ui` 管理端。
- **Config_Store**：`config.json` 的读写层（`Config::load` / `save` / `persist_suppliers`）。

## Requirements

### Requirement 1: 全局号池配置的读写与校验

**User Story:** 作为运维管理员，我想在一个地方设置「号池一共养几个号」，这样我不必逐家去调 `maxPurchase` 并心算三家加起来会买成多少。

#### Acceptance Criteria

1. THE Pool_Config SHALL 包含 `enabled`（布尔）、`targetCount`（Pool_Target）、`lowQuotaThreshold`（额度水位）三个字段。
2. WHERE Pool_Config 在 `config.json` 中缺失，THE Config_Store SHALL 采用 `enabled = false` 的默认值，使系统行为与本特性上线前完全一致。
3. WHEN 管理员提交 `enabled = true` 且 `targetCount` 不在 1..=10000 范围内的配置，THE Pool_Engine SHALL 拒绝该配置并返回说明取值范围的校验错误。
4. WHEN 管理员提交 `lowQuotaThreshold` 不在 0..=100000 范围内的配置，THE Pool_Engine SHALL 拒绝该配置并返回说明取值范围的校验错误。
5. WHEN 管理员通过 `GET /api/admin/key-supplier/pool` 读取配置，THE Pool_Engine SHALL 返回当前 Pool_Config 的全部字段值。
6. WHEN 管理员通过 `PUT /api/admin/key-supplier/pool` 写入合法配置，THE Config_Store SHALL 将该配置持久化到 `config.json` 顶层并使后续触发读取到新值。
7. THE Config_Store SHALL 使写入 Pool_Config 的操作与现有 `persist_suppliers` 走同一条 `config.json` 读改写路径并共用同一把锁，避免两者互相覆盖。
8. IF `config.json` 中的 Pool_Config 存在但校验失败，THEN THE Pool_Engine SHALL 在启动时记录一条 error 级日志，并对后续每次自动触发返回「配置非法，跳过采购」的 skipped 结果。
9. IF Pool_Config 中任一数值字段解析失败，THEN THE Pool_Engine SHALL 跳过本次采购，而不采用字段默认值继续下单。

### Requirement 2: 自动采购凭据的识别

**User Story:** 作为运维管理员，我想让水位统计所有自动采购来的号、不管是哪家买的，这样我手工放进池子的常驻号不会把缺口顶掉，而升级前买的那批号也不会被漏掉导致重复买。

#### Acceptance Criteria

1. THE Pool_Engine SHALL 只判定「该凭据是否由自动采购导入」，SHALL NOT 在水位统计中区分凭据来自哪一家供货商。
2. THE Pool_Engine SHALL 把 `supplier_id` 非空的凭据判定为 Purchased_Credential，无论该 id 是否仍存在于当前配置中。
3. THE Pool_Engine SHALL 把 `supplier_id` 为空、且 `source_channel` 与任一已配置供货商的 `sourceChannel` 完全相等的凭据判定为 Legacy_Purchased_Credential，并将其计入 Purchased_Credential。
4. THE Pool_Engine SHALL 在第 3 条的匹配中要求字符串完全相等，SHALL NOT 使用前缀匹配、子串匹配或大小写不敏感匹配。
5. THE Pool_Engine SHALL 排除 `supplier_id` 为空且 `source_channel` 不匹配任何已配置 `sourceChannel` 的凭据（手动添加、批量导入的自定义备注）使其不计入 Global_Usable_Count。
6. THE Pool_Engine SHALL 排除 `source_channel` 与 `supplier_id` 均为空的凭据。
7. WHERE 某已配置供货商的 `sourceChannel` 为空字符串，THE Pool_Engine SHALL NOT 用该空值参与第 3 条的匹配，避免把所有无备注凭据一并算入。
8. THE Pool_Engine SHALL 把已禁用供货商名下的凭据计入——它们仍在承载流量，不算就会超买。
9. WHERE 某供货商已从配置中删除，THE Pool_Engine SHALL NOT 再用该家的 `sourceChannel` 参与第 3 条的匹配（配置已不存在，无从取值），但其名下带 `supplier_id` 的凭据仍按第 2 条计入。

### Requirement 3: 全局可用数统计

**User Story:** 作为运维管理员，我想让「还差几个」这件事按整个号池算，这样某一家的号全死了但池子还满的时候不会白买一批。

#### Acceptance Criteria

1. THE Pool_Engine SHALL 通过统计全部 Purchased_Credential 中可用者的数量得出 Global_Usable_Count。
2. THE Pool_Engine SHALL 把已判死（`died_at` 非空）的凭据计为不可用，使其不占用目标存量额度。
3. THE Pool_Engine SHALL 把已判死但仍在保留期内、尚未被清理删除的凭据同样计为不可用——它们还留在池子里，但对流量而言已经是废号。
4. THE Pool_Engine SHALL 把额度耗尽（`quota_exhausted_at` 非空）的凭据计为不可用。
5. WHERE `lowQuotaThreshold` 大于 0，THE Pool_Engine SHALL 把剩余额度小于或等于该阈值的凭据计为不可用。
6. WHERE Quota_Source 查不到某凭据的剩余额度，THE Pool_Engine SHALL 把该凭据计为可用，沿用现有「缺数据时宁可少买」的方向。
7. THE Pool_Engine SHALL 把手动禁用的凭据计为可用，沿用现有语义——那是人主动暂停，不该触发采购。
8. THE Pool_Engine SHALL 在每次触发时重新统计 Global_Usable_Count，SHALL NOT 缓存上一次的结果。
9. THE Pool_Engine SHALL 在统计时先在凭据锁内收集判定所需的最小字段，再在锁外查询剩余额度，避免在持有凭据锁时调用余额数据源造成死锁。

### Requirement 4: 缺口计算与采购量夹逼

**User Story:** 作为运维管理员，我想让每次到货只补到我设的存量就停，这样池子不会越滚越大。

#### Acceptance Criteria

1. THE Pool_Engine SHALL 把 Deficit 计算为 `Pool_Target - Global_Usable_Count`，并把负值截断为 0。
2. IF Deficit 为 0，THEN THE Pool_Engine SHALL 跳过本次采购并把 Trigger_Event 标记为 skipped，原因为「号池已达目标存量」。
3. THE Pool_Engine SHALL 把本次采购量夹逼到不超过 Deficit。
4. THE Pool_Engine SHALL 把本次采购量夹逼到不超过 Trigger_Supplier 的 `maxPurchase`。
5. WHERE Trigger_Supplier 的 `kind` 为 `kiro-rs`，THE Pool_Engine SHALL 先查询该家可用库存并把采购量夹逼到不超过该库存数。
6. WHERE Trigger_Supplier 的 `kind` 为 `kiro-app` 或 `kiroapp-io`，THE Pool_Engine SHALL 跳过库存查询直接按夹逼后的数量下单，理由是查询与领取不在同一事务、多一次往返只会把货让给别人。
7. IF 夹逼后的采购量小于 Trigger_Supplier 的 `minPurchase`，THEN THE Pool_Engine SHALL 跳过本次采购并标记 skipped，且不将采购量放大到 `minPurchase`。
8. WHEN 本次采购因 `minPurchase` 被跳过，THE Pool_Engine SHALL 记录一条 info 级日志，包含 Trigger_Supplier 的 id、Deficit、夹逼后的采购量与该家 `minPurchase`。
9. THE Pool_Engine SHALL 忽略 Trigger_Event 携带的数量字段作为采购量依据，仅将该字段原样记录到 Event_Store 供对账。
10. IF Global_Usable_Count 已大于 Pool_Target，THEN THE Pool_Engine SHALL 跳过本次采购，且 SHALL NOT 删除、禁用或以任何方式处置多出的凭据。

### Requirement 5: 先到先得与串行消耗

**User Story:** 作为运维管理员，我想让先推来到货通知的那家先拿到订单，这样我不用配任何优先级，抢货快的自然拿得多。

#### Acceptance Criteria

1. THE Pool_Engine SHALL 只向 Trigger_Supplier 下单，SHALL NOT 向未推送本次到货通知的供货商下单。
2. THE Pool_Engine SHALL 沿用 Event_Store 现有的全局 FIFO 取件顺序（`claim_next` 按 `id ASC`）决定多家同时推送时的处理先后，SHALL NOT 引入供货商维度的排序或优先级配置。
3. WHILE 一次触发正在执行，THE Pool_Engine SHALL 串行处理后续触发，使任意时刻只有一次采购在下单（沿用现有 `processing_lock`）。
4. THE Purchase_Executor SHALL 使凭据导入在返回时即对后续的 Global_Usable_Count 统计可见，使同一轮内后续事件能看到已补上的号。
5. WHEN 两家在极短时间内先后推送到货且 Deficit 为 1，THE Pool_Engine SHALL 使先被处理的那家买到 1 个，后被处理的那家因 Deficit 归零而跳过。
6. WHEN Pool_Config 在一次触发执行过程中被修改，THE Pool_Engine SHALL 使用触发开始时读取的配置快照完成本次采购。
7. THE Pool_Engine SHALL NOT 为缺口未补齐的情况向其它供货商顺延下单——顺延依赖优先级顺序，与先到先得语义冲突；未补齐的缺口留给下一次到货推送。

### Requirement 6: 全局补货闸取代逐家补货闸

**User Story:** 作为运维管理员，我想只在一个地方设水位，这样我不用担心三家的水位配得不一致导致某家一直在买。

#### Acceptance Criteria

1. WHERE `enabled` 为真，THE Global_Restock_Gate SHALL 以 Global_Usable_Count 与 Pool_Target 的比较取代现有逐家 `restockOnlyWhenExhausted` 判定。
2. WHERE `enabled` 为真，THE Pool_Engine SHALL 忽略各家自己的 `restockOnlyWhenExhausted` 与 `restockUsableThreshold` 配置。
3. WHERE `enabled` 为真，THE Pool_Engine SHALL 使用 Pool_Config 的 `lowQuotaThreshold` 而非各家自己的同名字段。
4. WHERE `enabled` 为假，THE Purchase_Executor SHALL 保持现有逐家独立的触发与补货闸行为不变。
5. WHEN Trigger_Event 的类型不是 `new_keys_available`，THE Pool_Engine SHALL 只落库留痕而不做缺口计算与采购。
6. WHEN 管理员调用现有的单家手动采购 `POST /api/admin/key-suppliers/{id}/purchase`，THE Purchase_Executor SHALL 按管理员指定的数量执行采购，不受 Pool_Target 与 Global_Restock_Gate 约束。
7. WHEN 单家手动采购在 `enabled` 为真时执行，THE Event_Store SHALL 在该事件记录中标注该笔采购未经过号池引擎。

### Requirement 7: 超额保护与幂等

**User Story:** 作为运维管理员，我想确认任何重放、重试或重启都不会让我买超，这样我敢把自动采购一直开着。

#### Acceptance Criteria

1. THE Purchase_Executor SHALL 在向供货商发起采购前重新校验「本次采购量 <= 当次触发计算出的 Deficit」，校验不通过时放弃本笔请求。
2. THE Pool_Engine SHALL 沿用现有的确定性 `client_order_id` 派生逻辑（优先使用供货商提供的幂等键，否则由 `event_id` 派生），SHALL NOT 引入随机订单号。
3. WHEN 同一个 Trigger_Event 被供货商重复推送，THE Event_Store SHALL 按 `(supplier_id, event_id)` 判定为重复并跳过重复执行。
4. WHEN 一次触发因进程重启而被重新执行，THE Pool_Engine SHALL 派生出与首次执行相同的 `client_order_id`，并重新计算 Deficit（首次已导入的凭据会使 Deficit 相应减小）。
5. WHERE Trigger_Supplier 的 `kind` 为 `kiro-app`（采购无幂等键），THE Pool_Engine SHALL 对该家的采购请求禁用重试，沿用现有 `RetryPolicy::Never`。
6. IF Pool_Target 读取结果为 0 或不可解析，THEN THE Pool_Engine SHALL 跳过本次采购并记录 skipped，原因为「目标存量不可用」。
7. WHEN `kiroapp-io` 返回的成交量小于请求量（部分成交），THE Pool_Engine SHALL 接受该结果并把实际成交量计入导入流程，SHALL NOT 因差额再次下单。

### Requirement 8: 买不到时的处理

**User Story:** 作为运维管理员，我想区分「抢不到」和「出故障」，这样我不会为正常的竞争失败去查日志，也不会漏掉真的故障。

#### Acceptance Criteria

1. WHEN Trigger_Supplier 返回 `OutOfStock`，THE Pool_Engine SHALL 沿用现有语义把结果记为 skipped 并附原因「库存已被抢完」，而不是记为 failed。
2. WHEN Trigger_Supplier 返回 `InsufficientBalance`，THE Pool_Engine SHALL 沿用现有语义把结果记为 skipped 并附原因「供货商积分不足，需充值」。
3. WHEN Trigger_Supplier 返回 `OrderConflict`，THE Pool_Engine SHALL 沿用现有语义把结果记为 skipped 并记录一条 error 级日志说明积分已扣需人工核对。
4. IF Trigger_Supplier 因供货商 API 故障（超时、5xx、429）未能成交，THEN THE Pool_Engine SHALL 把结果记为 failed 并保留缺口给下一次到货推送。
5. WHEN 采购成功但部分凭据导入失败，THE Pool_Engine SHALL 沿用现有 `fail_with_summary` 路径落库，使 `total_debit` 等金额字段与水位快照都不被抹除。
6. THE Pool_Engine SHALL NOT 因任何 Unfulfillable_Signal 而在同一次触发内向其它供货商下单。

### Requirement 9: 可观测性与对账

**User Story:** 作为运维管理员，我想在事件列表里看清每次到货时池子里有多少号、缺多少、最终买了几个，这样「为什么没买」不用去翻代码。

#### Acceptance Criteria

1. THE Event_Store SHALL 为每次触发记录：触发时的 Global_Usable_Count、Deficit、夹逼后的请求量。
2. THE Event_Store SHALL 在跳过与失败路径上同样写入第 1 条的三个数值，SHALL NOT 只在成功时记录。
3. THE Event_Store SHALL 继续为每笔采购记录：供货商 id、成交量、导入量、重复量、失败量、`total_debit`、`unit_price`、`supplier_order_id`、`replayed`。
4. WHEN 一次触发被跳过，THE Event_Store SHALL 在事件的 message 字段中记录可区分的跳过原因（已达目标存量 / 低于单家下限 / 库存被抢完 / 积分不足 / 配置非法）。
5. THE Pool_Engine SHALL 在每次触发结束时记录一条 info 级日志，包含 Pool_Target、Global_Usable_Count、Deficit、Trigger_Supplier id、请求量、成交量与本次扣费。
6. WHEN 管理员请求 `GET /api/admin/key-supplier/pool/status`，THE Pool_Engine SHALL 返回当前 Pool_Target、Global_Usable_Count、Deficit 与四类不可用（判死 / 额度耗尽 / 额度低于水位 / 合计）的拆分，且不发起任何采购。
7. THE Pool_Engine SHALL 在状态响应中把按 `supplier_id` 识别的凭据数与按 `sourceChannel` 备注识别的 Legacy_Purchased_Credential 数分开列出，使备注匹配失效导致的计数下降可被识别。
8. THE Pool_Engine SHALL 在状态响应中列出当前参与备注匹配的 `sourceChannel` 集合，使「我买的号为什么没算进来」可自查。
9. THE Pool_Engine SHALL 使 `pool/status` 接口不产生任何写操作，使重复调用产生一致结果且不改变系统状态。
10. THE Pool_Engine SHALL 使日志与事件记录中的 `apiKey` 与 `webhookSecret` 字段值被脱敏，沿用现有 `sanitize_error` 语义。

### Requirement 10: 管理端配置界面

**User Story:** 作为运维管理员，我想在供货商页面填一个数字就完成配置，并能当场看到「现在池子里几个、还差几个、这些号是怎么认出来的」。

#### Acceptance Criteria

1. THE Admin_UI SHALL 在供货商页面提供一张全局号池卡片，包含启用开关、目标存量输入框、额度水位输入框。
2. THE Admin_UI SHALL 在该卡片中展示需求 9 第 6 条返回的当前 Global_Usable_Count、Deficit 与四类不可用拆分。
3. THE Admin_UI SHALL 区分显示按 `supplier_id` 识别与按备注识别两类凭据数。
4. WHERE 存在 Legacy_Purchased_Credential，THE Admin_UI SHALL 显示提示文案说明这些号靠 `sourceChannel` 备注识别，改动该备注会使其不再计入水位。
5. WHEN 管理员输入不在 1..=10000 范围内的目标存量，THE Admin_UI SHALL 显示取值范围提示并禁用保存按钮。
6. WHILE 全局号池启用，THE Admin_UI SHALL 在每家供货商的 `maxPurchase` 字段旁显示说明文案，指出全局目标存量优先、单家上限只作单笔安全边界。
7. WHILE 全局号池启用，THE Admin_UI SHALL 在每家供货商的补货闸设置旁显示说明文案，指出该设置在全局号池下不参与判定。
8. WHEN 管理员保存配置失败，THE Admin_UI SHALL 展示服务端返回的校验错误原文并保留已填内容。
9. THE Admin_UI SHALL 使全局号池卡片的所有交互元素可通过键盘操作并带有可访问名称。

### Requirement 11: 兼容与失效保护

**User Story:** 作为运维管理员，我想确认这个功能不开就等于不存在、配错就少买而不是多买，这样我可以放心先升级再慢慢配。

#### Acceptance Criteria

1. WHERE `enabled` 为假，THE Purchase_Executor SHALL 产生与本特性上线前等价的采购决策（同一事件、同一配置下的请求量相同）。
2. WHEN 系统从不含 Pool_Config 的 `config.json` 启动，THE Config_Store SHALL 成功加载配置并把 Pool_Config 视为未启用。
3. WHEN 系统从已发布版本升级，THE Pool_Engine SHALL 通过需求 2 第 3 条的备注匹配把升级前采购的凭据计入 Global_Usable_Count，使首次启用后的缺口反映真实库存而非全额目标存量。
4. IF Quota_Source 未注入（装配顺序问题导致的静默降级），THEN THE Pool_Engine SHALL 按需求 3 第 6 条把查不到额度的凭据计为可用，使额度水位判定不生效而非误判为缺货。
5. IF Trigger_Supplier 在 Trigger_Event 落库之后、下单之前被删除，THEN THE Pool_Engine SHALL 跳过本次采购而不下单。
6. IF Trigger_Supplier 的 `autoPurchase` 为假或 `enabled` 为假，THEN THE Pool_Engine SHALL 沿用现有语义跳过本次采购。
7. IF Trigger_Supplier 的 `is_operable()` 为假（`baseUrl` 或 `apiKey` 为空），THEN THE Pool_Engine SHALL 跳过本次采购并记录一条 warn 级日志说明该家配置不完整。

### Requirement 12: 正确性属性

**User Story:** 作为维护者，我想有一组可用属性测试反复验证的不变式，这样任何后续改动一旦破坏「不超买」就会立刻失败。

#### Acceptance Criteria

1. FOR ALL Global_Usable_Count 与 1..=10000 范围内的 Pool_Target，THE Pool_Engine SHALL 使计算出的采购量满足 `Global_Usable_Count + 采购量 <= Pool_Target`（目标存量不变式）。
2. FOR ALL Global_Usable_Count 与 Pool_Target，THE Pool_Engine SHALL 使 Deficit 非负（缺口非负属性）。
3. FOR ALL 供货商配置与 Deficit，THE Pool_Engine SHALL 使采购量小于或等于该家 `maxPurchase`（单家上限不变式）。
4. FOR ALL 供货商配置与 Deficit，IF 采购量大于 0，THEN THE Pool_Engine SHALL 使采购量大于或等于该家 `minPurchase`（下限二值性：要么不买，要么买够下限）。
5. FOR ALL 凭据集合与供货商配置集合，THE Pool_Engine SHALL 使 Purchased_Credential 的判定结果与凭据在集合中的顺序无关（识别规则的顺序无关性）。
10. FOR ALL 凭据集合，THE Pool_Engine SHALL 使 Global_Usable_Count 不包含任何 `died_at` 非空的凭据（死号不占额度不变式）。
6. FOR ALL 同一个 Trigger_Event 与同一份配置快照及同一个 Global_Usable_Count，重复计算采购量 SHALL 产生相同结果（确定性属性）。
7. FOR ALL 同一个 Trigger_Event 与同一家供货商，重复派生 `client_order_id` SHALL 产生相同的值（幂等键确定性属性）。
8. FOR ALL 合法 Pool_Config，序列化后再反序列化 SHALL 产生与原值相等的 Pool_Config（往返属性）。
9. FOR ALL 任意到货推送序列，THE Pool_Engine SHALL 使串行处理完该序列后的 Global_Usable_Count 不超过 Pool_Target（序列化消耗不变式）。

## 待确认决策

以下语义已按「宁可少买不可多买」取了默认值并写成验收标准。请逐条确认或改判，改判后我会同步修改对应验收标准。

| 编号 | 决策点 | 文档采用的默认 | 备选 |
| --- | --- | --- | --- |
| D1 | N 的作用域 | 号池目标存量：所有采购来的可用号合计不超过 N（需求 4.1） | 时间窗口预算（每日/每小时全局买 N 个）；两者都要 |
| D2 | 下单对象 | 只向推送方那一家下单（需求 5.1） | 向所有家下单直到补齐缺口 |
| D3 | 缺口未补齐时是否换家 | 不换。缺口留给下一次到货推送（需求 5.7） | 顺延到其它家——但这需要一个顺序，与「不设优先级」冲突 |
| D4 | 手动采购是否受 N 约束 | 不受约束，但事件里标注未经号池引擎（需求 6.6、6.7） | 受 N 约束 |
| **D5** | **采购号的识别方式** | **两级：`supplier_id` 非空，或 `supplier_id` 为空但 `source_channel` 与某家配置的 `sourceChannel` 精确相等（需求 2.2、2.3）。不区分来自哪一家** | **只认 `supplier_id`（会漏掉升级前买的号，导致买超）；放宽成「`source_channel` 非空即算」（会把手工备注也算进去）；追加 `nicknamePrefix` 作为第三级兜底** |
| D6 | 手动添加的凭据是否计入 | 不计入（需求 2.5、2.6）。它们不由采购流程管理，数量也不受目标存量控制 | 计入——它们同样在承载流量 |
| D7 | 已删除或已禁用供货商名下遗留的凭据是否计入 | 计入（需求 2.2、2.8）。它们还在服务流量，不算就会超买 | 不计入 |
| **D12** | **死号是否占用目标存量额度** | **不占。判死后即使还在保留期内没被清理，也算不可用，缺口会去补新的（需求 3.2、3.3）** | **占用——但那意味着号死了不补货，池子会一路缩到 0** |
| D8 | 逐家补货闸在启用后的命运 | 完全失效，只看全局（需求 6.2） | 两者取与（都满足才买） |
| D9 | 是否需要按金额封顶 | 本次不做。`total_debit` 与 `purchase_price` 已落库，具备后续实现的数据底座 | 增加日/月金额预算 |
| D10 | 池子超过 N 时的处理 | 只停止采购，不动已有凭据（需求 4.10） | 主动禁用超出部分 |
| D11 | 是否给旧版采购号回填 `supplier_id` | 本次不做。备注匹配已足够支撑水位判定，且默认 `sourceChannel` 三家相同、无法可靠归属到具体某家 | 做一次性回填（需要在只有一家匹配时才写，并加 `backfilled` 标记，参照现有 `added_at_backfilled` 的做法） |

## 明确不在本次范围内

- 事件队头阻塞（全局 FIFO `claim_next` + 单个 `processing_lock`）的改造
- 事件层自动重试、退避与死信（含 `RateLimited.retry_after` 的启用）
- 供货商被删后残留事件永远 retry 失败的修复
- 事件表清理与归档
- `config.json` 全量读改写的共享锁改造（本特性只保证不新增不受同一把锁保护的写入点）
- 按金额封顶的预算功能（见 D9）
- 旧版采购号的 `supplier_id` 回填（见 D11）
- 供货商质量档案与每存活小时成本核算
- 409 已成交订单的付费 key 自动恢复
