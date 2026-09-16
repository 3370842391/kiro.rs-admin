#!/usr/bin/env bash
# 号池反代压力测试
#
# 验证 2026-07-26 一系列修复在高并发下确实生效，重点看三件事：
#   1. 上游连接数是否跟随并发（h2 单连接队头阻塞已消除 → 应远大于 1）
#   2. Tokio 运行时是否被同步磁盘 I/O 堵死（堵死的特征是上游连接归零、入站堆积）
#   3. fd / TIME_WAIT 是否触及上限（Connection: close 后每请求一条连接）
#
# 用法：stress-test.sh <base_url> <api_key> [并发数] [持续秒数] [容器名匹配串]
#
# 默认并发1、流式输出；用 STRESS_STREAM=0 验证非流式。
# KIRO_API_KEY 可替代第二参数（第二参数传空字符串）；STRESS_RESULTS_DIR指定保留目录。
# 这是闭环worker，未实现固定到达速率；request_id可关联服务端trace/发送统计。
set -uo pipefail

BASE_URL="${1:?用法: stress-test.sh <base_url> <api_key> [并发] [秒数]}"
API_KEY="${2:-${KIRO_API_KEY:-}}"
[ -n "$API_KEY" ] || { echo "缺少 API Key：设置 KIRO_API_KEY 或传入第二参数" >&2; exit 2; }
CONCURRENCY="${3:-1}"
DURATION="${4:-60}"
# 用于 docker ps 匹配被测容器，采样连接数用；留空则跳过采样
CONTAINER_MATCH="${5:-}"

RESULT_ROOT="${STRESS_RESULTS_DIR:-$(mktemp -d)}"
mkdir -p "$RESULT_ROOT"
WORKDIR=$(mktemp -d "$RESULT_ROOT/run-XXXXXX")
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
STREAM="${STRESS_STREAM:-1}"
MAX_REQUESTS_PER_WORKER="${STRESS_MAX_REQUESTS_PER_WORKER:-100}"
case "$CONCURRENCY:$DURATION:$MAX_REQUESTS_PER_WORKER" in *[!0-9:]*|:*|*::*|*:) echo "并发、时长和请求上限必须为正整数" >&2; exit 2;; esac
[ "$CONCURRENCY" -gt 0 ] && [ "$DURATION" -gt 0 ] && [ "$MAX_REQUESTS_PER_WORKER" -gt 0 ] || exit 2
case "$STREAM" in 0|1) ;; *) echo "STRESS_STREAM 必须为0或1" >&2; exit 2;; esac
AUTH_HEADERS=$(mktemp "$WORKDIR/.auth-XXXXXX")
chmod 600 "$AUTH_HEADERS"
printf 'Authorization: Bearer %s\nContent-Type: application/json\nanthropic-version: 2023-06-01\n' "$API_KEY" > "$AUTH_HEADERS"
unset API_KEY
trap 'rm -f -- "$AUTH_HEADERS"' EXIT
STREAM_JSON=false
[ "$STREAM" = 1 ] && STREAM_JSON=true
PAYLOAD="{\"model\":\"claude-sonnet-4-6\",\"max_tokens\":16,\"stream\":$STREAM_JSON,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}"
echo "逐请求记录: $WORKDIR（退出后保留）"

echo "目标      : $BASE_URL"
echo "并发      : $CONCURRENCY"
echo "持续      : ${DURATION}s"
echo "开始      : $(date -u '+%H:%M:%S') UTC"
echo

# 单个 worker：在时限内循环发请求，每个请求只写一条 JSONL 验真记录。
# 独立文件避免多进程写同一文件互相截断。
worker() {
  # 分三行声明：同一条 local 语句里引用刚声明的变量，在 set -u 下会报 unbound
  local id="$1"
  local deadline="$2"
  local out="$WORKDIR/w$id.jsonl"
  local sequence=0
  local stream_arg=()
  [ "$STREAM" = 1 ] && stream_arg=(--stream)
  while [ "$(date +%s)" -lt "$deadline" ] && [ "$sequence" -lt "$MAX_REQUESTS_PER_WORKER" ]; do
    sequence=$((sequence + 1))
    local request_id
    request_id=$(python3 -c 'import uuid; print(uuid.uuid4())')
    local body="$WORKDIR/$request_id.body"
    local metrics="$WORKDIR/$request_id.metrics"
    local headers="$WORKDIR/$request_id.headers"
    local remaining=$((deadline - $(date +%s)))
    [ "$remaining" -gt 0 ] || break
    [ "$remaining" -le 180 ] || remaining=180
    local curl_exit=0
    curl -sS --no-buffer -o "$body" -D "$headers" \
      -w '%{http_code} %{time_total} %{time_starttransfer}\n' \
      --max-time "$remaining" --max-filesize 8388608 \
      --header "@$AUTH_HEADERS" -H "x-oneapi-request-id: $request_id" \
      -d "$PAYLOAD" "$BASE_URL/v1/messages" > "$metrics" 2>"$WORKDIR/$request_id.stderr" || curl_exit=$?
    python3 "$SCRIPT_DIR/stress_result.py" record --body "$body" --metrics "$metrics" \
      --exit-code "$curl_exit" --request-id "$request_id" --headers "$headers" --out "$out" "${stream_arg[@]}" || return 1
  done
}

DEADLINE=$(( $(date +%s) + DURATION ))
WORKER_PIDS=()
for i in $(seq 1 "$CONCURRENCY"); do
  worker "$i" "$DEADLINE" &
  WORKER_PIDS+=("$!")
done

# 压测进行中每 5 秒采样一次连接与资源，这是判断修复是否生效的核心数据
if [ -n "$CONTAINER_MATCH" ]; then
  CID=$(docker ps --format '{{.Names}}' | grep -- "$CONTAINER_MATCH" | head -1)
  PID=$(docker inspect -f '{{.State.Pid}}' "$CID" 2>/dev/null || echo '')
else
  CID=''; PID=''
fi
printf '%-10s %8s %8s %10s %8s %8s\n' 时间 上游连接 入站 TIME_WAIT fd CPU%
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  if [ -n "$PID" ]; then
    UP=$(nsenter -t "$PID" -n ss -tn 2>/dev/null | awk 'NR>1{split($5,a,":"); if(a[1]!~/^172\./ && a[1]!~/^127\./) print a[1]}' | wc -l)
    IN=$(nsenter -t "$PID" -n ss -tn 2>/dev/null | awk 'NR>1{split($4,a,":"); if(a[2]=="8990") print}' | wc -l)
    TW=$(nsenter -t "$PID" -n ss -tan 2>/dev/null | grep -c TIME-WAIT)
    FD=$(ls "/proc/$PID/fd" 2>/dev/null | wc -l)
    CPU=$(docker stats --no-stream --format '{{.CPUPerc}}' "$CID" 2>/dev/null)
    printf '%-10s %8s %8s %10s %8s %8s\n' "$(date -u +%H:%M:%S)" "$UP" "$IN" "$TW" "$FD" "$CPU"
  fi
  sleep 5
done

worker_failed=0
for worker_pid in "${WORKER_PIDS[@]}"; do
  wait "$worker_pid" || worker_failed=1
done
python3 "$SCRIPT_DIR/stress_result.py" summary "$WORKDIR"
[ "$worker_failed" = 0 ] || { echo "有worker未能保存结果，请检查目录" >&2; exit 1; }
