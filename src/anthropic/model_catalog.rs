use std::collections::HashMap;

use crate::{anthropic::types::Model, kiro::model::available_models::UpstreamModel};

const PUBLIC_CREATED_AT: i64 = 1_781_481_600;
const PUBLIC_MAX_OUTPUT_TOKENS: i32 = 64_000;
const CLAUDE_FAMILIES: &[&str] = &["opus", "sonnet", "haiku", "fable", "mythos"];

#[derive(Clone)]
struct PublicCandidate {
    canonical_sort_id: String,
    rank: u8,
    id: String,
    display_name: String,
    owned_by: String,
}

fn all_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn claude_version_ids(id: &str) -> Option<(String, String, bool)> {
    let lower = id.to_ascii_lowercase();
    let (base, thinking) = lower
        .strip_suffix("-thinking")
        .map(|base| (base, true))
        .unwrap_or((&lower, false));
    let parts: Vec<_> = base.split('-').collect();
    if parts.len() < 3 || parts[0] != "claude" || !CLAUDE_FAMILIES.contains(&parts[1]) {
        return None;
    }

    let (major, minor) = match parts.as_slice() {
        ["claude", _, version] if all_digits(version) => (*version, None),
        ["claude", _, version] => {
            let (major, minor) = version.split_once('.')?;
            (all_digits(major) && all_digits(minor)).then_some((major, Some(minor)))?
        }
        ["claude", _, major, minor] if all_digits(major) && all_digits(minor) => {
            (*major, Some(*minor))
        }
        _ => return None,
    };
    let prefix = format!("claude-{}", parts[1]);
    let canonical = minor
        .map(|minor| format!("{prefix}-{major}.{minor}"))
        .unwrap_or_else(|| format!("{prefix}-{major}"));
    let hyphen = minor
        .map(|minor| format!("{prefix}-{major}-{minor}"))
        .unwrap_or_else(|| canonical.clone());
    Some((canonical, hyphen, thinking))
}

fn owner_for(id: &str) -> &'static str {
    match id {
        value if value.starts_with("claude-") => "anthropic",
        value if value.starts_with("gpt-") => "openai",
        value if value.starts_with("deepseek-") => "deepseek",
        value if value.starts_with("minimax-") => "minimax",
        value if value.starts_with("glm-") => "zhipu",
        value if value.starts_with("qwen") => "qwen",
        _ => "kiro",
    }
}

fn candidate(
    canonical_sort_id: &str,
    rank: u8,
    id: String,
    base_name: &str,
    thinking: bool,
) -> PublicCandidate {
    PublicCandidate {
        canonical_sort_id: canonical_sort_id.to_string(),
        rank,
        owned_by: owner_for(&id).to_string(),
        display_name: if thinking {
            format!("{base_name} (Thinking)")
        } else {
            base_name.to_string()
        },
        id,
    }
}

pub fn public_models(upstream: Vec<UpstreamModel>) -> Vec<Model> {
    let mut candidates = Vec::new();
    for model in upstream {
        let id = model.model_id.trim();
        if id.is_empty() {
            continue;
        }
        let display_name = model
            .model_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(id);
        if let Some((canonical, hyphen, already_thinking)) = claude_version_ids(id) {
            if already_thinking {
                let fallback_name;
                let base_name = if model.model_name.is_none() {
                    fallback_name = canonical.clone();
                    fallback_name.as_str()
                } else {
                    display_name.trim_end_matches(" (Thinking)")
                };
                candidates.push(candidate(
                    &canonical,
                    2,
                    format!("{canonical}-thinking"),
                    base_name,
                    true,
                ));
                if hyphen != canonical {
                    candidates.push(candidate(
                        &canonical,
                        3,
                        format!("{hyphen}-thinking"),
                        base_name,
                        true,
                    ));
                }
            } else {
                candidates.push(candidate(
                    &canonical,
                    0,
                    canonical.clone(),
                    display_name,
                    false,
                ));
                if hyphen != canonical {
                    candidates.push(candidate(
                        &canonical,
                        1,
                        hyphen.clone(),
                        display_name,
                        false,
                    ));
                }
                candidates.push(candidate(
                    &canonical,
                    2,
                    format!("{canonical}-thinking"),
                    display_name,
                    true,
                ));
                if hyphen != canonical {
                    candidates.push(candidate(
                        &canonical,
                        3,
                        format!("{hyphen}-thinking"),
                        display_name,
                        true,
                    ));
                }
            }
        } else {
            candidates.push(candidate(id, 0, id.to_string(), display_name, false));
        }
    }

    let mut unique: HashMap<String, PublicCandidate> = HashMap::new();
    for candidate in candidates {
        unique.entry(candidate.id.clone()).or_insert(candidate);
    }
    let mut unique: Vec<_> = unique.into_values().collect();
    unique.sort_by(|left, right| {
        left.canonical_sort_id
            .cmp(&right.canonical_sort_id)
            .then(left.rank.cmp(&right.rank))
            .then(left.id.cmp(&right.id))
    });
    unique
        .into_iter()
        .map(|candidate| Model {
            id: candidate.id,
            object: "model".to_string(),
            created: PUBLIC_CREATED_AT,
            owned_by: candidate.owned_by,
            display_name: candidate.display_name,
            model_type: "chat".to_string(),
            max_tokens: PUBLIC_MAX_OUTPUT_TOKENS,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::available_models::{TokenLimits, UpstreamModel};

    fn upstream(id: &str, name: Option<&str>) -> UpstreamModel {
        UpstreamModel {
            model_id: id.to_string(),
            model_name: name.map(str::to_string),
            description: None,
            token_limits: None,
        }
    }

    #[test]
    fn claude_dot_version_generates_four_compatible_entries() {
        let ids: Vec<_> = public_models(vec![upstream("claude-opus-4.8", Some("Claude Opus 4.8"))])
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "claude-opus-4.8",
                "claude-opus-4-8",
                "claude-opus-4.8-thinking",
                "claude-opus-4-8-thinking",
            ]
        );
    }

    #[test]
    fn claude_hyphen_input_is_normalized_without_duplicates() {
        let models = public_models(vec![
            upstream("claude-opus-4-8", None),
            upstream("claude-opus-4.8", None),
        ]);
        let unique: std::collections::HashSet<_> =
            models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(models.len(), 4);
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn existing_thinking_model_does_not_get_double_suffix() {
        let ids: Vec<_> = public_models(vec![upstream("claude-opus-4.8-thinking", None)])
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(
            ids,
            vec!["claude-opus-4.8-thinking", "claude-opus-4-8-thinking"]
        );
    }

    #[test]
    fn gpt_and_unknown_models_keep_upstream_ids() {
        let models = public_models(vec![
            upstream("gpt-5.6-sol", Some("GPT Sol")),
            upstream("vendor-new-model", None),
        ]);
        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert_eq!(models[0].owned_by, "openai");
        assert_eq!(models[0].display_name, "GPT Sol");
        assert_eq!(models[1].id, "vendor-new-model");
        assert_eq!(models[1].owned_by, "kiro");
    }

    #[test]
    fn public_max_tokens_never_uses_upstream_input_limit() {
        let mut input = upstream("gpt-5.6-sol", None);
        input.token_limits = Some(TokenLimits {
            max_input_tokens: Some(1_000_000),
        });
        assert_eq!(public_models(vec![input])[0].max_tokens, 64_000);
    }

    #[test]
    fn unsupported_claude_family_is_not_given_aliases() {
        let models = public_models(vec![upstream("claude-unknown-4.8", None)]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-unknown-4.8");
    }
}
