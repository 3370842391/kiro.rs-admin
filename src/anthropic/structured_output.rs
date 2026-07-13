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
    if text.len() > MAX_STRUCTURED_OUTPUT_BYTES {
        return Err(StructuredOutputError::InvalidJson);
    }

    let value = serde_json::from_str::<serde_json::Value>(text)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::validate_output_json;
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
    fn rejects_markdown_prose_and_multiple_json_values() {
        assert!(
            validate_output_json("```json\n{\"answer\":42,\"label\":\"ok\"}\n```", &format())
                .is_err()
        );
        assert!(
            validate_output_json("result: {\"answer\":42,\"label\":\"ok\"}", &format()).is_err()
        );
        assert!(
            validate_output_json(
                "{\"answer\":42,\"label\":\"a\"} {\"answer\":43,\"label\":\"b\"}",
                &format(),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unsupported_format_type() {
        let mut unsupported = format();
        unsupported.format_type = "text".into();
        assert!(validate_output_json("{}", &unsupported).is_err());
    }
}
