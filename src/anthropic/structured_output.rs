use super::exact_output::extract_single_json_bounded;
use super::types::OutputFormat;

const MAX_STRUCTURED_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StructuredOutputError {
    UnsupportedFormat(String),
    InvalidJson,
    SchemaViolation,
}

impl std::fmt::Display for StructuredOutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat(format_type) => {
                write!(
                    formatter,
                    "unsupported structured output format: {format_type}"
                )
            }
            Self::InvalidJson => formatter.write_str("output is not exactly one JSON value"),
            Self::SchemaViolation => formatter.write_str("output does not satisfy JSON schema"),
        }
    }
}

impl std::error::Error for StructuredOutputError {}

pub(crate) fn validate_output_json(
    text: &str,
    format: &OutputFormat,
) -> Result<serde_json::Value, StructuredOutputError> {
    if format.format_type != "json_schema" {
        return Err(StructuredOutputError::UnsupportedFormat(
            format.format_type.clone(),
        ));
    }
    let candidate = extract_output_json(text).ok_or(StructuredOutputError::InvalidJson)?;
    let value = serde_json::from_str::<serde_json::Value>(&candidate)
        .map_err(|_| StructuredOutputError::InvalidJson)?;
    let mut candidate = value.clone();
    match super::tool_schema::validate_and_repair(&format.schema, &mut candidate) {
        super::tool_schema::ToolInputOutcome::Valid => Ok(value),
        super::tool_schema::ToolInputOutcome::Repaired { .. }
        | super::tool_schema::ToolInputOutcome::Invalid { .. } => {
            Err(StructuredOutputError::SchemaViolation)
        }
    }
}

pub(crate) fn extract_output_json(text: &str) -> Option<String> {
    if text.len() > MAX_STRUCTURED_OUTPUT_BYTES {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        return serde_json::to_string(&value).ok();
    }
    extract_single_json_bounded(
        text,
        MAX_STRUCTURED_OUTPUT_BYTES,
        MAX_STRUCTURED_OUTPUT_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{extract_single_json_bounded, validate_output_json};
    use crate::anthropic::types::OutputFormat;

    fn format() -> OutputFormat {
        OutputFormat {
            format_type: "json_schema".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "answer": {"type": "integer"},
                    "label": {"type": "string"}
                },
                "required": ["answer", "label"],
                "additionalProperties": false
            }),
        }
    }

    #[test]
    fn accepts_exact_json_value_matching_schema_and_utf8() {
        let value = validate_output_json(r#"{"answer":42,"label":"你好"}"#, &format())
            .expect("valid structured output");
        assert_eq!(value["answer"], 42);
        assert_eq!(value["label"], "你好");
    }

    #[test]
    fn rejects_missing_required_wrong_type_and_additional_property() {
        assert!(validate_output_json(r#"{"answer":42}"#, &format()).is_err());
        assert!(validate_output_json(r#"{"answer":"42","label":"ok"}"#, &format()).is_err());
        assert!(
            validate_output_json(r#"{"answer":42,"label":"ok","unexpected":true}"#, &format(),)
                .is_err()
        );
    }

    #[test]
    fn accepts_markdown_fenced_json() {
        let value =
            validate_output_json("```json\n{\"answer\":42,\"label\":\"ok\"}\n```", &format())
                .expect("fenced JSON should be recovered");
        assert_eq!(value, json!({"answer": 42, "label": "ok"}));
    }

    #[test]
    fn accepts_explanation_around_one_json_value() {
        let value = validate_output_json(
            "Here is the result:\n{\"answer\":42,\"label\":\"ok\"}\nThanks!",
            &format(),
        )
        .expect("the unique JSON value should be recovered");
        assert_eq!(value["answer"], 42);
    }

    #[test]
    fn rejects_multiple_json_values_even_when_each_matches_schema() {
        assert!(
            validate_output_json(
                "first {\"answer\":42,\"label\":\"a\"} second {\"answer\":43,\"label\":\"b\"}",
                &format(),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_extended_constraints_through_the_shared_validator() {
        let format = OutputFormat {
            format_type: "json_schema".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "label": {"type": "string", "minLength": 3},
                    "score": {"type": "number", "minimum": 10},
                    "items": {"type": "array", "minItems": 2}
                },
                "required": ["label", "score", "items"],
                "allOf": [{"properties": {"label": {"pattern": "^[A-Z]+$"}}}]
            }),
        };

        assert!(validate_output_json(r#"{"label":"x","score":9,"items":[]}"#, &format).is_err());
    }

    #[test]
    fn extracted_candidate_still_requires_the_original_schema() {
        assert!(validate_output_json("result: {\"answer\":42}", &format()).is_err());
        assert!(
            validate_output_json("result: {\"answer\":\"42\",\"label\":\"ok\"}", &format(),)
                .is_err()
        );
        assert!(
            validate_output_json(
                "result: {\"answer\":42,\"label\":\"ok\",\"unexpected\":true}",
                &format(),
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_extractor_enforces_text_and_candidate_limits() {
        let text = "prefix {\"a\":1} suffix";
        assert!(extract_single_json_bounded(text, text.len(), 7).is_some());
        assert!(extract_single_json_bounded(text, text.len() - 1, 7).is_none());
        assert!(extract_single_json_bounded(text, text.len(), 6).is_none());
    }

    #[test]
    fn rejects_unsupported_format_type() {
        let mut unsupported = format();
        unsupported.format_type = "text".into();
        assert!(validate_output_json("{}", &unsupported).is_err());
    }
}
