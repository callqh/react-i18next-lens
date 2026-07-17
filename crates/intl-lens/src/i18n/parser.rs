use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Strict i18next JSON v4 resource parser.
pub struct TranslationParser;

impl TranslationParser {
    pub fn parse_file(path: &Path) -> Result<HashMap<String, String>> {
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            bail!(
                "unsupported translation resource '{}': React i18next Lens accepts JSON only",
                path.display()
            );
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read translation resource {}", path.display()))?;
        Self::parse_json(&content)
            .with_context(|| format!("invalid i18next JSON resource {}", path.display()))
    }

    pub fn parse_json(content: &str) -> Result<HashMap<String, String>> {
        let value: Value = serde_json::from_str(content)?;
        let Value::Object(_) = value else {
            bail!("an i18next JSON resource must have an object at its root");
        };

        let mut result = HashMap::new();
        flatten_json(&value, String::new(), &mut result);
        Ok(result)
    }
}

fn flatten_json(value: &Value, prefix: String, result: &mut HashMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_json(value, path, result);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                flatten_json(value, format!("{prefix}.{index}"), result);
            }
        }
        Value::String(value) => {
            result.insert(prefix, value.clone());
        }
        Value::Number(value) => {
            result.insert(prefix, value.to_string());
        }
        Value::Bool(value) => {
            result.insert(prefix, value.to_string());
        }
        Value::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_flat_plural_and_array_values() {
        let resource = r#"{
            "buttons": { "save": "Save" },
            "flat.key": "Flat",
            "item_one": "One item",
            "item_other": "{{count}} items",
            "steps": ["First", "Second"]
        }"#;

        let parsed = TranslationParser::parse_json(resource).unwrap();
        assert_eq!(parsed.get("buttons.save").map(String::as_str), Some("Save"));
        assert_eq!(parsed.get("flat.key").map(String::as_str), Some("Flat"));
        assert_eq!(parsed.get("item_one").map(String::as_str), Some("One item"));
        assert_eq!(parsed.get("steps.1").map(String::as_str), Some("Second"));
    }

    #[test]
    fn rejects_non_object_roots() {
        assert!(TranslationParser::parse_json(r#"["not", "a", "resource"]"#).is_err());
    }
}
