//! Accent color: the desktop theme's (default) or one of libadwaita's nine.
//!
//! A fixed color is applied as CSS over libadwaita's accent colors. On
//! libadwaita 1.5 with a distro theme (Zorin's patch swaps in the GTK
//! theme's own stylesheet, Ubuntu's draws Yaru accents), the theme writes its
//! accent into many rules as literal colors, so a fixed color also loads
//! libadwaita's stock stylesheet above the theme's: stock Adwaita uses the
//! named accent colors everywhere. From GTK 4.16 / libadwaita 1.6 on, the
//! accent is also a set of CSS variables.

use std::cell::{Cell, OnceCell};
use std::time::Instant;

use gtk::{gdk, gio};

use crate::trace;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    /// Whatever the desktop theme uses.
    #[default]
    System,
    Blue,
    Teal,
    Green,
    Yellow,
    Orange,
    Red,
    Pink,
    Purple,
    Slate,
}

impl Accent {
    pub const ALL: [Self; 10] = [
        Self::System,
        Self::Blue,
        Self::Teal,
        Self::Green,
        Self::Yellow,
        Self::Orange,
        Self::Red,
        Self::Pink,
        Self::Purple,
        Self::Slate,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Blue => "Blue",
            Self::Teal => "Teal",
            Self::Green => "Green",
            Self::Yellow => "Yellow",
            Self::Orange => "Orange",
            Self::Red => "Red",
            Self::Pink => "Pink",
            Self::Purple => "Purple",
            Self::Slate => "Slate",
        }
    }

    /// The CSS class of its swatch in Preferences.
    pub fn swatch_class(self) -> String {
        format!("fv-accent-{}", self.name().to_lowercase())
    }

    /// Background (libadwaita 1.6's accent palette) and the lighter color
    /// used for accent text on dark backgrounds (OKLab lightness raised to
    /// 0.85, as libadwaita 1.6 derives it in dark mode).
    fn colors(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Self::System => return None,
            Self::Blue => ("#3584e4", "#81d0ff"),
            Self::Teal => ("#2190a4", "#7bdff4"),
            Self::Green => ("#3a944a", "#8de698"),
            Self::Yellow => ("#c88800", "#ffc057"),
            Self::Orange => ("#ed5b00", "#ff9c5b"),
            Self::Red => ("#e62d42", "#ff888c"),
            Self::Pink => ("#d56199", "#ffa0d8"),
            Self::Purple => ("#9141ac", "#fba7ff"),
            Self::Slate => ("#6f8396", "#bbd1e5"),
        })
    }
}

/// libadwaita's own dark stylesheet, built into the library.
const STOCK: [&str; 2] = [
    "/org/gnome/Adwaita/styles/defaults-dark.css",
    "/org/gnome/Adwaita/styles/base.css",
];

#[derive(Default)]
struct Providers {
    /// Swatches and the accent override.
    accent: OnceCell<gtk::CssProvider>,
    /// Stock Adwaita over a distro theme, while a fixed color is chosen.
    stock: OnceCell<Option<gtk::CssProvider>>,
    stock_added: Cell<bool>,
}

thread_local! {
    static PROVIDERS: Providers = Providers::default();
}

/// Applies `accent` to the whole app (on the UI thread, before the window
/// shows, so the old color doesn't flash).
pub fn apply(accent: Accent) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let started = Instant::now();
    PROVIDERS.with(|p| {
        // Distro themes only patch libadwaita 1.5 (Zorin's and Ubuntu's).
        let want_stock = accent != Accent::System && adw::minor_version() < 6;
        if want_stock != p.stock_added.get()
            && let Some(stock) = p.stock.get_or_init(stock_provider)
        {
            if want_stock {
                // Above the theme's own stylesheet, below the app's.
                gtk::style_context_add_provider_for_display(
                    &display,
                    stock,
                    gtk::STYLE_PROVIDER_PRIORITY_THEME + 1,
                );
            } else {
                gtk::style_context_remove_provider_for_display(&display, stock);
            }
            p.stock_added.set(want_stock);
        }
        let provider = p.accent.get_or_init(|| {
            let provider = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                // Over style.css, which reads @accent_bg_color.
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
            provider
        });
        provider.load_from_string(&css(accent, gtk::minor_version() >= 16));
    });
    trace!("accent {}: {:?}", accent.name(), started.elapsed());
}

fn stock_provider() -> Option<gtk::CssProvider> {
    // The theme also sets properties stock never touches (Zorin paints
    // switches, sliders and sidebar rows with background images, entries
    // with tinted text). Resetting everything first, at the lowest
    // specificity, leaves exactly what stock alone would give.
    let mut css = String::from("* { all: unset; }\n");
    for path in STOCK {
        let data = gio::resources_lookup_data(path, gio::ResourceLookupFlags::NONE).ok()?;
        css += std::str::from_utf8(&data).ok()?;
        css.push('\n');
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css);
    Some(provider)
}

/// Swatch colors for Preferences, plus the override for a fixed accent.
fn css(accent: Accent, variables: bool) -> String {
    let mut css = String::new();
    for swatch in Accent::ALL {
        if let Some((bg, _)) = swatch.colors() {
            css += &format!(
                ".fv-swatch.{} {{ background-color: {bg}; }}\n",
                swatch.swatch_class()
            );
        }
    }
    if let Some((bg, fg)) = accent.colors() {
        css += &format!(
            "@define-color accent_bg_color {bg};\n\
             @define-color accent_color {fg};\n\
             @define-color accent_fg_color #ffffff;\n\
             .fv-tree .navigation-sidebar > row:selected \
             {{ background-color: alpha(@accent_bg_color, 0.35); }}\n"
        );
        // GTK 4.14 would warn about the syntax.
        if variables {
            css += &format!(
                ":root {{ --accent-bg-color: {bg}; --accent-color: {fg}; \
                 --accent-fg-color: #ffffff; }}\n"
            );
        }
    }
    css
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for accent in Accent::ALL {
            let text = toml::to_string(&std::collections::BTreeMap::from([("a", accent)])).unwrap();
            let back: std::collections::BTreeMap<String, Accent> = toml::from_str(&text).unwrap();
            assert_eq!(back["a"], accent);
            assert_eq!(text, format!("a = \"{}\"\n", accent.name().to_lowercase()));
        }
    }

    #[test]
    fn system_only_styles_swatches() {
        let system = css(Accent::System, true);
        assert!(!system.contains("accent_bg_color"));
        assert!(system.contains(".fv-swatch.fv-accent-blue"));
        let old = css(Accent::Teal, false);
        assert!(old.contains("@define-color accent_bg_color #2190a4"));
        assert!(!old.contains("--accent"));
        assert!(css(Accent::Teal, true).contains("--accent-bg-color: #2190a4"));
    }
}
