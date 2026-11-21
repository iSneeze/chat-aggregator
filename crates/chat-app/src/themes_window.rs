//! The Themes window: a two-pane overlay theme browser and live customizer.
//!
//! - Left pane: built-in themes (`Default`, `Minimal`) and custom folders from
//!   `themes/`, with an "Active" indicator, inline "New theme…" creation, and
//!   "Open folder".
//! - Right pane: manifest-driven customization controls built from each
//!   theme's `theme.toml` (colors, typography, motion, visible elements), plus
//!   "Use as active", "Copy URL", "Reload", and "Reset".
//!
//! Changes save to the active profile (`[overlay.theme_options.<theme>]`) and
//! push live to connected OBS overlays over SSE without reloading the page.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chat_engine::theme_options::{
    OptionKind, ThemeManifest, ThemeValue, ThemeValues, lookup_theme_values,
};
use chat_engine::{Builtin, EngineHandle, ThemeSource};
use chat_render::themes;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, StepAction};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt, h_flex,
    scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_config::{AppConfig, edit_settings};
use crate::ui::{
    Debouncer, card, choice, confirm, open_folder, parse_number, report, segmented, stepped,
};
use form::{Form, set};
use palette::SuggestedPalette;

mod form;
mod palette;

/// Short debounce for color picker slider drags so live OBS previews feel
/// responsive without rewriting the profile file on every pixel of movement.
const COLOR_APPLY_AFTER: Duration = Duration::from_millis(150);

/// One theme in the left-hand list.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    /// Canonical key (`"default"`, `"minimal"`, or the folder name).
    key: String,
    /// Display name (`"Default"`, `"Minimal"`, or the folder name).
    label: String,
    builtin: Option<Builtin>,
    /// Parsed `theme.toml`, or why it couldn't be read/parsed.
    manifest: Result<ThemeManifest, String>,
}

fn builtin_entries() -> Vec<Entry> {
    Builtin::ALL
        .into_iter()
        .map(|b| Entry {
            key: b.name().to_string(),
            label: b.label().to_string(),
            builtin: Some(b),
            manifest: Ok(b.manifest().clone()),
        })
        .collect()
}

/// Scans built-in themes plus folders in `themes_dir` off the UI thread.
fn scan(themes_dir: &Path) -> Vec<Entry> {
    let custom = themes::list(themes_dir).into_iter().map(|name| {
        let source = ThemeSource::Folder(themes_dir.join(&name));
        let manifest = source.manifest().map_err(|e| format!("{e:#}"));
        Entry {
            key: name.clone(),
            label: name,
            builtin: None,
            manifest,
        }
    });
    builtin_entries().into_iter().chain(custom).collect()
}

/// Percent-encodes a theme name for `?theme=<name>` query parameters.
fn encode_query_value(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// State for the inline "New theme…" / "Copy…" form in the left pane.
struct NewThemeForm {
    input: Entity<InputState>,
    /// Which built-in theme's files (`message.html`, `overlay.css`,
    /// `theme.toml`) are copied as the starting template.
    base: Builtin,
    error: Option<String>,
    _enter: Subscription,
}

pub struct ThemesWindow {
    engine: EngineHandle,
    config: Entity<AppConfig>,
    window: AnyWindowHandle,
    /// Base overlay address (`http://127.0.0.1:7878/`), kept up to date from
    /// engine status.
    server: String,
    loaded_for: u64,
    entries: Vec<Entry>,
    scan_gen: u64,
    /// Selected theme's canonical key.
    selected: String,
    form: Option<Form>,
    debounce: Debouncer,
    /// Inline "New theme…" / "Copy…" form, when open.
    new_theme: Option<NewThemeForm>,
    /// Temporary palette suggestions extracted from a user-picked image.
    extracted_palettes: Vec<SuggestedPalette>,
    _subscriptions: Vec<Subscription>,
}

impl ThemesWindow {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        initial_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries = builtin_entries();
        let server = engine.status().borrow().overlay_url.clone();
        let loaded_for = config.read(cx).switches();

        let subscriptions = vec![
            cx.observe_in(&config, window, |this, config, window, cx| {
                let switches = config.read(cx).switches();
                if switches != this.loaded_for {
                    this.loaded_for = switches;
                    let saved = this.saved_values(&this.selected, cx);
                    this.open_form(saved, window, cx);
                }
                cx.notify();
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.rescan(window, cx);
                }
            }),
            cx.on_release(|this, cx| {
                if this.debounce.take_dirty() {
                    this.apply(cx);
                }
            }),
        ];

        Self::watch_server(&engine, window, cx);

        let mut view = Self {
            engine,
            config,
            window: window.window_handle(),
            server,
            loaded_for,
            entries,
            scan_gen: 0,
            selected: String::new(),
            form: None,
            debounce: Debouncer::default(),
            new_theme: None,
            extracted_palettes: Vec::new(),
            _subscriptions: subscriptions,
        };
        // Keep the requested key even if it's a custom folder not scanned yet:
        // `apply_scanned` opens its form once the background scan finishes.
        view.select(initial_key, window, cx);
        view.rescan(window, cx);
        view
    }

    /// Selects `key` when the user opens or refocuses the window from the main
    /// window's "Customize…" button.
    pub(crate) fn select_theme_key(
        &mut self,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected != key {
            self.select(key, window, cx);
        }
        self.rescan(window, cx);
    }

    /// Flushes debounced edits right away (called before profile switches).
    pub(crate) fn flush(&mut self, cx: &mut App) {
        if self.debounce.take_dirty() {
            self.apply(cx);
        }
    }

    fn watch_server(engine: &EngineHandle, window: &mut Window, cx: &mut Context<Self>) {
        let mut status = engine.status();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let server = status.borrow_and_update().overlay_url.clone();
                let open = this.update(cx, |view, cx| {
                    if view.server != server {
                        view.server = server;
                        cx.notify();
                    }
                });
                if open.is_err() || status.changed().await.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn themes_dir(&self, cx: &App) -> PathBuf {
        self.config.read(cx).themes_dir()
    }

    fn active_theme_key(&self, cx: &App) -> String {
        let config = self.config.read(cx);
        let dir = config.themes_dir();
        config
            .settings()
            .profile
            .overlay
            .theme_source(Some(&dir))
            .key()
    }

    fn saved_values(&self, key: &str, cx: &App) -> ThemeValues {
        let settings = self.config.read(cx).settings();
        lookup_theme_values(&settings.profile.overlay.theme_options, key)
            .cloned()
            .unwrap_or_default()
    }

    // ---- list & scanning ----

    fn select(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.debounce.take_dirty() {
            self.apply(cx);
        }
        let saved = self.saved_values(&key, cx);
        self.selected = key;
        self.extracted_palettes.clear();
        self.open_form(saved, window, cx);
    }

    fn open_form(&mut self, values: ThemeValues, window: &mut Window, cx: &mut Context<Self>) {
        self.debounce.cancel();
        let manifest = self
            .entries
            .iter()
            .find(|entry| entry.key == self.selected)
            .and_then(|entry| entry.manifest.as_ref().ok())
            .cloned();
        self.form = manifest
            .map(|manifest| Form::new(self.selected.clone(), manifest, &values, window, cx));
        cx.notify();
    }

    fn rescan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scan_gen += 1;
        let scan_gen = self.scan_gen;
        let dir = self.themes_dir(cx);
        let scanning = cx.background_executor().spawn(async move { scan(&dir) });
        cx.spawn_in(window, async move |this, cx| {
            let entries = scanning.await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.scan_gen != scan_gen {
                    return;
                }
                view.apply_scanned(entries, window, cx);
            });
        })
        .detach();
    }

    fn apply_scanned(&mut self, entries: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) {
        let prev_manifest = self
            .entries
            .iter()
            .find(|e| e.key == self.selected)
            .map(|e| e.manifest.clone());
        let next_manifest = entries
            .iter()
            .find(|e| e.key == self.selected)
            .map(|e| e.manifest.clone());
        let unchanged = entries == self.entries;
        self.entries = entries;

        if next_manifest.is_none() {
            let fallback = self
                .entries
                .first()
                .map(|e| e.key.clone())
                .unwrap_or_else(|| Builtin::Default.name().to_string());
            self.select(fallback, window, cx);
            return;
        }

        if unchanged {
            return;
        }

        if next_manifest != prev_manifest || self.form.is_none() {
            if self.debounce.take_dirty() {
                self.apply(cx);
            }
            let values = match &self.form {
                Some(form) if form.theme_key == self.selected => form.values.clone(),
                _ => self.saved_values(&self.selected, cx),
            };
            self.open_form(values, window, cx);
        } else {
            cx.notify();
        }
    }

    // ---- theme actions ----

    fn set_active(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let theme = match Builtin::from_name(key) {
            Some(Builtin::Default) => None,
            Some(builtin) => Some(builtin.name().to_string()),
            None => Some(key.to_string()),
        };
        edit_settings(
            &self.engine,
            window.window_handle(),
            move |s| s.profile.overlay.theme = theme,
            cx,
        );
        cx.notify();
    }

    fn copy_url(&self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        let url = format!("{}?theme={}", self.server, encode_query_value(&entry.key));
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        window.push_notification(
            Notification::success(format!("Copied URL for \"{}\"", entry.label)),
            cx,
        );
    }

    fn open_preview(&self, entry: &Entry, cx: &mut Context<Self>) {
        let url = format!(
            "{}preview?theme={}",
            self.server,
            encode_query_value(&entry.key)
        );
        cx.open_url(&url);
    }

    pub(super) fn apply_preset(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.form else {
            return;
        };
        let Some(preset) = form.manifest.presets.get(index).cloned() else {
            return;
        };
        for (opt_name, value) in preset.values {
            if let Some(opt) = form.manifest.options.iter().find(|o| o.name == opt_name) {
                set(&mut form.values, opt, value);
            }
        }
        form.sync_controls(window, cx);
        self.debounce.cancel();
        self.apply(cx);
        cx.notify();
    }

    pub(super) fn pick_palette_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Pick a screenshot or image (PNG, JPEG, WebP)".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = paths.await;
            let path = match result {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(e)) => {
                    let _ = this.update_in(cx, |_, window, cx| {
                        report(window, cx, format!("Couldn't open the file dialog ({e})."));
                    });
                    None
                }
            };
            let Some(path) = path else {
                return;
            };
            let extracting = cx
                .background_executor()
                .spawn(async move { palette::extract_from_file(&path) });
            let extracted = extracting.await;
            let _ = this.update_in(cx, |view, window, cx| match extracted {
                Ok(palettes) => {
                    view.extracted_palettes = palettes;
                    cx.notify();
                }
                Err(e) => report(window, cx, format!("Couldn't read image colors: {e:#}")),
            });
        })
        .detach();
    }

    pub(super) fn apply_extracted_palette(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pal) = self.extracted_palettes.get(index).cloned() else {
            return;
        };
        let Some(form) = &mut self.form else {
            return;
        };
        let changes = palette::apply_to_manifest(&form.manifest, &pal, &form.values);
        for (opt_name, value) in changes {
            if let Some(opt) = form.manifest.options.iter().find(|o| o.name == opt_name) {
                set(&mut form.values, opt, value);
            }
        }
        form.sync_controls(window, cx);
        self.debounce.cancel();
        self.apply(cx);
        cx.notify();
    }

    fn open_new_theme_form(&mut self, base: Builtin, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("name for the new theme"));
        let enter = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.create_theme(window, cx);
            }
        });
        self.new_theme = Some(NewThemeForm {
            input,
            base,
            error: None,
            _enter: enter,
        });
        cx.notify();
    }

    fn create_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.new_theme else {
            return;
        };
        let name = form.input.read(cx).value().trim().to_string();
        let base = form.base;
        if let Err(problem) = themes::validate_name(&name) {
            form.error = Some(problem);
            cx.notify();
            return;
        }
        let dir = self.themes_dir(cx);
        let name_for_io = name.clone();
        let creating = cx.background_executor().spawn(async move {
            let created = themes::create_from_builtin(&dir, &name_for_io, base)?;
            Ok::<_, anyhow::Error>((created, scan(&dir)))
        });
        self.scan_gen += 1;
        let scan_gen = self.scan_gen;
        cx.spawn_in(window, async move |this, cx| {
            let result = creating.await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok((created_dir, entries)) => {
                    view.new_theme = None;
                    if view.scan_gen == scan_gen {
                        view.entries = entries;
                    }
                    view.select(name.clone(), window, cx);
                    window.push_notification(
                        Notification::success(format!(
                            "Theme \"{name}\" created from {} in {}",
                            base.label(),
                            created_dir.display()
                        )),
                        cx,
                    );
                }
                Err(e) => {
                    if let Some(form) = &mut view.new_theme {
                        form.error = Some(format!("{e:#}"));
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ---- form interactions ----

    fn can_reset(&self) -> bool {
        self.form.as_ref().is_some_and(|form| {
            !form.values.is_empty() || self.debounce.is_dirty() || form.has_errors()
        })
    }

    fn reset(&mut self, label: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let key = form.theme_key.clone();
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            format!("Reset \"{label}\"?"),
            "Every option goes back to this theme's default. Connected overlays update right away.",
            "Reset",
            move |window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.reset_now(key.clone(), window, cx));
                }
            },
        );
    }

    fn reset_now(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == key {
            self.open_form(ThemeValues::new(), window, cx);
        }
        edit_settings(
            &self.engine,
            window.window_handle(),
            move |s| {
                remove_theme_options(&mut s.profile.overlay.theme_options, &key);
            },
            cx,
        );
    }

    pub(super) fn typed(&mut self, cx: &mut Context<Self>) {
        self.debounce.schedule(cx, |this, cx| {
            this.apply(cx);
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn step(
        &mut self,
        i: usize,
        action: StepAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.form else {
            return;
        };
        let Some(opt) = form.manifest.options.get(i) else {
            return;
        };
        let OptionKind::Number {
            default,
            min,
            max,
            step,
            ..
        } = opt.kind
        else {
            return;
        };
        let Some(field) = form.number_fields.get(i).cloned().flatten() else {
            return;
        };
        let text = field.read(cx).value();
        let from = if text.trim().is_empty() {
            default
        } else if let Some(typed) = parse_number(&text) {
            typed
        } else {
            match form.current(opt) {
                ThemeValue::Number(fitting) => fitting,
                _ => default,
            }
        };
        let to = stepped(from, action, min, max, step.unwrap_or(1.0));
        if to == from {
            return;
        }
        field.update(cx, |field, cx| field.set_value(to.to_string(), window, cx));
        self.typed(cx);
        cx.notify();
    }

    pub(super) fn pick(&mut self, index: usize, value: ThemeValue, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let Some(opt) = form.manifest.options.get(index) else {
            return;
        };
        set(&mut form.values, opt, value);
        self.debounce.cancel();
        self.apply(cx);
        cx.notify();
    }

    pub(super) fn pick_color(&mut self, index: usize, hex: String, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let Some(opt) = form.manifest.options.get(index) else {
            return;
        };
        set(&mut form.values, opt, ThemeValue::Text(hex));
        self.debounce
            .schedule_after(COLOR_APPLY_AFTER, cx, |this, cx| {
                this.apply(cx);
                cx.notify();
            });
        cx.notify();
    }

    fn apply(&mut self, cx: &mut App) {
        self.debounce.cancel();
        let Some(form) = &mut self.form else {
            return;
        };
        form.read_fields(cx);
        let key = form.theme_key.clone();
        let values = form.values.clone();
        edit_settings(
            &self.engine,
            self.window,
            move |s| {
                remove_theme_options(&mut s.profile.overlay.theme_options, &key);
                if !values.is_empty() {
                    s.profile.overlay.theme_options.insert(key, values);
                }
            },
            cx,
        );
    }

    // ---- rendering ----

    fn render_list(&self, active_key: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                self.render_entry(i, entry, entry.key == active_key, cx)
                    .into_any_element()
            })
            .collect();

        let default_base = Builtin::from_name(&self.selected).unwrap_or(Builtin::Default);
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;

        card(cx)
            .w(px(260.))
            .flex_none()
            .min_h_0()
            .child(
                div()
                    .id("theme-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(v_flex().gap_1().children(rows)),
            )
            .children(self.new_theme.as_ref().map(|form| {
                let base_choices = Builtin::ALL.into_iter().enumerate().map(|(i, builtin)| {
                    choice(
                        ("themes-new-base", i),
                        builtin.label(),
                        form.base == builtin,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = &mut this.new_theme {
                            form.base = builtin;
                            cx.notify();
                        }
                    }))
                });
                v_flex()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(border)
                    .child(Input::new(&form.input).id("themes-new-name").small())
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(muted).child("Start from"))
                            .child(segmented(cx, base_choices)),
                    )
                    .children(
                        form.error
                            .clone()
                            .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("themes-new-create")
                                    .label("Create")
                                    .primary()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.create_theme(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("themes-new-cancel")
                                    .label("Cancel")
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.new_theme = None;
                                        cx.notify();
                                    })),
                            ),
                    )
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("themes-new")
                            .label("New theme…")
                            .icon(IconName::Plus)
                            .small()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_new_theme_form(default_base, window, cx);
                            })),
                    )
                    .child(
                        Button::new("themes-folder")
                            .label("Open folder")
                            .icon(IconName::FolderOpen)
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                let dir = this.themes_dir(cx);
                                open_folder(&dir, window, cx);
                            })),
                    ),
            )
    }

    fn render_entry(
        &self,
        i: usize,
        entry: &Entry,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (note, color) = match &entry.manifest {
            Err(problem) => (problem.clone(), theme.danger),
            Ok(_) if entry.builtin.is_some() => ("built in".to_string(), theme.muted_foreground),
            Ok(m) if m.options.is_empty() => {
                ("custom · no options".to_string(), theme.muted_foreground)
            }
            Ok(_) => ("custom".to_string(), theme.muted_foreground),
        };
        let key = entry.key.clone();
        v_flex()
            .id(("theme-entry", i))
            .p_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(entry.key == self.selected, |row| row.bg(theme.accent))
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.selected != key {
                    this.select(key.clone(), window, cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_semibold()
                            .text_ellipsis()
                            .child(entry.label.clone()),
                    )
                    .when(is_active, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .px_1p5()
                                .py_0p5()
                                .rounded(theme.radius)
                                .bg(theme.primary.opacity(0.16))
                                .text_color(theme.primary)
                                .child("Active"),
                        )
                    }),
            )
            .child(div().text_sm().text_color(color).line_clamp(2).child(note))
            .test_support()
    }

    fn render_selected(&self, active_key: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let pane = card(cx).flex_1().min_w_0().min_h_0();
        let Some(entry) = self.entries.iter().find(|e| e.key == self.selected) else {
            return pane;
        };
        let is_active = entry.key == active_key;
        let entry_key = entry.key.clone();
        let entry_for_copy = entry.clone();
        let entry_for_preview = entry.clone();
        let label_for_reset = entry.label.clone();

        let heading = h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_lg()
                    .font_semibold()
                    .text_ellipsis()
                    .child(entry.label.clone()),
            )
            .when(!is_active, |row| {
                let key = entry_key.clone();
                row.child(
                    Button::new("theme-use-active")
                        .label("Use as active")
                        .primary()
                        .small()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_active(&key, window, cx);
                        })),
                )
            })
            .when_some(entry.builtin, |row, builtin| {
                row.child(
                    Button::new("theme-copy-builtin")
                        .label("Copy as custom…")
                        .small()
                        .tooltip(
                            "Copy this built-in theme into your themes folder to edit its CSS or HTML",
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_new_theme_form(builtin, window, cx);
                        })),
                )
            })
            .child(
                Button::new("theme-preview")
                    .icon(IconName::ExternalLink)
                    .label("Preview")
                    .small()
                    .tooltip("Open a live browser preview with sample messages")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_preview(&entry_for_preview, cx);
                    })),
            )
            .child(
                Button::new("theme-copy-url")
                    .icon(IconName::Copy)
                    .label("Copy URL")
                    .small()
                    .tooltip("Copy a pinned overlay URL (?theme=…) for OBS")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.copy_url(&entry_for_copy, window, cx);
                    })),
            )
            .child(
                Button::new("theme-window-reload")
                    .label("Reload")
                    .small()
                    .tooltip("Reload connected overlays (after editing custom CSS or HTML)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.engine.reload_overlays();
                        window.push_notification(Notification::info("Overlays reloaded"), cx);
                    })),
            )
            .child(
                Button::new("theme-reset")
                    .label("Reset")
                    .small()
                    .disabled(!self.can_reset())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reset(label_for_reset.clone(), window, cx);
                    })),
            );

        let body = match (&entry.manifest, &self.form) {
            (Err(problem), _) => v_flex()
                .gap_2()
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(format!("Couldn't read theme.toml: {problem}")),
                )
                .child(div().text_sm().text_color(muted).child(
                    "Fix theme.toml in the theme folder and focus this window again to reload its options.",
                ))
                .into_any_element(),
            (Ok(manifest), _) if manifest.options.is_empty() => v_flex()
                .gap_2()
                .child(
                    div()
                        .text_color(muted)
                        .child("This custom theme doesn't declare customizable options yet."),
                )
                .child(div().text_sm().text_color(muted).child(
                    "Add a theme.toml file in its folder (or start from \"New theme…\") to add color, font, and layout controls here.",
                ))
                .into_any_element(),
            (Ok(_), Some(form)) => div()
                .id("theme-form")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .pr_2()
                .child(
                    v_flex()
                        .gap_4()
                        .children(self.render_presets(form, &self.extracted_palettes, cx))
                        .child(self.render_form(form, cx)),
                )
                .into_any_element(),
            (Ok(_), None) => div().into_any_element(),
        };

        let subtitle = if is_active {
            "Active overlay theme · changes apply live to connected overlays."
        } else {
            "Changes save per theme and apply live to overlays using this theme."
        };

        pane.child(heading)
            .child(div().text_sm().text_color(muted).child(subtitle))
            .child(body)
    }
}

/// Removes saved options for `key`, also clearing any differently cased entry
/// for a built-in theme (e.g. hand-edited `[overlay.theme_options.Minimal]`).
fn remove_theme_options(all: &mut std::collections::BTreeMap<String, ThemeValues>, key: &str) {
    all.remove(key);
    if let Some(builtin) = Builtin::from_name(key) {
        all.retain(|k, _| Builtin::from_name(k) != Some(builtin));
    }
}

impl Render for ThemesWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_key = self.active_theme_key(cx);
        h_flex()
            .size_full()
            .p_4()
            .gap_3()
            .items_stretch()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_list(&active_key, cx))
            .child(self.render_selected(&active_key, cx))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use chat_engine::theme_options::ThemeValue;
    use chat_engine::{Builtin, ConfigFile, Engine, EngineConfig};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, WindowHandle};

    use super::ThemesWindow;
    use crate::app_config::AppConfig;

    struct Ui {
        window: WindowHandle<Root>,
        view: Entity<ThemesWindow>,
        engine: Engine,
        settings_dir: PathBuf,
        _runtime: tokio::runtime::Runtime,
    }

    impl Drop for Ui {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.settings_dir);
        }
    }

    fn open(cx: &mut TestAppContext) -> Ui {
        cx.update(gpui_kit::init);
        cx.executor().allow_parking();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let settings_dir =
            std::env::temp_dir().join(format!("chat-app-themes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&settings_dir);
        let mut engine_cfg = EngineConfig::default();
        engine_cfg.settings.server.port = 0;
        engine_cfg.themes_dir = Some(ConfigFile::themes_dir(&settings_dir));
        engine_cfg.save_to = Some(settings_dir.join("config.toml"));
        let engine = runtime.block_on(Engine::start(engine_cfg)).unwrap();
        let handle = engine.handle();
        let dir = settings_dir.clone();

        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let config = AppConfig::new(&handle, dir, cx);
            let themes = cx.new(|cx| {
                ThemesWindow::new(
                    handle,
                    config,
                    Builtin::Default.name().to_string(),
                    window,
                    cx,
                )
            });
            view = Some(themes.clone());
            Root::new(themes, window, cx)
        });
        Ui {
            window,
            view: view.expect("themes window built"),
            engine,
            settings_dir,
            _runtime: runtime,
        }
    }

    fn wait_until(cx: &mut TestAppContext, condition: impl Fn(&mut TestAppContext) -> bool) {
        for _ in 0..100 {
            cx.run_until_parked();
            if condition(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not met in time");
    }

    #[gpui_kit::test]
    fn toggling_and_picking_options_saves_and_resets(cx: &mut TestAppContext) {
        let ui = open(cx);
        let handle = ui.engine.handle();

        // Pick "Custom" accent style (option 0, choice 1).
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click(("theme-choice-0", 1usize), cx);
        })
        .unwrap();

        wait_until(cx, |_| {
            handle
                .settings()
                .borrow()
                .profile
                .overlay
                .theme_options
                .get("default")
                .and_then(|v| v.get("accent_mode"))
                == Some(&ThemeValue::Text("Custom".into()))
        });

        // Switch to Minimal (entry 1) and activate it.
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click(("theme-entry", 1usize), cx);
        })
        .unwrap();
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("theme-use-active", cx);
        })
        .unwrap();

        wait_until(cx, |_| {
            handle.settings().borrow().profile.overlay.theme.as_deref() == Some("minimal")
        });

        // Switch back to Default (entry 0) and confirm its saved option is still loaded.
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click(("theme-entry", 0usize), cx);
        })
        .unwrap();
        let can_reset = ui.view.read_with(cx, |v, _| v.can_reset());
        assert!(can_reset, "Default has customized accent_mode");

        // Click the Sakura preset (index 2) and confirm its palette saves live.
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click(("theme-preset", 2usize), cx);
        })
        .unwrap();
        wait_until(cx, |_| {
            handle
                .settings()
                .borrow()
                .profile
                .overlay
                .theme_options
                .get("default")
                .and_then(|v| v.get("accent_color"))
                == Some(&ThemeValue::Text("#ff85b3".into()))
        });
    }

    #[gpui_kit::test]
    fn creating_a_theme_from_a_builtin_copies_its_files(cx: &mut TestAppContext) {
        let ui = open(cx);
        // Open New theme…, choose Minimal (index 1), and create "Cozy Minimal".
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("themes-new", cx);
            window.click(("themes-new-base", 1usize), cx);
            window.click("themes-new-name", cx);
            window.input("Cozy Minimal", cx);
            window.click("themes-new-create", cx);
        })
        .unwrap();

        let dir = ui.settings_dir.join("themes").join("Cozy Minimal");
        wait_until(cx, |_| dir.join("overlay.css").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("overlay.css")).unwrap(),
            Builtin::Minimal.css()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("message.html")).unwrap(),
            Builtin::Minimal.message_template()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("theme.toml")).unwrap(),
            Builtin::Minimal.manifest_toml()
        );
        wait_until(cx, |cx| {
            ui.view.read_with(cx, |v, _| v.selected == "Cozy Minimal")
        });
    }

    #[gpui_kit::test]
    fn invalid_new_theme_name_is_refused(cx: &mut TestAppContext) {
        let ui = open(cx);
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.click("themes-new", cx);
            window.click("themes-new-name", cx);
            window.input("../escape", cx);
            window.click("themes-new-create", cx);
            assert_eq!(window.find("themes-new-name").value(), Some("../escape"));
        })
        .unwrap();
        let error = ui.view.read_with(cx, |v, _| {
            v.new_theme.as_ref().and_then(|f| f.error.clone())
        });
        assert!(error.is_some_and(|e| e.contains("letters")));
        assert!(!ui.settings_dir.join("escape").exists());
    }
}
