//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::{HashMap, VecDeque};

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::Event;

/// thinking 块的 signature 占位字符串
///
/// Anthropic Messages API 协议规定 thinking 模式下，assistant 消息的
/// `{type:"thinking", ...}` 块必须带 `signature` 字段并在下一轮原样回传，
/// 否则 SDK / 服务端会拒绝请求并报：
/// `The content[].thinking in the thinking mode must be passed back to the API`。
///
/// 上游 Kiro 不下发真实签名（它本身不是 Anthropic 服务端），因此 kiro.rs 在
/// thinking 块结束时插入一个非空占位字符串以满足客户端本地校验。
/// converter 在解析 assistant 消息回传 Kiro 时只读 `block.thinking`，不读
/// signature，因此该占位字符串只在客户端 ↔ kiro.rs 之间存在，不会影响转发。
const TOOL_USE_XML_PREFIX: &str = "<tool_use";
const TOOL_USE_XML_CLOSE: &str = "</tool_use>";

/// 跨 chunk 过滤字面 `<tool_use ...>...</tool_use>` XML 泄漏（见
/// [`crate::kiro::model::events::strip_tool_use_xml_leaks`] 的语义）。
///
/// 流式下一个 `<tool_use>` 块可能跨多个 chunk，故用状态机：`stripping` 表示已进入
/// 一个未闭合的 `<tool_use>`，持续吞字直到遇到 `</tool_use>`；chunk 末尾若残留可能是
/// `<tool_use` 前缀的后缀，则缓冲到下个 chunk 再判定。
#[derive(Debug, Default)]
struct ToolUseXmlLeakFilter {
    buffer: String,
    stripping: bool,
}

impl ToolUseXmlLeakFilter {
    fn filter(&mut self, content: &str) -> String {
        self.buffer.push_str(content);
        let mut out = String::with_capacity(self.buffer.len());
        let mut rest = self.buffer.as_str();

        loop {
            if self.stripping {
                if let Some(close_start) = rest.find(TOOL_USE_XML_CLOSE) {
                    rest = &rest[close_start + TOOL_USE_XML_CLOSE.len()..];
                    self.stripping = false;
                    continue;
                }
                // 仍未闭合：丢弃已吞内容，但保留末尾可能是 `</tool_use>` 前缀的后缀，
                // 以正确处理闭合标签被切分到多个 chunk 的情形。
                let keep = longest_prefix_suffix(rest, TOOL_USE_XML_CLOSE);
                self.buffer = rest[rest.len() - keep..].to_string();
                return out;
            }

            let Some(start) = rest.find(TOOL_USE_XML_PREFIX) else {
                // 无 `<tool_use`：全部输出，但保留末尾可能是 `<tool_use` 前缀的后缀。
                let keep = longest_prefix_suffix(rest, TOOL_USE_XML_PREFIX);
                let emit_len = rest.len().saturating_sub(keep);
                out.push_str(&rest[..emit_len]);
                self.buffer = rest[emit_len..].to_string();
                return out;
            };

            out.push_str(&rest[..start]);
            let after_start = &rest[start..];
            let Some(open_end) = after_start.find('>') else {
                // 开标签尚未见到 `>`：可能是真标签的开头 → 进入 stripping 缓冲等闭合；
                // 否则原样输出 `<tool_use` 继续。
                if is_potential_tool_use_tag_start(after_start) {
                    self.stripping = true;
                    self.buffer.clear();
                    return out;
                }
                out.push_str(&after_start[..TOOL_USE_XML_PREFIX.len()]);
                rest = &after_start[TOOL_USE_XML_PREFIX.len()..];
                continue;
            };

            let tag_head = &after_start[..open_end];
            if !tag_head
                .get(TOOL_USE_XML_PREFIX.len()..)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
            {
                // 形似但非真标签（如 `<tool_user>`）：保留 `<tool_use` 继续扫描。
                out.push_str(&after_start[..TOOL_USE_XML_PREFIX.len()]);
                rest = &after_start[TOOL_USE_XML_PREFIX.len()..];
                continue;
            }

            let after_open = &after_start[open_end + 1..];
            if let Some(close_start) = after_open.find(TOOL_USE_XML_CLOSE) {
                rest = &after_open[close_start + TOOL_USE_XML_CLOSE.len()..];
            } else {
                // 开标签完整但闭合未到：进入 stripping，保留末尾可能是 `</tool_use>`
                // 前缀的后缀（处理闭合标签被切分到多个 chunk）。
                self.stripping = true;
                let keep = longest_prefix_suffix(after_open, TOOL_USE_XML_CLOSE);
                self.buffer = after_open[after_open.len() - keep..].to_string();
                return out;
            }
        }
    }

    /// 收尾：对残留缓冲做一次性剥离（截断的未闭合块会被丢弃）。
    fn finish(&mut self) -> String {
        self.stripping = false;
        let remaining = std::mem::take(&mut self.buffer);
        if remaining.is_empty() {
            String::new()
        } else {
            crate::kiro::model::events::strip_tool_use_xml_leaks(&remaining)
        }
    }
}

/// `s` 是否可能是 `<tool_use` 真标签的开头（用于开标签尚未闭合时的跨 chunk 判定）。
fn is_potential_tool_use_tag_start(s: &str) -> bool {
    TOOL_USE_XML_PREFIX.starts_with(s)
        || s.get(TOOL_USE_XML_PREFIX.len()..)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
}

/// 返回 `s` 末尾「恰好是 `needle` 某个前缀」的最长长度（chunk 边界保留用）。
/// 用于把可能被切断的 `<tool_use` / `</tool_use>` 标签保留到下个 chunk 再判定。
fn longest_prefix_suffix(s: &str, needle: &str) -> usize {
    let max = s.len().min(needle.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if s.is_char_boundary(s.len() - len) && needle.starts_with(&s[s.len() - len..]) {
            return len;
        }
    }
    0
}

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
/// 只包含**真正表示引用**的字符。
///
/// 原表把 `.` `,` `;` `:` `-` `)` `/` `>` 等常见标点也算作引用字符，于是模型写完一句话
/// 紧接着开标签（`done.<thinking>`、`note:<thinking>`）时，前一个字符命中标点就被判成
/// 「模型在讨论这个标签」而整段跳过——开标签漏检，thinking 内容连同字面标签一起泄漏进
/// 正文。实测 11 种常见形态里漏检 7 种：ASCII 的 `.` `:` `,` `;` `)` `-` 全中，标签**后面**
/// 紧跟标点同样漏（`has_quote_after` 那道检查）。
///
/// Kiro-Go 的做法是裸 `strings.Index`、完全不做引用过滤（proxy/handler.go:1135），
/// 保证真标签永不漏检。这里保留反引号/引号/反斜杠四个字符的判定，既留住「模型讨论标签」
/// 的保护，又消除标点造成的大面积漏检——为防一个罕见误判而引入高频漏检，取舍是反的。
const QUOTE_CHARS: &[u8] = &[b'`', b'"', b'\'', b'\\'];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// 标签是否被**同一个引号字符成对包裹** —— 即模型在讨论这个标签而非真的用它。
///
/// 判定必须看成对,不能看单侧。原实现「任一侧命中引号字符就跳过」会造成大量漏检:
/// - `<thinking>` 后紧跟反引号（模型 thinking 第一句就引代码/命令/文件名）
/// - `</thinking>` 前是反引号（模型 thinking 末尾刚引完一段代码）
///
/// 这两种在真实推理里极常见,实测 9 种引号相邻形态漏检 7 种。而真正的「讨论标签」写法
/// 是两侧成对: `` `<thinking>` ``、`"<thinking>"`。
///
/// `open` 指向标签首字符,`close` 指向标签末字符之后一位。
fn tag_is_quote_wrapped(buffer: &str, open: usize, close: usize) -> bool {
    if open == 0 {
        return false;
    }
    let bytes = buffer.as_bytes();
    let (Some(&before), Some(&after)) = (bytes.get(open - 1), bytes.get(close)) else {
        return false;
    };
    before == after && QUOTE_CHARS.contains(&before)
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        let after_pos = absolute_pos + TAG.len();

        // 成对包裹才算「模型在讨论这个标签」，见 tag_is_quote_wrapped。
        if tag_is_quote_wrapped(buffer, absolute_pos, after_pos) {
            search_start = absolute_pos + 1;
            continue;
        }

        // 流式守卫：标签后不足 2 字节时等下一个 chunk——要凑够上下文才能判断成对包裹。
        // 真正的流末尾由 find_real_thinking_end_tag_at_buffer_end 兜住。
        let after_content = &buffer[after_pos..];
        if after_content.len() < 2 {
            return None;
        }

        // 只要不被引号包裹就认。
        //
        // 原先额外要求后面紧跟 `\n\n`，于是 `</thinking>\nDone.`、`</thinking>Done.`、
        // `</thinking> Done.` 三种形态全部漏检——闭标签字面泄漏进 thinking 内容，块也
        // 不能及时闭合（线上实测:thinking 内容里出现独立的一行 `</thinking>`）。
        //
        // Kiro-Go 在块内直接用裸 `strings.Index` 找第一个闭标签、零后缀要求
        //（proxy/handler.go:1159）。这里保留引号判定（模型在讨论这个标签时不算闭合），
        // 去掉后缀要求：为防一个罕见误判而引入高频漏检，取舍是反的。
        let _ = after_content;
        return Some(absolute_pos);
    }

    None
}

/// 找出缓冲区**末尾那一串连续结束标签**的起始位置。
///
/// 上游 opus 在 thinking 通道复读退化时会连着吐多个 `</thinking>`（线上熔断日志
/// `channel=thinking repeat_count=4 unit_bytes=22`，22 字节正是两个标签）。这些标签后面
/// 都没有 `\n\n`，`find_real_thinking_end_tag` 一个都不认，于是被当正文泄漏给客户端。
///
/// 「一串」的定义：一个或多个 `</thinking>`，彼此之间只隔空白，且**延伸到缓冲区末尾**。
/// 要求延伸到末尾是关键——这样绝不会把思考正文中间提到的标签当成结尾。被引号包裹的
/// 标签（模型在讨论这个标签）不计入。
///
/// 返回 `None` 表示末尾不是这种标签串。
fn trailing_end_tag_run_start(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    // 从末尾往前剥：先去掉尾部空白，再要求紧邻的是一个未被引号包裹的完整标签。
    let mut cursor = buffer.trim_end().len();
    let mut run_start = None;

    loop {
        if cursor < TAG.len() {
            return run_start;
        }
        let candidate = cursor - TAG.len();
        // 必须用 get()：thinking 正文是任意 UTF-8，按字节回退可能落在多字节字符中间，
        // 直接 `&buffer[candidate..cursor]` 会 panic（例："内容</thinking>后面还有"）。
        if buffer.get(candidate..cursor) != Some(TAG) {
            return run_start;
        }
        // 引号包裹说明模型在讨论这个标签，不算结束标签，停止回剥。
        //
        // 注意：`QUOTE_CHARS` 里含 `>`，而**紧邻的前一个标签正好以 `>` 结尾**，
        // 若直接查前一字符会把「标签紧跟标签」误判成被引号包裹。因此先判定前面
        // （跳过空白后）是否就是另一个结束标签；是则属于同一串，不做引号检查。
        let before = buffer[..candidate].trim_end();
        let preceded_by_tag = before.ends_with(TAG);
        if !preceded_by_tag {
            let quoted_before = !before.is_empty() && is_quote_char(buffer, before.len() - 1);
            if quoted_before {
                return run_start;
            }
        }
        // 后侧：串内标签的后面是下一个标签或空白，只需在**串尾**那次检查。
        if run_start.is_none() && is_quote_char(buffer, cursor) {
            return None;
        }
        run_start = Some(candidate);
        // 继续往前，跳过标签之间的空白
        cursor = buffer[..candidate].trim_end().len();
    }
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    // 复读退化：末尾可能是一串连续标签。原逻辑从左往右扫、只认「后面全是空白」的那个，
    // 于是 `内容</thinking></thinking>` 会返回**第二个**，`buffer[..end_pos]` 里就带着
    // 字面的第一个标签。取两者较小值把整串一次收掉；单个标签时两者结果完全相同，
    // 故对既有行为是纯增量。
    let run_start = trailing_end_tag_run_start(buffer);
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        let after_pos = absolute_pos + TAG.len();

        // 成对包裹才算「模型在讨论这个标签」，见 tag_is_quote_wrapped。
        if tag_is_quote_wrapped(buffer, absolute_pos, after_pos) {
            search_start = absolute_pos + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[after_pos..].trim().is_empty() {
            return Some(run_start.map_or(absolute_pos, |run| run.min(absolute_pos)));
        }

        search_start = absolute_pos + 1;
    }

    run_start
}

/// 从 `end_pos`（`find_real_thinking_end_tag_at_buffer_end` 的返回值）算出应跳过的字节数。
///
/// 若 `end_pos` 正是末尾标签串的起点，就跳过**整串**；否则退化为单个标签长度，与原行为一致。
/// 不这样做的话，剥离只推进一个标签的长度，串里剩下的标签会作为普通文本泄漏出去。
fn end_tag_skip_len(buffer: &str, end_pos: usize) -> usize {
    const TAG: &str = "</thinking>";
    if trailing_end_tag_run_start(buffer) == Some(end_pos) {
        // 标签串按定义延伸到缓冲区末尾，故整段跳完。
        return buffer.len() - end_pos;
    }
    TAG.len()
}

/// 缓冲区末尾有多少字节可能是半截的 `<thinking>` 开标签。
///
/// 返回「最长的、同时是 `<thinking>` 真前缀的后缀」的长度；没有则返回 0。
/// 用于首个 thinking 块**之后**的正文：此时块间排序已定，不需要再无条件扣住 10 字节，
/// 只有尾巴确实像半截开标签时才等下一个 chunk，其余立即吐出，避免短正文被永久扣住。
fn partial_open_tag_suffix_len(buffer: &str) -> usize {
    const TAG: &str = "<thinking>";
    // 从最长可能的真前缀开始试，命中即返回；按字符边界安全切片。
    let max = TAG.len().saturating_sub(1).min(buffer.len());
    for len in (1..=max).rev() {
        let start = buffer.len() - len;
        if !buffer.is_char_boundary(start) {
            continue;
        }
        if TAG.starts_with(&buffer[start..]) {
            return len;
        }
    }
    0
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 成对包裹才算「模型在讨论这个标签」，见 tag_is_quote_wrapped。
        // 单侧命中就跳过会造成大量漏检：模型 thinking 第一句常常直接引代码/命令，
        // 形如 `<thinking>` 后紧跟反引号。
        let after_pos = absolute_pos + TAG.len();
        if !tag_is_quote_wrapped(buffer, absolute_pos, after_pos) {
            return Some(absolute_pos);
        }

        // 继续搜索下一个匹配
        search_start = absolute_pos + 1;
    }

    None
}

/// 检查 `name_pos`（指向标签名首字母）的前面是否构成合法的开标签起始，
/// 兼容裸写法 `<tag` 和带命名空间前缀的写法 `<prefix:tag`。
///
/// 返回 `Some(lt_pos)`（指向 `<` 的字节位置）表示合法；`None` 表示不是标签。
fn open_tag_lt_pos(buffer: &str, name_pos: usize) -> Option<usize> {
    let bytes = buffer.as_bytes();
    if name_pos == 0 {
        return None;
    }
    let prev = bytes[name_pos - 1];
    if prev == b'<' {
        return Some(name_pos - 1);
    }
    // 形如 `<prefix:tag`：name 前面是 ':'，再往前是一段标识符，再往前是 '<'
    if prev == b':' {
        let i = name_pos - 1; // 指向 ':'
        let mut j = i; // 标识符左边界扫描
        while j > 0 && {
            let c = bytes[j - 1];
            c.is_ascii_alphanumeric() || c == b'_'
        } {
            j -= 1;
        }
        // 标识符非空，且其左边是 '<'
        if j < i && j > 0 && bytes[j - 1] == b'<' {
            return Some(j - 1);
        }
    }
    None
}

/// 查找 invoke 开标签，返回指向 `<` 的字节位置
///
/// 兼容裸 `<invoke ...>` 与带命名空间前缀 `<prefix:invoke ...>` 两种写法。
///
/// **不做引用字符判定**。原先「`<` 前紧贴引号就跳过」是有害的：漏检一个真 invoke 意味着
/// 工具永不执行、整块 XML 泄漏成正文（调用点在无法识别时直接把整段当文本，见
/// `extract_invoke_content_blocks`），对话就此打断且无法恢复。
///
/// 误判方向反而是安全的：调用点后面有 5 道闸门——必须找到闭合 `</invoke>`、必须解析成功、
/// 工具名必须在本次请求声明的工具表内（`known_tool_names`）、不能位于代码围栏内、
/// 前文还要过 `invoke_looks_like_real_leak`。任一不过就安全退回当文本。模型「讨论」invoke
/// 时几乎总在反引号或围栏里，正好被围栏那道拦住。
///
/// 判定该留在有兜底的那一侧，而不是放在没有兜底的入口。
fn find_invoke_start(buffer: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(rel) = buffer[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(buffer, name_pos) {
            // 标签名后必须是边界字符（空白或 '>'），避免误匹配 invoked 之类
            let after = name_pos + "invoke".len();
            let next_ok = buffer.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            if next_ok {
                return Some(lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 从 `start` 之后查找第一个 invoke 闭标签，返回结束位置（exclusive，含闭标签）
///
/// 兼容裸 `</invoke>` 与带前缀 `</prefix:invoke>`。找不到返回 `None`（块还没到齐）。
fn find_invoke_block_end(buffer: &str, start: usize) -> Option<usize> {
    // 块 A 的边界 = 下一个 `<invoke` 开标签（即下一个块 B 的起点），没有则到 buffer 结尾。
    // 这样连发 burst（A 紧跟 B）时，A 的搜索区间被 B 的开标签卡住，绝不会吃进 B。
    let boundary = match find_next_invoke_open(buffer, start) {
        Some(p) => p,
        None => buffer.len(),
    };
    // 在 [start, boundary) 区间里取【最后一个】 `</invoke>` 作为真闭合。
    // 贪婪取最后一个 → patch 正文里出现的字面 `</invoke>` 不会导致提前截断；
    // 区间被下一个块开标签卡住 → 不会跨块误合并。
    find_last_invoke_close(buffer, start, boundary)
}

/// 从 `start` 之后查找下一个真正的 `<invoke`（或 `<prefix:invoke`）开标签的字节位置。
/// 跳过 `start` 处当前块自身的开标签。
fn find_next_invoke_open(buffer: &str, start: usize) -> Option<usize> {
    // 先跳过当前块的开标签：从 start 之后第一个 '>' 之后开始找。
    let after_open = match buffer[start..].find('>') {
        Some(rel) => start + rel + 1,
        None => return None,
    };
    // 只认结构：`<invoke` 或 `<prefix:invoke`，开标签名后须是空白/`>`/`/` 边界。
    //
    // 历史注记：这个函数当初是为绕开 find_invoke_start 的引用字符判定而写的——`>` 曾在
    // QUOTE_CHARS 里，而连发 burst 里 B 的 `<invoke` 恰好紧跟在 A 的 `</invoke>` 的 `>`
    // 后面，于是被判成「被引用」而漏检。那正是「入口加条件导致漏检」的又一例。
    // 该判定已从 find_invoke_start 移除，两处逻辑现已一致；保留本函数是因为它还承担
    // 「跳过当前块自身开标签」的语义，合并需要额外验证，暂不动。
    let region = &buffer[after_open..];
    let mut search = 0usize;
    while let Some(rel) = region[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(region, name_pos) {
            let after = name_pos + "invoke".len();
            let next_ok = region.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            if next_ok {
                return Some(after_open + lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 在 `[from, boundary)` 区间内查找最后一个 `</invoke>` / `</prefix:invoke>` 的结束位置
/// （exclusive，含闭标签）。找不到返回 `None`（块还没到齐）。
fn find_last_invoke_close(buffer: &str, from: usize, boundary: usize) -> Option<usize> {
    let region_end = boundary.min(buffer.len());
    if from >= region_end {
        return None;
    }
    let region = &buffer[from..region_end];
    let bytes = region.as_bytes();
    let mut search = 0usize;
    let mut last: Option<usize> = None;
    while let Some(rel) = region[search..].find("invoke>") {
        let name_pos = search + rel;
        // '</invoke>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some(from + name_pos + "invoke>".len());
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:invoke>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some(from + name_pos + "invoke>".len());
            }
        }
        search = name_pos + "invoke>".len();
    }
    last
}

/// 从标签字符串中抠出 `name="..."` 的值（取第一个匹配）
fn extract_name_attr(tag: &str) -> Option<String> {
    let needle = "name=\"";
    let rel = tag.find(needle)?;
    let start = rel + needle.len();
    let end_rel = tag[start..].find('"')?;
    Some(tag[start..start + end_rel].to_string())
}

/// 解析一个完整 invoke 块，抠出 (tool_name, input_json_string)
///
/// - tool name 来自 invoke 开标签的 `name="..."`（兼容 antml: 前缀）
/// - 参数为零个或多个 `<parameter name="K">V</parameter>`（兼容前缀）
/// - 参数值取到下一个参数开标签前的**最后一个** `</parameter>` 为界（贪婪），
///   允许多行 / 含 `<` / 中文 / 含字面 `</parameter>`（P0-1 修复）
/// - 用 serde_json 拼成 object（值都是字符串，自动转义）
/// - 无合法 name 或拼不出合法 JSON 返回 `None`
fn parse_invoke_block(block: &str) -> Option<(String, String)> {
    // invoke 开标签 = 块开头到第一个 '>'
    let open_end = block.find('>')?;
    let open_tag = &block[..=open_end];
    let tool_name = extract_name_attr(open_tag)?;
    if tool_name.is_empty() {
        return None;
    }

    let mut map = serde_json::Map::new();
    let body = &block[open_end + 1..];
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find("parameter name=\"") {
        let name_kw = cursor + rel;
        // 确认是真正的 '<parameter' 或 '<prefix:parameter' 开标签
        // name_kw 指向 'parameter'，往前应是 '<' 或 '<prefix:'
        // 确认是真正的开标签（'<parameter' / '<prefix:parameter'）；仅用于校验，不需要位置值
        if open_tag_lt_pos(body, name_kw).is_none() {
            cursor = name_kw + "parameter".len();
            continue;
        }
        // 找该参数开标签的 '>'
        let tag_gt = match body[name_kw..].find('>') {
            Some(r) => name_kw + r,
            None => break, // 开标签未闭合，停止
        };
        let param_open_tag = &body[name_kw..tag_gt + 1];
        // 从 'parameter name="..."' 抠 key（剥掉前缀干扰：直接找 name="）
        let key = match extract_name_attr(param_open_tag) {
            Some(k) => k,
            None => {
                cursor = tag_gt + 1;
                continue;
            }
        };
        // 参数值取到 </parameter>（兼容前缀）为界。find_param_close 较贵，只调一次，
        // 同时复用 (闭标签起始, 闭标签结束) 两个值：起始用于切值，结束用于推进游标。
        let val_start = tag_gt + 1;
        let (close_start, close_end) = match find_param_close(body, val_start) {
            Some(pair) => pair,
            None => break, // 值未闭合，停止
        };
        let value = &body[val_start..close_start];
        map.insert(key, serde_json::Value::String(value.to_string()));
        // 推进到闭标签之后
        cursor = close_end;
    }

    let obj = serde_json::Value::Object(map);
    let s = serde_json::to_string(&obj).ok()?;
    Some((tool_name, s))
}

/// 从 `from` 开始查找第一个 parameter 闭标签，返回 (起始位置, 结束位置 exclusive)
///
/// 兼容裸 `</parameter>` 与带前缀 `</prefix:parameter>`。
fn find_param_close(body: &str, from: usize) -> Option<(usize, usize)> {
    // P0-1：参数值（尤其 apply_patch 的 patch 正文）可能含字面 `</parameter>`。
    // 朴素「取第一个 </parameter>」会把值截断。改成「贪婪取边界内最后一个 </parameter>」：
    // 边界 = 下一个 `<parameter name="` 开标签（多参数场景），没有则到 body 结尾。
    // 这样：① 单参数（含 apply_patch）取到真正的最后一个闭合，内容里的字面闭合不误伤；
    //      ② 多参数仍按下一个参数开标签正确切分。
    // 局限（已诚实标注）：若参数值里同时含字面 `<parameter name="`，边界判定会偏早；
    // 实测 apply_patch 正文极少出现该字面串，可接受。
    let boundary = match find_next_param_open(body, from) {
        Some(p) => p,
        None => body.len(),
    };
    let region = &body[from..boundary];
    let kw = "parameter>";
    let mut last: Option<(usize, usize)> = None;
    let mut search = 0usize;
    let bytes = region.as_bytes();
    while let Some(rel) = region[search..].find(kw) {
        let name_pos = search + rel;
        // '</parameter>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some((from + name_pos - 2, from + name_pos + kw.len()));
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:parameter>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some((from + j - 2, from + name_pos + kw.len()));
            }
        }
        search = name_pos + kw.len();
    }
    last
}

/// 从 `from` 开始查找下一个 `<parameter name="`（或 `<prefix:parameter name="`）开标签的字节位置。
/// 用于 `find_param_close` 的贪婪边界：当前参数值最多吃到下一个参数开标签之前。
fn find_next_param_open(body: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(rel) = body[search..].find("parameter name=\"") {
        let kw_pos = search + rel;
        // 必须是真正的开标签：'parameter' 前面是 '<' 或 '<prefix:'
        if let Some(lt) = open_tag_lt_pos(body, kw_pos) {
            return Some(lt);
        }
        search = kw_pos + "parameter".len();
    }
    None
}

/// 剥掉块前文本尾部的独立 stray token 行（单独一行的 `call` 或 `count`）
///
/// 实测里 `<invoke>` 前常出现一行裸 `call`/`count`，需要从块前叙述文本里剥掉，
/// 避免泄漏给客户端。只剥“尾部、且独占一行”的 stray token，前面的正常叙述保留。
/// 已实测到的 stray token 集合：Opus 长上下文退化时，泄漏的 `<invoke>` 前常有一行裸的
/// `call` / `count` / `card`。集合形式便于以后扩充。
const STRAY_INVOKE_TOKENS: &[&str] = &["call", "count", "card"];

/// 复读熔断阈值：同一个非空短行/分片连续出现这么多次，判定为
/// 「上游长上下文退化复读死循环」，立即熔断本轮文本或 Thinking 输出。
///
/// 取值权衡：正常工具调用前最多出现 1 个引导词行（偶有 2~3），绝不会连续几十次。
/// 设为 16 可在客户端出现大面积刷屏前止血；比较保留行首缩进，避免把正常嵌套代码
/// 中不同层级的闭合括号误判为同一行。
/// 正文通道阈值。
///
/// 从 16 提到 40：此前连续行检测被上游分片打败（分片切在行中间，比较的是
/// `"CHECK NE"` / `"XT ITEM"` 这类碎片，计数永不累积），正文通道的熔断**实际上从未
/// 生效过**——线上实测 20 次连续相同行原样放行、无任何告警。修好行重组后它才第一次
/// 真正开始工作，因此阈值要按「首次启用」重新取。
///
/// Kiro-Go 在这件事上有直接教训：他们做过内容级去重，把 `6666666666` 吃成 `666`、
/// `abababab` 吃成 `abab`、`1833` 吃成 `183`，之后整个删掉并留下注释警告不要重新引入
/// （proxy/kiro.go:608）。按完整行比较比他们删掉的分片级去重安全得多，但同一个风险仍在：
/// 猜错就静默吃掉真实输出。16 行相同在合法输出里是可能的（日志转储、重复的测试输出），
/// 40 行几乎不可能，而离「客户端大面积刷屏」仍有充足余量。
const REPEAT_GUARD_TRIP_THRESHOLD: u32 = 40;
/// Thinking 通道的连续复读阈值。
///
/// 原值 4 太激进：正常推理里连续出现 4 个相同短行完全可能（分点枚举、重复的过渡词、
/// 反复的「先看 X」这类自述），把它判成退化会误杀大量正常请求。而 thinking 跳闸的
/// 代价曾经是**整轮响应连工具调用一起丢弃**，于是一次误判就把客户端的 agentic
/// 循环打断在半路。
///
/// 现在 thinking 跳闸只截断 thinking 通道（见 `thinking_repeat_tripped`），代价小了，
/// 但阈值本身仍应回到合理区间：12 既能在客户端看到大面积复读前止血，也给正常推理
/// 留足空间；真正的死循环会迅速越过它，另有周期检测器兜住多行循环形态。
const REPEAT_GUARD_THINKING_TRIP_THRESHOLD: u32 = 12;
/// 只对短候选做连续比较，避免复制和比较超大正文。
const REPEAT_GUARD_MAX_UNIT_BYTES: usize = 512;
/// 多行周期最多覆盖 8 行；截图中的 `user/assist` 退化循环周期为 4 行。
const REPEAT_GUARD_MAX_PERIOD: usize = 8;
/// 至少观察 16 个重复行，避免正常短列表或强调段落触发熔断。
const REPEAT_GUARD_MIN_PERIODIC_LINES: usize = 16;
/// 长周期至少完整重复 4 轮才判定退化。
const REPEAT_GUARD_MIN_PERIODIC_CYCLES: usize = 4;
/// 8 行周期乘 4 轮；固定上限避免按输出长度增长内存。
const REPEAT_GUARD_PERIODIC_HISTORY: usize =
    REPEAT_GUARD_MAX_PERIOD * REPEAT_GUARD_MIN_PERIODIC_CYCLES;
/// thinking 内容的来源通道。先到的通道独占本轮，另一条的内容一律丢弃，
/// 避免上游同时下发原生 reasoning 事件和字面 `<thinking>` 标签时推理输出重复两遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ThinkingSource {
    /// 尚未确定，两条通道都可争抢。
    #[default]
    Unknown,
    /// 已锁定原生 `reasoningContentEvent`。
    ReasoningEvent,
    /// 已锁定正文里的字面 `<thinking>` 标签。
    TagBlock,
}

impl ThinkingSource {
    /// 原生 reasoning 事件是否可用。首次调用时锁定该通道。
    fn allow_reasoning(&mut self) -> bool {
        match self {
            Self::TagBlock => false,
            _ => {
                *self = Self::ReasoningEvent;
                true
            }
        }
    }

    /// 字面标签是否可用。首次调用时锁定该通道。
    fn allow_tag(&mut self) -> bool {
        match self {
            Self::ReasoningEvent => false,
            _ => {
                *self = Self::TagBlock;
                true
            }
        }
    }
}

// 曾有一套「长 thinking 内容 SHA256 去重」：≥256 字节的 reasoning 事件若与最近 4 条中
// 任意一条完全相同就整条丢弃。已移除，原因见下。
//
// 它与周期复读熔断是同一个提交加进来的（9ecb965），动机是压 thinking 刷屏——而那个问题
// 现在由修好行重组的熔断器正经处理。真正的问题是它会**静默吃掉合法输出**：模型在推理里
// 重复引用同一段代码（对比修改前后时很常见）、重述同一份清单，第二次就凭空消失，用户看到
// 的是有缺口的推理。
//
// Kiro-Go 踩过完全一样的坑并留下警告（proxy/kiro.go:608）：「不要重新引入基于内容的去重。
// 在字符串层面，重放的分片和自我重复的文本无法区分」——他们的旧实现把 6666666666 吃成
// 666、abababab 吃成 abab、1833 吃成 183。同一段注释还指出上游**从不重放分片**（对真实
// 流量验证过，TCP 已保证至多一次投递），也就是说每一次「重复」都是模型真实产出。
//
// 结论：诚实地重复输出优于静默丢失。真正的退化复读由熔断器按阈值处理，那条路会明确
// 告知客户端（upstream_repetition_guard），不会让内容凭空消失。

#[derive(Debug, Clone, Copy)]
struct PeriodicRepeatMatch {
    period: usize,
    cycles: usize,
}

#[derive(Debug, Default)]
struct PeriodicRepeatGuard {
    channel: &'static str,
    recent_lines: VecDeque<String>,
    partial_line: String,
    partial_oversized: bool,
    code_fence_marker: Option<char>,
}

impl PeriodicRepeatGuard {
    fn reset_for_channel(&mut self, channel: &'static str) {
        self.channel = channel;
        self.recent_lines.clear();
        self.partial_line.clear();
        self.partial_oversized = false;
        self.code_fence_marker = None;
    }

    fn observe_segment(
        &mut self,
        segment: &str,
        channel: &'static str,
    ) -> Option<PeriodicRepeatMatch> {
        if self.channel != channel {
            self.reset_for_channel(channel);
        }

        let complete = segment.ends_with('\n');
        let body = segment.strip_suffix('\n').unwrap_or(segment);
        let body = body.strip_suffix('\r').unwrap_or(body);

        if !self.partial_oversized {
            if self.partial_line.len().saturating_add(body.len()) <= REPEAT_GUARD_MAX_UNIT_BYTES {
                self.partial_line.push_str(body);
            } else {
                self.partial_line.clear();
                self.partial_oversized = true;
                self.recent_lines.clear();
            }
        }

        if !complete {
            return None;
        }
        if self.partial_oversized {
            self.partial_line.clear();
            self.partial_oversized = false;
            return None;
        }

        let line = std::mem::take(&mut self.partial_line)
            .trim_end()
            .to_string();
        let trimmed = line.trim_start();
        let fence = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        if let Some(marker) = fence {
            match self.code_fence_marker {
                Some(open) if open == marker => self.code_fence_marker = None,
                None => self.code_fence_marker = Some(marker),
                _ => {}
            }
            self.recent_lines.clear();
            return None;
        }
        if self.code_fence_marker.is_some() {
            self.recent_lines.clear();
            return None;
        }
        if line.is_empty() {
            return None;
        }

        self.recent_lines.push_back(line);
        while self.recent_lines.len() > REPEAT_GUARD_PERIODIC_HISTORY {
            self.recent_lines.pop_front();
        }
        self.detect_suffix_cycle()
    }

    fn detect_suffix_cycle(&self) -> Option<PeriodicRepeatMatch> {
        let lines: Vec<&String> = self.recent_lines.iter().collect();
        for period in 2..=REPEAT_GUARD_MAX_PERIOD {
            let cycles = REPEAT_GUARD_MIN_PERIODIC_CYCLES
                .max(REPEAT_GUARD_MIN_PERIODIC_LINES.div_ceil(period));
            let required = period * cycles;
            if lines.len() < required {
                continue;
            }
            let suffix = &lines[lines.len() - required..];
            let pattern = &suffix[..period];
            if pattern.iter().all(|line| line == &pattern[0]) {
                continue;
            }
            let alpha_numeric = pattern
                .iter()
                .flat_map(|line| line.chars())
                .filter(|ch| ch.is_alphanumeric())
                .count();
            if alpha_numeric < 8 {
                continue;
            }
            if suffix
                .iter()
                .enumerate()
                .all(|(index, line)| line == &pattern[index % period])
            {
                return Some(PeriodicRepeatMatch { period, cycles });
            }
        }
        None
    }
}

/// 块级复读折叠：对「已完整的整段文本」做一次性复读熔断。
///
/// 用于非流式 / web_search loop 路径（`extract_invoke_content_blocks` 入口）——
/// 那条路不经过流式 `emit_text_delta_raw` 的逐 chunk 熔断，所以在这里独立兜一次。
///
/// 规则与流式版一致：同一个 `STRAY_INVOKE_TOKENS`（call/count/card）连续作为独占一行
/// 重复超过 `REPEAT_GUARD_TRIP_THRESHOLD` 次，判定为 Opus 退化复读，**从超阈值处截断**，
/// 丢弃其后的全部复读垃圾（断雪球、不灌历史）。阈值内的少量引导词重复原样保留。
fn collapse_stray_token_floods(text: &str) -> std::borrow::Cow<'_, str> {
    let mut last_line = "";
    let mut run: u32 = 0;
    let mut cut_at: Option<usize> = None;
    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line = segment.trim();
        if STRAY_INVOKE_TOKENS.contains(&line) {
            if line == last_line {
                run += 1;
            } else {
                last_line = line;
                run = 1;
            }
            if run >= REPEAT_GUARD_TRIP_THRESHOLD {
                // 从「本段（这一行）开头」截断：保留阈值内已累计的内容。
                cut_at = Some(offset);
                break;
            }
        } else if !line.is_empty() {
            last_line = line;
            run = 0;
        }
        offset += segment.len();
    }
    match cut_at {
        Some(pos) => std::borrow::Cow::Owned(text[..pos].to_string()),
        None => std::borrow::Cow::Borrowed(text),
    }
}

fn strip_trailing_stray_tokens(before: &str) -> &str {
    let mut end = before.len();
    loop {
        let bytes = before.as_bytes();
        // 先跳过尾部的换行符，定位“最后一行”的真实结束位置
        let mut e = end;
        while e > 0 && (bytes[e - 1] == b'\n' || bytes[e - 1] == b'\r') {
            e -= 1;
        }
        let line_start = before[..e].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let last_line = before[line_start..e].trim();
        // Opus 长上下文退化时，泄漏的 <invoke> 前常有一个孤立的 stray token 行。
        // 实测样本里出现过 call / count / card 三种；用集合便于以后扩充。
        if STRAY_INVOKE_TOKENS.contains(&last_line) {
            // 只剥 stray token 行本身，【保留】前一行末尾的换行符。
            // 旧实现用 line_start - 1 把前一行的换行也吞掉，会把前面的叙述正文和
            // 后续 <invoke> 挤到同一行，导致 invoke_looks_like_real_leak 的“行首”判定
            // 失败、漏捞真泄漏（narrative\ncall\n<invoke>）。改成 end = line_start：
            //   "some text\ncall" -> "some text\n"（行首信号保留）
            //   "call"（无前导正文）-> ""（line_start==0）
            end = line_start;
            if end == 0 {
                return "";
            }
        } else {
            break;
        }
    }
    &before[..end]
}

/// 判定一个 `<invoke>` 块到底像“真泄漏的工具调用”还是“正文里讨论的文本”
///
/// 实测真泄漏的 `<invoke>` 都出现在**行首**（前面是流的开头、或上一行已经换行结束），
/// 而正文讨论里的 `<invoke>` 一般**嵌在一句话中间**——前面同一行还有普通文字。
///
/// 判定规则（输入 `before` 是 `<invoke>` 之前、已剥过 stray token 的文本）：
/// - `before` 为空（`<invoke>` 在流开头）→ 像真泄漏，抓。
/// - `before` 去掉尾部空格/制表符后以换行结尾（`<invoke>` 独占新行）→ 抓。
/// - 否则（同一行前面还有非空白正文）→ 像讨论文本，不抓。
///
/// 注意：这里的“尾部空白”只剥行内空白（空格 / 制表符），不剥换行；
/// 换行结尾才是“另起一行”的信号。
fn invoke_looks_like_real_leak(before: &str) -> bool {
    // 剥掉尾部的行内空白（空格 / 制表符），但保留换行
    let trimmed = before.trim_end_matches([' ', '\t']);
    // 行首：要么前面什么都没有，要么上一行已经以换行结束
    trimmed.is_empty() || trimmed.ends_with('\n') || trimmed.ends_with('\r')
}

/// 推进「代码围栏」奇偶状态，对切分到多个 chunk 的 ``` 分隔符鲁棒。
///
/// 只在遇到换行符时才对「已重组的完整行」判定是否为围栏行（行首去空白后以 ``` 开头）。
/// 未遇换行的尾部留在 `partial` 里，等后续 chunk 拼齐——所以即使 ``` 被切成
/// `` `` `` + `` ` `` 两个 chunk，重组成完整行后仍能正确翻转 `open`。
///
/// 返回值仅在内部使用；主要副作用是更新 `open` 与 `partial`。
fn advance_code_fence_state(open: &mut bool, partial: &mut String, text: &str) {
    for ch in text.chars() {
        if ch == '\n' {
            if partial.trim_start().starts_with("```") {
                *open = !*open;
            }
            partial.clear();
        } else {
            partial.push(ch);
        }
    }
}

/// 纯函数：在不改动真实状态的前提下，试算「把 `text` 走完之后围栏是否打开」。
/// 用于 drain 决策处判断某个 `<invoke>` 是否落在围栏内。
fn fence_open_after(open: bool, partial: &str, text: &str) -> bool {
    let mut o = open;
    let mut p = partial.to_string();
    advance_code_fence_state(&mut o, &mut p, text);
    // 还要考虑：partial 里残留的「未换行行」如果本身已经是 ``` 开头，
    // 它在遇到换行前不算翻转（保守：只有完整行才翻转）。这里返回已翻转的 o。
    o
}

/// 计算缓冲区末尾“可能是部分 `<invoke` 开标签前缀”的字节数，需要保留等待更多内容
///
/// 例如缓冲区以 `<inv` / `<` / `<i` 结尾时，可能是被切碎的 invoke 开标签，
/// 保留这段尾巴等下一个 chunk 拼齐，避免把半个标签当文本吐出去。
fn partial_invoke_tag_suffix_len(buf: &str) -> usize {
    // 任何形如 `<...`（最后一个 '<' 之后没有 '>'）的尾巴都可能是部分开标签
    if let Some(lt) = buf.rfind('<') {
        if !buf[lt..].contains('>') {
            return buf.len() - lt;
        }
    }
    0
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) => pos,
        None => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) = if let Some(end_pos) = find_real_thinking_end_tag(after_open) {
        (
            &after_open[..end_pos],
            &after_open[end_pos + "</thinking>\n\n".len()..],
        )
    } else if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        let after_tag = end_pos + end_tag_skip_len(after_open, end_pos);
        (&after_open[..end_pos], after_open[after_tag..].trim_start())
    } else {
        // 没有结束标签：模型在思考途中被 max_tokens 截断。此时开标签之后的全部内容
        // 都是推理，应当作 thinking 交付。
        //
        // 原先在这里原样返回整段文本，字面 `<thinking>` 标签和整段推理会一起泄漏进正文
        // （流式侧的同类泄漏见 process_content_with_thinking 的多块支持）。被引用字符
        // 包裹的标签已由 find_real_thinking_start_tag 排除，不会误伤讨论该标签的正文。
        (after_open, "")
    };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw.strip_prefix('\n').unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// 一次性（非流式 / 整段已完整）把 assistant 文本切成 Anthropic content block 序列，
/// 把混在文本里的字面 `<invoke name="...">...</invoke>` 工具调用捞回成结构化 `tool_use`。
///
/// 复用与流式 `drain_invoke_sniff_buffer` **完全相同**的安全判定，避免误抓正文里讨论的命令：
///   ① 行首判定 `invoke_looks_like_real_leak`（块前去 stray token 后须在行首）
///   ② 代码围栏判定 `fence_open_after`（被 ``` 包裹的展示文本不捞）
///   ③ 工具表硬护栏 `known_tool_names`（解析出的工具名必须是本次请求声明的工具）
/// 任一不满足 → 该 `<invoke>` 块当普通文本原样保留。
///
/// 与流式版本的区别：这里输入是**已完整**的整段文本，所以不需要 hold 缓冲、
/// 部分开标签、`MAX_INVOKE_HOLD_BYTES` 那套增量逻辑——直接线性扫描即可。
///
/// 返回的 content block 形态与调用方现有约定一致：
///   - 文本：`{"type":"text","text": "..."}`
///   - 工具：`{"type":"tool_use","id":"toolu_...","name":"...","input": {...}}`
/// 文本块按需合并相邻片段；空文本片段不产出。`input` 解析失败时 fall back 成 `{}`。
///
/// `tool_name_map`（短名 → 原始名）用于把捞回的工具名还原成客户端可识别的原始名，
/// 经统一入口 `CompletedToolUse::from_kiro`；映射为空或命中失败时按原名返回。
pub(crate) fn extract_invoke_content_blocks(
    text: &str,
    known_tool_names: &std::collections::HashSet<String>,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Vec<serde_json::Value> {
    // 🛑 块级复读熔断：先把 Opus 退化的「同一 stray token 连续复读」截断，
    // 再做 invoke 嗅探。覆盖 web_search loop（99.9% 真实流量）这条非流式路径。
    let collapsed = collapse_stray_token_floods(text);
    let text: &str = &collapsed;
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    let mut pending_text = String::new();
    // 围栏奇偶状态：跨「已吐出的文本」累进，确保 ``` 跨片段也能正确判定。
    let mut fence_open = false;
    let mut fence_partial = String::new();

    let push_text = |blocks: &mut Vec<serde_json::Value>, pending: &mut String| {
        if !pending.is_empty() {
            blocks.push(serde_json::json!({"type": "text", "text": pending.clone()}));
            pending.clear();
        }
    };

    let mut rest = text;
    loop {
        let start = match find_invoke_start(rest) {
            Some(s) => s,
            None => {
                pending_text.push_str(rest);
                break;
            }
        };
        let end = match find_invoke_block_end(rest, start) {
            Some(e) => e,
            None => {
                // 块没闭合（整段已完整仍未见 </invoke>）→ 不是干净的工具调用，整段当文本。
                pending_text.push_str(rest);
                break;
            }
        };

        let before = &rest[..start];
        let stripped_before = strip_trailing_stray_tokens(before);
        // ③ 围栏：在「块之前的文本」走完后围栏是否打开
        let fence_after_before = fence_open_after(fence_open, &fence_partial, before);
        // ② 工具名解析 + 工具表护栏
        let parsed = parse_invoke_block(&rest[start..end]);
        let name_known = parsed
            .as_ref()
            .map(|(n, _)| known_tool_names.contains(n))
            .unwrap_or(false);

        if invoke_looks_like_real_leak(stripped_before) && !fence_after_before && name_known {
            // 真泄漏：保留剥过 stray token 的前文（推进围栏），再产出结构化 tool_use。
            if !stripped_before.is_empty() {
                advance_code_fence_state(&mut fence_open, &mut fence_partial, stripped_before);
                pending_text.push_str(stripped_before);
            }
            push_text(&mut blocks, &mut pending_text);
            let (name, input_json) = parsed.expect("parsed is Some when name_known");
            let input: serde_json::Value =
                serde_json::from_str(&input_json).unwrap_or_else(|_| serde_json::json!({}));
            // 统一还原（名字 + 入参）并统一拼块，与结构化 / websearch 路径同口径。
            let tool_use_id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
            let completed = CompletedToolUse::from_kiro(tool_use_id, &name, input, tool_name_map);
            blocks.push(completed.to_anthropic_block());
        } else {
            // 不捞（句中 / 围栏内 / 工具名未知 / 解析失败）→ 整块（含 before）当文本，推进围栏。
            let chunk = &rest[..end];
            advance_code_fence_state(&mut fence_open, &mut fence_partial, chunk);
            pending_text.push_str(chunk);
        }
        rest = &rest[end..];
    }

    push_text(&mut blocks, &mut pending_text);
    blocks
}

fn tool_semantic_key(block: &serde_json::Value) -> Option<String> {
    if block.get("type")?.as_str()? != "tool_use" {
        return None;
    }
    let name = block.get("name")?.as_str()?;
    let input = serde_json::to_string(block.get("input")?).ok()?;
    Some(format!("{name}\0{input}"))
}

/// 固定字段修复后再次去重文本捞回调用与原生结构化调用。
///
/// 只移除与原生调用语义相同的捞回块，原生块即使参数相同也保留各自 ID，避免改变
/// 客户显式并行调用语义。
pub(crate) fn dedupe_reclaimed_tools_after_repair(
    blocks: &mut Vec<serde_json::Value>,
    native_tool_ids: &std::collections::HashSet<String>,
) {
    let native_keys: std::collections::HashSet<String> = blocks
        .iter()
        .filter(|block| {
            block
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| native_tool_ids.contains(id))
        })
        .filter_map(tool_semantic_key)
        .collect();

    blocks.retain(|block| {
        let is_native = block
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| native_tool_ids.contains(id));
        is_native || tool_semantic_key(block).is_none_or(|key| !native_keys.contains(&key))
    });
}

pub(crate) fn normalize_non_stream_content_blocks(
    base_content: Vec<serde_json::Value>,
    native_tool_uses: Vec<serde_json::Value>,
    known_tool_names: &std::collections::HashSet<String>,
    tool_name_map: &HashMap<String, String>,
) -> Vec<serde_json::Value> {
    let native_keys: std::collections::HashSet<String> = native_tool_uses
        .iter()
        .filter_map(tool_semantic_key)
        .collect();
    let mut blocks = Vec::new();
    for block in base_content {
        if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
            let text = block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            blocks.extend(extract_invoke_content_blocks(
                text,
                known_tool_names,
                tool_name_map,
            ));
        } else {
            blocks.push(block);
        }
    }
    blocks.retain(|block| tool_semantic_key(block).is_none_or(|key| !native_keys.contains(&key)));
    blocks.extend(native_tool_uses);
    blocks
}

/// 累积完成的工具调用（`ToolUseEvent` 的所有分片拼接、解析成功后的结果）。
#[derive(Debug, Clone)]
pub struct CompletedToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

impl CompletedToolUse {
    /// 从 Kiro 侧 (name, input) 还原为客户端可见的完整工具调用。
    ///
    /// 这是**唯一的还原入口**：名字按 `tool_name_map` 还原、入参按 Kiro 名反向重写
    /// （见 `converter::restore_tool_use_for_client`）。结构化事件、`<invoke>` 文本捞回、
    /// websearch 三条来源都经此收敛，避免各站点各自调用还原逻辑。
    pub fn from_kiro(
        id: String,
        kiro_name: &str,
        input: serde_json::Value,
        tool_name_map: &HashMap<String, String>,
    ) -> Self {
        let (name, input) =
            super::converter::restore_tool_use_for_client(kiro_name, input, tool_name_map);
        Self { id, name, input }
    }

    /// 产出非流式 Anthropic `tool_use` 内容块。**唯一的非流式块拼装点。**
    pub fn to_anthropic_block(&self) -> serde_json::Value {
        json!({
            "type": "tool_use",
            "id": self.id,
            "name": self.name,
            "input": self.input,
        })
    }
}

/// 工具调用 JSON 累积过程中的错误。
///
/// - `InvalidJson`：上游把某个 tool_use 的完整 `input` 拼出来后，仍不是合法 JSON。
/// - `IncompleteJson`：整条流结束时仍有 tool_use 的 JSON 参数在 EOF 处未写完，即上游在
///   工具参数写到一半时截断（“流式半截 JSON”）。
///
/// 两种情况都**不能**把半截 / 非法 JSON 当成完整工具调用转发给客户端——那会让
/// 客户端拿到无法解析或语义错误的参数去执行工具。这里显式暴露为错误，由上层
/// 决定回 502（非流式 / 缓冲流）或在 SSE 里补一个 `error` 事件（实时流）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolJsonAccumulatorError {
    InvalidJson {
        tool_use_id: String,
        name: String,
        message: String,
    },
    IncompleteJson {
        tool_use_id: String,
        name: String,
        bytes: usize,
    },
}

impl ToolJsonAccumulatorError {
    /// Anthropic error 事件里统一的 error.type。
    pub fn error_type(&self) -> &'static str {
        "upstream_tool_json_error"
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidJson {
                tool_use_id,
                name,
                message,
            } => format!(
                "Upstream returned invalid JSON for tool_use {} ({}): {}",
                tool_use_id, name, message
            ),
            Self::IncompleteJson {
                tool_use_id,
                name,
                bytes,
            } => format!(
                "Upstream ended before completing tool_use {} ({}) JSON input; buffered {} bytes. The tool call was not forwarded to the client.",
                tool_use_id, name, bytes
            ),
        }
    }
}

impl std::fmt::Display for ToolJsonAccumulatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ToolJsonAccumulatorError {}

/// 工具调用参数（JSON）累积器。
///
/// Kiro 把 tool_use 的 `input` JSON 拆成多个 `toolUseEvent` 分片下发，最后一片
/// 带 `stop=true`。分片可能切在 JSON 的任意字节位置（甚至 token 中间），因此
/// **不能**逐片当作 `input_json_delta` 直接转发——必须按 `tool_use_id` 累积，
/// 只在收到 `stop=true` 时整体解析，成功后一次性发出完整的工具调用。
#[derive(Debug, Default)]
pub struct ToolJsonAccumulator {
    /// tool_use_id -> (工具名, 已累积的 JSON 分片)
    buffers: HashMap<String, (String, String)>,
}

impl ToolJsonAccumulator {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    /// 累积一个 `toolUseEvent` 分片。
    ///
    /// - 未收到 `stop` 时返回 `Ok(None)`（继续缓冲，不发出任何事件）。
    /// - 收到 `stop` 时把累积的 JSON 整体解析：成功返回 `Ok(Some(CompletedToolUse))`，
    ///   失败返回 `Err(InvalidJson)`。空参数按 `{}` 处理。
    /// - 工具名按 `tool_name_map` 还原为客户端原始名（短名 → 原名）。
    pub fn push(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
        tool_name_map: &HashMap<String, String>,
    ) -> Result<Option<CompletedToolUse>, ToolJsonAccumulatorError> {
        let entry = self
            .buffers
            .entry(tool_use.tool_use_id.clone())
            .or_insert_with(|| (tool_use.name.clone(), String::new()));
        if entry.0.is_empty() {
            entry.0 = tool_use.name.clone();
        }
        entry.1.push_str(&tool_use.input);

        if !tool_use.stop {
            return Ok(None);
        }

        let (kiro_name, input_json) = self
            .buffers
            .remove(&tool_use.tool_use_id)
            .unwrap_or_else(|| (tool_use.name.clone(), tool_use.input.clone()));
        let input = if input_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str::<serde_json::Value>(&input_json).map_err(|e| {
                ToolJsonAccumulatorError::InvalidJson {
                    tool_use_id: tool_use.tool_use_id.clone(),
                    name: kiro_name.clone(),
                    message: e.to_string(),
                }
            })?
        };

        // 通过统一入口还原客户端工具名 + 入参。
        Ok(Some(CompletedToolUse::from_kiro(
            tool_use.tool_use_id.clone(),
            &kiro_name,
            input,
            tool_name_map,
        )))
    }

    /// 流结束时收尾。对每个从未收到 `stop=true` 的残留缓冲分三种情况处理：
    ///
    /// - **空入参**（0 字节 / 纯空白）：上游把 tool_use 块开出来、但没写任何参数就断流，
    ///   等价于无参工具调用（如 `EnterPlanMode`）。按 `{}` **打捞**成完整工具调用返回，
    ///   与 base 版（首片即发 `content_block_start{input:{}}`、收尾补 `content_block_stop`）
    ///   行为一致——避免把合法的无参调用整个丢弃、连累整轮失败。
    ///
    ///   **但仅限该工具的客户端 schema 没有 required 字段时**。若客户端声明了 required
    ///   （如 `Read.file_path`、`Bash.command`），`{}` 必然撞 missing required，把本可
    ///   透明重试的 `IncompleteJson` 降级成**不可重试**的 schema 硬失败（线上 29 次/天）。
    ///   这种情况改报 `IncompleteJson`，让 EOF 重试路径正常生效。
    /// - **完整 JSON**：把流结束视作隐式 `stop`，打捞成完整工具调用。
    /// - **错误 JSON**：EOF 截断记为 `IncompleteJson`，其他语法错误记为 `InvalidJson`。
    ///
    /// 收尾按批次原子提交：先解析全部残留，任一项错误就不返回本批任何工具调用；全部
    /// 为空或完整时才统一返回。调用后缓冲被清空，重复调用返回 `(空, None)`。
    pub fn finish(
        &mut self,
        tool_name_map: &HashMap<String, String>,
        tool_contracts: &HashMap<String, super::tool_schema::ToolContract>,
    ) -> (Vec<CompletedToolUse>, Option<ToolJsonAccumulatorError>) {
        // 稳定顺序：按 tool_use_id 排序，保证多缓冲时输出与代表错误选取的确定性。
        let mut entries: Vec<(String, (String, String))> = self.buffers.drain().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut parsed = Vec::with_capacity(entries.len());
        let mut error = None;
        for (tool_use_id, (kiro_name, input_json)) in entries {
            let input = if input_json.trim().is_empty() {
                // 空入参只在「客户端 schema 无 required 字段」时才按无参工具打捞。
                // 否则 {} 必然 missing required → 不可重试硬失败，改报可重试的 IncompleteJson。
                if contract_requires_fields(tool_name_map, tool_contracts, &kiro_name) {
                    if error.is_none() {
                        error = Some(ToolJsonAccumulatorError::IncompleteJson {
                            tool_use_id,
                            name: kiro_name,
                            bytes: 0,
                        });
                    }
                    continue;
                }
                serde_json::json!({})
            } else {
                match serde_json::from_str::<serde_json::Value>(&input_json) {
                    Ok(input) => input,
                    Err(parse_error) => {
                        if error.is_none() {
                            error = Some(if parse_error.is_eof() {
                                ToolJsonAccumulatorError::IncompleteJson {
                                    tool_use_id,
                                    name: kiro_name,
                                    bytes: input_json.len(),
                                }
                            } else {
                                ToolJsonAccumulatorError::InvalidJson {
                                    tool_use_id,
                                    name: kiro_name,
                                    message: parse_error.to_string(),
                                }
                            });
                        }
                        continue;
                    }
                }
            };
            parsed.push((tool_use_id, kiro_name, input));
        }

        if let Some(error) = error {
            return (Vec::new(), Some(error));
        }

        let completed = parsed
            .into_iter()
            .map(|(tool_use_id, kiro_name, input)| {
                CompletedToolUse::from_kiro(tool_use_id, &kiro_name, input, tool_name_map)
            })
            .collect();
        (completed, None)
    }
}

/// 该 Kiro 侧工具名对应的**客户端契约**是否声明了 required 字段。
///
/// 名字解析与 [`CompletedToolUse::from_kiro`] 保持同一口径（`tool_name_map`: kiro名→客户端名），
/// 保证「打捞判定」和「后续 schema 校验」看的是同一份契约。契约缺失（未声明工具）时返回
/// false——那条路由由 `emit_completed_tool_use` 的未声明分支处理，不在这里抢先报错。
fn contract_requires_fields(
    tool_name_map: &HashMap<String, String>,
    tool_contracts: &HashMap<String, super::tool_schema::ToolContract>,
    kiro_name: &str,
) -> bool {
    let client_name = tool_name_map
        .get(kiro_name)
        .map(String::as_str)
        .unwrap_or(kiro_name);
    tool_contracts
        .get(client_name)
        .and_then(|contract| contract.schema.get("required"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|required| !required.is_empty())
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 断流时仍未闭合的块类型（取索引最小的那个）。
    ///
    /// 这是判断续写是否安全的关键输入，三种情形结论完全不同：
    /// - 断在 `text` 中：可以续写，把已下发文本作为 assistant 历史接上；
    /// - 断在 `tool_use` 中：**不能**续写。半截入参 JSON 接不回来，更要紧的是续写
    ///   可能产出重复的 tool_use，客户端会把副作用真的执行两遍（同一个文件写两次）；
    /// - 断在 `thinking` 中：危险。续写产生的新 thinking 块签名对不上，客户端校验
    ///   失败会直接报错，比不续写更糟。
    ///
    /// 返回 `None` 表示断在块边界上——那是最安全的续写位置。
    pub(crate) fn open_block_type(&self) -> Option<&str> {
        let mut open = self
            .active_blocks
            .iter()
            .filter(|(_, block)| block.started && !block.stopped)
            .collect::<Vec<_>>();
        open.sort_by_key(|(index, _)| **index);
        open.first().map(|(_, block)| block.block_type.as_str())
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 本轮是否实际发出过 tool_use content block。
    pub fn has_tool_use(&self) -> bool {
        self.has_tool_use
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 开始同一条 Anthropic message 的下一轮上游续写。
    /// 保留 message_start 与单调块索引，只重置本轮终态。
    fn reset_for_continuation(&mut self) {
        self.message_delta_sent = false;
        self.message_ended = false;
        self.stop_reason = None;
        self.active_blocks
            .retain(|_, block| block.started && !block.stopped);
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_input_tokens: i32,
        cache_read_input_tokens: i32,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "cache_creation_input_tokens": cache_creation_input_tokens,
                        "cache_read_input_tokens": cache_read_input_tokens
                    }
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::get_context_window_size;

const CONTINUATION_OVERLAP_MAX_BYTES: usize = 8 * 1024;

#[derive(Debug)]
struct ContinuationOverlapFilter {
    previous_tail: String,
    pending: String,
    resolved: bool,
}

impl ContinuationOverlapFilter {
    fn new(previous: &str) -> Self {
        let start = find_char_boundary(
            previous,
            previous
                .len()
                .saturating_sub(CONTINUATION_OVERLAP_MAX_BYTES),
        );
        Self {
            previous_tail: previous[start..].to_string(),
            pending: String::new(),
            resolved: false,
        }
    }

    fn push(&mut self, text: &str) -> String {
        if self.resolved {
            return text.to_string();
        }
        self.pending.push_str(text);
        if self.pending.len() < CONTINUATION_OVERLAP_MAX_BYTES
            && self.pending_can_extend_existing_suffix()
        {
            return String::new();
        }
        self.resolve()
    }

    fn finish(mut self) -> String {
        self.resolve()
    }

    fn pending_can_extend_existing_suffix(&self) -> bool {
        if self.pending.is_empty() {
            return true;
        }
        self.previous_tail
            .char_indices()
            .map(|(index, _)| &self.previous_tail[index..])
            .any(|suffix| suffix.len() > self.pending.len() && suffix.starts_with(&self.pending))
    }

    fn resolve(&mut self) -> String {
        if self.resolved {
            return std::mem::take(&mut self.pending);
        }
        let overlap = self
            .previous_tail
            .char_indices()
            .map(|(index, _)| &self.previous_tail[index..])
            .filter(|suffix| self.pending.starts_with(*suffix))
            .map(str::len)
            .max()
            .unwrap_or(0);
        let overlap = find_char_boundary(&self.pending, overlap);
        let output = self.pending[overlap..].to_string();
        self.pending.clear();
        self.resolved = true;
        output
    }
}

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 本请求开始时解析出的不可变上下文窗口快照。
    context_window_size: i32,
    /// 见 [`Self::set_context_window_signal_threshold_pct`]。默认 85%。
    context_window_signal_threshold_pct: f64,
    /// 消息 ID
    pub message_id: String,
    /// 客户端可见输入与 Kiro 整体上下文占用的双轨计量。
    input_usage: super::usage::InputTokenUsage,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// 本次请求声明的所有工具名（原始 client 名）。`<invoke>` 文本容错的灾难兜底：
    /// 只有合成名在此集合里才允许捞回成结构化 tool_use，否则当文本吐出。
    /// 为空（请求未带 tools）时不捞回任何 invoke——宁可漏捞，不可误执行。
    pub known_tool_names: std::collections::HashSet<String>,
    /// 客户端可见工具名对应的输入契约。工具 JSON 完整解析后、产生任何工具事件前验证。
    tool_contracts: HashMap<String, super::tool_schema::ToolContract>,
    /// 请求入口是否已显式初始化契约层。生产路径即使没有声明工具也会初始化为空，
    /// 从而拒绝上游幻觉出的未请求工具；低层构造器保持向后兼容。
    tool_contracts_initialized: bool,
    /// 跨整条流的「代码围栏」奇偶状态：每遇到一行以 ``` 开头就翻转。
    /// 在围栏内（true）时，`<invoke>` 一律不捞回（视为正文展示的代码块）。
    pub code_fence_open: bool,
    /// 围栏检测的「未完成行」累加器：只在遇到换行时才对完整行判定是否为 ``` 围栏行。
    /// 这样即使 ``` 分隔符被切分到多个 chunk（如 `` `` + ` ``），重组成完整行后仍能正确识别。
    pub fence_scan_partial: String,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// invoke 文本嗅探缓冲区（用于从明文流里嗅探字面 `<invoke>` 工具调用块）
    pub invoke_sniff_buffer: String,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 上游原生 reasoningContentEvent 下发的 thinking 签名
    pending_thinking_signature: Option<String>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// 中转层 CacheMeter 的缓存覆盖情况（estimate 口径）。最终上报时按客户端可见 total
    /// 做互斥分摊：`input + cache_creation + cache_read == total`，避免把被缓存
    /// 覆盖的前缀重复计进 input_tokens。
    pub cache_usage: super::cache_metering::CacheUsage,
    /// meteringEvent 上报的 credit 计费量（上游真实下发）
    pub credits: f64,
    /// 复读熔断：最近一次候选来自 text 还是 thinking；通道切换时重置连续计数。
    repeat_guard_last_channel: &'static str,
    /// 复读熔断：最近一次候选。只去掉行尾空白，保留行首缩进参与比较。
    repeat_guard_last_line: String,
    /// 复读熔断：跨 chunk 的行重组缓冲。
    ///
    /// 上游分片不按行边界切（实测形态：`"NEXT ITEM."` → `"\nCHECK NE"` → `"XT ITEM"`），
    /// 直接比较 `split_inclusive('\n')` 的产物等于在比较碎片，每次都不同，连续计数永不
    /// 累积——正文通道的熔断因此从未生效。这里先攒够一整行再比较。
    repeat_guard_partial: String,
    /// 行重组缓冲是否已超过单行上限（超长行不参与比较，避免无界增长）。
    repeat_guard_partial_oversized: bool,
    /// 复读熔断：当前尾行已连续重复的次数。
    repeat_guard_run: u32,
    /// 多行周期复读检测；只保存少量完整短行，不保存整段响应。
    repeat_guard_periodic: PeriodicRepeatGuard,
    /// 复读熔断：thinking 通道是否已跳闸。
    ///
    /// 与 `repeat_guard_tripped` 分开：thinking 不是最终交付物，它复读了只需要停掉
    /// thinking 输出，正文与工具调用必须照常走完。合在一起时一次 thinking 误判会把
    /// 整轮响应连 tool_use 一起丢掉，客户端的 agentic 循环直接断在半路——SSE 流少了
    /// `message_stop`，表现为「工具调用凭空消失、会话卡住」。
    thinking_repeat_tripped: bool,
    /// 复读熔断：正文通道是否已跳闸（触发后本轮后续文本一律丢弃，不再吐、不写历史）。
    repeat_guard_tripped: bool,
    /// 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，`stop` 时整体解析，
    /// 避免把“流式半截 JSON”当成完整工具调用转发。
    tool_json_accumulator: ToolJsonAccumulator,
    /// 工具调用 JSON 错误（非法 / 半截）。一旦置位，收尾时补发 `error` 事件，
    /// 上层据此把本次请求记为 error 而非 success。
    tool_json_error: Option<ToolJsonAccumulatorError>,
    /// 跨 chunk 过滤混入 assistant 文本的字面 `<tool_use>` XML 泄漏。
    tool_use_xml_filter: ToolUseXmlLeakFilter,
    /// 身份归一化流式过滤器（跨 chunk 把整词 Kiro→Claude）。仅在 config 开启时生效。
    identity_filter: Option<super::identity::IdentityStreamFilter>,
    /// 是否发生过「未声明工具降级成文本」。用于让 `saw_upstream_tool_use` 那条
    /// 「收到 tool_use 却没发出工具块」的协议守卫放行本策略。
    degraded_undeclared_tool: bool,
    /// 是否收到过上游 toolUseEvent；与实际发出工具块分开记录。
    saw_upstream_tool_use: bool,
    /// 是否实际向客户端发出过非空文本、thinking、redacted thinking 或工具块。
    has_visible_output: bool,
    /// 非工具 JSON 类的终止协议错误。
    terminal_protocol_error: Option<String>,
    /// 工具选择策略，用于在流结束时验证上游输出。
    tool_choice_policy: super::converter::ToolChoicePolicy,
    /// required tool 模式下暂存工具前的模型旁白，避免先创建 text block。
    required_tool_preamble: String,
    /// 暂存旁白是否已经释放或丢弃。
    required_tool_preamble_released: bool,
    /// 已实际发送给客户端的工具名。
    emitted_tool_names: Vec<String>,
    /// 客户端禁用并行工具时，首个原始工具分片占用的本轮唯一工具 ID。
    /// 在 JSON 拼接前占位，避免后续非法或半截工具污染首个工具的成功响应。
    single_tool_slot_id: Option<String>,
    /// 唯一工具是否已经完成解析或进入过 Schema 校验；关闭后同 ID 重放也必须抑制。
    single_tool_slot_closed: bool,
    /// 终止协议错误的 Anthropic 错误类型。
    terminal_protocol_error_type: Option<&'static str>,
    /// 是否观察到真实 thinking 或 redacted_thinking 输出。
    saw_reasoning_output: bool,
    /// thinking 内容的来源通道，先到者独占。
    ///
    /// 上游可能同时下发原生 `reasoningContentEvent` **和**正文里的字面 `<thinking>` 标签，
    /// 两条路都会生成 thinking 块，客户端于是看到同一段推理出现两遍。互锁后只认先到的
    /// 那条通道，另一条的内容直接丢弃。
    ///
    /// 三态设计来自 Kiro-Go 的 `thinkingStreamSource`（proxy/handler.go）。
    thinking_source: ThinkingSource,
    /// 当前字面 `<thinking>` 块的内容是否要丢弃（原生 reasoning 已占据通道）。
    ///
    /// 丢弃时标签仍照常解析消费，只是不建 thinking 块、不发 delta——否则标签会泄漏成正文。
    drop_tag_thinking: bool,
    /// 缺少真实 reasoning 时是否按严格协议终止流。
    strict_thinking_validation: bool,
    /// AWS event-stream 的语义与显式失败信号。
    attempt_observation: super::tool_attempt::AttemptObservation,
    /// 收尾后统一得到的 attempt 失败分类。
    terminal_attempt_failure: Option<super::tool_attempt::AttemptFailure>,
    /// 已真实发送给客户端的正文。用于构造纯文本续写请求，不包含 thinking/tool_use。
    accumulated_text: String,
    /// 续写首段接缝去重器；只在自动续写轮启用。
    continuation_overlap: Option<ContinuationOverlapFilter>,
}

fn events_have_visible_output(events: &[SseEvent]) -> bool {
    events.iter().any(|event| {
        if event.event == "content_block_start" {
            return matches!(
                event.data["content_block"]["type"].as_str(),
                Some("tool_use" | "redacted_thinking")
            );
        }
        if event.event != "content_block_delta" {
            return false;
        }
        let delta = &event.data["delta"];
        delta["text"].as_str().is_some_and(|text| !text.is_empty())
            || delta["thinking"]
                .as_str()
                .is_some_and(|thinking| !thinking.is_empty())
            || delta["partial_json"]
                .as_str()
                .is_some_and(|json| !json.is_empty())
    })
}

impl StreamContext {
    /// 解析最终上报口径的 `(input_tokens, cache_creation, cache_read)`。
    ///
    /// Anthropic API 始终使用客户端可见输入，再由缓存计量做互斥分摊。
    pub fn resolved_usage(&self) -> (i32, i32, i32) {
        self.input_usage.split_api(&self.cache_usage)
    }

    /// **内部（真实互斥）口径**，不受 per-key 计费模式影响。供 traces.db / 利润报表用。
    ///
    /// 与 [`Self::resolved_usage`] 分开是刻意的：对外可以按同行口径（legacy）收费，
    /// 但我们自己的账必须按互斥口径记——否则被缓存覆盖的前缀会重复计一次，报表虚高。
    pub fn internal_usage(&self) -> (i32, i32, i32) {
        self.input_usage.split_internal(&self.cache_usage)
    }

    /// Kiro 上报的整体上下文占用，只用于日志和上下文护栏。
    pub fn upstream_context_tokens(&self) -> Option<i32> {
        self.input_usage.upstream_context_tokens()
    }

    pub fn set_context_window_size(&mut self, value: i32) {
        self.context_window_size = value.max(1);
    }

    /// 设置下发 `model_context_window_exceeded` 的占比阈值。
    ///
    /// 夹到 `(0, 100]`：0 或负数会让每个请求都被判超限，>100 等于永不触发。
    /// 非法值退回 100.0（原行为），宁可不触发也不误触发。
    pub fn set_context_window_signal_threshold_pct(&mut self, value: f64) {
        self.context_window_signal_threshold_pct = if value.is_finite() && value > 0.0 {
            value.min(100.0)
        } else {
            100.0
        };
    }

    pub(crate) fn set_tool_contracts(
        &mut self,
        contracts: HashMap<String, super::tool_schema::ToolContract>,
    ) {
        self.tool_contracts = contracts;
        self.tool_contracts_initialized = true;
    }

    /// 工具调用 JSON 错误信息（非法 / 半截）。上层据此把本次请求记为 error、
    /// 或在非流式路径返回 502。无错误时返回 `None`。
    pub fn terminal_error_message(&self) -> Option<String> {
        self.terminal_attempt_failure
            .as_ref()
            .map(|failure| failure.public_error().1)
            .or_else(|| self.terminal_protocol_error.clone())
            .or_else(|| self.tool_json_error.as_ref().map(|err| err.message()))
    }

    pub(crate) fn terminal_error_type(&self) -> Option<&'static str> {
        self.terminal_attempt_failure
            .as_ref()
            .map(|failure| failure.public_error().0)
            .or_else(|| {
                self.tool_json_error
                    .as_ref()
                    .map(ToolJsonAccumulatorError::error_type)
            })
            .or(self.terminal_protocol_error_type)
    }

    pub(crate) fn terminal_attempt_failure(&self) -> Option<&super::tool_attempt::AttemptFailure> {
        self.terminal_attempt_failure.as_ref()
    }

    /// 上游正文是否已被复读熔断器判定为退化输出。
    ///
    /// 只反映**正文**通道。thinking 通道跳闸见 `thinking_repetition_guard_tripped`——
    /// 它不算本轮失败，正文与工具调用照常交付，故不进入这个判定。
    pub fn repetition_guard_tripped(&self) -> bool {
        self.repeat_guard_tripped
    }

    /// 断流时未闭合的块类型，`None` 表示断在块边界上。
    /// 语义与续写安全性的对应关系见 [`SseStateManager::open_block_type`]。
    pub(crate) fn open_block_type(&self) -> Option<&str> {
        self.state_manager.open_block_type()
    }

    /// 供断流路径打点用的稳定标签，不返回 `Option` 以便直接进日志与 trace。
    pub(crate) fn break_block_label(&self) -> &str {
        self.open_block_type().unwrap_or("block_boundary")
    }

    /// thinking 通道是否已被复读熔断器静音。
    ///
    /// 与 `repetition_guard_tripped` 分开：thinking 复读只停掉 thinking 输出，
    /// 本轮仍算成功，不记 error、不中止正文与工具调用。
    ///
    /// 仅测试使用：线上可见性由 `repeat_guard_filter` 的 `thinking_only=true`
    /// 日志字段提供，无需额外的运行时消费者。
    #[cfg(test)]
    pub fn thinking_repetition_guard_tripped(&self) -> bool {
        self.thinking_repeat_tripped
    }

    pub(crate) fn accumulated_text(&self) -> &str {
        &self.accumulated_text
    }

    pub(crate) fn current_stop_reason(&self) -> String {
        self.state_manager.get_stop_reason()
    }

    pub(crate) fn saw_tool_use(&self) -> bool {
        self.saw_upstream_tool_use || self.state_manager.has_tool_use()
    }

    pub(crate) fn saw_reasoning_output(&self) -> bool {
        self.saw_reasoning_output
    }

    pub(crate) fn has_terminal_error(&self) -> bool {
        self.terminal_error_message().is_some()
    }

    pub(crate) fn begin_continuation(&mut self) {
        self.continuation_overlap = Some(ContinuationOverlapFilter::new(&self.accumulated_text));
    }

    pub(crate) fn flush_continuation_overlap(&mut self) -> Vec<SseEvent> {
        let Some(filter) = self.continuation_overlap.take() else {
            return Vec::new();
        };
        let content = filter.finish();
        if content.is_empty() {
            Vec::new()
        } else {
            self.process_filtered_assistant_content(&content)
        }
    }

    /// 已发送中间轮的 content_block_stop 后调用；不重复发送 message_start。
    pub(crate) fn prepare_for_continuation(&mut self) {
        self.state_manager.reset_for_continuation();
        self.text_block_index = None;
        self.thinking_block_index = None;
        self.in_thinking_block = false;
        self.thinking_extracted = false;
        self.thinking_buffer.clear();
        self.invoke_sniff_buffer.clear();
        self.repeat_guard_last_channel = "";
        self.repeat_guard_last_line.clear();
        self.repeat_guard_partial.clear();
        self.repeat_guard_partial_oversized = false;
        self.repeat_guard_run = 0;
        self.repeat_guard_periodic = PeriodicRepeatGuard::default();
        self.repeat_guard_tripped = false;
        self.thinking_repeat_tripped = false;
        // 只清块级的丢弃标记；`thinking_source` 的通道锁**跨续写保持**——续写是同一轮
        // 逻辑响应的延续，重置会让另一条通道中途抢占，重新引入推理输出重复两遍。
        self.drop_tag_thinking = false;
        self.tool_json_accumulator = ToolJsonAccumulator::new();
        self.tool_json_error = None;
        self.tool_use_xml_filter = ToolUseXmlLeakFilter::default();
        self.saw_upstream_tool_use = false;
        self.has_visible_output = false;
        self.terminal_protocol_error = None;
        self.terminal_protocol_error_type = None;
        self.terminal_attempt_failure = None;
        self.attempt_observation = super::tool_attempt::AttemptObservation::default();
    }

    /// 返回工具 JSON 的 typed 终态，供 handler 精确区分可重试的 EOF 半截与其他错误。
    #[cfg(test)]
    pub fn terminal_tool_json_error(&self) -> Option<&ToolJsonAccumulatorError> {
        self.tool_json_error.as_ref()
    }

    /// 创建 StreamContext
    #[cfg(test)]
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
    ) -> Self {
        Self::new_with_constraints(
            model,
            input_tokens,
            thinking_enabled,
            false,
            tool_name_map,
            known_tool_names,
            super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: false,
            },
        )
    }

    pub fn new_with_constraints(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        strict_thinking_validation: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
        tool_choice_policy: super::converter::ToolChoicePolicy,
    ) -> Self {
        let model = model.into();
        let context_window_size = get_context_window_size(&model).max(1);
        Self {
            state_manager: SseStateManager::new(),
            model,
            context_window_size,
            context_window_signal_threshold_pct: 85.0,
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_usage: super::usage::InputTokenUsage::new(input_tokens),
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            known_tool_names,
            tool_contracts: HashMap::new(),
            tool_contracts_initialized: false,
            degraded_undeclared_tool: false,
            code_fence_open: false,
            fence_scan_partial: String::new(),
            thinking_enabled,
            thinking_buffer: String::new(),
            invoke_sniff_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            pending_thinking_signature: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            cache_usage: super::cache_metering::CacheUsage::default(),
            credits: 0.0,
            repeat_guard_last_channel: "",
            repeat_guard_last_line: String::new(),
            repeat_guard_partial: String::new(),
            repeat_guard_partial_oversized: false,
            repeat_guard_run: 0,
            repeat_guard_periodic: PeriodicRepeatGuard::default(),
            repeat_guard_tripped: false,
            thinking_repeat_tripped: false,
            tool_json_accumulator: ToolJsonAccumulator::new(),
            tool_json_error: None,
            tool_use_xml_filter: ToolUseXmlLeakFilter::default(),
            identity_filter: None,
            saw_upstream_tool_use: false,
            has_visible_output: false,
            terminal_protocol_error: None,
            tool_choice_policy,
            required_tool_preamble: String::new(),
            required_tool_preamble_released: false,
            emitted_tool_names: Vec::new(),
            single_tool_slot_id: None,
            single_tool_slot_closed: false,
            terminal_protocol_error_type: None,
            saw_reasoning_output: false,
            thinking_source: ThinkingSource::default(),
            drop_tag_thinking: false,
            strict_thinking_validation,
            attempt_observation: super::tool_attempt::AttemptObservation::default(),
            terminal_attempt_failure: None,
            accumulated_text: String::new(),
            continuation_overlap: None,
        }
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.input_usage.client_visible_tokens(),
                    "output_tokens": 1,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled || self.tool_choice_policy.is_required() {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        self.attempt_observation.observe(event);
        let events = match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(reasoning),
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let actual_input_tokens = (context_usage.context_usage_percentage
                    * (self.context_window_size as f64)
                    / 100.0) as i32;
                self.input_usage
                    .observe_upstream_context(actual_input_tokens);
                // 上下文占比越过阈值时下发 model_context_window_exceeded，让客户端压缩历史。
                //
                // 原实现写死 100%，是**事后通知**：那时压缩自己也没余量了——compact 请求
                // 同样带全量历史、同样撞上游字节上限，形成死锁。线上实测 240 分钟内信号
                // 发了 5 次，会话仍然一路死在 400。阈值默认降到 85%，给压缩留 15% 窗口。
                //
                // 这条路径不动 usage 上报，所以对下游（NewAPI 按 message_start.input_tokens
                // 扣费）零计费影响。
                if context_usage.context_usage_percentage
                    >= self.context_window_signal_threshold_pct
                {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                let upstream_context_tokens = self
                    .upstream_context_tokens()
                    .unwrap_or(actual_input_tokens);
                tracing::debug!(
                    client_visible_tokens = self.input_usage.client_visible_tokens(),
                    upstream_context_tokens,
                    context_usage_percentage = context_usage.context_usage_percentage,
                    "received upstream context usage"
                );
                Vec::new()
            }
            Event::Metering(metering) => {
                // 上游 meteringEvent 只下发 credit；token / cache 字段不存在。
                self.credits += metering.usage;
                tracing::debug!("metering credits +{:.6}", metering.usage);
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                let safe_reason = super::tool_attempt::safe_upstream_error_reason(error_message);
                tracing::error!(
                    error_type = %error_code,
                    reason = safe_reason.as_deref().unwrap_or("[redacted]"),
                    "收到上游错误事件"
                );
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 处理 ContentLengthExceededException
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                }
                let safe_reason = super::tool_attempt::safe_upstream_error_reason(message);
                tracing::warn!(
                    error_type = %exception_type,
                    reason = safe_reason.as_deref().unwrap_or("[redacted]"),
                    "收到上游异常事件"
                );
                Vec::new()
            }
            _ => Vec::new(),
        };
        if events_have_visible_output(&events) {
            self.has_visible_output = true;
        }
        events
    }

    /// 处理助手响应事件
    /// 开启流式身份归一化（Kiro→Claude 跨 chunk 改写）。由 handler 依据 config 调用。
    pub fn enable_identity_filter(&mut self) {
        self.identity_filter = Some(super::identity::IdentityStreamFilter::default());
    }

    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        // 先过滤字面 <tool_use> XML 泄漏（跨 chunk）。过滤后为空可能是过滤器在缓冲
        // 半个标签，直接返回；后续 token 估算与文本处理都用过滤后内容
        // （被剥离的 XML 因此不计入 output_tokens）。
        let content = self.tool_use_xml_filter.filter(content);
        if content.is_empty() {
            return Vec::new();
        }
        // 身份归一化（流式，跨 chunk 整词 Kiro→Claude）。仅 config 开启时生效；
        // 过滤器可能缓冲末尾疑似 "kiro" 前缀（≤3 字符），此时本轮可能产出空串。
        let identity_out = self.identity_filter.as_mut().map(|f| f.push(&content));
        let content = match &identity_out {
            Some(s) => s.as_str(),
            None => content.as_str(),
        };
        if content.is_empty() {
            return Vec::new();
        }

        let continuation_out = self
            .continuation_overlap
            .as_mut()
            .map(|filter| filter.push(content));
        let content = match &continuation_out {
            Some(value) => value.as_str(),
            None => content,
        };
        if content.is_empty() {
            return Vec::new();
        }

        self.process_filtered_assistant_content(content)
    }

    fn process_filtered_assistant_content(&mut self, content: &str) -> Vec<SseEvent> {
        if self.tool_choice_policy.is_required()
            && !self.saw_upstream_tool_use
            && !self.required_tool_preamble_released
        {
            self.required_tool_preamble.push_str(content);
            return Vec::new();
        }

        let mut events = Vec::new();
        if self.is_thinking_block_open() && !self.in_thinking_block {
            events.extend(self.close_open_thinking_block());
        }

        // 估算 tokens
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            events.extend(self.process_content_with_thinking(content));
            return events;
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        events.extend(self.create_text_delta_events(content));
        events
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            // 不再要求 `!thinking_extracted`：一轮响应里 thinking 块可以出现多次
            // （想一下 → 调工具 → 看结果 → 再想一下）。旧条件在第一个块闭合后永久
            // 锁死入口，第二个及之后的 `<thinking>` 会落到 else 分支被当普通正文原样
            // 吐出，客户端就会看到裸的 `<thinking>` 标签加整段推理内容。
            if !self.in_thinking_block {
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    // 通道互锁：原生 reasoning 事件已占据 thinking 通道时，本块内容全部
                    // 丢弃——但标签仍要照常解析消费，否则会泄漏成正文。
                    self.drop_tag_thinking = !self.thinking_source.allow_tag();
                    if self.drop_tag_thinking {
                        tracing::debug!(
                            message_id = %self.message_id,
                            "dropping tag thinking block: channel already owned by native reasoning"
                        );
                        self.thinking_block_index = None;
                        continue;
                    }

                    self.saw_reasoning_output = true;

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    // 首块之前：保守扣住整个标签长度，保证 text 块不会抢在 thinking 块前面。
                    // 首块之后：排序已定，只扣真正像半截开标签的尾巴，否则短正文
                    //（如仅 6 字节的「你好」）会被永久扣在缓冲区里等一个不会来的标签。
                    let hold = if self.thinking_extracted {
                        partial_open_tag_suffix_len(&self.thinking_buffer)
                    } else {
                        "<thinking>".len()
                    };
                    let target_len = self.thinking_buffer.len().saturating_sub(hold);
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        //
                        // 该抑制**只在首个 thinking 块之前**成立：它要解决的是「text 块
                        // 抢在 thinking 块前面」的排序问题。首块已提交后排序已定，此时
                        // 继续抑制空白会让块间的换行被永久扣在缓冲区里，破坏正文排版。
                        if !safe_content.is_empty()
                            && (self.thinking_extracted || !safe_content.trim().is_empty())
                        {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_pos) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            if let Some(event) = self.create_guarded_thinking_delta_event(
                                thinking_index,
                                &thinking_content,
                            ) {
                                events.push(event);
                            }
                        }
                    }

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    // 下一个块重新判定归属（通道锁本身不变，仅清掉本块的丢弃标记）。
                    self.drop_tag_thinking = false;

                    // 发送空的 thinking_delta 事件，然后发送 content_block_stop 事件
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 剥离闭标签本身，再吃掉紧跟的换行（最多两个）。
                    //
                    // 不能再硬编码 `</thinking>\n\n` 的长度：闭标签检测已不要求后跟
                    // `\n\n`，后面可能是 `\nDone.`、`Done.` 或 ` Done.`。按固定长度切会
                    // 吃掉正文开头的字符。
                    let mut after = end_pos + end_tag_skip_len(&self.thinking_buffer, end_pos);
                    let mut eaten = 0;
                    while eaten < 2 && self.thinking_buffer[after..].starts_with('\n') {
                        after += 1;
                        eaten += 1;
                    }
                    self.thinking_buffer = self.thinking_buffer[after..].to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>\n\n` 的内容：
                    // find_real_thinking_end_tag 要求标签后有 `\n\n` 才返回 Some，
                    // 因此保留区必须覆盖 `</thinking>\n\n` 的完整长度（13 字节），
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    //
                    // 复读退化补充：上游会连着吐多个 `</thinking>`（见
                    // trailing_end_tag_run_start）。固定 13 字节的保留区挡不住整串，
                    // 会把前面几个完整标签当正文发出。这里改成「13 字节」与「末尾标签串
                    // 长度」取较大者——**只放大、不缩小**，故不会让原本该发的正文被扣住，
                    // 更不会造成提前截断；多留的部分由收尾 flush 统一剥离。
                    let default_hold = "</thinking>\n\n".len();
                    let run_hold = trailing_end_tag_run_start(&self.thinking_buffer)
                        .map(|start| self.thinking_buffer.len() - start)
                        .unwrap_or(0);
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub(default_hold.max(run_hold));
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                if let Some(event) = self.create_guarded_thinking_delta_event(
                                    thinking_index,
                                    &safe_content,
                                ) {
                                    events.push(event);
                                }
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            }
        }

        events
    }

    /// 创建 text_delta 事件（带 invoke 嗅探的统一明文漏斗）
    ///
    /// 这是 thinking / 非 thinking 两条路径 + 两个端点唯一共用的明文出口。
    /// 在这里把文本累进 `invoke_sniff_buffer`，循环嗅探完整的字面 `<invoke>` 工具调用块：
    /// - 命中完整块：先把块前文本（剥掉尾部独立的 `call`/`count` 行）走 `emit_text_delta_raw` 吐出，
    ///   再合成结构化 tool_use 事件，再继续循环；
    /// - 未命中完整块：保留可能的部分标签尾巴留在缓冲区，其余走 `emit_text_delta_raw`。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        self.invoke_sniff_buffer.push_str(text);
        self.drain_invoke_sniff_buffer(false)
    }

    /// 行首未闭合 `<invoke` 块的字节上限。仅用于防止"行首一个永不闭合的 `<invoke`
    /// 把整条流永久 hold 住"这种极端情况；正常的 invoke（哪怕是大 patch）都远小于此，
    /// 所以不会误杀合法的多行/分片工具调用。
    const MAX_INVOKE_HOLD_BYTES: usize = 262_144;

    /// 嗅探并排空 `invoke_sniff_buffer`
    ///
    /// - `flush=false`（流式中途）：未命中完整块时，保留可能是部分标签的尾巴（最长一个未闭合
    ///   `<invoke` 块或一段疑似开标签前缀），其余前缀文本走 `emit_text_delta_raw` 吐出。
    /// - `flush=true`（流末尾）：不再保留尾巴，剩余全部走 `emit_text_delta_raw` 吐出（防尾字节丢）。
    fn drain_invoke_sniff_buffer(&mut self, flush: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // Drive the loop on an owned local buffer taken out of `self` ONCE, instead of
        // cloning `self.invoke_sniff_buffer` on every iteration. Under degraded-model
        // floods this buffer can grow up to MAX_INVOKE_HOLD_BYTES, so a per-iteration
        // full clone was O(n) per loop (quadratic overall). The only in-loop allocation
        // now is the (smaller) remainder after a reclaimed block. Every exit path writes
        // the intended remainder back into `self.invoke_sniff_buffer` (empty if fully
        // consumed); the Some->Some path keeps looping on the local `buf`.
        let mut buf = std::mem::take(&mut self.invoke_sniff_buffer);
        loop {
            match find_invoke_start(&buf) {
                Some(start) => {
                    match find_invoke_block_end(&buf, start) {
                        Some(end) => {
                            // 命中完整块：先判定它像真泄漏还是正文讨论（P1 歧义信号）
                            let before = strip_trailing_stray_tokens(&buf[..start]);
                            // 🅱 先把 before 里的围栏开合并进一个「试算」状态：如果这个 <invoke>
                            // 落在代码围栏内（正文展示的代码块），一律不捞回，当文本吐出。
                            let fence_after_before = fence_open_after(
                                self.code_fence_open,
                                &self.fence_scan_partial,
                                before,
                            );
                            // 🅳 灾难兜底：只有解析出的工具名在本次请求声明的工具表里，才允许捞回。
                            // 表为空（请求没带 tools）或名字不在表里 → 当文本吐，宁可漏捞不可误执行。
                            let parsed = parse_invoke_block(&buf[start..end]);
                            let name_known = parsed
                                .as_ref()
                                .map(|(n, _)| self.known_tool_names.contains(n))
                                .unwrap_or(false);
                            if invoke_looks_like_real_leak(before)
                                && !fence_after_before
                                && name_known
                            {
                                // 真泄漏：吐块前文本（剥掉尾部独立的 call/count 行）+ 合成 tool_use
                                if !before.is_empty() {
                                    events.extend(self.emit_text_delta_raw(before));
                                }
                                // parsed 在上面已确认是 Some 且 name_known
                                let (name, input_json) =
                                    parsed.expect("parsed is Some when name_known");
                                // 解析完整入参 → 统一还原 → 统一发出（与结构化 toolUseEvent 同一发出口）。
                                let input: serde_json::Value =
                                    serde_json::from_str(&input_json).unwrap_or_else(|_| json!({}));
                                let tool_use_id = format!(
                                    "toolu_{}",
                                    Uuid::new_v4().to_string().replace('-', "")
                                );
                                let completed = CompletedToolUse::from_kiro(
                                    tool_use_id,
                                    &name,
                                    input,
                                    &self.tool_name_map,
                                );
                                events.extend(self.emit_completed_tool_use(completed));
                            } else {
                                // 不捞回（嵌句中 / 围栏内 / 工具名未知 / 解析失败）→ 整段当普通文本吐出
                                events.extend(self.emit_text_delta_raw(&buf[..end]));
                            }
                            // 推进本地缓冲区到块之后，继续循环（不再回写 self、不再整体 clone）
                            buf = buf[end..].to_string();
                            continue;
                        }
                        None => {
                            // 块还没到齐。先用 P1 行首判定：不在行首的 <invoke 当讨论文本，
                            // 直接整段吐出，不进 hold 缓冲（P2：避免 hold 住后续文本到流末尾）。
                            let before = strip_trailing_stray_tokens(&buf[..start]);
                            // 🅱 围栏内的未闭合 <invoke> 也不 hold（是正文代码块），直接当文本吐。
                            let fence_after_before = fence_open_after(
                                self.code_fence_open,
                                &self.fence_scan_partial,
                                before,
                            );
                            if !invoke_looks_like_real_leak(before) || fence_after_before {
                                if !buf.is_empty() {
                                    events.extend(self.emit_text_delta_raw(&buf));
                                }
                                break;
                            }
                            // 行首的未闭合块：把 start 之前的文本吐出，保留 start.. 等闭合
                            if start > 0 {
                                events.extend(self.emit_text_delta_raw(&buf[..start]));
                            }
                            let remainder = buf[start..].to_string();
                            if flush {
                                // flush 模式：残留半块当普通文本吐出
                                if !remainder.is_empty() {
                                    events.extend(self.emit_text_delta_raw(&remainder));
                                }
                            } else {
                                // P2 上限：hold 的 <invoke 块累计超过阈值仍没等到 </invoke>，
                                // 放弃等待，当普通文本吐出，避免无限期 hold 后续文本。
                                // 仅用纯字节上限兜底"永不闭合的 `<invoke` 把流卡死"；
                                // 不再按换行数放弃——多行参数（apply_patch 等）是常态，
                                // 换行数不是放弃 hold 的好信号，否则会误杀分片到达的合法 invoke。
                                let too_long = remainder.len() > Self::MAX_INVOKE_HOLD_BYTES;
                                if too_long {
                                    events.extend(self.emit_text_delta_raw(&remainder));
                                } else {
                                    // 保留半块到 self，等下一片到达再续
                                    self.invoke_sniff_buffer = remainder;
                                }
                            }
                            break;
                        }
                    }
                }
                None => {
                    // 没有任何 invoke 开标签
                    if flush {
                        if !buf.is_empty() {
                            events.extend(self.emit_text_delta_raw(&buf));
                        }
                    } else {
                        // 保留一段可能是部分 `<invoke` 开标签前缀的尾巴，其余吐出
                        let keep = partial_invoke_tag_suffix_len(&buf);
                        let split = buf.len() - keep;
                        let safe = find_char_boundary(&buf, split);
                        if safe > 0 {
                            events.extend(self.emit_text_delta_raw(&buf[..safe]));
                        }
                        self.invoke_sniff_buffer = buf[safe..].to_string();
                    }
                    break;
                }
            }
        }
        events
    }

    /// 创建 text_delta 事件（原始逻辑，无嗅探）
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    /// 复读熔断过滤器：在文本或 Thinking 真正吐给客户端之前，逐行检测
    /// 「同一非空短单元连续复读」。
    ///
    /// 工作方式（流式安全，跨 chunk 累计）：
    /// - 把输入按行切，只去掉行尾空白，保留行首缩进；
    /// - 空行忽略且不重置，覆盖 `}\n\n}\n\n` 形态；
    /// - 不超过 `REPEAT_GUARD_MAX_UNIT_BYTES` 的相同候选在同一通道连续达到阈值即跳闸；
    /// - 跳闸后本轮后续 text/thinking 一律丢弃，并以独立错误终态收尾。
    ///
    /// 返回应当继续吐出的文本（跳闸时返回空串）。
    fn repeat_guard_filter(&mut self, text: &str, channel: &'static str) -> String {
        // 正文已跳闸：本轮剩余内容全部丢弃，断雪球。
        if self.repeat_guard_tripped {
            return String::new();
        }
        // thinking 已跳闸：只静音 thinking 通道，正文与工具调用继续走完。
        if channel == "thinking" && self.thinking_repeat_tripped {
            return String::new();
        }

        let mut kept = String::new();
        // 用 split_inclusive 保留换行符，确保放行的正常文本不丢字节。
        for segment in text.split_inclusive('\n') {
            // 跨 chunk 重组成完整行再比较：上游分片不按行边界切，直接比较碎片会让连续
            // 计数永不累积（正文通道的熔断此前因此从未生效）。
            let complete = segment.ends_with('\n');
            // 只剥换行符，**不能**剥空格/制表符：分片可能正好切在行内空格处
            //（`"line 0 "` + `"distinct..."`），提前剥掉会把行内空格吃掉，重组出
            // `"line 0distinct..."` 这种与原文不符的比较对象。行尾空白统一在行完整后
            // 由下面的 trim_end 处理。
            let body = segment.strip_suffix('\n').unwrap_or(segment);
            let body = body.strip_suffix('\r').unwrap_or(body);
            if !self.repeat_guard_partial_oversized {
                if self.repeat_guard_partial.len().saturating_add(body.len())
                    <= REPEAT_GUARD_MAX_UNIT_BYTES
                {
                    self.repeat_guard_partial.push_str(body);
                } else {
                    // 超长行不参与比较：既避免缓冲无界增长，也不会把长正文误判成复读。
                    self.repeat_guard_partial.clear();
                    self.repeat_guard_partial_oversized = true;
                    self.repeat_guard_last_line.clear();
                    self.repeat_guard_run = 0;
                }
            }
            if !complete {
                // 行还没结束，本片原样放行，等下一个 chunk 续上。
                //
                // 周期检测器必须在这里也喂一次：它有自己的 partial_line 重组，需要收到
                // **每一个**分片。早退跳过 observe_segment 会把它饿死——只收到零星的完整
                // 片段，拼成乱码行，乱码行凑巧形成周期而误判（实测 period=6 cycles=4）。
                if let Some(cycle) = self.repeat_guard_periodic.observe_segment(segment, channel) {
                    if channel == "thinking" {
                        self.thinking_repeat_tripped = true;
                    } else {
                        self.repeat_guard_tripped = true;
                    }
                    tracing::warn!(
                        message_id = %self.message_id,
                        channel = %channel,
                        period_lines = cycle.period,
                        cycle_count = cycle.cycles,
                        thinking_only = channel == "thinking",
                        "upstream periodic repetition guard tripped"
                    );
                    return kept;
                }
                kept.push_str(segment);
                continue;
            }
            let line_owned = if self.repeat_guard_partial_oversized {
                self.repeat_guard_partial.clear();
                self.repeat_guard_partial_oversized = false;
                String::new()
            } else {
                std::mem::take(&mut self.repeat_guard_partial)
            };
            let line: &str = line_owned.trim_end();
            if line.is_empty() {
                kept.push_str(segment);
                continue;
            }

            if line.len() <= REPEAT_GUARD_MAX_UNIT_BYTES {
                if channel == self.repeat_guard_last_channel && line == self.repeat_guard_last_line
                {
                    self.repeat_guard_run += 1;
                } else {
                    self.repeat_guard_last_channel = channel;
                    self.repeat_guard_last_line = line.to_string();
                    self.repeat_guard_run = 1;
                }
                let trip_threshold = if channel == "thinking" {
                    REPEAT_GUARD_THINKING_TRIP_THRESHOLD
                } else {
                    REPEAT_GUARD_TRIP_THRESHOLD
                };
                if self.repeat_guard_run >= trip_threshold {
                    if channel == "thinking" {
                        self.thinking_repeat_tripped = true;
                    } else {
                        self.repeat_guard_tripped = true;
                    }
                    tracing::warn!(
                        message_id = %self.message_id,
                        channel = %channel,
                        repeat_count = self.repeat_guard_run,
                        unit_bytes = line.len(),
                        thinking_only = channel == "thinking",
                        "upstream repetition guard tripped"
                    );
                    return kept;
                }
            } else {
                self.repeat_guard_last_channel = channel;
                self.repeat_guard_last_line.clear();
                self.repeat_guard_run = 0;
            }

            if let Some(cycle) = self.repeat_guard_periodic.observe_segment(segment, channel) {
                if channel == "thinking" {
                    self.thinking_repeat_tripped = true;
                } else {
                    self.repeat_guard_tripped = true;
                }
                tracing::warn!(
                    message_id = %self.message_id,
                    channel = %channel,
                    period_lines = cycle.period,
                    cycle_count = cycle.cycles,
                    thinking_only = channel == "thinking",
                    "upstream periodic repetition guard tripped"
                );
                return kept;
            }
            kept.push_str(segment);
        }
        kept
    }

    fn emit_text_delta_raw(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 🛑 复读熔断（root cause: Opus 长上下文退化，把同一 stray token 一行行无限复读）。
        // 在文本出口处过滤：一旦同一短行连续重复超过阈值，丢弃后续复读文本，
        // 既不让它喷给客户端、不烧满 max_tokens，也不写进对话历史（断雪球）。
        let kept = self.repeat_guard_filter(text, "text");
        if kept.is_empty() {
            return events;
        }
        let text: &str = &kept;
        self.has_visible_output = true;
        self.accumulated_text.push_str(text);

        // 🅱 维护跨流的代码围栏奇偶状态：所有真正作为「文本」吐出的内容都过这里，
        // 在此累进围栏状态，使后续 <invoke> 能判断自己是否落在代码块内。
        let mut fence_open = self.code_fence_open;
        let mut fence_partial = std::mem::take(&mut self.fence_scan_partial);
        advance_code_fence_state(&mut fence_open, &mut fence_partial, text);
        self.code_fence_open = fence_open;
        self.fence_scan_partial = fence_partial;

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    fn is_thinking_block_open(&self) -> bool {
        self.thinking_block_index
            .is_some_and(|idx| self.state_manager.is_block_open_of_type(idx, "thinking"))
    }

    fn close_open_text_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.text_block_index else {
            return Vec::new();
        };
        if !self.state_manager.is_block_open_of_type(idx, "text") {
            self.text_block_index = None;
            return Vec::new();
        }
        self.text_block_index = None;
        self.state_manager
            .handle_content_block_stop(idx)
            .into_iter()
            .collect()
    }

    fn ensure_thinking_block(&mut self) -> Vec<SseEvent> {
        if self.is_thinking_block_open() {
            return Vec::new();
        }

        let mut events = Vec::new();
        self.saw_reasoning_output = true;
        let buffered = std::mem::take(&mut self.thinking_buffer);
        if !buffered.trim().is_empty() {
            events.extend(self.create_text_delta_events(&buffered));
        }
        events.extend(self.close_open_text_block());

        let idx = self.state_manager.next_block_index();
        self.thinking_block_index = Some(idx);
        self.thinking_extracted = true;
        events.extend(self.state_manager.handle_content_block_start(
            idx,
            "thinking",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "thinking",
                    "thinking": ""
                }
            }),
        ));
        events
    }

    fn close_open_thinking_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.thinking_block_index else {
            return Vec::new();
        };
        if !self.state_manager.is_block_open_of_type(idx, "thinking") {
            return Vec::new();
        }

        let signature = self.pending_thinking_signature.take().unwrap_or_else(|| {
            super::thinking_signature::issue_signature(&self.message_id, &self.thinking_buffer)
        });
        let mut events = vec![
            self.create_thinking_delta_event(idx, ""),
            self.create_signature_delta_event_with(idx, &signature),
        ];
        if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
            events.push(stop_event);
        }
        events
    }

    fn process_reasoning_content(
        &mut self,
        reasoning: &crate::kiro::model::events::ReasoningContentEvent,
    ) -> Vec<SseEvent> {
        if !self.thinking_enabled {
            if let Some(text) = reasoning.text.as_deref()
                && !text.is_empty()
            {
                if self.tool_choice_policy.is_required()
                    && !self.saw_upstream_tool_use
                    && !self.required_tool_preamble_released
                {
                    self.required_tool_preamble.push_str(text);
                    return Vec::new();
                }
                self.output_tokens += estimate_tokens(text);
                return self.create_text_delta_events(text);
            }
            return Vec::new();
        }

        // 通道互锁：正文标签已占据 thinking 通道时，原生事件一律丢弃，避免同一段推理
        // 从两条路各输出一遍。
        if !self.thinking_source.allow_reasoning() {
            tracing::debug!(
                message_id = %self.message_id,
                "dropped native reasoning event: thinking channel already owned by tag blocks"
            );
            return Vec::new();
        }

        let mut events = Vec::new();

        if let Some(signature) = reasoning.signature.as_deref()
            && !signature.is_empty()
        {
            self.pending_thinking_signature = Some(signature.to_string());
        }

        if let Some(text) = reasoning.text.as_deref()
            && !text.is_empty()
        {
            self.output_tokens += estimate_tokens(text);
            events.extend(self.ensure_thinking_block());
            if let Some(idx) = self.thinking_block_index {
                if let Some(event) = self.create_guarded_thinking_delta_event(idx, text) {
                    events.push(event);
                }
            }
        }

        if let Some(redacted) = reasoning.redacted_content.as_deref()
            && !redacted.is_empty()
        {
            self.saw_reasoning_output = true;
            self.output_tokens += 8;
            events.extend(self.create_redacted_thinking_events(redacted));
        }

        events
    }

    fn create_redacted_thinking_events(&mut self, data: &str) -> Vec<SseEvent> {
        self.has_visible_output = true;
        let mut events = self.close_open_thinking_block();
        events.extend(self.close_open_text_block());

        let idx = self.state_manager.next_block_index();
        events.extend(self.state_manager.handle_content_block_start(
            idx,
            "redacted_thinking",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "redacted_thinking",
                    "data": data
                }
            }),
        ));
        if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
            events.push(stop_event);
        }
        events
    }

    /// 创建受通用复读保护的 thinking_delta。空 delta 是协议收尾信号，不参与检测。
    fn create_guarded_thinking_delta_event(
        &mut self,
        index: i32,
        thinking: &str,
    ) -> Option<SseEvent> {
        if thinking.is_empty() {
            return Some(self.create_thinking_delta_event(index, ""));
        }
        let kept = self.repeat_guard_filter(thinking, "thinking");
        if kept.is_empty() {
            return None;
        }
        self.has_visible_output = true;
        Some(self.create_thinking_delta_event(index, &kept))
    }

    /// 创建原始 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件
    ///
    /// Anthropic 协议下 thinking 块流式结束前必须发一个 signature_delta，
    /// SDK 会把它聚合到 thinking 块的 `signature` 字段。客户端在下一轮把
    /// assistant 消息回传时本地校验 thinking 块必须带非空 signature，否则抛出
    /// `The content[].thinking in the thinking mode must be passed back to the API`。
    ///
    /// 上游 Kiro 不是 Anthropic 服务端，不会下发真实签名，因此这里发一个非空
    /// 占位字符串以满足客户端本地校验。该字段不参与转发回 Kiro 的逻辑
    /// （converter 只读 `block.thinking`，不读 signature）。
    fn create_signature_delta_event(&self, index: i32) -> SseEvent {
        let signature =
            super::thinking_signature::issue_signature(&self.message_id, &self.thinking_buffer);
        self.create_signature_delta_event_with(index, &signature)
    }

    fn create_signature_delta_event_with(&self, index: i32, signature: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature,
                }
            }),
        )
    }

    /// 统一的工具调用流式发出口：结构化 `toolUseEvent` 与 `<invoke>` 文本捞回都经此发出。
    ///
    /// 块索引按 `completed.id` 复用/分配（结构化按 tool_use_id 复用；invoke 合成用新 id 故新分配），
    /// 依次发 `content_block_start{name, input:{}}` → 单个完整 `input_json_delta` → `content_block_stop`。
    fn emit_completed_tool_use(&mut self, mut completed: CompletedToolUse) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.tool_choice_policy.disables_parallel_tool_use() {
            if !self.tool_choice_policy.accepts_tool_fragment(
                &mut self.single_tool_slot_id,
                self.single_tool_slot_closed,
                &completed.id,
            ) {
                tracing::warn!(
                    tool_id = %completed.id,
                    tool_name = %completed.name,
                    "suppressed additional upstream tool call because the client disabled parallel tool use"
                );
                return events;
            }
            self.single_tool_slot_closed = true;
        }
        if self.tool_contracts_initialized {
            // 未声明工具：先试解析成客户端已声明的等价名（大小写 / 语义同义词）。
            if !self.tool_contracts.contains_key(&completed.name)
                && let Some(resolved) = super::tool_schema::resolve_undeclared_tool_name(
                    &self.tool_contracts,
                    &completed.name,
                )
            {
                tracing::warn!(
                    upstream_tool = %completed.name,
                    client_tool = %resolved,
                    "上游未声明工具已解析为客户端等价工具"
                );
                completed.name = resolved;
            }
            // 解析不到等价工具 → **降级成文本**而非整轮失败（此前是零重试 502）。
            // 客户端至少能看到模型意图并继续对话；靠系统提示词描述工具的客户端
            // （Cline / Codex 系）自己会从文本里解析。
            // 先在此处判定并 return，之后再取 contract 引用，避免为规避借用冲突而克隆契约。
            if !self.tool_contracts.contains_key(&completed.name) {
                tracing::warn!(
                    tool = %completed.name,
                    "上游返回了未声明工具，降级为文本交付"
                );
                let rendered = format!(
                    "\n[未声明工具调用 {}]\n{}\n",
                    completed.name,
                    serde_json::to_string_pretty(&completed.input)
                        .unwrap_or_else(|_| completed.input.to_string())
                );
                self.degraded_undeclared_tool = true;
                events.extend(self.emit_text_delta_raw(&rendered));
                return events;
            }
            // 借用只活到校验结束：把结果与 client_name 取出后即释放，之后才写 self。
            let (outcome, client_name) = {
                let contract = self
                    .tool_contracts
                    .get(&completed.name)
                    .expect("contract presence checked above");
                let client_name = contract.client_name.clone();
                let outcome =
                    super::tool_schema::validate_and_repair(&contract.schema, &mut completed.input);
                (outcome, client_name)
            };
            match outcome {
                super::tool_schema::ToolInputOutcome::Valid => {}
                super::tool_schema::ToolInputOutcome::Repaired { paths } => {
                    tracing::warn!(
                        tool = %client_name,
                        paths = ?paths,
                        "确定性修复上游工具固定字段"
                    );
                }
                super::tool_schema::ToolInputOutcome::Invalid { violations } => {
                    let error = super::tool_schema::ToolSchemaError {
                        tool_name: client_name.clone(),
                        violations,
                    };
                    tracing::warn!(tool = %client_name, "上游工具参数不满足客户端Schema");
                    self.terminal_attempt_failure =
                        Some(super::tool_attempt::AttemptFailure::InvalidToolSchema {
                            failure: super::tool_schema::ToolSchemaFailure::from_error_and_input(
                                error,
                                &completed.input,
                            ),
                        });
                    self.state_manager.set_stop_reason("error");
                    return events;
                }
            }
        }
        self.state_manager.set_has_tool_use(true);
        self.has_visible_output = true;
        self.emitted_tool_names.push(completed.name.clone());
        let input_json =
            serde_json::to_string(&completed.input).unwrap_or_else(|_| "{}".to_string());

        let block_index = if let Some(&idx) = self.tool_block_indices.get(&completed.id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices.insert(completed.id.clone(), idx);
            idx
        };
        tracing::debug!(
            tool_id = %completed.id,
            tool_name = %completed.name,
            block_index,
            input_bytes = input_json.len(),
            "emitting completed Anthropic tool_use block"
        );

        events.extend(self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": completed.id,
                    "name": completed.name,
                    "input": {}
                }
            }),
        ));

        // 一次性发出完整参数 JSON（来源已保证是合法 JSON）。
        self.output_tokens += estimate_tokens(&input_json);
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            block_index,
            json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": input_json
                }
            }),
        ) {
            events.push(delta_event);
        }

        if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
            events.push(stop_event);
        }

        events
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        if self.tool_choice_policy.is_required() && !self.required_tool_preamble_released {
            let discarded_bytes = self.required_tool_preamble.len();
            self.required_tool_preamble.clear();
            self.required_tool_preamble_released = true;
            tracing::debug!(
                discarded_bytes,
                "discarded required-tool narration before native tool_use"
            );
        }
        self.saw_upstream_tool_use = true;
        tracing::debug!(
            tool_id = %tool_use.tool_use_id,
            tool_name = %tool_use.name,
            stop = tool_use.stop,
            input_bytes = tool_use.input.len(),
            "received upstream tool_use fragment"
        );
        if !self.tool_choice_policy.accepts_tool_fragment(
            &mut self.single_tool_slot_id,
            self.single_tool_slot_closed,
            &tool_use.tool_use_id,
        ) {
            tracing::warn!(
                tool_id = %tool_use.tool_use_id,
                tool_name = %tool_use.name,
                "suppressed additional upstream tool fragment because the client disabled parallel tool use"
            );
            return events;
        }

        if self.is_thinking_block_open() && !self.in_thinking_block {
            events.extend(self.close_open_thinking_block());
        }

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        if let Some(event) = self
                            .create_guarded_thinking_delta_event(thinking_index, &thinking_content)
                        {
                            events.push(event);
                        }
                    }
                }

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // 先发送空的 thinking_delta
                    events.push(self.create_thinking_delta_event(thinking_index, ""));
                    // signature_delta：满足客户端 thinking 模式下的本地校验
                    events.push(self.create_signature_delta_event(thinking_index));
                    // 再发送 content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let after_pos = end_pos + end_tag_skip_len(&self.thinking_buffer, end_pos);
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block 时，将缓冲区当作普通文本 flush。
        //
        // 原先还要求 `!thinking_extracted`。支持一轮多个 thinking 块后，「为嗅探
        // `<thinking>` 而暂存尾部文本」这件事在首块之后同样会发生，旧条件会让这段
        // 待输出文本在 tool_use 到来时被静默吞掉。
        if self.thinking_enabled && !self.in_thinking_block && !self.thinking_buffer.is_empty() {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // 通过累积器缓冲工具参数 JSON 分片：只有收到 stop=true 且解析成功时才
        // 发出完整的工具调用；半截 / 非法 JSON 记为错误，交由收尾（generate_final_events）
        // 统一补发 error 事件，避免把无法解析的参数当成完整调用转发给客户端。
        let completed = match self
            .tool_json_accumulator
            .push(tool_use, &self.tool_name_map)
        {
            Ok(Some(completed)) => completed,
            Ok(None) => return events,
            Err(e) => {
                tracing::error!("{}", e);
                if self.tool_choice_policy.disables_parallel_tool_use() {
                    self.single_tool_slot_closed = true;
                }
                self.tool_json_error = Some(e);
                self.state_manager.set_stop_reason("error");
                return events;
            }
        };

        // 统一发出（与 <invoke> 文本捞回路径共用同一发出口）。
        events.extend(self.emit_completed_tool_use(completed));
        events
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 收尾：flush <tool_use> XML 过滤器的残留（截断的未闭合块会被丢弃），
        // 走同一文本路径，交由后续 thinking / invoke 缓冲一并 flush。
        let leftover = self.tool_use_xml_filter.finish();
        if !leftover.is_empty() {
            // XML 残留也要过身份过滤器（保持与正常 chunk 同口径）。
            let leftover = self
                .identity_filter
                .as_mut()
                .map(|f| f.push(&leftover))
                .unwrap_or(leftover);
            if !leftover.is_empty() {
                if self.thinking_enabled {
                    events.extend(self.process_content_with_thinking(&leftover));
                } else {
                    events.extend(self.create_text_delta_events(&leftover));
                }
            }
        }
        // flush 身份过滤器缓冲的末尾残留（疑似 kiro 前缀但流已结束，原样输出）。
        if let Some(tail) = self.identity_filter.as_mut().map(|f| f.finish()) {
            if !tail.is_empty() {
                if self.thinking_enabled {
                    events.extend(self.process_content_with_thinking(&tail));
                } else {
                    events.extend(self.create_text_delta_events(&tail));
                }
            }
        }

        if self.tool_choice_policy.is_required()
            && !self.saw_upstream_tool_use
            && !self.required_tool_preamble_released
        {
            self.required_tool_preamble_released = true;
            let buffered = std::mem::take(&mut self.required_tool_preamble);
            if !buffered.is_empty() {
                events.extend(self.create_text_delta_events(&buffered));
            }
        }

        if self.is_thinking_block_open() && !self.in_thinking_block {
            events.extend(self.close_open_thinking_block());
        }

        // Flush thinking_buffer 中的剩余内容
        if self.thinking_enabled && !self.thinking_buffer.is_empty() {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            if let Some(event) = self.create_guarded_thinking_delta_event(
                                thinking_index,
                                &thinking_content,
                            ) {
                                events.push(event);
                            }
                        }
                    }

                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let after_pos = end_pos + end_tag_skip_len(&self.thinking_buffer, end_pos);
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    if let Some(thinking_index) = self.thinking_block_index {
                        let thinking_content = self.thinking_buffer.clone();
                        if let Some(event) = self
                            .create_guarded_thinking_delta_event(thinking_index, &thinking_content)
                        {
                            events.push(event);
                        }
                    }
                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = self.thinking_buffer.clone();
                events.extend(self.create_text_delta_events(&buffer_content));
            }
            self.thinking_buffer.clear();
        }

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块。
        //
        // 护栏：`saw_upstream_tool_use` 覆盖"仅缓冲、尚未发出块"的 tool_use（如无参工具在
        // stop=true 前断流）——此时 active_blocks 里只有 thinking 块，has_non_thinking_blocks()
        // 仍为 false，若不看 has_tool_use 会误判为纯思考轮，把随后打捞的 tool_use 的
        // stop_reason 盖成 max_tokens 并多吐一个空格。
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
            && !self.saw_upstream_tool_use
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        // Flush invoke 嗅探缓冲区的残留：先再嗅探一次完整块（万一最后一块就是完整 invoke），
        // 剩下的走 emit_text_delta_raw flush 出去（防尾字节丢）。
        if !self.invoke_sniff_buffer.is_empty() {
            events.extend(self.drain_invoke_sniff_buffer(true));
        }

        // 收尾检查工具调用累积器：对残留缓冲区分处理——
        //   · 空入参（无参工具，如 EnterPlanMode）→ 打捞成完整 tool_use 发出；
        //   · 半截 JSON（上游写参数途中截断）→ 记为错误，以 error 事件终止。
        // process_tool_use 中已置位的错误保持不变。
        if self.tool_json_error.is_none() {
            let map = self.tool_name_map.clone();
            let contracts = self.tool_contracts.clone();
            let (salvaged, incomplete) = self.tool_json_accumulator.finish(&map, &contracts);
            if !salvaged.is_empty() {
                // 兜底：若 thinking 块此刻仍开着（罕见：in_thinking_block 为 true 且缓冲为空、
                // 上面各分支都没关它），先把它关掉，避免 tool_use 块发出后才补 thinking 的
                // content_block_stop 导致块顺序错乱。close_open_thinking_block 幂等，已关则 no-op。
                events.extend(self.close_open_thinking_block());
            }
            for completed in salvaged {
                tracing::warn!(
                    "上游工具 {} ({}) 未发 stop=true 即断流；残留入参已严格解析为完整 JSON，按隐式 stop 打捞发出",
                    completed.name,
                    completed.id
                );
                events.extend(self.emit_completed_tool_use(completed));
            }
            if let Some(e) = incomplete {
                tracing::error!("{}", e);
                self.tool_json_error = Some(e);
                self.state_manager.set_stop_reason("error");
            }
        }

        if events_have_visible_output(&events) {
            self.has_visible_output = true;
        }
        let mut classified_failure = self.attempt_observation.failure(
            self.tool_json_error.clone(),
            self.state_manager.has_tool_use(),
        );
        if self.has_visible_output
            && matches!(
                classified_failure,
                Some(super::tool_attempt::AttemptFailure::EmptyResponse)
            )
        {
            classified_failure = None;
        }
        if self.terminal_attempt_failure.is_none() {
            self.terminal_attempt_failure = classified_failure
                .as_ref()
                .filter(|failure| {
                    !matches!(failure, super::tool_attempt::AttemptFailure::EmptyResponse)
                })
                .cloned();
        }
        // 未声明工具被**主动降级成文本**时不算协议异常：确实没发 tool_use 块，但那是既定策略，
        // 内容已作为文本交付。不排除的话降级路径会被这条守卫重新打成 upstream_protocol_error。
        if self.saw_upstream_tool_use
            && !self.state_manager.has_tool_use()
            && !self.degraded_undeclared_tool
            && self.tool_json_error.is_none()
            && self.terminal_attempt_failure.is_none()
        {
            self.terminal_protocol_error = Some(
                "upstream ended with tool_use but produced no valid tool_use content block"
                    .to_string(),
            );
        }
        if self.terminal_protocol_error.is_none()
            && self.tool_json_error.is_none()
            && self.terminal_attempt_failure.is_none()
        {
            let tool_choice_error = match &self.tool_choice_policy {
                super::converter::ToolChoicePolicy::Auto {
                    disable_parallel_tool_use: true,
                }
                | super::converter::ToolChoicePolicy::RequiredAny {
                    disable_parallel_tool_use: true,
                }
                | super::converter::ToolChoicePolicy::RequiredSpecific {
                    disable_parallel_tool_use: true,
                    ..
                } if self.emitted_tool_names.len() > 1 => Some(
                    "client disabled parallel tool use but upstream produced multiple tool calls"
                        .to_string(),
                ),
                super::converter::ToolChoicePolicy::RequiredAny { .. }
                    if self.emitted_tool_names.is_empty() =>
                {
                    Some("client required a tool call but upstream produced none".to_string())
                }
                super::converter::ToolChoicePolicy::RequiredSpecific { name, .. }
                    if !self.emitted_tool_names.iter().any(|actual| actual == name) =>
                {
                    Some(format!(
                        "client required tool {name} but upstream did not produce it"
                    ))
                }
                super::converter::ToolChoicePolicy::Disabled
                    if !self.emitted_tool_names.is_empty() =>
                {
                    Some("client disabled tool calls but upstream produced one".to_string())
                }
                _ => None,
            };
            if let Some(message) = tool_choice_error {
                self.terminal_protocol_error = Some(message);
                self.terminal_protocol_error_type = Some("upstream_tool_choice_error");
            }
        }
        if self.thinking_enabled
            && !self.strict_thinking_validation
            && !self.saw_reasoning_output
            && self.has_visible_output
            && self.tool_json_error.is_none()
            && self.terminal_protocol_error.is_none()
            && self.terminal_attempt_failure.is_none()
        {
            // 这一条曾占全部 WARN 的 70%，把 docker 日志可回溯窗口压到 4 小时。
            // 请求本身正常完成，属正常降级，改为按模型聚合上报（每分钟一行 INFO）。
            super::thinking_degradation::record(&self.model);
        }
        if self.thinking_enabled
            && self.strict_thinking_validation
            && !self.saw_reasoning_output
            && self.tool_json_error.is_none()
            && self.terminal_protocol_error.is_none()
            && self.terminal_attempt_failure.is_none()
        {
            self.terminal_protocol_error = Some(
                "client requested thinking but upstream produced no thinking content".to_string(),
            );
            self.terminal_protocol_error_type = Some("upstream_thinking_protocol_error");
        }
        if self.terminal_attempt_failure.is_none()
            && self.terminal_protocol_error.is_none()
            && matches!(
                classified_failure,
                Some(super::tool_attempt::AttemptFailure::EmptyResponse)
            )
        {
            self.terminal_attempt_failure =
                Some(super::tool_attempt::AttemptFailure::EmptyResponse);
        }

        let terminal_error = self.terminal_error_message();
        tracing::debug!(
            emitted_tool_names = ?self.emitted_tool_names,
            stop_reason = %self.state_manager.get_stop_reason(),
            terminal_error = terminal_error.as_deref().unwrap_or(""),
            "finalized Anthropic tool event state"
        );
        if let Some(message) = terminal_error {
            let error_type = self
                .terminal_attempt_failure
                .as_ref()
                .map(|failure| failure.public_error().0)
                .or_else(|| {
                    self.tool_json_error
                        .as_ref()
                        .map(ToolJsonAccumulatorError::error_type)
                })
                .or(self.terminal_protocol_error_type)
                .unwrap_or("upstream_protocol_error");
            events.push(SseEvent::new(
                "error",
                json!({
                    "type": "error",
                    "error": {
                        "type": error_type,
                        "message": message
                    }
                }),
            ));
            return events;
        }

        if self.repeat_guard_tripped {
            let (final_input_tokens, cache_creation, cache_read) = self.resolved_usage();
            let mut terminal_events = self.state_manager.generate_final_events(
                final_input_tokens,
                self.output_tokens,
                cache_creation,
                cache_read,
            );
            // 只留 content_block_stop：`error` 事件本身就是 SSE 流的终止事件，
            // 后面不再跟 message_delta / message_stop。本仓库所有终态错误路径统一如此。
            terminal_events.retain(|event| event.event == "content_block_stop");
            events.extend(terminal_events);
            events.push(SseEvent::new(
                "error",
                json!({
                    "type": "error",
                    "error": {
                        "type": "upstream_repetition_guard",
                        "message": "Repeated upstream output was truncated"
                    }
                }),
            ));
            return events;
        }

        // 客户端可见 total − 缓存覆盖 = 未缓存 input。
        let (final_input_tokens, cache_creation, cache_read) = self.resolved_usage();

        // 生成最终事件（message_delta + message_stop）
        events.extend(self.state_manager.generate_final_events(
            final_input_tokens,
            self.output_tokens,
            cache_creation,
            cache_read,
        ));

        events
    }

    /// 按实际读取终止方式收尾；中断绝不生成成功 terminal，也不回显传输错误正文。
    pub(crate) fn generate_final_events_for(
        &mut self,
        termination: &super::tool_attempt::AttemptTermination,
    ) -> Vec<SseEvent> {
        if matches!(
            termination,
            super::tool_attempt::AttemptTermination::ClientClosed
        ) {
            return Vec::new();
        }
        let mut events = self.generate_final_events();
        if matches!(termination, super::tool_attempt::AttemptTermination::Eof) {
            return events;
        }

        events.retain(|event| !matches!(event.event.as_str(), "message_delta" | "message_stop"));
        let has_specific_failure = self
            .terminal_attempt_failure
            .as_ref()
            .is_some_and(|failure| {
                !matches!(failure, super::tool_attempt::AttemptFailure::EmptyResponse)
            });
        if !has_specific_failure {
            events.retain(|event| event.event != "error");
        }
        if !events.iter().any(|event| event.event == "error") {
            let (error_type, message) = match termination {
                super::tool_attempt::AttemptTermination::ReadError(_) => (
                    "upstream_stream_interrupted",
                    "Upstream response stream was interrupted",
                ),
                super::tool_attempt::AttemptTermination::IdleTimeout => (
                    "upstream_stream_idle_timeout",
                    "Upstream response stream timed out",
                ),
                super::tool_attempt::AttemptTermination::Eof
                | super::tool_attempt::AttemptTermination::ClientClosed => unreachable!(),
            };
            events.push(SseEvent::new(
                "error",
                json!({
                    "type": "error",
                    "error": {"type": error_type, "message": message}
                }),
            ));
        }
        events
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用客户端可见 `input_tokens` 与缓存拆分更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    #[cfg(test)]
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
    ) -> Self {
        Self::new_with_constraints(
            model,
            estimated_input_tokens,
            thinking_enabled,
            false,
            tool_name_map,
            known_tool_names,
            super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: false,
            },
        )
    }

    pub fn new_with_constraints(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        strict_thinking_validation: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
        tool_choice_policy: super::converter::ToolChoicePolicy,
    ) -> Self {
        let inner = StreamContext::new_with_constraints(
            model,
            estimated_input_tokens,
            thinking_enabled,
            strict_thinking_validation,
            tool_name_map,
            known_tool_names,
            tool_choice_policy,
        );
        Self {
            inner,
            event_buffer: Vec::new(),
            initial_events_generated: false,
        }
    }

    /// 注入由 CacheMeter 计算的缓存覆盖情况（estimate 口径），最终上报时分摊。
    pub fn set_cache_usage(&mut self, cache_usage: super::cache_metering::CacheUsage) {
        self.inner.cache_usage = cache_usage;
    }

    pub fn set_context_window_size(&mut self, value: i32) {
        self.inner.set_context_window_size(value);
    }

    pub fn set_context_window_signal_threshold_pct(&mut self, value: f64) {
        self.inner.set_context_window_signal_threshold_pct(value);
    }

    pub(crate) fn break_block_label(&self) -> &str {
        self.inner.break_block_label()
    }

    pub(crate) fn set_tool_contracts(
        &mut self,
        contracts: HashMap<String, super::tool_schema::ToolContract>,
    ) {
        self.inner.set_tool_contracts(contracts);
    }

    /// 开启流式身份归一化（委托给 inner StreamContext）。
    pub fn enable_identity_filter(&mut self) {
        self.inner.enable_identity_filter();
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 成功时生成 message_delta/message_stop；协议错误时生成 error
    /// 2. 用客户端可见 input_tokens 更正 message_start 事件
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        self.finish_and_get_all_events_for(&super::tool_attempt::AttemptTermination::Eof)
    }

    pub(crate) fn finish_and_get_all_events_for(
        &mut self,
        termination: &super::tool_attempt::AttemptTermination,
    ) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 客户端可见 total 的互斥缓存分摊（与 inner 收尾一致）。
        let (final_input_tokens, cache_creation, cache_read) = self.inner.resolved_usage();

        // 生成最终事件（StreamContext 内部会用同样的优先级与分摊）
        let final_events = self.inner.generate_final_events_for(termination);
        self.event_buffer.extend(final_events);

        // 更正 message_start 事件中的 input_tokens 与 cache_* 字段
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(final_input_tokens);
                        usage["cache_creation_input_tokens"] = serde_json::json!(cache_creation);
                        usage["cache_read_input_tokens"] = serde_json::json!(cache_read);
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }

    /// 取出最终用量（在 finish_and_get_all_events 之后调用）
    ///
    /// 返回顺序：(input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, credits)
    pub fn final_usage(&self) -> (i32, i32, i32, i32, f64) {
        let (input, creation, read) = self.inner.resolved_usage();
        (
            input,
            self.inner.output_tokens,
            creation,
            read,
            self.inner.credits,
        )
    }

    /// 上游终态错误信息（转发内部 StreamContext）。缓冲流据此记 error。
    pub fn terminal_error_message(&self) -> Option<String> {
        self.inner.terminal_error_message()
    }

    pub(crate) fn terminal_error_type(&self) -> Option<&'static str> {
        self.inner.terminal_error_type()
    }

    pub(crate) fn terminal_attempt_failure(&self) -> Option<&super::tool_attempt::AttemptFailure> {
        self.inner.terminal_attempt_failure()
    }

    pub fn repetition_guard_tripped(&self) -> bool {
        self.inner.repetition_guard_tripped()
    }

    #[cfg(test)]
    pub fn terminal_tool_json_error(&self) -> Option<&ToolJsonAccumulatorError> {
        self.inner.terminal_tool_json_error()
    }
}

/// 简单的 token 估算（中英文字符混合）
///
/// 公开供 cache_meter 等模块复用同一估算口径。
pub fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_content_filter_sequence_ends_with_typed_error() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-5",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: "CONTENT_FILTERED".into(),
            },
        ));
        let _ = ctx.process_kiro_event(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 12.5,
            },
        ));
        let _ = ctx.process_kiro_event(&Event::Metering(
            crate::kiro::model::events::MeteringEvent { usage: 0.25 },
        ));

        let events = ctx.generate_final_events();

        assert!(matches!(
            ctx.terminal_attempt_failure(),
            Some(super::super::tool_attempt::AttemptFailure::ContentFiltered)
        ));
        assert!(events.iter().any(|event| {
            event.event == "error"
                && event.data["error"]["type"] == "invalid_request_error"
                && event.data["error"]["message"]
                    == "Request was blocked by upstream content filtering"
        }));
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
        assert_eq!(ctx.credits, 0.25);
    }

    #[test]
    fn metadata_content_filter_after_text_preserves_success() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-5",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut response = crate::kiro::model::events::AssistantResponseEvent::default();
        response.content = "I cannot help with that request.".into();
        let _ = ctx.process_kiro_event(&Event::AssistantResponse(response));
        let _ = ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: "CONTENT_FILTERED".into(),
            },
        ));

        let events = ctx.generate_final_events();

        assert!(ctx.terminal_attempt_failure().is_none());
        assert!(!events.iter().any(|event| event.event == "error"));
        assert!(events.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn required_any_stream_ends_with_error_without_tool_use() {
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
            super::super::converter::ToolChoicePolicy::RequiredAny {
                disable_parallel_tool_use: false,
            },
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("plain text"));
        events.extend(ctx.generate_final_events());

        assert!(events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_tool_choice_error"
        }));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn disable_parallel_stream_emits_only_first_tool_and_finishes_successfully() {
        let policies = [
            super::super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: true,
            },
            super::super::converter::ToolChoicePolicy::RequiredAny {
                disable_parallel_tool_use: true,
            },
            super::super::converter::ToolChoicePolicy::RequiredSpecific {
                name: "first_tool".to_string(),
                disable_parallel_tool_use: true,
            },
        ];

        for policy in policies {
            let known = ["first_tool".to_string(), "second_tool".to_string()]
                .into_iter()
                .collect();
            let mut ctx = StreamContext::new_with_constraints(
                "claude-opus-4-8",
                10,
                false,
                false,
                HashMap::new(),
                known,
                policy.clone(),
            );

            let mut events = ctx.generate_initial_events();
            events.extend(ctx.process_tool_use(&tool_evt(
                "tool_1",
                "first_tool",
                r#"{"value":1}"#,
                true,
            )));
            events.extend(ctx.process_tool_use(&tool_evt(
                "tool_2",
                "second_tool",
                r#"{"value":2}"#,
                true,
            )));
            events.extend(ctx.generate_final_events());

            let tool_starts = events
                .iter()
                .filter(|event| {
                    event.event == "content_block_start"
                        && event.data["content_block"]["type"] == "tool_use"
                })
                .collect::<Vec<_>>();
            assert_eq!(tool_starts.len(), 1, "policy: {policy:?}");
            assert_eq!(
                tool_starts[0].data["content_block"]["id"], "tool_1",
                "policy: {policy:?}"
            );
            assert!(
                !events.iter().any(|event| event.event == "error"),
                "policy: {policy:?}"
            );
            assert!(
                events.iter().any(|event| event.event == "message_stop"),
                "policy: {policy:?}"
            );
        }
    }

    #[test]
    fn disable_parallel_stream_reserves_first_tool_before_schema_validation() {
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            ["first_tool".to_string(), "second_tool".to_string()]
                .into_iter()
                .collect(),
            super::super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: true,
            },
        );
        ctx.set_tool_contracts(HashMap::from([
            (
                "first_tool".to_string(),
                super::super::tool_schema::ToolContract {
                    client_name: "first_tool".to_string(),
                    schema: serde_json::json!({
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }),
                },
            ),
            (
                "second_tool".to_string(),
                super::super::tool_schema::ToolContract {
                    client_name: "second_tool".to_string(),
                    schema: serde_json::json!({
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }),
                },
            ),
        ]));

        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_tool_use(&tool_evt("tool_1", "first_tool", r#"{}"#, true)));
        events.extend(ctx.process_tool_use(&tool_evt(
            "tool_2",
            "second_tool",
            r#"{"value":"ok"}"#,
            true,
        )));
        events.extend(ctx.generate_final_events());

        assert!(!events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
        }));
        assert!(events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_tool_schema_error"
        }));
    }

    #[test]
    fn disable_parallel_stream_ignores_malformed_later_tool_fragments() {
        for (input, stop) in [(r#"{"broken":}"#, true), (r#"{"half":"#, false)] {
            let mut ctx = StreamContext::new_with_constraints(
                "claude-opus-4-8",
                10,
                false,
                false,
                HashMap::new(),
                ["first_tool".to_string(), "second_tool".to_string()]
                    .into_iter()
                    .collect(),
                super::super::converter::ToolChoicePolicy::Auto {
                    disable_parallel_tool_use: true,
                },
            );

            let mut events = ctx.generate_initial_events();
            events.extend(ctx.process_tool_use(&tool_evt(
                "tool_1",
                "first_tool",
                r#"{"value":1}"#,
                true,
            )));
            events.extend(ctx.process_tool_use(&tool_evt("tool_2", "second_tool", input, stop)));
            events.extend(ctx.generate_final_events());

            let tool_starts = events
                .iter()
                .filter(|event| {
                    event.event == "content_block_start"
                        && event.data["content_block"]["type"] == "tool_use"
                })
                .collect::<Vec<_>>();
            assert_eq!(tool_starts.len(), 1, "input={input:?}, stop={stop}");
            assert_eq!(tool_starts[0].data["content_block"]["id"], "tool_1");
            assert!(
                !events.iter().any(|event| event.event == "error"),
                "input={input:?}, stop={stop}"
            );
            assert!(events.iter().any(|event| event.event == "message_stop"));
        }
    }

    #[test]
    fn disable_parallel_stream_does_not_replace_invalid_first_tool_with_same_id_replay() {
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            ["first_tool".to_string()].into_iter().collect(),
            super::super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: true,
            },
        );
        ctx.set_tool_contracts(HashMap::from([(
            "first_tool".to_string(),
            super::super::tool_schema::ToolContract {
                client_name: "first_tool".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }),
            },
        )]));

        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_tool_use(&tool_evt("tool_1", "first_tool", r#"{}"#, true)));
        events.extend(ctx.process_tool_use(&tool_evt(
            "tool_1",
            "first_tool",
            r#"{"value":"replay"}"#,
            true,
        )));
        events.extend(ctx.generate_final_events());

        assert!(!events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
        }));
        assert!(events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_tool_schema_error"
        }));
    }

    #[test]
    fn disable_parallel_stream_suppresses_same_id_after_first_tool_completed() {
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            ["first_tool".to_string()].into_iter().collect(),
            super::super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: true,
            },
        );

        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_tool_use(&tool_evt(
            "tool_1",
            "first_tool",
            r#"{"value":1}"#,
            true,
        )));
        events.extend(ctx.process_tool_use(&tool_evt(
            "tool_1",
            "first_tool",
            r#"{"value":2}"#,
            true,
        )));
        events.extend(ctx.generate_final_events());

        let tool_starts = events
            .iter()
            .filter(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_starts.len(), 1);
        assert!(!events.iter().any(|event| event.event == "error"));
        assert!(events.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn required_native_tool_is_first_content_block_and_discards_narration() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            known,
            super::super::converter::ToolChoicePolicy::RequiredSpecific {
                name: "get_weather".into(),
                disable_parallel_tool_use: false,
            },
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("I will call the weather tool."));
        events.extend(ctx.process_tool_use(&tool_evt(
            "tool_1",
            "get_weather",
            "{\"location\":\"Paris\"}",
            true,
        )));
        events.extend(ctx.generate_final_events());

        assert_eq!(events[0].event, "message_start");
        let first_content = events
            .iter()
            .find(|event| event.event == "content_block_start")
            .expect("required tool response must contain a content block");
        assert_eq!(first_content.data["content_block"]["type"], "tool_use");
        assert_eq!(first_content.data["index"], 0);
        assert!(!events.iter().any(|event| {
            event.event == "content_block_start" && event.data["content_block"]["type"] == "text"
        }));
    }

    #[test]
    fn required_native_tool_discards_unrequested_reasoning_text_before_tool() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            known,
            super::super::converter::ToolChoicePolicy::RequiredSpecific {
                name: "get_weather".into(),
                disable_parallel_tool_use: false,
            },
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_reasoning_content(
            &crate::kiro::model::events::ReasoningContentEvent {
                text: Some("I should call the weather tool.".into()),
                signature: None,
                redacted_content: None,
            },
        ));
        events.extend(ctx.process_tool_use(&tool_evt(
            "tool_1",
            "get_weather",
            "{\"location\":\"Paris\"}",
            true,
        )));
        events.extend(ctx.generate_final_events());

        let first_content = events
            .iter()
            .find(|event| event.event == "content_block_start")
            .expect("required tool response must contain a content block");
        assert_eq!(first_content.data["content_block"]["type"], "tool_use");
        assert_eq!(first_content.data["index"], 0);
        assert!(!events.iter().any(|event| {
            event.event == "content_block_start" && event.data["content_block"]["type"] == "text"
        }));
    }

    #[test]
    fn auto_policy_keeps_text_before_native_tool() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            known,
            super::super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: false,
            },
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("I will call it."));
        events.extend(ctx.process_tool_use(&tool_evt("tool_1", "get_weather", "{}", true)));
        events.extend(ctx.generate_final_events());

        let content_types = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .filter_map(|event| event.data["content_block"]["type"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(content_types, vec!["text", "tool_use"]);
    }

    #[test]
    fn required_textual_invoke_is_recovered_as_first_tool_block() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            false,
            false,
            HashMap::new(),
            known,
            super::super::converter::ToolChoicePolicy::RequiredSpecific {
                name: "get_weather".into(),
                disable_parallel_tool_use: false,
            },
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response(
            "call\n<invoke name=\"get_weather\"><parameter name=\"location\">Paris</parameter></invoke>",
        ));
        events.extend(ctx.generate_final_events());

        let first_content = events
            .iter()
            .find(|event| event.event == "content_block_start")
            .expect("textual invoke must become a content block");
        assert_eq!(first_content.data["content_block"]["type"], "tool_use");
        assert_eq!(first_content.data["index"], 0);
        assert!(!events.iter().any(|event| event.event == "error"));
    }

    #[test]
    fn compatible_thinking_stream_finishes_when_plain_text_arrives() {
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            true,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
            super::super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: false,
            },
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("正常中文回复"));
        events.extend(ctx.generate_final_events());

        assert!(events.iter().any(|event| event.event == "message_stop"));
        assert!(!events.iter().any(|event| event.event == "error"));
    }

    #[test]
    fn strict_thinking_stream_errors_when_plain_text_arrives() {
        let mut ctx = StreamContext::new_with_constraints(
            "claude-opus-4-8",
            10,
            true,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
            super::super::converter::ToolChoicePolicy::Auto {
                disable_parallel_tool_use: false,
            },
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("plain text"));
        events.extend(ctx.generate_final_events());

        assert!(events.iter().any(|event| {
            event.event == "error"
                && event.data["error"]["type"] == "upstream_thinking_protocol_error"
        }));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn context_usage_drives_stream_api_usage() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            72,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_kiro_event(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 0.5417,
            },
        ));

        // 终态 usage 必须报上游真实占用：客户端据此判断是否该自动压缩。
        assert_eq!(ctx.resolved_usage(), (5_417, 0, 0));
        assert_eq!(ctx.upstream_context_tokens(), Some(5_417));
    }

    #[test]
    fn context_window_snapshot_drives_context_usage() {
        let mut ctx = StreamContext::new_with_thinking(
            "custom-model",
            72,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.set_context_window_size(1_000_000);
        ctx.process_kiro_event(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 50.0,
            },
        ));
        assert_eq!(ctx.upstream_context_tokens(), Some(500_000));
    }

    #[test]
    fn buffered_cc_stream_rewrites_message_start_with_upstream_context() {
        let mut ctx = BufferedStreamContext::new(
            "claude-opus-4.8",
            72,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_and_buffer(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 0.5417,
            },
        ));
        let events = ctx.finish_and_get_all_events();
        let start = events
            .iter()
            .find(|event| event.event == "message_start")
            .unwrap();
        // 缓冲流吐 message_start 时 contextUsageEvent 已到，必须带真实占用。
        assert_eq!(start.data["message"]["usage"]["input_tokens"], 5_417);
    }

    #[test]
    fn full_upstream_context_still_sets_overflow_stop_reason() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            72,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.process_kiro_event(&Event::ContextUsage(
            crate::kiro::model::events::ContextUsageEvent {
                context_usage_percentage: 100.0,
            },
        ));
        assert_eq!(
            ctx.state_manager.get_stop_reason(),
            "model_context_window_exceeded"
        );
        let events = ctx.generate_final_events();
        assert!(events.iter().any(|event| {
            event.event == "error"
                && event.data["error"]["type"] == "upstream_context_window_exceeded"
        }));
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn empty_upstream_stream_emits_error_without_success_terminal_events() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let events = ctx.generate_final_events();

        assert!(events.iter().any(|event| event.event == "error"));
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
        assert_eq!(
            ctx.terminal_error_message().as_deref(),
            Some("Upstream returned no assistant content after one retry")
        );
        let error = events.iter().find(|event| event.event == "error").unwrap();
        assert_eq!(error.data["error"]["type"], "upstream_empty_response");
    }

    #[test]
    fn explicit_upstream_error_preserves_safe_reason() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::Error {
            error_code: "ValidationException".into(),
            error_message: "Input content length exceeds threshold.".into(),
        });
        let events = ctx.generate_final_events();

        let error = events.iter().find(|event| event.event == "error").unwrap();
        assert_eq!(error.data["error"]["type"], "upstream_protocol_error");
        assert_eq!(
            error.data["error"]["message"],
            "Upstream reported ValidationException: Input content length exceeds threshold."
        );
    }

    #[test]
    fn explicit_upstream_exception_preserves_bounded_safe_reason() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let reason = format!("{}TAIL", "temporary upstream capacity issue ".repeat(40));
        let _ = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ModelError".into(),
            message: reason,
        });
        let events = ctx.generate_final_events();

        let error = events.iter().find(|event| event.event == "error").unwrap();
        let message = error.data["error"]["message"].as_str().unwrap();
        assert_eq!(error.data["error"]["type"], "upstream_protocol_error");
        assert!(
            message.starts_with("Upstream reported ModelError: temporary upstream capacity issue")
        );
        assert!(!message.contains("TAIL"));
        assert!(message.chars().count() <= "Upstream reported ModelError: ".chars().count() + 512);
    }

    #[test]
    fn explicit_upstream_exception_is_preserved_without_success_terminal_events() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let sensitive = "request body: secret customer document";
        let _ = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ModelError".into(),
            message: sensitive.into(),
        });
        let events = ctx.generate_final_events();

        let error = events.iter().find(|event| event.event == "error").unwrap();
        assert_eq!(error.data["error"]["type"], "upstream_protocol_error");
        assert!(
            error.data["error"]["message"]
                .as_str()
                .unwrap()
                .contains("ModelError")
        );
        assert!(!error.data.to_string().contains(sensitive));
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
        assert!(matches!(
            ctx.terminal_attempt_failure(),
            Some(super::super::tool_attempt::AttemptFailure::UpstreamError {
                error_type,
                message,
            }) if error_type == "ModelError" && message == sensitive
        ));
    }

    #[test]
    fn content_length_exception_keeps_max_tokens_success_terminal() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_assistant_response("partial output");
        let _ = ctx.process_kiro_event(&Event::Exception {
            exception_type: "ContentLengthExceededException".into(),
            message: "output limit reached".into(),
        });
        let events = ctx.generate_final_events();

        assert!(!events.iter().any(|event| event.event == "error"));
        assert!(events.iter().any(|event| {
            event.event == "message_delta" && event.data["delta"]["stop_reason"] == "max_tokens"
        }));
        assert!(events.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn read_error_finalization_never_emits_success_terminal_events() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_assistant_response("partial text");
        let events = ctx.generate_final_events_for(
            &super::super::tool_attempt::AttemptTermination::ReadError(
                "secret transport detail".into(),
            ),
        );

        assert!(events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_stream_interrupted"
        }));
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
        assert!(
            !events
                .iter()
                .any(|event| event.data.to_string().contains("secret transport detail"))
        );
    }

    #[test]
    fn idle_timeout_and_client_close_never_emit_success_terminal_events() {
        let mut idle = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = idle.generate_initial_events();
        let events = idle.generate_final_events_for(
            &super::super::tool_attempt::AttemptTermination::IdleTimeout,
        );
        assert!(events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_stream_idle_timeout"
        }));
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));

        let mut closed = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        assert!(
            closed
                .generate_final_events_for(
                    &super::super::tool_attempt::AttemptTermination::ClientClosed,
                )
                .is_empty()
        );
    }

    #[test]
    fn incomplete_tool_signal_emits_error_without_success_terminal_events() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4.8",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_tool_use(&tool_evt("tool_1", "test_tool", "{\"half\":", false));
        let events = ctx.generate_final_events();

        assert!(events.iter().any(|event| event.event == "error"));
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
        assert!(!events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
        }));
        assert!(matches!(
            ctx.terminal_tool_json_error(),
            Some(ToolJsonAccumulatorError::IncompleteJson { .. })
        ));
    }

    // ---- ToolJsonAccumulator: 流式半截 / 非法工具调用 JSON ----

    fn tool_evt(
        id: &str,
        name: &str,
        input: &str,
        stop: bool,
    ) -> crate::kiro::model::events::ToolUseEvent {
        crate::kiro::model::events::ToolUseEvent {
            name: name.to_string(),
            tool_use_id: id.to_string(),
            input: input.to_string(),
            stop,
        }
    }

    fn weather_contracts() -> HashMap<String, super::super::tool_schema::ToolContract> {
        HashMap::from([(
            "get_weather".to_string(),
            super::super::tool_schema::ToolContract {
                client_name: "get_weather".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"},
                        "unit": {"type": "string", "enum": ["celsius"]}
                    },
                    "required": ["city", "unit"],
                    "additionalProperties": false
                }),
            },
        )])
    }

    #[test]
    fn stream_repairs_fixed_tool_fields_before_any_tool_event_is_emitted() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            ["get_weather".to_string()].into_iter().collect(),
        );
        ctx.set_tool_contracts(weather_contracts());

        let events = ctx.process_tool_use(&tool_evt(
            "tool_1",
            "get_weather",
            r#"{"city":"Paris","unit":"wrong"}"#,
            true,
        ));
        let json_delta = events
            .iter()
            .find(|event| event.event == "content_block_delta")
            .expect("repaired tool input delta");
        let input: serde_json::Value =
            serde_json::from_str(json_delta.data["delta"]["partial_json"].as_str().unwrap())
                .unwrap();
        assert_eq!(input["unit"], "celsius");
    }

    #[test]
    fn stream_rejects_non_fixed_schema_violation_without_emitting_tool_block() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            ["get_weather".to_string()].into_iter().collect(),
        );
        ctx.set_tool_contracts(weather_contracts());

        let tool_events = ctx.process_tool_use(&tool_evt(
            "tool_1",
            "get_weather",
            r#"{"unit":"celsius"}"#,
            true,
        ));
        let final_events = ctx.generate_final_events();

        assert!(tool_events.is_empty());
        assert!(final_events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_tool_schema_error"
        }));
        assert!(!final_events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
        }));
    }

    #[test]
    /// 未声明且无等价工具 → 降级成文本交付，**不得**发出 tool_use（否则客户端会去执行
    /// 一个它没声明的工具），也不再整轮报错（此前是零重试 502）。
    fn stream_degrades_undeclared_tool_to_text_when_contracts_exist() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            ["get_weather".to_string()].into_iter().collect(),
        );
        ctx.set_tool_contracts(weather_contracts());

        let tool_events =
            ctx.process_tool_use(&tool_evt("tool_1", "delete_everything", r#"{}"#, true));
        let final_events = ctx.generate_final_events();

        let all: Vec<_> = tool_events.iter().chain(final_events.iter()).collect();
        let errors: Vec<_> = all.iter().filter(|e| e.event == "error").collect();
        assert!(errors.is_empty(), "降级路径不应再报错: {errors:?}");
        assert!(
            !all.iter()
                .any(|event| event.data.pointer("/content_block/type") == Some(&json!("tool_use"))),
            "绝不能把未声明工具当 tool_use 发给客户端"
        );
        let text: String = all
            .iter()
            .filter(|event| event.data["delta"]["type"] == "text_delta")
            .filter_map(|event| event.data["delta"]["text"].as_str())
            .collect();
        assert!(
            text.contains("delete_everything"),
            "应把工具意图以文本呈现: {text:?}"
        );
    }

    #[test]
    /// 客户端一个工具都没声明（Cline / Codex 系靠系统提示词描述工具）时同样走降级，
    /// 而不是把整轮打成 502——这类客户端本来就从文本里自己解析工具调用。
    fn stream_degrades_unrequested_tool_after_empty_contracts_are_initialized() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.set_tool_contracts(HashMap::new());

        let tool_events =
            ctx.process_tool_use(&tool_evt("tool_1", "delete_everything", r#"{}"#, true));
        let final_events = ctx.generate_final_events();

        let all: Vec<_> = tool_events.iter().chain(final_events.iter()).collect();
        assert!(
            !all.iter().any(|event| event.event == "error"),
            "空 contracts 也不应整轮报错"
        );
        assert!(
            !all.iter()
                .any(|event| event.data.pointer("/content_block/type") == Some(&json!("tool_use"))),
        );
    }

    #[test]
    fn tool_json_accumulator_reassembles_split_fragments() {
        let mut acc = ToolJsonAccumulator::new();
        let map = HashMap::new();
        // 用非内置工具名，专注验证分片重组本身（内置名的双向映射另有专项测试）。
        // JSON 被切成三片（切在 token 中间），只有最后一片带 stop。
        assert!(
            acc.push(&tool_evt("t1", "custom_tool", "{\"pa", false), &map)
                .unwrap()
                .is_none()
        );
        assert!(
            acc.push(&tool_evt("t1", "custom_tool", "th\":\"/a", false), &map)
                .unwrap()
                .is_none()
        );
        let completed = acc
            .push(&tool_evt("t1", "custom_tool", ".txt\"}", true), &map)
            .unwrap()
            .unwrap();
        assert_eq!(completed.id, "t1");
        assert_eq!(completed.name, "custom_tool");
        assert_eq!(completed.input, serde_json::json!({"path": "/a.txt"}));
    }

    #[test]
    fn tool_json_accumulator_empty_input_is_empty_object() {
        let mut acc = ToolJsonAccumulator::new();
        let completed = acc
            .push(&tool_evt("t1", "noop", "", true), &HashMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(completed.input, serde_json::json!({}));
    }

    /// 断流（未收 stop=true）+ 空入参：**有 required 字段**的工具不能按 `{}` 打捞，
    /// 否则必然 missing required → 不可重试硬失败。应报可重试的 IncompleteJson。
    #[test]
    fn finish_reports_incomplete_json_for_empty_input_when_schema_has_required_fields() {
        let contracts = HashMap::from([(
            "Read".to_string(),
            crate::anthropic::tool_schema::ToolContract {
                client_name: "Read".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"file_path": {"type": "string"}},
                    "required": ["file_path"],
                    "additionalProperties": false
                }),
            },
        )]);
        let mut acc = ToolJsonAccumulator::new();
        // stop=false 模拟断流：块开出来了但没写任何参数。
        assert!(
            acc.push(&tool_evt("t1", "Read", "", false), &HashMap::new())
                .unwrap()
                .is_none()
        );

        let (salvaged, err) = acc.finish(&HashMap::new(), &contracts);

        assert!(salvaged.is_empty(), "有 required 字段时不应打捞成 {{}}");
        assert!(
            matches!(err, Some(ToolJsonAccumulatorError::IncompleteJson { .. })),
            "应报可重试的 IncompleteJson，实际: {err:?}"
        );
    }

    /// 反面：无 required 字段的无参工具（如 EnterPlanMode）仍按 `{}` 正常打捞，
    /// 不能因为上面的收敛把合法无参调用打掉。
    #[test]
    fn finish_still_salvages_empty_input_for_tools_without_required_fields() {
        let contracts = HashMap::from([(
            "EnterPlanMode".to_string(),
            crate::anthropic::tool_schema::ToolContract {
                client_name: "EnterPlanMode".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            },
        )]);
        let mut acc = ToolJsonAccumulator::new();
        assert!(
            acc.push(&tool_evt("t1", "EnterPlanMode", "", false), &HashMap::new())
                .unwrap()
                .is_none()
        );

        let (salvaged, err) = acc.finish(&HashMap::new(), &contracts);

        assert!(err.is_none(), "无参工具不应报错: {err:?}");
        assert_eq!(salvaged.len(), 1);
        assert_eq!(salvaged[0].input, serde_json::json!({}));
    }

    #[test]
    fn tool_json_accumulator_invalid_json_errors() {
        let mut acc = ToolJsonAccumulator::new();
        let err = acc
            .push(
                &tool_evt("t1", "read_file", "{not json", true),
                &HashMap::new(),
            )
            .unwrap_err();
        assert_eq!(err.error_type(), "upstream_tool_json_error");
        assert!(matches!(err, ToolJsonAccumulatorError::InvalidJson { .. }));
    }

    #[test]
    fn tool_json_accumulator_incomplete_on_missing_stop() {
        let mut acc = ToolJsonAccumulator::new();
        // 有字节但从未 stop → finish() 报 IncompleteJson（不打捞半截 JSON）。
        assert!(
            acc.push(
                &tool_evt("t1", "read_file", "{\"path\":\"/a", false),
                &HashMap::new()
            )
            .unwrap()
            .is_none()
        );
        let (salvaged, err) = acc.finish(&HashMap::new(), &HashMap::new());
        assert!(salvaged.is_empty(), "半截 JSON 不应被打捞");
        let err = err.expect("半截 JSON 应报 IncompleteJson");
        assert!(matches!(
            err,
            ToolJsonAccumulatorError::IncompleteJson { .. }
        ));
        // 已取出残留后再 finish() 应无错误、无打捞。
        let (salvaged, err) = acc.finish(&HashMap::new(), &HashMap::new());
        assert!(salvaged.is_empty() && err.is_none());
    }

    #[test]
    fn tool_json_accumulator_salvages_empty_input_on_missing_stop() {
        // 回归：无参工具（如 EnterPlanMode）上游开了 tool_use 块但未发 stop=true 即断流，
        // 残留缓冲为 0 字节 → 应按 {} 打捞成完整工具调用，而非丢弃报错。
        let mut acc = ToolJsonAccumulator::new();
        assert!(
            acc.push(&tool_evt("t1", "EnterPlanMode", "", false), &HashMap::new())
                .unwrap()
                .is_none()
        );
        let (salvaged, err) = acc.finish(&HashMap::new(), &HashMap::new());
        assert!(err.is_none(), "空入参不应报错");
        assert_eq!(salvaged.len(), 1);
        assert_eq!(salvaged[0].id, "t1");
        assert_eq!(salvaged[0].name, "EnterPlanMode");
        assert_eq!(salvaged[0].input, serde_json::json!({}));
    }

    #[test]
    fn tool_json_accumulator_salvages_complete_json_without_stop() {
        let mut acc = ToolJsonAccumulator::new();
        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        acc.push(
            &tool_evt(
                "write1",
                "fs_write",
                r#"{"path":"/tmp/a","text":"ok"}"#,
                false,
            ),
            &map,
        )
        .unwrap();

        let (completed, err) = acc.finish(&map, &HashMap::new());
        assert!(err.is_none(), "完整 JSON 残留应按隐式 stop 打捞");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, "write1");
        assert_eq!(completed[0].name, "Write");
        assert_eq!(
            completed[0].input,
            serde_json::json!({"file_path": "/tmp/a", "content": "ok"})
        );
    }

    #[test]
    fn tool_json_accumulator_distinguishes_invalid_from_incomplete_at_finish() {
        let mut incomplete = ToolJsonAccumulator::new();
        incomplete
            .push(
                &tool_evt("incomplete1", "read_file", r#"{"path":"/a"#, false),
                &HashMap::new(),
            )
            .unwrap();
        let (completed, err) = incomplete.finish(&HashMap::new(), &HashMap::new());
        assert!(completed.is_empty());
        assert!(matches!(
            err,
            Some(ToolJsonAccumulatorError::IncompleteJson { .. })
        ));

        let mut invalid = ToolJsonAccumulator::new();
        invalid
            .push(
                &tool_evt("invalid1", "read_file", r#"{"path":]"#, false),
                &HashMap::new(),
            )
            .unwrap();
        let (completed, err) = invalid.finish(&HashMap::new(), &HashMap::new());
        assert!(completed.is_empty());
        assert!(matches!(
            err,
            Some(ToolJsonAccumulatorError::InvalidJson { .. })
        ));
    }

    #[test]
    fn tool_json_accumulator_finish_is_atomic_when_any_entry_errors() {
        // 多残留：旧实现会先把空参工具按 {} 打捞，再同时返回半截错误；
        // 新契约要求只要任一项出错，本批就不得返回任何工具调用。
        let mut acc = ToolJsonAccumulator::new();
        acc.push(
            &tool_evt("empty1", "EnterPlanMode", "", false),
            &HashMap::new(),
        )
        .unwrap();
        acc.push(
            &tool_evt("half1", "read_file", "{\"path\":\"/a", false),
            &HashMap::new(),
        )
        .unwrap();
        let (completed, err) = acc.finish(&HashMap::new(), &HashMap::new());
        assert!(completed.is_empty(), "错误批次不得部分提交工具调用");
        assert!(
            matches!(err, Some(ToolJsonAccumulatorError::IncompleteJson { .. })),
            "半截 JSON 仍应报错"
        );
    }

    #[test]
    fn tool_json_accumulator_finish_is_atomic_with_invalid_json() {
        let mut acc = ToolJsonAccumulator::new();
        acc.push(
            &tool_evt("empty1", "EnterPlanMode", "", false),
            &HashMap::new(),
        )
        .unwrap();
        acc.push(
            &tool_evt("invalid1", "read_file", r#"{"path":]"#, false),
            &HashMap::new(),
        )
        .unwrap();

        let (completed, err) = acc.finish(&HashMap::new(), &HashMap::new());
        assert!(completed.is_empty(), "非法 JSON 批次不得部分提交空参工具");
        assert!(matches!(
            err,
            Some(ToolJsonAccumulatorError::InvalidJson { .. })
        ));
    }

    #[test]
    fn tool_json_accumulator_restores_short_tool_name() {
        let mut acc = ToolJsonAccumulator::new();
        let mut map = HashMap::new();
        map.insert(
            "short_abc123".to_string(),
            "the_original_very_long_tool_name".to_string(),
        );
        let completed = acc
            .push(&tool_evt("t1", "short_abc123", "{}", true), &map)
            .unwrap()
            .unwrap();
        assert_eq!(completed.name, "the_original_very_long_tool_name");
    }

    /// 防回归：统一管道的两个去向（流式 emit_completed_tool_use 与非流式 to_anthropic_block）
    /// 对同一 CompletedToolUse 产出一致的 id / name / input。
    #[test]
    fn emit_and_block_agree_on_shape() {
        let completed = CompletedToolUse {
            id: "toolu_1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/a"}),
        };

        // 非流式块
        let block = completed.to_anthropic_block();
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "toolu_1");
        assert_eq!(block["name"], "read_file");
        assert_eq!(block["input"], serde_json::json!({"path": "/a"}));

        // 流式发出：start 的 id/name 与块一致；delta 的 partial_json 解析后与块 input 一致。
        let mut ctx = StreamContext::new_with_thinking(
            "m",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let events = ctx.emit_completed_tool_use(completed);
        let start = events
            .iter()
            .find(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
            })
            .expect("应有 tool_use content_block_start");
        assert_eq!(start.data["content_block"]["id"], block["id"]);
        assert_eq!(start.data["content_block"]["name"], block["name"]);
        let delta = events
            .iter()
            .find(|e| e.event == "content_block_delta")
            .expect("应有 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(partial).unwrap();
        assert_eq!(
            parsed, block["input"],
            "流式增量拼出的 input 应与非流式块一致"
        );
        assert!(events.iter().any(|e| e.event == "content_block_stop"));
    }

    // ---- <tool_use> XML 泄漏过滤 ----

    #[test]
    fn tool_use_xml_filter_strips_single_chunk_block() {
        let mut f = ToolUseXmlLeakFilter::default();
        let out = f
            .filter("before <tool_use id=\"t\" name=\"Write\">{\"path\":\"/a\"}</tool_use> after")
            + &f.finish();
        assert!(!out.contains("<tool_use"));
        assert!(out.contains("before") && out.contains("after"));
    }

    #[test]
    fn tool_use_xml_filter_strips_cross_chunk_open_split() {
        let mut f = ToolUseXmlLeakFilter::default();
        let mut out = f.filter("before <tool");
        out.push_str(&f.filter("_use id=\"t\">{\"a\":1}</tool_use>after"));
        out.push_str(&f.finish());
        assert!(!out.contains("<tool_use"), "out={out:?}");
        assert!(out.contains("before") && out.contains("after"));
    }

    /// 优化点：闭合标签 `</tool_use>` 被切分到多个 chunk 时也应完整剥离，
    /// 且其后文本不被吞（参考实现在此会漏）。
    #[test]
    fn tool_use_xml_filter_strips_cross_chunk_close_split() {
        let mut f = ToolUseXmlLeakFilter::default();
        let mut out = f.filter("x <tool_use name=\"W\">{\"a\":1}</tool");
        out.push_str(&f.filter("_use>y"));
        out.push_str(&f.finish());
        assert!(!out.contains("<tool_use"), "out={out:?}");
        assert!(
            out.contains('x') && out.contains('y'),
            "闭合跨 chunk 时其后文本不应被吞: {out:?}"
        );
    }

    #[test]
    fn tool_use_xml_filter_keeps_similar_text() {
        let mut f = ToolUseXmlLeakFilter::default();
        let out = f.filter("use <tool_user> here") + &f.finish();
        assert_eq!(out, "use <tool_user> here");
    }

    #[test]
    fn tool_use_xml_filter_drops_unclosed_at_finish() {
        let mut f = ToolUseXmlLeakFilter::default();
        let mut out = f.filter("keep <tool_use name=\"W\">partial...");
        out.push_str(&f.finish());
        assert!(out.contains("keep"));
        assert!(!out.contains("<tool_use") && !out.contains("partial"));
    }

    #[test]
    fn stream_context_filters_tool_use_xml_leak() {
        let mut ctx = StreamContext::new_with_thinking(
            "m",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("hello <tool"));
        events.extend(ctx.process_assistant_response("_use name=\"W\">{\"a\":1}</tool_use> world"));
        events.extend(ctx.generate_final_events());
        let text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(!text.contains("<tool_use"), "泄漏未被过滤: {text:?}");
        assert!(text.contains("hello") && text.contains("world"));
    }

    /// 测试用的「已知工具表」：包含 invoke 测试里会合成的工具名，
    /// 让 🅳 工具表校验放行这些名字，从而能验证捞回逻辑本身。
    fn test_known_tools() -> std::collections::HashSet<String> {
        [
            "exec_command",
            "apply_patch",
            "tool_a",
            "tool_b",
            "write_file",
            "wait_agent",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    // ---- extract_invoke_content_blocks: one-shot (non-streaming) reclamation ----

    #[test]
    fn extract_blocks_reclaims_clean_leak_and_strips_stray_token() {
        let text = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let blocks = extract_invoke_content_blocks(
            text,
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        let tu = blocks
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("must reclaim tool_use");
        assert_eq!(tu["name"], "exec_command");
        assert_eq!(tu["input"]["cmd"], "echo hi");
        assert!(
            !blocks.iter().any(|b| b["type"] == "text"
                && b["text"]
                    .as_str()
                    .map(|t| t.contains("<invoke"))
                    .unwrap_or(false)),
            "no literal <invoke> may remain as text"
        );
        assert!(
            !blocks
                .iter()
                .any(|b| b["type"] == "text" && b["text"] == "call\n"),
            "stray token line must be stripped"
        );
    }

    #[test]
    fn extract_blocks_restores_shortened_name_via_map() {
        let short = "shrunk_name_abcd1234";
        let original = "an_extremely_long_original_tool_name_that_exceeds_the_limit";
        let text = format!(
            "call\n<invoke name=\"{}\">\n<parameter name=\"x\">y</parameter>\n</invoke>",
            short
        );
        let mut known = std::collections::HashSet::new();
        known.insert(short.to_string());
        let mut map = std::collections::HashMap::new();
        map.insert(short.to_string(), original.to_string());
        let blocks = extract_invoke_content_blocks(&text, &known, &map);
        let tu = blocks
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("reclaimed");
        assert_eq!(
            tu["name"], original,
            "shortened name must be restored to original"
        );
    }

    #[test]
    fn extract_blocks_does_not_reclaim_fenced_or_unknown() {
        // fenced -> display, not reclaimed
        let fenced = "see:\n```\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>\n```";
        let b1 = extract_invoke_content_blocks(
            fenced,
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        assert!(
            !b1.iter().any(|b| b["type"] == "tool_use"),
            "fenced must not reclaim"
        );
        // unknown tool name -> not reclaimed
        let unknown = "call\n<invoke name=\"not_a_real_tool\">\n<parameter name=\"x\">y</parameter>\n</invoke>";
        let b2 = extract_invoke_content_blocks(
            unknown,
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        assert!(
            !b2.iter().any(|b| b["type"] == "tool_use"),
            "unknown name must not reclaim"
        );
    }

    #[test]
    fn extract_blocks_clean_text_is_single_unchanged_text_block() {
        let blocks = extract_invoke_content_blocks(
            "just a normal answer with no tool calls",
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "just a normal answer with no tool calls");
    }

    #[test]
    fn normalize_non_stream_content_recovers_get_weather_and_deduplicates_native_call() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let native = vec![serde_json::json!({
            "type": "tool_use",
            "id": "toolu_native",
            "name": "get_weather",
            "input": {"location": "Paris"}
        })];
        let base = vec![serde_json::json!({
            "type": "text",
            "text": "call\n<invoke name=\"get_weather\">\n<parameter name=\"location\">Paris</parameter>\n</invoke>"
        })];
        let blocks = normalize_non_stream_content_blocks(base, native, &known, &HashMap::new());
        let tools: Vec<_> = blocks
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["id"], "toolu_native");
        assert!(!blocks.iter().any(|block| {
            block["type"] == "text"
                && block["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("<invoke"))
        }));
    }

    #[test]
    fn normalize_non_stream_content_recovers_text_only_tool_call() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let blocks = normalize_non_stream_content_blocks(
            vec![serde_json::json!({
                "type": "text",
                "text": "call\n<invoke name=\"get_weather\"><parameter name=\"location\">Paris</parameter></invoke>"
            })],
            Vec::new(),
            &known,
            &HashMap::new(),
        );
        assert!(
            blocks
                .iter()
                .any(|block| { block["type"] == "tool_use" && block["name"] == "get_weather" })
        );
    }

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, map, test_known_tools());
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true, // 累积器仅在 stop=true 时整体发出工具调用（含关闭前一个块）
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn repeat_guard_trips_when_upstream_fragments_mid_line() {
        // 回归（线上实测）：上游分片不按行边界切，实测形态
        // `"NEXT ITEM."` → `"\nCHECK NE"` → `"XT ITEM"` → `".\n\nDONE OK"`。
        // 旧实现直接比较 split_inclusive('\n') 的产物，等于在比较碎片，每次都不同，
        // 连续计数永不累积——正文通道的熔断因此从未生效：线上 20 次连续相同行原样放行、
        // 无任何告警。既有测试全按「整行投喂」构造，所以一直绿着却漏掉了这个形态。
        let line = "CHECK NEXT ITEM.\n";
        let flood: String = line.repeat(120);

        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        // 按 5 字节硬切，故意让每片都跨行边界。
        let bytes = flood.as_bytes();
        let mut events = Vec::new();
        let mut start = 0usize;
        while start < bytes.len() {
            let mut end = (start + 5).min(bytes.len());
            while end < bytes.len() && !flood.is_char_boundary(end) {
                end += 1;
            }
            events.extend(ctx.process_assistant_response(&flood[start..end]));
            start = end;
        }
        events.extend(ctx.generate_final_events());

        assert!(ctx.repetition_guard_tripped(), "分片投喂下熔断必须仍然跳闸");
        let text = collect_text_content(&events);
        let passed = text.matches("CHECK NEXT ITEM.").count();
        assert!(
            passed <= REPEAT_GUARD_TRIP_THRESHOLD as usize,
            "放行数不应超过阈值，实际 {passed}"
        );
        assert!(passed < 120, "复读必须被截断，实际放行 {passed} / 120");
    }

    #[test]
    fn repeat_guard_does_not_trip_on_fragmented_distinct_lines() {
        // 反向保护：分片重组不得把「不同的行」误并成同一行而误判。
        // Kiro-Go 因为内容级去重吃掉真实输出而删掉了整个机制
        //（把 6666666666 吃成 666、1833 吃成 183，见 proxy/kiro.go:608），
        // 这里确保按完整行比较不会重现那类误伤。
        let mut body = String::new();
        for i in 0..120 {
            body.push_str(&format!("line {i} distinct content here\n"));
        }

        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let bytes = body.as_bytes();
        let mut events = Vec::new();
        let mut start = 0usize;
        while start < bytes.len() {
            let mut end = (start + 7).min(bytes.len());
            while end < bytes.len() && !body.is_char_boundary(end) {
                end += 1;
            }
            events.extend(ctx.process_assistant_response(&body[start..end]));
            start = end;
        }

        assert!(!ctx.repetition_guard_tripped(), "不同内容的行不得触发熔断");
        let text = collect_text_content(&events);
        assert!(
            text.contains("line 0 distinct") && text.contains("line 119 distinct"),
            "所有正常行必须完整放行，不得丢字节"
        );
    }

    #[test]
    fn thinking_pipeline_survives_adversarial_fragmentation() {
        // 属性测试：把一批「刁钻但合法」的内容按 1..=9 字节硬切喂进流水线，断言
        // 既不 panic（UTF-8 边界 / 字节索引），也不静默吃掉内容。
        //
        // 这轮六个 bug 全是「按理想形态构造的测试一直绿着」造成的，所以这里刻意覆盖
        // 多字节字符、标签紧贴标点/引号、未闭合标签、嵌套提及等形态。
        let corpus = [
            "普通中文回答，没有任何标签。",
            "<thinking>\n推理内容\n</thinking>\n\n正文",
            "Starting now.<thinking>\nreasoning\n</thinking>\nDone.",
            "answer<thinking>`cargo test`失败</thinking>next",
            "见 `code`</thinking>\n收尾",
            "<thinking>未闭合的推理，流就这么结束了",
            "讨论 `<thinking>` 这个标签本身,不该被当成开标签",
            "混合 emoji 🚀 与中文，以及 tab\t和多空格   结尾 ",
            "</thinking></thinking></thinking>连续闭标签",
            "行一\n行一\n行一\n不同行\n行一\n",
            "",
            "\n\n\n",
            "a",
        ];

        for text in corpus {
            for step in 1..=9usize {
                let mut ctx = StreamContext::new_with_thinking(
                    "test-model",
                    1,
                    true,
                    HashMap::new(),
                    test_known_tools(),
                );
                let _ = ctx.generate_initial_events();

                let mut events = Vec::new();
                let mut start = 0usize;
                while start < text.len() {
                    let mut end = (start + step).min(text.len());
                    while end < text.len() && !text.is_char_boundary(end) {
                        end += 1;
                    }
                    // 不 panic 即为通过的第一层断言。
                    events.extend(ctx.process_assistant_response(&text[start..end]));
                    start = end;
                }
                events.extend(ctx.generate_final_events());

                // 第二层：内容不得凭空消失。把 thinking 与正文合起来，去掉标签与空白后，
                // 原文的可见字符应当全部还在（顺序可能因分块而变，故只比字符集合与总量）。
                let delivered: String =
                    collect_thinking_content(&events) + &collect_text_content(&events);
                let strip = |s: &str| -> String {
                    s.replace("<thinking>", "")
                        .replace("</thinking>", "")
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect()
                };
                let want = strip(text);
                let got = strip(&delivered);
                assert!(
                    got.chars().count() >= want.chars().count(),
                    "step={step} 内容丢失\n原文有效字符={:?}\n交付有效字符={:?}\n原始输入={text:?}",
                    want,
                    got
                );
            }
        }
    }

    #[test]
    fn invoke_start_is_found_regardless_of_preceding_char() {
        // 漏检一个真 invoke 的后果远重于误判：工具永不执行、整块 XML 泄漏成正文，
        // 对话打断且无法恢复。误判方向有 5 道下游闸门兜底（闭合标签/解析/工具表/围栏/
        // 泄漏启发式），所以入口不再做引用字符判定。
        for (name, input) in [
            ("行首", "<invoke name=\"Read\">"),
            ("紧跟反引号", "`<invoke name=\"Read\">"),
            ("紧跟双引号", "\"<invoke name=\"Read\">"),
            ("紧跟反斜杠", "\\<invoke name=\"Read\">"),
            // 连发 burst：B 的开标签紧跟 A 的闭标签 `>`。这正是当初被引用判定漏掉、
            // 逼出一个重复函数绕行的形态。
            ("紧跟前一个闭标签", "</invoke><invoke name=\"Read\">"),
            ("句中", "text <invoke name=\"Read\">"),
            ("带命名空间前缀", "<invoke name=\"Read\">"),
        ] {
            assert!(
                find_invoke_start(input).is_some(),
                "[{name}] invoke 开标签必须被识别: {input:?}"
            );
        }

        // 结构不成立的仍不认，避免误匹配 invoked / invoker 之类。
        assert!(find_invoke_start("invoked the tool").is_none());
        assert!(find_invoke_start("the invoker said").is_none());
    }

    #[test]
    fn quote_adjacent_tags_are_not_hidden_by_single_side_check() {
        // 回归：原实现「任一侧命中引号字符就跳过」，导致模型 thinking 里引代码时标签漏检。
        // 实测 9 种引号相邻形态漏 7 种。判定改为成对包裹。

        // 开标签后紧跟引号类字符——模型 thinking 第一句直接引代码/命令/文件名。
        for input in [
            "<thinking>`cargo test` 会失败",
            "<thinking>\"foo\" 这个值",
            "<thinking>'x' 变量",
            "<thinking>\\n 转义",
        ] {
            assert!(
                find_real_thinking_start_tag(input).is_some(),
                "开标签后紧跟引号不得漏检: {input:?}"
            );
        }

        // 闭标签前后紧跟引号类字符——thinking 末尾刚引完代码，或正文以代码开头。
        for input in [
            "见 `code`</thinking>\nDone",
            "值是 \"x\"</thinking>\nDone",
            "reasoning</thinking>`next`",
        ] {
            assert!(
                find_real_thinking_end_tag(input).is_some(),
                "闭标签紧邻引号不得漏检: {input:?}"
            );
        }

        // 成对包裹仍须跳过：这才是「模型在讨论这个标签」。
        assert!(find_real_thinking_start_tag("about `<thinking>` tag").is_none());
        assert!(find_real_thinking_start_tag("the \"<thinking>\" marker").is_none());
        assert!(find_real_thinking_end_tag("about `</thinking>` tag\n\n").is_none());
        assert!(find_real_thinking_end_tag("the \"</thinking>\" one\n\n").is_none());

        // 不同引号字符不构成成对，不算引用。
        assert!(find_real_thinking_start_tag("`<thinking>\" mixed").is_some());
    }

    #[test]
    fn close_tag_followed_by_text_closes_block_and_keeps_text() {
        // 线上实测形态：`Starting now.<thinking>\n...\n</thinking>\nDone.`
        // 闭标签后只有单换行加正文。旧实现要求后跟 `\n\n`，于是闭标签字面泄漏进 thinking
        // 内容（实测 thinking 里出现独立一行 `</thinking>`），块也不能及时闭合。
        for (name, chunk, want_text) in [
            (
                "单换行接正文",
                "Starting now.<thinking>\nreasoning\n</thinking>\nDone.",
                "Done.",
            ),
            (
                "无换行接正文",
                "Starting now.<thinking>\nreasoning\n</thinking>Done.",
                "Done.",
            ),
            (
                "空格接正文",
                "Starting now.<thinking>\nreasoning\n</thinking> Done.",
                " Done.",
            ),
            (
                "双换行接正文",
                "Starting now.<thinking>\nreasoning\n</thinking>\n\nDone.",
                "Done.",
            ),
        ] {
            let mut ctx = StreamContext::new_with_thinking(
                "test-model",
                1,
                true,
                HashMap::new(),
                test_known_tools(),
            );
            let _ = ctx.generate_initial_events();
            let mut events = ctx.process_assistant_response(chunk);
            events.extend(ctx.generate_final_events());

            let thinking = collect_thinking_content(&events);
            let text = collect_text_content(&events);

            assert!(
                thinking.contains("reasoning"),
                "[{name}] 推理内容须进 thinking，实际 {thinking:?}"
            );
            assert!(
                !thinking.contains("</thinking>"),
                "[{name}] 闭标签不得泄漏进 thinking，实际 {thinking:?}"
            );
            assert!(
                !text.contains("<thinking>") && !text.contains("</thinking>"),
                "[{name}] 正文不得出现裸标签，实际 {text:?}"
            );
            assert!(
                text.contains("Starting now."),
                "[{name}] 开标签前的正文须保留，实际 {text:?}"
            );
            // 剥离长度按实际后缀计算，不得吃掉正文开头的字符。
            assert!(
                text.contains(want_text.trim_start()),
                "[{name}] 闭标签后的正文须完整保留（期望含 {:?}），实际 {text:?}",
                want_text.trim_start()
            );
        }
    }

    #[test]
    fn punctuation_before_open_tag_does_not_hide_it() {
        // 回归：原 QUOTE_CHARS 含 `.` `,` `;` `:` `-` `)` `/` `>` 等标点，模型写完一句话
        // 紧接着开标签时前一个字符命中标点，整段被跳过——开标签漏检，thinking 连同字面
        // 标签一起泄漏进正文。实测 11 种形态漏检 7 种。
        for input in [
            "done.<thinking>reasoning",
            "note:<thinking>reasoning",
            "ok,<thinking>reasoning",
            "ok;<thinking>reasoning",
            "(x)<thinking>reasoning",
            "a-<thinking>reasoning",
            "path/<thinking>reasoning",
            "quoted>%<thinking>reasoning",
            "回答你后面几个问题。<thinking>autoflow",
            "answer\n<thinking>reasoning",
            "answer <thinking>reasoning",
        ] {
            assert!(
                find_real_thinking_start_tag(input).is_some(),
                "开标签必须被识别: {input:?}"
            );
        }

        // 标签**后面**紧跟标点也不得漏检（原 has_quote_after 的锅）。
        assert!(find_real_thinking_start_tag("answer\n<thinking>. reasoning").is_some());
        assert!(find_real_thinking_start_tag("answer\n<thinking>- item").is_some());

        // 真正的引用仍须跳过：模型在讨论这个标签。
        for input in [
            "talk about `<thinking>` tag",
            "the \"<thinking>\" marker",
            "it is '<thinking>' here",
        ] {
            assert!(
                find_real_thinking_start_tag(input).is_none(),
                "被引用包裹的标签不得当成开标签: {input:?}"
            );
        }
    }

    #[test]
    fn punctuation_around_close_tag_does_not_hide_it() {
        // 闭标签用同一张表，同样受影响。
        assert!(find_real_thinking_end_tag("reasoning.</thinking>\n\nanswer").is_some());
        assert!(find_real_thinking_end_tag("reasoning:</thinking>\n\nanswer").is_some());
        assert!(find_real_thinking_end_tag("reasoning)</thinking>\n\nanswer").is_some());
        // 引用形态仍跳过。
        assert!(find_real_thinking_end_tag("about `</thinking>` tag\n\nmore").is_none());
    }

    #[test]
    fn thinking_source_locks_to_first_channel() {
        // 单元级：对齐 Kiro-Go 的 TestThinkingSourceReasoningFirst / TagFirst /
        // SameSourceRemainsAllowed 三个用例。
        let mut source = ThinkingSource::default();
        assert!(source.allow_reasoning());
        assert_eq!(source, ThinkingSource::ReasoningEvent);
        assert!(!source.allow_tag(), "原生先到后标签必须被拒");

        let mut source = ThinkingSource::default();
        assert!(source.allow_tag());
        assert_eq!(source, ThinkingSource::TagBlock);
        assert!(!source.allow_reasoning(), "标签先到后原生必须被拒");

        // 同源可反复通过。
        let mut source = ThinkingSource::default();
        assert!(source.allow_tag());
        assert!(source.allow_tag());
        let mut source = ThinkingSource::default();
        assert!(source.allow_reasoning());
        assert!(source.allow_reasoning());
    }

    #[test]
    fn native_reasoning_wins_and_tag_block_is_dropped_without_leaking() {
        // 上游同时下发原生 reasoning 事件和正文里的字面 `<thinking>` 标签时，
        // 只认先到的原生通道；标签块内容丢弃，且**不得泄漏成正文**。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut events = ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("native reasoning".into()),
                signature: None,
                redacted_content: None,
            },
        ));
        events.extend(ctx.process_assistant_response(
            "<thinking>\nduplicate via tag\n</thinking>\n\nvisible answer",
        ));
        events.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&events);
        let text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();

        assert!(thinking.contains("native reasoning"));
        assert!(
            !thinking.contains("duplicate via tag"),
            "标签块内容不得再进 thinking，实际 thinking={thinking:?}"
        );
        assert!(
            !text.contains("duplicate via tag") && !text.contains("<thinking>"),
            "被丢弃的标签块不得泄漏成正文，实际 text={text:?}"
        );
        assert!(
            text.contains("visible answer"),
            "标签块之后的正文必须照常交付，实际 text={text:?}"
        );
    }

    #[test]
    fn tag_thinking_wins_and_later_native_reasoning_is_dropped() {
        // 反向：标签先占据通道后，原生 reasoning 事件必须被丢弃。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut events = ctx.process_assistant_response("<thinking>\nvia tag\n</thinking>\n\n");
        events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("duplicate via native".into()),
                signature: None,
                redacted_content: None,
            },
        )));
        events.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&events);
        assert!(thinking.contains("via tag"));
        assert!(
            !thinking.contains("duplicate via native"),
            "标签已占据通道后原生事件须丢弃，实际 thinking={thinking:?}"
        );
    }

    #[test]
    fn second_thinking_block_is_parsed_not_leaked_as_text() {
        // 一轮响应里 thinking 可以出现多次（想一下 → 调工具 → 看结果 → 再想一下）。
        // 回归：旧实现在首块闭合后永久锁死入口，第二个 `<thinking>` 会被当正文原样吐出，
        // 客户端看到裸标签 + 整段推理内容。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut events = ctx.process_assistant_response("<thinking>\nfirst pass\n</thinking>\n\n");
        events.extend(ctx.process_assistant_response("visible answer\n\n"));
        events.extend(ctx.process_assistant_response("<thinking>\nsecond pass\n</thinking>\n\n"));
        events.extend(ctx.process_assistant_response("final answer"));
        events.extend(ctx.generate_final_events());

        let thinking_text: String = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .filter_map(|e| e.data["delta"]["thinking"].as_str())
            .collect();
        let visible_text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();

        assert!(
            thinking_text.contains("second pass"),
            "第二个 thinking 块必须走 thinking_delta，实际 thinking={thinking_text:?}"
        );
        assert!(
            !visible_text.contains("<thinking>"),
            "正文不得出现裸 thinking 标签，实际 text={visible_text:?}"
        );
        assert!(
            !visible_text.contains("second pass"),
            "第二块的推理内容不得泄漏进正文，实际 text={visible_text:?}"
        );
        assert!(
            visible_text.contains("final answer"),
            "第二块之后的正文必须照常交付，实际 text={visible_text:?}"
        );

        // 两个 thinking 块应各自拿到独立的 block index，并各自闭合。
        let thinking_starts = events
            .iter()
            .filter(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "thinking"
            })
            .count();
        assert_eq!(thinking_starts, 2, "应产生两个独立的 thinking 块");
    }

    #[test]
    fn unterminated_thinking_becomes_thinking_not_leaked_text() {
        // 模型在思考途中被 max_tokens 截断，没吐出 `</thinking>`。
        // 回归：旧实现原样返回整段文本，字面标签和推理内容一起泄漏进正文。
        let (thinking, text) =
            extract_thinking_from_complete_text("<thinking>\ncut off mid thought");
        assert_eq!(thinking.as_deref(), Some("cut off mid thought"));
        assert_eq!(text, "", "未闭合时不得把标签或推理内容留在正文里");

        // 开标签前的正文仍须保留。
        let (thinking, text) = extract_thinking_from_complete_text("preamble\n<thinking>\ncut off");
        assert_eq!(thinking.as_deref(), Some("cut off"));
        assert!(text.starts_with("preamble"));
        assert!(!text.contains("<thinking>"));

        // 被反引号包裹的标签是在讨论它，不算开标签，正文原样保留。
        let (thinking, text) =
            extract_thinking_from_complete_text("talking about `<thinking>` tag");
        assert!(thinking.is_none());
        assert_eq!(text, "talking about `<thinking>` tag");
    }

    #[test]
    fn tiny_max_tokens_requests_get_no_thinking_prefix() {
        // 客户端辅助小请求（标题生成等）带着主会话的 thinking 配置但 max_tokens 极小，
        // 注入 thinking 会让上游在矛盾约束下什么都不产出，线上表现为 100% 空响应失败。
        use crate::anthropic::converter::convert_request;
        use crate::anthropic::types::{Message as AnthropicMessage, MessagesRequest, Thinking};

        let build = |max_tokens: i32| MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4.5".to_string(),
            max_tokens,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("give me a title"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: Some(Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 24_576,
            }),
            output_config: None,
            metadata: None,
        };

        let tiny = convert_request(&build(64)).unwrap();
        let wire = serde_json::to_string(&tiny.conversation_state).unwrap();
        assert!(
            !wire.contains("thinking_mode"),
            "max_tokens=64 不得注入 thinking 前缀"
        );

        let normal = convert_request(&build(64_000)).unwrap();
        let wire = serde_json::to_string(&normal.conversation_state).unwrap();
        assert!(
            wire.contains("thinking_mode"),
            "正常请求仍须注入 thinking 前缀"
        );
    }

    #[test]
    fn short_text_after_thinking_is_not_held_hostage_by_tag_sniffing() {
        // 回归：首块之后若仍无条件扣住 10 字节（`<thinking>` 长度），比它短的正文
        // 会被永久扣在缓冲区里等一个不会到来的标签，直到收尾才吐出。
        assert_eq!(partial_open_tag_suffix_len("你好"), 0);
        assert_eq!(partial_open_tag_suffix_len("abc<thin"), 5);
        assert_eq!(partial_open_tag_suffix_len("abc<"), 1);
        // 完整标签不算「半截」，交给 find_real_thinking_start_tag 处理。
        assert_eq!(partial_open_tag_suffix_len("<thinking>"), 0);
        // 多字节字符不得 panic，也不得误判。
        assert_eq!(partial_open_tag_suffix_len("内容中"), 0);

        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");
        let text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert_eq!(text, "你好", "首块之后的短正文须立即交付");
    }

    #[test]
    fn text_mentioning_thinking_tag_after_first_block_is_not_reparsed() {
        // 首块之后放开了入口，但被引用字符包裹的标签仍必须当普通正文处理，
        // 否则模型讨论这个标签时会被误判为进入 thinking 块。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut events = ctx.process_assistant_response("<thinking>\nreal\n</thinking>\n\n");
        events.extend(ctx.process_assistant_response("talking about `<thinking>` as a tag here"));
        events.extend(ctx.generate_final_events());

        let visible_text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(
            visible_text.contains("as a tag here"),
            "被反引号包裹的标签后的文本必须留在正文，实际 text={visible_text:?}"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true, // 累积器仅在 stop=true 时整体发出工具调用（含关闭前一个块）
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn generate_final_events_salvaged_tool_after_thinking_is_tool_use_not_max_tokens() {
        // 回归 Bug 1：thinking 模式下"纯思考 → 无参工具(EnterPlanMode)缓冲 → 上游未发 stop=true 即断流"。
        // 收尾时 max_tokens 启发式不得抢在打捞前把 stop_reason 盖成 max_tokens、也不得多吐空格 text 块；
        // 打捞出的 tool_use 应正常发出，stop_reason 应为 tool_use。
        use crate::kiro::model::events::{ReasoningContentEvent, ToolUseEvent};

        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true, // thinking enabled
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        // 1) 原生 thinking：开出并写入 thinking 块（随后被 tool_use 分片关闭）。
        let _ = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("let me think about the plan".to_string()),
            signature: Some("sig".to_string()),
            redacted_content: None,
        }));

        // 2) 无参工具 EnterPlanMode，stop=false → 仅缓冲、不发 tool_use 块，但记录上游工具信号。
        let mid = ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "EnterPlanMode".to_string(),
            tool_use_id: "toolu_plan".to_string(),
            input: String::new(),
            stop: false,
        }));
        assert!(
            mid.iter().all(|e| {
                e.event != "content_block_start" || e.data["content_block"]["type"] != "tool_use"
            }),
            "stop=false 分片不应发出 tool_use 块（仅缓冲）"
        );

        // 3) 收尾。
        let final_events = ctx.generate_final_events();

        // tool_use 块应被打捞发出，name=EnterPlanMode。
        let tool_start = final_events.iter().find(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            tool_start.is_some(),
            "应打捞发出 EnterPlanMode 的 tool_use 块"
        );
        assert_eq!(
            tool_start.unwrap().data["content_block"]["name"],
            "EnterPlanMode"
        );

        // stop_reason 必须是 tool_use，而不是 max_tokens。
        let delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(
            delta.data["delta"]["stop_reason"], "tool_use",
            "有 tool_use 时 stop_reason 应为 tool_use，不得被 max_tokens 启发式抢跑"
        );

        // 不得为"纯思考轮"补发空格 text 块。
        let has_space_text = final_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "text_delta"
                && e.data["delta"]["text"] == " "
        });
        assert!(!has_space_text, "不应补发空格 text 块");

        // 顺序健壮性：若存在 thinking 的 content_block_stop，应在 tool_use 的 content_block_start 之前。
        let pos_tool_start = final_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        if let (Some(tool_pos), Some(think_idx)) = (pos_tool_start, ctx.thinking_block_index) {
            if let Some(think_stop_pos) = final_events.iter().position(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(think_idx as i64)
            }) {
                assert!(
                    think_stop_pos < tool_pos,
                    "thinking 块应在 tool_use 块之前关闭"
                );
            }
        }
    }

    #[test]
    fn generate_final_events_thinking_only_still_reports_max_tokens() {
        // 反向回归：真·纯思考轮（无任何 tool_use）仍应触发 max_tokens 补空格逻辑，
        // 确保 Bug 1 的护栏没有误伤原有行为。
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let _ = ctx.process_kiro_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("thinking only, no tools".to_string()),
            signature: Some("sig".to_string()),
            redacted_content: None,
        }));

        let final_events = ctx.generate_final_events();
        let delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(
            delta.data["delta"]["stop_reason"], "max_tokens",
            "纯思考轮 stop_reason 仍应为 max_tokens"
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(find_real_thinking_start_tag("<thinking>"), Some(0));
        assert_eq!(find_real_thinking_start_tag("prefix<thinking>"), Some(6));
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            find_real_thinking_start_tag("about `<thinking>` tag<thinking>content"),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            find_real_thinking_start_tag("about \"<thinking>\" and '<thinking>' then<thinking>"),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(find_real_thinking_end_tag("</thinking>\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("content</thinking>\n\n"),
            Some(7)
        );
        assert_eq!(
            find_real_thinking_end_tag("some text</thinking>\n\nmore text"),
            Some(9)
        );

        // 流式守卫：标签后不足 2 字节时等下一个 chunk——要凑够上下文才能判断是否被引号
        // 包裹。真正的流末尾由 find_real_thinking_end_tag_at_buffer_end 兜住。
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);

        // 不再要求后跟 `\n\n`：单换行、无换行、空格接正文都必须识别为闭标签。
        // 原先这三种全漏检，闭标签会字面泄漏进 thinking 内容且块不能及时闭合。
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), Some(0));
        assert_eq!(find_real_thinking_end_tag("</thinking>\nDone."), Some(0));
        assert_eq!(find_real_thinking_end_tag("</thinking>Done."), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("reasoning</thinking>\nDone."),
            Some(9)
        );

        // 被引号包裹仍不算闭合：模型在讨论这个标签。
        assert_eq!(find_real_thinking_end_tag("about `</thinking>` tag"), None);
    }

    /// 复读退化回归：上游连吐多个 `</thinking>` 时，末尾整串都要被识别成结束标签，
    /// 否则前面几个会作为字面文本泄漏给客户端（线上 `unit_bytes=22` 即两个标签）。
    #[test]
    fn trailing_end_tag_run_collapses_repeated_tags() {
        // 连续两个 / 三个：都应指向**第一个**标签的位置
        assert_eq!(
            trailing_end_tag_run_start("内容</thinking></thinking>"),
            Some(6)
        );
        assert_eq!(
            trailing_end_tag_run_start("内容</thinking></thinking></thinking>"),
            Some(6)
        );
        // 标签之间夹空白也算同一串
        assert_eq!(
            trailing_end_tag_run_start("内容</thinking>\n\n</thinking>"),
            Some(6)
        );
        // 串后面允许尾随空白
        assert_eq!(
            trailing_end_tag_run_start("内容</thinking></thinking>\n\n"),
            Some(6)
        );
        // 单个标签：与原行为一致（这是保证「纯增量」的关键）
        assert_eq!(trailing_end_tag_run_start("内容</thinking>"), Some(6));
        // 标签后有真实内容 → 不是末尾串
        assert_eq!(trailing_end_tag_run_start("内容</thinking>后面还有"), None);
        // 完全没有标签
        assert_eq!(trailing_end_tag_run_start("纯文本"), None);
        // 被反引号包裹（模型在讨论这个标签）→ 不认
        assert_eq!(trailing_end_tag_run_start("提到 `</thinking>`"), None);
    }

    /// 量化 thinking 标签串检测在**流式热路径**上的开销。
    ///
    /// `trailing_end_tag_run_start` 跑在每个 chunk 的 else 分支上，是全项目最高频的新增
    /// 调用点，必须确认它不是 O(buffer)。纯度量、不断言耗时。
    #[test]
    fn measure_thinking_hot_path_overhead() {
        // 模拟一段长 thinking：2000 个 chunk，累计约 100 KiB
        let chunk = "这是一段思考内容，用来模拟真实的流式分片输出。".repeat(2);
        let rounds = 2_000;

        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_assistant_response("<thinking>");

        let started = std::time::Instant::now();
        let mut total_bytes = 0usize;
        for _ in 0..rounds {
            total_bytes += chunk.len();
            let _ = ctx.process_assistant_response(&chunk);
        }
        let elapsed = started.elapsed();

        println!(
            "thinking 热路径：{} 个 chunk / {} KiB，总耗时 {:?}，单 chunk 均摊 {:?}",
            rounds,
            total_bytes / 1024,
            elapsed,
            elapsed / rounds
        );
    }

    /// UTF-8 边界压测：thinking 正文是任意 Unicode，回剥时按字节切片极易切进多字节字符
    /// 中间导致 panic（开发中真实踩到过）。这里穷举各种多字节前缀 × 标签串组合，
    /// 只要求**不 panic**，把崩溃类回归钉死。
    #[test]
    fn trailing_end_tag_run_never_panics_on_multibyte_boundaries() {
        let prefixes = [
            "",
            "内容",
            "后",
            "🙂",
            "a内",
            "日本語テキスト",
            "emoji🙂混排",
            "①②③",
        ];
        let suffixes = [
            "",
            "</thinking>",
            "</thinking></thinking>",
            "</thinking>\n\n</thinking>  ",
            "</thinking>后面",
            "`</thinking>`",
            "</think",
            "thinking>",
        ];
        for prefix in prefixes {
            for suffix in suffixes {
                let buffer = format!("{prefix}{suffix}");
                // 两个函数都要在任意输入下安全返回
                let run = trailing_end_tag_run_start(&buffer);
                if let Some(start) = run {
                    assert!(
                        buffer.is_char_boundary(start),
                        "返回位置必须是字符边界: {buffer:?} -> {start}"
                    );
                    let skip = end_tag_skip_len(&buffer, start);
                    assert!(
                        buffer.is_char_boundary(start + skip),
                        "跳过后的位置必须是字符边界: {buffer:?}"
                    );
                }
                let _ = find_real_thinking_end_tag_at_buffer_end(&buffer);
                let _ = find_real_thinking_end_tag(&buffer);
            }
        }
    }

    /// `find_real_thinking_end_tag_at_buffer_end` 对连续标签必须返回**串首**，
    /// 这样 `buffer[..end_pos]` 里才不会夹带字面标签。
    #[test]
    fn buffer_end_lookup_returns_run_start_for_repeated_tags() {
        let buffer = "思考内容</thinking></thinking>";
        let end_pos = find_real_thinking_end_tag_at_buffer_end(buffer).expect("应识别为结束标签");
        assert_eq!(
            &buffer[..end_pos],
            "思考内容",
            "thinking 正文不得夹带字面标签"
        );
        // 跳过整串后应无残留
        let after = end_pos + end_tag_skip_len(buffer, end_pos);
        assert_eq!(
            buffer[after..].trim(),
            "",
            "整串标签都应被跳过，不得作为文本泄漏"
        );
    }

    /// 单标签场景下新旧路径结果必须完全一致——回归「不引发其他 bug」这条约束。
    #[test]
    fn buffer_end_lookup_unchanged_for_single_tag() {
        for buffer in [
            "内容</thinking>",
            "内容</thinking>   ",
            "内容</thinking>\n\n",
            "</thinking>",
        ] {
            let end_pos = find_real_thinking_end_tag_at_buffer_end(buffer).expect("单标签应被识别");
            let after = end_pos + end_tag_skip_len(buffer, end_pos);
            assert_eq!(buffer[after..].trim(), "", "剥离后不应有残留: {buffer:?}");
            assert!(
                !buffer[..end_pos].contains("</thinking>"),
                "正文不得含字面标签: {buffer:?}"
            );
        }
    }

    /// 端到端：流式吐出带重复结束标签的 thinking，客户端不应看到任何字面 `</thinking>`，
    /// 且 thinking 正文要完整保留。
    #[test]
    fn stream_does_not_leak_literal_end_tags_on_repetition() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response(
            "<thinking>我在更新失败提示。</thinking></thinking></thinking>",
        ));
        events.extend(ctx.generate_final_events());

        let thinking: String = events
            .iter()
            .filter(|e| e.data["delta"]["type"] == "thinking_delta")
            .filter_map(|e| e.data["delta"]["thinking"].as_str())
            .collect();
        let text: String = events
            .iter()
            .filter(|e| e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();

        assert!(
            !thinking.contains("</thinking>"),
            "thinking 正文泄漏了字面标签: {thinking:?}"
        );
        assert!(
            !text.contains("</thinking>"),
            "正文泄漏了字面标签: {text:?}"
        );
        assert!(
            thinking.contains("我在更新失败提示。"),
            "thinking 内容丢失: {thinking:?}"
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 单侧引号**不再**跳过：改为成对判定。
        //
        // 真实推理里单侧反引号更可能是真闭合——模型 thinking 末尾用行内代码收尾
        // （`见 \`code\`</thinking>`），前一字符正是反引号；标签后紧跟反引号同理，
        // 是正文以行内代码开头。按旧的单侧规则这些都会被跳过，闭标签泄漏进 thinking。
        // 「在讨论这个标签」的真实写法是两侧成对，已由上面的用例覆盖。
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), Some(1));
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("reasoning `code`</thinking>\nDone."),
            Some(16)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about \"</thinking>\" tag</thinking>\n\n"),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about '</thinking>' tag</thinking>\n\n"),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            find_real_thinking_end_tag("discussing `</thinking>` tag</thinking>\n\n"),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            find_real_thinking_end_tag("`</thinking>` and `</thinking>` done</thinking>\n\n"),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            ),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true, // 累积器仅在 stop=true 时整体发出工具调用（含关闭前一个块）
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_thinking_block_emits_signature_delta_before_stop() {
        // 客户端在 thinking 模式下要求 thinking 块带 signature 字段，否则下一轮回传时
        // 会抛出 "must be passed back to the API"。本测试验证 thinking 块结束前发送了
        // 一个非空的 signature_delta 事件。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>abc</thinking>\n\nhello"));
        all.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");

        let pos_sig = all.iter().position(|e| {
            e.event == "content_block_delta"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
        });
        let pos_stop = all.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });

        assert!(pos_sig.is_some(), "signature_delta should be emitted");
        assert!(pos_stop.is_some(), "content_block_stop should be emitted");
        assert!(
            pos_sig.unwrap() < pos_stop.unwrap(),
            "signature_delta must precede content_block_stop"
        );
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有合成的 tool_use 调用
    ///
    /// 抓 `content_block_start` 里 `content_block.type == "tool_use"` 的 name，
    /// 再配对同 index 的 `input_json_delta.partial_json`，返回 (name, input_json)。
    fn collect_tool_uses(events: &[SseEvent]) -> Vec<(String, String)> {
        let mut result = Vec::new();
        for e in events.iter() {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use" {
                let index = e.data["index"].as_i64();
                let name = e.data["content_block"]["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                // 找同 index 的 input_json_delta
                let input = events
                    .iter()
                    .find(|d| {
                        d.event == "content_block_delta"
                            && d.data["index"].as_i64() == index
                            && d.data["delta"]["type"] == "input_json_delta"
                    })
                    .and_then(|d| d.data["delta"]["partial_json"].as_str())
                    .unwrap_or("")
                    .to_string();
                result.push((name, input));
            }
        }
        result
    }

    #[test]
    fn test_invoke_sniff_backtick_wrapped_is_not_captured() {
        // 🔴 防误伤：被反引号包裹的 <invoke> 是引用，不应被抓
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("示例：`<invoke name=\"x\">` 这种写法"));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "被反引号包裹的不应被抓: {:?}", tools);

        let text = collect_text_content(&all);
        assert!(
            text.contains("<invoke name=\"x\">"),
            "原文应原样保留在 text 中: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_sniff_single_bare_invoke() {
        // 🟢 单个裸 invoke（无外壳）
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "exec_command", "name 应为 exec_command");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(parsed["cmd"], "ls", "input 应含 cmd=ls");
    }

    #[test]
    fn test_invoke_sniff_param_value_with_lt_multiline_chinese() {
        // 🟢 参数值含 `<`、多行、中文 → 不被截断
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let value = "第一行 a < b\n第二行 路径 /tmp/中文";
        let chunk = format!(
            "<invoke name=\"write_file\"><parameter name=\"content\">{}</parameter></invoke>",
            value
        );
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(&chunk));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(
            parsed["content"], value,
            "参数值应完整保留（含 < / 多行 / 中文）"
        );
    }

    #[test]
    fn test_invoke_sniff_two_invokes_sequential() {
        // 🟢 2 个 invoke 串联 → 2 个 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "<invoke name=\"tool_a\"><parameter name=\"x\">1</parameter></invoke><invoke name=\"tool_b\"><parameter name=\"y\">2</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 2, "应合成 2 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "tool_a");
        assert_eq!(tools[1].0, "tool_b");
    }

    #[test]
    fn test_invoke_sniff_split_across_chunks() {
        // 🟢 跨 chunk 分片：标签被切碎多次喂入
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<inv"));
        all.extend(ctx.process_assistant_response("oke name=\"exec_command\">"));
        all.extend(ctx.process_assistant_response("<parameter name=\"cmd\">ls</parameter></in"));
        all.extend(ctx.process_assistant_response("voke>"));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "跨 chunk 应合成 1 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "exec_command");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(parsed["cmd"], "ls");
    }

    #[test]
    fn stream_recovers_fragmented_get_weather_invoke_as_tool_use() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let mut context =
            StreamContext::new_with_thinking("claude-opus-4-6", 1, false, HashMap::new(), known);
        let mut events = context.generate_initial_events();
        events.extend(context.process_assistant_response("call\n<invoke name=\"get_"));
        events.extend(context.process_assistant_response(
            "weather\"><parameter name=\"location\">Paris</parameter></invoke>",
        ));
        events.extend(context.generate_final_events());

        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
                && event.data["content_block"]["name"] == "get_weather"
        }));
        let delta_index = events
            .iter()
            .position(|event| event.event == "message_delta")
            .unwrap();
        let delta = &events[delta_index];
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");

        let tool_start_index = events
            .iter()
            .position(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
            })
            .unwrap();
        let tool_stop_index = events
            .iter()
            .rposition(|event| event.event == "content_block_stop")
            .unwrap();
        assert!(tool_start_index < tool_stop_index);
        assert!(tool_stop_index < delta_index);
    }

    #[test]
    fn test_invoke_sniff_strips_stray_call_token() {
        // 🟢 stray token：<invoke> 前有单独一行 `call` → 剥掉，text 不含残留 call
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "call\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);

        let text = collect_text_content(&all);
        assert!(
            !text.contains("call"),
            "前置的 stray `call` 应被剥掉，text 不应残留: {:?}",
            text
        );
    }

    #[test]
    fn strip_trailing_stray_preserves_preceding_newline() {
        // 回归：narrative 文本后跟一行 stray token（`some text\ncall`）。
        // 旧实现把 stray 行连同其【前面的换行】一起剥掉 -> 得到 "some text"（无换行结尾），
        // 这会让随后的 invoke_looks_like_real_leak 行首启发式失败、漏捞真泄漏。
        // 正确：只剥 stray 行本身，保留前一行的换行 -> "some text\n"。
        let got = strip_trailing_stray_tokens("some text\ncall");
        assert_eq!(
            got, "some text\n",
            "must keep the newline terminating the narrative line so the invoke stays line-start"
        );
        // 且剥完的结果应让行首判定通过
        assert!(
            invoke_looks_like_real_leak(got),
            "stripped narrative must still look like a line-start leak (ends with newline)"
        );
    }

    #[test]
    fn test_invoke_sniff_reclaims_after_narrative_then_stray_token() {
        // 端到端：`正文\ncall\n<invoke...>` —— 正文 + stray token + 真泄漏 invoke。
        // 旧实现漏捞（stray 剥过头把正文和 invoke 挤一行），修后应成功捞回 tool_use。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "先看看结果。\ncall\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "narrative+stray+invoke 应捞回 1 个 tool_use: {:?}",
            tools
        );
        let text = collect_text_content(&all);
        assert!(text.contains("先看看结果"), "叙述正文应保留: {:?}", text);
        assert!(
            !text.contains("call\n<invoke") && !text.contains("<invoke"),
            "invoke 不应泄漏为文本: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_sniff_keeps_narrative_before_invoke() {
        // 🟢 invoke 前有叙述：text 含"先看看"，1 个 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "先看看\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);

        let text = collect_text_content(&all);
        assert!(
            text.contains("先看看"),
            "叙述文本应保留在 text 中: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_sniff_truncated_block_not_captured() {
        // 🔴 截断半块（无 </invoke> 闭合）→ 0 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "<invoke name=\"exec_command\"><parameter name=\"cmd\">ls",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "未闭合的块不应被抓: {:?}", tools);
    }

    #[test]
    fn test_invoke_midsentence_not_captured() {
        // 🔴 P1：正文里嵌在句子中间（无反引号、非行首）的 <invoke> 是讨论文本，不应被抓
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "解析器示意：模型吐出 <invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke> 这种文本",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "句中讨论的 <invoke> 不应被抓: {:?}",
            tools
        );

        let text = collect_text_content(&all);
        assert!(
            text.contains("解析器示意") && text.contains("这种文本"),
            "正文应完整保留（含前后叙述）: {:?}",
            text
        );
        assert!(
            text.contains("<invoke name=\"exec_command\">"),
            "原 <invoke> 文本应原样保留在 text 中: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_midsentence_unclosed_not_hold() {
        // 🔴 P2：流式中途遇到句中不闭合的 <invoke，不应 hold 住后续文本到流末尾
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        // 第一次 process：句中不闭合的 <invoke>，前面同一行有正文“讨论”
        let first = ctx.process_assistant_response("讨论 <invoke name=\"x\"> 语义，");
        let first_text = collect_text_content(&first);
        assert!(
            first_text.contains("讨论"),
            "句中不闭合的 <invoke 不应 hold 住正文，应及时吐出“讨论”: {:?}",
            first_text
        );

        let mut all = first;
        all.extend(ctx.process_assistant_response("后面内容。"));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "不闭合的句中 <invoke 不应被抓: {:?}",
            tools
        );

        let text = collect_text_content(&all);
        assert!(
            text.contains("讨论") && text.contains("语义") && text.contains("后面内容。"),
            "全部正文应完整保留: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_multiline_patch_split_still_captured() {
        // 🟢 P3：行首合法 invoke，参数值是 20+ 行多行文本（模拟 apply_patch），
        // 逐行流式喂入。修复前换行数 ≥16 会被 too_long 误杀降级成文本；修复后应抓到。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        // 构造一个 24 行的多行 patch 内容
        let mut patch_lines = Vec::new();
        for i in 0..24 {
            patch_lines.push(format!("+ line number {i} of the patch body"));
        }
        let patch_value = patch_lines.join("\n");

        // 整块拼好后，按行切片逐片喂入（每片末尾补回换行，最后一行不补）
        let full = format!(
            "<invoke name=\"apply_patch\"><parameter name=\"input\">{}</parameter></invoke>",
            patch_value
        );
        let mut all = Vec::new();
        // 按换行拆成片，逐片喂；保证 invoke 在每片到齐前换行数早已 ≥16
        let bytes = full.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            // 找到下一个换行边界（含换行）作为一片
            let mut end = idx;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            if end < bytes.len() {
                end += 1; // 把换行也带上
            }
            let piece = std::str::from_utf8(&bytes[idx..end]).unwrap();
            all.extend(ctx.process_assistant_response(piece));
            idx = end;
        }
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "分片喂入的多行 invoke 应抓到 1 个 tool_use: {:?}",
            tools
        );
        assert_eq!(tools[0].0, "apply_patch", "name 应为 apply_patch");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(
            parsed["input"], patch_value,
            "多行参数值应完整保留（换行不丢）"
        );
    }

    #[test]
    fn test_invoke_large_patch_split_captured() {
        // 🟢 P3：参数值 ~17KB 多行，分片喂入，断言抓到 1 个 tool_use（在 256KB 上限之下）。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        // 每行 ~70 字节 × 250 行 ≈ 17KB
        let mut lines = Vec::new();
        for i in 0..250 {
            lines.push(format!(
                "+ patch content row {i:04} padding xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
            ));
        }
        let big_value = lines.join("\n");
        assert!(
            big_value.len() > 16 * 1024,
            "测试数据应 >16KB，实际 {}",
            big_value.len()
        );

        let full = format!(
            "<invoke name=\"apply_patch\"><parameter name=\"input\">{}</parameter></invoke>",
            big_value
        );
        // 固定 512 字节一片喂入（注意 UTF-8 边界，这里内容是 ASCII 安全）
        let mut all = Vec::new();
        let bytes = full.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            let end = (idx + 512).min(bytes.len());
            let piece = std::str::from_utf8(&bytes[idx..end]).unwrap();
            all.extend(ctx.process_assistant_response(piece));
            idx = end;
        }
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "~17KB 分片喂入的 invoke 应抓到 1 个 tool_use: {:?}",
            tools.iter().map(|t| &t.0).collect::<Vec<_>>()
        );
        assert_eq!(tools[0].0, "apply_patch");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(parsed["input"], big_value, "大 patch 参数值应完整保留");
    }

    #[test]
    fn test_unclosed_invoke_eventually_flushed_as_text() {
        // 🟢 锁定字节兜底仍在：行首 `<invoke>` 永不闭合、喂入超过 MAX_INVOKE_HOLD_BYTES，
        // 应被当文本吐出（不无限 hold）。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        // 行首开标签，永不闭合；填充超过上限的纯文本（无 </invoke>）
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<invoke name=\"x\">"));
        // 一次喂入超过上限的内容（用不含 `<` 的填充，避免触发其它路径）
        let filler = "A".repeat(StreamContext::MAX_INVOKE_HOLD_BYTES + 1024);
        all.extend(ctx.process_assistant_response(&filler));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "永不闭合的 invoke 不应被抓: {:?}",
            tools.len()
        );

        let text = collect_text_content(&all);
        assert!(
            text.contains("<invoke name=\"x\">"),
            "超上限的未闭合块应被当文本吐出（含开标签）"
        );
        assert!(
            text.contains(&"A".repeat(100)),
            "填充文本应被吐出，不应无限 hold"
        );
    }

    #[test]
    fn test_invoke_in_markdown_list_not_captured() {
        // 🔴 markdown 列表项 `- <invoke>` 当讨论文本，不抓。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "- <invoke name=\"exec_command\"><parameter name=\"cmd\">rm -rf /</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "markdown 列表里的 <invoke> 不应被抓: {:?}",
            tools
        );
        let text = collect_text_content(&all);
        assert!(
            text.contains("rm -rf /"),
            "危险命令应留在文本里、不被执行: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_in_blockquote_not_captured() {
        // 🔴 引用 `> <invoke>` 当讨论文本，不抓。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "> <invoke name=\"exec_command\"><parameter name=\"cmd\">rm -rf /</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "引用块里的 <invoke> 不应被抓: {:?}",
            tools
        );
        let text = collect_text_content(&all);
        assert!(
            text.contains("rm -rf /"),
            "危险命令应留在文本里、不被执行: {:?}",
            text
        );
    }

    fn block_start_position(events: &[SseEvent], block_type: &str) -> (usize, i64) {
        let pos = events
            .iter()
            .position(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == block_type
            })
            .unwrap_or_else(|| panic!("{block_type} block should start"));
        let idx = events[pos].data["index"]
            .as_i64()
            .unwrap_or_else(|| panic!("{block_type} block index should exist"));
        (pos, idx)
    }

    fn block_stop_position(events: &[SseEvent], index: i64) -> usize {
        events
            .iter()
            .position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(index)
            })
            .unwrap_or_else(|| panic!("block {index} should stop"))
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    // ===== 新增回归测试：P0-1 参数含字面 XML / 🅱 代码围栏 / 🅳 工具表 / 🅲 card =====

    /// 🅿️ P0-1：参数值里含字面 `</invoke>`，块不应被假闭合截断，input 要完整。
    #[test]
    fn test_invoke_param_value_contains_literal_invoke_close() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        // patch 正文里出现字面 </invoke>，真正的闭合在最后
        let payload = "count\n<invoke name=\"apply_patch\"><parameter name=\"input\">line1\n</invoke>\nstill in patch\nline3</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "apply_patch");
        let parsed: serde_json::Value = serde_json::from_str(&tools[0].1).expect("合法 JSON");
        let input = parsed["input"].as_str().expect("有 input");
        assert!(
            input.contains("still in patch"),
            "input 不应被假闭合截断: {input:?}"
        );
        assert!(input.contains("line3"), "input 应含 line3: {input:?}");
        let text = collect_text_content(&all);
        assert!(
            !text.contains("still in patch"),
            "patch 正文不应泄漏到 text: {text:?}"
        );
    }

    /// 🅿️ P0-1：参数值里含字面 `</parameter>`，值不应被截断丢失后半段。
    #[test]
    fn test_invoke_param_value_contains_literal_parameter_close() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"apply_patch\"><parameter name=\"input\">before</parameter> after the fake close</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        let parsed: serde_json::Value = serde_json::from_str(&tools[0].1).expect("合法 JSON");
        let input = parsed["input"].as_str().expect("有 input");
        assert!(
            input.contains("after the fake close"),
            "后半段不应丢: {input:?}"
        );
    }

    /// 🅱：代码围栏（```）内的 <invoke> 是正文展示，不应被捞回成 tool_use。
    #[test]
    fn test_invoke_inside_code_fence_not_captured() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload = "示例代码：\n```\n<invoke name=\"exec_command\"><parameter name=\"cmd\">rm -rf /</parameter></invoke>\n```\n讲解完毕。";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "围栏内展示文本不应被捞回: {:?}", tools);
        let text = collect_text_content(&all);
        assert!(
            text.contains("<invoke name=\"exec_command\">"),
            "应原样保留: {text:?}"
        );
    }

    /// 🅳：合成出的工具名不在已知工具表里 → 不捞回，当文本吐出（防误执行）。
    #[test]
    fn test_invoke_unknown_tool_name_not_synthesized() {
        // 已知工具表里没有 totally_unknown_tool
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"totally_unknown_tool\"><parameter name=\"x\">1</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "未知工具名不应被合成: {:?}", tools);
        let text = collect_text_content(&all);
        assert!(
            text.contains("totally_unknown_tool"),
            "未知工具应原样当文本: {text:?}"
        );
    }

    /// 🅳：已知工具表为空（请求没带 tools）→ 一律不捞回，宁可漏捞不可误执行。
    #[test]
    fn test_invoke_empty_known_tools_never_captured() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let _ = ctx.generate_initial_events();
        let payload =
            "count\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "工具表为空时不应捞回: {:?}", tools);
    }

    /// 🅲：stray token `card` 也应被剥掉，块仍被捞回。
    #[test]
    fn test_invoke_strips_stray_card_token() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload = "我先等结果。\n\ncard\n<invoke name=\"wait_agent\"><parameter name=\"x\">1</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "card 前缀的块应被捞回: {:?}", tools);
        assert_eq!(tools[0].0, "wait_agent");
        let text = collect_text_content(&all);
        assert!(
            !text.contains("card"),
            "card stray token 不应泄漏: {text:?}"
        );
        assert!(text.contains("我先等结果"), "正常叙述应保留: {text:?}");
    }

    /// 🅱 跨 chunk：``` 围栏开标签在 chunk 边界被切碎，仍能正确识别围栏内不捞回。
    #[test]
    fn test_invoke_fence_split_across_chunks() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut all = Vec::new();
        // 围栏开标签分两个 chunk 到达
        all.extend(ctx.process_assistant_response("看代码：\n``"));
        all.extend(ctx.process_assistant_response(
            "`\n<invoke name=\"exec_command\"><parameter name=\"cmd\">x</parameter></invoke>\n```",
        ));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "跨 chunk 围栏内不应捞回: {:?}", tools);
    }

    /// 🟡 回归（Reviewer 问题1）：连发 burst，块 A 在 `</invoke>` 前混了非 `>` 收尾文字，
    /// 不应把 A、B 误合并成一个块、也不应让 B 的参数串进 A。两个块都应独立捞回。
    #[test]
    fn test_invoke_burst_with_trailing_text_not_merged() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"tool_a\"><parameter name=\"x\">1</parameter>trailing plain</invoke><invoke name=\"tool_b\"><parameter name=\"y\">2</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            2,
            "应独立合成 2 个 tool_use，不能误合并: {:?}",
            tools
        );
        assert_eq!(tools[0].0, "tool_a");
        assert_eq!(tools[1].0, "tool_b");
        let a: serde_json::Value = serde_json::from_str(&tools[0].1).expect("合法 JSON");
        let b: serde_json::Value = serde_json::from_str(&tools[1].1).expect("合法 JSON");
        assert!(a.get("y").is_none(), "B 的参数 y 不应串进 A: {a:?}");
        assert_eq!(a["x"], "1");
        assert_eq!(b["y"], "2");
    }

    /// 🟢 正常连发 burst（块紧贴、A 以 </parameter> 收尾）仍应正确拆成两个。
    #[test]
    fn test_invoke_burst_clean_two_blocks() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"tool_a\"><parameter name=\"x\">1</parameter></invoke><invoke name=\"tool_b\"><parameter name=\"y\">2</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 2, "紧贴连发应拆成 2 个: {:?}", tools);
        assert_eq!(tools[0].0, "tool_a");
        assert_eq!(tools[1].0, "tool_b");
    }

    /// 🔁 回放验证：用问题 thread `019e9e8d` 里真实的 `count\n<invoke>` 泄漏原文，
    /// 断言新容错把它捞回成结构化 tool_use（而不是泄漏成字面 XML 文本）。
    /// 真实工具名 exec_command 在工具表里 → 应捞回；参数 cmd / yield_time_ms 完整。
    #[test]
    fn test_invoke_real_leak_sample_from_thread_019e9e8d() {
        let known: std::collections::HashSet<String> =
            ["exec_command", "update_plan", "update_goal"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), known);
        let _ = ctx.generate_initial_events();
        // 逐字摘自 thread 019e9e8d 真实泄漏 assistant 消息
        let real = "）。\n\ncount\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">cd /Users/yuyifeng/.codex/everything-codex/runtime/agent-tools && python3 -m pytest -q -p no:cacheprovider objects/dev/beads/leaves/create_issue/ 2>&1 | tail -8</parameter>\n<parameter name=\"yield_time_ms\">60000</parameter>\n</invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(real));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "真实泄漏样本应被捞回成 1 个 tool_use: {:?}",
            tools
        );
        assert_eq!(tools[0].0, "exec_command", "name 应为 exec_command");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert!(
            parsed["cmd"].as_str().unwrap_or("").contains("pytest"),
            "cmd 参数应完整保留: {:?}",
            parsed
        );
        assert_eq!(parsed["yield_time_ms"], "60000", "yield_time_ms 参数应保留");
        // 关键：字面 <invoke> 不应泄漏到 text
        let text = collect_text_content(&all);
        assert!(
            !text.contains("<invoke name=\"exec_command\">"),
            "字面 <invoke> 不应泄漏到文本: {:?}",
            text
        );
        // count stray token 也不应泄漏
        assert!(
            !text.contains("\ncount\n") && !text.ends_with("count"),
            "count stray token 不应泄漏: {:?}",
            text
        );
    }

    // ---- 复读熔断 (repeat guard)：root cause = Opus 长上下文退化复读 ----

    /// 🔴→🟢 复现真实泄漏：模型一句正常话后无限复读 `count`（thread 019ea4e9 的真账）。
    /// 熔断后吐出的 count 数必须远小于喂入的数量，且不撑满输出。
    #[test]
    fn repeat_guard_trips_on_count_flood() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();

        // 真实形态：正常话 + call + 海量 count（这里用 5000 次模拟 3.2 万次）
        let mut payload = String::from("先看 crawlee 状态。\n\ncall\n\n");
        for _ in 0..5000 {
            payload.push_str("count\n\n");
        }
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(&payload));
        all.extend(ctx.generate_final_events());

        let text = collect_text_content(&all);
        let emitted_counts = text.matches("count").count();
        assert!(
            emitted_counts < 64,
            "复读应被熔断：吐出的 count 数应远小于喂入的 5000，实际={}",
            emitted_counts
        );
        // 正常开头那句话必须保留（熔断不能误伤正文）
        assert!(
            text.contains("先看 crawlee 状态"),
            "熔断不应误伤正常正文: {:?}",
            &text[..text.len().min(80)]
        );
    }

    /// 🟢 不误伤：正常工具调用前的 1 个引导词 `count` + 真 <invoke> 仍被正常捞回。
    #[test]
    fn repeat_guard_does_not_trip_on_single_stray_token() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload =
            "count\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "单个引导词不应触发熔断，invoke 应正常捞回: {:?}",
            tools
        );
        assert_eq!(tools[0].0, "exec_command");
    }

    /// 🟢 不误伤：正常多行文本里偶尔出现 count 单词（非独占行复读）不熔断。
    #[test]
    fn repeat_guard_does_not_trip_on_normal_prose() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let payload =
            "我数了一下 count = 3，然后继续做别的事。\n这是第二行正常文字。\n第三行也正常。";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let text = collect_text_content(&all);
        assert!(
            text.contains("我数了一下"),
            "正常正文不应被熔断: {:?}",
            text
        );
        assert!(
            text.contains("第三行也正常"),
            "正常正文应完整保留: {:?}",
            text
        );
    }

    /// 🟢 跨 chunk 复读也能熔断（流式分片到达，每片一个 count）。
    #[test]
    fn repeat_guard_trips_across_chunks() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("call\n\n"));
        for _ in 0..2000 {
            all.extend(ctx.process_assistant_response("count\n\n"));
        }
        all.extend(ctx.generate_final_events());
        let text = collect_text_content(&all);
        let emitted_counts = text.matches("count").count();
        assert!(
            emitted_counts < 64,
            "跨 chunk 复读也应熔断：实际吐出 count={}",
            emitted_counts
        );
    }

    #[test]
    fn repeat_guard_trips_on_four_line_cycle_across_chunks() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let cycle = [
            ("user", "继续\n"),
            ("assistStill ", "down.\n"),
            ("user", "继续\n"),
            ("assistNo ", "change.\n"),
        ];
        let mut events = Vec::new();
        for _ in 0..8 {
            for (prefix, suffix) in cycle {
                events.extend(ctx.process_assistant_response(prefix));
                events.extend(ctx.process_assistant_response(suffix));
            }
        }
        events.extend(ctx.generate_final_events());

        assert!(
            ctx.repetition_guard_tripped(),
            "four-line upstream cycle must trip the repetition guard"
        );
        assert!(events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_repetition_guard"
        }));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn repeat_guard_preserves_short_cycle_and_fenced_code() {
        let mut short = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = short.generate_initial_events();
        let mut short_events = Vec::new();
        for _ in 0..3 {
            short_events.extend(short.process_assistant_response("alpha\nbeta\ngamma\ndelta\n"));
        }
        assert!(!short.repetition_guard_tripped());
        assert_eq!(
            collect_text_content(&short_events),
            "alpha\nbeta\ngamma\ndelta\n".repeat(3)
        );

        let mut fenced = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = fenced.generate_initial_events();
        let mut fenced_events = Vec::new();
        fenced_events.extend(fenced.process_assistant_response("```text\n"));
        for _ in 0..10 {
            fenced_events.extend(fenced.process_assistant_response("alpha\nbeta\n"));
        }
        fenced_events.extend(fenced.process_assistant_response("```\n"));

        assert!(!fenced.repetition_guard_tripped());
        assert_eq!(
            collect_text_content(&fenced_events)
                .matches("alpha\nbeta\n")
                .count(),
            10
        );
    }

    #[test]
    fn repeat_guard_trips_on_generic_brace_flood() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut events = Vec::new();
        for _ in 0..100 {
            events.extend(ctx.process_assistant_response("}\n\n"));
        }
        events.extend(ctx.generate_final_events());

        let text = collect_text_content(&events);
        // 相对阈值断言：跳闸后放行数不应超过阈值本身，调参时不必再改这里。
        assert!(
            text.matches('}').count() <= REPEAT_GUARD_TRIP_THRESHOLD as usize,
            "generic flood was not stopped: {text:?}"
        );
        assert!(ctx.repetition_guard_tripped());
        assert!(!events.iter().any(|event| event.event == "message_delta"));
        assert!(!events.iter().any(|event| event.event == "message_stop"));
        assert!(events.iter().any(|event| {
            event.event == "error" && event.data["error"]["type"] == "upstream_repetition_guard"
        }));
    }

    #[test]
    fn repeat_guard_trips_on_native_thinking_flood() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut events = Vec::new();
        for _ in 0..100 {
            events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
                crate::kiro::model::events::ReasoningContentEvent {
                    text: Some("}\n\n".into()),
                    signature: None,
                    redacted_content: None,
                },
            )));
        }
        events.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&events);
        assert!(
            thinking.matches('}').count() < 32,
            "thinking flood was not stopped: {thinking:?}"
        );
        // thinking 复读只静音 thinking 通道，本轮不算失败：
        // 正文与工具调用必须照常交付，否则一次 thinking 误判就会打断客户端的
        // agentic 循环（这是线上「工具调用凭空消失、会话卡住」的成因）。
        assert!(ctx.thinking_repetition_guard_tripped());
        assert!(
            !ctx.repetition_guard_tripped(),
            "thinking 复读不得把整轮判为退化"
        );
        assert!(
            !events.iter().any(|event| {
                event.event == "error" && event.data["error"]["type"] == "upstream_repetition_guard"
            }),
            "thinking 复读不得产生终态 error"
        );
        assert!(
            events.iter().any(|event| event.event == "message_stop"),
            "thinking 复读后本轮仍须正常收尾"
        );
    }

    #[test]
    fn repeat_guard_stops_native_thinking_at_threshold() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut events = Vec::new();
        for _ in 0..20 {
            events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
                crate::kiro::model::events::ReasoningContentEvent {
                    text: Some("用户要求直接实现。\n".into()),
                    signature: None,
                    redacted_content: None,
                },
            )));
        }

        let thinking = collect_thinking_content(&events);
        // 阈值 12：正常推理里连续几个相同短行（分点、过渡词）不该被判成退化，
        // 但真正的死循环会迅速越过它。
        assert!(
            thinking.matches("用户要求直接实现。").count()
                <= REPEAT_GUARD_THINKING_TRIP_THRESHOLD as usize,
            "thinking 复读应在阈值处停止，实际内容: {thinking:?}"
        );
        assert!(ctx.thinking_repetition_guard_tripped());
        assert!(
            !ctx.repetition_guard_tripped(),
            "thinking 复读不得把整轮判为退化"
        );
    }

    #[test]
    fn normal_thinking_repetition_below_threshold_survives() {
        // 回归：阈值原为 4，正常推理里连续 4 个相同短行（分点枚举、重复过渡词）
        // 就会跳闸，进而丢弃整轮响应连工具调用——线上表现为会话被打断。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut events = Vec::new();
        for _ in 0..5 {
            events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
                crate::kiro::model::events::ReasoningContentEvent {
                    text: Some("先看下一处。\n".into()),
                    signature: None,
                    redacted_content: None,
                },
            )));
        }

        assert!(
            !ctx.thinking_repetition_guard_tripped(),
            "连续 5 次相同短行不应触发 thinking 熔断"
        );
        let thinking = collect_thinking_content(&events);
        assert_eq!(
            thinking.matches("先看下一处。").count(),
            5,
            "阈值以下的重复必须全部保留，实际: {thinking:?}"
        );
    }

    #[test]
    fn native_reasoning_keeps_exact_duplicate_long_event() {
        // 曾按内容 SHA256 丢弃「与最近 4 条完全相同」的长 reasoning 事件，已移除。
        //
        // 模型在推理里重复引用同一段代码（对比修改前后时很常见）、重述同一份清单，都是
        // 合法输出；静默丢掉第二份会让用户看到有缺口的推理。Kiro-Go 踩过同一个坑并留下
        // 警告（proxy/kiro.go:608），同时指出上游从不重放分片——所以每次「重复」都是模型
        // 真实产出。真正的退化复读由熔断器按阈值处理，并明确告知客户端。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let reasoning = format!("{}\n", "inspect the built bundle carefully. ".repeat(16));
        assert!(reasoning.len() >= 256);
        let event = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: Some(reasoning.clone()),
            signature: None,
            redacted_content: None,
        });

        let mut events = ctx.process_kiro_event(&event);
        events.extend(ctx.process_kiro_event(&event));

        assert_eq!(
            collect_thinking_content(&events),
            format!("{reasoning}{reasoning}"),
            "重复的长 reasoning 必须原样交付，不得静默丢弃"
        );
        assert!(!ctx.repetition_guard_tripped());
    }

    #[test]
    fn long_reasoning_dedup_applies_across_assistant_and_native_sources() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let reasoning = "inspect the built bundle carefully. ".repeat(16);
        let wrapped = format!("<thinking>\n{reasoning}</thinking>\n\n");

        let mut events = ctx.process_assistant_response(&wrapped);
        events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some(reasoning.clone()),
                signature: None,
                redacted_content: None,
            },
        )));
        events.extend(ctx.process_kiro_event(&Event::ToolUse(tool_evt(
            "tool_1",
            "exec_command",
            r#"{"cmd":"pwd"}"#,
            true,
        ))));
        events.extend(ctx.generate_final_events());

        assert_eq!(collect_thinking_content(&events), reasoning);
        let tools = collect_tool_uses(&events);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].0, "exec_command");
        assert!(!events.iter().any(|event| event.event == "error"));
    }

    #[test]
    fn native_reasoning_keeps_short_and_distinct_events() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let short = "same short fragment";
        let long_a = "A".repeat(300);
        let long_b = "B".repeat(300);
        let mut events = Vec::new();
        for text in [
            short.to_string(),
            short.to_string(),
            long_a.clone(),
            long_b.clone(),
        ] {
            events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
                crate::kiro::model::events::ReasoningContentEvent {
                    text: Some(text),
                    signature: None,
                    redacted_content: None,
                },
            )));
        }

        assert_eq!(
            collect_thinking_content(&events),
            format!("{short}{short}{long_a}{long_b}")
        );
        assert!(!ctx.repetition_guard_tripped());
    }

    #[test]
    fn duplicate_native_reasoning_preserves_following_tool_use() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let reasoning = "inspect before calling the tool. ".repeat(16);
        let event = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: Some(reasoning.clone()),
            signature: None,
            redacted_content: None,
        });
        let mut events = ctx.process_kiro_event(&event);
        events.extend(ctx.process_kiro_event(&event));
        events.extend(ctx.process_kiro_event(&Event::ToolUse(tool_evt(
            "tool_1",
            "exec_command",
            r#"{"cmd":"pwd"}"#,
            true,
        ))));
        events.extend(ctx.generate_final_events());

        // 重复内容原样交付（不再静默去重），且不得影响后续工具调用。
        assert_eq!(
            collect_thinking_content(&events),
            format!("{reasoning}{reasoning}")
        );
        let tools = collect_tool_uses(&events);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].0, "exec_command");
        assert!(!events.iter().any(|event| event.event == "error"));
    }

    #[test]
    fn repeated_reasoning_survives_continuation_boundary() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let reasoning = "repeatable reasoning in a separate upstream round. ".repeat(12);
        let event = Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
            text: Some(reasoning.clone()),
            signature: None,
            redacted_content: None,
        });

        let mut events = ctx.process_kiro_event(&event);
        let _ = ctx.generate_final_events();
        ctx.prepare_for_continuation();
        events.extend(ctx.process_kiro_event(&event));

        assert_eq!(collect_thinking_content(&events), reasoning.repeat(2));
    }

    #[test]
    fn periodic_repeat_guard_resets_at_continuation_boundary() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let cycle = "alpha item\nbeta item\ngamma item\ndelta item\n";

        for _ in 0..3 {
            let _ = ctx.repeat_guard_filter(cycle, "text");
        }
        ctx.prepare_for_continuation();
        for _ in 0..3 {
            let _ = ctx.repeat_guard_filter(cycle, "text");
        }

        assert!(!ctx.repetition_guard_tripped());
    }

    #[test]
    fn continuation_overlap_removes_replayed_suffix_across_chunks() {
        let mut filter = ContinuationOverlapFilter::new("alpha beta gamma");
        assert_eq!(filter.push("beta "), "");
        assert_eq!(filter.push("gamma delta"), " delta");
        assert_eq!(filter.push(" epsilon"), " epsilon");
    }

    #[test]
    fn continuation_overlap_preserves_fresh_text() {
        let mut filter = ContinuationOverlapFilter::new("alpha beta gamma");
        assert_eq!(filter.push("completely new"), "completely new");
    }

    #[test]
    fn continuation_keeps_one_message_and_deduplicates_the_seam() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let mut events = ctx.generate_initial_events();
        let mut first = crate::kiro::model::events::AssistantResponseEvent::default();
        first.content = "alpha beta".into();
        events.extend(ctx.process_kiro_event(&Event::AssistantResponse(first)));
        let mut intermediate = ctx.generate_final_events();
        intermediate
            .retain(|event| !matches!(event.event.as_str(), "message_delta" | "message_stop"));
        events.extend(intermediate);

        ctx.prepare_for_continuation();
        ctx.begin_continuation();
        let mut second = crate::kiro::model::events::AssistantResponseEvent::default();
        second.content = "beta gamma".into();
        events.extend(ctx.process_kiro_event(&Event::AssistantResponse(second)));
        events.extend(ctx.flush_continuation_overlap());
        events.extend(ctx.generate_final_events());

        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "message_start")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "message_stop")
                .count(),
            1
        );
        assert_eq!(collect_text_content(&events), "alpha beta gamma");
    }

    /// 断点块类型是第 2 批续写方案的必需输入：断在 tool_use 中续写会让客户端
    /// 把副作用执行两遍，断在 thinking 中会让签名校验失败。这条钉住标签本身。
    #[test]
    fn break_block_label_reports_open_block_or_boundary() {
        let mut manager = SseStateManager::new();
        // 没有任何块打开 → 断在边界上，是最安全的续写位置。
        assert_eq!(manager.open_block_type(), None);

        let text_index = manager.next_block_index();
        manager.handle_content_block_start(text_index, "text", serde_json::json!({}));
        assert_eq!(manager.open_block_type(), Some("text"));

        // 已闭合的块不再算「未闭合」。
        manager.handle_content_block_stop(text_index);
        assert_eq!(manager.open_block_type(), None);

        let tool_index = manager.next_block_index();
        manager.handle_content_block_start(tool_index, "tool_use", serde_json::json!({}));
        assert_eq!(
            manager.open_block_type(),
            Some("tool_use"),
            "断在 tool_use 中必须能被识别出来——这是不可续写的情形"
        );
    }

    /// 阈值默认 85%，且非法值必须退回 100%（宁可不触发也不误触发）。
    ///
    /// 如果 0 或负数被原样接受，`percentage >= threshold` 会对**每个**请求成立，
    /// 客户端会被逼着无谓压缩每一轮对话——比不修更糟。
    #[test]
    fn context_window_signal_threshold_defaults_to_85_and_clamps_illegal_values() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-5",
            10,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        assert_eq!(ctx.context_window_signal_threshold_pct, 85.0);

        for illegal in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            ctx.set_context_window_signal_threshold_pct(illegal);
            assert_eq!(
                ctx.context_window_signal_threshold_pct, 100.0,
                "非法阈值 {illegal} 必须退回 100，避免每个请求都误判超限"
            );
        }

        // >100 夹到 100（等于永不触发），合法值原样保留。
        ctx.set_context_window_signal_threshold_pct(150.0);
        assert_eq!(ctx.context_window_signal_threshold_pct, 100.0);
        ctx.set_context_window_signal_threshold_pct(90.0);
        assert_eq!(ctx.context_window_signal_threshold_pct, 90.0);
    }

    #[test]
    fn reasoning_output_is_visible_to_the_auto_continue_gate() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let _ = ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("reasoning".into()),
                signature: None,
                redacted_content: None,
            },
        ));

        assert!(ctx.saw_reasoning_output());
    }

    #[test]
    fn repeat_guard_allows_fifteen_identical_lines() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut events = Vec::new();
        for _ in 0..15 {
            events.extend(ctx.process_assistant_response("}\n"));
        }
        assert!(!ctx.repetition_guard_tripped());
        assert_eq!(collect_text_content(&events).matches('}').count(), 15);
    }

    #[test]
    fn repeat_guard_preserves_differently_indented_braces() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        );
        let _ = ctx.generate_initial_events();
        let mut events = Vec::new();
        for _ in 0..40 {
            events.extend(ctx.process_assistant_response("}\n  }\n    }\n"));
        }
        assert!(!ctx.repetition_guard_tripped());
        assert_eq!(collect_text_content(&events).matches('}').count(), 120);
    }

    // ---- 块级复读熔断 (collapse_stray_token_floods)：覆盖 web_search loop 路径 ----

    /// 🔴→🟢 块级路径（extract_invoke_content_blocks / web_search loop）也必须熔断 count 洪水。
    #[test]
    fn extract_blocks_collapses_count_flood() {
        let mut text = String::from("先看 crawlee 状态。\n\ncall\n\n");
        for _ in 0..5000 {
            text.push_str("count\n\n");
        }
        let blocks = extract_invoke_content_blocks(
            &text,
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        let joined: String = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        let emitted = joined.matches("count").count();
        assert!(emitted < 64, "块级路径应折叠 count 洪水：实际={}", emitted);
        assert!(
            joined.contains("先看 crawlee 状态"),
            "正常正文应保留: {:?}",
            &joined[..joined.len().min(60)]
        );
    }

    /// 🟢 块级不误伤：单个引导词 count + 真 invoke 仍被捞回。
    #[test]
    fn extract_blocks_keeps_single_stray_and_reclaims() {
        let text = "count\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let blocks = extract_invoke_content_blocks(
            text,
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        assert!(
            blocks
                .iter()
                .any(|b| b["type"] == "tool_use" && b["name"] == "exec_command"),
            "单个引导词不应触发折叠，invoke 应捞回: {:?}",
            blocks
        );
    }

    #[test]
    fn test_native_reasoning_event_emits_thinking_with_signature() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("native reasoning".to_string()),
                signature: Some("real-signature".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_assistant_response("final answer"));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(collect_thinking_content(&all_events), "native reasoning");
        assert_eq!(collect_text_content(&all_events), "final answer");
        assert!(all_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "real-signature"
        }));
    }

    #[test]
    fn test_native_reasoning_signature_only_applies_to_next_thinking_text() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: Some("signature-before-text".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("delayed native reasoning".to_string()),
                signature: None,
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_thinking_content(&all_events),
            "delayed native reasoning"
        );
        assert!(all_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "signature-before-text"
        }));
    }

    #[test]
    fn test_native_reasoning_text_downgrades_to_text_when_thinking_disabled() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("visible reasoning fallback".to_string()),
                signature: Some("ignored-signature".to_string()),
                redacted_content: Some("ignored-redacted".to_string()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_text_content(&all_events),
            "visible reasoning fallback"
        );
        assert_eq!(collect_thinking_content(&all_events), "");
        assert!(!all_events.iter().any(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "signature_delta"
        }));
        assert!(!all_events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "redacted_thinking"
        }));
    }

    #[test]
    fn test_native_redacted_thinking_is_ordered_between_thinking_and_text() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("native reasoning".to_string()),
                signature: Some("real-signature".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: None,
                redacted_content: Some("encrypted-thinking".to_string()),
            },
        )));
        all_events.extend(ctx.process_assistant_response("final answer"));
        all_events.extend(ctx.generate_final_events());

        let (_, thinking_idx) = block_start_position(&all_events, "thinking");
        let thinking_stop_pos = block_stop_position(&all_events, thinking_idx);
        let (redacted_start_pos, redacted_idx) =
            block_start_position(&all_events, "redacted_thinking");
        let redacted_stop_pos = block_stop_position(&all_events, redacted_idx);
        let (text_start_pos, _) = block_start_position(&all_events, "text");

        assert!(
            thinking_stop_pos < redacted_start_pos,
            "thinking block must close before redacted_thinking starts"
        );
        assert!(
            redacted_stop_pos < text_start_pos,
            "redacted_thinking block must close before text starts"
        );
        assert_eq!(collect_thinking_content(&all_events), "native reasoning");
        assert_eq!(collect_text_content(&all_events), "final answer");
    }

    #[test]
    fn test_native_reasoning_event_emits_redacted_thinking() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: None,
                redacted_content: Some("encrypted-thinking".to_string()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert!(all_events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "redacted_thinking"
                && e.data["content_block"]["data"] == "encrypted-thinking"
        }));
    }
}
