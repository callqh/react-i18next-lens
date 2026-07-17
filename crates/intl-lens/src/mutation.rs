use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::catalog::TranslationCatalog;
use crate::configuration::WorkspaceConfig;
use crate::domain::TranslationKey;
use crate::pathing::resolve_within_root;
use crate::workspace::Generation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddMissingKey {
    pub key: TranslationKey,
    pub default_value: Option<String>,
    pub translations: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileFingerprint {
    pub exists: bool,
    pub byte_length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MutationEdit {
    pub file: PathBuf,
    pub before: String,
    pub after: String,
    pub fingerprint: FileFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MutationPreview {
    pub generation: Generation,
    pub edits: Vec<MutationEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationError {
    UnknownLocale {
        locale: String,
    },
    InvalidTargetValue {
        locale: String,
    },
    NoResourceTemplate {
        locale: String,
    },
    InvalidResource {
        file: PathBuf,
        message: String,
    },
    KeyConflict {
        file: PathBuf,
        path: String,
    },
    StaleGeneration {
        expected: Generation,
        actual: Generation,
    },
    StaleFile {
        file: PathBuf,
    },
    Io {
        file: PathBuf,
        message: String,
    },
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TransactionRecord {
    temporary_files: Vec<PathBuf>,
    targets: Vec<TransactionTarget>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TransactionTarget {
    file: PathBuf,
    backup: Option<PathBuf>,
    after: FileFingerprint,
}

pub fn preview_add_missing_key(
    root: &Path,
    generation: Generation,
    config: &WorkspaceConfig,
    catalog: &TranslationCatalog,
    request: &AddMissingKey,
) -> Result<MutationPreview, MutationError> {
    for (locale, value) in &request.translations {
        if !config.locales.contains(locale) {
            return Err(MutationError::UnknownLocale {
                locale: locale.clone(),
            });
        }
        if locale != &config.source_locale
            && (value.trim().is_empty() || value == &request.key.canonical())
        {
            return Err(MutationError::InvalidTargetValue {
                locale: locale.clone(),
            });
        }
    }
    let mut values = request.translations.clone();
    values
        .entry(config.source_locale.clone())
        .or_insert_with(|| {
            request
                .default_value
                .clone()
                .unwrap_or_else(|| request.key.canonical())
        });

    let mut edits = Vec::new();
    for (locale, value) in values {
        if catalog.get(&locale, &request.key).is_some() {
            continue;
        }
        let file = resolve_resource_file(root, config, &locale, &request.key).ok_or_else(|| {
            MutationError::NoResourceTemplate {
                locale: locale.clone(),
            }
        })?;
        let before = match fs::read_to_string(&file) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(MutationError::Io {
                    file,
                    message: error.to_string(),
                });
            }
        };
        let after = add_value(&before, &request.key, &value, config.key_separator, &file)?;
        edits.push(MutationEdit {
            fingerprint: fingerprint(&before, !before.is_empty() || file.exists()),
            file,
            before,
            after,
        });
    }
    edits.sort_by(|left, right| left.file.cmp(&right.file));
    Ok(MutationPreview { generation, edits })
}

pub fn apply_preview(
    root: &Path,
    current_generation: Generation,
    preview: &MutationPreview,
) -> Result<(), MutationError> {
    if current_generation != preview.generation {
        return Err(MutationError::StaleGeneration {
            expected: preview.generation,
            actual: current_generation,
        });
    }

    for edit in &preview.edits {
        if resolve_within_root(root, &edit.file).as_ref() != Some(&edit.file) {
            return Err(MutationError::Io {
                file: edit.file.clone(),
                message: "mutation target escaped the workspace".to_string(),
            });
        }
        let current = fingerprint_file(&edit.file)?;
        if current != edit.fingerprint {
            return Err(MutationError::StaleFile {
                file: edit.file.clone(),
            });
        }
    }

    let mut prepared = Vec::new();
    for (index, edit) in preview.edits.iter().enumerate() {
        if let Some(parent) = edit.file.parent() {
            fs::create_dir_all(parent).map_err(|error| MutationError::Io {
                file: parent.to_path_buf(),
                message: error.to_string(),
            })?;
        }
        let temporary = sibling(&edit.file, "react-i18next-lens.tmp", index);
        let result = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .and_then(|mut output| {
                output.write_all(edit.after.as_bytes())?;
                output.sync_all()
            });
        if let Err(error) = result {
            for file in &prepared {
                fs::remove_file(file).ok();
            }
            fs::remove_file(&temporary).ok();
            return Err(MutationError::Io {
                file: temporary,
                message: error.to_string(),
            });
        }
        prepared.push(temporary);
        if let Some(parent) = edit.file.parent() {
            sync_directory(parent)?;
        }
    }

    let transaction_id = format!("{}-{}", std::process::id(), preview.generation.value());
    let transaction_directory = transaction_directory(root)?;
    let journal = transaction_directory.join(format!("transaction-{transaction_id}.json"));
    let committed = journal.with_extension("committed");
    let mut targets = Vec::new();
    for (index, edit) in preview.edits.iter().enumerate() {
        let backup = if edit.file.exists() {
            let backup = sibling(&edit.file, "react-i18next-lens.backup", index);
            if let Err(error) = copy_backup(&edit.file, &backup) {
                cleanup_files(&prepared);
                cleanup_backups(&targets);
                return Err(error);
            }
            if let Some(parent) = backup.parent() {
                if let Err(error) = sync_directory(parent) {
                    cleanup_files(&prepared);
                    cleanup_backups(&targets);
                    fs::remove_file(&backup).ok();
                    return Err(error);
                }
            }
            let backup_fingerprint = match fingerprint_file(&backup) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    cleanup_files(&prepared);
                    cleanup_backups(&targets);
                    fs::remove_file(backup).ok();
                    return Err(error);
                }
            };
            if backup_fingerprint.sha256 != edit.fingerprint.sha256
                || backup_fingerprint.byte_length != edit.fingerprint.byte_length
            {
                cleanup_files(&prepared);
                cleanup_backups(&targets);
                fs::remove_file(&backup).ok();
                return Err(MutationError::StaleFile {
                    file: edit.file.clone(),
                });
            }
            Some(backup)
        } else {
            None
        };
        targets.push(TransactionTarget {
            file: edit.file.clone(),
            backup,
            after: fingerprint(&edit.after, true),
        });
    }
    let transaction = TransactionRecord {
        temporary_files: prepared.clone(),
        targets,
    };
    if let Err(error) = write_synced_json(&journal, &transaction) {
        cleanup_transaction(&transaction);
        return Err(error);
    }
    sync_directory(&transaction_directory)?;

    for (index, edit) in preview.edits.iter().enumerate() {
        let temporary = &prepared[index];
        let current = match fingerprint_file(&edit.file) {
            Ok(current) => current,
            Err(error) => {
                finish_rollback(root, &transaction, &journal)?;
                return Err(error);
            }
        };
        if current != edit.fingerprint {
            finish_rollback(root, &transaction, &journal)?;
            return Err(MutationError::StaleFile {
                file: edit.file.clone(),
            });
        }
        if let Err(error) = atomic_replace(temporary, &edit.file) {
            finish_rollback(root, &transaction, &journal)?;
            return Err(MutationError::Io {
                file: edit.file.clone(),
                message: error.to_string(),
            });
        }
        if let Some(parent) = edit.file.parent() {
            if let Err(error) = sync_directory(parent) {
                finish_rollback(root, &transaction, &journal)?;
                return Err(error);
            }
        }
    }
    if let Err(error) = write_synced(&committed, b"committed") {
        finish_rollback(root, &transaction, &journal)?;
        return Err(error);
    }
    sync_directory(&transaction_directory)?;
    cleanup_transaction(&transaction);
    fs::remove_file(journal).ok();
    fs::remove_file(committed).ok();
    sync_directory(&transaction_directory)?;
    Ok(())
}

pub fn recover_pending_mutations(root: &Path) {
    let Ok(directory) = transaction_directory(root) else {
        return;
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("transaction-")
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(transaction) = serde_json::from_str::<TransactionRecord>(&content) else {
            fs::remove_file(&path).ok();
            continue;
        };
        if !validate_transaction(root, &transaction) {
            fs::remove_file(&path).ok();
            fs::remove_file(path.with_extension("committed")).ok();
            continue;
        }
        let committed = path.with_extension("committed");
        if committed.exists() {
            cleanup_transaction(&transaction);
        } else if rollback_transaction(root, &transaction).is_err() {
            continue;
        }
        fs::remove_file(path).ok();
        fs::remove_file(committed).ok();
    }
    sync_directory(&directory).ok();
}

fn rollback_transaction(root: &Path, transaction: &TransactionRecord) -> Result<(), MutationError> {
    if !validate_transaction(root, transaction) {
        return Err(MutationError::Io {
            file: root.to_path_buf(),
            message: "invalid mutation transaction".to_string(),
        });
    }
    for target in &transaction.targets {
        if fingerprint_file(&target.file).ok().as_ref() != Some(&target.after) {
            continue;
        }
        match &target.backup {
            Some(backup) if backup.exists() => {
                atomic_replace(backup, &target.file).map_err(|error| MutationError::Io {
                    file: target.file.clone(),
                    message: error.to_string(),
                })?;
            }
            None => match fs::remove_file(&target.file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(MutationError::Io {
                        file: target.file.clone(),
                        message: error.to_string(),
                    });
                }
            },
            Some(_) => {}
        }
        if let Some(parent) = target.file.parent() {
            sync_directory(parent)?;
        }
    }
    cleanup_transaction(transaction);
    Ok(())
}

fn finish_rollback(
    root: &Path,
    transaction: &TransactionRecord,
    journal: &Path,
) -> Result<(), MutationError> {
    rollback_transaction(root, transaction)?;
    fs::remove_file(journal).ok();
    if let Some(parent) = journal.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn validate_transaction(root: &Path, transaction: &TransactionRecord) -> bool {
    let valid_path = |path: &Path, marker: &str| {
        transaction_path_is_within(root, path)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(marker))
    };
    transaction
        .temporary_files
        .iter()
        .all(|path| valid_path(path, ".react-i18next-lens.tmp."))
        && transaction.targets.iter().all(|target| {
            transaction_path_is_within(root, &target.file)
                && target.file.extension().and_then(|value| value.to_str()) == Some("json")
                && target
                    .backup
                    .as_ref()
                    .is_none_or(|path| valid_path(path, ".react-i18next-lens.backup."))
        })
}

fn transaction_path_is_within(root: &Path, path: &Path) -> bool {
    resolve_within_root(root, path).is_some()
        || path
            .strip_prefix(root)
            .ok()
            .and_then(|relative| resolve_within_root(root, relative))
            .is_some()
}

fn transaction_directory(root: &Path) -> Result<PathBuf, MutationError> {
    let canonical = root.canonicalize().map_err(|error| MutationError::Io {
        file: root.to_path_buf(),
        message: error.to_string(),
    })?;
    let identity = format!(
        "{:x}",
        Sha256::digest(canonical.to_string_lossy().as_bytes())
    );
    let directory = std::env::temp_dir()
        .join("react-i18next-lens-transactions")
        .join(identity);
    fs::create_dir_all(&directory).map_err(|error| MutationError::Io {
        file: directory.clone(),
        message: error.to_string(),
    })?;
    Ok(directory)
}

fn cleanup_transaction(transaction: &TransactionRecord) {
    cleanup_files(&transaction.temporary_files);
    for backup in transaction
        .targets
        .iter()
        .filter_map(|target| target.backup.as_ref())
    {
        fs::remove_file(backup).ok();
    }
}

fn cleanup_backups(targets: &[TransactionTarget]) {
    for backup in targets.iter().filter_map(|target| target.backup.as_ref()) {
        fs::remove_file(backup).ok();
    }
}

fn write_synced_json(path: &Path, value: &impl serde::Serialize) -> Result<(), MutationError> {
    let content = serde_json::to_vec(value).expect("transaction record must serialize");
    write_synced(path, &content)
}

fn write_synced(path: &Path, content: &[u8]) -> Result<(), MutationError> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .and_then(|mut file| {
            file.write_all(content)?;
            file.sync_all()
        })
        .map_err(|error| MutationError::Io {
            file: path.to_path_buf(),
            message: error.to_string(),
        })
}

fn copy_backup(source: &Path, backup: &Path) -> Result<(), MutationError> {
    let mut input = OpenOptions::new()
        .read(true)
        .open(source)
        .map_err(|error| MutationError::Io {
            file: source.to_path_buf(),
            message: error.to_string(),
        })?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(backup)
        .map_err(|error| MutationError::Io {
            file: backup.to_path_buf(),
            message: error.to_string(),
        })?;
    let result = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all());
    if let Err(error) = result {
        fs::remove_file(backup).ok();
        return Err(MutationError::Io {
            file: backup.to_path_buf(),
            message: error.to_string(),
        });
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), MutationError> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| MutationError::Io {
            file: path.to_path_buf(),
            message: error.to_string(),
        })
}

#[cfg(unix)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn fingerprint_file(file: &Path) -> Result<FileFingerprint, MutationError> {
    match fs::read_to_string(file) {
        Ok(content) => Ok(fingerprint(&content, true)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(fingerprint("", false)),
        Err(error) => Err(MutationError::Io {
            file: file.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

fn cleanup_files(files: &[PathBuf]) {
    for file in files {
        fs::remove_file(file).ok();
    }
}

fn add_value(
    source: &str,
    key: &TranslationKey,
    value: &str,
    key_separator: Option<char>,
    file: &Path,
) -> Result<String, MutationError> {
    let normalized_source = if source.trim().is_empty() {
        "{}\n"
    } else {
        source
    };
    let root = if normalized_source.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(normalized_source).map_err(|error| MutationError::InvalidResource {
            file: file.to_path_buf(),
            message: error.to_string(),
        })?
    };
    let serde_json::Value::Object(object) = &root else {
        return Err(MutationError::InvalidResource {
            file: file.to_path_buf(),
            message: "resource root must be an object".to_string(),
        });
    };

    if key_separator.is_none() {
        if object.contains_key(key.path.as_str()) {
            return Err(MutationError::KeyConflict {
                file: file.to_path_buf(),
                path: key.path.as_str().to_string(),
            });
        }
        insert_preserving(normalized_source, key.path.as_str(), value, false).ok_or_else(|| {
            MutationError::InvalidResource {
                file: file.to_path_buf(),
                message: "could not locate the root JSON object".to_string(),
            }
        })
    } else {
        validate_nested_insert(object, key.path.as_str(), file)?;
        insert_preserving(normalized_source, key.path.as_str(), value, true).ok_or_else(|| {
            MutationError::InvalidResource {
                file: file.to_path_buf(),
                message: "could not locate the target JSON object".to_string(),
            }
        })
    }
}

fn validate_nested_insert(
    root: &serde_json::Map<String, serde_json::Value>,
    path: &str,
    file: &Path,
) -> Result<(), MutationError> {
    let mut segments = path.split('.').peekable();
    let mut current = root;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            if current.contains_key(segment) {
                return Err(MutationError::KeyConflict {
                    file: file.to_path_buf(),
                    path: path.to_string(),
                });
            }
            return Ok(());
        }
        let Some(node) = current.get(segment) else {
            return Ok(());
        };
        let serde_json::Value::Object(object) = node else {
            return Err(MutationError::KeyConflict {
                file: file.to_path_buf(),
                path: path.to_string(),
            });
        };
        current = object;
    }
    Ok(())
}

fn insert_preserving(content: &str, key: &str, value: &str, nested: bool) -> Option<String> {
    let parts = key.split('.').collect::<Vec<_>>();
    let indent = detect_indent(content);
    let root_open = content.find('{')?;
    let root_close = matching_brace(content, root_open)?;
    let mut target = (root_open, root_close);
    let mut depth_found = 0;
    if nested {
        for index in 0..parts.len().saturating_sub(1) {
            let Some(range) = find_nested_object_range(content, &parts[..=index]) else {
                break;
            };
            target = range;
            depth_found = index + 1;
        }
    }

    let remaining = if nested {
        &parts[depth_found..]
    } else {
        &parts[..]
    };
    let base_depth = if nested { depth_found + 1 } else { 1 };
    let encoded_value = serde_json::to_string(value).ok()?;
    let mut lines = Vec::new();
    for (index, segment) in remaining.iter().enumerate() {
        let encoded_key = if nested {
            serde_json::to_string(segment).ok()?
        } else {
            serde_json::to_string(key).ok()?
        };
        let level = base_depth + index;
        if !nested || index == remaining.len() - 1 {
            lines.push(format!(
                "{}{}: {}",
                indent.repeat(level),
                encoded_key,
                encoded_value
            ));
            break;
        }
        lines.push(format!("{}{}: {{", indent.repeat(level), encoded_key));
    }
    if nested {
        for index in (0..remaining.len().saturating_sub(1)).rev() {
            lines.push(format!("{}}}", indent.repeat(base_depth + index)));
        }
    }
    insert_before_closing_brace(content, target.0, target.1, &lines.join("\n"))
}

fn detect_indent(content: &str) -> String {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let leading = &line[..line.len() - trimmed.len()];
            (!leading.is_empty() && !trimmed.is_empty()).then(|| leading.to_string())
        })
        .min_by_key(String::len)
        .unwrap_or_else(|| "  ".to_string())
}

fn find_nested_object_range(content: &str, chain: &[&str]) -> Option<(usize, usize)> {
    let mut object_open = content.find('{')?;
    let mut result = None;
    for segment in chain {
        let object_close = matching_brace(content, object_open)?;
        let range = direct_object_property(content, object_open, object_close, segment)?;
        object_open = range.0;
        result = Some(range);
    }
    result
}

fn direct_object_property(
    content: &str,
    open: usize,
    close: usize,
    expected: &str,
) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        while cursor < close && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b',') {
            cursor += 1;
        }
        if cursor >= close {
            return None;
        }
        let key_end = string_token_end(content, cursor)?;
        let key = serde_json::from_str::<String>(&content[cursor..key_end]).ok()?;
        cursor = key_end;
        while cursor < close && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b':') {
            return None;
        }
        cursor += 1;
        while cursor < close && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let value_start = cursor;
        let value_end = json_value_end(content, value_start, close)?;
        if key == expected && bytes.get(value_start) == Some(&b'{') {
            return Some((value_start, value_end - 1));
        }
        cursor = value_end;
    }
    None
}

fn string_token_end(content: &str, start: usize) -> Option<usize> {
    if content.as_bytes().get(start) != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    for (relative, byte) in content.as_bytes()[start + 1..].iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(start + relative + 2);
        }
    }
    None
}

fn json_value_end(content: &str, start: usize, limit: usize) -> Option<usize> {
    match content.as_bytes().get(start)? {
        b'"' => string_token_end(content, start),
        b'{' => matching_brace(content, start).map(|close| close + 1),
        b'[' => matching_delimiter(content, start, b'[', b']').map(|close| close + 1),
        _ => (start..limit)
            .find(|index| matches!(content.as_bytes()[*index], b',' | b'}'))
            .or(Some(limit)),
    }
}

fn matching_delimiter(content: &str, open: usize, opening: u8, closing: u8) -> Option<usize> {
    let mut depth = 1_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (relative, byte) in content.as_bytes()[open + 1..].iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match *byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            value if value == opening && !in_string => depth += 1,
            value if value == closing && !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + relative + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn matching_brace(content: &str, open: usize) -> Option<usize> {
    let mut depth = 1_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (relative, character) in content[open + 1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + 1 + relative);
                }
            }
            _ => {}
        }
    }
    None
}

fn insert_before_closing_brace(
    content: &str,
    open: usize,
    close: usize,
    entry: &str,
) -> Option<String> {
    let inside = &content[open + 1..close];
    let closing_line_start = content[..close]
        .rfind('\n')
        .map_or(open + 1, |index| index + 1);
    let closing_prefix = &content[closing_line_start..close];
    let closing_indent = if closing_prefix.trim().is_empty() {
        closing_prefix
    } else {
        ""
    };
    let mut output = String::with_capacity(content.len() + entry.len() + 4);
    if inside.trim().is_empty() {
        output.push_str(&content[..open + 1]);
        output.push('\n');
        output.push_str(entry);
        output.push('\n');
        if closing_indent.trim().is_empty() {
            output.push_str(closing_indent);
        }
        output.push_str(&content[close..]);
        return Some(output);
    }

    let last_content = content[..close].rfind(|character: char| !character.is_whitespace())?;
    output.push_str(&content[..last_content + 1]);
    if content.as_bytes().get(last_content) != Some(&b',') {
        output.push(',');
    }
    output.push('\n');
    output.push_str(entry);
    output.push('\n');
    output.push_str(closing_indent);
    output.push_str(&content[close..]);
    Some(output)
}

fn resolve_resource_file(
    root: &Path,
    config: &WorkspaceConfig,
    locale: &str,
    key: &TranslationKey,
) -> Option<PathBuf> {
    let pattern = config.resource_patterns.first()?;
    let relative = pattern
        .replace("{locale}", locale)
        .replace("{namespace}", key.namespace.as_str());
    resolve_within_root(root, Path::new(&relative))
}

fn fingerprint(content: &str, exists: bool) -> FileFingerprint {
    FileFingerprint {
        exists,
        byte_length: content.len() as u64,
        sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
    }
}

fn sibling(file: &Path, suffix: &str, index: usize) -> PathBuf {
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("resource.json");
    file.with_file_name(format!(".{name}.{suffix}.{}.{index}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn previews_and_applies_only_safe_missing_values() {
        let root = fixture("apply");
        let config = config();
        let catalog = TranslationCatalog::load(&root, &config);
        let key = TranslationKey::from_source(
            "common:buttons.save",
            None,
            None,
            "common",
            Some(':'),
            Some('.'),
        )
        .unwrap();
        let request = AddMissingKey {
            key,
            default_value: Some("Save".to_string()),
            translations: HashMap::from([("ja".to_string(), "保存".to_string())]),
        };
        let preview =
            preview_add_missing_key(&root, Generation::new(7), &config, &catalog, &request)
                .unwrap();
        assert_eq!(preview.edits.len(), 2);
        assert!(preview
            .edits
            .iter()
            .all(|edit| edit.after.contains("buttons")));
        apply_preview(&root, Generation::new(7), &preview).unwrap();
        assert!(fs::read_to_string(root.join("locales/en/common.json"))
            .unwrap()
            .contains("Save"));
        assert!(fs::read_to_string(root.join("locales/ja/common.json"))
            .unwrap()
            .contains("保存"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_stale_generation_and_changed_files() {
        let root = fixture("stale");
        let config = config();
        let catalog = TranslationCatalog::load(&root, &config);
        let key =
            TranslationKey::from_source("common:new", None, None, "common", Some(':'), Some('.'))
                .unwrap();
        let preview = preview_add_missing_key(
            &root,
            Generation::new(4),
            &config,
            &catalog,
            &AddMissingKey {
                key,
                default_value: None,
                translations: HashMap::new(),
            },
        )
        .unwrap();
        assert!(matches!(
            apply_preview(&root, Generation::new(5), &preview),
            Err(MutationError::StaleGeneration { .. })
        ));
        fs::write(root.join("locales/en/common.json"), r#"{"other":"change"}"#).unwrap();
        assert!(matches!(
            apply_preview(&root, Generation::new(4), &preview),
            Err(MutationError::StaleFile { .. })
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn preserves_existing_format_and_key_order() {
        let source = "{\n\t\"buttons\": {\n\t\t\"cancel\": \"Cancel\"\n\t},\n\t\"other\": \"unchanged\"\n}\n";
        let key = TranslationKey::from_source(
            "common:buttons.save",
            None,
            None,
            "common",
            Some(':'),
            Some('.'),
        )
        .unwrap();
        let output = add_value(
            source,
            &key,
            "Save \"now\"",
            Some('.'),
            Path::new("common.json"),
        )
        .unwrap();

        assert!(output.starts_with("{\n\t\"buttons\": {\n\t\t\"cancel\": \"Cancel\","));
        assert!(output.contains("\t\t\"save\": \"Save \\\"now\\\"\""));
        assert!(output.ends_with("\t\"other\": \"unchanged\"\n}\n"));
        serde_json::from_str::<serde_json::Value>(&output).unwrap();
    }

    #[test]
    fn rejects_unknown_locales_and_target_placeholders() {
        let root = fixture("invalid-target");
        let config = config();
        let catalog = TranslationCatalog::load(&root, &config);
        let key =
            TranslationKey::from_source("common:new", None, None, "common", Some(':'), Some('.'))
                .unwrap();
        let unknown = AddMissingKey {
            key: key.clone(),
            default_value: None,
            translations: HashMap::from([("fr".to_string(), "Nouveau".to_string())]),
        };
        assert!(matches!(
            preview_add_missing_key(&root, Generation::new(1), &config, &catalog, &unknown),
            Err(MutationError::UnknownLocale { .. })
        ));
        let placeholder = AddMissingKey {
            key: key.clone(),
            default_value: None,
            translations: HashMap::from([("ja".to_string(), key.canonical())]),
        };
        assert!(matches!(
            preview_add_missing_key(&root, Generation::new(1), &config, &catalog, &placeholder),
            Err(MutationError::InvalidTargetValue { .. })
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn selects_only_direct_parent_objects_when_names_repeat() {
        let source = "{\n  \"wrapper\": { \"buttons\": { \"nested\": \"Nested\" } },\n  \"buttons\": { \"cancel\": \"Cancel\" }\n}\n";
        let key = TranslationKey::from_source(
            "common:buttons.save",
            None,
            None,
            "common",
            Some(':'),
            Some('.'),
        )
        .unwrap();
        let output = add_value(source, &key, "Save", Some('.'), Path::new("common.json")).unwrap();
        assert!(output.contains("\"buttons\": { \"nested\": \"Nested\" }"));
        assert!(output.contains("\"cancel\": \"Cancel\","));
        assert!(output.contains("\"save\": \"Save\""));
        serde_json::from_str::<serde_json::Value>(&output).unwrap();
    }

    #[test]
    fn recovers_an_interrupted_multi_file_transaction() {
        let root = fixture("transaction-recovery");
        let target = root.join("locales/en/common.json");
        let backup = sibling(&target, "react-i18next-lens.backup", 0);
        let temporary = sibling(&target, "react-i18next-lens.tmp", 0);
        fs::copy(&target, &backup).unwrap();
        fs::write(&target, r#"{"partial":"write"}"#).unwrap();
        fs::write(&temporary, "pending").unwrap();
        let transaction = TransactionRecord {
            temporary_files: vec![temporary.clone()],
            targets: vec![TransactionTarget {
                file: target.clone(),
                backup: Some(backup.clone()),
                after: fingerprint(r#"{"partial":"write"}"#, true),
            }],
        };
        assert!(validate_transaction(&root, &transaction));
        let journal = transaction_directory(&root)
            .unwrap()
            .join("transaction-test.json");
        fs::write(&journal, serde_json::to_vec(&transaction).unwrap()).unwrap();

        recover_pending_mutations(&root);

        assert_eq!(fs::read_to_string(&target).unwrap(), "{}\n");
        assert!(!backup.exists());
        assert!(!temporary.exists());
        assert!(!journal.exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rollback_never_overwrites_a_concurrent_edit() {
        let root = fixture("transaction-concurrent-edit");
        let target = root.join("locales/en/common.json");
        let backup = sibling(&target, "react-i18next-lens.backup", 0);
        fs::write(&backup, "{}\n").unwrap();
        fs::write(&target, r#"{"external":"edit"}"#).unwrap();
        let transaction = TransactionRecord {
            temporary_files: Vec::new(),
            targets: vec![TransactionTarget {
                file: target.clone(),
                backup: Some(backup.clone()),
                after: fingerprint(r#"{"our":"edit"}"#, true),
            }],
        };
        assert!(validate_transaction(&root, &transaction));

        rollback_transaction(&root, &transaction).unwrap();

        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"external":"edit"}"#
        );
        assert!(!backup.exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn recovery_rejects_paths_outside_the_workspace() {
        let root = fixture("transaction-path-escape");
        let outside = std::env::temp_dir().join("react-i18next-lens-do-not-touch.json");
        fs::write(&outside, "keep").unwrap();
        let transaction = TransactionRecord {
            temporary_files: Vec::new(),
            targets: vec![TransactionTarget {
                file: outside.clone(),
                backup: None,
                after: fingerprint("keep", true),
            }],
        };
        let journal = transaction_directory(&root)
            .unwrap()
            .join("transaction-crafted.json");
        fs::write(&journal, serde_json::to_vec(&transaction).unwrap()).unwrap();

        recover_pending_mutations(&root);

        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert!(!journal.exists());
        fs::remove_file(outside).ok();
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn backup_creation_never_follows_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;

        let root = fixture("transaction-backup-symlink");
        let config = config();
        let catalog = TranslationCatalog::load(&root, &config);
        let key =
            TranslationKey::from_source("common:new", None, None, "common", Some(':'), Some('.'))
                .unwrap();
        let preview = preview_add_missing_key(
            &root,
            Generation::new(1),
            &config,
            &catalog,
            &AddMissingKey {
                key,
                default_value: Some("New".to_string()),
                translations: HashMap::new(),
            },
        )
        .unwrap();
        let target = &preview.edits[0].file;
        let backup = sibling(target, "react-i18next-lens.backup", 0);
        let outside = std::env::temp_dir().join(format!(
            "react-i18next-lens-external-backup-target-{}",
            std::process::id()
        ));
        fs::write(&outside, "untouched").unwrap();
        symlink(&outside, &backup).unwrap();

        assert!(apply_preview(&root, Generation::new(1), &preview).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched");
        assert!(backup.symlink_metadata().unwrap().file_type().is_symlink());

        fs::remove_file(backup).ok();
        fs::remove_file(outside).ok();
        fs::remove_dir_all(root).ok();
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
        fs::write(root.join("locales/en/common.json"), "{}\n").unwrap();
        fs::write(root.join("locales/ja/common.json"), "{}\n").unwrap();
        root
    }
}
