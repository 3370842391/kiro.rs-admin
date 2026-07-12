# 服务器测试构建容器设计

## 背景

当前每次推送 `master` 都会触发 GitHub Actions。近期 `Build Artifacts` 通常耗时约 4–6 分钟，`Build and Push GHCR Images` 通常耗时约 7–9 分钟。检测迭代只需要服务器实际使用的 Linux musl x64 产物，却需要等待多平台产物和 GHCR 镜像完成。

测试阶段需要一条手动触发、只服务于内部检测的快速路径；正式发布、客户更新和生产容器仍必须使用 GitHub Release/GHCR。

## 目标与隔离边界

- 在服务器持久化 Bun、Cargo registry 和 Cargo target 编译缓存。
- 每次构建一个明确 Git commit，只生成 Linux musl x64 测试镜像。
- 测试容器名为 `kiro-rs-test`，公网端口为 `8991`，容器内部仍监听 `8990`。
- 测试数据目录为 `data-test`，不与生产 `data` 共享写入。
- 构建失败不停止现有测试容器；新容器启动失败则恢复旧测试镜像。
- 生产 `kiro-rs` 容器、8990 端口、GitHub Actions 和正式在线更新不受影响。

## 仓库组成

### `Dockerfile.test`

使用 BuildKit cache mount：

- Bun 下载缓存持久化，仍执行 `bun install --frozen-lockfile`。
- Cargo registry/git 缓存持久化。
- `/app/target` 持久化，使相同依赖和未改动 crate 不必重新编译。
- 执行 `cargo build --release --locked --no-default-features`，与 Alpine/musl 生产镜像保持一致。
- 将最终二进制复制出 cache mount，再放入最小 Alpine 运行镜像。

镜像只用于服务器内部测试，不推送 GHCR。

### `docker-compose.test.yml`

定义唯一服务 `kiro-rs-test`：

- 镜像标签由 `TEST_IMAGE_TAG` 注入，格式为 Git commit 短 SHA。
- `container_name: kiro-rs-test`。
- `0.0.0.0:8991:8990` 公网映射。
- `./data-test:/app/config` 独立数据卷。
- `restart: unless-stopped`。
- 容器健康检查访问内部管理页面根路径，证明 HTTP 服务已启动。

### `scripts/test-deploy.sh`

脚本只在服务器专用源码目录运行，拒绝带有未提交修改的工作树。接口为：

```bash
./scripts/test-deploy.sh [git-ref]
```

未传 `git-ref` 时使用 `deploy/master`。脚本流程：

1. fetch 配置的远端并解析目标 ref 为准确 commit SHA。
2. 以 detached HEAD 切到该 commit，避免移动正式分支。
3. 记录当前 `kiro-rs-test` 容器使用的旧镜像。
4. 通过 `Dockerfile.test` 构建 `kiro-rs-test:<short-sha>`。
5. 运行新镜像的 `--version` smoke test；失败时退出且不碰旧容器。
6. 使用新镜像重建测试容器。
7. 在限定时间内轮询 `http://127.0.0.1:8991/`。
8. 健康检查失败时删除失败容器并用记录的旧镜像恢复。
9. 成功时输出 commit、镜像、URL 和总耗时。

同一 commit 已存在本地镜像时允许复用 Docker 层和编译缓存，但仍执行容器替换与健康检查。

## 服务器目录与公网访问

服务器使用专用目录，例如：

```text
/opt/kiro-rs-test/
├── src/        # 专用 Git checkout
└── data-test/  # 测试配置、凭据和数据库
```

首次初始化仅复制生产目录中的配置型 JSON 快照，例如 `config.json`、`credentials.json` 和 `model_mappings.json`；不复制 trace DB、WAL、日志或缓存数据库。测试实例启动后创建专用 Ztest Client Key，不复用客户侧生产 Key。

Docker 映射监听所有网卡，并在服务器防火墙放行 TCP 8991。外部检测地址为 `http://<server-ip>:8991`。本轮不引入域名或 TLS 反向代理；如检测平台强制 HTTPS，再独立增加测试子域名。

## 并发、失败与回退

- 脚本使用本地锁文件，拒绝两个测试构建并发运行。
- fetch、checkout、build 或 smoke test 失败时，不停止现有测试容器。
- 替换后的 HTTP 健康检查失败时，恢复旧镜像并再次检查；恢复失败时输出明确的人工处理命令和最近容器日志。
- `data-test` 不在构建或回退中删除，也不被 Git 跟踪。
- 生产容器和生产数据目录不作为脚本操作目标。

## 测试与验收

- `bash -n scripts/test-deploy.sh` 验证脚本语法。
- `docker compose -f docker-compose.test.yml config` 验证 Compose 展开结果和 8991 映射。
- 在服务器执行首次构建，记录冷启动耗时并确认 `http://127.0.0.1:8991/` 与公网 `http://<server-ip>:8991/` 可访问。
- 修改一个小型 Rust 文件或只更新前端后执行第二次构建，确认 Bun/Cargo/target cache 命中并记录热构建耗时。
- 构造无效 ref，确认现有测试容器不中断。
- 用无法启动的测试配置验证健康检查失败后旧镜像可以恢复。
- 最后使用专用测试 Key 运行本地 Anthropic probe 或 Ztest，生产 8990 请求不受影响。

## 非目标

- 不把服务器注册为 GitHub self-hosted runner。
- 不用测试镜像替代正式 GitHub Release/GHCR。
- 不自动把测试结果晋升到生产。
- 不在测试目录保存 GitHub、Docker Registry 或生产客户密钥。
