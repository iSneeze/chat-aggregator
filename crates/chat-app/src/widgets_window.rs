//! The Widgets window: the widgets in a list on the left (the built-in ones
//! first, then those imported into the widgets folder), the picked one's
//! settings on the right, and its URL to copy into an OBS browser source.
//!
//! The form is built from the settings a widget declares in its own page
//! (the block described in `chat_server::widgets`), so a new widget needs
//! no code here. The values picked are kept in `config.toml`
//! (`[widgets.<name>]`) and go into the widget's URL: OBS gets them with
//! the address, so there's nothing to keep in sync.
//!
//! Switches and choices apply at once. Typed values apply once typing
//! pauses, as in the settings window, so the "7" on the way to "72" is
//! never refused as too small; so do the − and + of a number field, so
//! ten clicks save once.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use chat_engine::EngineHandle;
use chat_engine::widgets::{
    self, Import, Kind, Manifest, Setting, Value, Values, Widget, WidgetError,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{
    Input, InputEvent, InputState, MaskPattern, NumberInput, NumberInputEvent, NumberStep,
    StepAction,
};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable, IconName, Sizable, StyledExt, WindowExt, h_flex,
    scroll::ScrollableElement, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_config::{AppConfig, edit_settings};
use crate::app_view::{card, report, segmented};
use crate::reactions_window::{choice, confirm};
use crate::settings_window::APPLY_AFTER;

/// The width of a setting's label: the controls line up after it.
const LABEL_WIDTH: Pixels = px(140.);

/// A widget in the list.
#[derive(Debug, PartialEq)]
struct Entry {
    /// Its name in the URL (and its file's, if imported).
    name: String,
    builtin: bool,
    /// The widget, or why its file can't be one.
    widget: Result<Widget, WidgetError>,
}

impl Entry {
    /// What the window calls it: its title, or its file's name if the file
    /// can't be read as a widget.
    fn title(&self) -> &str {
        self.widget
            .as_ref()
            .map_or(&self.name, |widget| &widget.manifest.title)
    }
}

/// The built-in widgets, then the ones in the widgets folder `dir`.
fn scan(dir: &Path) -> Vec<Entry> {
    let builtin = Widget::builtin().iter().map(|widget| Entry {
        name: widget.name.clone(),
        builtin: true,
        widget: Ok(widget.clone()),
    });
    let imported = widgets::imported(dir).into_iter().map(|found| Entry {
        name: found.name,
        builtin: false,
        widget: found.widget,
    });
    builtin.chain(imported).collect()
}

/// What the window knows about the widget `name` in `entries`: its
/// settings, why it can't be used, or `None` if it's not there.
fn shown<'a>(entries: &'a [Entry], name: &str) -> Option<Result<&'a Manifest, &'a WidgetError>> {
    entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.widget.as_ref().map(|widget| &widget.manifest))
}

/// The form for one widget's settings.
struct Form {
    widget: Widget,
    /// The values picked: only those that fit and differ from the default,
    /// which is what `config.toml` gets and the URL carries. Typed values
    /// join them once typing pauses.
    values: Values,
    /// The text field of each number and text setting, by the setting's
    /// place in the block (`None` for switches and choices).
    fields: Vec<Option<Entity<InputState>>>,
    /// What's wrong with a typed value, by the setting's place.
    errors: Vec<Option<String>>,
    // Dropped with the form: a replaced form's fields stop reporting.
    _subscriptions: Vec<Subscription>,
}

impl Form {
    fn new(
        widget: Widget,
        saved: &Values,
        window: &mut Window,
        cx: &mut Context<WidgetsWindow>,
    ) -> Self {
        let settings = &widget.manifest.settings;
        // Saved values that no longer fit (the widget was updated since)
        // are left out, and so leave `config.toml` with the next change.
        let values: Values = settings
            .iter()
            .filter_map(|setting| {
                let value = saved.get(&setting.name)?;
                let wanted = setting.accepts(value) && *value != setting.default_value();
                wanted.then(|| (setting.name.clone(), value.clone()))
            })
            .collect();
        let mut subscriptions = Vec::new();
        let fields = settings
            .iter()
            .enumerate()
            .map(|(i, setting)| {
                let current = values
                    .get(&setting.name)
                    .cloned()
                    .unwrap_or_else(|| setting.default_value());
                let (text, placeholder) = match (&setting.kind, current) {
                    // An empty number field means the default: it shows
                    // greyed out.
                    (Kind::Number { default, .. }, Value::Number(number)) => {
                        (number.to_string(), default.to_string())
                    }
                    // No placeholder: an empty text is a value of its own
                    // (no sticker, say), not the default.
                    (Kind::Text { .. }, Value::Text(text)) => (text, String::new()),
                    _ => return None,
                };
                let number = matches!(setting.kind, Kind::Number { .. });
                let input = cx.new(|cx| {
                    let mut input = InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(text);
                    if number {
                        // A number field (`NumberInput`) would only take
                        // digits and a dot; `typed_value` takes a comma too,
                        // and says what's wrong with anything else.
                        input = input.mask_pattern(MaskPattern::None);
                        // Without a step of its own, − and + only report
                        // (`NumberInputEvent::Step`): `step` does them.
                        input.set_step(None::<NumberStep>, window, cx);
                    }
                    input
                });
                subscriptions.push(cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        this.typed(cx);
                    }
                }));
                if number {
                    subscriptions.push(cx.subscribe_in(
                        &input,
                        window,
                        move |this, _, event: &NumberInputEvent, window, cx| {
                            // Its only kind of event: `let` takes it apart
                            // without a `match` (and stops compiling if
                            // gpui-component adds another, to be looked at).
                            let NumberInputEvent::Step(action) = event;
                            this.step(i, *action, window, cx);
                        },
                    ));
                }
                Some(input)
            })
            .collect();
        Self {
            errors: vec![None; settings.len()],
            widget,
            values,
            fields,
            _subscriptions: subscriptions,
        }
    }

    /// Takes the typed values in: those that fit replace what was picked
    /// before; the others get a note saying what's wrong, and the last
    /// value that fit stays in place.
    fn read_fields(&mut self, cx: &App) {
        let settings = &self.widget.manifest.settings;
        for (i, setting) in settings.iter().enumerate() {
            let Some(field) = &self.fields[i] else {
                continue;
            };
            match typed_value(setting, &field.read(cx).value()) {
                Ok(value) => {
                    self.errors[i] = None;
                    set(&mut self.values, setting, value);
                }
                Err(problem) => self.errors[i] = Some(problem),
            }
        }
    }

    fn has_errors(&self) -> bool {
        self.errors.iter().any(Option::is_some)
    }

    /// The value the form shows for `setting`: picked, or the default.
    fn current(&self, setting: &Setting) -> Value {
        self.values
            .get(&setting.name)
            .cloned()
            .unwrap_or_else(|| setting.default_value())
    }
}

/// Picks `value` for `setting`: the default isn't kept (the URL leaves it
/// out anyway, and a widget's new default then applies).
fn set(values: &mut Values, setting: &Setting, value: Value) {
    if value == setting.default_value() {
        values.remove(&setting.name);
    } else {
        values.insert(setting.name.clone(), value);
    }
}

/// A typed value for `setting`, or what's wrong with it.
fn typed_value(setting: &Setting, text: &str) -> Result<Value, String> {
    match &setting.kind {
        Kind::Number {
            default,
            min,
            max,
            unit,
            ..
        } => {
            if text.trim().is_empty() {
                return Ok(Value::Number(*default));
            }
            match parse_number(text) {
                // `contains` is false for "NaN", and "inf" is out of range.
                Some(number) if (min..=max).contains(&&number) => Ok(Value::Number(number)),
                _ => {
                    let unit = unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default();
                    Err(format!("A number from {min} to {max}{unit}"))
                }
            }
        }
        Kind::Text { .. } => Ok(Value::Text(text.to_string())),
        // Switches and choices have no text field: nothing typed to read.
        Kind::Bool { .. } | Kind::Choice { .. } => Ok(setting.default_value()),
    }
}

/// A typed number, in or out of range.
fn parse_number(text: &str) -> Option<f64> {
    // A comma too: "0,5" is how much of the world writes a half.
    text.trim().replace(',', ".").parse().ok()
}

/// One step up or down from `from`, onto the grid of `min + n × step` that
/// a browser's number field uses too (from 56, steps of 4 from 8 go to 60
/// or 52; a typed 57 goes to 60 or 56), and kept from `min` to `max`.
fn stepped(from: f64, action: StepAction, min: f64, max: f64, step: f64) -> f64 {
    // How many steps above `min`. A hair of slack, so float noise
    // (0.1 + 0.2 is 0.30000000000000004) doesn't count as off the grid.
    let steps = (from - min) / step;
    let steps = match action {
        StepAction::Increment => (steps + 1e-9).floor() + 1.0,
        StepAction::Decrement => (steps - 1e-9).ceil() - 1.0,
    };
    // Rounded to the decimals the step and `min` are written with, so the
    // field shows "0.3", not that noise.
    let scale = 10f64.powi(decimals(step).max(decimals(min)));
    let value = ((min + steps * step) * scale).round() / scale;
    // `max`/`min` rather than `clamp`, which panics if `min > max` (a
    // widget's block can't say that, but nothing here has to rely on it).
    value.max(min).min(max)
}

/// How many decimals `x` is written with (0.25: 2).
fn decimals(x: f64) -> i32 {
    // `f64`'s `Display` writes the shortest text that reads back as the
    // same number ("0.1"), never with an exponent.
    let text = x.to_string();
    text.split_once('.')
        .map_or(0, |(_, fraction)| fraction.len() as i32)
}

pub struct WidgetsWindow {
    engine: EngineHandle,
    config: Entity<AppConfig>,
    /// For reporting problems from places that don't get the window (the
    /// typing timer, closing).
    window: AnyWindowHandle,
    /// The overlay's address (`http://127.0.0.1:7878/`): the widgets are
    /// served next to it, so their URLs follow a new port.
    server: String,
    entries: Vec<Entry>,
    /// The widget shown on the right, by name.
    selected: String,
    /// Its settings, if it's a widget that can be used.
    form: Option<Form>,
    /// The pending "apply what's typed" timer. Storing a new one drops the
    /// old one, and dropping a GPUI `Task` cancels it (the debounce, as in
    /// the settings window).
    pending: Option<Task<()>>,
    /// Typed values not taken in yet.
    dirty: bool,
    /// Why the last file picked couldn't be imported.
    import_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl WidgetsWindow {
    pub fn new(
        engine: EngineHandle,
        config: Entity<AppConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries = scan(&config.read(cx).widgets_dir());
        let server = engine.status().borrow().overlay_url.clone();
        let subscriptions = vec![
            // The JSON API switched on, here or in the settings: the
            // banner goes.
            cx.observe(&config, |_, _, cx| cx.notify()),
            // Back from editing a widget's file or the widgets folder: show
            // them as they are now.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.rescan(window, cx);
                }
            }),
            // Closing the window within the typing pause mustn't lose the
            // last edit: apply it now.
            cx.on_release(|this, cx| {
                if this.dirty {
                    this.apply(cx);
                }
            }),
        ];
        Self::watch_server(&engine, window, cx);
        let first = entries
            .first()
            .map(|entry| entry.name.clone())
            .unwrap_or_default();
        let mut view = Self {
            engine,
            config,
            window: window.window_handle(),
            server,
            entries,
            selected: String::new(),
            form: None,
            pending: None,
            dirty: false,
            import_error: None,
            _subscriptions: subscriptions,
        };
        view.select(first, window, cx);
        view
    }

    /// Keeps `server` up to date: a new port in the settings moves the
    /// widgets too.
    fn watch_server(engine: &EngineHandle, window: &mut Window, cx: &mut Context<Self>) {
        let mut status = engine.status();
        // As in the main window: a weak handle, so the loop ends with the
        // window.
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

    fn widgets_dir(&self, cx: &App) -> PathBuf {
        self.config.read(cx).widgets_dir()
    }

    // ---- the list ----

    /// Shows the widget `name` on the right, with the values saved for it.
    fn select(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        // What's typed for the widget being left is kept, not dropped.
        if self.dirty {
            self.apply(cx);
        }
        let saved = self.config.read(cx).settings().widgets.get(&name).cloned();
        self.selected = name;
        self.open_form(saved.unwrap_or_default(), window, cx);
    }

    /// A fresh form for the selected widget, showing `values`.
    fn open_form(&mut self, values: Values, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = None;
        self.dirty = false;
        let widget = self
            .entries
            .iter()
            .find(|entry| entry.name == self.selected)
            .and_then(|entry| entry.widget.as_ref().ok())
            .cloned();
        self.form = widget.map(|widget| Form::new(widget, &values, window, cx));
        cx.notify();
    }

    /// Reads the widgets folder again. The form is only rebuilt if the
    /// widget it shows changed its settings (or broke, or got fixed): an
    /// edit to its look keeps what's being typed.
    fn rescan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = scan(&self.widgets_dir(cx));
        if entries == self.entries {
            return;
        }
        let now = shown(&entries, &self.selected);
        let changed = now != shown(&self.entries, &self.selected);
        let gone = now.is_none();
        self.entries = entries;
        if gone {
            // Its file was deleted or renamed: show the first one instead.
            let first = self.entries.first().map(|e| e.name.clone());
            self.select(first.unwrap_or_default(), window, cx);
        } else if changed {
            if self.dirty {
                self.apply(cx);
            }
            // The form's own values: newer than the settings' while the
            // engine is still saving them.
            let values = match &self.form {
                Some(form) => form.values.clone(),
                None => self
                    .config
                    .read(cx)
                    .settings()
                    .widgets
                    .get(&self.selected)
                    .cloned()
                    .unwrap_or_default(),
            };
            self.open_form(values, window, cx);
        }
        cx.notify();
    }

    /// Asks for a widget file to import.
    fn pick_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import a widget (an .html file)".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = paths.await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        view.import_file(path, window, cx);
                    }
                }
                Ok(Ok(None)) | Err(_) => {} // cancelled
                // On Linux the dialog goes through xdg-desktop-portal,
                // which needs a portal backend installed.
                Ok(Err(e)) => report(
                    window,
                    cx,
                    format!(
                        "Couldn't open the file dialog ({e}). You can also copy the widget's \
                         file into the widgets folder (Open folder)."
                    ),
                ),
            });
        })
        .detach();
    }

    /// Checks the file `path` and imports it: right away if it's new,
    /// after asking if it would replace a widget of the same name.
    fn import_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.import_error = None;
        cx.notify();
        let dir = self.widgets_dir(cx);
        let file = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        // File work off the UI thread, so the window never stutters.
        let checking = cx
            .background_executor()
            .spawn(async move { widgets::prepare_import(&dir, &path) });
        cx.spawn_in(window, async move |this, cx| {
            let checked = checking.await;
            let _ = this.update_in(cx, |view, window, cx| match checked {
                Err(problem) => {
                    view.import_error = Some(format!("{file} can't be imported: {problem}."));
                    cx.notify();
                }
                Ok(import) if import.replaces => {
                    let title = import.widget.manifest.title.clone();
                    // The question's answer may come back more than once
                    // (an `Fn`): it shares the import rather than moving it.
                    let import = Rc::new(import);
                    let weak = cx.entity().downgrade();
                    confirm(
                        window,
                        cx,
                        format!("Replace \"{title}\"?"),
                        "A widget of that name is imported already, likely an older version \
                         of it. The new file takes its place; the values picked for it stay \
                         where they still fit.",
                        "Replace",
                        move |window, cx| {
                            if let Some(view) = weak.upgrade() {
                                view.update(cx, |view, cx| view.save_import(&import, window, cx));
                            }
                        },
                    );
                }
                Ok(import) => view.save_import(&import, window, cx),
            });
        })
        .detach();
    }

    fn save_import(&mut self, import: &Import, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = import.save(&self.widgets_dir(cx)) {
            self.import_error = Some(format!("Couldn't save the widget: {e}"));
            cx.notify();
            return;
        }
        // If it replaced the widget shown, the rescan updates the form.
        self.rescan(window, cx);
        if self.selected != import.widget.name {
            self.select(import.widget.name.clone(), window, cx);
        }
        let title = &import.widget.manifest.title;
        window.push_notification(Notification::success(format!("Imported \"{title}\"")), cx);
    }

    fn open_folder(&self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.widgets_dir(cx);
        // It only exists once a widget was imported; create it so there's
        // something to open (and to put widgets into).
        if let Err(e) = std::fs::create_dir_all(&dir) {
            report(
                window,
                cx,
                format!("Couldn't create {}: {e}", dir.display()),
            );
            return;
        }
        cx.open_with_system(&dir);
    }

    /// Asks, then deletes the selected imported widget's file and forgets
    /// the values picked for it.
    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.name == self.selected && !entry.builtin)
        else {
            return;
        };
        let name = entry.name.clone();
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            format!("Remove \"{}\"?", entry.title()),
            "Its file is deleted from the widgets folder, and the values picked for it are \
             forgotten. OBS sources showing it go blank.",
            "Remove",
            move |window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.remove_now(name.clone(), window, cx));
                }
            },
        );
    }

    fn remove_now(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        // Its form goes without applying: a pending edit would otherwise
        // save its values again.
        if self.selected == name {
            self.form = None;
            self.pending = None;
            self.dirty = false;
        }
        match widgets::remove(&self.widgets_dir(cx), &name) {
            Ok(()) => {}
            // Already gone (deleted in the file manager): only the values
            // are left to forget.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                report(window, cx, format!("Couldn't remove the widget: {e}"));
                return;
            }
        }
        edit_settings(
            &self.engine,
            window.window_handle(),
            move |s| {
                s.widgets.remove(&name);
            },
            cx,
        );
        self.rescan(window, cx);
    }

    // ---- the form ----

    /// Whether Reset has anything to do: a value that isn't the default,
    /// something typed and not taken in yet, or a typed value with a note.
    fn can_reset(&self) -> bool {
        self.form
            .as_ref()
            .is_some_and(|form| !form.values.is_empty() || self.dirty || form.has_errors())
    }

    /// Reset: asks, then puts every setting of the selected widget back to
    /// the default its block gives.
    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let name = form.widget.name.clone();
        let view = cx.entity().downgrade();
        confirm(
            window,
            cx,
            format!("Reset \"{}\"?", form.widget.manifest.title),
            "Every setting goes back to the widget's own default. OBS sources keep their \
             URL until you copy the new one in.",
            "Reset",
            move |window, cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| view.reset_now(name.clone(), window, cx));
                }
            },
        );
    }

    fn reset_now(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == name {
            // A fresh form shows the defaults, and drops what's typed along
            // with its timer (`open_form`): taking it in would only be
            // undone here.
            self.open_form(Values::new(), window, cx);
        }
        edit_settings(
            &self.engine,
            window.window_handle(),
            move |s| {
                s.widgets.remove(&name);
            },
            cx,
        );
    }

    /// A field was typed in: (re)starts the timer that takes it in.
    fn typed(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        cx.notify(); // Reset may have something to do now
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(APPLY_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                this.apply(cx);
                cx.notify();
            });
        }));
    }

    /// − or + (or ↑ ↓) on the number field of setting `i`: one step from
    /// what it shows, taken in like typing.
    fn step(&mut self, i: usize, action: StepAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let Some(setting) = form.widget.manifest.settings.get(i) else {
            return;
        };
        // Copies of the numbers (`f64` is `Copy`); `..` leaves the unit,
        // a `String`, where it is.
        let Kind::Number {
            default,
            min,
            max,
            step,
            ..
        } = setting.kind
        else {
            return;
        };
        let Some(field) = form.fields.get(i).cloned().flatten() else {
            return;
        };
        let text = field.read(cx).value();
        let from = if text.trim().is_empty() {
            default // what an empty field shows
        } else if let Some(typed) = parse_number(&text) {
            typed // out of range too: − brings a typed 9999 back in
        } else {
            // No number at all: from the last value that fit.
            match form.current(setting) {
                Value::Number(fitting) => fitting,
                _ => default,
            }
        };
        // No step in the block: 1, like a browser's number field.
        let to = stepped(from, action, min, max, step.unwrap_or(1.0));
        if to == from {
            return; // at the end already
        }
        field.update(cx, |field, cx| field.set_value(to.to_string(), window, cx));
        // `set_value` doesn't report a change (only typing does): say it.
        self.typed(cx);
        cx.notify();
    }

    /// A switch or choice: applies at once, along with what's typed.
    fn pick(&mut self, setting: usize, value: Value, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let Some(setting) = form.widget.manifest.settings.get(setting) else {
            return;
        };
        set(&mut form.values, setting, value);
        // Dropping the pending timer cancels it; `apply` does its work now.
        self.pending = None;
        self.apply(cx);
        cx.notify();
    }

    /// Takes the typed values in and saves the widget's values (none at all
    /// removes its table from `config.toml`). Takes `App` rather than
    /// `Context<Self>`: it's also called while the window is being
    /// released, when only `App` is available.
    fn apply(&mut self, cx: &mut App) {
        self.dirty = false;
        let Some(form) = &mut self.form else {
            return;
        };
        form.read_fields(cx);
        let name = form.widget.name.clone();
        let values = form.values.clone();
        edit_settings(
            &self.engine,
            self.window,
            move |s| {
                if values.is_empty() {
                    s.widgets.remove(&name);
                } else {
                    s.widgets.insert(name, values);
                }
            },
            cx,
        );
    }

    /// The URL with the values picked, for the clipboard.
    fn copy_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // What's typed counts, even within the pause.
        if self.dirty {
            self.pending = None;
            self.apply(cx);
            cx.notify();
        }
        let Some(form) = &self.form else {
            return;
        };
        if form.has_errors() {
            report(
                window,
                cx,
                "Some settings need fixing first: they're marked in red.".into(),
            );
            return;
        }
        let url = form.widget.url(&self.server, &form.values);
        cx.write_to_clipboard(ClipboardItem::new_string(url));
        window.push_notification(Notification::success("Widget URL copied"), cx);
    }

    fn switch_api_on(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        edit_settings(
            &self.engine,
            window.window_handle(),
            |s| s.server.api = true,
            cx,
        );
    }

    // ---- drawing ----

    /// Widgets get chat through the JSON API: without it they show nothing.
    fn render_api_banner(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        h_flex()
            .gap_3()
            .items_center()
            .p_3()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().warning)
            .child(
                div().flex_1().text_color(cx.theme().warning).child(
                    "Widgets get chat through the JSON API, which is off: they can't connect.",
                ),
            )
            .child(
                Button::new("widgets-api-on")
                    .label("Switch it on")
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.switch_api_on(window, cx))),
            )
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| self.render_entry(i, entry, cx).into_any_element())
            .collect::<Vec<_>>();
        let muted = cx.theme().muted_foreground;
        card(cx)
            .w(px(300.))
            .flex_none()
            .min_h_0()
            .child(
                div()
                    .id("widget-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(v_flex().gap_1().children(rows)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("widget-import")
                            .label("Import…")
                            .icon(IconName::Plus)
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_file(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("widget-folder")
                            .label("Open folder")
                            .icon(IconName::FolderOpen)
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_folder(window, cx)),
                            ),
                    ),
            )
            .child(div().text_sm().text_color(muted).child(
                "Only import widgets from people you trust: a widget runs its own code in OBS.",
            ))
            .children(
                self.import_error
                    .clone()
                    .map(|e| div().text_sm().text_color(cx.theme().danger).child(e)),
            )
    }

    fn render_entry(&self, i: usize, entry: &Entry, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (note, color) = match &entry.widget {
            Err(problem) => (problem.to_string(), theme.danger),
            Ok(_) if entry.builtin => ("built in".to_string(), theme.muted_foreground),
            Ok(_) => ("imported".to_string(), theme.muted_foreground),
        };
        let name = entry.name.clone();
        v_flex()
            .id(("widget", i))
            .p_1()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(entry.name == self.selected, |row| row.bg(theme.accent))
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.selected != name {
                    this.select(name.clone(), window, cx);
                }
            }))
            .child(
                div()
                    .font_semibold()
                    .text_ellipsis()
                    .child(entry.title().to_string()),
            )
            .child(div().text_sm().text_color(color).line_clamp(2).child(note))
            // Findable by its id in the UI tests (does nothing otherwise).
            // Last: it wraps the element.
            .test_support()
    }

    fn render_selected(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let pane = card(cx).flex_1().min_w_0().min_h_0();
        let Some(entry) = self.entries.iter().find(|e| e.name == self.selected) else {
            return pane;
        };
        let heading = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_lg()
                    .font_semibold()
                    .child(entry.title().to_string()),
            )
            // Only with a form: a file that can't be a widget has no
            // settings to reset.
            .when(self.form.is_some(), |row| {
                row.child(
                    Button::new("widget-reset")
                        .label("Reset")
                        .small()
                        .disabled(!self.can_reset())
                        .on_click(cx.listener(|this, _, window, cx| this.reset(window, cx))),
                )
            })
            .when(!entry.builtin, |row| {
                row.child(
                    Button::new("widget-remove")
                        .label("Remove")
                        .icon(IconName::Delete)
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| this.remove(window, cx))),
                )
            });
        let pane = pane.child(heading);
        let (Ok(widget), Some(form)) = (&entry.widget, &self.form) else {
            let problem = entry
                .widget
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            return pane
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child(format!("This file can't be used as a widget: {problem}.")),
                )
                .child(div().text_color(muted).child(
                    "Fix it in the widgets folder (it's read again when you come back to this \
                     window), or remove it.",
                ));
        };

        let url = widget.url(&self.server, &form.values);
        let address = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family("monospace")
                    .text_sm()
                    .child(url),
            )
            .child(
                Button::new("widget-copy")
                    .icon(IconName::Copy)
                    .label("Copy URL")
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.copy_url(window, cx))),
            );
        let rows = widget
            .manifest
            .settings
            .iter()
            .enumerate()
            .map(|(i, setting)| self.render_setting(i, setting, form, cx).into_any_element())
            .collect::<Vec<_>>();
        let settings = if rows.is_empty() {
            div()
                .text_color(muted)
                .child("This widget has no settings.")
                .into_any_element()
        } else {
            div()
                .id("widget-settings")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(v_flex().gap_3().children(rows))
                .into_any_element()
        };

        let description = &widget.manifest.description;
        pane.when(!description.is_empty(), |pane| {
            pane.child(div().text_color(muted).child(description.clone()))
        })
        .child(address)
        .child(settings)
    }

    fn render_setting(
        &self,
        i: usize,
        setting: &Setting,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let field = form.fields.get(i).cloned().flatten();
        let control = match (&setting.kind, form.current(setting), field) {
            // − value unit +: the buttons and ↑ ↓ report to `step`.
            (Kind::Number { unit, .. }, _, Some(field)) => div()
                .id(("widget-field", i))
                .w(px(200.))
                .child(
                    NumberInput::new(&field)
                        .accessibility_label(setting.label.clone())
                        .when_some(unit.clone(), |input, unit| {
                            input.suffix(div().text_sm().text_color(muted).child(unit))
                        }),
                )
                // Findable in the UI tests, to click − or + within it.
                .test_support()
                .into_any_element(),
            (Kind::Text { .. }, _, Some(field)) => Input::new(&field)
                .id(("widget-field", i))
                .into_any_element(),
            (Kind::Bool { .. }, Value::Bool(on), _) => Switch::new(("widget-switch", i))
                .checked(on)
                .on_click(cx.listener(move |this, on: &bool, _, cx| {
                    this.pick(i, Value::Bool(*on), cx);
                }))
                .into_any_element(),
            (Kind::Choice { options, .. }, Value::Text(picked), _) => {
                // One id per setting, each option numbered within it.
                let id = SharedString::from(format!("widget-choice-{i}"));
                segmented(
                    cx,
                    options.iter().enumerate().map(|(n, option)| {
                        let value = Value::Text(option.clone());
                        choice((id.clone(), n), option.clone(), *option == picked).on_click(
                            cx.listener(move |this, _, _, cx| this.pick(i, value.clone(), cx)),
                        )
                    }),
                )
                .into_any_element()
            }
            // A value of the wrong type can't be current (`Form::new` and
            // `set` only keep fitting ones), nor a typed setting without
            // its field.
            _ => div().into_any_element(),
        };
        // Help and problems line up under the control.
        let note = |text: String| div().pl(LABEL_WIDTH + px(12.)).text_sm().child(text);
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .w(LABEL_WIDTH)
                            .flex_none()
                            .text_color(muted)
                            .child(setting.label.clone()),
                    )
                    .child(div().flex_1().min_w_0().child(control)),
            )
            .children(
                setting
                    .help
                    .clone()
                    .map(|help| note(help).text_color(muted)),
            )
            .children(
                form.errors
                    .get(i)
                    .cloned()
                    .flatten()
                    .map(|problem| note(problem).text_color(cx.theme().danger)),
            )
    }
}

impl Render for WidgetsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let api_on = self.config.read(cx).settings().server.api;
        let banner = (!api_on).then(|| self.render_api_banner(cx));
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                v_flex()
                    .child(div().text_lg().font_semibold().child("Widgets"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                "Pages that show chat their own way, as OBS browser sources: pick \
                         their settings here, then paste the URL into a browser source.",
                            ),
                    ),
            )
            .children(banner)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .items_stretch()
                    .child(self.render_list(cx))
                    .child(self.render_selected(cx)),
            )
    }
}

/// UI tests: the real window, headless, driven by simulated clicks and
/// typing (see `app_view::tests` for how these work).
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use chat_engine::widgets::{Value, Values, Widget};
    use chat_engine::{ConfigFile, Engine, EngineConfig, Settings};
    use gpui_kit::component::Root;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, Entity, SharedString, TestAppContext, WindowHandle};

    use super::WidgetsWindow;
    use crate::app_config::AppConfig;
    use crate::settings_window::APPLY_AFTER;

    struct Ui {
        window: WindowHandle<Root>,
        /// `None` once a test lets go of it, so closing releases it.
        view: Option<Entity<WidgetsWindow>>,
        engine: Engine,
        dir: PathBuf,
        _runtime: tokio::runtime::Runtime,
    }

    impl Drop for Ui {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Ui {
        fn view(&self) -> &Entity<WidgetsWindow> {
            self.view.as_ref().unwrap()
        }

        fn widgets_dir(&self) -> PathBuf {
            ConfigFile::widgets_dir(&self.dir)
        }

        fn settings(&self) -> Settings {
            self.engine.handle().settings().borrow().clone()
        }

        /// The values the engine has for the widget `name`.
        fn saved(&self, name: &str) -> Option<Values> {
            self.settings().widgets.get(name).cloned()
        }

        fn saved_value(&self, name: &str, setting: &str) -> Option<Value> {
            self.saved(name)?.get(setting).cloned()
        }
    }

    /// The window with the JSON API on, after `prepare` had its way with
    /// the engine's settings and the (empty) settings folder.
    fn open(cx: &mut TestAppContext, prepare: impl FnOnce(&mut EngineConfig, &Path)) -> Ui {
        cx.update(gpui_kit::init);
        // The engine answers from its tokio threads (see
        // `manual_window::tests::run_until`).
        cx.executor().allow_parking();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("chat-app-widgets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = EngineConfig::default();
        config.settings.server.port = 0;
        config.settings.server.api = true;
        config.save_to = Some(dir.join("config.toml"));
        config.widgets_dir = Some(ConfigFile::widgets_dir(&dir));
        prepare(&mut config, &dir);
        let engine = runtime.block_on(Engine::start(config)).unwrap();
        let handle = engine.handle();
        let settings_dir = dir.clone();
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let config = AppConfig::new(&handle, settings_dir, cx);
            let widgets = cx.new(|cx| WidgetsWindow::new(handle, config, window, cx));
            view = Some(widgets.clone());
            Root::new(widgets, window, cx)
        });
        Ui {
            window,
            view,
            engine,
            dir,
            _runtime: runtime,
        }
    }

    /// A small widget with one setting, a size defaulting to `size`.
    fn page(title: &str, size: u32) -> String {
        format!(
            concat!(
                "<!doctype html><title>{title}</title>\n",
                r#"<script type="application/json" id="widget">"#,
                r#"{{"title": "{title}", "settings": [{{"name": "size", "label": "Size", "#,
                r#""type": "number", "default": {size}, "min": 8, "max": 512, "unit": "px"}}]}}"#,
                "</script>\n<script>document.body.dataset.ready = '1';</script>\n",
            ),
            title = title,
            size = size,
        )
    }

    /// Where the built-in Live Emoji has `setting`: the form's controls are
    /// numbered by it.
    fn live_emoji(setting: &str) -> usize {
        let widget = &Widget::builtin()[0];
        assert_eq!(widget.name, "live-emoji");
        let settings = &widget.manifest.settings;
        settings.iter().position(|s| s.name == setting).unwrap()
    }

    fn run_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        for _ in 0..500 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    fn act(
        cx: &mut TestAppContext,
        ui: &Ui,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App),
    ) {
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            f(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    /// Runs `f` on the view, with its window.
    fn with_view(
        cx: &mut TestAppContext,
        ui: &Ui,
        f: impl FnOnce(&mut WidgetsWindow, &mut gpui_kit::Window, &mut gpui_kit::Context<WidgetsWindow>),
    ) {
        let view = ui.view().clone();
        cx.update_window(ui.window.into(), |_, window, cx| {
            view.update(cx, |view, cx| f(view, window, cx));
        })
        .unwrap();
        cx.run_until_parked();
    }

    /// Replaces the text of the form's field `setting` (typing would add to
    /// it), as typing would report it.
    fn type_into(cx: &mut TestAppContext, ui: &Ui, setting: usize, text: &str) {
        with_view(cx, ui, |view, window, cx| {
            let field = view.form.as_ref().unwrap().fields[setting].clone().unwrap();
            field.update(cx, |field, cx| field.set_value(text, window, cx));
            // `set_value` doesn't report a change (only typing does): say it.
            view.typed(cx);
        });
    }

    fn selected(cx: &mut TestAppContext, ui: &Ui) -> String {
        ui.view().read_with(cx, |view, _| view.selected.clone())
    }

    fn names(cx: &mut TestAppContext, ui: &Ui) -> Vec<String> {
        ui.view().read_with(cx, |view, _| {
            view.entries.iter().map(|e| e.name.clone()).collect()
        })
    }

    fn error(cx: &mut TestAppContext, ui: &Ui, setting: usize) -> Option<String> {
        ui.view().read_with(cx, |view, _| {
            view.form
                .as_ref()
                .and_then(|form| form.errors[setting].clone())
        })
    }

    fn clipboard(cx: &mut TestAppContext) -> Option<String> {
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    fn dialog_open(cx: &mut TestAppContext, ui: &Ui) -> bool {
        cx.update_window(ui.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find("dialog").is_some()
        })
        .unwrap()
    }

    /// As in `app_view::tests`: a dialog takes clicks once it has faded in.
    fn click_in_dialog(cx: &mut TestAppContext, ui: &Ui, button: &'static str) {
        let fade_in = *gpui_kit::component::dialog::ANIMATION_DURATION;
        std::thread::sleep(fade_in + Duration::from_millis(50));
        act(cx, ui, |window, cx| {
            window.within("dialog").click(button, cx)
        });
    }

    #[gpui_kit::test]
    fn picked_values_are_saved_and_go_into_the_url(cx: &mut TestAppContext) {
        let ui = open(cx, |_, _| {});
        assert_eq!(selected(cx, &ui), "live-emoji", "the built-in one first");
        let base = format!("http://{}/widgets/live-emoji", ui.engine.addr());
        act(cx, &ui, |window, cx| window.click("widget-copy", cx));
        assert_eq!(clipboard(cx), Some(base.clone()), "nothing picked yet");

        // A choice applies at once...
        let calm = SharedString::from(format!("widget-choice-{}", live_emoji("mode")));
        act(cx, &ui, |window, cx| window.click((calm, 1usize), cx));
        let calm = Some(Value::Text("calm".into()));
        run_until(cx, |_| ui.saved_value("live-emoji", "mode") == calm);

        // ...a typed value once typing pauses.
        let size = live_emoji("size");
        type_into(cx, &ui, size, "7");
        type_into(cx, &ui, size, "72");
        cx.executor().advance_clock(APPLY_AFTER / 2);
        cx.run_until_parked();
        assert_eq!(
            ui.saved_value("live-emoji", "size"),
            None,
            "not while typing"
        );
        cx.executor().advance_clock(APPLY_AFTER);
        run_until(cx, |_| {
            ui.saved_value("live-emoji", "size") == Some(Value::Number(72.))
        });
        assert_eq!(
            error(cx, &ui, size),
            None,
            "the \"7\" on the way was never refused"
        );

        act(cx, &ui, |window, cx| window.click("widget-copy", cx));
        assert_eq!(clipboard(cx), Some(format!("{base}?mode=calm&size=72")));
        let file = ui.dir.join("config.toml");
        run_until(cx, |_| {
            ConfigFile::load(&file)
                .is_ok_and(|f| f.widgets["live-emoji"].get("size") == Some(&Value::Number(72.)))
        });

        // Copy takes what's typed, within the pause too.
        type_into(cx, &ui, size, "96");
        act(cx, &ui, |window, cx| window.click("widget-copy", cx));
        assert_eq!(clipboard(cx), Some(format!("{base}?mode=calm&size=96")));

        // Back to the defaults: nothing left to keep.
        type_into(cx, &ui, size, "");
        let party = SharedString::from(format!("widget-choice-{}", live_emoji("mode")));
        act(cx, &ui, |window, cx| window.click((party, 0usize), cx));
        run_until(cx, |_| ui.saved("live-emoji").is_none());
        act(cx, &ui, |window, cx| window.click("widget-copy", cx));
        assert_eq!(clipboard(cx), Some(base));
    }

    /// Clicks − (`"decrement"`) or + (`"increment"`) of the form's number
    /// field `setting`.
    fn step_button(cx: &mut TestAppContext, ui: &Ui, setting: usize, button: &'static str) {
        act(cx, ui, |window, cx| {
            window.within(("widget-field", setting)).click(button, cx)
        });
    }

    /// What the form's field `setting` shows.
    fn shown(cx: &mut TestAppContext, ui: &Ui, setting: usize) -> String {
        ui.view().read_with(cx, |view, cx| {
            let form = view.form.as_ref().unwrap();
            form.fields[setting]
                .as_ref()
                .unwrap()
                .read(cx)
                .value()
                .to_string()
        })
    }

    #[gpui_kit::test]
    fn minus_and_plus_step_a_number_and_save_once_clicking_pauses(cx: &mut TestAppContext) {
        let ui = open(cx, |_, dir| {
            let widgets = ConfigFile::widgets_dir(dir);
            std::fs::create_dir_all(&widgets).unwrap();
            // Its size has no step.
            std::fs::write(widgets.join("test-widget.html"), page("Test Widget", 48)).unwrap();
        });
        // 56 by default, from 8 to 512 in steps of 4.
        let size = live_emoji("size");
        assert_eq!(shown(cx, &ui, size), "56");
        step_button(cx, &ui, size, "increment");
        step_button(cx, &ui, size, "increment");
        assert_eq!(shown(cx, &ui, size), "64");
        cx.executor().advance_clock(APPLY_AFTER / 2);
        cx.run_until_parked();
        assert_eq!(
            ui.saved_value("live-emoji", "size"),
            None,
            "not while clicking"
        );
        cx.executor().advance_clock(APPLY_AFTER);
        run_until(cx, |_| {
            ui.saved_value("live-emoji", "size") == Some(Value::Number(64.))
        });
        let base = format!("http://{}/widgets/live-emoji", ui.engine.addr());
        act(cx, &ui, |window, cx| window.click("widget-copy", cx));
        assert_eq!(clipboard(cx), Some(format!("{base}?size=64")));

        // Onto the grid from a typed value between two steps.
        type_into(cx, &ui, size, "57");
        step_button(cx, &ui, size, "increment");
        assert_eq!(shown(cx, &ui, size), "60");
        type_into(cx, &ui, size, "57");
        step_button(cx, &ui, size, "decrement");
        assert_eq!(shown(cx, &ui, size), "56");

        // An empty field steps from the default it shows.
        type_into(cx, &ui, size, "");
        step_button(cx, &ui, size, "decrement");
        assert_eq!(shown(cx, &ui, size), "52");

        // Not past the ends; a typed value beyond them comes back in.
        type_into(cx, &ui, size, "512");
        step_button(cx, &ui, size, "increment");
        assert_eq!(shown(cx, &ui, size), "512");
        type_into(cx, &ui, size, "9999");
        step_button(cx, &ui, size, "decrement");
        assert_eq!(shown(cx, &ui, size), "512");
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, |_| {
            ui.saved_value("live-emoji", "size") == Some(Value::Number(512.))
        });
        assert_eq!(error(cx, &ui, size), None);

        // Without a step in the block: steps of 1.
        act(cx, &ui, |window, cx| window.click(("widget", 1usize), cx));
        assert_eq!(selected(cx, &ui), "test-widget");
        step_button(cx, &ui, 0, "increment");
        assert_eq!(shown(cx, &ui, 0), "49");
    }

    #[gpui_kit::test]
    fn reset_asks_then_puts_back_the_defaults(cx: &mut TestAppContext) {
        let ui = open(cx, |config, _| {
            let values = Values::from([
                ("mode".to_string(), Value::Text("calm".into())),
                ("size".to_string(), Value::Number(72.)),
            ]);
            config.settings.widgets.insert("live-emoji".into(), values);
        });
        let size = live_emoji("size");
        assert_eq!(shown(cx, &ui, size), "72");

        // Cancel keeps it all, what's typed too.
        type_into(cx, &ui, size, "96");
        act(cx, &ui, |window, cx| window.click("widget-reset", cx));
        assert!(dialog_open(cx, &ui));
        click_in_dialog(cx, &ui, "cancel");
        assert_eq!(shown(cx, &ui, size), "96");
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, |_| {
            ui.saved_value("live-emoji", "size") == Some(Value::Number(96.))
        });

        // Reset, with something typed on top that isn't taken in yet.
        type_into(cx, &ui, size, "100");
        act(cx, &ui, |window, cx| window.click("widget-reset", cx));
        click_in_dialog(cx, &ui, "ok");
        run_until(cx, |_| ui.saved("live-emoji").is_none());
        assert_eq!(shown(cx, &ui, size), "56", "the block's default");
        let file = ui.dir.join("config.toml");
        run_until(cx, |_| {
            ConfigFile::load(&file).is_ok_and(|f| !f.widgets.contains_key("live-emoji"))
        });
        // The typed "100" was dropped, not saved after all.
        cx.executor().advance_clock(APPLY_AFTER * 2);
        cx.run_until_parked();
        assert_eq!(ui.saved("live-emoji"), None);
        let base = format!("http://{}/widgets/live-emoji", ui.engine.addr());
        act(cx, &ui, |window, cx| window.click("widget-copy", cx));
        assert_eq!(clipboard(cx), Some(base));

        // Nothing left to reset: the button does nothing.
        act(cx, &ui, |window, cx| window.click("widget-reset", cx));
        assert!(!dialog_open(cx, &ui));
        // Typing makes it worth asking again.
        type_into(cx, &ui, size, "60");
        act(cx, &ui, |window, cx| window.click("widget-reset", cx));
        assert!(dialog_open(cx, &ui));
    }

    #[gpui_kit::test]
    fn a_typed_value_that_doesnt_fit_is_not_saved(cx: &mut TestAppContext) {
        let ui = open(cx, |config, _| {
            let values = Values::from([("size".to_string(), Value::Number(72.))]);
            config.settings.widgets.insert("live-emoji".into(), values);
        });
        let size = live_emoji("size");
        for typed in ["9999", "big"] {
            type_into(cx, &ui, size, typed);
            cx.executor().advance_clock(APPLY_AFTER * 2);
            cx.run_until_parked();
            let problem = error(cx, &ui, size);
            assert_eq!(
                problem.as_deref(),
                Some("A number from 8 to 512 px"),
                "{typed}"
            );
        }
        // Copy refuses rather than copying a URL without what's typed.
        act(cx, &ui, |window, cx| window.click("widget-copy", cx));
        assert_eq!(clipboard(cx), None);

        // A switch saves the form: the last size that fit stays.
        let glow = ("widget-switch", live_emoji("glow"));
        act(cx, &ui, |window, cx| window.click(glow, cx));
        run_until(cx, |_| {
            ui.saved_value("live-emoji", "glow") == Some(Value::Bool(false))
        });
        assert_eq!(
            ui.saved_value("live-emoji", "size"),
            Some(Value::Number(72.))
        );

        type_into(cx, &ui, size, "0,5e2"); // a comma for the decimal point
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, |_| {
            ui.saved_value("live-emoji", "size") == Some(Value::Number(50.))
        });
        assert_eq!(error(cx, &ui, size), None);
    }

    #[gpui_kit::test]
    fn closing_the_window_saves_what_is_typed(cx: &mut TestAppContext) {
        let mut ui = open(cx, |_, _| {});
        type_into(cx, &ui, live_emoji("size"), "72");
        // Nothing may hold the view any more for it to be released.
        ui.view = None;
        cx.update_window(ui.window.into(), |_, window, _| window.remove_window())
            .unwrap();
        run_until(cx, |_| {
            ui.saved_value("live-emoji", "size") == Some(Value::Number(72.))
        });
    }

    #[gpui_kit::test]
    fn importing_adds_a_widget_and_replacing_one_asks_first(cx: &mut TestAppContext) {
        let ui = open(cx, |_, _| {});
        let downloads = ui.dir.join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        // Downloaded widgets are often just "index.html": named by title.
        std::fs::write(downloads.join("index.html"), page("Test Widget", 56)).unwrap();
        let file = downloads.join("index.html");
        with_view(cx, &ui, |view, window, cx| {
            view.import_file(file, window, cx)
        });
        run_until(cx, |cx| selected(cx, &ui) == "test-widget");
        assert_eq!(names(cx, &ui), ["live-emoji", "test-widget"]);
        let imported = ui.widgets_dir().join("test-widget.html");
        assert!(imported.is_file());

        type_into(cx, &ui, 0, "72");
        cx.executor().advance_clock(APPLY_AFTER * 2);
        run_until(cx, |_| {
            ui.saved_value("test-widget", "size") == Some(Value::Number(72.))
        });

        // A newer version: same title, a new default.
        let newer = downloads.join("test-widget-2.html");
        std::fs::write(&newer, page("Test Widget", 64)).unwrap();
        let import = |cx: &mut TestAppContext| {
            let file = newer.clone();
            with_view(cx, &ui, |view, window, cx| {
                view.import_file(file, window, cx)
            });
            run_until(cx, |cx| dialog_open(cx, &ui));
        };
        import(cx);
        click_in_dialog(cx, &ui, "cancel");
        run_until(cx, |cx| !dialog_open(cx, &ui));
        let kept = std::fs::read_to_string(&imported).unwrap();
        assert!(kept.contains("\"default\": 56"), "cancel keeps the old one");

        import(cx);
        click_in_dialog(cx, &ui, "ok"); // "Replace"
        run_until(cx, |_| {
            std::fs::read_to_string(&imported).is_ok_and(|page| page.contains("\"default\": 64"))
        });
        assert_eq!(names(cx, &ui), ["live-emoji", "test-widget"]);
        let (default, size) = ui.view().read_with(cx, |view, _| {
            let form = view.form.as_ref().unwrap();
            let setting = &form.widget.manifest.settings[0];
            (setting.default_value(), form.current(setting))
        });
        assert_eq!(
            default,
            Value::Number(64.),
            "the form shows the new version"
        );
        assert_eq!(size, Value::Number(72.), "the value picked stays");
    }

    #[gpui_kit::test]
    fn a_file_that_isnt_a_widget_is_refused_with_the_reason(cx: &mut TestAppContext) {
        let ui = open(cx, |_, _| {});
        let file = ui.dir.join("notes.html");
        std::fs::write(&file, "<!doctype html><p>Not a widget.</p>").unwrap();
        with_view(cx, &ui, |view, window, cx| {
            view.import_file(file, window, cx)
        });
        let refused =
            |cx: &mut TestAppContext| ui.view().read_with(cx, |view, _| view.import_error.clone());
        run_until(cx, |cx| refused(cx).is_some());
        let refused = refused(cx).unwrap();
        assert!(
            refused.starts_with("notes.html can't be imported"),
            "{refused}"
        );
        assert!(refused.contains("no widget block"), "{refused}");
        assert!(!ui.widgets_dir().exists(), "nothing written");
        assert_eq!(names(cx, &ui), ["live-emoji"]);
    }

    #[gpui_kit::test]
    fn removing_deletes_the_file_and_forgets_its_values(cx: &mut TestAppContext) {
        let ui = open(cx, |config, dir| {
            let widgets = ConfigFile::widgets_dir(dir);
            std::fs::create_dir_all(&widgets).unwrap();
            std::fs::write(widgets.join("test-widget.html"), page("Test Widget", 56)).unwrap();
            std::fs::write(widgets.join("broken.html"), "<p>Not a widget.</p>").unwrap();
            let values = Values::from([("size".to_string(), Value::Number(72.))]);
            config.settings.widgets.insert("test-widget".into(), values);
        });
        assert_eq!(names(cx, &ui), ["live-emoji", "broken", "test-widget"]);
        let has_remove = |cx: &mut TestAppContext| {
            cx.update_window(ui.window.into(), |_, window, cx| {
                window.render_frame(cx);
                window.try_find("widget-remove").is_some()
            })
            .unwrap()
        };
        assert!(!has_remove(cx), "the built-in one stays");

        act(cx, &ui, |window, cx| window.click(("widget", 2usize), cx));
        assert_eq!(selected(cx, &ui), "test-widget");
        act(cx, &ui, |window, cx| window.click("widget-remove", cx));
        click_in_dialog(cx, &ui, "ok");
        run_until(cx, |cx| {
            ui.saved("test-widget").is_none() && selected(cx, &ui) == "live-emoji"
        });
        assert!(!ui.widgets_dir().join("test-widget.html").exists());

        // A file that can't be a widget says why, and can go too.
        act(cx, &ui, |window, cx| window.click(("widget", 1usize), cx));
        let problem = ui.view().read_with(cx, |view, _| {
            assert!(view.form.is_none(), "no form for it");
            view.entries[1]
                .widget
                .as_ref()
                .err()
                .map(ToString::to_string)
        });
        assert!(problem.is_some_and(|p| p.contains("no widget block")));
        act(cx, &ui, |window, cx| window.click("widget-remove", cx));
        click_in_dialog(cx, &ui, "ok");
        run_until(cx, |cx| names(cx, &ui) == ["live-emoji"]);
        assert!(!ui.widgets_dir().join("broken.html").exists());
    }

    #[gpui_kit::test]
    fn without_the_json_api_a_banner_switches_it_on(cx: &mut TestAppContext) {
        let ui = open(cx, |config, _| config.settings.server.api = false);
        let banner = |cx: &mut TestAppContext| {
            cx.update_window(ui.window.into(), |_, window, cx| {
                window.render_frame(cx);
                window.try_find("widgets-api-on").is_some()
            })
            .unwrap()
        };
        assert!(banner(cx));
        act(cx, &ui, |window, cx| window.click("widgets-api-on", cx));
        run_until(cx, |cx| ui.settings().server.api && !banner(cx));
    }

    #[test]
    fn a_step_lands_on_the_grid_from_min_within_the_range() {
        use super::stepped;
        use gpui_kit::component::input::StepAction::{Decrement as Down, Increment as Up};

        // Live Emoji's size: 8 to 512, steps of 4.
        let size = |from, action| stepped(from, action, 8., 512., 4.);
        assert_eq!(size(56., Up), 60.);
        assert_eq!(size(56., Down), 52.);
        assert_eq!(size(57., Up), 60., "off the grid: to the next step up");
        assert_eq!(size(57., Down), 56., "...or down");
        assert_eq!(size(512., Up), 512., "not past max");
        assert_eq!(size(8., Down), 8., "nor min");
        assert_eq!(size(9999., Down), 512., "from beyond max: back in");
        assert_eq!(size(2., Up), 8., "from below min too");

        // A max that's off the grid is still reached, and left onto it.
        let thirds = |from, action| stepped(from, action, 0., 10., 3.);
        assert_eq!(thirds(9., Up), 10.);
        assert_eq!(thirds(10., Down), 9.);

        // Tenths come out as the field should show them, not as float
        // noise, and that noise doesn't count as off the grid.
        let tenths = |from, action| stepped(from, action, 0., 3., 0.1);
        assert_eq!(tenths(0.2, Up), 0.3);
        assert_eq!(tenths(0.1 + 0.2, Up), 0.4);
        assert_eq!(tenths(1., Down), 0.9);
        assert_eq!(tenths(0.7, Up).to_string(), "0.8");

        // The grid starts at min, with min's decimals.
        assert_eq!(stepped(0.25, Up, 0.25, 10., 1.), 1.25);
        assert_eq!(stepped(1., Up, 0.25, 10., 1.), 1.25);
    }
}
