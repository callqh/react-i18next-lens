use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use react_i18next_lens::audit::{AuditReport, FixSuggestion, MissingTranslation, PlaceholderIssue};
use react_i18next_lens::domain::TranslationKey;
use react_i18next_lens::mutation::{AddMissingKey, MutationPreview};
use react_i18next_lens::workspace::Workspace;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use walkdir::WalkDir;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct AuditParams {
    workspace: Option<PathBuf>,
    scope: Option<String>,
    include_suggestions: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct MissingTranslationsParams {
    workspace: Option<PathBuf>,
    locales: Vec<String>,
    include_context: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct SuggestTranslationFixesParams {
    workspace: Option<PathBuf>,
    key: String,
    target_locales: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ValidatePlaceholdersParams {
    workspace: Option<PathBuf>,
    key: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct PreviewMutationParams {
    workspace: Option<PathBuf>,
    key: String,
    default_value: Option<String>,
    translations: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ApplyMutationParams {
    preview_id: String,
}

#[derive(Debug, Serialize)]
struct ServerInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct ToolDefinition {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
}

#[derive(Debug, Serialize)]
struct ResourceDefinition {
    uri: String,
    name: String,
    description: String,
    mime_type: String,
}

#[derive(Debug, Serialize)]
struct ResourceContents {
    uri: String,
    mime_type: String,
    text: String,
}

#[derive(Debug, Serialize)]
struct FixSuggestionResponse {
    key: String,
    source_locale: String,
    source_value: String,
    target_locales: Vec<String>,
    files_to_edit: Vec<PathBuf>,
    suggestion: FixSuggestion,
}

struct McpServer {
    workspace_root: PathBuf,
    next_preview: AtomicU64,
    previews: Mutex<std::collections::HashMap<String, StoredPreview>>,
}

struct StoredPreview {
    root: PathBuf,
    workspace_fingerprint: String,
    preview: MutationPreview,
}

impl McpServer {
    fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            next_preview: AtomicU64::new(1),
            previews: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn handle_request(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let id = request.id.clone();

        let response = match request.method.as_str() {
            "initialize" => self.handle_initialize(request.params),
            "notifications/initialized" => Ok(Value::Null),
            "ping" => Ok(json!({ "pong": true })),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.handle_tool_call(request.params),
            "resources/list" => self.handle_resources_list(),
            "resources/read" => self.handle_resource_read(request.params),
            method => Err(anyhow!("Unsupported method: {method}")),
        };

        match response {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32000,
                    message: error.to_string(),
                    data: None,
                }),
            },
        }
    }

    fn handle_initialize(&self, _params: Option<Value>) -> Result<Value> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": ServerInfo {
                name: "react-i18next-lens-mcp",
                version: env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "listChanged": false }
            }
        }))
    }

    fn handle_tool_call(&self, params: Option<Value>) -> Result<Value> {
        let params = params.unwrap_or(Value::Null);
        let tool_name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing tool name"))?;
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

        let result = match tool_name {
            "audit_i18n" => {
                let arguments: AuditParams = serde_json::from_value(arguments)?;
                self.audit_i18n(arguments)?
            }
            "get_missing_translations" => {
                let arguments: MissingTranslationsParams = serde_json::from_value(arguments)?;
                self.get_missing_translations(arguments)?
            }
            "suggest_translation_fixes" => {
                let arguments: SuggestTranslationFixesParams = serde_json::from_value(arguments)?;
                self.suggest_translation_fixes(arguments)?
            }
            "validate_placeholders" => {
                let arguments: ValidatePlaceholdersParams = serde_json::from_value(arguments)?;
                self.validate_placeholders(arguments)?
            }
            "preview_add_missing_key" => {
                let arguments: PreviewMutationParams = serde_json::from_value(arguments)?;
                self.preview_add_missing_key(arguments)?
            }
            "apply_mutation" => {
                let arguments: ApplyMutationParams = serde_json::from_value(arguments)?;
                self.apply_mutation(arguments)?
            }
            name => return Err(anyhow!("Unknown tool: {name}")),
        };

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&result)?
            }],
            "structuredContent": result,
            "isError": false
        }))
    }

    fn handle_resources_list(&self) -> Result<Value> {
        let resources = vec![
            ResourceDefinition {
                uri: "react-i18next-lens://config".to_string(),
                name: "React i18next Lens Config".to_string(),
                description: "Resolved i18n configuration for the current workspace".to_string(),
                mime_type: "application/json".to_string(),
            },
            ResourceDefinition {
                uri: "react-i18next-lens://audit/latest".to_string(),
                name: "Latest Audit Report".to_string(),
                description: "Fresh audit report generated from the current workspace".to_string(),
                mime_type: "application/json".to_string(),
            },
            ResourceDefinition {
                uri: "react-i18next-lens://translations/index".to_string(),
                name: "Translation Inventory".to_string(),
                description: "Loaded locales and translation key count".to_string(),
                mime_type: "application/json".to_string(),
            },
        ];

        Ok(json!({ "resources": resources }))
    }

    fn handle_resource_read(&self, params: Option<Value>) -> Result<Value> {
        let uri = params
            .as_ref()
            .and_then(|value| value.get("uri"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing resource uri"))?;

        let contents = match uri {
            "react-i18next-lens://config" => {
                let workspace = self.load_workspace(&self.workspace_root)?;
                let snapshot = workspace.snapshot();
                vec![ResourceContents {
                    uri: uri.to_string(),
                    mime_type: "application/json".to_string(),
                    text: serde_json::to_string_pretty(&*snapshot.config)?,
                }]
            }
            "react-i18next-lens://audit/latest" => {
                let report = self.build_report(&self.workspace_root)?;
                vec![ResourceContents {
                    uri: uri.to_string(),
                    mime_type: "application/json".to_string(),
                    text: serde_json::to_string_pretty(&report)?,
                }]
            }
            "react-i18next-lens://translations/index" => {
                let workspace = self.load_workspace(&self.workspace_root)?;
                let snapshot = workspace.snapshot();
                let payload = json!({
                    "workspace": self.workspace_root,
                    "locales": snapshot.config.locales,
                    "total_keys": snapshot.catalog.entries().map(|entry| entry.key.clone()).collect::<std::collections::HashSet<_>>().len()
                });
                vec![ResourceContents {
                    uri: uri.to_string(),
                    mime_type: "application/json".to_string(),
                    text: serde_json::to_string_pretty(&payload)?,
                }]
            }
            _ => return Err(anyhow!("Unknown resource uri: {uri}")),
        };

        Ok(json!({ "contents": contents }))
    }

    fn audit_i18n(&self, params: AuditParams) -> Result<Value> {
        let workspace = self.resolve_workspace(params.workspace);
        let report = self.build_report(&workspace)?;

        let include_suggestions = params.include_suggestions;
        let scope = params.scope.unwrap_or_else(|| "workspace".to_string());

        let missing = if include_suggestions {
            serde_json::to_value(&report.missing)?
        } else {
            serde_json::to_value(strip_missing_suggestions(&report.missing))?
        };

        Ok(json!({
            "workspace": workspace,
            "scope": scope,
            "summary": report.summary,
            "missing": missing,
            "unused": report.unused,
            "placeholder_issues": report.placeholder_issues
        }))
    }

    fn get_missing_translations(&self, params: MissingTranslationsParams) -> Result<Value> {
        let workspace = self.resolve_workspace(params.workspace);
        let report = self.build_report(&workspace)?;

        let filtered: Vec<Value> = report
            .missing
            .into_iter()
            .filter_map(|item| {
                let missing_in = if params.locales.is_empty() {
                    item.missing_in.clone()
                } else {
                    item.missing_in
                        .iter()
                        .filter(|locale| params.locales.contains(*locale))
                        .cloned()
                        .collect()
                };

                if missing_in.is_empty() {
                    return None;
                }

                let mut value = json!({
                    "key": item.key,
                    "source_locale": item.source_locale,
                    "source_value": item.source_value,
                    "missing_in": missing_in,
                });

                if params.include_context {
                    value["used_in"] = serde_json::to_value(item.used_in).ok()?;
                    value["suggestion"] = serde_json::to_value(item.suggestion).ok()?;
                }

                Some(value)
            })
            .collect();

        Ok(json!({
            "workspace": workspace,
            "requested_locales": params.locales,
            "count": filtered.len(),
            "missing": filtered
        }))
    }

    fn suggest_translation_fixes(&self, params: SuggestTranslationFixesParams) -> Result<Value> {
        if params.key.trim().is_empty() {
            return Err(anyhow!("'key' is required"));
        }

        let workspace = self.resolve_workspace(params.workspace);
        let report = self.build_report(&workspace)?;
        let missing = report
            .missing
            .into_iter()
            .find(|item| item.key == params.key)
            .ok_or_else(|| anyhow!("No missing translation found for key '{}'", params.key))?;

        let target_locales = if params.target_locales.is_empty() {
            missing.missing_in.clone()
        } else {
            let filtered: Vec<String> = missing
                .missing_in
                .iter()
                .filter(|locale| params.target_locales.contains(*locale))
                .cloned()
                .collect();

            if filtered.is_empty() {
                return Err(anyhow!(
                    "Key '{}' is not missing in the requested locales",
                    params.key
                ));
            }

            filtered
        };

        let mut files_to_edit = Vec::new();
        for locale in &target_locales {
            if let Some(path) = find_locale_file(&workspace, locale) {
                files_to_edit.push(path);
            }
        }

        let suggestion = missing.suggestion.unwrap_or(FixSuggestion {
            action: "add_translation".to_string(),
            files_to_edit: files_to_edit.clone(),
            context: Some(format!("Translation for '{}'", params.key)),
        });

        let response = FixSuggestionResponse {
            key: params.key,
            source_locale: missing.source_locale,
            source_value: missing.source_value,
            target_locales,
            files_to_edit,
            suggestion,
        };

        Ok(serde_json::to_value(response)?)
    }

    fn validate_placeholders(&self, params: ValidatePlaceholdersParams) -> Result<Value> {
        if params.key.trim().is_empty() {
            return Err(anyhow!("'key' is required"));
        }

        let workspace = self.resolve_workspace(params.workspace);
        let report = self.build_report(&workspace)?;
        let issues: Vec<PlaceholderIssue> = report
            .placeholder_issues
            .into_iter()
            .filter(|issue| issue.key == params.key)
            .collect();

        let valid = issues.is_empty();

        Ok(json!({
            "workspace": workspace,
            "key": params.key,
            "valid": valid,
            "issues": issues
        }))
    }

    fn preview_add_missing_key(&self, params: PreviewMutationParams) -> Result<Value> {
        if params.key.trim().is_empty() {
            return Err(anyhow!("'key' is required"));
        }
        let root = self.resolve_workspace(params.workspace);
        let workspace_fingerprint = fingerprint_workspace(&root)?;
        let workspace = self.load_workspace(&root)?;
        let snapshot = workspace.snapshot();
        let key = TranslationKey::from_source(
            &params.key,
            None,
            None,
            &snapshot.config.default_namespace,
            snapshot.config.namespace_separator,
            snapshot.config.key_separator,
        )
        .ok_or_else(|| anyhow!("invalid translation key: {}", params.key))?;
        let preview = workspace
            .preview_mutation(&AddMissingKey {
                key,
                default_value: params.default_value,
                translations: params.translations,
            })
            .map_err(|error| anyhow!("mutation preview failed: {error:?}"))?;
        let preview_id = format!(
            "preview-{}",
            self.next_preview.fetch_add(1, Ordering::Relaxed)
        );
        if fingerprint_workspace(&root)? != workspace_fingerprint {
            return Err(anyhow!(
                "workspace changed while creating the preview; try again"
            ));
        }
        self.previews
            .lock()
            .map_err(|_| anyhow!("preview store is unavailable"))?
            .insert(
                preview_id.clone(),
                StoredPreview {
                    root,
                    workspace_fingerprint,
                    preview: preview.clone(),
                },
            );
        Ok(json!({ "preview_id": preview_id, "preview": preview }))
    }

    fn apply_mutation(&self, params: ApplyMutationParams) -> Result<Value> {
        let stored = self
            .previews
            .lock()
            .map_err(|_| anyhow!("preview store is unavailable"))?
            .remove(&params.preview_id)
            .ok_or_else(|| anyhow!("unknown or already applied preview: {}", params.preview_id))?;
        if fingerprint_workspace(&stored.root)? != stored.workspace_fingerprint {
            return Err(anyhow!(
                "workspace changed since preview; create a new mutation preview"
            ));
        }
        let workspace = self.load_workspace(&stored.root)?;
        let generation = workspace
            .apply_mutation(&stored.preview)
            .map_err(|error| anyhow!("mutation apply failed: {error:?}"))?;
        Ok(json!({
            "preview_id": params.preview_id,
            "applied": true,
            "generation": generation.value(),
            "files_changed": stored.preview.edits.len()
        }))
    }

    fn resolve_workspace(&self, override_path: Option<PathBuf>) -> PathBuf {
        override_path.unwrap_or_else(|| self.workspace_root.clone())
    }

    fn build_report(&self, workspace: &Path) -> Result<AuditReport> {
        let workspace = self.load_workspace(workspace)?;
        let snapshot = workspace.snapshot();
        Ok((*snapshot.audit).clone())
    }

    fn load_workspace(&self, root: &Path) -> Result<Workspace> {
        Workspace::load(root.to_path_buf()).map_err(|failure| {
            anyhow!(
                "configuration failed: {}",
                failure
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
    }
}

fn fingerprint_workspace(root: &Path) -> Result<String> {
    let mut files = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !entry
                .file_name()
                .to_str()
                .is_some_and(|name| matches!(name, ".git" | "node_modules" | "target"))
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    let mut digest = Sha256::new();
    for file in files {
        let relative = file.strip_prefix(root).unwrap_or(&file);
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(
            std::fs::read(&file)
                .with_context(|| format!("failed to fingerprint {}", file.display()))?,
        );
        digest.update([0]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn strip_missing_suggestions(items: &[MissingTranslation]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            json!({
                "key": item.key,
                "source_value": item.source_value,
                "source_locale": item.source_locale,
                "missing_in": item.missing_in,
                "used_in": item.used_in
            })
        })
        .collect()
}

fn find_locale_file(workspace: &Path, locale: &str) -> Option<PathBuf> {
    let core = Workspace::load(workspace.to_path_buf()).ok()?;
    let snapshot = core.snapshot();
    let file = snapshot
        .catalog
        .entries()
        .find(|entry| entry.locale == locale)
        .map(|entry| entry.file.clone())
        .or_else(|| {
            snapshot.config.resource_patterns.first().map(|pattern| {
                workspace.join(
                    pattern
                        .replace("{locale}", locale)
                        .replace("{namespace}", &snapshot.config.default_namespace),
                )
            })
        });
    file
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "audit_i18n",
            description: "Run a full i18n audit for the workspace and return missing, unused, and placeholder issues.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Workspace path. Defaults to current directory." },
                    "scope": { "type": "string", "description": "Audit scope label for clients." },
                    "include_suggestions": { "type": "boolean", "default": false }
                }
            }),
        },
        ToolDefinition {
            name: "get_missing_translations",
            description: "List missing translation keys, optionally filtered by locale and with source usage context.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workspace": { "type": "string" },
                    "locales": { "type": "array", "items": { "type": "string" }, "default": [] },
                    "include_context": { "type": "boolean", "default": false }
                }
            }),
        },
        ToolDefinition {
            name: "suggest_translation_fixes",
            description: "Return actionable file targets and source text for adding missing translations.",
            input_schema: json!({
                "type": "object",
                "required": ["key"],
                "properties": {
                    "workspace": { "type": "string" },
                    "key": { "type": "string" },
                    "target_locales": { "type": "array", "items": { "type": "string" }, "default": [] }
                }
            }),
        },
        ToolDefinition {
            name: "validate_placeholders",
            description: "Check placeholder consistency for a specific translation key across locales.",
            input_schema: json!({
                "type": "object",
                "required": ["key"],
                "properties": {
                    "workspace": { "type": "string" },
                    "key": { "type": "string" }
                }
            }),
        },
        ToolDefinition {
            name: "preview_add_missing_key",
            description: "Preview a safe add-missing-key mutation and return before/after edits plus a preview_id without writing files.",
            input_schema: json!({
                "type": "object",
                "required": ["key"],
                "properties": {
                    "workspace": { "type": "string" },
                    "key": { "type": "string" },
                    "default_value": { "type": "string" },
                    "translations": { "type": "object", "additionalProperties": { "type": "string" } }
                }
            }),
        },
        ToolDefinition {
            name: "apply_mutation",
            description: "Explicitly apply a previous preview after generation and fingerprint validation.",
            input_schema: json!({
                "type": "object",
                "required": ["preview_id"],
                "properties": { "preview_id": { "type": "string" } }
            }),
        },
    ]
}

fn read_message(reader: &mut impl Read) -> Result<Option<JsonRpcRequest>> {
    let mut content_length = None;
    let mut header_buffer = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 {
            if header_buffer.is_empty() {
                return Ok(None);
            }
            return Err(anyhow!("Unexpected EOF while reading headers"));
        }

        header_buffer.push(byte[0]);

        if header_buffer.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let headers = String::from_utf8(header_buffer).context("Headers were not valid UTF-8")?;
    for line in headers.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
    }

    let content_length = content_length.ok_or_else(|| anyhow!("Missing Content-Length header"))?;
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    let request: JsonRpcRequest = serde_json::from_slice(&body)?;
    if request.jsonrpc.as_deref().unwrap_or("2.0") != "2.0" {
        return Err(anyhow!("Only JSON-RPC 2.0 requests are supported"));
    }

    Ok(Some(request))
}

fn write_message(writer: &mut impl Write, response: &JsonRpcResponse) -> Result<()> {
    let body = serde_json::to_vec(response)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive("react_i18next_lens=info".parse()?))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let workspace_root = std::env::current_dir().context("Failed to resolve current directory")?;
    let server = McpServer::new(workspace_root);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    while let Some(request) = read_message(&mut reader)? {
        let is_notification = request.id.is_none();
        let response = server.handle_request(request);
        if !is_notification {
            write_message(&mut writer, &response)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_tool_definitions() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 6);
        assert_eq!(tools[0].name, "audit_i18n");
    }

    #[test]
    fn parses_content_length_message() {
        let payload = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);
        let mut bytes = raw.as_bytes();
        let request = read_message(&mut bytes)
            .expect("request should parse")
            .expect("request should exist");

        assert_eq!(request.method, "ping");
        assert_eq!(request.id, Some(json!(1)));
    }

    #[test]
    fn strips_missing_suggestions_when_requested() {
        let stripped = strip_missing_suggestions(&[MissingTranslation {
            key: "common.save".to_string(),
            source_value: "Save".to_string(),
            source_locale: "en".to_string(),
            missing_in: vec!["vi".to_string()],
            used_in: vec![],
            suggestion: Some(FixSuggestion {
                action: "add_translation".to_string(),
                files_to_edit: vec![PathBuf::from("locales/vi.json")],
                context: None,
            }),
        }]);

        assert_eq!(stripped.len(), 1);
        assert!(stripped[0].get("suggestion").is_none());
    }

    #[test]
    fn writes_content_length_response() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0",
            id: Some(json!(1)),
            result: Some(json!({ "pong": true })),
            error: None,
        };
        let mut output = Vec::new();
        write_message(&mut output, &response).expect("response should serialize");

        let text = String::from_utf8(output).expect("utf8 output");
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n{\"jsonrpc\":\"2.0\""));
    }

    #[test]
    fn workspace_fingerprint_changes_with_workspace_content() {
        let root = std::env::temp_dir().join(format!(
            "react-i18next-lens-mcp-fingerprint-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("i18next.ts");
        std::fs::write(&file, "export default {};").unwrap();
        let before = fingerprint_workspace(&root).unwrap();
        std::fs::write(&file, "export default { fallbackLng: 'en' };").unwrap();
        let after = fingerprint_workspace(&root).unwrap();

        assert_ne!(before, after);
        std::fs::remove_dir_all(root).ok();
    }
}
