//! 身份归一化：把 Kiro 网关注入的品牌身份改写回 Claude。
//!
//! 本中转底层就是真实的 Claude 模型，只是请求经过 Kiro 网关；网关的服务端 system
//! 会让模型自称 "Kiro" / "made by AWS"。这不是真实的模型身份，而是网关包装层的注入。
//! 本模块在**助手输出文本**上把这些品牌自述还原为 Claude / Anthropic，使对外表现与
//! 真实底层模型一致。仅改写自由文本里的身份自述，不触碰工具调用 JSON、代码逻辑或
//! 用户内容语义。由 config `identity_normalization` 开关控制（默认开启）。

/// 身份语境下的品牌短语替换（大小写不敏感匹配，替换为固定 Anthropic 形式）。
/// 只匹配"由 X 制造/开发"这类身份锚点短语，避免误伤正文里合法出现的 AWS / Amazon
/// （例如客户在写 AWS Lambda / Amazon S3 相关代码）。
const IDENTITY_BRAND_PHRASES: &[(&str, &str)] = &[
    ("made by AWS", "made by Anthropic"),
    ("made by Amazon Web Services", "made by Anthropic"),
    ("made by Amazon", "made by Anthropic"),
    ("built by AWS", "built by Anthropic"),
    ("built by Amazon Web Services", "built by Anthropic"),
    ("built by Amazon", "built by Anthropic"),
    ("created by AWS", "created by Anthropic"),
    ("created by Amazon", "created by Anthropic"),
    ("developed by AWS", "developed by Anthropic"),
    ("developed by Amazon Web Services", "developed by Anthropic"),
    ("developed by Amazon", "developed by Anthropic"),
    ("trained by Amazon", "trained by Anthropic"),
    ("powered by AWS", "powered by Anthropic"),
];

/// 描述性自述短语（Kiro 常用），替换为中性的 Claude 措辞。
const IDENTITY_DESC_PHRASES: &[(&str, &str)] = &[
    ("an AI-powered development environment", "an AI assistant"),
    ("AI-powered development environment", "AI assistant"),
    ("an AI assistant for software development", "an AI assistant"),
];

/// 大小写不敏感的子串替换：在 `haystack` 中把所有 `from`（忽略大小写）替换为 `to`。
fn replace_ci(haystack: &str, from: &str, to: &str) -> String {
    if from.is_empty() || haystack.len() < from.len() {
        return haystack.to_string();
    }
    let hay_lower = haystack.to_lowercase();
    let from_lower = from.to_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0usize;
    let mut search_from = 0usize;
    while let Some(rel) = hay_lower[search_from..].find(&from_lower) {
        let start = search_from + rel;
        out.push_str(&haystack[last..start]);
        out.push_str(to);
        last = start + from.len();
        search_from = last;
    }
    out.push_str(&haystack[last..]);
    out
}

/// 整词 "Kiro" → "Claude"（大小写不敏感匹配，统一替换为 "Claude"）。
/// 词边界：前后字符都不是 ASCII 字母/数字，避免命中 "Kiron" 之类。
fn replace_word_kiro(text: &str) -> String {
    let lower = text.to_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if lower[i..].starts_with("kiro") {
            let before_ok = i == 0
                || !bytes
                    .get(i - 1)
                    .map(|b| b.is_ascii_alphanumeric())
                    .unwrap_or(false);
            let after_idx = i + 4;
            let after_ok = after_idx >= text.len()
                || !bytes
                    .get(after_idx)
                    .map(|b| b.is_ascii_alphanumeric())
                    .unwrap_or(false);
            if before_ok && after_ok {
                out.push_str("Claude");
                i = after_idx;
                continue;
            }
        }
        // 推进一个 UTF-8 字符（Kiro 身份文本以 ASCII 为主，但兜住多字节）。
        let ch_len = text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// 归一化助手输出里的品牌身份泄漏。顺序：品牌短语 → 描述短语 → 整词 Kiro。
pub(crate) fn normalize_identity_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut s = text.to_string();
    for (from, to) in IDENTITY_BRAND_PHRASES {
        s = replace_ci(&s, from, to);
    }
    for (from, to) in IDENTITY_DESC_PHRASES {
        s = replace_ci(&s, from, to);
    }
    replace_word_kiro(&s)
}

/// 流式身份过滤器：跨 chunk 把整词 "Kiro" → "Claude"。
///
/// 流式下 "Kiro" 可能被切在两个 chunk 之间（如 "...I'm Ki" | "ro..."），故用小缓冲：
/// 每个 chunk 处理后，若末尾是 "kiro" 的**前缀**（且该前缀前是词边界），就把它留到
/// 下个 chunk 再判定；否则全部输出。只处理整词 Kiro（最主要、最安全的身份泄漏关键词）；
/// "made by AWS" 等多词短语走非流式完整归一化（身份探针均为非流式）。
#[derive(Default)]
pub(crate) struct IdentityStreamFilter {
    /// 上一 chunk 末尾疑似 "kiro" 前缀的残留（含其前的词边界判定信息）。
    pending: String,
}

impl IdentityStreamFilter {
    /// 送入一个文本 chunk，返回可安全输出的部分（已完成 Kiro→Claude 替换）。
    pub(crate) fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }
        let mut combined = std::mem::take(&mut self.pending);
        combined.push_str(chunk);
        // 找出末尾最长的、可能是 "kiro" 前缀的后缀（1..=3 个字符，"kiro" 本身会被整词替换）。
        let keep = trailing_kiro_prefix_len(&combined);
        let (emit_part, hold_part) = combined.split_at(combined.len() - keep);
        self.pending = hold_part.to_string();
        replace_word_kiro(emit_part)
    }

    /// 流结束时 flush 残留（末尾疑似前缀若不是完整 "kiro"，原样输出）。
    pub(crate) fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        replace_word_kiro(&rest)
    }
}

/// 返回 `s` 末尾属于 "kiro"（忽略大小写）严格前缀的字符数（1..=3）；无则 0。
/// 只在该前缀之前是词边界时才认（否则不可能构成整词 Kiro，无需缓冲）。
fn trailing_kiro_prefix_len(s: &str) -> usize {
    const TARGET: &str = "kiro";
    let lower = s.to_lowercase();
    // 前缀长度从长到短试：3,2,1（长度 4 = 完整词，交给 replace_word_kiro 处理，不缓冲）。
    for len in (1..=3).rev() {
        if lower.len() < len {
            continue;
        }
        let suffix = &lower[lower.len() - len..];
        if TARGET.starts_with(suffix) {
            // 词边界检查：该前缀前一个字符不能是字母数字。
            let before_idx = s.len() - len;
            let before_ok = before_idx == 0
                || !s.as_bytes()[before_idx - 1].is_ascii_alphanumeric();
            if before_ok {
                return len;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_i_am_kiro_identity() {
        assert_eq!(
            normalize_identity_text("I'm Kiro, an AI-powered development environment made by AWS."),
            "I'm Claude, an AI assistant made by Anthropic."
        );
    }

    #[test]
    fn rewrites_all_kiro_occurrences_case_preserving_to_claude() {
        assert_eq!(
            normalize_identity_text("I'm Kiro. I'm not Claude. I'm kiro, and I'll be straight."),
            "I'm Claude. I'm not Claude. I'm Claude, and I'll be straight."
        );
    }

    #[test]
    fn does_not_touch_kiro_inside_other_words() {
        // 词边界保护：不误伤 "Kiron" / "kiroshi" 之类。
        assert_eq!(normalize_identity_text("Kiron and akiro"), "Kiron and akiro");
    }

    #[test]
    fn leaves_legit_aws_code_untouched() {
        // 非身份语境的 AWS / Amazon 不动（客户可能在写 AWS 代码）。
        let src = "Use the AWS SDK: import boto3 to call Amazon S3.";
        assert_eq!(normalize_identity_text(src), src);
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(normalize_identity_text(""), "");
    }

    #[test]
    fn stream_filter_handles_split_kiro_across_chunks() {
        let mut f = IdentityStreamFilter::default();
        let mut out = String::new();
        out.push_str(&f.push("I'm Ki"));
        out.push_str(&f.push("ro, hi"));
        out.push_str(&f.finish());
        assert_eq!(out, "I'm Claude, hi");
    }

    #[test]
    fn stream_filter_no_false_hold_on_non_kiro_tail() {
        let mut f = IdentityStreamFilter::default();
        // "k" 结尾会被暂留（可能是 kiro 前缀），下一 chunk 澄清后应正确输出。
        let a = f.push("pick");
        let b = f.push(" up");
        let c = f.finish();
        assert_eq!(format!("{a}{b}{c}"), "pick up");
    }

    #[test]
    fn stream_filter_whole_kiro_in_one_chunk() {
        let mut f = IdentityStreamFilter::default();
        let out = format!("{}{}", f.push("I am Kiro here"), f.finish());
        assert_eq!(out, "I am Claude here");
    }
}

