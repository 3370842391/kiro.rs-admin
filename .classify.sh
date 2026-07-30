#!/usr/bin/env bash
# 在测试站上分类 CONTENT_LENGTH 失败请求：按 trace 关联「实际发送体积 + 图片数」。
# 逐行状态机，不靠 grep 邻近窗口。
set -u
W=${1:-120m}
RAW=$(mktemp)
docker logs kiro-rs-test --since "$W" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' > "$RAW"

python3 - "$RAW" <<'PY'
import re, sys
lines = open(sys.argv[1], encoding='utf-8', errors='replace').read().splitlines()

# 每条 "Received POST" 开一个请求记录，携带 image_count / image_total_b64_kb；
# 随后的 "图片预算处理完成" 补 after_kb；"实际发送请求体元数据" 补 body_bytes；
# "CONTENT_LENGTH_EXCEEDS_THRESHOLD"（真正的上游失败，出现在 provider/流式失败行）标记失败。
# 用「最近一个请求」关联——单会话串行足够准，避免跨请求 grep 串行。
reqs = []
cur = None
def num(pat, s):
    m = re.search(pat, s)
    return int(m.group(1)) if m else None

for ln in lines:
    if 'Received POST' in ln:
        cur = {
            'img': num(r'image_count=(\d+)', ln),
            'img_kb': num(r'image_total_b64_kb=(\d+)', ln),
            'mc': num(r'message_count=(\d+)', ln),
            'after_kb': None, 'body': None, 'failed': False,
        }
        reqs.append(cur)
    elif cur is not None:
        if '图片预算处理完成' in ln:
            cur['after_kb'] = num(r'image_after_b64_kb=(\d+)', ln)
        elif '实际发送请求体元数据' in ln and cur.get('body') is None:
            cur['body'] = num(r'body_bytes=(\d+)', ln)
        # 真正的上游长度失败：provider 层或流式失败行里带 reason
        if 'CONTENT_LENGTH_EXCEEDS_THRESHOLD' in ln and 'incoming image payload' not in ln:
            cur['failed'] = True

fails = [r for r in reqs if r['failed']]
print(f"总请求 {len(reqs)}  失败(CONTENT_LENGTH) {len(fails)}")
if not fails:
    sys.exit()

with_img = [r for r in fails if (r['img'] or 0) > 0]
no_img   = [r for r in fails if (r['img'] or 0) == 0]
print(f"  带图失败 {len(with_img)}   无图失败 {len(no_img)}")

def stat(name, rs, key):
    vals = sorted(v for v in (r[key] for r in rs) if v is not None)
    if not vals:
        print(f"    {name}: 无数据"); return
    print(f"    {name}: min={vals[0]} 中位={vals[len(vals)//2]} max={vals[-1]}")

print("  [带图失败]")
stat("body_bytes", with_img, 'body')
stat("图片after_kb", with_img, 'after_kb')
stat("message_count", with_img, 'mc')
print("  [无图失败]")
stat("body_bytes", no_img, 'body')
stat("message_count", no_img, 'mc')
PY
rm -f "$RAW"
