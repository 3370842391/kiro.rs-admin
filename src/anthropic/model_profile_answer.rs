use super::model_profile::{ResolvedModelProfile, cutoff_month_year, is_trusted_profile_source};
use super::types::MessagesRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileProbe {
    ContextWindow,
    KnowledgeCutoff,
}

pub(crate) fn exact_model_profile_answer(
    request: &MessagesRequest,
    profile: &ResolvedModelProfile,
    enabled: bool,
) -> Option<String> {
    if !enabled {
        return None;
    }
    match classify_profile_probe(request)? {
        ProfileProbe::ContextWindow => {
            let field = profile.context_window_field.as_ref()?;
            (field.value > 0 && is_trusted_profile_source(&field.source))
                .then(|| field.value.to_string())
        }
        ProfileProbe::KnowledgeCutoff => {
            let field = profile.knowledge_cutoff_field.as_ref()?;
            is_trusted_profile_source(&field.source)
                .then(|| cutoff_month_year(&field.value))
                .flatten()
        }
    }
}

fn classify_profile_probe(request: &MessagesRequest) -> Option<ProfileProbe> {
    if request.messages.len() != 1
        || request.messages[0].role != "user"
        || request.tools.is_some()
        || request.tool_choice.is_some()
        || request.thinking.is_some()
        || request.output_config.is_some()
        || request.force_web_search_loop
        || request
            .system
            .as_ref()
            .is_some_and(|items| items.iter().any(|item| !item.text.trim().is_empty()))
    {
        return None;
    }

    let text = match &request.messages[0].content {
        serde_json::Value::String(text) => text.as_str(),
        serde_json::Value::Array(blocks) if blocks.len() == 1 => {
            let block = blocks.first()?;
            if block.get("type").and_then(serde_json::Value::as_str) != Some("text") {
                return None;
            }
            block.get("text")?.as_str()?
        }
        _ => return None,
    };
    if text.as_bytes().len() > 512 {
        return None;
    }
    let trimmed = text.trim();
    match trimmed {
        "请问你的最大上下文窗口是多少 token？只返回一个不带单位和解释的整数。" =>
        {
            return Some(ProfileProbe::ContextWindow);
        }
        "你的知识截止日期是什么时候？只返回月份和年份，不要附加解释。" =>
        {
            return Some(ProfileProbe::KnowledgeCutoff);
        }
        _ => {}
    }

    let normalized = collapse_ascii_whitespace(&trimmed.to_ascii_lowercase());
    classify_context_english(&normalized).or_else(|| classify_cutoff_english(&normalized))
}

fn collapse_ascii_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_one_prefix<'a>(value: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes
        .iter()
        .find_map(|prefix| value.strip_prefix(prefix))
}

fn classify_context_english(value: &str) -> Option<ProfileProbe> {
    let rest = strip_one_prefix(
        value,
        &[
            "what is your maximum context window size in tokens? ",
            "what is your maximum context window in tokens? ",
            "tell me your maximum context window size in tokens. ",
            "tell me your maximum context window in tokens. ",
            "please what is your maximum context window size in tokens? ",
            "please tell me your maximum context window size in tokens. ",
        ],
    )?;
    let rest = strip_one_prefix(
        rest,
        &[
            "reply with just a single integer",
            "reply with only a single integer",
            "reply with just single integer",
            "respond with just a single integer",
            "respond with only a single integer",
        ],
    )?;
    valid_context_tail(rest).then_some(ProfileProbe::ContextWindow)
}

fn valid_context_tail(mut rest: &str) -> bool {
    rest = rest.trim();
    if let Some(next) = rest.strip_prefix("(no commas, no units, no explanation)") {
        rest = next.trim();
    }
    rest = rest.strip_prefix(',').unwrap_or(rest).trim();
    if rest.is_empty() || rest == "." {
        return true;
    }
    let Some(example) = rest
        .strip_prefix("e.g. ")
        .or_else(|| rest.strip_prefix("example: "))
    else {
        return false;
    };
    example
        .trim_end_matches('.')
        .chars()
        .all(|ch| ch.is_ascii_digit())
        && !example.trim_end_matches('.').is_empty()
}

fn classify_cutoff_english(value: &str) -> Option<ProfileProbe> {
    let rest = strip_one_prefix(
        value,
        &[
            "what is your knowledge cutoff date? ",
            "what is your knowledge cutoff? ",
            "tell me your knowledge cutoff date. ",
            "tell me your knowledge cutoff. ",
            "please what is your knowledge cutoff date? ",
            "please tell me your knowledge cutoff date. ",
        ],
    )?;
    let rest = strip_one_prefix(
        rest,
        &[
            "reply with just the month and year",
            "reply with only the month and year",
            "respond with just the month and year",
            "respond with only the month and year",
        ],
    )?;
    valid_cutoff_tail(rest).then_some(ProfileProbe::KnowledgeCutoff)
}

fn valid_cutoff_tail(mut rest: &str) -> bool {
    rest = rest.trim();
    rest = rest.strip_prefix(',').unwrap_or(rest).trim();
    if let Some(example) = rest.strip_prefix("e.g. ") {
        let Some(end) = example.find(". no additional explanation.") else {
            return false;
        };
        let value = example[..end].trim().trim_matches(['\'', '"']);
        return valid_example_month_year(value)
            && &example[end..] == ". no additional explanation.";
    }
    matches!(
        rest,
        "." | ". no additional explanation." | "no additional explanation."
    )
}

fn valid_example_month_year(value: &str) -> bool {
    let Some((month, year)) = value.rsplit_once(' ') else {
        return false;
    };
    matches!(
        month,
        "january"
            | "february"
            | "march"
            | "april"
            | "may"
            | "june"
            | "july"
            | "august"
            | "september"
            | "october"
            | "november"
            | "december"
    ) && year.len() == 4
        && year.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::model_profile::{ManualField, ModelProfileStore, PatchProfile};

    fn request(prompt: &str) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": prompt}]
        }))
        .unwrap()
    }

    fn profile() -> ResolvedModelProfile {
        let store = ModelProfileStore::new_in_memory();
        store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set(1_000_000)),
                knowledge_cutoff: Some(ManualField::set("2026-01".into())),
                ..Default::default()
            })
            .unwrap();
        store.resolve("claude-opus-4-8")
    }

    #[test]
    fn answers_two_strict_profile_probes() {
        assert_eq!(
            exact_model_profile_answer(
                &request(
                    "What is your maximum context window size in tokens? Reply with just a single integer (no commas, no units, no explanation), e.g. 200000."
                ),
                &profile(),
                true,
            ),
            Some("1000000".into())
        );
        assert_eq!(
            exact_model_profile_answer(
                &request(
                    "What is your knowledge cutoff date? Reply with just the month and year, e.g. 'March 2024'. No additional explanation."
                ),
                &profile(),
                true,
            ),
            Some("January 2026".into())
        );
    }

    #[test]
    fn answers_two_exact_chinese_templates_without_utf8_slicing() {
        assert_eq!(
            exact_model_profile_answer(
                &request("请问你的最大上下文窗口是多少 token？只返回一个不带单位和解释的整数。"),
                &profile(),
                true,
            ),
            Some("1000000".into())
        );
        assert_eq!(
            exact_model_profile_answer(
                &request("你的知识截止日期是什么时候？只返回月份和年份，不要附加解释。"),
                &profile(),
                true,
            ),
            Some("January 2026".into())
        );
    }

    #[test]
    fn fail_closed_guards_reject_ambiguous_requests() {
        let mut cases = vec![
            serde_json::json!({"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":"What is your maximum context window size in tokens? Reply with just a single integer."}],"system":"You are helpful"}),
            serde_json::json!({"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":"What is your maximum context window size in tokens? Reply with just a single integer."},{"role":"user","content":"again"}]}),
            serde_json::json!({"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":"What is your maximum context window size in tokens? Reply with just a single integer."},{"type":"text","text":"again"}]}]}),
            serde_json::json!({"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":"What is your maximum context window size in tokens? Reply with just a single integer."}],"tools":[]}),
            serde_json::json!({"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":"What is your maximum context window size in tokens? Reply with just a single integer."}],"thinking":{"type":"enabled","budget_tokens":1000}}),
            serde_json::json!({"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":"What is your maximum context window size in tokens? Reply with just a single integer and also say hello."}]}),
        ];
        cases.push(serde_json::json!({"model":"claude-opus-4-8","max_tokens":64,"messages":[{"role":"user","content":"x".repeat(513)}]}));
        for value in cases {
            let request: MessagesRequest = serde_json::from_value(value).unwrap();
            assert_eq!(exact_model_profile_answer(&request, &profile(), true), None);
        }
        assert_eq!(
            exact_model_profile_answer(
                &request(
                    "What is your maximum context window size in tokens? Reply with just a single integer."
                ),
                &profile(),
                false,
            ),
            None
        );
    }

    #[test]
    fn heuristic_or_missing_profile_values_are_not_answers() {
        let store = ModelProfileStore::new_in_memory();
        let unknown = store.resolve("unknown-model");
        assert_eq!(
            exact_model_profile_answer(
                &request("What is your knowledge cutoff date? Reply with just the month and year."),
                &unknown,
                true,
            ),
            None
        );
    }
}
