//! Persisted settings in `~/.config/photoviewer/settings.toml`.

use std::fs;
use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

use crate::model::{Show, SortKey};

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
    /// Largest first (newest, Z to A), which the user calls ascending. It is
    /// the old `sort_desc` value, so saved settings keep their order.
    #[serde(alias = "sort_desc")]
    pub sort_ascending: bool,
    /// Which files the grid shows.
    pub show: Show,
    pub thumb_size: i32,
    /// Fullscreen background: black instead of the grid's dark gray.
    pub viewer_black: bool,
    /// Video volume in percent.
    pub volume: f64,
    /// The fullscreen info overlay (I) is on.
    pub viewer_info: bool,
    /// Seconds per photo in a slideshow.
    pub slideshow_interval: f64,
    pub slideshow_loop: bool,
    pub slideshow_shuffle: bool,
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
            sort_ascending: true,
            show: Show::All,
            thumb_size: 160,
            viewer_black: false,
            volume: 100.0,
            viewer_info: false,
            slideshow_interval: 4.0,
            slideshow_loop: true,
            slideshow_shuffle: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_sort_direction_carries_over() {
        let old: Settings = toml::from_str("sort_desc = false").unwrap();
        assert!(!old.sort_ascending);
        let old: Settings = toml::from_str("sort_desc = true").unwrap();
        assert!(old.sort_ascending);
        let saved = toml::to_string(&Settings::default()).unwrap();
        assert!(saved.contains("sort_ascending = true"));
        assert!(!saved.contains("sort_desc"));
    }
}
