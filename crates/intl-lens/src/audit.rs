use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::analysis::{AnalyzerConfig, ReactSourceAnalyzer};
use crate::catalog::{MessageValue, TranslationCatalog};
use crate::configuration::WorkspaceConfig;
use crate::domain::{ByteSpan, KeyResolution, TranslationKey};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub summary: AuditSummary,
    pub missing: Vec<MissingTranslation>,
    pub unused: Vec<UnusedKey>,
    pub placeholder_issues: Vec<PlaceholderIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditSummary {
    pub total_keys: usize,
    pub total_locales: usize,
    pub missing_translations: usize,
    pub unused_keys: usize,
    pub placeholder_mismatches: usize,
    pub dynamic_usages: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingTranslation {
    pub key: String,
    pub source_value: String,
    pub source_locale: String,
    pub missing_in: Vec<String>,
    pub used_in: Vec<KeyUsage>,
    pub suggestion: Option<FixSuggestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnusedKey {
    pub key: String,
    pub defined_in: TranslationLocation,
    pub provisional: bool,
    pub suggestion: Option<FixSuggestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslationLocation {
    pub file_path: PathBuf,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaceholderIssue {
    pub key: String,
    pub issue_type: PlaceholderIssueType,
    pub locale_values: HashMap<String, String>,
    pub expected_placeholders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceholderIssueType {
    Mismatch,
    Missing,
    Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyUsage {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixSuggestion {
    pub action: String,
    pub files_to_edit: Vec<PathBuf>,
    pub context: Option<String>,
}

pub fn audit_workspace(
    root: &Path,
    config: &WorkspaceConfig,
    catalog: &TranslationCatalog,
) -> AuditReport {
    let analyzer = ReactSourceAnalyzer::new(AnalyzerConfig {
        default_namespace: config.default_namespace.clone(),
        namespace_separator: config.namespace_separator,
        key_separator: config.key_separator,
    });
    let mut sources = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_ignored(entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && is_source(entry.path()))
    {
        let Ok(source) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let analysis = analyzer.analyze(entry.path(), &source);
        sources.push((entry.path().to_path_buf(), source, analysis));
    }
    audit_sources(
        root,
        config,
        catalog,
        sources
            .iter()
            .map(|(path, source, analysis)| (path.as_path(), source.as_str(), analysis)),
    )
}

pub fn audit_sources<'a>(
    root: &Path,
    config: &WorkspaceConfig,
    catalog: &TranslationCatalog,
    sources: impl Iterator<Item = (&'a Path, &'a str, &'a crate::analysis::SourceAnalysis)>,
) -> AuditReport {
    let mut used: HashMap<TranslationKey, Vec<KeyUsage>> = HashMap::new();
    let mut dynamic_usages = 0;
    for (path, source, analysis) in sources {
        dynamic_usages += analysis.unresolved.len();
        for usage in &analysis.usages {
            let KeyResolution::Static(key) = &usage.resolution else {
                continue;
            };
            let (line, column, code) = usage_context(source, usage.expression_span);
            used.entry(key.clone()).or_default().push(KeyUsage {
                file: path.to_path_buf(),
                line,
                column,
                code,
            });
        }
    }

    let mut keys = catalog
        .entries()
        .map(|entry| entry.key.clone())
        .chain(used.keys().cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    keys.sort_by_key(TranslationKey::canonical);

    let mut missing = Vec::new();
    let mut unused = Vec::new();
    let mut placeholder_issues = Vec::new();
    for key in &keys {
        let usages = used.get(key).cloned().unwrap_or_default();
        let source_entry = catalog.get(&config.source_locale, key);
        let source_value = source_entry
            .map(|entry| entry.value.display())
            .unwrap_or_default();
        let missing_in = config
            .locales
            .iter()
            .filter(|locale| {
                catalog.get(locale, key).is_none_or(|entry| {
                    let value = entry.value.display();
                    value.trim().is_empty() || value == key.canonical()
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !missing_in.is_empty() {
            missing.push(MissingTranslation {
                key: key.canonical(),
                source_value: source_value.clone(),
                source_locale: config.source_locale.clone(),
                suggestion: Some(FixSuggestion {
                    action: "preview_add_missing_translation".to_string(),
                    files_to_edit: missing_in
                        .iter()
                        .filter_map(|locale| resource_path(root, config, locale, key))
                        .collect(),
                    context: Some(format!("Translation for '{}'", key.canonical())),
                }),
                missing_in,
                used_in: usages.clone(),
            });
        }

        if usages.is_empty() {
            if let Some(entry) = source_entry {
                unused.push(UnusedKey {
                    key: key.canonical(),
                    defined_in: TranslationLocation {
                        file_path: entry.file.clone(),
                        line: catalog
                            .source(&entry.file)
                            .map(|source| line_at(source, entry.key_span.start as usize))
                            .unwrap_or(0),
                    },
                    provisional: dynamic_usages > 0,
                    suggestion: Some(FixSuggestion {
                        action: "review_only".to_string(),
                        files_to_edit: Vec::new(),
                        context: Some(if dynamic_usages > 0 {
                            "Static usage not found; dynamic usages exist, so deletion is unsafe"
                                .to_string()
                        } else {
                            "Static usage not found; automatic deletion is unsupported".to_string()
                        }),
                    }),
                });
            }
        }

        if let Some(issue) = placeholder_issue(config, catalog, key, source_value) {
            placeholder_issues.push(issue);
        }
    }

    AuditReport {
        summary: AuditSummary {
            total_keys: keys.len(),
            total_locales: config.locales.len(),
            missing_translations: missing.len(),
            unused_keys: unused.len(),
            placeholder_mismatches: placeholder_issues.len(),
            dynamic_usages,
        },
        missing,
        unused,
        placeholder_issues,
    }
}

fn placeholder_issue(
    config: &WorkspaceConfig,
    catalog: &TranslationCatalog,
    key: &TranslationKey,
    source_value: String,
) -> Option<PlaceholderIssue> {
    let expected = extract_placeholders(&source_value);
    let mut mismatches = HashMap::new();
    let mut saw_missing = false;
    let mut saw_extra = false;
    for locale in &config.locales {
        if locale == &config.source_locale {
            continue;
        }
        let Some(entry) = catalog.get(locale, key) else {
            continue;
        };
        let value = match &entry.value {
            MessageValue::String(value) => value.clone(),
            value => value.display(),
        };
        let actual = extract_placeholders(&value);
        if actual != expected {
            saw_missing |= expected.iter().any(|item| !actual.contains(item));
            saw_extra |= actual.iter().any(|item| !expected.contains(item));
            mismatches.insert(locale.clone(), value);
        }
    }
    (!mismatches.is_empty()).then(|| PlaceholderIssue {
        key: key.canonical(),
        issue_type: match (saw_missing, saw_extra) {
            (true, false) => PlaceholderIssueType::Missing,
            (false, true) => PlaceholderIssueType::Extra,
            _ => PlaceholderIssueType::Mismatch,
        },
        locale_values: mismatches,
        expected_placeholders: expected,
    })
}

fn extract_placeholders(value: &str) -> Vec<String> {
    let regex = regex::Regex::new(r"\{\{-?\s*([\w.]+)\s*\}\}").expect("valid placeholder regex");
    let mut placeholders = regex
        .captures_iter(value)
        .filter_map(|captures| captures.get(1).map(|value| value.as_str().to_string()))
        .collect::<Vec<_>>();
    placeholders.sort();
    placeholders.dedup();
    placeholders
}

fn resource_path(
    root: &Path,
    config: &WorkspaceConfig,
    locale: &str,
    key: &TranslationKey,
) -> Option<PathBuf> {
    config.resource_patterns.first().map(|pattern| {
        root.join(
            pattern
                .replace("{locale}", locale)
                .replace("{namespace}", key.namespace.as_str()),
        )
    })
}

fn usage_context(source: &str, span: ByteSpan) -> (usize, usize, String) {
    let start = span.start as usize;
    let line = line_at(source, start);
    let line_start = source[..start.min(source.len())]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let column = source[line_start..start.min(source.len())].chars().count();
    let code = source[line_start..]
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    (line, column, code)
}

fn line_at(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
}

fn is_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    )
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("node_modules" | ".git" | "target" | ".next" | "dist" | "build")
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn reports_physical_missing_placeholder_mismatch_and_dynamic_uncertainty() {
        let root = fixture("audit");
        fs::write(
            root.join("component.tsx"),
            r#"import { useTranslation } from 'react-i18next';
               const { t } = useTranslation('common');
               t('greeting'); t(`dynamic.${kind}`);"#,
        )
        .unwrap();
        let config = config();
        let catalog = TranslationCatalog::load(&root, &config);
        let report = audit_workspace(&root, &config, &catalog);

        assert_eq!(report.summary.dynamic_usages, 1);
        assert_eq!(report.placeholder_issues.len(), 1);
        assert_eq!(report.missing.len(), 1);
        assert!(report.unused.iter().all(|item| item.provisional));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn extracts_i18next_placeholders() {
        assert_eq!(
            extract_placeholders("Hello {{ name }} {{- user.id }} {{name}}"),
            ["name", "user.id"]
        );
    }

    fn config() -> WorkspaceConfig {
        WorkspaceConfig {
            source_locale: "en".to_string(),
            locales: vec!["en".to_string(), "ja".to_string()],
            resource_patterns: vec!["locales/{locale}/{namespace}.json".to_string()],
            default_namespace: "common".to_string(),
            fallback_locales: Vec::new(),
            key_separator: Some('.'),
            namespace_separator: Some(':'),
        }
    }

    fn fixture(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("react-i18next-lens-{name}-{nonce}"));
        fs::create_dir_all(root.join("locales/en")).unwrap();
        fs::create_dir_all(root.join("locales/ja")).unwrap();
        fs::write(
            root.join("locales/en/common.json"),
            r#"{"greeting":"Hello {{name}}","unused":"Unused"}"#,
        )
        .unwrap();
        fs::write(
            root.join("locales/ja/common.json"),
            r#"{"greeting":"こんにちは"}"#,
        )
        .unwrap();
        root
    }
}
