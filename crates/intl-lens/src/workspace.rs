use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use walkdir::WalkDir;

use crate::analysis::{AnalyzerConfig, ReactSourceAnalyzer, SourceAnalysis};
use crate::audit::{audit_sources, AuditReport};
use crate::catalog::TranslationCatalog;
use crate::configuration::{ConfigLoad, ConfigurationDiagnostic, WorkspaceConfig};
use crate::mutation::{self, AddMissingKey, MutationError, MutationPreview};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct Generation(u64);

impl Generation {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDocument {
    pub path: PathBuf,
    pub content: String,
    pub version: i32,
    pub is_open: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub generation: Generation,
    pub config: Arc<WorkspaceConfig>,
    pub documents: Arc<HashMap<PathBuf, OpenDocument>>,
    pub analyses: Arc<HashMap<PathBuf, SourceAnalysis>>,
    pub catalog: Arc<TranslationCatalog>,
    pub audit: Arc<AuditReport>,
}

#[derive(Debug, Clone)]
pub struct ReloadFailure {
    pub diagnostics: Vec<ConfigurationDiagnostic>,
}

/// Owns the coherent application state shared by all delivery adapters.
///
/// Writers build an entire candidate while readers continue using the previous
/// generation. A single atomic swap publishes configuration, documents,
/// analyses, and catalog together.
pub struct Workspace {
    root: PathBuf,
    next_generation: AtomicU64,
    write_lock: Mutex<()>,
    snapshot: ArcSwap<WorkspaceSnapshot>,
}

impl Workspace {
    pub fn load(root: PathBuf) -> Result<Self, ReloadFailure> {
        crate::mutation::recover_pending_mutations(&root);
        let config = load_config(&root)?;
        let catalog = load_catalog(&root, &config)?;
        let (documents, analyses) = scan_source_tree(&root, &config);
        let audit = build_audit(&root, &config, &catalog, &documents, &analyses);
        Ok(Self {
            root,
            next_generation: AtomicU64::new(2),
            write_lock: Mutex::new(()),
            snapshot: ArcSwap::from_pointee(WorkspaceSnapshot {
                generation: Generation(1),
                config: Arc::new(config),
                documents: Arc::new(documents),
                analyses: Arc::new(analyses),
                catalog: Arc::new(catalog),
                audit: Arc::new(audit),
            }),
        })
    }

    pub fn snapshot(&self) -> Arc<WorkspaceSnapshot> {
        self.snapshot.load_full()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn open_document(&self, path: PathBuf, content: String, version: i32) -> Generation {
        self.update_document(path, content, version)
    }

    pub fn change_document(&self, path: PathBuf, content: String, version: i32) -> Generation {
        self.update_document(path, content, version)
    }

    pub fn close_document(&self, path: &Path) -> Generation {
        let _write = self.write_lock.lock().expect("workspace writer poisoned");
        let current = self.snapshot.load_full();
        let mut documents = (*current.documents).clone();
        let mut analyses = (*current.analyses).clone();
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let analysis = analyzer(&current.config).analyze(path, &content);
                documents.insert(
                    path.to_path_buf(),
                    OpenDocument {
                        path: path.to_path_buf(),
                        content,
                        version: 0,
                        is_open: false,
                    },
                );
                analyses.insert(path.to_path_buf(), analysis);
            }
            Err(_) => {
                documents.remove(path);
                analyses.remove(path);
            }
        }
        self.publish(
            current.config.clone(),
            documents,
            analyses,
            current.catalog.clone(),
        )
    }

    pub fn reload(&self) -> Result<Generation, ReloadFailure> {
        let _write = self.write_lock.lock().expect("workspace writer poisoned");
        let config = load_config(&self.root)?;
        let catalog = Arc::new(load_catalog(&self.root, &config)?);
        let current = self.snapshot.load_full();
        let (mut documents, _) = scan_source_tree(&self.root, &config);
        for (path, document) in current
            .documents
            .iter()
            .filter(|(_, document)| document.is_open)
        {
            documents.insert(path.clone(), document.clone());
        }
        let analyzer = analyzer(&config);
        let analyses = documents
            .iter()
            .map(|(path, document)| {
                (
                    path.clone(),
                    analyzer.analyze(path, document.content.as_str()),
                )
            })
            .collect();
        Ok(self.publish(Arc::new(config), documents, analyses, catalog))
    }

    pub fn preview_mutation(
        &self,
        request: &AddMissingKey,
    ) -> Result<MutationPreview, MutationError> {
        let snapshot = self.snapshot();
        mutation::preview_add_missing_key(
            &self.root,
            snapshot.generation,
            &snapshot.config,
            &snapshot.catalog,
            request,
        )
    }

    pub fn audit(&self) -> (Generation, Arc<AuditReport>) {
        let snapshot = self.snapshot();
        (snapshot.generation, snapshot.audit.clone())
    }

    pub fn apply_mutation(&self, preview: &MutationPreview) -> Result<Generation, MutationError> {
        let _write = self.write_lock.lock().expect("workspace writer poisoned");
        let current = self.snapshot.load_full();
        mutation::apply_preview(&self.root, current.generation, preview)?;
        let catalog = Arc::new(TranslationCatalog::load(&self.root, &current.config));
        Ok(self.publish(
            current.config.clone(),
            (*current.documents).clone(),
            (*current.analyses).clone(),
            catalog,
        ))
    }

    fn update_document(&self, path: PathBuf, content: String, version: i32) -> Generation {
        let _write = self.write_lock.lock().expect("workspace writer poisoned");
        let current = self.snapshot.load_full();
        let analysis = analyzer(&current.config).analyze(&path, &content);
        let mut documents = (*current.documents).clone();
        let mut analyses = (*current.analyses).clone();
        documents.insert(
            path.clone(),
            OpenDocument {
                path: path.clone(),
                content,
                version,
                is_open: true,
            },
        );
        analyses.insert(path, analysis);
        self.publish(
            current.config.clone(),
            documents,
            analyses,
            current.catalog.clone(),
        )
    }

    fn publish(
        &self,
        config: Arc<WorkspaceConfig>,
        documents: HashMap<PathBuf, OpenDocument>,
        analyses: HashMap<PathBuf, SourceAnalysis>,
        catalog: Arc<TranslationCatalog>,
    ) -> Generation {
        let generation = Generation::new(self.next_generation.fetch_add(1, Ordering::Relaxed));
        let audit = build_audit(&self.root, &config, &catalog, &documents, &analyses);
        self.snapshot.store(Arc::new(WorkspaceSnapshot {
            generation,
            config,
            documents: Arc::new(documents),
            analyses: Arc::new(analyses),
            catalog,
            audit: Arc::new(audit),
        }));
        generation
    }
}

fn analyzer(config: &WorkspaceConfig) -> ReactSourceAnalyzer {
    ReactSourceAnalyzer::new(AnalyzerConfig {
        default_namespace: config.default_namespace.clone(),
        namespace_separator: config.namespace_separator,
        key_separator: config.key_separator,
    })
}

fn scan_source_tree(
    root: &Path,
    config: &WorkspaceConfig,
) -> (
    HashMap<PathBuf, OpenDocument>,
    HashMap<PathBuf, SourceAnalysis>,
) {
    let analyzer = analyzer(config);
    let mut documents = HashMap::new();
    let mut analyses = HashMap::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_ignored(entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && is_source(entry.path()))
    {
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let path = entry.path().to_path_buf();
        analyses.insert(path.clone(), analyzer.analyze(&path, &content));
        documents.insert(
            path.clone(),
            OpenDocument {
                path,
                content,
                version: 0,
                is_open: false,
            },
        );
    }
    (documents, analyses)
}

fn build_audit(
    root: &Path,
    config: &WorkspaceConfig,
    catalog: &TranslationCatalog,
    documents: &HashMap<PathBuf, OpenDocument>,
    analyses: &HashMap<PathBuf, SourceAnalysis>,
) -> AuditReport {
    audit_sources(
        root,
        config,
        catalog,
        analyses.iter().filter_map(|(path, analysis)| {
            documents
                .get(path)
                .map(|document| (path.as_path(), document.content.as_str(), analysis))
        }),
    )
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

fn load_config(root: &Path) -> Result<WorkspaceConfig, ReloadFailure> {
    let ConfigLoad {
        config,
        diagnostics,
    } = WorkspaceConfig::load(root);
    if !diagnostics.is_empty() {
        return Err(ReloadFailure { diagnostics });
    }
    config.ok_or(ReloadFailure { diagnostics })
}

fn load_catalog(
    root: &Path,
    config: &WorkspaceConfig,
) -> Result<TranslationCatalog, ReloadFailure> {
    let catalog = TranslationCatalog::load(root, config);
    if !catalog.has_resource_for_locale(&config.source_locale) {
        return Err(ReloadFailure {
            diagnostics: vec![ConfigurationDiagnostic {
                path: root.to_path_buf(),
                message: format!(
                    "source locale '{}' has no readable JSON resource matching {:?}",
                    config.source_locale, config.resource_patterns
                ),
            }],
        });
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::domain::KeyResolution;

    use super::*;

    #[test]
    fn publishes_document_and_analysis_as_one_generation() {
        let root = fixture("coherent-snapshot");
        let workspace = Arc::new(Workspace::load(root.clone()).unwrap());
        let path = root.join("component.tsx");
        workspace.open_document(
            path.clone(),
            "import { useTranslation } from 'react-i18next'; const { t } = useTranslation('common'); t('value0')".to_string(),
            0,
        );

        let finished = Arc::new(AtomicBool::new(false));
        let writer_workspace = workspace.clone();
        let writer_path = path.clone();
        let writer_finished = finished.clone();
        let writer = thread::spawn(move || {
            for version in 1..100 {
                writer_workspace.change_document(
                    writer_path.clone(),
                    format!("import {{ useTranslation }} from 'react-i18next'; const {{ t }} = useTranslation('common'); t('value{version}')"),
                    version,
                );
            }
            writer_finished.store(true, Ordering::Release);
        });

        while !finished.load(Ordering::Acquire) {
            let snapshot = workspace.snapshot();
            let document = snapshot.documents.get(&path).unwrap();
            let analysis = snapshot.analyses.get(&path).unwrap();
            let expected = format!("common:value{}", document.version);
            let actual = analysis
                .usages
                .iter()
                .find_map(|usage| match &usage.resolution {
                    KeyResolution::Static(key) => Some(key.canonical()),
                    KeyResolution::Dynamic { .. } => None,
                })
                .unwrap();
            assert_eq!(actual, expected);
        }
        writer.join().unwrap();
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn failed_reload_keeps_the_previous_snapshot() {
        let root = fixture("failed-reload");
        let workspace = Workspace::load(root.clone()).unwrap();
        let before = workspace.snapshot();
        fs::write(root.join("react-i18next-lens.json"), "{}").unwrap();

        assert!(workspace.reload().is_err());
        assert_eq!(workspace.snapshot().generation, before.generation);
        assert_eq!(workspace.snapshot().config.source_locale, "en");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_workspace_without_source_locale_resources() {
        let root = fixture("missing-source-resource");
        fs::remove_file(root.join("locales/en/common.json")).unwrap();
        let failure = Workspace::load(root.clone()).err().unwrap();
        assert!(failure.diagnostics[0]
            .message
            .contains("source locale 'en'"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn audit_uses_open_buffer_from_the_same_snapshot() {
        let root = fixture("open-buffer-audit");
        let workspace = Workspace::load(root.clone()).unwrap();
        let path = root.join("component.tsx");
        workspace.open_document(
            path,
            "import { useTranslation } from 'react-i18next'; const { t } = useTranslation('common'); t('missing')".to_string(),
            1,
        );
        let snapshot = workspace.snapshot();
        let missing = snapshot
            .audit
            .missing
            .iter()
            .find(|item| item.key == "common:missing")
            .unwrap();
        assert_eq!(missing.used_in.len(), 1);
        assert_eq!(snapshot.generation, workspace.audit().0);
        fs::remove_dir_all(root).ok();
    }

    fn fixture(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("react-i18next-lens-{name}-{nonce}"));
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
        fs::write(root.join("locales/en/common.json"), r#"{"value":"Value"}"#).unwrap();
        root
    }
}
