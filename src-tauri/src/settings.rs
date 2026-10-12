//! The app's own settings (`settings.json` in the data directory): the
//! things the engine does not persist itself. Written atomically, read
//! tolerantly (a missing or damaged file means defaults).

use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What the user chose for one network link, keyed by interface name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct LinkPref {
    /// A name the user gave the link.
    pub label: Option<String>,
    /// A CSS colour (`#rrggbb`) used for the link everywhere in the UI.
    pub color: Option<String>,
    pub enabled: bool,
    /// Bytes per second; `None` = unlimited.
    pub limit: Option<u64>,
}

impl Default for LinkPref {
    fn default() -> Self {
        Self {
            label: None,
            color: None,
            enabled: true,
            limit: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Settings {
    /// Where new packages go; `None` = the platform's Downloads folder.
    pub download_dir: Option<String>,
    /// Overall speed limit in bytes per second; `None` = unlimited.
    pub global_limit: Option<u64>,
    /// Connections per link. Applies on the next start.
    pub slots_per_link: u8,
    /// Connections in total. Applies on the next start.
    pub max_connections_total: u32,
    /// Resume interrupted downloads at launch. Applies on the next start.
    pub auto_resume: bool,
    /// Tick "extract after download" for archives in the add dialog.
    pub extract_by_default: bool,
    /// Tick "delete archive after extraction" in the add dialog.
    pub delete_archives_by_default: bool,
    /// `PATTERN=CONNS[:FILES]` rules. Apply on the next start.
    pub host_rules: Vec<String>,
    pub link_prefs: BTreeMap<String, LinkPref>,
    pub notifications: bool,
    /// Closing the window keeps the app running in the tray.
    pub close_to_tray: bool,
    /// Look for a new version a few seconds after launch.
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: None,
            global_limit: None,
            slots_per_link: 8,
            max_connections_total: 32,
            auto_resume: true,
            extract_by_default: true,
            delete_archives_by_default: false,
            host_rules: Vec::new(),
            link_prefs: BTreeMap::new(),
            notifications: true,
            close_to_tray: false,
            check_updates: true,
        }
    }
}

impl Settings {
    /// Whether `other` differs in a setting that only applies when the
    /// engine starts.
    #[must_use]
    pub fn needs_restart_for(&self, other: &Self) -> bool {
        self.slots_per_link != other.slots_per_link
            || self.max_connections_total != other.max_connections_total
            || self.auto_resume != other.auto_resume
            || self.host_rules != other.host_rules
    }

    /// Checks the values a form could get wrong.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=32).contains(&self.slots_per_link) {
            return Err("Connections per link must be between 1 and 32.".into());
        }
        if !(1..=512).contains(&self.max_connections_total) {
            return Err("Total connections must be between 1 and 512.".into());
        }
        if self.global_limit == Some(0) {
            return Err("A speed limit of 0 isn't valid; leave it empty for no limit.".into());
        }
        let mut hosts = nuxd_core::config::HostPolicy::default();
        for rule in &self.host_rules {
            hosts.add_rule_text(rule)?;
        }
        for (name, pref) in &self.link_prefs {
            if pref.limit == Some(0) {
                return Err(format!("{name}: a speed limit of 0 isn't valid."));
            }
            if let Some(color) = &pref.color
                && !is_hex_color(color)
            {
                return Err(format!("{name}: `{color}` isn't a #rrggbb colour."));
            }
        }
        Ok(())
    }

    /// The engine configuration these settings describe.
    #[must_use]
    pub fn engine_config(&self) -> nuxd_core::config::EngineConfig {
        let mut config = nuxd_core::config::EngineConfig {
            slots_per_link: self.slots_per_link,
            max_connections_total: self.max_connections_total,
            auto_resume_on_startup: self.auto_resume,
            global_limit: self.global_limit.and_then(nuxd_core::model::ByteRate::new),
            ..Default::default()
        };
        for rule in &self.host_rules {
            // Validated on save; a bad rule from a hand-edited file is skipped.
            let _ = config.hosts.add_rule_text(rule);
        }
        config
    }

    /// The download folder to offer: the chosen one, else Downloads.
    #[must_use]
    pub fn download_path(&self) -> PathBuf {
        match &self.download_dir {
            Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
            _ => directories::UserDirs::new()
                .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

fn is_hex_color(text: &str) -> bool {
    text.len() == 7 && text.starts_with('#') && text[1..].chars().all(|c| c.is_ascii_hexdigit())
}

pub fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

/// Overlays `stored` on `base`, key by key, so a settings file written
/// by an older version (missing newer keys) still loads. The types stay
/// free of `serde(default)`, which would make every field optional in
/// the generated TypeScript.
fn overlay(base: &mut serde_json::Value, stored: serde_json::Value) {
    match (base, stored) {
        (serde_json::Value::Object(base), serde_json::Value::Object(stored)) => {
            for (key, value) in stored {
                match base.get_mut(&key) {
                    Some(slot) => overlay(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Parses settings text, filling in anything missing from the defaults.
fn parse(bytes: &[u8]) -> Option<Settings> {
    let stored: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut merged = serde_json::to_value(Settings::default()).ok()?;
    overlay(&mut merged, stored);
    // A link's saved choices are partial too: fill each from LinkPref's defaults.
    let default_pref = serde_json::to_value(LinkPref::default()).ok()?;
    if let Some(serde_json::Value::Object(prefs)) = merged.get_mut("link_prefs") {
        for pref in prefs.values_mut() {
            let mut full = default_pref.clone();
            overlay(&mut full, std::mem::take(pref));
            *pref = full;
        }
    }
    serde_json::from_value(merged).ok()
}

/// Reads the settings, falling back to defaults when the file is missing
/// or unreadable.
pub fn load(data_dir: &Path) -> Settings {
    std::fs::read(path_in(data_dir))
        .ok()
        .and_then(|bytes| parse(&bytes))
        .unwrap_or_default()
}

/// Writes the settings through a temporary file, so a crash can't leave
/// a half-written one.
pub fn save(data_dir: &Path, settings: &Settings) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("couldn't create {data_dir:?}: {e}"))?;
    let path = path_in(data_dir);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| format!("couldn't write settings: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("couldn't save settings: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        Settings::default().validate().unwrap();
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("nuxd-settings-{}", std::process::id()));
        let mut settings = Settings {
            global_limit: Some(5_000_000),
            ..Default::default()
        };
        settings.link_prefs.insert(
            "wlan0".into(),
            LinkPref {
                label: Some("Home".into()),
                color: Some("#33aa77".into()),
                enabled: false,
                limit: Some(1000),
            },
        );
        save(&dir, &settings).unwrap();
        assert_eq!(load(&dir), settings);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_means_defaults() {
        let dir = std::env::temp_dir().join(format!("nuxd-settings-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(path_in(&dir), b"{ nope").unwrap();
        assert_eq!(load(&dir), Settings::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_files_fill_in_defaults() {
        let settings =
            parse(br##"{"notifications": false, "link_prefs": {"a": {"label": "A"}}}"##).unwrap();
        assert!(!settings.notifications);
        assert_eq!(settings.slots_per_link, 8);
        let pref = &settings.link_prefs["a"];
        assert_eq!(pref.label.as_deref(), Some("A"));
        assert!(pref.enabled);
    }

    #[test]
    fn validation_catches_bad_input() {
        let bad = |f: &dyn Fn(&mut Settings)| {
            let mut s = Settings::default();
            f(&mut s);
            s.validate().is_err()
        };
        assert!(bad(&|s| s.slots_per_link = 0));
        assert!(bad(&|s| s.max_connections_total = 0));
        assert!(bad(&|s| s.global_limit = Some(0)));
        assert!(bad(&|s| s.host_rules = vec!["nope".into()]));
        assert!(bad(&|s| {
            s.link_prefs.insert(
                "x".into(),
                LinkPref {
                    color: Some("red".into()),
                    ..Default::default()
                },
            );
        }));
    }

    #[test]
    fn restart_is_needed_only_for_start_time_settings() {
        let a = Settings::default();
        let live = Settings {
            global_limit: Some(10),
            notifications: false,
            ..Default::default()
        };
        assert!(!a.needs_restart_for(&live));
        let slots = Settings {
            slots_per_link: 4,
            ..Default::default()
        };
        assert!(a.needs_restart_for(&slots));
    }
}
