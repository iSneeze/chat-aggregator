//! The dynamic settings form for a single widget, built from the settings
//! declared in the widget's HTML manifest (`chat_server::widgets`).

use chat_engine::widgets::{Kind, Setting, Value, Values, Widget};
use gpui_kit::component::input::{
    Input, InputEvent, InputState, MaskPattern, NumberInput, NumberInputEvent, NumberStep,
    StepAction,
};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::WidgetsWindow;
use crate::ui::{choice, segmented};

/// The width of a setting's label: the controls line up after it.
const LABEL_WIDTH: Pixels = px(140.);

/// The form for one widget's settings.
pub(super) struct Form {
    pub(super) widget: Widget,
    /// The values picked: only those that fit and differ from the default,
    /// which is what `config.toml` gets and the URL carries. Typed values
    /// join them once typing pauses.
    pub(super) values: Values,
    /// The text field of each number and text setting, by the setting's
    /// place in the block (`None` for switches and choices).
    pub(super) fields: Vec<Option<Entity<InputState>>>,
    /// What's wrong with a typed value, by the setting's place.
    pub(super) errors: Vec<Option<String>>,
    // Dropped with the form: a replaced form's fields stop reporting.
    _subscriptions: Vec<Subscription>,
}

impl Form {
    pub(super) fn new(
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
    pub(super) fn read_fields(&mut self, cx: &App) {
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

    pub(super) fn has_errors(&self) -> bool {
        self.errors.iter().any(Option::is_some)
    }

    /// The value the form shows for `setting`: picked, or the default.
    pub(super) fn current(&self, setting: &Setting) -> Value {
        self.values
            .get(&setting.name)
            .cloned()
            .unwrap_or_else(|| setting.default_value())
    }
}

/// Picks `value` for `setting`: the default isn't kept (the URL leaves it
/// out anyway, and a widget's new default then applies).
pub(super) fn set(values: &mut Values, setting: &Setting, value: Value) {
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
pub(super) fn parse_number(text: &str) -> Option<f64> {
    // A comma too: "0,5" is how much of the world writes a half.
    text.trim().replace(',', ".").parse().ok()
}

/// One step up or down from `from`, onto the grid of `min + n × step` that
/// a browser's number field uses too (from 56, steps of 4 from 8 go to 60
/// or 52; a typed 57 goes to 60 or 56), and kept from `min` to `max`.
pub(super) fn stepped(from: f64, action: StepAction, min: f64, max: f64, step: f64) -> f64 {
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

impl WidgetsWindow {
    pub(super) fn render_setting(
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
