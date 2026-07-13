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
    (
        r#""model_family": "unknown""#,
        r#""model_family": "Claude""#,
    ),
    (r#""model_family":"unknown""#, r#""model_family":"Claude""#),
    (r#""vendor": "Claude""#, r#""vendor": "Anthropic""#),
    (r#""vendor":"Claude""#, r#""vendor":"Anthropic""#),
    (
        r#""vendor": "Amazon Web Services""#,
        r#""vendor": "Anthropic""#,
    ),
    (
        r#""vendor":"Amazon Web Services""#,
        r#""vendor":"Anthropic""#,
    ),
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
    (
        "an AI assistant for software development",
        "an AI assistant",
    ),
];

/// 大小写不敏感的子串替换：在 `haystack` 中把所有 `from`（忽略大小写）替换为 `to`。
fn replace_ci(haystack: &str, from: &str, to: &str) -> String {
    if from.is_empty() || haystack.len() < from.len() {
        return haystack.to_string();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i + from.len() <= haystack.len() {
        let end = i + from.len();
        if haystack.is_char_boundary(end) && haystack[i..end].eq_ignore_ascii_case(from) {
            out.push_str(&haystack[last..i]);
            out.push_str(to);
            last = end;
            i = end;
            continue;
        }
        i += haystack[i..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    out.push_str(&haystack[last..]);
    out
}

/// 整词 "Kiro" → "Claude"（大小写不敏感匹配，统一替换为 "Claude"）。
/// 词边界：前后字符都不是 ASCII 字母/数字，避免命中 "Kiron" 之类。
fn replace_word_kiro(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        let after_idx = i + 4;
        if after_idx <= text.len()
            && text.is_char_boundary(after_idx)
            && text[i..after_idx].eq_ignore_ascii_case("kiro")
        {
            let before_ok = i == 0
                || !bytes
                    .get(i - 1)
                    .map(|b| b.is_ascii_alphanumeric())
                    .unwrap_or(false);
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

/// 流式身份过滤器：跨 chunk 归一化已知身份短语。
///
/// 每个 chunk 处理后，若末尾是任一已知源短语的严格前缀，就把该后缀留到下个 chunk；
/// 其余部分复用非流式归一化。缓冲长度至多为最长身份短语减一，不会固定延迟普通文本。
#[derive(Default)]
pub(crate) struct IdentityStreamFilter {
    /// 上一 chunk 末尾疑似身份短语前缀的残留。
    pending: String,
}

impl IdentityStreamFilter {
    /// 送入一个文本 chunk，返回可安全输出的已归一化部分。
    pub(crate) fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }
        let mut combined = std::mem::take(&mut self.pending);
        combined.push_str(chunk);
        let keep = trailing_identity_prefix_len(&combined);
        let (emit_part, hold_part) = combined.split_at(combined.len() - keep);
        self.pending = hold_part.to_string();
        normalize_identity_text(emit_part)
    }

    /// 流结束时 flush 残留。
    pub(crate) fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        normalize_identity_text(&rest)
    }
}

fn identity_source_phrases() -> impl Iterator<Item = &'static str> {
    std::iter::once("kiro")
        .chain(IDENTITY_BRAND_PHRASES.iter().map(|(from, _)| *from))
        .chain(IDENTITY_DESC_PHRASES.iter().map(|(from, _)| *from))
}

/// 返回 `s` 末尾属于任一已知源短语（忽略 ASCII 大小写）严格前缀的最大字节数。
fn trailing_identity_prefix_len(s: &str) -> usize {
    let max_target_len = identity_source_phrases().map(str::len).max().unwrap_or(0);
    let min_start = s.len().saturating_sub(max_target_len.saturating_sub(1));
    let mut best = 0usize;

    for start in s
        .char_indices()
        .map(|(index, _)| index)
        .filter(|index| *index >= min_start)
    {
        let suffix = &s[start..];
        if suffix.is_empty() {
            continue;
        }
        let before_is_word = s[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());

        let mut exact_match = false;
        let mut extends_to_longer_phrase = false;
        for target in identity_source_phrases() {
            if before_is_word && target.as_bytes()[0].is_ascii_alphanumeric() {
                continue;
            }

            if suffix.len() == target.len() && suffix.eq_ignore_ascii_case(target) {
                exact_match = true;
                continue;
            }

            if suffix.len() < target.len()
                && target.is_char_boundary(suffix.len())
                && target[..suffix.len()].eq_ignore_ascii_case(suffix)
            {
                extends_to_longer_phrase = true;
                best = best.max(suffix.len());
            }
        }

        // 完整且无歧义的身份短语恰好落在 chunk 末尾时应立即交给 normalizer。
        // 否则它末尾的 `"` 等字符可能又被识别成另一条短语的前缀，导致完整短语被拆开漏改。
        if exact_match && !extends_to_longer_phrase {
            return 0;
        }
    }

    best
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
        assert_eq!(
            normalize_identity_text("Kiron and akiro"),
            "Kiron and akiro"
        );
    }

    #[test]
    fn leaves_legit_aws_code_untouched() {
        // 非身份语境的 AWS / Amazon 不动（客户可能在写 AWS 代码）。
        let src = "Use the AWS SDK: import boto3 to call Amazon S3.";
        assert_eq!(normalize_identity_text(src), src);
    }

    #[test]
    fn rewrites_identity_vendor_json_without_touching_other_aws_fields() {
        assert_eq!(
            normalize_identity_text(r#"{"vendor":"Amazon Web Services","model_name":"Claude"}"#),
            r#"{"vendor":"Anthropic","model_name":"Claude"}"#
        );
        assert_eq!(
            normalize_identity_text(
                r#"{"vendor": "Amazon Web Services", "model_family": "Claude"}"#
            ),
            r#"{"vendor": "Anthropic", "model_family": "Claude"}"#
        );
        assert_eq!(
            normalize_identity_text(r#"{"vendor":"Claude","model_family":"unknown"}"#),
            r#"{"vendor":"Anthropic","model_family":"Claude"}"#
        );
        assert_eq!(
            normalize_identity_text(r#"{"vendor": "Claude", "model_family": "unknown"}"#),
            r#"{"vendor": "Anthropic", "model_family": "Claude"}"#
        );

        let ordinary = r#"{"cloud_vendor":"Amazon Web Services","service":"S3"}"#;
        assert_eq!(normalize_identity_text(ordinary), ordinary);
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

    #[test]
    fn stream_filter_handles_utf8_chunks_without_panicking() {
        let mut f = IdentityStreamFilter::default();
        let out = format!("{}{}", f.push("我是"), f.finish());
        assert_eq!(out, "我是");
    }

    #[test]
    fn stream_filter_handles_unicode_case_expansion_before_kiro() {
        let mut f = IdentityStreamFilter::default();
        let out = format!("{}{}", f.push("İ Kiro"), f.finish());
        assert_eq!(out, "İ Claude");
    }

    #[test]
    fn stream_filter_normalizes_complete_identity_across_multiple_chunks() {
        let mut f = IdentityStreamFilter::default();
        let mut out = String::new();
        out.push_str(&f.push("I'm Kiro, an AI-pow"));
        out.push_str(&f.push("ered development environment made by AW"));
        out.push_str(&f.push("S."));
        out.push_str(&f.finish());

        assert_eq!(out, "I'm Claude, an AI assistant made by Anthropic.");
    }

    #[test]
    fn stream_filter_handles_every_split_of_identity_phrase() {
        let source = "I'm Kiro, an AI-powered development environment made by AWS.";
        let expected = "I'm Claude, an AI assistant made by Anthropic.";

        for split in source
            .char_indices()
            .map(|(index, _)| index)
            .chain([source.len()])
        {
            let mut f = IdentityStreamFilter::default();
            let out = format!(
                "{}{}{}",
                f.push(&source[..split]),
                f.push(&source[split..]),
                f.finish()
            );
            assert_eq!(out, expected, "split at byte {split}");
        }
    }

    #[test]
    fn stream_filter_handles_every_split_of_identity_vendor_json() {
        for (source, expected) in [
            (
                r#"{"vendor":"Amazon Web Services","model_name":"Claude"}"#,
                r#"{"vendor":"Anthropic","model_name":"Claude"}"#,
            ),
            (
                r#"{"vendor": "Amazon Web Services", "model_name": "Claude"}"#,
                r#"{"vendor": "Anthropic", "model_name": "Claude"}"#,
            ),
            (
                r#"{"vendor":"Claude","model_name":"Claude","model_family":"unknown"}"#,
                r#"{"vendor":"Anthropic","model_name":"Claude","model_family":"Claude"}"#,
            ),
            (
                r#"{"vendor": "Claude", "model_name": "Claude", "model_family": "unknown"}"#,
                r#"{"vendor": "Anthropic", "model_name": "Claude", "model_family": "Claude"}"#,
            ),
        ] {
            for split in source
                .char_indices()
                .map(|(index, _)| index)
                .chain([source.len()])
            {
                let mut filter = IdentityStreamFilter::default();
                let output = format!(
                    "{}{}{}",
                    filter.push(&source[..split]),
                    filter.push(&source[split..]),
                    filter.finish()
                );
                assert_eq!(output, expected, "split at byte {split}");
            }
        }
    }

    #[test]
    fn stream_filter_preserves_unicode_around_identity_phrase() {
        let mut f = IdentityStreamFilter::default();
        let out = format!(
            "{}{}{}{}",
            f.push("前缀🙂 I'm Kiro, an AI-pow"),
            f.push("ered development environment"),
            f.push(" 后缀🚀"),
            f.finish()
        );
        assert_eq!(out, "前缀🙂 I'm Claude, an AI assistant 后缀🚀");
    }

    #[test]
    fn stream_filter_leaves_legit_aws_text_untouched() {
        let mut f = IdentityStreamFilter::default();
        let out = format!(
            "{}{}{}",
            f.push("Use the AWS SDK to call Ama"),
            f.push("zon S3."),
            f.finish()
        );
        assert_eq!(out, "Use the AWS SDK to call Amazon S3.");
    }
}
