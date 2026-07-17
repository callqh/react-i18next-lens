use std::path::{Component, Path, PathBuf};

pub fn resolve_within_root(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let canonical_root = root.canonicalize().ok()?;
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        canonical_root.join(candidate)
    };
    let normalized = normalize(&joined)?;
    if !normalized.starts_with(&canonical_root) {
        return None;
    }

    let mut existing = normalized.as_path();
    while !existing.exists() {
        existing = existing.parent()?;
    }
    existing
        .canonicalize()
        .ok()?
        .starts_with(&canonical_root)
        .then_some(normalized)
}

fn normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn rejects_paths_outside_the_workspace() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("react-i18next-lens-path-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        assert!(resolve_within_root(&root, Path::new("locales/en/common.json")).is_some());
        assert!(resolve_within_root(&root, Path::new("../outside.json")).is_none());
        assert!(resolve_within_root(&root, Path::new("/tmp/outside.json")).is_none());
        fs::remove_dir_all(root).ok();
    }
}
