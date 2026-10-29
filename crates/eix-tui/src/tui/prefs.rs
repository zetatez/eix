//! Global UI preferences that survive a TUI restart.
//!
//! The colorscheme picked with `/colorscheme` is stored here instead of in the
//! config file: the TUI only overrides `[theme] preset`, and rewriting a
//! hand-written `config.toml` would drop the user's comments. The file is
//! written on exit (Ctrl-C and Ctrl-D included) and applied on the next launch.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "prefs.json";

/// Picker value that means "no preset": the terminal palette stays untouched.
pub const DEFAULT_THEME: &str = "@default";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefs {
    /// Colorscheme preset picked in the TUI. `None` means the user never
    /// picked one, so `[theme] preset` from config still decides.
    #[serde(default)]
    pub theme: Option<String>,
}

impl Prefs {
    /// Override for `[theme] preset`: `None` keeps the config value, while
    /// `Some(None)` clears it because the user picked "default".
    pub fn preset_override(&self) -> Option<Option<String>> {
        self.theme
            .as_deref()
            .map(|theme| (theme != DEFAULT_THEME).then(|| theme.to_string()))
    }
}

/// Load the saved preferences. A missing or corrupt file falls back to
/// defaults: UI state must never block startup.
pub fn load(data_dir: &Path) -> Prefs {
    std::fs::read(state_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Replace the saved preferences atomically, so a crash mid-write cannot leave
/// a truncated file behind.
pub fn save(data_dir: &Path, prefs: &Prefs) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot create {}", data_dir.display()))?;
    let path = state_path(data_dir);
    let temp = data_dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(prefs).context("cannot encode UI preferences")?;
    std::fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("cannot replace {}", path.display()))
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "eix-prefs-{}-{}-{}",
            std::process::id(),
            eix_core::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn round_trips_the_theme_and_remembers_a_cleared_preset() {
        let data = temp_dir();
        assert_eq!(load(&data), Prefs::default());
        assert_eq!(load(&data).preset_override(), None, "config still decides");

        save(
            &data,
            &Prefs {
                theme: Some("dark-github".into()),
            },
        )
        .unwrap();
        assert_eq!(
            load(&data).preset_override(),
            Some(Some("dark-github".into()))
        );

        // Picking "default" must override a preset that config still has.
        save(
            &data,
            &Prefs {
                theme: Some(DEFAULT_THEME.into()),
            },
        )
        .unwrap();
        assert_eq!(load(&data).preset_override(), Some(None));

        std::fs::remove_dir_all(&data).ok();
    }

    #[test]
    fn corrupt_files_fall_back_to_defaults() {
        let data = temp_dir();
        std::fs::write(state_path(&data), b"{not json").unwrap();
        assert_eq!(load(&data), Prefs::default());
        std::fs::remove_dir_all(&data).ok();
    }
}
