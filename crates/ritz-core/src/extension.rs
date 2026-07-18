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

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::condition;
use crate::error::{Result, RitzError};
use crate::schema::Extension;
use crate::variables::ui_requires_is_valid;

/// A non-fatal problem encountered while loading extensions. These are collected
/// rather than propagated so one bad manifest never aborts loading the rest of a
/// directory (directory-level IO errors stay fatal). Each renders to a single
/// banner line via [`fmt::Display`].
#[derive(Debug, Clone)]
pub enum ExtensionLoadError {
    /// A manifest failed to read, parse, or validate.
    Parse { path: PathBuf, reason: String },
    /// Two or more loaded modules share an (Author, Name) config identity
    /// (Version-blind), so they collide in the config namespace.
    Dup {
        author: String,
        name: String,
        paths: Vec<PathBuf>,
    },
}

impl fmt::Display for ExtensionLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExtensionLoadError::Parse { path, reason } => {
                write!(f, "failed to load {}: {}", path.display(), reason)
            }
            ExtensionLoadError::Dup {
                author,
                name,
                paths,
            } => {
                let joined = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "duplicate module {author}::{name} loaded from {joined}"
                )
            }
        }
    }
}

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
///
/// A single unparseable/invalid manifest is demoted to a collected
/// [`ExtensionLoadError`] rather than aborting the whole directory; only
/// directory-level IO failures (unreadable dir, bad entry) stay fatal.
pub fn discover(root: &Path) -> Result<(Vec<LoadedExtension>, Vec<ExtensionLoadError>)> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    discover_into(root, root, &mut out, &mut errors)?;
    Ok((out, errors))
}

fn discover_into(
    dir: &Path,
    root: &Path,
    out: &mut Vec<LoadedExtension>,
    errors: &mut Vec<ExtensionLoadError>,
) -> Result<()> {
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
            discover_into(&path, root, out, errors)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            // Per-file parse/validate failures are collected, not propagated.
            match load_file(&path, dir, root) {
                Ok(ext) => out.push(ext),
                Err(e) => errors.push(ExtensionLoadError::Parse {
                    path: path.clone(),
                    reason: e.to_string(),
                }),
            }
        }
    }
    Ok(())
}

/// Load and merge extensions from multiple directories. Later directories
/// override earlier by extension id. Returns extensions in stable id order,
/// plus every non-fatal load problem (per-file parse/validate failures and
/// post-merge duplicate-identity collisions).
pub fn load_all(dirs: &[PathBuf]) -> Result<(Vec<LoadedExtension>, Vec<ExtensionLoadError>)> {
    let mut by_id: IndexMap<String, LoadedExtension> = IndexMap::new();
    let mut errors = Vec::new();
    for dir in dirs {
        let (exts, errs) = discover(dir)?;
        errors.extend(errs);
        for ext in exts {
            by_id.insert(ext.spec.id(), ext);
        }
    }
    let loaded: Vec<LoadedExtension> = by_id.into_values().collect();

    // Post-merge duplicate detection, Version-blind: ids differ by Version, but
    // config keys on Author+Name only, so same-(Author,Name) modules share one
    // config namespace and collide. Group and flag any group with >1 member.
    let mut groups: IndexMap<(String, String), Vec<PathBuf>> = IndexMap::new();
    for ext in &loaded {
        groups
            .entry((ext.spec.meta.author.clone(), ext.spec.meta.name.clone()))
            .or_default()
            .push(ext.manifest.clone());
    }
    for ((author, name), paths) in groups {
        if paths.len() > 1 {
            errors.push(ExtensionLoadError::Dup {
                author,
                name,
                paths,
            });
        }
    }

    Ok((loaded, errors))
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
/// `global:`; variables are non-empty, unique across the module, and section
/// names don't collide.
///
/// The duplicate-section and duplicate-`Variable` checks guard against
/// hand-edited manifests (and future editor writes): a repeated `Variable` means
/// two fields fight over one config slot (data loss), and colliding section
/// names produce ambiguous/lost UI sections. Section collision is checked
/// case-insensitively and whitespace-trimmed — exact-duplicate JSON keys already
/// silently collapse in the `IndexMap` during parse, so the surviving
/// near-duplicates ("Audio" vs "audio ") are what validation can still catch.
pub fn validate(ext: &Extension) -> Result<()> {
    let id = ext.id();

    let mut seen_sections: HashSet<String> = HashSet::new();
    let mut seen_vars: HashSet<&str> = HashSet::new();
    for (section, fields) in &ext.ui {
        if !seen_sections.insert(section.trim().to_lowercase()) {
            return Err(RitzError::InvalidExtension {
                id: id.clone(),
                reason: format!("duplicate UI section name: {section:?}"),
            });
        }
        for field in fields {
            if field.variable.is_empty() {
                return Err(RitzError::InvalidExtension {
                    id: id.clone(),
                    reason: "UI field has an empty Variable".into(),
                });
            }
            if !seen_vars.insert(field.variable.as_str()) {
                return Err(RitzError::InvalidExtension {
                    id: id.clone(),
                    reason: format!("duplicate Variable across UI fields: {}", field.variable),
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
        let (exts, errors) = discover(&shipped_dir()).expect("discover");
        assert!(errors.is_empty(), "bundled modules produced errors: {errors:?}");
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

    #[test]
    fn discover_skips_and_warns_on_a_bad_file() {
        let tmp = std::env::temp_dir().join(format!("ritz-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // One valid manifest.
        std::fs::write(
            tmp.join("good.json"),
            r#"{"Extension": {"Name": "Good", "Author": "Ritze", "Version": "1.0"}}"#,
        )
        .unwrap();
        // One deliberately-broken manifest (malformed JSON).
        std::fs::write(tmp.join("bad.json"), "{ this is not json").unwrap();

        let (exts, errors) = discover(&tmp).expect("discover should not be fatal");
        assert_eq!(exts.len(), 1, "only the valid manifest should load");
        assert_eq!(exts[0].spec.meta.name, "Good");
        assert_eq!(errors.len(), 1, "the broken manifest should be reported");
        assert!(matches!(errors[0], ExtensionLoadError::Parse { .. }));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_rejects_duplicate_section_name() {
        // Distinct exact JSON keys that normalize to the same section name
        // (case/whitespace) — the near-duplicates that survive IndexMap parse.
        let ext: Extension = serde_json::from_value(serde_json::json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "UI": {
                "Audio": [{"Type": "toggle", "Variable": "a"}],
                "audio ": [{"Type": "toggle", "Variable": "b"}]
            }
        }))
        .unwrap();
        assert!(validate(&ext).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_variable() {
        let ext: Extension = serde_json::from_value(serde_json::json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "UI": {
                "S1": [{"Type": "toggle", "Variable": "dup"}],
                "S2": [{"Type": "integer", "Variable": "dup"}]
            }
        }))
        .unwrap();
        assert!(validate(&ext).is_err());
    }

    #[test]
    fn validate_accepts_clean_manifest() {
        let ext: Extension = serde_json::from_value(serde_json::json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "UI": {
                "Audio": [{"Type": "toggle", "Variable": "a"}],
                "Video": [
                    {"Type": "toggle", "Variable": "b"},
                    {"Type": "integer", "Variable": "c"}
                ]
            }
        }))
        .unwrap();
        assert!(validate(&ext).is_ok());
    }

    #[test]
    fn load_all_detects_version_blind_duplicates() {
        let tmp = std::env::temp_dir().join(format!("ritz-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Same Author+Name, differing Version → distinct ids but one config key.
        std::fs::write(
            tmp.join("a.json"),
            r#"{"Extension": {"Name": "Dup", "Author": "Ritze", "Version": "1.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("b.json"),
            r#"{"Extension": {"Name": "Dup", "Author": "Ritze", "Version": "2.0"}}"#,
        )
        .unwrap();

        let (exts, errors) = load_all(&[tmp.clone()]).expect("load_all");
        assert_eq!(exts.len(), 2, "both distinct-id modules load");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ExtensionLoadError::Dup { author, name, paths }
                    if author == "Ritze" && name == "Dup" && paths.len() == 2
            )),
            "expected a Dup error, got: {errors:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
