//! Persisted settings in `~/.config/photoviewer/settings.toml`.

use std::fs;
use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

use crate::model::SortKey;

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub last_folder: Option<String>,
    /// Folder opened at startup; `None` means Home.
    pub start_folder: Option<String>,
    /// Open `last_folder` at startup instead of `start_folder`.
    pub open_last_folder: bool,
    pub window_width: i32,
    pub window_height: i32,
    pub maximized: bool,
    pub pane_width: i32,
    pub pane_visible: bool,
    pub show_hidden: bool,
    pub sort_key: SortKey,
    pub sort_desc: bool,
    pub thumb_size: i32,
    /// Fullscreen background: black instead of 50% gray.
    pub viewer_black: bool,
    /// Video volume in percent.
    pub volume: f64,
    /// Expanded folder-tree rows as (root, folder) path pairs.
    pub expanded: Vec<(String, String)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            last_folder: None,
            start_folder: None,
            open_last_folder: false,
            window_width: 1400,
            window_height: 900,
            maximized: false,
            pane_width: 260,
            pane_visible: true,
            show_hidden: false,
            sort_key: SortKey::Modified,
            sort_desc: true,
            thumb_size: 160,
            viewer_black: false,
            volume: 100.0,
            expanded: Vec::new(),
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        glib::user_config_dir()
            .join("photoviewer")
            .join("settings.toml")
    }

    pub fn load() -> Self {
        let Ok(text) = fs::read_to_string(Self::path()) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("Ignoring unreadable settings file: {e}");
            Self::default()
        })
    }

    pub fn save(&self) {
        let path = Self::path();
        let result = toml::to_string(self)
            .map_err(std::io::Error::other)
            .and_then(|text| {
                fs::create_dir_all(path.parent().expect("settings path has a parent"))?;
                let tmp = path.with_extension("toml.tmp");
                fs::write(&tmp, text)?;
                fs::rename(&tmp, &path)
            });
        if let Err(e) = result {
            eprintln!("Could not save settings to {}: {e}", path.display());
        }
    }
}
