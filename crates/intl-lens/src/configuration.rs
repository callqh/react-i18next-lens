use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrayExpressionElement, AssignmentExpression, CallExpression, ExportDefaultDeclaration,
    Expression, ObjectProperty, TemplateLiteral, VariableDeclarator,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use serde::Deserialize;

use crate::pathing::resolve_within_root;

const SUPPORTED_CONFIG_EXTENSIONS: &[&str] =
    &["js", "jsx", "cjs", "mjs", "ts", "tsx", "cts", "mts", "json"];
const DISCOVERABLE_CONFIG_STEMS: &[&str] =
    &["next-i18next.config", "i18next.config", "i18n.config"];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    pub source_locale: String,
    pub locales: Vec<String>,
    pub resource_patterns: Vec<String>,
    pub default_namespace: String,
    pub fallback_locales: Vec<String>,
    pub key_separator: Option<char>,
    pub namespace_separator: Option<char>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLoad {
    pub config: Option<WorkspaceConfig>,
    pub diagnostics: Vec<ConfigurationDiagnostic>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LensProjectFile {
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    source_locale: Option<String>,
    #[serde(default)]
    locales: Option<Vec<String>>,
    #[serde(default)]
    resources: Option<Vec<String>>,
    #[serde(default, alias = "defaultNS")]
    default_namespace: Option<String>,
}

#[derive(Debug, Default)]
struct ConfigFacts {
    source_locale: Option<String>,
    locales: Vec<String>,
    resource_patterns: Vec<String>,
    default_namespace: Option<String>,
    fallback_locales: Vec<String>,
    key_separator: Option<Option<char>>,
    namespace_separator: Option<Option<char>>,
    unresolved_fields: HashSet<String>,
}

#[derive(Debug)]
enum ConfigurationDiscoveryError {
    NotFound,
    Ambiguous(Vec<PathBuf>),
}

impl ConfigurationDiscoveryError {
    fn message(&self) -> String {
        match self {
            Self::NotFound =>
                "no supported i18next configuration was discovered; add a standard next-i18next.config.*, i18next.config.*, or i18n.config.* file, or use optional react-i18next-lens.json overrides"
                    .to_string(),
            Self::Ambiguous(candidates) => format!(
                "multiple i18next configuration files were discovered: {}; select one with react-i18next-lens.json extends",
                candidates
                    .iter()
                    .map(|path| path.file_name().unwrap_or_default().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl WorkspaceConfig {
    pub fn load(root: &Path) -> ConfigLoad {
        let project_path = root.join("react-i18next-lens.json");
        let project_content = match std::fs::read_to_string(&project_path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return ConfigLoad {
                    config: None,
                    diagnostics: vec![ConfigurationDiagnostic {
                        path: project_path,
                        message: format!("failed to read project configuration: {error}"),
                    }],
                };
            }
        };

        let project: LensProjectFile = match project_content {
            Some(content) => match serde_json::from_str(&content) {
                Ok(project) => project,
                Err(error) => {
                    return ConfigLoad {
                        config: None,
                        diagnostics: vec![ConfigurationDiagnostic {
                            path: project_path,
                            message: format!("invalid project configuration: {error}"),
                        }],
                    };
                }
            },
            None => match discover_configuration_source(root) {
                Ok(source) => LensProjectFile {
                    extends: Some(
                        source
                            .strip_prefix(root)
                            .unwrap_or(&source)
                            .to_string_lossy()
                            .to_string(),
                    ),
                    ..LensProjectFile::default()
                },
                Err(error) => {
                    return ConfigLoad {
                        config: None,
                        diagnostics: vec![ConfigurationDiagnostic {
                            path: project_path,
                            message: error.message(),
                        }],
                    };
                }
            },
        };

        let mut diagnostics = Vec::new();
        let mut facts = ConfigFacts::default();
        let source_path = match &project.extends {
            Some(extends) => Some(root.join(extends)),
            None => match discover_configuration_source(root) {
                Ok(source) => Some(source),
                Err(ConfigurationDiscoveryError::NotFound) => None,
                Err(error) => {
                    return ConfigLoad {
                        config: None,
                        diagnostics: vec![ConfigurationDiagnostic {
                            path: project_path,
                            message: error.message(),
                        }],
                    };
                }
            },
        };
        if let Some(source_path) = source_path {
            match analyze_configuration_source(root, &source_path) {
                Ok(source_facts) => facts = source_facts,
                Err(message) => diagnostics.push(ConfigurationDiagnostic {
                    path: source_path,
                    message,
                }),
            }
        }

        if let Some(source_locale) = project.source_locale {
            facts.source_locale = Some(source_locale);
            facts.unresolved_fields.remove("sourceLocale");
        }
        if let Some(locales) = project.locales {
            facts.locales = locales;
            facts.unresolved_fields.remove("locales");
        }
        if let Some(resources) = project.resources {
            facts.resource_patterns = resources;
            facts.unresolved_fields.remove("resources");
        }
        if let Some(default_namespace) = project.default_namespace {
            facts.default_namespace = Some(default_namespace);
            facts.unresolved_fields.remove("defaultNamespace");
        }

        if !facts.unresolved_fields.is_empty() {
            let mut fields = facts.unresolved_fields.into_iter().collect::<Vec<_>>();
            fields.sort();
            diagnostics.push(ConfigurationDiagnostic {
                path: project_path,
                message: format!(
                    "configuration fields are dynamic and require Lens overrides: {}",
                    fields.join(", ")
                ),
            });
            return ConfigLoad {
                config: None,
                diagnostics,
            };
        }

        let Some(source_locale) = facts.source_locale.filter(|locale| !locale.is_empty()) else {
            diagnostics.push(ConfigurationDiagnostic {
                path: project_path,
                message: "sourceLocale could not be resolved; configure it explicitly".to_string(),
            });
            return ConfigLoad {
                config: None,
                diagnostics,
            };
        };

        if facts.resource_patterns.is_empty() {
            diagnostics.push(ConfigurationDiagnostic {
                path: project_path,
                message: "translation resource path could not be resolved; configure resources"
                    .to_string(),
            });
            return ConfigLoad {
                config: None,
                diagnostics,
            };
        }

        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        for pattern in &mut facts.resource_patterns {
            let Some(resolved) = resolve_within_root(root, Path::new(pattern)) else {
                diagnostics.push(ConfigurationDiagnostic {
                    path: project_path.clone(),
                    message: format!("resource pattern must stay inside the workspace: {pattern}"),
                });
                return ConfigLoad {
                    config: None,
                    diagnostics,
                };
            };
            *pattern = resolved
                .strip_prefix(&canonical_root)
                .unwrap_or(&resolved)
                .to_string_lossy()
                .replace('\\', "/");
        }

        if !facts.locales.iter().any(|locale| locale == &source_locale) {
            facts.locales.push(source_locale.clone());
        }
        deduplicate(&mut facts.locales);
        deduplicate(&mut facts.resource_patterns);
        deduplicate(&mut facts.fallback_locales);

        ConfigLoad {
            config: Some(WorkspaceConfig {
                source_locale,
                locales: facts.locales,
                resource_patterns: facts.resource_patterns,
                default_namespace: facts
                    .default_namespace
                    .unwrap_or_else(|| "translation".to_string()),
                fallback_locales: facts.fallback_locales,
                key_separator: facts.key_separator.unwrap_or(Some('.')),
                namespace_separator: facts.namespace_separator.unwrap_or(Some(':')),
            }),
            diagnostics,
        }
    }
}

pub fn configuration_files(root: &Path) -> Vec<PathBuf> {
    let project = root.join("react-i18next-lens.json");
    let mut files = vec![project.clone()];
    if let Ok(content) = std::fs::read_to_string(project) {
        if let Ok(config) = serde_json::from_str::<LensProjectFile>(&content) {
            if let Some(extends) = config.extends {
                files.push(root.join(extends));
            } else if let Ok(source) = discover_configuration_source(root) {
                files.push(source);
            }
        }
    } else if let Ok(source) = discover_configuration_source(root) {
        files.push(source);
    }
    files
}

fn discover_configuration_source(root: &Path) -> Result<PathBuf, ConfigurationDiscoveryError> {
    let mut candidates = Vec::new();
    for stem in DISCOVERABLE_CONFIG_STEMS {
        for extension in SUPPORTED_CONFIG_EXTENSIONS {
            let candidate = root.join(format!("{stem}.{extension}"));
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }
    match candidates.as_slice() {
        [source] => Ok(source.clone()),
        [] => Err(ConfigurationDiscoveryError::NotFound),
        _ => Err(ConfigurationDiscoveryError::Ambiguous(candidates)),
    }
}

fn analyze_configuration_source(root: &Path, path: &Path) -> Result<ConfigFacts, String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !SUPPORTED_CONFIG_EXTENSIONS.contains(&extension) {
        return Err(format!(
            "unsupported configuration extension: {}",
            path.display()
        ));
    }
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read extended configuration: {error}"))?;
    if extension == "json" {
        let value: serde_json::Value = serde_json::from_str(&source)
            .map_err(|error| format!("invalid JSON configuration: {error}"))?;
        let mut facts = ConfigFacts::default();
        collect_json_facts(&value, &mut facts);
        for pattern in &mut facts.resource_patterns {
            *pattern = normalize_resource_pattern(root, path, pattern);
        }
        return Ok(facts);
    }
    let source_type = SourceType::from_path(path)
        .map_err(|_| format!("unsupported configuration extension: {}", path.display()))?;
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &source, source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(parsed
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "));
    }

    let mut names = ExportedConfigNames::new(&source);
    names.visit_program(&parsed.program);
    let mut collector = ConfigurationCollector::new(root, path, &source, names.names);
    collector.visit_program(&parsed.program);
    Ok(collector.finish())
}

struct ConfigurationCollector<'a> {
    root: &'a Path,
    path: &'a Path,
    source: &'a str,
    facts: ConfigFacts,
    collection_depth: usize,
    exported_names: HashSet<String>,
}

impl<'a> ConfigurationCollector<'a> {
    fn new(
        root: &'a Path,
        path: &'a Path,
        source: &'a str,
        exported_names: HashSet<String>,
    ) -> Self {
        Self {
            root,
            path,
            source,
            facts: ConfigFacts::default(),
            collection_depth: 0,
            exported_names,
        }
    }

    fn finish(mut self) -> ConfigFacts {
        for pattern in &mut self.facts.resource_patterns {
            *pattern = normalize_resource_pattern(self.root, self.path, pattern);
        }
        self.facts
    }

    fn collect_property(&mut self, property: &ObjectProperty<'_>) {
        let Some(name) = property.key.static_name() else {
            return;
        };
        match name.as_ref() {
            "sourceLocale" | "defaultLocale" => {
                self.facts.source_locale = static_string(&property.value);
                if self.facts.source_locale.is_none() {
                    self.facts
                        .unresolved_fields
                        .insert("sourceLocale".to_string());
                }
            }
            "locales" | "supportedLngs" | "preload" => {
                let values = static_string_array(&property.value);
                if !values.is_empty() {
                    self.facts.locales.extend(values);
                } else {
                    self.facts.unresolved_fields.insert("locales".to_string());
                }
            }
            "localePath" => {
                if let Some(path) = static_path_string(&property.value) {
                    self.facts
                        .resource_patterns
                        .push(format!("{path}/{{locale}}/{{namespace}}.json"));
                } else {
                    self.facts.unresolved_fields.insert("resources".to_string());
                }
            }
            "loadPath" => {
                if let Some(path) = static_path_string(&property.value) {
                    self.facts.resource_patterns.push(path);
                } else {
                    self.facts.unresolved_fields.insert("resources".to_string());
                }
            }
            "defaultNS" | "defaultNamespace" => {
                self.facts.default_namespace = static_string(&property.value);
                if self.facts.default_namespace.is_none() {
                    self.facts
                        .unresolved_fields
                        .insert("defaultNamespace".to_string());
                }
            }
            "fallbackLng" => {
                let scalar_fallback = static_string(&property.value);
                if self.facts.source_locale.is_none() {
                    self.facts.source_locale.clone_from(&scalar_fallback);
                }
                self.facts.fallback_locales = scalar_fallback
                    .into_iter()
                    .chain(static_string_array(&property.value))
                    .collect();
            }
            "keySeparator" => {
                self.facts.key_separator = static_separator(&property.value);
                if self.facts.key_separator.is_none() {
                    self.facts
                        .unresolved_fields
                        .insert("keySeparator".to_string());
                }
            }
            "nsSeparator" => {
                self.facts.namespace_separator = static_separator(&property.value);
                if self.facts.namespace_separator.is_none() {
                    self.facts
                        .unresolved_fields
                        .insert("nsSeparator".to_string());
                }
            }
            _ => {}
        }
    }
}

impl<'b> Visit<'b> for ConfigurationCollector<'_> {
    fn visit_variable_declarator(&mut self, declaration: &VariableDeclarator<'b>) {
        let exported = match &declaration.id {
            oxc_ast::ast::BindingPattern::BindingIdentifier(identifier) => {
                self.exported_names.contains(identifier.name.as_str())
            }
            _ => false,
        };
        if exported {
            self.collection_depth += 1;
            walk::walk_variable_declarator(self, declaration);
            self.collection_depth -= 1;
        } else {
            walk::walk_variable_declarator(self, declaration);
        }
    }
    fn visit_export_default_declaration(&mut self, declaration: &ExportDefaultDeclaration<'b>) {
        self.collection_depth += 1;
        walk::walk_export_default_declaration(self, declaration);
        self.collection_depth -= 1;
    }

    fn visit_assignment_expression(&mut self, assignment: &AssignmentExpression<'b>) {
        let span = assignment.left.span();
        let target = self.source[span.start as usize..span.end as usize]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        if target == "module.exports" || target == "exports.default" {
            self.collection_depth += 1;
            walk::walk_assignment_expression(self, assignment);
            self.collection_depth -= 1;
        } else {
            walk::walk_assignment_expression(self, assignment);
        }
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'b>) {
        let span = call.callee.span();
        let callee = self.source[span.start as usize..span.end as usize]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let is_configuration_call = callee == "defineConfig"
            || callee == "i18next.init"
            || callee.ends_with(".initReactI18next");
        if self.collection_depth == 0 && is_configuration_call {
            self.collection_depth += 1;
            walk::walk_call_expression(self, call);
            self.collection_depth -= 1;
        } else {
            walk::walk_call_expression(self, call);
        }
    }

    fn visit_object_property(&mut self, property: &ObjectProperty<'b>) {
        if self.collection_depth > 0 {
            self.collect_property(property);
        }
        walk::walk_object_property(self, property);
    }

    fn visit_template_literal(&mut self, template: &TemplateLiteral<'b>) {
        if self.collection_depth == 0 {
            walk::walk_template_literal(self, template);
            return;
        }
        let span = template.span();
        let raw = &self.source[span.start as usize..span.end as usize];
        if raw.ends_with(".json`") && raw.contains("${") {
            let pattern = raw
                .trim_matches('`')
                .replace("${language}", "{locale}")
                .replace("${lng}", "{locale}")
                .replace("${locale}", "{locale}")
                .replace("${namespace}", "{namespace}")
                .replace("${ns}", "{namespace}");
            if pattern.contains("{locale}") {
                self.facts.resource_patterns.push(pattern);
            }
        }
        walk::walk_template_literal(self, template);
    }
}

struct ExportedConfigNames<'a> {
    source: &'a str,
    names: HashSet<String>,
}

impl<'a> ExportedConfigNames<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            names: HashSet::new(),
        }
    }

    fn assignment_target(&self, assignment: &AssignmentExpression<'_>) -> String {
        let span = assignment.left.span();
        self.source[span.start as usize..span.end as usize]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }
}

impl<'b> Visit<'b> for ExportedConfigNames<'_> {
    fn visit_export_default_declaration(&mut self, declaration: &ExportDefaultDeclaration<'b>) {
        if let Some(identifier) = declaration
            .declaration
            .as_expression()
            .and_then(Expression::get_identifier_reference)
        {
            self.names.insert(identifier.name.to_string());
        }
        walk::walk_export_default_declaration(self, declaration);
    }

    fn visit_assignment_expression(&mut self, assignment: &AssignmentExpression<'b>) {
        if matches!(
            self.assignment_target(assignment).as_str(),
            "module.exports" | "exports.default"
        ) {
            if let Some(identifier) = assignment.right.get_identifier_reference() {
                self.names.insert(identifier.name.to_string());
            }
        }
        walk::walk_assignment_expression(self, assignment);
    }
}

fn static_string(expression: &Expression<'_>) -> Option<String> {
    match expression.get_inner_expression() {
        Expression::StringLiteral(value) => Some(value.value.to_string()),
        Expression::TemplateLiteral(value) if value.expressions.is_empty() => {
            value.single_quasi().map(|value| value.to_string())
        }
        _ => None,
    }
}

fn static_path_string(expression: &Expression<'_>) -> Option<String> {
    if let Some(value) = static_string(expression) {
        return Some(value);
    }
    let Expression::CallExpression(call) = expression.get_inner_expression() else {
        return None;
    };
    let member = call.callee.get_member_expr()?;
    if member.object().get_identifier_reference()?.name != "path" {
        return None;
    }
    let method = member.static_property_name()?;
    if !matches!(method, "resolve" | "join") {
        return None;
    }
    let parts = call
        .arguments
        .iter()
        .map(|argument| static_string(argument.as_expression()?))
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn collect_json_facts(value: &serde_json::Value, facts: &mut ConfigFacts) {
    let serde_json::Value::Object(object) = value else {
        return;
    };
    for (name, value) in object {
        match name.as_str() {
            "sourceLocale" | "defaultLocale" => {
                facts.source_locale = value.as_str().map(str::to_string);
            }
            "locales" | "supportedLngs" | "preload" => {
                if let Some(values) = value.as_array() {
                    facts.locales.extend(
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_string)),
                    );
                }
            }
            "localePath" => {
                if let Some(path) = value.as_str() {
                    facts
                        .resource_patterns
                        .push(format!("{path}/{{locale}}/{{namespace}}.json"));
                }
            }
            "loadPath" => {
                if let Some(path) = value.as_str() {
                    facts.resource_patterns.push(path.to_string());
                }
            }
            "defaultNS" | "defaultNamespace" => {
                facts.default_namespace = value.as_str().map(str::to_string);
            }
            "fallbackLng" => {
                if let Some(locale) = value.as_str() {
                    facts.fallback_locales.push(locale.to_string());
                    facts
                        .source_locale
                        .get_or_insert_with(|| locale.to_string());
                }
            }
            "keySeparator" => facts.key_separator = json_separator(value),
            "nsSeparator" => facts.namespace_separator = json_separator(value),
            _ => {}
        }
        collect_json_facts(value, facts);
    }
}

fn json_separator(value: &serde_json::Value) -> Option<Option<char>> {
    match value {
        serde_json::Value::Bool(false) => Some(None),
        serde_json::Value::String(value) if value.chars().count() == 1 => {
            Some(value.chars().next())
        }
        _ => None,
    }
}

fn static_string_array(expression: &Expression<'_>) -> Vec<String> {
    let Expression::ArrayExpression(array) = expression.get_inner_expression() else {
        return Vec::new();
    };
    array
        .elements
        .iter()
        .filter_map(|element| match element {
            ArrayExpressionElement::StringLiteral(value) => Some(value.value.to_string()),
            _ => None,
        })
        .collect()
}

fn static_separator(expression: &Expression<'_>) -> Option<Option<char>> {
    match expression.get_inner_expression() {
        Expression::BooleanLiteral(value) if !value.value => Some(None),
        _ => static_string(expression).and_then(|value| {
            let mut chars = value.chars();
            let separator = chars.next()?;
            chars.next().is_none().then_some(Some(separator))
        }),
    }
}

fn normalize_resource_pattern(root: &Path, source_path: &Path, pattern: &str) -> String {
    let pattern = pattern
        .replace("{{lng}}", "{locale}")
        .replace("{{ns}}", "{namespace}")
        .trim_start_matches("./")
        .to_string();
    let path = PathBuf::from(&pattern);
    if path.is_absolute() {
        return path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
    }
    let parent = source_path.parent().unwrap_or(root);
    parent
        .join(path)
        .strip_prefix(root)
        .unwrap_or_else(|_| Path::new(&pattern))
        .to_string_lossy()
        .replace('\\', "/")
}

fn deduplicate(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn loads_next_i18next_configuration_without_executing_it() {
        let root = workspace("next-i18next");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./next-i18next.config.js"}"#,
        )
        .unwrap();
        fs::write(
            root.join("next-i18next.config.js"),
            r#"
                const path = require('path')
                module.exports = {
                  i18n: { defaultLocale: 'en', locales: ['ja', 'en', 'zh-CN'] },
                  localePath: path.resolve('./public/static/locales'),
                  defaultNS: 'common',
                  fallbackLng: 'en',
                }
            "#,
        )
        .unwrap();

        let loaded = WorkspaceConfig::load(&root);
        let config = loaded.config.expect("valid config");
        assert_eq!(config.source_locale, "en");
        assert_eq!(config.locales, ["ja", "en", "zh-CN"]);
        assert_eq!(config.default_namespace, "common");
        assert_eq!(
            config.resource_patterns,
            ["public/static/locales/{locale}/{namespace}.json"]
        );
        assert_eq!(config.fallback_locales, ["en"]);
        assert!(loaded.diagnostics.is_empty());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn discovers_next_i18next_configuration_without_a_lens_project_file() {
        let root = workspace("zero-config-next-i18next");
        fs::create_dir_all(root.join("public/static/locales/en")).unwrap();
        fs::create_dir_all(root.join("public/static/locales/ja")).unwrap();
        fs::write(
            root.join("next-i18next.config.js"),
            r#"
                const path = require('path')
                module.exports = {
                  i18n: { defaultLocale: 'en', locales: ['en', 'ja'] },
                  localePath: path.resolve('./public/static/locales'),
                  defaultNS: 'common',
                }
            "#,
        )
        .unwrap();

        let loaded = WorkspaceConfig::load(&root);
        let config = loaded.config.expect("auto-discovered config");
        assert_eq!(config.source_locale, "en");
        assert_eq!(config.locales, ["en", "ja"]);
        assert_eq!(
            config.resource_patterns,
            ["public/static/locales/{locale}/{namespace}.json"]
        );
        assert!(loaded.diagnostics.is_empty());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn layers_lens_overrides_over_an_auto_discovered_configuration() {
        let root = workspace("zero-config-with-overrides");
        fs::create_dir_all(root.join("public/locales/en")).unwrap();
        fs::create_dir_all(root.join("public/locales/ja")).unwrap();
        fs::write(
            root.join("next-i18next.config.js"),
            r#"
                module.exports = {
                  i18n: { defaultLocale: 'en', locales: ['en', 'ja'] },
                  localePath: './public/locales',
                  defaultNS: 'common',
                }
            "#,
        )
        .unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"sourceLocale":"ja"}"#,
        )
        .unwrap();

        let loaded = WorkspaceConfig::load(&root);
        let config = loaded.config.expect("discovered config with overrides");
        assert_eq!(config.source_locale, "ja");
        assert_eq!(config.locales, ["en", "ja"]);
        assert_eq!(
            config.resource_patterns,
            ["public/locales/{locale}/{namespace}.json"]
        );
        assert!(loaded.diagnostics.is_empty());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn loads_typescript_dynamic_import_resource_pattern() {
        let root = workspace("typescript-config");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./i18n.config.ts"}"#,
        )
        .unwrap();
        fs::write(
            root.join("i18n.config.ts"),
            r#"
                export default defineConfig({
                  supportedLngs: ['en', 'de'],
                  fallbackLng: 'en',
                  defaultNS: 'common',
                  resourceLoader: (language, namespace) =>
                    import(`./locales/${language}/${namespace}.json`),
                })
            "#,
        )
        .unwrap();

        let loaded = WorkspaceConfig::load(&root);
        let config = loaded.config.expect("valid config");
        assert_eq!(config.source_locale, "en");
        assert_eq!(config.locales, ["en", "de"]);
        assert_eq!(
            config.resource_patterns,
            ["locales/{locale}/{namespace}.json"]
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn requires_explicit_override_for_dynamic_source_locale() {
        let root = workspace("dynamic-source-locale");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./i18n.config.mjs","resources":["locales/{locale}/{namespace}.json"]}"#,
        )
        .unwrap();
        fs::write(
            root.join("i18n.config.mjs"),
            "export default { fallbackLng: process.env.DEFAULT_LOCALE }",
        )
        .unwrap();

        let loaded = WorkspaceConfig::load(&root);
        assert!(loaded.config.is_none());
        assert!(loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("sourceLocale")));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_configuration_extensions_outside_the_public_contract() {
        let root = workspace("unsupported-config-extension");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./i18n.config.yaml","sourceLocale":"en","resources":["locales/{locale}/{namespace}.json"]}"#,
        )
        .unwrap();
        fs::write(root.join("i18n.config.yaml"), "fallbackLng: en").unwrap();

        let loaded = WorkspaceConfig::load(&root);
        assert!(loaded.config.is_some());
        assert!(loaded.diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("unsupported configuration extension")));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ignores_configuration_shaped_objects_outside_the_exported_config() {
        let root = workspace("ignore-unrelated-object");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./next-i18next.config.js"}"#,
        )
        .unwrap();
        fs::write(
            root.join("next-i18next.config.js"),
            r#"
                module.exports = {
                  i18n: { defaultLocale: 'en', locales: ['en', 'ja'] },
                  localePath: './locales'
                }
                const testFixture = { defaultLocale: 'wrong', locales: ['wrong'] }
            "#,
        )
        .unwrap();

        let config = WorkspaceConfig::load(&root).config.unwrap();
        assert_eq!(config.source_locale, "en");
        assert_eq!(config.locales, ["en", "ja"]);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_resource_patterns_outside_the_workspace() {
        let root = workspace("outside-resource");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"sourceLocale":"en","resources":["../outside/{locale}/{namespace}.json"]}"#,
        )
        .unwrap();
        let loaded = WorkspaceConfig::load(&root);
        assert!(loaded.config.is_none());
        assert!(loaded.diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("must stay inside the workspace")));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn loads_plain_json_extended_configuration() {
        let root = workspace("json-config");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./i18next.config.json"}"#,
        )
        .unwrap();
        fs::write(
            root.join("i18next.config.json"),
            r#"{"fallbackLng":"en","supportedLngs":["en","ja"],"backend":{"loadPath":"locales/{{lng}}/{{ns}}.json"}}"#,
        )
        .unwrap();
        let config = WorkspaceConfig::load(&root).config.unwrap();
        assert_eq!(config.source_locale, "en");
        assert_eq!(config.locales, ["en", "ja"]);
        assert_eq!(
            config.resource_patterns,
            ["locales/{locale}/{namespace}.json"]
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn resolves_exported_local_config_objects() {
        let root = workspace("exported-variable");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./i18n.config.ts"}"#,
        )
        .unwrap();
        fs::write(
            root.join("i18n.config.ts"),
            r#"
                const config = {
                  fallbackLng: 'en',
                  supportedLngs: ['en', 'ja'],
                  loadPath: './locales/{{lng}}/{{ns}}.json'
                }
                export default config
            "#,
        )
        .unwrap();
        let config = WorkspaceConfig::load(&root).config.unwrap();
        assert_eq!(config.locales, ["en", "ja"]);
        assert_eq!(
            config.resource_patterns,
            ["locales/{locale}/{namespace}.json"]
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_dynamic_namespace_configuration_instead_of_defaulting() {
        let root = workspace("dynamic-default-namespace");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("react-i18next-lens.json"),
            r#"{"extends":"./i18n.config.js","sourceLocale":"en","resources":["locales/{locale}/{namespace}.json"]}"#,
        )
        .unwrap();
        fs::write(
            root.join("i18n.config.js"),
            "module.exports = { defaultNS: computeNamespace() }",
        )
        .unwrap();
        let loaded = WorkspaceConfig::load(&root);
        assert!(loaded.config.is_none());
        assert!(loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("defaultNamespace")));
        fs::remove_dir_all(root).ok();
    }

    fn workspace(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("react-i18next-lens-{name}-{nonce}"))
    }
}
