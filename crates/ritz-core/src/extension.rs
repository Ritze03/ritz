//! Extension discovery, loading, and validation.
//!
//! Layout in an extensions directory: any `.json` file at any depth is a
//! manifest, with its parent folder holding the `.sh` scripts it references.
//! Folders may be nested freely, e.g.:
//! - `default/core.json`                  — a simple extension (no scripts)
//! - `default/scripts/scripts.json`       — a complex extension + sibling `.sh`
//! - `cs2-custom-script/cs2-custom-script.json` — a user drop-in
//!
//! Multiple directories can be merged (e.g. shipped built-ins + user drop-ins);
//! later directories override earlier ones by extension id.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::condition;
use crate::error::{Result, RitzError};
use crate::schema::Extension;
use crate::variables::ui_requires_is_valid;

#[derive(Debug, Clone)]
pub struct LoadedExtension {
    pub spec: Extension,
    /// Directory holding the manifest (used to resolve hook/builder scripts).
    pub dir: PathBuf,
    /// The manifest file itself.
    pub manifest: PathBuf,
    /// The manifest's parent folder relative to the discovery root, used to
    /// build the GUI tree (e.g. `default/scripts`, `built-in/lsfg-vk`). Empty
    /// for a manifest sitting directly in the root.
    pub rel_dir: PathBuf,
    /// True when the manifest file stem matches its parent folder name
    /// (e.g. `scripts/scripts.json`). Used by the GUI to collapse the
    /// folder node and show the extension as a leaf at the parent level.
    pub is_folder_ext: bool,
}

/// Discover all extensions under a directory, recursing to any depth.
pub fn discover(root: &Path) -> Result<Vec<LoadedExtension>> {
    let mut out = Vec::new();
    discover_into(root, root, &mut out)?;
    Ok(out)
}

fn discover_into(dir: &Path, root: &Path, out: &mut Vec<LoadedExtension>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|source| RitzError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| RitzError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            discover_into(&path, root, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(load_file(&path, dir, root)?);
        }
    }
    Ok(())
}

/// Load and merge extensions from multiple directories. Later directories
/// override earlier by extension id. Returns extensions in stable id order.
pub fn load_all(dirs: &[PathBuf]) -> Result<Vec<LoadedExtension>> {
    let mut by_id: IndexMap<String, LoadedExtension> = IndexMap::new();
    for dir in dirs {
        for ext in discover(dir)? {
            by_id.insert(ext.spec.id(), ext);
        }
    }
    Ok(by_id.into_values().collect())
}

fn load_file(manifest: &Path, dir: &Path, root: &Path) -> Result<LoadedExtension> {
    let text = std::fs::read_to_string(manifest).map_err(|source| RitzError::Io {
        path: manifest.display().to_string(),
        source,
    })?;
    let spec: Extension = serde_json::from_str(&text).map_err(|source| RitzError::Json {
        path: manifest.display().to_string(),
        source,
    })?;
    validate(&spec)?;
    let rel_dir = dir.strip_prefix(root).unwrap_or(dir).to_path_buf();
    let is_folder_ext = manifest.file_stem().and_then(|s| s.to_str())
        == dir.file_name().and_then(|s| s.to_str());
    Ok(LoadedExtension {
        spec,
        dir: dir.to_path_buf(),
        manifest: manifest.to_path_buf(),
        rel_dir,
        is_folder_ext,
    })
}

fn check_requires(id: &str, requires: &Option<String>, allow_global: bool) -> Result<()> {
    let Some(req) = requires else { return Ok(()) };
    if allow_global {
        condition::parse(req).map_err(|e| RitzError::InvalidExtension {
            id: id.to_string(),
            reason: e.to_string(),
        })?;
    } else {
        ui_requires_is_valid(req).map_err(|e| RitzError::InvalidExtension {
            id: id.to_string(),
            reason: e.to_string(),
        })?;
    }
    Ok(())
}

/// Validate an extension: all `Requires` expressions parse; UI `Requires` reject
/// `global:`; variables are non-empty.
pub fn validate(ext: &Extension) -> Result<()> {
    let id = ext.id();

    for fields in ext.ui.values() {
        for field in fields {
            if field.variable.is_empty() {
                return Err(RitzError::InvalidExtension {
                    id: id.clone(),
                    reason: "UI field has an empty Variable".into(),
                });
            }
            // UI Requires must not use global:.
            check_requires(&id, &field.requires, false)?;
        }
    }

    for spec in ext.env_vars.iter().chain(ext.game_env_vars.iter()) {
        check_requires(&id, &spec.requires, true)?;
        for entry in &spec.builder {
            check_requires(&id, &entry.requires, true)?;
        }
    }
    for wrapper in &ext.wrappers {
        check_requires(&id, &wrapper.requires, true)?;
        for entry in &wrapper.builder {
            check_requires(&id, &entry.requires, true)?;
        }
    }
    for arg in &ext.game_launch_args {
        check_requires(&id, &arg.requires, true)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the workspace's bundled `resources/extensions/` directory.
    fn shipped_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../resources/extensions")
            .canonicalize()
            .expect("bundled extensions dir")
    }

    #[test]
    fn shipped_extensions_load_and_validate() {
        // Discovery recurses into default/ and built-in/ subfolders.
        let exts = discover(&shipped_dir()).expect("discover");
        let ids: Vec<String> = exts.iter().map(|e| e.spec.meta.name.clone()).collect();
        assert!(ids.iter().any(|n| n == "Gamescope"), "got: {ids:?}");
        assert!(ids.iter().any(|n| n == "AMD"), "got: {ids:?}");
        assert!(ids.iter().any(|n| n == "LSFG-VK"), "got: {ids:?}");
        assert!(exts.len() >= 6, "expected >=6 extensions, got {}", exts.len());

        // rel_dir reflects the nested layout used by the GUI tree.
        let scripts = exts.iter().find(|e| e.spec.meta.name == "Scripts").expect("scripts");
        assert_eq!(scripts.rel_dir, PathBuf::from("default/scripts"));
        let lsfg = exts.iter().find(|e| e.spec.meta.name == "LSFG-VK").expect("lsfg");
        assert!(
            lsfg.rel_dir.starts_with("built-in"),
            "lsfg rel_dir = {:?}",
            lsfg.rel_dir
        );
    }

    #[test]
    fn validate_rejects_global_in_ui_requires() {
        let ext: Extension = serde_json::from_value(serde_json::json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "UI": {"S": [{"Type": "toggle", "Variable": "a", "Requires": "global:b"}]}
        }))
        .unwrap();
        assert!(validate(&ext).is_err());
    }

    #[test]
    fn validate_allows_global_in_builder_requires() {
        let ext: Extension = serde_json::from_value(serde_json::json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "ENV_VARS": [{"Name": "FOO", "Requires": "global:b",
                "Builder": [{"Requires": "", "Type": "set", "Value": "1"}]}]
        }))
        .unwrap();
        assert!(validate(&ext).is_ok());
    }
}
