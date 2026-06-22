//! Pure transform for the lsfg-vk `conf.toml`.
//!
//! lsfg-vk matches the running process by the `exe` field. Because Steam tracks
//! the ritz process, the exe is always `"RitzLauncher"` — the game inherits frame
//! generation from the launcher's process context.
//!
//! These functions read/modify/write the TOML text while preserving any other
//! `[[game]]` entries the user has. No threads, no file IO — that lives in the
//! app's lsfg backend.

use crate::error::{Result, RitzError};

/// The exe name ritz registers under.
pub const RITZ_EXE: &str = "RitzLauncher";

#[derive(Debug, Clone, PartialEq)]
pub struct LsfgSettings {
    pub multiplier: u32,
    pub flow_scale: Option<f64>,
    pub performance_mode: Option<bool>,
    pub hdr_mode: Option<bool>,
    /// `experimental_present_mode`; `None` leaves it unset.
    pub present_mode: Option<String>,
}

fn parse_doc(existing: &str) -> Result<toml::Table> {
    if existing.trim().is_empty() {
        return Ok(toml::Table::new());
    }
    existing
        .parse::<toml::Table>()
        .map_err(|e| RitzError::Condition(format!("lsfg conf.toml parse error: {e}")))
}

fn serialize(doc: &toml::Table) -> Result<String> {
    toml::to_string_pretty(doc)
        .map_err(|e| RitzError::Condition(format!("lsfg conf.toml serialize error: {e}")))
}

/// Find (or create) the ritz `[[game]]` entry, returning a mutable reference to
/// its table.
fn ritz_game_entry(doc: &mut toml::Table) -> &mut toml::Table {
    doc.entry("version")
        .or_insert_with(|| toml::Value::Integer(1));

    let games = doc
        .entry("game")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let arr = match games {
        toml::Value::Array(a) => a,
        other => {
            *other = toml::Value::Array(Vec::new());
            other.as_array_mut().unwrap()
        }
    };

    let idx = arr.iter().position(|g| {
        g.get("exe").and_then(|e| e.as_str()) == Some(RITZ_EXE)
    });
    let idx = match idx {
        Some(i) => i,
        None => {
            let mut t = toml::Table::new();
            t.insert("exe".into(), toml::Value::String(RITZ_EXE.into()));
            arr.push(toml::Value::Table(t));
            arr.len() - 1
        }
    };
    arr[idx].as_table_mut().expect("game entry is a table")
}

/// Write all settings into the ritz game entry; returns the new TOML text.
pub fn apply(existing: &str, settings: &LsfgSettings) -> Result<String> {
    let mut doc = parse_doc(existing)?;
    {
        let game = ritz_game_entry(&mut doc);
        game.insert("exe".into(), toml::Value::String(RITZ_EXE.into()));
        game.insert(
            "multiplier".into(),
            toml::Value::Integer(settings.multiplier as i64),
        );
        if let Some(fs) = settings.flow_scale {
            game.insert("flow_scale".into(), toml::Value::Float(fs));
        }
        if let Some(pm) = settings.performance_mode {
            game.insert("performance_mode".into(), toml::Value::Boolean(pm));
        }
        if let Some(hdr) = settings.hdr_mode {
            game.insert("hdr_mode".into(), toml::Value::Boolean(hdr));
        }
        if let Some(mode) = &settings.present_mode {
            game.insert(
                "experimental_present_mode".into(),
                toml::Value::String(mode.clone()),
            );
        }
    }
    serialize(&doc)
}

/// Update only the multiplier of the ritz game entry (the activation-delay
/// hot-patch).
pub fn set_multiplier(existing: &str, multiplier: u32) -> Result<String> {
    let mut doc = parse_doc(existing)?;
    {
        let game = ritz_game_entry(&mut doc);
        game.insert(
            "multiplier".into(),
            toml::Value::Integer(multiplier as i64),
        );
    }
    serialize(&doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> LsfgSettings {
        LsfgSettings {
            multiplier: 3,
            flow_scale: Some(0.8),
            performance_mode: Some(true),
            hdr_mode: Some(false),
            present_mode: Some("fifo".into()),
        }
    }

    #[test]
    fn apply_to_empty_creates_entry() {
        let out = apply("", &settings()).unwrap();
        let doc: toml::Table = out.parse().unwrap();
        let games = doc["game"].as_array().unwrap();
        assert_eq!(games.len(), 1);
        let g = games[0].as_table().unwrap();
        assert_eq!(g["exe"].as_str(), Some(RITZ_EXE));
        assert_eq!(g["multiplier"].as_integer(), Some(3));
        assert_eq!(g["flow_scale"].as_float(), Some(0.8));
        assert_eq!(g["experimental_present_mode"].as_str(), Some("fifo"));
    }

    #[test]
    fn apply_preserves_other_games() {
        let existing = r#"
version = 1
[[game]]
exe = "OtherGame"
multiplier = 2
"#;
        let out = apply(existing, &settings()).unwrap();
        let doc: toml::Table = out.parse().unwrap();
        let games = doc["game"].as_array().unwrap();
        assert_eq!(games.len(), 2, "other game preserved + ritz added");
        assert!(games
            .iter()
            .any(|g| g.get("exe").and_then(|e| e.as_str()) == Some("OtherGame")));
        assert!(games
            .iter()
            .any(|g| g.get("exe").and_then(|e| e.as_str()) == Some(RITZ_EXE)));
    }

    #[test]
    fn apply_updates_existing_ritz_entry() {
        let first = apply("", &settings()).unwrap();
        let mut s2 = settings();
        s2.multiplier = 4;
        let second = apply(&first, &s2).unwrap();
        let doc: toml::Table = second.parse().unwrap();
        let games = doc["game"].as_array().unwrap();
        assert_eq!(games.len(), 1, "ritz entry updated, not duplicated");
        assert_eq!(games[0]["multiplier"].as_integer(), Some(4));
    }

    #[test]
    fn set_multiplier_hot_patch() {
        // Activation delay: start at 1, then bump to target.
        let mut s = settings();
        s.multiplier = 1;
        let at_one = apply("", &s).unwrap();
        let bumped = set_multiplier(&at_one, 3).unwrap();
        let doc: toml::Table = bumped.parse().unwrap();
        let g = doc["game"].as_array().unwrap()[0].as_table().unwrap();
        assert_eq!(g["multiplier"].as_integer(), Some(3));
        // other settings preserved
        assert_eq!(g["flow_scale"].as_float(), Some(0.8));
    }
}
