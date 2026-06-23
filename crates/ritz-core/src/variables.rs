//! Resolved variable values and the per-extension lookup used during building.
//!
//! A [`ResolvedVar`] carries two things the builder needs: `truthy` (drives
//! `Requires` and empty-block skipping) and `value` (the string substituted for
//! `{var}` interpolation). The type-specific logic that produces these from raw
//! config + field type lives in `resolve.rs`; this module is the lookup surface.
//!
//! Scoping: a name is resolved within the current extension unless it begins with
//! `global:`, in which case it comes from the shared global map populated by
//! `global:`-declared fields of any extension.

use std::collections::HashMap;

use crate::schema::{FieldType, UiField};

pub const GLOBAL_PREFIX: &str = "global:";

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedVar {
    /// Drives `Requires` truthiness.
    pub truthy: bool,
    /// Value substituted for `{var}` interpolation (empty when not applicable).
    pub value: String,
}

impl ResolvedVar {
    pub fn new(truthy: bool, value: impl Into<String>) -> Self {
        Self {
            truthy,
            value: value.into(),
        }
    }

    pub fn falsy() -> Self {
        Self {
            truthy: false,
            value: String::new(),
        }
    }
}

/// Lookup surface for one extension's build pass: its own locals plus the shared
/// global map. The global map is typically shared (cloned `Rc`/owned copy) across
/// all extensions during a build.
#[derive(Debug, Clone, Default)]
pub struct VarStore {
    local: HashMap<String, ResolvedVar>,
    global: HashMap<String, ResolvedVar>,
}

impl VarStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_global(global: HashMap<String, ResolvedVar>) -> Self {
        Self {
            local: HashMap::new(),
            global,
        }
    }

    /// Insert a local variable (short name, no prefix).
    pub fn insert_local(&mut self, name: impl Into<String>, var: ResolvedVar) {
        self.local.insert(name.into(), var);
    }

    /// Insert a global variable. Accepts either the bare name or the
    /// `global:`-prefixed form; stored bare.
    pub fn insert_global(&mut self, name: impl Into<String>, var: ResolvedVar) {
        let name = name.into();
        let bare = name.strip_prefix(GLOBAL_PREFIX).unwrap_or(&name).to_string();
        self.global.insert(bare, var);
    }

    fn get(&self, name: &str) -> Option<&ResolvedVar> {
        if let Some(bare) = name.strip_prefix(GLOBAL_PREFIX) {
            self.global.get(bare)
        } else {
            self.local.get(name)
        }
    }

    /// Truthiness for `Requires`; unknown names are falsy.
    pub fn truthy(&self, name: &str) -> bool {
        self.get(name).map(|v| v.truthy).unwrap_or(false)
    }

    /// Interpolation value for `{var}`; unknown names yield "".
    pub fn value(&self, name: &str) -> &str {
        self.get(name).map(|v| v.value.as_str()).unwrap_or("")
    }

    /// A closure suitable for [`crate::condition`] evaluation.
    pub fn lookup_fn(&self) -> impl Fn(&str) -> bool + '_ {
        move |name: &str| self.truthy(name)
    }

    /// Replace every `{name}` occurrence with its value. `{{` and `}}` escape to
    /// literal braces. Unknown names interpolate to empty.
    pub fn interpolate(&self, template: &str) -> String {
        let mut out = String::with_capacity(template.len());
        let chars: Vec<char> = template.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '{' if chars.get(i + 1) == Some(&'{') => {
                    out.push('{');
                    i += 2;
                }
                '}' if chars.get(i + 1) == Some(&'}') => {
                    out.push('}');
                    i += 2;
                }
                '{' => {
                    // Read until closing brace.
                    let start = i + 1;
                    let mut j = start;
                    while j < chars.len() && chars[j] != '}' {
                        j += 1;
                    }
                    if j < chars.len() {
                        let name: String = chars[start..j].iter().collect();
                        out.push_str(self.value(name.trim()));
                        i = j + 1;
                    } else {
                        // No closing brace: emit literally.
                        out.push('{');
                        i += 1;
                    }
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }
}

/// Compute a [`ResolvedVar`] from a field definition and its raw stored value.
///
/// `raw` is the effective value after inheritance resolution:
/// - toggle: `"true"`/`"false"`
/// - non-toggle: `None` means the enable-checkbox is off; `Some(v)` means enabled
///   with value `v`.
pub fn resolve_field(field: &UiField, enabled: bool, raw: Option<&str>) -> ResolvedVar {
    match field.field_type {
        FieldType::Toggle => {
            let on = matches!(raw, Some("true")) || (raw.is_none() && enabled);
            ResolvedVar::new(on, if on { "true" } else { "false" })
        }
        FieldType::Selection => match (enabled, raw) {
            (true, Some(v)) if !v.is_empty() => ResolvedVar::new(true, v),
            _ => ResolvedVar::falsy(),
        },
        FieldType::Integer | FieldType::Float => match (enabled, raw) {
            (true, Some(v)) if !v.is_empty() => ResolvedVar::new(true, v),
            _ => ResolvedVar::falsy(),
        },
        FieldType::String | FieldType::MultiString => match (enabled, raw) {
            (true, Some(v)) if !v.is_empty() => ResolvedVar::new(true, v),
            _ => ResolvedVar::falsy(),
        },
    }
}

/// Validate that a UI-context `Requires` does not reference `global:` variables.
/// (Cross-extension dependencies are only allowed in the build phase.)
pub fn ui_requires_is_valid(requires: &str) -> crate::Result<()> {
    let parsed = crate::condition::parse(requires)?;
    if let Some(expr) = parsed {
        for v in expr.referenced_vars() {
            if v.starts_with(GLOBAL_PREFIX) {
                return Err(crate::RitzError::Condition(format!(
                    "`global:` reference `{v}` is not allowed in a UI Requires"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> VarStore {
        let mut s = VarStore::new();
        s.insert_local("gamescope_enabled", ResolvedVar::new(true, "true"));
        s.insert_local("backend", ResolvedVar::new(true, "sdl"));
        s.insert_local("sharpness", ResolvedVar::new(true, "5"));
        s.insert_global("global:hdr_on", ResolvedVar::new(true, "true"));
        s
    }

    #[test]
    fn truthy_and_value() {
        let s = store();
        assert!(s.truthy("gamescope_enabled"));
        assert!(!s.truthy("missing"));
        assert_eq!(s.value("backend"), "sdl");
        assert_eq!(s.value("missing"), "");
    }

    #[test]
    fn global_scoping() {
        let s = store();
        assert!(s.truthy("global:hdr_on"));
        // bare name (not global) is not found locally
        assert!(!s.truthy("hdr_on"));
    }

    #[test]
    fn interpolation() {
        let s = store();
        assert_eq!(s.interpolate("--backend {backend}"), "--backend sdl");
        assert_eq!(s.interpolate("--sharpness {sharpness}"), "--sharpness 5");
        assert_eq!(s.interpolate("{missing}x"), "x");
        assert_eq!(s.interpolate("{{literal}}"), "{literal}");
    }

    #[test]
    fn resolve_toggle() {
        let field = UiField {
            name: None,
            description: None,
            field_type: FieldType::Toggle,
            variable: "x".into(),
            default: None,
            options: None,
            display_options: None,
            requires: None,
        };
        assert!(resolve_field(&field, true, Some("true")).truthy);
        assert!(!resolve_field(&field, true, Some("false")).truthy);
    }

    #[test]
    fn ui_requires_rejects_global() {
        assert!(ui_requires_is_valid("a AND b").is_ok());
        assert!(ui_requires_is_valid("a AND global:b").is_err());
    }
}
