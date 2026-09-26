//! The app's look: the operating system's light/dark mode, a fixed light
//! or dark, or our own high-contrast theme (gpui-kit only ships "Default
//! Light" and "Default Dark").
//!
//! gpui-kit's `Theme` is a GPUI global: one value for the whole app, read
//! by every component while drawing. Changing it and redrawing is all it
//! takes to restyle every open window.

use chat_engine::Appearance;
use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;

/// Black background, white text and borders, a yellow focus/accent colour
/// and bright status colours. Colours it doesn't set come from gpui-kit's
/// dark theme.
const HIGH_CONTRAST: &str = include_str!("high-contrast.json");
const HIGH_CONTRAST_NAME: &str = "High Contrast";

/// Applies `appearance` to all windows. `window`, if given, is asked for
/// the system's light/dark mode (more reliable than asking the app on
/// Linux, says gpui-kit).
pub fn apply(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    // Which theme the dark mode uses: ours only for high contrast.
    let dark = if appearance == Appearance::HighContrast {
        high_contrast(cx)
    } else {
        ThemeRegistry::global(cx).default_dark_theme().clone()
    };
    Theme::global_mut(cx).dark_theme = dark;

    let mode = match appearance {
        Appearance::System => match &window {
            Some(window) => window.appearance().into(),
            None => cx.window_appearance().into(),
        },
        Appearance::Light => ThemeMode::Light,
        Appearance::Dark | Appearance::HighContrast => ThemeMode::Dark,
    };
    Theme::change(mode, window, cx);
    // `change` only redraws the window it was given; the others too.
    cx.refresh_windows();
}

/// Our high-contrast theme, loaded into gpui-kit's theme registry the
/// first time it's needed.
fn high_contrast(cx: &mut App) -> std::rc::Rc<gpui_kit::component::ThemeConfig> {
    let registry = ThemeRegistry::global_mut(cx);
    if !registry.themes().contains_key(HIGH_CONTRAST_NAME) {
        registry
            .load_themes_from_str(HIGH_CONTRAST)
            .expect("the built-in high-contrast theme is valid JSON (tested)");
    }
    registry.themes()[HIGH_CONTRAST_NAME].clone()
}

/// UI tests, headless (see `app_view::tests` for why imports are explicit).
#[cfg(test)]
mod tests {
    use chat_engine::Appearance;
    use gpui_kit::TestAppContext;
    use gpui_kit::component::{ActiveTheme, Theme};

    #[gpui_kit::test]
    fn each_appearance_switches_the_theme(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(|cx| {
            super::apply(Appearance::HighContrast, None, cx);
            assert!(cx.theme().is_dark());
            assert_eq!(Theme::global(cx).theme_name().as_ref(), "High Contrast");
            assert_eq!(cx.theme().background, gpui_kit::black());
            assert_eq!(cx.theme().foreground, gpui_kit::white());

            super::apply(Appearance::Dark, None, cx);
            assert!(cx.theme().is_dark());
            assert_eq!(Theme::global(cx).theme_name().as_ref(), "Default Dark");

            super::apply(Appearance::Light, None, cx);
            assert!(!cx.theme().is_dark());

            // Back to high contrast: loaded only once, still works.
            super::apply(Appearance::HighContrast, None, cx);
            assert_eq!(Theme::global(cx).theme_name().as_ref(), "High Contrast");
        });
    }
}
