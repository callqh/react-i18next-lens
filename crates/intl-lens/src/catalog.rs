use std::collections::HashMap;
use std::path::{Path, PathBuf};

use globset::Glob;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::configuration::WorkspaceConfig;
use crate::domain::{ByteSpan, KeyPath, Namespace, TranslationKey};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageValue {
    String(String),
    Number(serde_json::Number),
    Boolean(bool),
    Object(serde_json::Map<String, serde_json::Value>),
    Array(Vec<serde_json::Value>),
}

impl MessageValue {
    pub fn display(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Number(value) => value.to_string(),
            Self::Boolean(value) => value.to_string(),
            Self::Object(value) => serde_json::to_string(value).unwrap_or_default(),
            Self::Array(value) => serde_json::to_string(value).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub key: TranslationKey,
    pub locale: String,
    pub value: MessageValue,
    pub file: PathBuf,
    pub key_span: ByteSpan,
    pub value_span: ByteSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogDiagnostic {
    pub file: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct TranslationCatalog {
    entries: HashMap<(String, TranslationKey), CatalogEntry>,
    files: HashMap<PathBuf, String>,
    diagnostics: Vec<CatalogDiagnostic>,
    resource_locales: std::collections::HashSet<String>,
}

impl TranslationCatalog {
    pub fn load(root: &Path, config: &WorkspaceConfig) -> Self {
        let mut catalog = Self::default();
        for resource in discover_resources(root, config) {
            let content = match std::fs::read_to_string(&resource.file) {
                Ok(content) => content,
                Err(error) => {
                    catalog.diagnostics.push(CatalogDiagnostic {
                        file: resource.file,
                        message: format!("failed to read translation resource: {error}"),
                    });
                    continue;
                }
            };

            match parse_resource(
                &content,
                &resource.locale,
                &resource.namespace,
                &resource.file,
                config.key_separator,
            ) {
                Ok(entries) => {
                    catalog.resource_locales.insert(resource.locale.clone());
                    catalog.files.insert(resource.file.clone(), content);
                    for entry in entries {
                        catalog
                            .entries
                            .insert((entry.locale.clone(), entry.key.clone()), entry);
                    }
                }
                Err(message) => catalog.diagnostics.push(CatalogDiagnostic {
                    file: resource.file,
                    message,
                }),
            }
        }
        catalog
    }

    pub fn get(&self, locale: &str, key: &TranslationKey) -> Option<&CatalogEntry> {
        self.entries.get(&(locale.to_string(), key.clone()))
    }

    pub fn translations(&self, key: &TranslationKey) -> Vec<&CatalogEntry> {
        let mut entries = self
            .entries
            .values()
            .filter(|entry| &entry.key == key)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.locale.cmp(&right.locale));
        entries
    }

    pub fn entries(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.values()
    }

    pub fn source(&self, file: &Path) -> Option<&str> {
        self.files.get(file).map(String::as_str)
    }

    pub fn diagnostics(&self) -> &[CatalogDiagnostic] {
        &self.diagnostics
    }

    pub fn has_resource_for_locale(&self, locale: &str) -> bool {
        self.resource_locales.contains(locale)
    }
}

struct ResourceMatch {
    locale: String,
    namespace: String,
    file: PathBuf,
}

fn discover_resources(root: &Path, config: &WorkspaceConfig) -> Vec<ResourceMatch> {
    let mut resources = Vec::new();
    for pattern in &config.resource_patterns {
        for locale in &config.locales {
            let locale_pattern = pattern.replace("{locale}", locale);
            if locale_pattern.contains("{namespace}") {
                let glob_pattern = locale_pattern.replace("{namespace}", "*");
                let Ok(matcher) = Glob::new(&glob_pattern).map(|glob| glob.compile_matcher())
                else {
                    continue;
                };
                for entry in WalkDir::new(root)
                    .into_iter()
                    .filter_entry(|entry| !is_ignored(entry.path()))
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_file())
                {
                    let Ok(relative) = entry.path().strip_prefix(root) else {
                        continue;
                    };
                    if matcher.is_match(relative) {
                        let namespace = namespace_from_pattern(&locale_pattern, relative)
                            .unwrap_or_else(|| config.default_namespace.clone());
                        resources.push(ResourceMatch {
                            locale: locale.clone(),
                            namespace,
                            file: entry.into_path(),
                        });
                    }
                }
            } else {
                let file = root.join(&locale_pattern);
                if file.is_file() {
                    resources.push(ResourceMatch {
                        locale: locale.clone(),
                        namespace: config.default_namespace.clone(),
                        file,
                    });
                }
            }
        }
    }
    resources.sort_by(|left, right| left.file.cmp(&right.file));
    resources.dedup_by(|left, right| left.locale == right.locale && left.file == right.file);
    resources
}

fn namespace_from_pattern(pattern: &str, path: &Path) -> Option<String> {
    let path = path.to_string_lossy().replace('\\', "/");
    let (prefix, suffix) = pattern.split_once("{namespace}")?;
    let namespace = path.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!namespace.is_empty() && !namespace.contains('/')).then(|| namespace.to_string())
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("node_modules" | ".git" | "target" | ".next")
        )
    })
}

fn parse_resource(
    source: &str,
    locale: &str,
    namespace: &str,
    file: &Path,
    key_separator: Option<char>,
) -> Result<Vec<CatalogEntry>, String> {
    serde_json::from_str::<serde_json::Value>(source)
        .map_err(|error| format!("invalid i18next JSON resource: {error}"))?;
    let mut parser = SpannedJsonParser::new(source, locale, namespace, file, key_separator);
    parser.parse()
}

struct SpannedJsonParser<'a> {
    source: &'a str,
    cursor: usize,
    locale: &'a str,
    namespace: &'a str,
    file: &'a Path,
    key_separator: Option<char>,
    entries: Vec<CatalogEntry>,
}

impl<'a> SpannedJsonParser<'a> {
    fn new(
        source: &'a str,
        locale: &'a str,
        namespace: &'a str,
        file: &'a Path,
        key_separator: Option<char>,
    ) -> Self {
        Self {
            source,
            cursor: 0,
            locale,
            namespace,
            file,
            key_separator,
            entries: Vec::new(),
        }
    }

    fn parse(&mut self) -> Result<Vec<CatalogEntry>, String> {
        self.skip_whitespace();
        self.parse_object(Vec::new())?;
        self.skip_whitespace();
        if self.cursor != self.source.len() {
            return Err("unexpected content after JSON resource".to_string());
        }
        Ok(std::mem::take(&mut self.entries))
    }

    fn parse_object(&mut self, path: Vec<String>) -> Result<ByteSpan, String> {
        let start = self.cursor;
        self.expect(b'{')?;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(ByteSpan::new(start as u32, self.cursor as u32));
        }

        loop {
            self.skip_whitespace();
            let (property, key_span) = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let mut child_path = path.clone();
            if let Some(separator) = self.key_separator {
                child_path.extend(property.split(separator).map(str::to_string));
            } else {
                child_path.push(property);
            }
            self.parse_value(child_path, key_span)?;
            self.skip_whitespace();
            if self.consume(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(ByteSpan::new(start as u32, self.cursor as u32))
    }

    fn parse_array(&mut self, path: Vec<String>, key_span: ByteSpan) -> Result<ByteSpan, String> {
        let start = self.cursor;
        self.expect(b'[')?;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(ByteSpan::new(start as u32, self.cursor as u32));
        }
        let mut index = 0;
        loop {
            let mut child_path = path.clone();
            child_path.push(index.to_string());
            self.parse_value(child_path, key_span)?;
            index += 1;
            self.skip_whitespace();
            if self.consume(b']') {
                break;
            }
            self.expect(b',')?;
            self.skip_whitespace();
        }
        Ok(ByteSpan::new(start as u32, self.cursor as u32))
    }

    fn parse_value(&mut self, path: Vec<String>, key_span: ByteSpan) -> Result<ByteSpan, String> {
        match self.peek() {
            Some(b'{') => {
                let start = self.cursor;
                let span = self.parse_object(path.clone())?;
                let serde_json::Value::Object(value) =
                    serde_json::from_str(&self.source[start..self.cursor])
                        .map_err(|error| error.to_string())?
                else {
                    unreachable!("object parser must produce an object")
                };
                self.push_entry(path, key_span, span, MessageValue::Object(value))?;
                Ok(span)
            }
            Some(b'[') => {
                let start = self.cursor;
                let entry_start = self.entries.len();
                let span = self.parse_array(path.clone(), key_span)?;
                self.entries.truncate(entry_start);
                let serde_json::Value::Array(value) =
                    serde_json::from_str(&self.source[start..self.cursor])
                        .map_err(|error| error.to_string())?
                else {
                    unreachable!("array parser must produce an array")
                };
                self.push_entry(path, key_span, span, MessageValue::Array(value))?;
                Ok(span)
            }
            Some(b'"') => {
                let start = self.cursor;
                let (value, _) = self.parse_string()?;
                let span = ByteSpan::new(start as u32, self.cursor as u32);
                self.push_entry(path, key_span, span, MessageValue::String(value))?;
                Ok(span)
            }
            Some(b't') | Some(b'f') => {
                let start = self.cursor;
                let value = if self.source[self.cursor..].starts_with("true") {
                    self.cursor += 4;
                    true
                } else if self.source[self.cursor..].starts_with("false") {
                    self.cursor += 5;
                    false
                } else {
                    return Err(format!("invalid boolean at byte {}", self.cursor));
                };
                let span = ByteSpan::new(start as u32, self.cursor as u32);
                self.push_entry(path, key_span, span, MessageValue::Boolean(value))?;
                Ok(span)
            }
            Some(b'n') if self.source[self.cursor..].starts_with("null") => {
                let start = self.cursor;
                self.cursor += 4;
                Ok(ByteSpan::new(start as u32, self.cursor as u32))
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.cursor;
                while matches!(
                    self.peek(),
                    Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                ) {
                    self.cursor += 1;
                }
                let raw = &self.source[start..self.cursor];
                let number = raw
                    .parse::<serde_json::Number>()
                    .map_err(|error| format!("invalid number at byte {start}: {error}"))?;
                let span = ByteSpan::new(start as u32, self.cursor as u32);
                self.push_entry(path, key_span, span, MessageValue::Number(number))?;
                Ok(span)
            }
            _ => Err(format!("invalid JSON value at byte {}", self.cursor)),
        }
    }

    fn push_entry(
        &mut self,
        path: Vec<String>,
        key_span: ByteSpan,
        value_span: ByteSpan,
        value: MessageValue,
    ) -> Result<(), String> {
        let path =
            KeyPath::new(path.join(".")).ok_or_else(|| "empty translation key".to_string())?;
        let namespace = Namespace::new(self.namespace)
            .ok_or_else(|| "empty translation namespace".to_string())?;
        self.entries.push(CatalogEntry {
            key: TranslationKey { namespace, path },
            locale: self.locale.to_string(),
            value,
            file: self.file.to_path_buf(),
            key_span,
            value_span,
        });
        Ok(())
    }

    fn parse_string(&mut self) -> Result<(String, ByteSpan), String> {
        let start = self.cursor;
        self.expect(b'"')?;
        let content_start = self.cursor;
        let mut escaped = false;
        while let Some(byte) = self.peek() {
            if !escaped && byte == b'"' {
                let content_end = self.cursor;
                self.cursor += 1;
                let raw = &self.source[start..self.cursor];
                let value = serde_json::from_str::<String>(raw)
                    .map_err(|error| format!("invalid JSON string at byte {start}: {error}"))?;
                return Ok((
                    value,
                    ByteSpan::new(content_start as u32, content_end as u32),
                ));
            }
            escaped = !escaped && byte == b'\\';
            self.cursor += 1;
        }
        Err(format!("unterminated JSON string at byte {start}"))
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.cursor += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(format!(
                "expected '{}' at byte {}",
                expected as char, self.cursor
            ))
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.cursor).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn preserves_nested_and_flat_key_spans() {
        let source = r#"{
  "buttons": { "save": "Save" },
  "flat.key": "Flat"
}"#;
        let entries = parse_resource(
            source,
            "en",
            "common",
            Path::new("locales/en/common.json"),
            Some('.'),
        )
        .unwrap();

        let save = entries
            .iter()
            .find(|entry| entry.key.canonical() == "common:buttons.save")
            .unwrap();
        assert_eq!(
            &source[save.key_span.start as usize..save.key_span.end as usize],
            "save"
        );
        assert_eq!(
            &source[save.value_span.start as usize..save.value_span.end as usize],
            "\"Save\""
        );
        assert!(entries
            .iter()
            .any(|entry| entry.key.canonical() == "common:flat.key"));
    }

    #[test]
    fn loads_explicit_locales_and_namespace_templates() {
        let root = workspace("catalog");
        fs::create_dir_all(root.join("locales/en")).unwrap();
        fs::create_dir_all(root.join("locales/ja")).unwrap();
        fs::write(root.join("locales/en/common.json"), r#"{"save":"Save"}"#).unwrap();
        fs::write(root.join("locales/ja/common.json"), r#"{"save":"保存"}"#).unwrap();
        fs::write(
            root.join("locales/fr/common.json"),
            r#"{"save":"Enregistrer"}"#,
        )
        .ok();
        let config = WorkspaceConfig {
            source_locale: "en".to_string(),
            locales: vec!["en".to_string(), "ja".to_string()],
            resource_patterns: vec!["locales/{locale}/{namespace}.json".to_string()],
            default_namespace: "common".to_string(),
            fallback_locales: Vec::new(),
            key_separator: Some('.'),
            namespace_separator: Some(':'),
        };

        let catalog = TranslationCatalog::load(&root, &config);
        let key =
            TranslationKey::from_source("common:save", None, None, "common", Some(':'), Some('.'))
                .unwrap();
        assert_eq!(catalog.translations(&key).len(), 2);
        assert_eq!(catalog.get("ja", &key).unwrap().value.display(), "保存");
        assert!(catalog.diagnostics().is_empty());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn preserves_structured_object_and_array_values() {
        let source = r#"{"menu":{"save":"Save"},"steps":["First","Second"]}"#;
        let entries =
            parse_resource(source, "en", "common", Path::new("common.json"), Some('.')).unwrap();
        assert!(entries.iter().any(|entry| {
            entry.key.canonical() == "common:menu"
                && matches!(&entry.value, MessageValue::Object(_))
        }));
        assert!(entries.iter().any(|entry| {
            entry.key.canonical() == "common:steps"
                && matches!(&entry.value, MessageValue::Array(_))
                && &source[entry.key_span.start as usize..entry.key_span.end as usize] == "steps"
        }));
        assert!(!entries
            .iter()
            .any(|entry| entry.key.canonical() == "common:steps.0"));
    }

    fn workspace(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("react-i18next-lens-{name}-{nonce}"))
    }
}
