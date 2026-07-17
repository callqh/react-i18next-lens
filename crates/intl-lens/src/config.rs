use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::i18n::namespace;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct I18nConfig {
    #[serde(default = "default_locale_paths", alias = "localesPaths")]
    pub locale_paths: Vec<String>,

    #[serde(default)]
    pub source_locale: String,

    #[serde(default = "default_key_style")]
    pub key_style: KeyStyle,

    #[serde(default)]
    pub namespace_enabled: bool,

    #[serde(default, alias = "defaultNS")]
    pub default_namespace: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyStyle {
    #[default]
    Nested,
    Flat,
    Auto,
}

impl Default for I18nConfig {
    fn default() -> Self {
        Self {
            locale_paths: default_locale_paths(),
            source_locale: String::new(),
            key_style: default_key_style(),
            namespace_enabled: false,
            default_namespace: None,
        }
    }
}

impl I18nConfig {
    pub fn load_from_workspace(root: &Path) -> Self {
        let config_paths = [root.join("react-i18next-lens.json")];

        for config_path in config_paths {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                let raw_config = serde_json::from_str::<Value>(&content).ok();

                if let Ok(mut config) = serde_json::from_str::<I18nConfig>(&content) {
                    let raw_object = raw_config.as_ref().and_then(|value| value.as_object());
                    let has_locale_paths = raw_object.is_some_and(|object| {
                        object.contains_key("localePaths")
                            || object.contains_key("locale_paths")
                            || object.contains_key("localesPaths")
                    });
                    let has_source_locale = raw_object.is_some_and(|object| {
                        object.contains_key("sourceLocale") || object.contains_key("source_locale")
                    });
                    let has_namespace_enabled = raw_object.is_some_and(|object| {
                        object.contains_key("namespaceEnabled")
                            || object.contains_key("namespace_enabled")
                    });
                    let has_default_namespace = raw_object.is_some_and(|object| {
                        object.contains_key("defaultNamespace")
                            || object.contains_key("default_namespace")
                            || object.contains_key("defaultNS")
                    });

                    config.apply_detected_framework_config(
                        root,
                        !has_locale_paths,
                        !has_source_locale,
                        !has_namespace_enabled,
                        !has_default_namespace,
                    );

                    tracing::info!("Loaded config from {:?}", config_path);
                    return config;
                }
            }
        }

        tracing::info!("No react-i18next-lens.json found; running configuration discovery");
        let mut config = Self::default();
        config.apply_detected_framework_config(root, true, true, true, true);
        config
    }

    fn apply_detected_framework_config(
        &mut self,
        root: &Path,
        merge_locale_paths: bool,
        apply_source_locale: bool,
        apply_namespace_enabled: bool,
        apply_default_namespace: bool,
    ) {
        let detected = detect_framework_i18n(root);

        if merge_locale_paths {
            let mut existing: HashSet<String> = self.locale_paths.iter().cloned().collect();
            for path in detected.locale_paths {
                if existing.insert(path.clone()) {
                    self.locale_paths.push(path);
                }
            }
        }

        if apply_source_locale {
            if let Some(source_locale) = detected.source_locale {
                self.source_locale = source_locale;
            }
        }

        if apply_namespace_enabled && detected.namespace_enabled {
            self.namespace_enabled = true;
        }

        if apply_default_namespace {
            if let Some(default_namespace) = detected.default_namespace {
                self.default_namespace = Some(default_namespace);
            }
        }
    }
}

#[derive(Debug, Default)]
struct DetectedI18nConfig {
    locale_paths: Vec<String>,
    source_locale: Option<String>,
    namespace_enabled: bool,
    default_namespace: Option<String>,
}

fn default_locale_paths() -> Vec<String> {
    vec![
        "locales".to_string(),
        "i18n".to_string(),
        "translations".to_string(),
        "public/locales".to_string(),
        "public/static/locales".to_string(),
        "src/locales".to_string(),
        "src/i18n".to_string(),
    ]
}

fn default_key_style() -> KeyStyle {
    KeyStyle::Auto
}

fn detect_framework_i18n(root: &Path) -> DetectedI18nConfig {
    let mut detected = DetectedI18nConfig::default();

    if let Some(namespace_project) = namespace::detect_namespace_project(root) {
        detected.locale_paths.extend(namespace_project.locale_paths);
        detected.source_locale = namespace_project.source_locale;
        detected.namespace_enabled = true;
        detected.default_namespace = namespace_project.default_namespace;
    }

    detected.locale_paths.sort();
    detected.locale_paths.dedup();
    detected
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn reads_explicit_locale_paths() {
        let config = serde_json::from_str::<I18nConfig>(
            r#"{"localesPaths":["src/lang","**/*/i18n/locales"]}"#,
        )
        .expect("parse i18n ally config");

        assert_eq!(
            config.locale_paths,
            vec!["src/lang".to_string(), "**/*/i18n/locales".to_string()]
        );
    }

    #[test]
    fn detects_next_i18next_config() {
        let root = test_workspace("next-i18next-detection");
        fs::create_dir_all(root.join("public/static/locales/en")).expect("create locales");
        fs::write(
            root.join("next-i18next.config.js"),
            r#"
            const path = require('path')

            module.exports = {
              i18n: {
                defaultLocale: 'en',
                locales: ['ja', 'en', 'zh', 'zh-CN'],
              },
              localePath: path.resolve('./public/static/locales'),
              defaultNS: 'common',
            }
            "#,
        )
        .expect("write next-i18next config");

        let config = I18nConfig::load_from_workspace(&root);

        assert!(config.namespace_enabled);
        assert_eq!(config.source_locale, "en");
        assert_eq!(config.default_namespace.as_deref(), Some("common"));
        assert!(config
            .locale_paths
            .contains(&"public/static/locales".to_string()));

        fs::remove_dir_all(root).ok();
    }

    fn test_workspace(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("intl-lens-{name}-{nonce}"))
    }
}
