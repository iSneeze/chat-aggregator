//! The manifest-driven customization form for a single overlay theme.
//!
//! Built dynamically from [`ThemeManifest`] (`theme.toml`), so both built-in
//! themes and custom theme folders get color pickers, number steppers,
//! switches, and segmented choices without hardcoded per-theme UI code.

use std::collections::HashMap;

use chat_engine::theme_options::{
    OptionKind, ThemeManifest, ThemeOption, ThemeValue, ThemeValues, normalize_hex_color,
};
use chat_render::fonts::{self, FontSelection};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::color_picker::{ColorPickerEvent, ColorPickerState, ColorSelect};
use gpui_kit::component::input::{
    InputEvent, InputState, MaskPattern, NumberInput, NumberInputEvent, NumberStep,
};
use gpui_kit::component::select::{
    SearchableVec, Select, SelectEvent, SelectGroup, SelectItem, SelectState,
};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, IconName, Sizable, StyledExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::ThemesWindow;
use super::palette::SuggestedPalette;
use crate::ui::{choice, parse_number, segmented};

/// Fixed label column width so controls align vertically down the form.
const LABEL_WIDTH: Pixels = px(136.);

/// One installed or imported font item in a `type = "font"` searchable picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FontItem {
    pub(super) title: SharedString,
    pub(super) value: String,
}

impl SelectItem for FontItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

pub(super) type FontSelectDelegate = SearchableVec<SelectGroup<FontItem>>;

/// Builds the grouped searchable font list (`Imported fonts` + `Installed fonts`),
/// preserving `selected` as a fallback entry if a saved font isn't currently on
/// disk or installed on this machine.
pub(super) fn build_font_groups(
    system_fonts: &[String],
    imported_fonts: &[String],
    selected: Option<&str>,
) -> FontSelectDelegate {
    let mut groups = Vec::new();

    // Disambiguate imported files that share a stem (e.g. Foo.ttf + Foo.woff2).
    let mut stem_counts: HashMap<String, usize> = HashMap::new();
    for file in imported_fonts {
        *stem_counts
            .entry(fonts::display_name(file).to_lowercase())
            .or_insert(0) += 1;
    }

    let mut imported_items: Vec<FontItem> = imported_fonts
        .iter()
        .map(|file| {
            let stem = fonts::display_name(file);
            let title = if stem_counts.get(&stem.to_lowercase()).copied().unwrap_or(0) > 1 {
                file.clone()
            } else {
                stem
            };
            FontItem {
                title: title.into(),
                value: fonts::format_file_value(file),
            }
        })
        .collect();

    if let Some(raw) = selected
        && let FontSelection::File(file) = fonts::parse_selection(raw)
        && !imported_fonts.iter().any(|f| f == file)
    {
        imported_items.insert(
            0,
            FontItem {
                title: format!("{} (missing)", fonts::display_name(file)).into(),
                value: fonts::format_file_value(file),
            },
        );
    }

    if !imported_items.is_empty() {
        groups.push(SelectGroup::new("Imported fonts").items(imported_items));
    }

    let mut system_items: Vec<FontItem> = system_fonts
        .iter()
        .map(|family| FontItem {
            title: family.clone().into(),
            value: fonts::format_system_value(family),
        })
        .collect();

    if let Some(raw) = selected
        && let FontSelection::System(family) = fonts::parse_selection(raw)
        && !system_fonts.iter().any(|f| f == family)
    {
        system_items.insert(
            0,
            FontItem {
                title: format!("{family} (not installed)").into(),
                value: fonts::format_system_value(family),
            },
        );
    }

    if !system_items.is_empty() {
        groups.push(SelectGroup::new("Installed fonts").items(system_items));
    }

    SearchableVec::new(groups)
}

/// The live customization form for one theme.
pub(super) struct Form {
    pub(super) theme_key: String,
    pub(super) manifest: ThemeManifest,
    /// Only values that pass validation and differ from the option's default.
    pub(super) values: ThemeValues,
    /// Number input state per option index (`None` for non-number options).
    pub(super) number_fields: Vec<Option<Entity<InputState>>>,
    /// Color picker state per option index (`None` for non-color options).
    pub(super) color_pickers: Vec<Option<Entity<ColorPickerState>>>,
    /// Searchable font picker state per option index (`None` for non-font options).
    pub(super) font_selects: Vec<Option<Entity<SelectState<FontSelectDelegate>>>>,
    /// Validation error per option index.
    pub(super) errors: Vec<Option<String>>,
    _subscriptions: Vec<Subscription>,
}

impl Form {
    pub(super) fn new(
        theme_key: String,
        manifest: ThemeManifest,
        saved: &ThemeValues,
        system_fonts: &[String],
        imported_fonts: &[String],
        window: &mut Window,
        cx: &mut Context<ThemesWindow>,
    ) -> Self {
        let values: ThemeValues = manifest
            .options
            .iter()
            .filter_map(|opt| {
                let raw = saved.get(&opt.name)?;
                let norm = opt.normalize(raw)?;
                (norm != opt.default_value()).then(|| (opt.name.clone(), norm))
            })
            .collect();

        let mut subscriptions = Vec::new();
        let mut number_fields = Vec::with_capacity(manifest.options.len());
        let mut color_pickers = Vec::with_capacity(manifest.options.len());
        let mut font_selects = Vec::with_capacity(manifest.options.len());

        for (i, opt) in manifest.options.iter().enumerate() {
            let current = values
                .get(&opt.name)
                .cloned()
                .unwrap_or_else(|| opt.default_value());

            match (&opt.kind, current) {
                (OptionKind::Number { default, .. }, ThemeValue::Number(number)) => {
                    let input = cx.new(|cx| {
                        let mut input = InputState::new(window, cx)
                            .placeholder(default.to_string())
                            .default_value(number.to_string())
                            .mask_pattern(MaskPattern::None);
                        input.set_step(None::<NumberStep>, window, cx);
                        input
                    });
                    subscriptions.push(cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                        if let InputEvent::Change = event {
                            this.typed(cx);
                        }
                    }));
                    subscriptions.push(cx.subscribe_in(
                        &input,
                        window,
                        move |this, _, event: &NumberInputEvent, window, cx| {
                            let NumberInputEvent::Step(action) = event;
                            this.step(i, *action, window, cx);
                        },
                    ));
                    number_fields.push(Some(input));
                    color_pickers.push(None);
                    font_selects.push(None);
                }
                (OptionKind::Color { .. }, ThemeValue::Text(hex)) => {
                    let initial = hex_to_hsla(&hex);
                    let picker = cx.new(|cx| {
                        let state = ColorPickerState::new(window, cx);
                        match initial {
                            Some(color) => state.default_value(color),
                            None => state,
                        }
                    });
                    subscriptions.push(cx.subscribe(
                        &picker,
                        move |this, _, event: &ColorPickerEvent, cx| {
                            if let ColorPickerEvent::Change(Some(color)) = event {
                                this.pick_color(i, hsla_to_hex(*color), cx);
                            }
                        },
                    ));
                    number_fields.push(None);
                    color_pickers.push(Some(picker));
                    font_selects.push(None);
                }
                (OptionKind::Font { .. }, ThemeValue::Text(current_font)) => {
                    let custom = matches!(
                        fonts::parse_selection(&current_font),
                        FontSelection::System(_) | FontSelection::File(_)
                    )
                    .then_some(current_font.as_str());
                    let groups = build_font_groups(system_fonts, imported_fonts, custom);
                    let select = cx.new(|cx| {
                        let mut state = SelectState::new(groups, None, window, cx).searchable(true);
                        if let Some(val) = custom {
                            state.set_selected_value(&val.to_string(), window, cx);
                        }
                        state
                    });
                    subscriptions.push(cx.subscribe_in(
                        &select,
                        window,
                        move |this, _, event: &SelectEvent<FontSelectDelegate>, window, cx| {
                            let SelectEvent::Confirm(picked) = event;
                            match picked {
                                Some(value) => {
                                    this.pick(i, ThemeValue::Text(value.clone()), cx);
                                }
                                None => {
                                    this.reset_font_option(i, window, cx);
                                }
                            }
                        },
                    ));
                    number_fields.push(None);
                    color_pickers.push(None);
                    font_selects.push(Some(select));
                }
                _ => {
                    number_fields.push(None);
                    color_pickers.push(None);
                    font_selects.push(None);
                }
            }
        }

        Self {
            errors: vec![None; manifest.options.len()],
            theme_key,
            manifest,
            values,
            number_fields,
            color_pickers,
            font_selects,
            _subscriptions: subscriptions,
        }
    }

    /// Reads typed number fields into `self.values`, recording any validation
    /// errors inline without discarding previously valid values.
    pub(super) fn read_fields(&mut self, cx: &App) {
        for (i, opt) in self.manifest.options.iter().enumerate() {
            let Some(field) = &self.number_fields[i] else {
                continue;
            };
            match typed_number(opt, &field.read(cx).value()) {
                Ok(value) => {
                    self.errors[i] = None;
                    set(&mut self.values, opt, value);
                }
                Err(problem) => self.errors[i] = Some(problem),
            }
        }
    }

    pub(super) fn has_errors(&self) -> bool {
        self.errors.iter().any(Option::is_some)
    }

    pub(super) fn has_color_options(&self) -> bool {
        self.manifest
            .options
            .iter()
            .any(|o| matches!(o.kind, OptionKind::Color { .. }))
    }

    /// Pushes `self.values` back into every `ColorPickerState`, number
    /// `InputState`, and font `SelectState` without emitting change events
    /// (used after applying a 1-click preset or image palette so controls stay
    /// in sync without rebuilding the form or losing scroll position).
    pub(super) fn sync_controls(
        &mut self,
        system_fonts: &[String],
        imported_fonts: &[String],
        window: &mut Window,
        cx: &mut App,
    ) {
        for (i, opt) in self.manifest.options.iter().enumerate() {
            self.errors[i] = None;
            let current = self.current(opt);
            match (&opt.kind, current) {
                (OptionKind::Color { .. }, ThemeValue::Text(hex)) => {
                    if let (Some(picker), Some(color)) = (&self.color_pickers[i], hex_to_hsla(&hex))
                    {
                        picker.update(cx, |state, cx| state.set_value(color, window, cx));
                    }
                }
                (OptionKind::Number { .. }, ThemeValue::Number(n)) => {
                    if let Some(field) = &self.number_fields[i] {
                        field.update(cx, |input, cx| {
                            input.set_value(n.to_string(), window, cx);
                        });
                    }
                }
                (OptionKind::Font { .. }, ThemeValue::Text(font)) => {
                    if let Some(select) = &self.font_selects[i] {
                        sync_font_select(select, &font, system_fonts, imported_fonts, window, cx);
                    }
                }
                _ => {}
            }
        }
    }

    /// Refreshes the item list of every font picker after `fonts/` is rescanned
    /// or a new font file is imported, keeping current selections intact.
    pub(super) fn refresh_fonts(
        &self,
        system_fonts: &[String],
        imported_fonts: &[String],
        window: &mut Window,
        cx: &mut App,
    ) {
        for (i, opt) in self.manifest.options.iter().enumerate() {
            let Some(select) = self.font_selects.get(i).and_then(Option::as_ref) else {
                continue;
            };
            if let ThemeValue::Text(font) = self.current(opt) {
                sync_font_select(select, &font, system_fonts, imported_fonts, window, cx);
            }
        }
    }

    pub(super) fn current(&self, opt: &ThemeOption) -> ThemeValue {
        self.values
            .get(&opt.name)
            .cloned()
            .unwrap_or_else(|| opt.default_value())
    }
}

/// Updates a font picker's items and selection for `current_font` without
/// emitting a `Confirm` event.
fn sync_font_select(
    select: &Entity<SelectState<FontSelectDelegate>>,
    current_font: &str,
    system_fonts: &[String],
    imported_fonts: &[String],
    window: &mut Window,
    cx: &mut App,
) {
    let custom = matches!(
        fonts::parse_selection(current_font),
        FontSelection::System(_) | FontSelection::File(_)
    )
    .then_some(current_font);
    let groups = build_font_groups(system_fonts, imported_fonts, custom);
    select.update(cx, |state, cx| {
        state.set_items(groups, window, cx);
        match custom {
            Some(val) => state.set_selected_value(&val.to_string(), window, cx),
            None => state.set_selected_index(None, window, cx),
        }
    });
}

/// Inserts `value` for `opt`, or removes it if it equals the option's default.
pub(super) fn set(values: &mut ThemeValues, opt: &ThemeOption, value: ThemeValue) {
    let Some(norm) = opt.normalize(&value) else {
        return;
    };
    if norm == opt.default_value() {
        values.remove(&opt.name);
    } else {
        values.insert(opt.name.clone(), norm);
    }
}

fn typed_number(opt: &ThemeOption, text: &str) -> Result<ThemeValue, String> {
    let OptionKind::Number {
        default,
        min,
        max,
        unit,
        ..
    } = &opt.kind
    else {
        return Ok(opt.default_value());
    };
    if text.trim().is_empty() {
        return Ok(ThemeValue::Number(*default));
    }
    match parse_number(text) {
        Some(n) if n.is_finite() && (*min..=*max).contains(&n) => Ok(ThemeValue::Number(n)),
        _ => {
            let unit = unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default();
            Err(format!("A number from {min} to {max}{unit}"))
        }
    }
}

pub(super) fn hex_to_hsla(hex: &str) -> Option<Hsla> {
    let norm = normalize_hex_color(hex)?;
    let bytes = norm.as_bytes();
    let parse_byte = |start: usize| u8::from_str_radix(&norm[start..start + 2], 16).ok();
    if bytes.len() != 7 {
        return None;
    }
    let r = parse_byte(1)? as f32 / 255.0;
    let g = parse_byte(3)? as f32 / 255.0;
    let b = parse_byte(5)? as f32 / 255.0;
    Some(Rgba { r, g, b, a: 1.0 }.into())
}

pub(super) fn hsla_to_hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", ch(rgba.r), ch(rgba.g), ch(rgba.b))
}

impl ThemesWindow {
    /// Renders curated 1-click presets and optional image-extracted palettes
    /// above the option groups.
    pub(super) fn render_presets(
        &self,
        form: &Form,
        extracted: &[SuggestedPalette],
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let has_presets = !form.manifest.presets.is_empty();
        let has_colors = form.has_color_options();
        if !has_presets && !has_colors {
            return None;
        }
        let theme = cx.theme();
        let border = theme.border;
        let heading_color = theme.foreground;
        let muted = theme.muted_foreground;

        let preset_chips: Vec<_> = form
            .manifest
            .presets
            .iter()
            .enumerate()
            .map(|(i, preset)| {
                let active = form.manifest.matches_preset(preset, &form.values);
                let swatches = form.manifest.preset_swatches(preset);
                render_palette_chip(
                    ("theme-preset", i),
                    preset.name.clone(),
                    &swatches,
                    active,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.apply_preset(i, window, cx);
                }))
                .test_support()
                .into_any_element()
            })
            .collect();

        let extracted_chips: Vec<_> = extracted
            .iter()
            .enumerate()
            .map(|(i, pal)| {
                let swatches: Vec<String> = pal.swatches().iter().map(|s| (*s).into()).collect();
                render_palette_chip(
                    ("theme-extracted", i),
                    pal.label.clone(),
                    &swatches,
                    false,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.apply_extracted_palette(i, window, cx);
                }))
                .test_support()
                .into_any_element()
            })
            .collect();

        Some(
            v_flex()
                .gap_2()
                .pb_3()
                .border_b_1()
                .border_color(border)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .font_semibold()
                                .text_color(heading_color)
                                .child("Presets"),
                        )
                        .when(has_colors, |row| {
                            row.child(
                                Button::new("theme-match-image")
                                    .label("Match image…")
                                    .small()
                                    .tooltip(
                                        "Pick a screenshot or artwork to generate matching overlay colors",
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_palette_image(window, cx);
                                    })),
                            )
                        }),
                )
                .when(has_presets, |col| {
                    col.child(h_flex().gap_2().flex_wrap().children(preset_chips))
                })
                .when(!extracted_chips.is_empty(), |col| {
                    col.child(
                        v_flex()
                            .gap_1p5()
                            .pt_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(muted)
                                            .child("Matched from image (click to apply):"),
                                    )
                                    .child(
                                        Button::new("theme-clear-extracted")
                                            .icon(IconName::Close)
                                            .xsmall()
                                            .ghost()
                                            .tooltip("Dismiss image suggestions")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.extracted_palettes.clear();
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(h_flex().gap_2().flex_wrap().children(extracted_chips)),
                    )
                }),
        )
    }

    /// Renders all options in `form`, grouped under section headers when
    /// options declare a `group`.
    pub(super) fn render_form(&self, form: &Form, cx: &mut Context<Self>) -> impl IntoElement {
        struct Section<'a> {
            group: Option<&'a str>,
            items: Vec<(usize, &'a ThemeOption)>,
        }
        let mut sections: Vec<Section<'_>> = Vec::new();
        for (i, opt) in form.manifest.options.iter().enumerate() {
            let group = opt.group.as_deref();
            if let Some(last) = sections.last_mut()
                && last.group == group
            {
                last.items.push((i, opt));
            } else {
                sections.push(Section {
                    group,
                    items: vec![(i, opt)],
                });
            }
        }

        let border = cx.theme().border;
        let heading_color = cx.theme().foreground;
        let blocks: Vec<_> = sections
            .into_iter()
            .enumerate()
            .map(|(sec_idx, section)| {
                let rows: Vec<_> = section
                    .items
                    .into_iter()
                    .map(|(i, opt)| self.render_option(i, opt, form, cx).into_any_element())
                    .collect();
                v_flex()
                    .gap_2p5()
                    .when(sec_idx > 0, |col| {
                        col.pt_3().border_t_1().border_color(border)
                    })
                    .when_some(section.group, |col, title| {
                        col.child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .text_color(heading_color)
                                .child(title.to_string()),
                        )
                    })
                    .children(rows)
                    .into_any_element()
            })
            .collect();
        v_flex().gap_4().children(blocks)
    }

    fn render_option(
        &self,
        i: usize,
        opt: &ThemeOption,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let control = match &opt.kind {
            OptionKind::Color { .. } => {
                if let Some(picker) = form.color_pickers.get(i).cloned().flatten() {
                    div()
                        .id(("theme-color", i))
                        .w(px(160.))
                        .child(
                            ColorSelect::new(&picker)
                                .small()
                                .accessibility_label(opt.label.clone()),
                        )
                        .test_support()
                        .into_any_element()
                } else {
                    div().into_any_element()
                }
            }
            OptionKind::Number { unit, .. } => {
                if let Some(field) = form.number_fields.get(i).cloned().flatten() {
                    div()
                        .id(("theme-number", i))
                        .w(px(180.))
                        .child(
                            NumberInput::new(&field)
                                .small()
                                .accessibility_label(opt.label.clone())
                                .when_some(unit.clone(), |input, unit| {
                                    input.suffix(div().text_sm().text_color(muted).child(unit))
                                }),
                        )
                        .test_support()
                        .into_any_element()
                } else {
                    div().into_any_element()
                }
            }
            OptionKind::Bool { .. } => {
                let on = matches!(form.current(opt), ThemeValue::Bool(true));
                Switch::new(("theme-switch", i))
                    .checked(on)
                    .on_click(cx.listener(move |this, on: &bool, _, cx| {
                        this.pick(i, ThemeValue::Bool(*on), cx);
                    }))
                    .into_any_element()
            }
            OptionKind::Choice { options, .. } => {
                let picked = match form.current(opt) {
                    ThemeValue::Text(s) => s,
                    _ => String::new(),
                };
                let id = SharedString::from(format!("theme-choice-{i}"));
                segmented(
                    cx,
                    options.iter().enumerate().map(|(n, label)| {
                        let value = ThemeValue::Text(label.clone());
                        choice((id.clone(), n), label.clone(), *label == picked).on_click(
                            cx.listener(move |this, _, _, cx| this.pick(i, value.clone(), cx)),
                        )
                    }),
                )
                .into_any_element()
            }
            OptionKind::Font { options, .. } => {
                let picked = match form.current(opt) {
                    ThemeValue::Text(s) => s,
                    _ => String::new(),
                };
                let id = SharedString::from(format!("theme-choice-{i}"));
                let presets = segmented(
                    cx,
                    options.iter().enumerate().map(|(n, label)| {
                        let value = ThemeValue::Text(label.clone());
                        choice((id.clone(), n), label.clone(), *label == picked).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.pick_font_preset(i, value.clone(), window, cx);
                            }),
                        )
                    }),
                );
                let custom_row = form.font_selects.get(i).cloned().flatten().map(|select| {
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(
                            div().flex_1().min_w_0().max_w(px(260.)).child(
                                Select::new(&select)
                                    .id(("theme-font", i))
                                    .small()
                                    .cleanable(true)
                                    .placeholder("Installed or imported font…")
                                    .search_placeholder("Search fonts…")
                                    .highlight_matches(true)
                                    .menu_width(px(280.))
                                    .accessibility_label(format!("{} custom font", opt.label)),
                            ),
                        )
                        .child(
                            Button::new(("theme-font-import", i))
                                .label("Import…")
                                .small()
                                .tooltip("Import a .ttf, .otf, .woff2, or .woff font file")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.import_font_for_option(i, window, cx);
                                })),
                        )
                        .child(
                            Button::new(("theme-font-folder", i))
                                .icon(IconName::FolderOpen)
                                .small()
                                .ghost()
                                .tooltip("Open imported fonts folder")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_fonts_folder(window, cx);
                                })),
                        )
                });
                v_flex()
                    .gap_1p5()
                    .child(presets)
                    .children(custom_row)
                    .into_any_element()
            }
        };

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
                            .child(opt.label.clone()),
                    )
                    .child(v_flex().flex_1().min_w_0().child(control)),
            )
            .children(opt.help.clone().map(|help| note(help).text_color(muted)))
            .children(
                form.errors
                    .get(i)
                    .cloned()
                    .flatten()
                    .map(|problem| note(problem).text_color(cx.theme().danger)),
            )
    }
}

/// Clickable preset / palette chip showing up to 3 circular color dots next to
/// the palette's name.
fn render_palette_chip(
    id: impl Into<ElementId>,
    label: String,
    swatches: &[String],
    active: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let dots = swatches.iter().filter_map(|hex| hex_to_hsla(hex)).map(|c| {
        div()
            .size(px(10.))
            .rounded_full()
            .bg(c)
            .border_1()
            .border_color(theme.border)
    });
    h_flex()
        .id(id)
        .gap_1p5()
        .items_center()
        .px_2p5()
        .py_1()
        .rounded(theme.radius)
        .border_1()
        .cursor_pointer()
        .text_sm()
        .border_color(if active { theme.primary } else { theme.border })
        .bg(if active {
            theme.primary.opacity(0.14)
        } else {
            theme.transparent
        })
        .hover(|s| s.bg(theme.accent))
        .child(h_flex().gap(px(3.)).items_center().children(dots))
        .child(label)
}
