use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct LspProcess {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
}

impl LspProcess {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_react-i18next-lens"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("language server should start");
        let stdin = child.stdin.take().expect("language server stdin");
        let stdout = child.stdout.take().expect("language server stdout");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            while let Some(message) = read_message(&mut stdout) {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            messages,
        }
    }

    fn send(&mut self, message: Value) {
        let payload = serde_json::to_vec(&message).expect("valid JSON-RPC message");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", payload.len()).unwrap();
        self.stdin.write_all(&payload).unwrap();
        self.stdin.flush().unwrap();
    }

    fn receive_until(&self, predicate: impl Fn(&Value) -> bool) -> Option<Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let message = self.messages.recv_timeout(remaining).ok()?;
            if predicate(&message) {
                return Some(message);
            }
        }
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn closing_a_document_refreshes_client_inlay_cache_when_supported() {
    let (mut server, root, source_path) = open_translation_document(json!({
        "workspace": {
            "inlayHint": { "refreshSupport": true }
        }
    }));
    server.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didClose",
        "params": { "textDocument": { "uri": url(&source_path) } }
    }));

    let refresh = server.receive_until(|message| {
        message.get("method") == Some(&json!("workspace/inlayHint/refresh"))
    });
    assert!(
        refresh.is_some(),
        "didClose must invalidate Zed's cached hints before the document is reopened"
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn translation_inlay_hints_are_not_classified_as_type_hints() {
    let (mut server, root, source_path) = open_translation_document(json!({}));
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/inlayHint",
        "params": {
            "textDocument": { "uri": url(&source_path) },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 3, "character": 0 }
            }
        }
    }));

    let response = server
        .receive_until(|message| message.get("id") == Some(&json!(2)))
        .expect("inlay hint response");
    let hint = response["result"]
        .as_array()
        .and_then(|hints| hints.first())
        .expect("translation inlay hint");
    assert_eq!(hint["label"], json!(" = Save"));
    assert!(
        hint.get("kind").is_none(),
        "translation hints must remain visible when editor type hints are disabled"
    );

    fs::remove_dir_all(root).ok();
}

fn open_translation_document(capabilities: Value) -> (LspProcess, PathBuf, PathBuf) {
    let root = fixture();
    let source_path = root.join("component.tsx");
    let source = "import { useTranslation } from 'react-i18next';\nconst { t } = useTranslation('common');\nt('buttons.save');\n";
    fs::write(&source_path, source).unwrap();

    let mut server = LspProcess::spawn();
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "rootUri": url(&root),
            "capabilities": capabilities
        }
    }));
    assert!(server
        .receive_until(|message| message.get("id") == Some(&json!(1)))
        .is_some());
    server.send(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
    server.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": url(&source_path),
                "languageId": "typescriptreact",
                "version": 0,
                "text": source
            }
        }
    }));

    (server, root, source_path)
}

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let mut payload = vec![0; content_length?];
    reader.read_exact(&mut payload).ok()?;
    serde_json::from_slice(&payload).ok()
}

fn fixture() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "react-i18next-lens-lsp-lifecycle-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("locales/en")).unwrap();
    fs::write(
        root.join("react-i18next-lens.json"),
        r#"{
            "sourceLocale": "en",
            "locales": ["en"],
            "resources": ["locales/{locale}/{namespace}.json"],
            "defaultNamespace": "common"
        }"#,
    )
    .unwrap();
    fs::write(
        root.join("locales/en/common.json"),
        r#"{"buttons":{"save":"Save"}}"#,
    )
    .unwrap();
    root
}

fn url(path: &std::path::Path) -> String {
    tower_lsp::lsp_types::Url::from_file_path(path)
        .unwrap()
        .to_string()
}
