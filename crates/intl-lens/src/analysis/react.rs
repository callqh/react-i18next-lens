use std::collections::{HashMap, HashSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, Expression, ImportDeclaration,
    ImportDeclarationSpecifier, JSXAttributeItem, JSXAttributeValue, JSXElementName, JSXExpression,
    JSXOpeningElement, ObjectExpression, Statement, VariableDeclarator,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_semantic::{Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, SourceType, Span};

use crate::domain::{ByteSpan, DynamicReason, KeyResolution, TranslationKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzerConfig {
    pub default_namespace: String,
    pub namespace_separator: char,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            default_namespace: "translation".to_string(),
            namespace_separator: ':',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationUsage {
    pub resolution: KeyResolution,
    pub expression_span: ByteSpan,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedUsage {
    pub span: ByteSpan,
    pub reason: DynamicReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisDiagnostic {
    pub span: Option<ByteSpan>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceAnalysis {
    pub usages: Vec<TranslationUsage>,
    pub unresolved: Vec<UnresolvedUsage>,
    pub diagnostics: Vec<AnalysisDiagnostic>,
}

pub struct ReactSourceAnalyzer {
    config: AnalyzerConfig,
}

impl ReactSourceAnalyzer {
    pub fn new(config: AnalyzerConfig) -> Self {
        Self { config }
    }

    pub fn analyze(&self, path: &Path, source: &str) -> SourceAnalysis {
        let source_type = match SourceType::from_path(path) {
            Ok(source_type) if is_supported_source_type(path) => source_type,
            _ => {
                return SourceAnalysis {
                    diagnostics: vec![AnalysisDiagnostic {
                        span: None,
                        message: format!("unsupported React source extension: {}", path.display()),
                    }],
                    ..SourceAnalysis::default()
                };
            }
        };

        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type).parse();
        let mut diagnostics = parsed
            .errors
            .iter()
            .map(|error| AnalysisDiagnostic {
                span: None,
                message: error.to_string(),
            })
            .collect::<Vec<_>>();

        let semantic = SemanticBuilder::new().build(&parsed.program);
        diagnostics.extend(semantic.errors.iter().map(|error| AnalysisDiagnostic {
            span: None,
            message: error.to_string(),
        }));

        let scoping = semantic.semantic.scoping();
        let mut bindings = BindingCollector::new(scoping);
        bindings.visit_program(&parsed.program);

        let mut extractor = UsageExtractor::new(&self.config, scoping, bindings.finish());
        extractor.visit_program(&parsed.program);
        let mut analysis = extractor.finish();
        analysis.diagnostics.extend(diagnostics);
        analysis
            .usages
            .sort_by_key(|usage| usage.expression_span.start);
        analysis.unresolved.sort_by_key(|usage| usage.span.start);
        analysis
    }
}

fn is_supported_source_type(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    )
}

#[derive(Debug, Clone, Default)]
struct TranslationContext {
    namespace: Option<String>,
    key_prefix: Option<String>,
}

#[derive(Default)]
struct Bindings {
    use_translation: HashSet<SymbolId>,
    i18next_instances: HashSet<SymbolId>,
    get_fixed_t: HashSet<SymbolId>,
    translation_functions: HashMap<SymbolId, TranslationContext>,
    trans_components: HashSet<SymbolId>,
}

struct BindingCollector<'s> {
    scoping: &'s Scoping,
    bindings: Bindings,
}

impl<'s> BindingCollector<'s> {
    fn new(scoping: &'s Scoping) -> Self {
        Self {
            scoping,
            bindings: Bindings::default(),
        }
    }

    fn finish(self) -> Bindings {
        self.bindings
    }

    fn reference_symbol(&self, expression: &Expression<'_>) -> Option<SymbolId> {
        let identifier = expression.get_identifier_reference()?;
        self.scoping
            .get_reference(identifier.reference_id())
            .symbol_id()
    }

    fn is_use_translation_call(&self, call: &CallExpression<'_>) -> bool {
        self.reference_symbol(&call.callee)
            .is_some_and(|symbol| self.bindings.use_translation.contains(&symbol))
    }

    fn fixed_t_context(&self, call: &CallExpression<'_>) -> Option<TranslationContext> {
        let is_named_import = self
            .reference_symbol(&call.callee)
            .is_some_and(|symbol| self.bindings.get_fixed_t.contains(&symbol));
        let is_instance_method = call.callee.get_member_expr().is_some_and(|member| {
            member.static_property_name() == Some("getFixedT")
                && self
                    .reference_symbol(member.object())
                    .is_some_and(|symbol| self.bindings.i18next_instances.contains(&symbol))
        });

        (is_named_import || is_instance_method).then(|| TranslationContext {
            namespace: call.arguments.get(1).and_then(static_argument_value),
            key_prefix: call.arguments.get(2).and_then(static_argument_value),
        })
    }
}

impl<'a> Visit<'a> for BindingCollector<'_> {
    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        let source = declaration.source.value.as_str();
        if !matches!(source, "react-i18next" | "next-i18next" | "i18next") {
            walk::walk_import_declaration(self, declaration);
            return;
        }

        for specifier in declaration.specifiers.iter().flatten() {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    let imported = specifier.imported.name();
                    let symbol = specifier.local.symbol_id();
                    match imported.as_str() {
                        "useTranslation" if matches!(source, "react-i18next" | "next-i18next") => {
                            self.bindings.use_translation.insert(symbol);
                        }
                        "Trans" if matches!(source, "react-i18next" | "next-i18next") => {
                            self.bindings.trans_components.insert(symbol);
                        }
                        "getFixedT" if source == "i18next" => {
                            self.bindings.get_fixed_t.insert(symbol);
                        }
                        _ => {}
                    }
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier)
                    if source == "i18next" =>
                {
                    self.bindings
                        .i18next_instances
                        .insert(specifier.local.symbol_id());
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier)
                    if source == "i18next" =>
                {
                    self.bindings
                        .i18next_instances
                        .insert(specifier.local.symbol_id());
                }
                _ => {}
            }
        }

        walk::walk_import_declaration(self, declaration);
    }

    fn visit_variable_declarator(&mut self, declaration: &VariableDeclarator<'a>) {
        let Some(Expression::CallExpression(call)) = declaration.init.as_ref() else {
            walk::walk_variable_declarator(self, declaration);
            return;
        };

        let context = if self.is_use_translation_call(call) {
            TranslationContext {
                namespace: call.arguments.first().and_then(static_namespace_argument),
                key_prefix: call
                    .arguments
                    .get(1)
                    .and_then(argument_object)
                    .and_then(|object| object_string_property(object, "keyPrefix")),
            }
        } else if let Some(context) = self.fixed_t_context(call) {
            context
        } else {
            walk::walk_variable_declarator(self, declaration);
            return;
        };

        match &declaration.id {
            BindingPattern::ObjectPattern(pattern) => {
                for property in &pattern.properties {
                    if property.key.is_specific_static_name("t") {
                        if let BindingPattern::BindingIdentifier(identifier) = &property.value {
                            self.bindings
                                .translation_functions
                                .insert(identifier.symbol_id(), context.clone());
                        }
                    }
                }
            }
            BindingPattern::BindingIdentifier(identifier) => {
                self.bindings
                    .translation_functions
                    .insert(identifier.symbol_id(), context);
            }
            _ => {}
        }

        walk::walk_variable_declarator(self, declaration);
    }
}

struct UsageExtractor<'c, 's> {
    config: &'c AnalyzerConfig,
    scoping: &'s Scoping,
    bindings: Bindings,
    analysis: SourceAnalysis,
}

impl<'c, 's> UsageExtractor<'c, 's> {
    fn new(config: &'c AnalyzerConfig, scoping: &'s Scoping, bindings: Bindings) -> Self {
        Self {
            config,
            scoping,
            bindings,
            analysis: SourceAnalysis::default(),
        }
    }

    fn finish(self) -> SourceAnalysis {
        self.analysis
    }

    fn reference_symbol(&self, expression: &Expression<'_>) -> Option<SymbolId> {
        let identifier = expression.get_identifier_reference()?;
        self.scoping
            .get_reference(identifier.reference_id())
            .symbol_id()
    }

    fn call_context(&self, call: &CallExpression<'_>) -> Option<TranslationContext> {
        if let Some(symbol) = self.reference_symbol(&call.callee) {
            if let Some(context) = self.bindings.translation_functions.get(&symbol) {
                return Some(context.clone());
            }
        }

        let member = call.callee.get_member_expr()?;
        if member.static_property_name() != Some("t") {
            return None;
        }
        let object_symbol = self.reference_symbol(member.object())?;
        self.bindings
            .i18next_instances
            .contains(&object_symbol)
            .then(TranslationContext::default)
    }

    fn push_call_usage(&mut self, call: &CallExpression<'_>, mut context: TranslationContext) {
        let Some(argument) = call.arguments.first() else {
            return;
        };
        let Some(expression) = argument.as_expression() else {
            self.push_dynamic(argument.span(), DynamicReason::NonLiteralArgument);
            return;
        };

        if let Some(options) = call.arguments.get(1).and_then(argument_object) {
            if let Some(namespace) = object_string_property(options, "ns") {
                context.namespace = Some(namespace);
            }
        }

        let default_value = call
            .arguments
            .get(1)
            .and_then(argument_object)
            .and_then(|object| object_string_property(object, "defaultValue"));

        match static_key_expression(expression) {
            Ok((raw_key, span)) => {
                let Some(key) = TranslationKey::from_source(
                    &raw_key,
                    context.namespace.as_deref(),
                    context.key_prefix.as_deref(),
                    &self.config.default_namespace,
                    self.config.namespace_separator,
                ) else {
                    return;
                };
                self.analysis.usages.push(TranslationUsage {
                    resolution: KeyResolution::Static(key),
                    expression_span: span.into(),
                    default_value,
                });
            }
            Err(reason) => self.push_dynamic(expression.span(), reason),
        }
    }

    fn push_dynamic(&mut self, span: Span, reason: DynamicReason) {
        let span = ByteSpan::from(span);
        self.analysis.unresolved.push(UnresolvedUsage {
            span,
            reason: reason.clone(),
        });
        self.analysis.usages.push(TranslationUsage {
            resolution: KeyResolution::Dynamic { span, reason },
            expression_span: span,
            default_value: None,
        });
    }

    fn push_trans_usage(&mut self, element: &JSXOpeningElement<'_>) {
        let JSXElementName::IdentifierReference(name) = &element.name else {
            return;
        };
        let is_trans = self
            .scoping
            .get_reference(name.reference_id())
            .symbol_id()
            .is_some_and(|symbol| self.bindings.trans_components.contains(&symbol));
        if !is_trans {
            return;
        }

        let mut key = None;
        let mut namespace = None;
        for attribute in &element.attributes {
            let JSXAttributeItem::Attribute(attribute) = attribute else {
                continue;
            };
            if attribute.is_identifier("i18nKey") {
                key = jsx_static_attribute(attribute.value.as_ref());
            } else if attribute.is_identifier("ns") {
                namespace = jsx_static_attribute(attribute.value.as_ref()).map(|value| value.0);
            }
        }

        let Some((raw_key, span)) = key else {
            return;
        };
        let Some(key) = TranslationKey::from_source(
            &raw_key,
            namespace.as_deref(),
            None,
            &self.config.default_namespace,
            self.config.namespace_separator,
        ) else {
            return;
        };
        self.analysis.usages.push(TranslationUsage {
            resolution: KeyResolution::Static(key),
            expression_span: span.into(),
            default_value: None,
        });
    }
}

impl<'a> Visit<'a> for UsageExtractor<'_, '_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Some(context) = self.call_context(call) {
            self.push_call_usage(call, context);
        }
        walk::walk_call_expression(self, call);
    }

    fn visit_jsx_opening_element(&mut self, element: &JSXOpeningElement<'a>) {
        self.push_trans_usage(element);
        walk::walk_jsx_opening_element(self, element);
    }
}

fn argument_object<'r, 'a>(argument: &'r Argument<'a>) -> Option<&'r ObjectExpression<'a>> {
    match argument.as_expression()?.get_inner_expression() {
        Expression::ObjectExpression(object) => Some(object),
        _ => None,
    }
}

fn object_string_property(object: &ObjectExpression<'_>, name: &str) -> Option<String> {
    object.properties.iter().find_map(|property| {
        let property = property.as_property()?;
        property
            .key
            .is_specific_static_name(name)
            .then(|| static_expression_value(&property.value))?
    })
}

fn static_argument_value(argument: &Argument<'_>) -> Option<String> {
    static_expression_value(argument.as_expression()?)
}

fn static_namespace_argument(argument: &Argument<'_>) -> Option<String> {
    let expression = argument.as_expression()?.get_inner_expression();
    if let Some(value) = static_expression_value(expression) {
        return Some(value);
    }
    let Expression::ArrayExpression(array) = expression else {
        return None;
    };
    array.elements.first().and_then(|element| match element {
        oxc_ast::ast::ArrayExpressionElement::StringLiteral(value) => Some(value.value.to_string()),
        _ => None,
    })
}

fn static_expression_value(expression: &Expression<'_>) -> Option<String> {
    match expression.get_inner_expression() {
        Expression::StringLiteral(literal) => Some(literal.value.to_string()),
        Expression::TemplateLiteral(template) if template.expressions.is_empty() => {
            template.single_quasi().map(|value| value.to_string())
        }
        _ => None,
    }
}

fn static_key_expression(expression: &Expression<'_>) -> Result<(String, Span), DynamicReason> {
    match expression.get_inner_expression() {
        Expression::StringLiteral(literal) => Ok((literal.value.to_string(), literal.span)),
        Expression::TemplateLiteral(template) if template.expressions.is_empty() => template
            .single_quasi()
            .map(|value| (value.to_string(), template.span))
            .ok_or(DynamicReason::NonLiteralArgument),
        Expression::TemplateLiteral(_) => Err(DynamicReason::InterpolatedTemplate),
        Expression::ArrowFunctionExpression(arrow) => {
            static_selector_path(arrow).ok_or(DynamicReason::SelectorNotStatic)
        }
        _ => Err(DynamicReason::NonLiteralArgument),
    }
}

fn static_selector_path(
    arrow: &oxc_ast::ast::ArrowFunctionExpression<'_>,
) -> Option<(String, Span)> {
    if !arrow.expression || arrow.params.items.len() != 1 {
        return None;
    }
    let BindingPattern::BindingIdentifier(root) = &arrow.params.items[0].pattern else {
        return None;
    };
    let Statement::ExpressionStatement(statement) = arrow.body.statements.first()? else {
        return None;
    };

    let mut properties = Vec::new();
    let mut current = statement.expression.get_inner_expression();
    let span = current.span();
    loop {
        if let Some(member) = current.as_member_expression() {
            properties.push(member.static_property_name()?.to_string());
            current = member.object().get_inner_expression();
            continue;
        }
        let Expression::Identifier(identifier) = current else {
            return None;
        };
        if identifier.name != root.name {
            return None;
        }
        break;
    }
    properties.reverse();
    (!properties.is_empty()).then(|| (properties.join("."), span))
}

fn jsx_static_attribute(value: Option<&JSXAttributeValue<'_>>) -> Option<(String, Span)> {
    match value? {
        JSXAttributeValue::StringLiteral(literal) => {
            Some((literal.value.to_string(), literal.span))
        }
        JSXAttributeValue::ExpressionContainer(container) => match &container.expression {
            JSXExpression::StringLiteral(literal) => {
                Some((literal.value.to_string(), literal.span))
            }
            JSXExpression::TemplateLiteral(template) if template.expressions.is_empty() => template
                .single_quasi()
                .map(|value| (value.to_string(), template.span)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(source: &str) -> SourceAnalysis {
        ReactSourceAnalyzer::new(AnalyzerConfig {
            default_namespace: "common".to_string(),
            namespace_separator: ':',
        })
        .analyze(Path::new("component.tsx"), source)
    }

    fn static_keys(analysis: &SourceAnalysis) -> Vec<String> {
        analysis
            .usages
            .iter()
            .filter_map(|usage| match &usage.resolution {
                KeyResolution::Static(key) => Some(key.canonical()),
                KeyResolution::Dynamic { .. } => None,
            })
            .collect()
    }

    #[test]
    fn resolves_use_translation_namespace_and_prefix() {
        let analysis = analyze(
            r#"
                import { useTranslation } from 'react-i18next';
                const { t } = useTranslation('settings', { keyPrefix: 'buttons' });
                t('save');
            "#,
        );

        assert_eq!(static_keys(&analysis), ["settings:buttons.save"]);
    }

    #[test]
    fn ignores_unrelated_t_binding() {
        let analysis = analyze(
            r#"
                const t = (value: string) => value;
                t('not-a-translation');
            "#,
        );

        assert!(analysis.usages.is_empty());
    }

    #[test]
    fn resolves_i18next_instance_and_call_namespace() {
        let analysis = analyze(
            r#"
                import i18next from 'i18next';
                i18next.t('save', { ns: 'actions', defaultValue: 'Save' });
            "#,
        );

        assert_eq!(static_keys(&analysis), ["actions:save"]);
        assert_eq!(analysis.usages[0].default_value.as_deref(), Some("Save"));
    }

    #[test]
    fn resolves_selector_syntax() {
        let analysis = analyze(
            r#"
                import { useTranslation } from 'react-i18next';
                const { t } = useTranslation('common');
                t($ => $.buttons.save);
            "#,
        );

        assert_eq!(static_keys(&analysis), ["common:buttons.save"]);
    }

    #[test]
    fn resolves_trans_component() {
        let analysis = analyze(
            r#"
                import { Trans as Translation } from 'react-i18next';
                export const View = () => <Translation ns="checkout" i18nKey="pay" />;
            "#,
        );

        assert_eq!(static_keys(&analysis), ["checkout:pay"]);
    }

    #[test]
    fn retains_dynamic_usage_as_unresolved() {
        let analysis = analyze(
            r#"
                import { useTranslation } from 'react-i18next';
                const { t } = useTranslation('common');
                t(`buttons.${action}`);
            "#,
        );

        assert_eq!(analysis.unresolved.len(), 1);
        assert_eq!(
            analysis.unresolved[0].reason,
            DynamicReason::InterpolatedTemplate
        );
    }
}
