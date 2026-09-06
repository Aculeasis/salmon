use std::collections::BTreeMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const STATE_FILENAME: &str = ".salmon-watch-2-state.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StateFile {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub snoozed: BTreeMap<String, SnoozeEntry>,
    #[serde(default)]
    pub preferences: Preferences,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            schema_version: schema_version(),
            snoozed: BTreeMap::new(),
            preferences: Preferences::default(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SnoozeEntry {
    pub snoozed_until: String,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Preferences {
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub sections: SectionPreferences,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            sections: SectionPreferences::default(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SectionPreferences {
    #[serde(default = "default_expanded")]
    pub servers_expanded: bool,
    #[serde(default = "default_expanded")]
    pub active_incidents_expanded: bool,
    #[serde(default)]
    pub snoozed_incidents_expanded: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Default for SectionPreferences {
    fn default() -> Self {
        Self {
            servers_expanded: true,
            active_incidents_expanded: true,
            snoozed_incidents_expanded: false,
            extra: BTreeMap::new(),
        }
    }
}

fn schema_version() -> u32 {
    1
}

fn default_expanded() -> bool {
    true
}

pub fn default_state_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine the user home directory")?;
    Ok(home.join(STATE_FILENAME))
}

pub fn load(path: &Path) -> Result<StateFile> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .with_context(|| format!("failed to parse state file {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StateFile::default()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read state file {}", path.display()))
        }
    }
}

pub fn save(path: &Path, state: &StateFile) -> Result<()> {
    let parent = path
        .parent()
        .context("state filename has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "failed to create temporary state file in {}",
            parent.display()
        )
    })?;

    set_owner_only_permissions(temporary.as_file())?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, state).context("failed to encode state file")?;
        writer
            .write_all(b"\n")
            .context("failed to finish state file")?;
        writer.flush().context("failed to flush state file")?;
    }
    temporary
        .as_file()
        .sync_all()
        .context("failed to sync state file")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace state file {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("failed to set state-file permissions")
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_file: &fs::File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_uses_ui_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let state = load(&directory.path().join("missing.json")).unwrap();

        assert_eq!(state.schema_version, 1);
        assert_eq!(state.preferences.theme, Theme::Dark);
        assert!(state.preferences.sections.servers_expanded);
        assert!(state.preferences.sections.active_incidents_expanded);
        assert!(!state.preferences.sections.snoozed_incidents_expanded);
    }

    #[test]
    fn unversioned_snooze_only_shape_loads_with_defaults() {
        let state: StateFile = serde_json::from_str(
            r#"{"snoozed":{"local.disk":{"snoozed_until":"2026-09-06T12:00:00Z"}}}"#,
        )
        .unwrap();

        assert_eq!(state.schema_version, 1);
        assert_eq!(state.snoozed.len(), 1);
        assert_eq!(state.preferences.theme, Theme::Dark);
    }

    #[test]
    fn save_is_round_trippable_and_preserves_unknown_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut state: StateFile = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "snoozed": {},
                "preferences": {
                    "theme": "light",
                    "sections": {
                        "servers_expanded": false,
                        "active_incidents_expanded": true,
                        "snoozed_incidents_expanded": true,
                        "future_section_setting": 42
                    },
                    "future_preference": "kept"
                },
                "future_top_level": {"also": "kept"}
            }"#,
        )
        .unwrap();
        state.preferences.sections.active_incidents_expanded = false;

        save(&path, &state).unwrap();
        let encoded: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        assert_eq!(encoded["future_top_level"]["also"], "kept");
        assert_eq!(encoded["preferences"]["future_preference"], "kept");
        assert_eq!(
            encoded["preferences"]["sections"]["future_section_setting"],
            42
        );
        assert_eq!(
            encoded["preferences"]["sections"]["active_incidents_expanded"],
            false
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_state_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        save(&path, &StateFile::default()).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
