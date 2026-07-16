# GUI 本地配置保存设计

## 目标

为 Kiro 批量登录 GUI 增加显式“保存配置”和“清除配置”操作。用户保存一次后，后续启动自动恢复常用登录、输出和 RS 连接字段，避免重复输入。

## 保存范围

保存以下 GUI 字段：

- 输入和输出格式模板
- 登录模式、Start URL、Region、超时、无头模式
- 密码保险库、完整凭据 JSON、Checkpoint 路径
- 恢复运行和结果方式
- RS URL、Admin Key
- SSH 开关、主机、用户、端口、私钥路径、远端主机/端口、本地端口

按用户明确选择，Admin Key 以明文 JSON 字段保存。界面在保存成功提示中说明配置含敏感信息。

绝不保存：原始账号文本、解析后的账号、一次性密码、新密码、Cookie、JWE、CSRF、workflow handle、access token、refresh token、凭据 JSON 文件内容或运行日志。

## 文件与持久化

默认文件为：

```text
%LOCALAPPDATA%\KiroBatchLogin\settings.json
```

配置包含固定 `version`，使用 UTF-8 JSON。保存时先写同目录临时文件、flush 和 fsync，再用 `os.replace` 原子替换目标文件，避免进程中断留下半个 JSON。

加载时只接受预定义字段与基本类型；未知字段忽略。文件不存在时使用现有默认值。JSON 损坏、版本不支持或字段类型错误时，GUI 继续启动并显示脱敏警告，不覆盖当前默认值。

环境变量 `KIRO_RS_ADMIN_KEY` 仍作为初始默认值；如果配置文件包含非空 Admin Key，已保存配置优先。

## GUI 交互

在底部左侧、现有“导入已有 JSON”旁增加：

- `保存配置`：收集当前表单值并原子写入配置文件；成功后状态栏和运行日志提示保存路径及“包含明文 Admin Key”。
- `清除配置`：确认后删除配置文件，将状态栏更新为“已清除，下次启动使用默认值”；不立即清空当前表单，避免误操作丢失正在编辑的内容。

启动时在创建控件变量之前加载配置，再用已验证字段设置变量，因此模式可见性与 RS/SSH 布局会自然恢复。

## 组件边界

新建 `scripts/batch_login/gui_settings.py`：

- `GuiSavedSettings`：稳定、可测试的配置数据对象。
- `default_settings_path()`：计算 `%LOCALAPPDATA%` 路径。
- `GuiSettingsStore.load()`：安全解析与字段过滤。
- `GuiSettingsStore.save()`：原子持久化。
- `GuiSettingsStore.clear()`：幂等删除。

`gui_app.py` 只负责把 tkinter 变量与 `GuiSavedSettings` 相互映射、显示确认框和状态提示，不直接实现 JSON/文件系统规则。

## 测试与验收

- 配置保存后重新加载字段完全一致，包括明文 Admin Key。
- 保存文件中不出现账号文本、密码或 token 字段。
- 原子替换路径和 UTF-8 JSON 可重开读取。
- 损坏 JSON、错误版本和错误字段类型返回警告但不阻止启动。
- 清除操作幂等。
- GUI 启动自动应用配置；按钮保存/清除调用存储层并给出提示。
- 完整 batch-login 单元测试、GUI `--check`、Python 编译和 `git diff --check` 全部通过。
