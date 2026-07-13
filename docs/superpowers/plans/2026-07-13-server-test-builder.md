# 服务器 8991 测试构建链 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `43.225.196.10` 上建立带持久化编译缓存的 Linux musl x64 测试构建链，并通过独立公网 8991 容器进行 Ztest 验证。

**Architecture:** 仓库增加测试 Dockerfile、Compose 文件和部署脚本；Docker BuildKit cache mount 保存 Bun/Cargo/target 缓存。服务器 `/opt/kiro-rs-test` 使用独立 Git checkout 和 `data-test`，构建成功且 smoke test 通过后才替换 `kiro-rs-test`，失败则保留或恢复旧测试镜像。生产 `/opt/kiro-rs-admin`、容器 `kiro-rs-admin` 和 127.0.0.1:8990 不作为脚本操作目标。

**Tech Stack:** Debian 12、Docker Engine/Compose v5、BuildKit、Alpine 3.21、Rust 1.92 musl、Bun 1、Bash、curl、UFW

---

### Task 1: 用契约测试锁定隔离与缓存要求

**Files:**
- Create: `scripts/test-builder-contract.test.ts`

- [ ] **Step 1: Write the failing contract test**

创建 `scripts/test-builder-contract.test.ts`：

```ts
import { expect, test } from 'bun:test'

async function read(path: string) {
  return Bun.file(path).text()
}

test('test builder is cached and isolated from production', async () => {
  const dockerfile = await read('Dockerfile.test')
  const compose = await read('docker-compose.test.yml')
  const script = await read('scripts/test-deploy.sh')
  const dockerignore = await read('.dockerignore')
  const gitignore = await read('.gitignore')

  expect(dockerfile).toContain('mount=type=cache')
  expect(dockerfile).toContain('cargo build --release --locked --no-default-features')
  expect(compose).toContain('0.0.0.0:8991:8990')
  expect(compose).toContain('./data-test:/app/config')
  expect(compose).toContain('kiro-rs-test:${TEST_IMAGE_TAG:-latest}')
  expect(script).toContain('git checkout --detach')
  expect(script).toContain('http://127.0.0.1:8991/')
  expect(script).toContain('docker run --rm')
  expect(script).not.toContain('docker stop kiro-rs-admin')
  expect(script).not.toContain('docker rm kiro-rs-admin')
  expect(dockerignore).toContain('data-test/')
  expect(gitignore).toContain('/data-test/')
})
```

- [ ] **Step 2: Run test to verify it fails**

Run from repository root:

```powershell
bun test scripts/test-builder-contract.test.ts
```

Expected: FAIL because `Dockerfile.test`, `docker-compose.test.yml` and `scripts/test-deploy.sh` do not exist.

### Task 2: 增加测试 Dockerfile、Compose 和敏感目录排除

**Files:**
- Create: `Dockerfile.test`
- Create: `docker-compose.test.yml`
- Modify: `.dockerignore`
- Modify: `.gitignore`

- [ ] **Step 1: Exclude test runtime data**

在 `.dockerignore` 的运行时数据段加入：

```text
data-test/
```

在 `.gitignore` 加入：

```text
/data-test/
```

- [ ] **Step 2: Add the cached test Dockerfile**

创建 `Dockerfile.test`：

```dockerfile
# syntax=docker/dockerfile:1.7

FROM oven/bun:1-alpine AS frontend-builder
WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/bun.lock* ./
RUN --mount=type=cache,id=kiro-test-bun,target=/root/.bun/install/cache \
    bun install --frozen-lockfile --ignore-scripts
COPY admin-ui ./
RUN bun run build

FROM rust:1.92-alpine AS rust-builder
RUN apk add --no-cache musl-dev perl make
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist
RUN --mount=type=cache,id=kiro-test-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=kiro-test-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=kiro-test-target,target=/app/target \
    cargo build --release --locked --no-default-features && \
    cp /app/target/release/kiro-rs /tmp/kiro-rs

FROM alpine:3.21
RUN apk add --no-cache ca-certificates curl
WORKDIR /app
COPY --from=rust-builder /tmp/kiro-rs /app/kiro-rs
VOLUME ["/app/config"]
EXPOSE 8990
ENTRYPOINT ["/app/kiro-rs"]
CMD ["-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
```

- [ ] **Step 3: Add the isolated Compose service**

创建 `docker-compose.test.yml`：

```yaml
services:
  kiro-rs-test:
    build:
      context: .
      dockerfile: Dockerfile.test
    image: kiro-rs-test:${TEST_IMAGE_TAG:-latest}
    container_name: kiro-rs-test
    ports:
      - "0.0.0.0:8991:8990"
    volumes:
      - ./data-test:/app/config
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:8990/"]
      interval: 2s
      timeout: 2s
      retries: 15
      start_period: 3s
```

- [ ] **Step 4: Run the contract test to observe the remaining failure**

```powershell
bun test scripts/test-builder-contract.test.ts
```

Expected: FAIL only because `scripts/test-deploy.sh` does not exist.

### Task 3: 实现带锁、smoke test、健康检查和回退的部署脚本

**Files:**
- Create: `scripts/test-deploy.sh`

- [ ] **Step 1: Add the deployment script**

创建可执行脚本，完整行为如下：

```bash
#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

REMOTE="${TEST_GIT_REMOTE:-deploy}"
REF="${1:-${REMOTE}/master}"
COMPOSE_FILE="${TEST_COMPOSE_FILE:-docker-compose.test.yml}"
LOCK_DIR="${TEST_DEPLOY_LOCK_DIR:-.test-deploy.lock}"
HEALTH_URL="${TEST_HEALTH_URL:-http://127.0.0.1:8991/}"

if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  echo "已有测试构建正在运行: $LOCK_DIR" >&2
  exit 1
fi
trap 'rmdir "$LOCK_DIR" 2>/dev/null || true' EXIT

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "测试源码目录存在未提交修改，拒绝切换 commit" >&2
  exit 1
fi

started_at="$(date +%s)"
git fetch "$REMOTE" \
  "+refs/heads/master:refs/remotes/${REMOTE}/master" --tags
target_commit="$(git rev-parse --verify "${REF}^{commit}")"
short_sha="$(git rev-parse --short=12 "$target_commit")"
new_image="kiro-rs-test:${short_sha}"
old_image="$(docker inspect kiro-rs-test --format '{{.Config.Image}}' 2>/dev/null || true)"

git checkout --detach "$target_commit"
echo "构建 commit: $target_commit"

TEST_IMAGE_TAG="$short_sha" docker compose -f "$COMPOSE_FILE" build
docker run --rm "$new_image" --version

TEST_IMAGE_TAG="$short_sha" docker compose -f "$COMPOSE_FILE" up \
  -d --no-build --force-recreate

healthy=false
for _ in $(seq 1 30); do
  if curl -fsS "$HEALTH_URL" >/dev/null; then
    healthy=true
    break
  fi
  sleep 1
done

if [[ "$healthy" != true ]]; then
  echo "新测试容器健康检查失败" >&2
  docker logs --tail 100 kiro-rs-test >&2 || true
  if [[ -n "$old_image" ]] && docker image inspect "$old_image" >/dev/null 2>&1; then
    old_tag="${old_image#kiro-rs-test:}"
    TEST_IMAGE_TAG="$old_tag" docker compose -f "$COMPOSE_FILE" up \
      -d --no-build --force-recreate
    for _ in $(seq 1 30); do
      if curl -fsS "$HEALTH_URL" >/dev/null; then
        echo "已恢复旧测试镜像: $old_image" >&2
        exit 1
      fi
      sleep 1
    done
    echo "旧测试镜像恢复后仍不健康，请检查 docker logs kiro-rs-test" >&2
  fi
  exit 1
fi

elapsed="$(( $(date +%s) - started_at ))"
echo "测试部署成功"
echo "commit=$target_commit"
echo "image=$new_image"
echo "url=http://43.225.196.10:8991/"
echo "elapsed_seconds=$elapsed"
```

- [ ] **Step 2: Run RED contract again, then make script executable**

```powershell
bun test scripts/test-builder-contract.test.ts
git update-index --chmod=+x scripts/test-deploy.sh
```

Expected: 1 pass, 0 fail.

- [ ] **Step 3: Validate shell syntax on the Debian server**

```powershell
scp -P 18792 scripts/test-deploy.sh root@43.225.196.10:/tmp/test-deploy.sh
ssh -p 18792 root@43.225.196.10 "bash -n /tmp/test-deploy.sh && rm -f /tmp/test-deploy.sh"
```

Expected: exit 0 and no output.

- [ ] **Step 4: Commit the local builder implementation**

```powershell
git add -- .dockerignore .gitignore Dockerfile.test docker-compose.test.yml scripts/test-deploy.sh scripts/test-builder-contract.test.ts
git diff --cached --check
git commit -m "feat(deploy): 增加8991测试构建链"
```

### Task 4: 补充操作文档并验证本地静态契约

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add the server test workflow documentation**

在“在线更新和发布”章节增加以下命令和边界：

```md
### 服务器测试构建

测试构建只服务内部检测，使用独立的 `kiro-rs-test` 容器、8991 端口和 `data-test` 目录；生产 8990 仍使用 GitHub/GHCR 镜像。

```bash
./scripts/test-deploy.sh                 # deploy/master
./scripts/test-deploy.sh 63c49359375227737b1d996a0b289425c67cc32a  # 指定 commit
```

首次构建会下载依赖；后续构建复用 BuildKit 中的 Bun、Cargo registry 和 target 缓存。测试数据与生产数据禁止使用同一宿主机目录。
```

- [ ] **Step 2: Run local verification**

```powershell
bun test scripts/test-builder-contract.test.ts
git diff --check
```

Expected: 1 pass, diff check exits 0.

- [ ] **Step 3: Commit documentation**

```powershell
git add -- README.md
git commit -m "docs(deploy): 说明服务器测试构建流程"
```

### Task 5: 在服务器初始化独立源码和测试数据

**Files on server:**
- Create directory: `/opt/kiro-rs-test`
- Create directory: `/opt/kiro-rs-test/data-test`
- Clone: `/opt/kiro-rs-test` Git checkout

- [ ] **Step 1: Verify the production boundary before mutation**

```powershell
ssh -p 18792 root@43.225.196.10 "docker inspect kiro-rs-admin --format '{{.Name}} {{.Config.Image}} {{range .Mounts}}{{.Source}}:{{.Destination}}{{end}}'; ss -ltnp | grep ':8991 ' || true"
```

Expected: production container is `kiro-rs-admin`, mount is `/opt/kiro-rs-admin/config:/app/config`, and port 8991 is unused.

- [ ] **Step 2: Clone the dedicated test checkout**

After the feature branch or merged commit is available on GitHub:

```powershell
ssh -p 18792 root@43.225.196.10 "git clone https://github.com/3370842391/kiro.rs-admin.git /opt/kiro-rs-test && cd /opt/kiro-rs-test && git remote rename origin deploy"
```

Expected: `/opt/kiro-rs-test/.git` exists and remote `deploy` points to the fork.

- [ ] **Step 3: Create an isolated configuration snapshot**

```powershell
$remote = @'
install -d -m 700 /opt/kiro-rs-test/data-test
for f in config.json credentials.json model_mappings.json; do
  if [ -f "/opt/kiro-rs-admin/config/$f" ]; then
    install -m 600 "/opt/kiro-rs-admin/config/$f" "/opt/kiro-rs-test/data-test/$f"
  fi
done
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Do not copy `traces.db*`, logs, usage files, WAL files or prompt cache databases. After startup, create a dedicated Ztest Client Key from the test Admin API/UI.

- [ ] **Step 4: Open the public test port**

```powershell
ssh -p 18792 root@43.225.196.10 "ufw allow 8991/tcp && ufw status | grep 8991"
```

Expected: TCP 8991 is ALLOW for IPv4 and IPv6.

### Task 6: 冷构建、热构建与公网验收

**Files:** none

- [ ] **Step 1: Run the initial cold build**

```powershell
ssh -p 18792 root@43.225.196.10 "cd /opt/kiro-rs-test && ./scripts/test-deploy.sh deploy/master"
```

Record `elapsed_seconds`, commit and image from the output.

- [ ] **Step 2: Verify isolation and public access**

```powershell
ssh -p 18792 root@43.225.196.10 "docker inspect kiro-rs-test --format 'image={{.Config.Image}} ports={{json .HostConfig.PortBindings}} mounts={{range .Mounts}}{{.Source}}:{{.Destination}}{{end}}'; curl -fsS http://127.0.0.1:8991/ >/dev/null"
curl.exe -fsS http://43.225.196.10:8991/ > $null
```

Expected:

```text
container = kiro-rs-test
host port = 0.0.0.0:8991
mount = /opt/kiro-rs-test/data-test:/app/config
local HTTP = success
public HTTP = success
```

- [ ] **Step 3: Run the warm-cache build**

Execute the same commit a second time:

```powershell
ssh -p 18792 root@43.225.196.10 "cd /opt/kiro-rs-test && ./scripts/test-deploy.sh deploy/master"
```

Expected: BuildKit reports cached dependency/compile steps and `elapsed_seconds` is materially below the GitHub GHCR baseline of 7–9 minutes.

- [ ] **Step 4: Verify an invalid ref does not interrupt the running test container**

```powershell
$remote = @'
before="$(docker inspect kiro-rs-test --format '{{.Id}}')"
cd /opt/kiro-rs-test
if ./scripts/test-deploy.sh refs/heads/does-not-exist; then
  exit 1
fi
after="$(docker inspect kiro-rs-test --format '{{.Id}}')"
test "$before" = "$after"
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: command exits 0 because the deployment failed safely and container ID did not change.

- [ ] **Step 5: Exercise the automatic image rollback path once**

Use a delayed local HTTP helper so the new-container health phase fails, then becomes healthy only after the script restores the previous image:

```powershell
$remote = @'
set -eu
cd /opt/kiro-rs-test
old_image="$(docker inspect kiro-rs-test --format '{{.Config.Image}}')"
( sleep 32; python3 -m http.server 18999 --bind 127.0.0.1 >/tmp/kiro-test-health.log 2>&1 ) &
helper_pid=$!
set +e
TEST_HEALTH_URL=http://127.0.0.1:18999/ ./scripts/test-deploy.sh deploy/master^
deploy_status=$?
set -e
kill "$helper_pid" 2>/dev/null || true
pkill -f 'python3 -m http.server 18999' 2>/dev/null || true
new_image="$(docker inspect kiro-rs-test --format '{{.Config.Image}}')"
test "$deploy_status" -ne 0
test "$new_image" = "$old_image"
curl -fsS http://127.0.0.1:8991/ >/dev/null
'@
ssh -p 18792 root@43.225.196.10 $remote
```

Expected: the intentionally failed deployment exits non-zero, `kiro-rs-test` returns to the exact old image, and 8991 remains healthy.

- [ ] **Step 6: Run API/Ztest acceptance**

Use the dedicated test Client Key against:

```text
http://43.225.196.10:8991
```

Run the local Anthropic probe first, then submit Ztest. Do not point production customer traffic at 8991.

### Task 7: 全量回归、范围检查与交付

**Files:** none

- [ ] **Step 1: Run all local verification commands**

```powershell
cargo test --all-features -j 1 --quiet
cargo check --all-features -j 1 --quiet
cd admin-ui
bun test src/lib/update-check.test.ts src/lib/cache-policy.test.ts
bun run build
cd ..
bun test scripts/test-builder-contract.test.ts
git diff --check
```

Expected: Rust tests/check, three Bun tests and frontend build all exit 0. Existing unrelated compiler warnings may be reported but no new warning may originate from changed files.

- [ ] **Step 2: Review scope and secrets**

```powershell
git status --short --branch
git diff master --stat
git log --oneline master..HEAD
```

Inspect `git diff master --` and confirm it does not contain any `csk_` value, GitHub token, credential JSON content, server `data-test` files, database, WAL or log files.

- [ ] **Step 3: Confirm production remained untouched**

```powershell
ssh -p 18792 root@43.225.196.10 "docker inspect kiro-rs-admin --format '{{.State.Status}} {{.Config.Image}} {{json .HostConfig.PortBindings}}'"
```

Expected: production container remains running with only `127.0.0.1:8990` binding.

- [ ] **Step 4: Enter the branch-finishing workflow**

Use `superpowers:finishing-a-development-branch` to offer local merge, PR, keep branch or discard. Do not push or replace production unless the user selects an option that explicitly authorizes it.
