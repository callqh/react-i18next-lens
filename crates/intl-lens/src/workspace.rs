use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

use crate::analysis::{AnalyzerConfig, ReactSourceAnalyzer, SourceAnalysis};
use crate::catalog::TranslationCatalog;
use crate::configuration::{ConfigLoad, ConfigurationDiagnostic, WorkspaceConfig};

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
}

#[derive(Debug, Clone)]
pub struct AnalyzedDocument {
    pub document: OpenDocument,
    pub analysis: SourceAnalysis,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub generation: Generation,
    pub config: Arc<WorkspaceConfig>,
    pub documents: Arc<HashMap<PathBuf, AnalyzedDocument>>,
    pub catalog: Arc<TranslationCatalog>,
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
        let config = load_config(&root)?;
        let catalog = load_catalog(&root, &config)?;
        Ok(Self {
            root,
            next_generation: AtomicU64::new(2),
            write_lock: Mutex::new(()),
            snapshot: ArcSwap::from_pointee(WorkspaceSnapshot {
                generation: Generation(1),
                config: Arc::new(config),
                documents: Arc::new(HashMap::new()),
                catalog: Arc::new(catalog),
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
        documents.remove(path);
        self.publish(current.config.clone(), documents, current.catalog.clone())
    }

    pub fn reload(&self) -> Result<Generation, ReloadFailure> {
        let _write = self.write_lock.lock().expect("workspace writer poisoned");
        let config = load_config(&self.root)?;
        let catalog = Arc::new(load_catalog(&self.root, &config)?);
        let current = self.snapshot.load_full();
        let analyzer = analyzer(&config);
        let documents = current
            .documents
            .iter()
            .map(|(path, analyzed)| {
                (
                    path.clone(),
                    AnalyzedDocument {
                        document: analyzed.document.clone(),
                        analysis: analyzer.analyze(path, analyzed.document.content.as_str()),
                    },
                )
            })
            .collect();
        Ok(self.publish(Arc::new(config), documents, catalog))
    }

    fn update_document(&self, path: PathBuf, content: String, version: i32) -> Generation {
        let _write = self.write_lock.lock().expect("workspace writer poisoned");
        let current = self.snapshot.load_full();
        let analysis = analyzer(&current.config).analyze(&path, &content);
        let mut documents = (*current.documents).clone();
        documents.insert(
            path.clone(),
            AnalyzedDocument {
                document: OpenDocument {
                    path,
                    content,
                    version,
                },
                analysis,
            },
        );
        self.publish(current.config.clone(), documents, current.catalog.clone())
    }

    fn publish(
        &self,
        config: Arc<WorkspaceConfig>,
        documents: HashMap<PathBuf, AnalyzedDocument>,
        catalog: Arc<TranslationCatalog>,
    ) -> Generation {
        let generation = Generation::new(self.next_generation.fetch_add(1, Ordering::Relaxed));
        self.snapshot.store(Arc::new(WorkspaceSnapshot {
            generation,
            config,
            documents: Arc::new(documents),
            catalog,
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
            let analyzed = snapshot.documents.get(&path).unwrap();
            let expected = format!("common:value{}", analyzed.document.version);
            let actual = analyzed
                .analysis
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
    fn load_defers_source_analysis_until_a_document_is_opened() {
        let root = fixture("editor-load");
        let path = root.join("component.tsx");
        fs::write(
            &path,
            "import { useTranslation } from 'react-i18next'; const { t } = useTranslation('common'); t('value')",
        )
        .unwrap();

        let workspace = Workspace::load(root.clone()).unwrap();
        assert!(workspace.snapshot().documents.is_empty());

        workspace.open_document(path.clone(), fs::read_to_string(path).unwrap(), 1);
        assert_eq!(workspace.snapshot().documents.len(), 1);
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
