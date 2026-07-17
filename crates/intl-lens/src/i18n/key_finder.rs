use std::path::Path;

use crate::analysis::{AnalyzerConfig, ReactSourceAnalyzer};
use crate::domain::KeyResolution;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundKey {
    pub key: String,
    pub start_offset: usize,
    pub line: usize,
    /// UTF-16 code units from the beginning of the line, as required by LSP.
    pub start_char: usize,
    /// UTF-16 code units from the beginning of the line, as required by LSP.
    pub end_char: usize,
}

pub struct KeyFinder {
    analyzer: ReactSourceAnalyzer,
}

impl KeyFinder {
    pub fn new(default_namespace: Option<&str>) -> Self {
        Self {
            analyzer: ReactSourceAnalyzer::new(AnalyzerConfig {
                default_namespace: default_namespace.unwrap_or("translation").to_string(),
                namespace_separator: ':',
            }),
        }
    }

    pub fn find_keys(&self, content: &str) -> Vec<FoundKey> {
        self.find_keys_in_path(Path::new("document.tsx"), content)
    }

    pub fn find_keys_in_path(&self, path: &Path, content: &str) -> Vec<FoundKey> {
        self.analyzer
            .analyze(path, content)
            .usages
            .into_iter()
            .filter_map(|usage| {
                let KeyResolution::Static(key) = usage.resolution else {
                    return None;
                };
                let start_offset = usage.expression_span.start as usize;
                let end_offset = usage.expression_span.end as usize;
                let (line, start_char, end_char) =
                    byte_span_to_lsp_line(content, start_offset, end_offset)?;
                Some(FoundKey {
                    key: key.canonical(),
                    start_offset,
                    line,
                    start_char,
                    end_char,
                })
            })
            .collect()
    }

    pub fn find_key_at_position(
        &self,
        content: &str,
        line: usize,
        character: usize,
    ) -> Option<FoundKey> {
        self.find_keys(content).into_iter().find(|key| {
            key.line == line && character >= key.start_char && character <= key.end_char
        })
    }
}

impl Default for KeyFinder {
    fn default() -> Self {
        Self::new(None)
    }
}

fn byte_span_to_lsp_line(
    content: &str,
    start_offset: usize,
    end_offset: usize,
) -> Option<(usize, usize, usize)> {
    if start_offset > end_offset
        || end_offset > content.len()
        || !content.is_char_boundary(start_offset)
        || !content.is_char_boundary(end_offset)
    {
        return None;
    }

    let prefix = &content[..start_offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |offset| offset + 1);
    let start_char = content[line_start..start_offset].encode_utf16().count();
    let end_char = content[line_start..end_offset].encode_utf16().count();
    Some((line, start_char, end_char))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_imported_react_translation_function() {
        let finder = KeyFinder::new(Some("common"));
        let content = r#"
            import { useTranslation } from 'react-i18next';
            const { t } = useTranslation('checkout');
            const translated = t('pay');
            const local = ((t: (key: string) => string) => t('ignore'))(String);
        "#;

        let keys = finder.find_keys(content);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, "checkout:pay");
    }

    #[test]
    fn reports_utf16_lsp_columns() {
        let finder = KeyFinder::new(Some("common"));
        let content =
            "const emoji = '😀';\nimport i18next from 'i18next';\n'😀'; i18next.t('save');";

        let keys = finder.find_keys(content);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].line, 2);
        assert_eq!(keys[0].start_char, 16);
    }

    #[test]
    fn ignores_unsupported_framework_syntax() {
        let finder = KeyFinder::default();
        let content = r#"
            $t('vue.key');
            $_('svelte.key');
            FlutterI18n.translate(context, 'flutter.key');
        "#;

        assert!(finder.find_keys(content).is_empty());
    }
}
