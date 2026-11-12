//! Bundled theme families. The JSON sets in `../themes/` come from gpui-kit v0.9.1
//! (`themes/`, Apache-2.0, same license as gpui-component).

use gpui::App;
use gpui_component::{Theme, ThemeMode, ThemeRegistry};

const SETS: [&str; 14] = [
    include_str!("../themes/gruvbox.json"),
    include_str!("../themes/catppuccin.json"),
    include_str!("../themes/tokyonight.json"),
    include_str!("../themes/ayu.json"),
    include_str!("../themes/everforest.json"),
    include_str!("../themes/solarized.json"),
    include_str!("../themes/flexoki.json"),
    include_str!("../themes/hybrid.json"),
    include_str!("../themes/macos-classic.json"),
    include_str!("../themes/mellifluous.json"),
    include_str!("../themes/molokai.json"),
    include_str!("../themes/aurora.json"),
    include_str!("../themes/jellybeans.json"),
    include_str!("../themes/spaceduck.json"),
];

/// (family, light variant, dark variant); `None` falls back to the built-in theme of that mode.
pub const FAMILIES: [(&str, Option<&str>, Option<&str>); 16] = [
    ("Default", None, None),
    ("macOS Classic", Some("macOS Classic Light"), Some("macOS Classic Dark")),
    ("Gruvbox", Some("Gruvbox Light"), Some("Gruvbox Dark")),
    ("Catppuccin", Some("Catppuccin Latte"), Some("Catppuccin Mocha")),
    ("Solarized", Some("Solarized Light"), Some("Solarized Dark")),
    ("Ayu", Some("Ayu Light"), Some("Ayu Dark")),
    ("Everforest", Some("Everforest Light"), Some("Everforest Dark")),
    ("Flexoki", Some("Flexoki Light"), Some("Flexoki Dark")),
    ("Hybrid", Some("Hybrid Light"), Some("Hybrid Dark")),
    ("Mellifluous", Some("Mellifluous Light"), Some("Mellifluous Dark")),
    ("Molokai", Some("Molokai Light"), Some("Molokai Dark")),
    ("Aurora", Some("Aurora Light"), None),
    ("Tokyo Night", None, Some("Tokyo Night")),
    ("Tokyo Storm", None, Some("Tokyo Storm")),
    ("Jellybeans", None, Some("Jellybeans")),
    ("Spaceduck", None, Some("Spaceduck")),
];

pub fn register(cx: &mut App) {
    for set in SETS {
        if let Err(e) = ThemeRegistry::global_mut(cx).load_themes_from_str(set) {
            eprintln!("respite: bundled theme not loaded: {e}");
        }
    }
}

thread_local! {
    /// The current theme's text colour, for painters that get no `cx` (the fold placeholder).
    static TEXT: std::cell::Cell<gpui::Hsla> = const { std::cell::Cell::new(gpui::white()) };
}

pub fn text_color() -> gpui::Hsla {
    TEXT.with(|c| c.get())
}

/// Call after every theme or appearance change.
pub fn sync_text_color(cx: &App) {
    TEXT.with(|c| c.set(Theme::global(cx).foreground));
}

/// Show or hide the line-under-cursor background (`editor.highlight_current_line`).
/// The theme supplies the colour; applying a theme or mode brings it back, so call this after.
pub fn current_line(show: bool, cx: &mut App) {
    if show {
        return;
    }
    let theme = Theme::global_mut(cx);
    let mut highlight = (*theme.highlight_theme).clone();
    highlight.style.editor_active_line = None;
    theme.highlight_theme = std::sync::Arc::new(highlight);
}

/// Point the light/dark slots at `family`'s variants. Apply a mode afterwards
/// (`Theme::change`) to show them.
pub fn select(family: &str, cx: &mut App) {
    let (light, dark) = FAMILIES.iter().find(|f| f.0 == family).map_or((None, None), |f| (f.1, f.2));
    let reg = ThemeRegistry::global(cx);
    let pick = |name: Option<&str>, mode| {
        name.and_then(|n| reg.themes().get(n).cloned()).unwrap_or_else(|| reg.default_themes()[&mode].clone())
    };
    let (light, dark) = (pick(light, ThemeMode::Light), pick(dark, ThemeMode::Dark));
    let theme = Theme::global_mut(cx);
    (theme.light_theme, theme.dark_theme) = (light, dark);
}
