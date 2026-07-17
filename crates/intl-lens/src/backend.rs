use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{SourceAnalysis, TranslationUsage};
use crate::configuration::configuration_files;
use crate::domain::{ByteSpan, KeyResolution, TranslationKey};
use crate::mutation::AddMissingKey;
use crate::workspace::{Workspace, WorkspaceSnapshot};

pub struct I18nBackend {
    client: Client,
    workspace: Arc<RwLock<Option<Arc<Workspace>>>>,
    inlay_hint_refresh_supported: Arc<RwLock<bool>>,
    watched_files_dynamic_registration_supported: Arc<RwLock<bool>>,
    watched_files_relative_pattern_supported: Arc<RwLock<bool>>,
}

impl I18nBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(RwLock::new(None)),
            inlay_hint_refresh_supported: Arc::new(RwLock::new(false)),
            watched_files_dynamic_registration_supported: Arc::new(RwLock::new(false)),
            watched_files_relative_pattern_supported: Arc::new(RwLock::new(false)),
        }
    }

    async fn initialize_workspace(&self, root: PathBuf) {
        match Workspace::load(root.clone()) {
            Ok(workspace) => {
                let workspace = Arc::new(workspace);
                let snapshot = workspace.snapshot();
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "React i18next Lens initialized: {} locales, {} messages in {:?}",
                            snapshot.config.locales.len(),
                            snapshot.catalog.entries().count(),
                            root
                        ),
                    )
                    .await;
                for diagnostic in snapshot.catalog.diagnostics() {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("{}: {}", diagnostic.file.display(), diagnostic.message),
                        )
                        .await;
                }
                *self.workspace.write().await = Some(workspace);
            }
            Err(failure) => {
                for diagnostic in failure.diagnostics {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("{}: {}", diagnostic.path.display(), diagnostic.message),
                        )
                        .await;
                }
            }
        }
    }

    async fn current_workspace(&self) -> Option<Arc<Workspace>> {
        self.workspace.read().await.clone()
    }

    async fn register_watched_files_capability(&self) {
        if !*self
            .watched_files_dynamic_registration_supported
            .read()
            .await
        {
            return;
        }
        let Some(workspace) = self.current_workspace().await else {
            return;
        };
        let snapshot = workspace.snapshot();
        let relative = *self.watched_files_relative_pattern_supported.read().await;
        let watchers = build_file_watchers(
            &snapshot,
            workspace.root(),
            &configuration_files(workspace.root()),
            relative,
        );
        if watchers.is_empty() {
            return;
        }
        let options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let Ok(register_options) = serde_json::to_value(options) else {
            return;
        };
        if let Err(error) = self
            .client
            .register_capability(vec![Registration {
                id: "react-i18next-lens-watched-files".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: Some(register_options),
            }])
            .await
        {
            tracing::warn!(?error, "watched-file registration failed");
        }
    }

    async fn diagnose_document(&self, uri: &Url) {
        let diagnostics = self
            .current_workspace()
            .await
            .and_then(|workspace| {
                let path = uri.to_file_path().ok()?;
                let snapshot = workspace.snapshot();
                let document = snapshot.documents.get(&path)?;
                let analysis = snapshot.analyses.get(&path)?;
                Some(diagnostics_for(&snapshot, analysis, &document.content))
            })
            .unwrap_or_default();
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }

    async fn reload(&self) {
        let Some(workspace) = self.current_workspace().await else {
            return;
        };
        match workspace.reload() {
            Ok(generation) => {
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Translations reloaded at generation {}", generation.value()),
                    )
                    .await;
                if *self.inlay_hint_refresh_supported.read().await {
                    if let Err(error) = self.client.inlay_hint_refresh().await {
                        tracing::warn!(?error, "inlay hint refresh failed");
                    }
                }
                if *self
                    .watched_files_dynamic_registration_supported
                    .read()
                    .await
                {
                    self.client
                        .unregister_capability(vec![Unregistration {
                            id: "react-i18next-lens-watched-files".to_string(),
                            method: "workspace/didChangeWatchedFiles".to_string(),
                        }])
                        .await
                        .ok();
                    self.register_watched_files_capability().await;
                }
                self.re_diagnose_open_documents().await;
            }
            Err(failure) => {
                for diagnostic in failure.diagnostics {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("{}: {}", diagnostic.path.display(), diagnostic.message),
                        )
                        .await;
                }
            }
        }
    }

    async fn re_diagnose_open_documents(&self) {
        let Some(workspace) = self.current_workspace().await else {
            return;
        };
        let uris = workspace
            .snapshot()
            .documents
            .keys()
            .filter_map(|path| Url::from_file_path(path).ok())
            .collect::<Vec<_>>();
        for uri in uris {
            self.diagnose_document(&uri).await;
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for I18nBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let workspace_capabilities = params.capabilities.workspace.as_ref();
        *self.inlay_hint_refresh_supported.write().await = workspace_capabilities
            .and_then(|workspace| workspace.inlay_hint.as_ref())
            .and_then(|inlay| inlay.refresh_support)
            .unwrap_or(false);
        let watched_files = workspace_capabilities
            .and_then(|workspace| workspace.did_change_watched_files.as_ref());
        *self
            .watched_files_dynamic_registration_supported
            .write()
            .await = watched_files
            .and_then(|watch| watch.dynamic_registration)
            .unwrap_or(false);
        *self.watched_files_relative_pattern_supported.write().await = watched_files
            .and_then(|watch| watch.relative_pattern_support)
            .unwrap_or(false);

        let root = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .and_then(|folder| folder.uri.to_file_path().ok())
            .or_else(|| params.root_uri.and_then(|uri| uri.to_file_path().ok()));
        if let Some(root) = root {
            self.initialize_workspace(root).await;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "\"".to_string(),
                        "'".to_string(),
                        ".".to_string(),
                    ]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
                    InlayHintOptions::default(),
                ))),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        ..Default::default()
                    },
                )),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec!["react-i18next-lens.addMissingSourceKey".to_string()],
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "react-i18next-lens".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "React i18next Lens server initialized")
            .await;
        self.register_watched_files_capability().await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let config_files = self
            .current_workspace()
            .await
            .map(|workspace| configuration_files(workspace.root()))
            .unwrap_or_default();
        if params.changes.iter().any(|change| {
            change.uri.path().ends_with(".json")
                || change
                    .uri
                    .to_file_path()
                    .is_ok_and(|path| config_files.contains(&path))
        }) {
            self.reload().await;
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        if let (Some(workspace), Ok(path)) = (self.current_workspace().await, uri.to_file_path()) {
            workspace.open_document(
                path,
                params.text_document.text,
                params.text_document.version,
            );
            self.diagnose_document(&uri).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        if let (Some(workspace), Ok(path)) = (self.current_workspace().await, uri.to_file_path()) {
            workspace.change_document(path, change.text, params.text_document.version);
            self.diagnose_document(&uri).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let is_config = self
            .current_workspace()
            .await
            .and_then(|workspace| {
                let path = params.text_document.uri.to_file_path().ok()?;
                Some(configuration_files(workspace.root()).contains(&path))
            })
            .unwrap_or(false);
        if params.text_document.uri.path().ends_with(".json") || is_config {
            self.reload().await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let (Some(workspace), Ok(path)) = (
            self.current_workspace().await,
            params.text_document.uri.to_file_path(),
        ) {
            workspace.close_document(&path);
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some((snapshot, key, _)) = self.key_at(&uri, position).await else {
            return Ok(None);
        };
        let translations = snapshot.catalog.translations(&key);
        if translations.is_empty() {
            return Ok(None);
        }
        let mut markdown = format!("### 🌍 `{}`\n\n", key.canonical());
        for entry in translations {
            markdown.push_str(&format!(
                "**{}**: {}\n\n",
                entry.locale,
                entry.value.display()
            ));
        }
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: None,
        }))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(workspace) = self.current_workspace().await else {
            return Ok(None);
        };
        let snapshot = workspace.snapshot();
        let Ok(path) = uri.to_file_path() else {
            return Ok(None);
        };
        let Some(document) = snapshot.documents.get(&path) else {
            return Ok(None);
        };
        let prefix = completion_prefix(&document.content, position).unwrap_or_default();
        let source_locale = &snapshot.config.source_locale;
        let mut items = snapshot
            .catalog
            .entries()
            .filter(|entry| &entry.locale == source_locale)
            .filter(|entry| entry.key.canonical().starts_with(&prefix) || prefix.is_empty())
            .map(|entry| CompletionItem {
                label: entry.key.canonical(),
                kind: Some(CompletionItemKind::TEXT),
                detail: Some(entry.value.display()),
                insert_text: Some(entry.key.canonical()),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| left.label.cmp(&right.label));
        items.dedup_by(|left, right| left.label == right.label);
        items.truncate(100);
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some((snapshot, key, _)) = self.key_at(&uri, position).await else {
            return Ok(None);
        };
        let mut locations = Vec::new();
        for entry in snapshot.catalog.translations(&key) {
            let Some(source) = snapshot.catalog.source(&entry.file) else {
                continue;
            };
            let Some(range) = byte_span_to_range(source, entry.key_span) else {
                continue;
            };
            if let Ok(uri) = Url::from_file_path(&entry.file) {
                locations.push(Location { uri, range });
            }
        }
        Ok(match locations.len() {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(locations.remove(0))),
            _ => Some(GotoDefinitionResponse::Array(locations)),
        })
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let actions = params
            .context
            .diagnostics
            .iter()
            .filter_map(|diagnostic| {
                let NumberOrString::String(code) = diagnostic.code.as_ref()? else {
                    return None;
                };
                if code != "missing-source-translation" {
                    return None;
                }
                let data = diagnostic.data.as_ref()?;
                let key = data.get("key")?.as_str()?;
                Some(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Add missing source key '{key}'"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    command: Some(Command {
                        title: "Preview and add source key".to_string(),
                        command: "react-i18next-lens.addMissingSourceKey".to_string(),
                        arguments: Some(vec![data.clone()]),
                    }),
                    ..Default::default()
                }))
            })
            .collect::<Vec<_>>();
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        if params.command != "react-i18next-lens.addMissingSourceKey" {
            return Ok(None);
        }
        let Some(argument) = params.arguments.first() else {
            return Ok(None);
        };
        let raw_key = argument
            .as_str()
            .or_else(|| argument.get("key").and_then(Value::as_str));
        let Some(raw_key) = raw_key else {
            return Ok(None);
        };
        let default_value = argument
            .get("defaultValue")
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(workspace) = self.current_workspace().await else {
            return Ok(None);
        };
        let snapshot = workspace.snapshot();
        let Some(key) = TranslationKey::from_source(
            raw_key,
            None,
            None,
            &snapshot.config.default_namespace,
            snapshot.config.namespace_separator,
            snapshot.config.key_separator,
        ) else {
            return Ok(None);
        };
        let request = AddMissingKey {
            key,
            default_value,
            translations: HashMap::new(),
        };
        let preview = match workspace.preview_mutation(&request) {
            Ok(preview) => preview,
            Err(error) => {
                self.client
                    .show_message(
                        MessageType::ERROR,
                        format!("Could not preview key: {error:?}"),
                    )
                    .await;
                return Ok(None);
            }
        };
        let changes = preview
            .edits
            .iter()
            .map(|edit| {
                format!(
                    "{}\nBefore:\n{}\nAfter:\n{}",
                    edit.file.display(),
                    truncate(&edit.before, 1200),
                    truncate(&edit.after, 1200)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let apply = self
            .client
            .show_message_request(
                MessageType::INFO,
                format!(
                    "Preview: add '{}' to {} file(s):\n\n{}",
                    raw_key,
                    preview.edits.len(),
                    changes
                ),
                Some(vec![
                    MessageActionItem {
                        title: "Apply".to_string(),
                        properties: Default::default(),
                    },
                    MessageActionItem {
                        title: "Cancel".to_string(),
                        properties: Default::default(),
                    },
                ]),
            )
            .await
            .ok()
            .flatten()
            .is_some_and(|action| action.title == "Apply");
        if !apply {
            return Ok(None);
        }
        match workspace.apply_mutation(&preview) {
            Ok(_) => {
                self.re_diagnose_open_documents().await;
                if *self.inlay_hint_refresh_supported.read().await {
                    self.client.inlay_hint_refresh().await.ok();
                }
            }
            Err(error) => {
                self.client
                    .show_message(MessageType::ERROR, format!("Could not add key: {error:?}"))
                    .await;
            }
        }
        Ok(None)
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let Some(workspace) = self.current_workspace().await else {
            return Ok(None);
        };
        let snapshot = workspace.snapshot();
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return Ok(None);
        };
        let (Some(document), Some(analysis)) =
            (snapshot.documents.get(&path), snapshot.analyses.get(&path))
        else {
            return Ok(None);
        };
        let hints = analysis
            .usages
            .iter()
            .filter_map(|usage| {
                let KeyResolution::Static(key) = &usage.resolution else {
                    return None;
                };
                let entry = snapshot.catalog.get(&snapshot.config.source_locale, key)?;
                let range = byte_span_to_range(&document.content, usage.expression_span)?;
                if !ranges_overlap(range, params.range) {
                    return None;
                }
                Some(InlayHint {
                    position: range.end,
                    label: InlayHintLabel::String(format!(
                        " = {}",
                        truncate(&entry.value.display(), 30)
                    )),
                    kind: Some(InlayHintKind::TYPE),
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(true),
                    padding_right: None,
                    data: None,
                })
            })
            .collect();
        Ok(Some(hints))
    }
}

impl I18nBackend {
    async fn key_at(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<(Arc<WorkspaceSnapshot>, TranslationKey, TranslationUsage)> {
        let workspace = self.current_workspace().await?;
        let snapshot = workspace.snapshot();
        let path = uri.to_file_path().ok()?;
        let document = snapshot.documents.get(&path)?;
        let analysis = snapshot.analyses.get(&path)?;
        let offset = position_to_byte(&document.content, position)?;
        let usage = analysis.usages.iter().find(|usage| {
            usage.expression_span.start as usize <= offset
                && offset <= usage.expression_span.end as usize
        })?;
        let KeyResolution::Static(key) = &usage.resolution else {
            return None;
        };
        Some((snapshot.clone(), key.clone(), usage.clone()))
    }
}

fn diagnostics_for(
    snapshot: &WorkspaceSnapshot,
    analysis: &SourceAnalysis,
    source: &str,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for usage in &analysis.usages {
        let KeyResolution::Static(key) = &usage.resolution else {
            continue;
        };
        let Some(range) = byte_span_to_range(source, usage.expression_span) else {
            continue;
        };
        if snapshot
            .catalog
            .get(&snapshot.config.source_locale, key)
            .is_none()
        {
            diagnostics.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String(
                    "missing-source-translation".to_string(),
                )),
                source: Some("react-i18next-lens".to_string()),
                message: format!("Source translation '{}' is missing", key.canonical()),
                data: Some(serde_json::json!({
                    "key": key.canonical(),
                    "defaultValue": usage.default_value
                })),
                ..Default::default()
            });
            continue;
        }
        let missing = snapshot
            .config
            .locales
            .iter()
            .filter(|locale| snapshot.catalog.get(locale, key).is_none())
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            diagnostics.push(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::HINT),
                code: Some(NumberOrString::String("incomplete-translation".to_string())),
                source: Some("react-i18next-lens".to_string()),
                message: format!(
                    "Translation '{}' is physically missing in: {}",
                    key.canonical(),
                    missing.join(", ")
                ),
                ..Default::default()
            });
        }
    }
    diagnostics
}

fn build_file_watchers(
    snapshot: &WorkspaceSnapshot,
    root: &Path,
    configuration_files: &[PathBuf],
    relative: bool,
) -> Vec<FileSystemWatcher> {
    let mut patterns = snapshot
        .config
        .resource_patterns
        .iter()
        .map(|pattern| pattern.replace("{locale}", "*").replace("{namespace}", "*"))
        .collect::<Vec<_>>();
    patterns.extend(configuration_files.iter().filter_map(|file| {
        file.strip_prefix(root)
            .ok()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
    }));
    patterns
        .into_iter()
        .map(|pattern| {
            let glob_pattern = if relative {
                Url::from_directory_path(root)
                    .ok()
                    .map(|base_uri| {
                        GlobPattern::Relative(RelativePattern {
                            base_uri: OneOf::Right(base_uri),
                            pattern: pattern.clone(),
                        })
                    })
                    .unwrap_or_else(|| GlobPattern::String(pattern.clone()))
            } else {
                GlobPattern::String(root.join(&pattern).to_string_lossy().replace('\\', "/"))
            };
            FileSystemWatcher {
                glob_pattern,
                kind: None,
            }
        })
        .collect()
}

fn position_to_byte(source: &str, position: Position) -> Option<usize> {
    let line_start = source
        .split_inclusive('\n')
        .take(position.line as usize)
        .map(str::len)
        .sum::<usize>();
    let line = source.get(line_start..)?.split('\n').next()?;
    let mut utf16 = 0_u32;
    for (byte, character) in line.char_indices() {
        if utf16 == position.character {
            return Some(line_start + byte);
        }
        utf16 += character.len_utf16() as u32;
        if utf16 > position.character {
            return None;
        }
    }
    (utf16 == position.character).then_some(line_start + line.len())
}

fn byte_span_to_range(source: &str, span: ByteSpan) -> Option<Range> {
    Some(Range {
        start: byte_to_position(source, span.start as usize)?,
        end: byte_to_position(source, span.end as usize)?,
    })
}

fn byte_to_position(source: &str, offset: usize) -> Option<Position> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let character = source[line_start..offset].encode_utf16().count() as u32;
    Some(Position { line, character })
}

fn completion_prefix(source: &str, position: Position) -> Option<String> {
    let offset = position_to_byte(source, position)?;
    let before = &source[..offset];
    let quote = before.rfind(['\'', '"'])?;
    Some(before[quote + 1..].to_string())
}

fn ranges_overlap(left: Range, right: Range) -> bool {
    position_leq(right.start, left.end) && position_leq(left.start, right.end)
}

fn position_leq(left: Position, right: Position) -> bool {
    left.line < right.line || (left.line == right.line && left.character <= right.character)
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    format!(
        "{}...",
        value
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_utf8_spans_to_utf16_ranges() {
        let source = "const 名称 = t('保存')";
        let start = source.find("保存").unwrap();
        let range = byte_span_to_range(
            source,
            ByteSpan::new(start as u32, (start + "保存".len()) as u32),
        )
        .unwrap();
        assert_eq!(range.start.character, 14);
        assert_eq!(range.end.character, 16);
        assert_eq!(position_to_byte(source, range.start), Some(start));
    }

    #[test]
    fn truncates_by_unicode_scalar_count() {
        assert_eq!(truncate("保存按钮文案", 5), "保存...");
    }
}
