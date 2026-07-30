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
# 只发最小 payload（max_tokens=16）：目的是压连接与调度层，不是烧上游额度。
set -uo pipefail

BASE_URL="${1:?用法: stress-test.sh <base_url> <api_key> [并发] [秒数]}"
API_KEY="${2:?缺少 api_key}"
CONCURRENCY="${3:-50}"
DURATION="${4:-60}"
# 用于 docker ps 匹配被测容器，采样连接数用；留空则跳过采样
CONTAINER_MATCH="${5:-}"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

echo "目标      : $BASE_URL"
echo "并发      : $CONCURRENCY"
echo "持续      : ${DURATION}s"
echo "开始      : $(date -u '+%H:%M:%S') UTC"
echo

# 单个 worker：在时限内循环发请求，把 "HTTP码 耗时" 逐行写进自己的结果文件。
# 独立文件避免多进程写同一文件互相截断。
worker() {
  # 分三行声明：同一条 local 语句里引用刚声明的变量，在 set -u 下会报 unbound
  local id="$1"
  local deadline="$2"
  local out="$WORKDIR/w$id"
  while [ "$(date +%s)" -lt "$deadline" ]; do
    curl -sS -o /dev/null \
      -w '%{http_code} %{time_total}\n' \
      --max-time 180 \
      -H "Authorization: Bearer $API_KEY" \
      -H 'Content-Type: application/json' \
      -H 'anthropic-version: 2023-06-01' \
      -d '{"model":"claude-sonnet-4-6","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}' \
      "$BASE_URL/v1/messages" >> "$out" 2>/dev/null || echo "000 0" >> "$out"
  done
}

DEADLINE=$(( $(date +%s) + DURATION ))
for i in $(seq 1 "$CONCURRENCY"); do
  worker "$i" "$DEADLINE" &
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

wait

echo
echo "=== 结果 ==="
cat "$WORKDIR"/w* 2>/dev/null | python3 -c '
import sys
rows = []
for line in sys.stdin:
    parts = line.split()
    if len(parts) == 2:
        try:
            rows.append((parts[0], float(parts[1])))
        except ValueError:
            pass

if not rows:
    print("没有采集到任何响应")
    sys.exit()

total = len(rows)
ok = [t for c, t in rows if c == "200"]
codes = {}
for c, _ in rows:
    codes[c] = codes.get(c, 0) + 1

print(f"请求总数 : {total}")
print(f"成功     : {len(ok)}  ({len(ok) / total * 100:.1f}%)")
print("状态码   : " + "  ".join(f"{c}={n}" for c, n in sorted(codes.items())))
if ok:
    ok.sort()
    def q(p):
        return ok[min(len(ok) - 1, int(len(ok) * p))]
    print(f"耗时(秒) : p50={q(.5):.2f}  p90={q(.9):.2f}  p99={q(.99):.2f}  max={ok[-1]:.2f}")
'
