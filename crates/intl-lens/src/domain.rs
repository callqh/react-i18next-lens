use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ByteSpan {
    pub start: u32,
    pub end: u32,
}

impl ByteSpan {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
}

impl From<oxc_span::Span> for ByteSpan {
    fn from(span: oxc_span::Span) -> Self {
        Self::new(span.start, span.end)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Namespace(String);

impl Namespace {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyPath(String);

impl KeyPath {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TranslationKey {
    pub namespace: Namespace,
    pub path: KeyPath,
}

impl TranslationKey {
    pub fn from_source(
        raw: &str,
        contextual_namespace: Option<&str>,
        key_prefix: Option<&str>,
        default_namespace: &str,
        namespace_separator: char,
    ) -> Option<Self> {
        let (namespace, raw_path) = raw.split_once(namespace_separator).map_or(
            (contextual_namespace.unwrap_or(default_namespace), raw),
            |parts| parts,
        );

        let path = match key_prefix {
            Some(prefix) if !prefix.is_empty() && !raw_path.is_empty() => {
                format!("{prefix}.{raw_path}")
            }
            _ => raw_path.to_string(),
        };

        Some(Self {
            namespace: Namespace::new(namespace)?,
            path: KeyPath::new(path)?,
        })
    }

    pub fn canonical(&self) -> String {
        format!("{}:{}", self.namespace.as_str(), self.path.as_str())
    }
}

impl fmt::Display for TranslationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.canonical())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DynamicReason {
    NonLiteralArgument,
    InterpolatedTemplate,
    SelectorNotStatic,
    AmbiguousNamespace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyResolution {
    Static(TranslationKey),
    Dynamic {
        span: ByteSpan,
        reason: DynamicReason,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_namespace_overrides_context() {
        let key = TranslationKey::from_source(
            "common:buttons.save",
            Some("settings"),
            None,
            "translation",
            ':',
        )
        .unwrap();

        assert_eq!(key.canonical(), "common:buttons.save");
    }

    #[test]
    fn context_and_prefix_form_canonical_identity() {
        let key = TranslationKey::from_source(
            "save",
            Some("common"),
            Some("buttons"),
            "translation",
            ':',
        )
        .unwrap();

        assert_eq!(key.canonical(), "common:buttons.save");
    }
}
