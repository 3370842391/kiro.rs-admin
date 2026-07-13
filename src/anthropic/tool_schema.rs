#[derive(Debug, Clone)]
pub(crate) struct ToolContract {
    pub(crate) client_name: String,
    pub(crate) schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolInputViolation {
    UndeclaredTool,
    MissingRequired(String),
    TypeMismatch { path: String, expected: String },
    ConstMismatch { path: String },
    EnumMismatch { path: String },
    AdditionalProperty(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolInputOutcome {
    Valid,
    Repaired { paths: Vec<String> },
    Invalid { violations: Vec<ToolInputViolation> },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolSchemaError {
    pub(crate) tool_name: String,
    pub(crate) violations: Vec<ToolInputViolation>,
}

impl std::fmt::Display for ToolSchemaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tool {:?} input violates schema: ",
            self.tool_name
        )?;
        for (index, violation) in self.violations.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            match violation {
                ToolInputViolation::UndeclaredTool => formatter.write_str("tool was not declared"),
                ToolInputViolation::MissingRequired(path) => {
                    write!(formatter, "missing required {path}")
                }
                ToolInputViolation::TypeMismatch { path, expected } => {
                    write!(formatter, "{path} expected {expected}")
                }
                ToolInputViolation::ConstMismatch { path } => {
                    write!(formatter, "{path} does not match const")
                }
                ToolInputViolation::EnumMismatch { path } => {
                    write!(formatter, "{path} is outside enum")
                }
                ToolInputViolation::AdditionalProperty(path) => {
                    write!(formatter, "unexpected property {path}")
                }
            }?;
        }
        Ok(())
    }
}

impl std::error::Error for ToolSchemaError {}

pub(crate) fn validate_tool_use_blocks(
    contracts: &std::collections::HashMap<String, ToolContract>,
    blocks: &mut [serde_json::Value],
) -> Result<Vec<String>, ToolSchemaError> {
    let mut candidate_blocks = blocks.to_vec();
    let mut repaired_paths = Vec::new();
    for block in &mut candidate_blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = block
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(contract) = contracts.get(&name) else {
            return Err(ToolSchemaError {
                tool_name: name,
                violations: vec![ToolInputViolation::UndeclaredTool],
            });
        };
        let mut candidate = block
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match validate_and_repair(&contract.schema, &mut candidate) {
            ToolInputOutcome::Valid => {}
            ToolInputOutcome::Repaired { paths } => {
                block["input"] = candidate;
                repaired_paths.extend(
                    paths
                        .into_iter()
                        .map(|path| format!("{}:{path}", contract.client_name)),
                );
            }
            ToolInputOutcome::Invalid { violations } => {
                return Err(ToolSchemaError {
                    tool_name: contract.client_name.clone(),
                    violations,
                });
            }
        }
    }
    blocks.clone_from_slice(&candidate_blocks);
    Ok(repaired_paths)
}

pub(crate) fn validate_and_repair(
    schema: &serde_json::Value,
    input: &mut serde_json::Value,
) -> ToolInputOutcome {
    let mut candidate = input.clone();
    let mut repairs = Vec::new();
    let mut violations = Vec::new();
    validate_value(
        schema,
        &mut candidate,
        "$",
        false,
        &mut repairs,
        &mut violations,
    );

    if !violations.is_empty() {
        return ToolInputOutcome::Invalid { violations };
    }
    if repairs.is_empty() {
        ToolInputOutcome::Valid
    } else {
        *input = candidate;
        ToolInputOutcome::Repaired { paths: repairs }
    }
}

fn validate_value(
    schema: &serde_json::Value,
    value: &mut serde_json::Value,
    path: &str,
    required_property: bool,
    repairs: &mut Vec<String>,
    violations: &mut Vec<ToolInputViolation>,
) {
    repair_or_validate_fixed_value(schema, value, path, required_property, repairs, violations);

    let Some(expected_type) = schema.get("type") else {
        validate_composite(schema, value, path, repairs, violations);
        return;
    };
    if !matches_declared_type(expected_type, value) {
        violations.push(ToolInputViolation::TypeMismatch {
            path: path.to_string(),
            expected: display_declared_type(expected_type),
        });
        return;
    }

    validate_composite(schema, value, path, repairs, violations);
}

fn validate_composite(
    schema: &serde_json::Value,
    value: &mut serde_json::Value,
    path: &str,
    repairs: &mut Vec<String>,
    violations: &mut Vec<ToolInputViolation>,
) {
    if let Some(object) = value.as_object_mut() {
        validate_object(schema, object, path, repairs, violations);
    } else if let Some(array) = value.as_array_mut()
        && let Some(items) = schema.get("items")
    {
        for (index, item) in array.iter_mut().enumerate() {
            validate_value(
                items,
                item,
                &format!("{path}[{index}]"),
                false,
                repairs,
                violations,
            );
        }
    }
}

fn validate_object(
    schema: &serde_json::Value,
    object: &mut serde_json::Map<String, serde_json::Value>,
    path: &str,
    repairs: &mut Vec<String>,
    violations: &mut Vec<ToolInputViolation>,
) {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object);
    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();

    if let Some(properties) = properties {
        for (name, property_schema) in properties {
            let child_path = property_path(path, name);
            let is_required = required.contains(name.as_str());
            if let Some(child) = object.get_mut(name) {
                validate_value(
                    property_schema,
                    child,
                    &child_path,
                    is_required,
                    repairs,
                    violations,
                );
            } else if is_required {
                if let Some(fixed) = deterministic_fixed_value(property_schema) {
                    object.insert(name.clone(), fixed);
                    repairs.push(child_path.clone());
                    let child = object.get_mut(name).expect("inserted required fixed value");
                    validate_value(
                        property_schema,
                        child,
                        &child_path,
                        true,
                        repairs,
                        violations,
                    );
                } else {
                    violations.push(ToolInputViolation::MissingRequired(child_path));
                }
            }
        }
    } else {
        for name in required {
            if !object.contains_key(name) {
                violations.push(ToolInputViolation::MissingRequired(property_path(
                    path, name,
                )));
            }
        }
    }

    let additional = schema.get("additionalProperties");
    let property_names: std::collections::HashSet<&str> = properties
        .into_iter()
        .flat_map(|properties| properties.keys().map(String::as_str))
        .collect();
    for (name, value) in object.iter_mut() {
        if property_names.contains(name.as_str()) {
            continue;
        }
        let child_path = property_path(path, name);
        match additional {
            Some(serde_json::Value::Bool(false)) => {
                violations.push(ToolInputViolation::AdditionalProperty(child_path));
            }
            Some(additional_schema @ serde_json::Value::Object(_)) => validate_value(
                additional_schema,
                value,
                &child_path,
                false,
                repairs,
                violations,
            ),
            _ => {}
        }
    }
}

fn repair_or_validate_fixed_value(
    schema: &serde_json::Value,
    value: &mut serde_json::Value,
    path: &str,
    required_property: bool,
    repairs: &mut Vec<String>,
    violations: &mut Vec<ToolInputViolation>,
) {
    if let Some(expected) = schema.get("const")
        && value != expected
        && required_property
    {
        *value = expected.clone();
        repairs.push(path.to_string());
    }

    if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !values.iter().any(|expected| expected == value)
        && required_property
        && values.len() == 1
    {
        *value = values[0].clone();
        repairs.push(path.to_string());
    }

    if schema
        .get("const")
        .is_some_and(|expected| value != expected)
    {
        violations.push(ToolInputViolation::ConstMismatch {
            path: path.to_string(),
        });
    }
    if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !values.iter().any(|expected| expected == value)
    {
        violations.push(ToolInputViolation::EnumMismatch {
            path: path.to_string(),
        });
    }
}

fn deterministic_fixed_value(schema: &serde_json::Value) -> Option<serde_json::Value> {
    schema.get("const").cloned().or_else(|| {
        let values = schema.get("enum")?.as_array()?;
        (values.len() == 1).then(|| values[0].clone())
    })
}

fn matches_declared_type(declared: &serde_json::Value, value: &serde_json::Value) -> bool {
    match declared {
        serde_json::Value::String(kind) => matches_type(kind, value),
        serde_json::Value::Array(kinds) => kinds
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|kind| matches_type(kind, value)),
        _ => true,
    }
}

fn matches_type(kind: &str, value: &serde_json::Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn display_declared_type(declared: &serde_json::Value) -> String {
    match declared {
        serde_json::Value::String(kind) => kind.clone(),
        serde_json::Value::Array(kinds) => kinds
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" | "),
        _ => "supported JSON value".to_string(),
    }
}

fn property_path(parent: &str, name: &str) -> String {
    if name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        format!("{parent}.{name}")
    } else {
        format!(
            "{parent}[{}]",
            serde_json::to_string(name).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_input_that_satisfies_supported_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "days": {"type": "integer"}
            },
            "required": ["city"],
            "additionalProperties": false
        });
        let mut input = serde_json::json!({"city": "Paris", "days": 3});

        assert_eq!(
            validate_and_repair(&schema, &mut input),
            ToolInputOutcome::Valid
        );
    }

    #[test]
    fn repairs_only_required_const_and_single_enum_values_recursively() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "unit": {"type": "string", "enum": ["celsius"]},
                "meta": {
                    "type": "object",
                    "properties": {"nonce": {"type": "string", "const": "fixed-42"}},
                    "required": ["nonce"],
                    "additionalProperties": false
                },
                "rows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"kind": {"type": "string", "const": "weather"}},
                        "required": ["kind"]
                    }
                }
            },
            "required": ["unit", "meta", "rows"],
            "additionalProperties": false
        });
        let mut input = serde_json::json!({
            "unit": "fahrenheit",
            "meta": {},
            "rows": [{"kind": "wrong"}, {}]
        });

        let outcome = validate_and_repair(&schema, &mut input);

        assert_eq!(
            outcome,
            ToolInputOutcome::Repaired {
                paths: vec![
                    "$.meta.nonce".to_string(),
                    "$.rows[0].kind".to_string(),
                    "$.rows[1].kind".to_string(),
                    "$.unit".to_string(),
                ]
            }
        );
        assert_eq!(input["unit"], "celsius");
        assert_eq!(input["meta"]["nonce"], "fixed-42");
        assert_eq!(input["rows"][0]["kind"], "weather");
        assert_eq!(input["rows"][1]["kind"], "weather");
    }

    #[test]
    fn never_guesses_missing_non_fixed_required_value() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        });
        let mut input = serde_json::json!({});

        assert_eq!(
            validate_and_repair(&schema, &mut input),
            ToolInputOutcome::Invalid {
                violations: vec![ToolInputViolation::MissingRequired("$.city".to_string())]
            }
        );
        assert_eq!(input, serde_json::json!({}));
    }

    #[test]
    fn reports_type_enum_and_additional_property_violations_without_coercion() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer"},
                "mode": {"type": "string", "enum": ["fast", "safe"]}
            },
            "required": ["count", "mode"],
            "additionalProperties": false
        });
        let original = serde_json::json!({"count": "3", "mode": "other", "extra": true});
        let mut input = original.clone();

        assert_eq!(
            validate_and_repair(&schema, &mut input),
            ToolInputOutcome::Invalid {
                violations: vec![
                    ToolInputViolation::TypeMismatch {
                        path: "$.count".to_string(),
                        expected: "integer".to_string(),
                    },
                    ToolInputViolation::EnumMismatch {
                        path: "$.mode".to_string()
                    },
                    ToolInputViolation::AdditionalProperty("$.extra".to_string()),
                ]
            }
        );
        assert_eq!(input, original);
    }

    #[test]
    fn reports_const_mismatch_for_non_required_fixed_property_without_repairing_it() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"optional_tag": {"type": "string", "const": "fixed"}},
            "required": []
        });
        let mut input = serde_json::json!({"optional_tag": "customer-value"});

        assert_eq!(
            validate_and_repair(&schema, &mut input),
            ToolInputOutcome::Invalid {
                violations: vec![ToolInputViolation::ConstMismatch {
                    path: "$.optional_tag".to_string()
                }]
            }
        );
        assert_eq!(input["optional_tag"], "customer-value");
    }

    #[test]
    fn validates_and_repairs_anthropic_tool_blocks_before_delivery() {
        let contracts = std::collections::HashMap::from([(
            "get_weather".to_string(),
            ToolContract {
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
        )]);
        let mut blocks = vec![serde_json::json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "get_weather",
            "input": {"city": "Paris", "unit": "wrong"}
        })];

        let repaired = validate_tool_use_blocks(&contracts, &mut blocks).unwrap();

        assert_eq!(repaired, vec!["get_weather:$.unit"]);
        assert_eq!(blocks[0]["input"]["unit"], "celsius");
    }

    #[test]
    fn invalid_tool_block_is_not_mutated_and_error_does_not_echo_values() {
        let contracts = std::collections::HashMap::from([(
            "get_weather".to_string(),
            ToolContract {
                client_name: "get_weather".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                    "additionalProperties": false
                }),
            },
        )]);
        let original = serde_json::json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "get_weather",
            "input": {"city": 7, "secret_customer_value": "do-not-echo"}
        });
        let mut blocks = vec![original.clone()];

        let error = validate_tool_use_blocks(&contracts, &mut blocks).unwrap_err();

        assert_eq!(blocks[0], original);
        assert_eq!(error.tool_name, "get_weather");
        assert!(error.to_string().contains("$.city"));
        assert!(error.to_string().contains("$.secret_customer_value"));
        assert!(!error.to_string().contains("do-not-echo"));
    }

    #[test]
    fn conflicting_required_const_and_single_enum_fails_closed_after_repair() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "const": "const-value",
                    "enum": ["enum-value"]
                }
            },
            "required": ["mode"]
        });
        let original = serde_json::json!({"mode": "upstream-value"});
        let mut input = original.clone();

        assert!(matches!(
            validate_and_repair(&schema, &mut input),
            ToolInputOutcome::Invalid { .. }
        ));
        assert_eq!(input, original, "冲突契约不得留下半修复参数");
    }

    #[test]
    fn undeclared_tool_is_rejected_when_request_has_contracts() {
        let contracts = std::collections::HashMap::from([(
            "get_weather".to_string(),
            ToolContract {
                client_name: "get_weather".to_string(),
                schema: serde_json::json!({"type": "object"}),
            },
        )]);
        let original = serde_json::json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "delete_everything",
            "input": {}
        });
        let mut blocks = vec![original.clone()];

        let error = validate_tool_use_blocks(&contracts, &mut blocks).unwrap_err();

        assert_eq!(error.tool_name, "delete_everything");
        assert_eq!(blocks, vec![original]);
    }

    #[test]
    fn unrequested_tool_is_rejected_when_request_has_no_contracts() {
        let original = serde_json::json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "delete_everything",
            "input": {}
        });
        let mut blocks = vec![original.clone()];

        let error =
            validate_tool_use_blocks(&std::collections::HashMap::new(), &mut blocks).unwrap_err();

        assert_eq!(error.tool_name, "delete_everything");
        assert_eq!(blocks, vec![original]);
    }
}
