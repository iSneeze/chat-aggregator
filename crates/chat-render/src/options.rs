//! Theme customization manifests (`theme.toml`) and live style resolution.
//!
//! A theme — built-in or custom — can declare customizable options in a
//! `theme.toml` file alongside `overlay.css` and `message.html`. Each option
//! describes:
//!
//! - Its UI metadata (`name`, `label`, optional `group` and `help`)
//! - Its value type (`color`, `number`, `bool`, or `choice`) and default
//! - How it maps onto the overlay (`css_var` / `css_values` on `:root`, and/or
//!   modifier classes on `<main class="chat">`)
//!
//! Values chosen by the user are stored per profile under
//! `[overlay.theme_options.<theme>]` and resolved through [`ThemeManifest`]
//! into a [`ResolvedStyle`], which `chat-server` both bakes into the initial
//! CSS/HTML response and pushes live over SSE (`event: style`) without
//! reloading connected overlays.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{Context, bail};

use crate::Builtin;
use crate::fonts::{self, FontSelection};

/// File name of a theme's customization manifest inside a theme folder.
pub const MANIFEST_FILE: &str = "theme.toml";

/// Saved values for one theme's options, keyed by option `name`.
pub type ThemeValues = BTreeMap<String, ThemeValue>;

/// One saved value for a theme option.
///
/// Stored directly as a TOML boolean, number, or string under
/// `[overlay.theme_options.<theme>]`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ThemeValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

/// The parsed `theme.toml` of a theme.
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeManifest {
    #[serde(default)]
    pub options: Vec<ThemeOption>,
    /// Optional 1-click presets shown at the top of the Themes window.
    #[serde(default)]
    pub presets: Vec<ThemePreset>,
}

/// A named 1-click preset declared in `theme.toml` (`[[presets]]`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemePreset {
    pub name: String,
    /// Option name -> value applied when the user clicks this preset.
    #[serde(default)]
    pub values: ThemeValues,
}

/// One customizable option declared in `theme.toml`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ThemeOption {
    /// Stable key saved in profile files (ASCII letters, digits, `-`, `_`).
    pub name: String,
    /// Human-readable label shown in the Themes window.
    pub label: String,
    /// Optional section heading in the Themes window (e.g. `"Colors"`).
    #[serde(default)]
    pub group: Option<String>,
    /// Optional short note shown below the control.
    #[serde(default)]
    pub help: Option<String>,
    /// Type, default, and allowed range/choices.
    #[serde(flatten)]
    pub kind: OptionKind,
    /// How this option maps to CSS custom properties and/or `.chat` classes.
    #[serde(flatten)]
    pub target: OptionTarget,
}

/// What kind of control and value an option uses.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OptionKind {
    /// A 6-digit hex color (`#rrggbb`).
    Color { default: String },
    /// A bounded number with an optional step and unit (`px`, `%`, …).
    Number {
        default: f64,
        min: f64,
        max: f64,
        #[serde(default)]
        step: Option<f64>,
        #[serde(default)]
        unit: Option<String>,
        /// Unit appended in CSS when it differs from the UI `unit` (set to
        /// `""` to emit a unitless CSS value). Defaults to `unit`.
        #[serde(default)]
        css_unit: Option<String>,
    },
    /// An on/off switch.
    Bool { default: bool },
    /// One choice from a fixed list of labels.
    Choice {
        default: String,
        options: Vec<String>,
    },
    /// A font selector: curated theme font stacks (like `Choice`), plus any
    /// installed OS font (`system:<Family>`) or imported font file
    /// (`file:<filename>`).
    Font {
        default: String,
        options: Vec<String>,
    },
}

/// Where an option's resolved value lands in the overlay DOM/CSS.
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct OptionTarget {
    /// CSS custom property written on `:root`, e.g. `"--chat-text"`.
    #[serde(default)]
    pub css_var: Option<String>,
    /// Maps a `choice` option label (or `"true"` / `"false"` for a `bool`)
    /// to a safe CSS value for [`Self::css_var`].
    #[serde(default)]
    pub css_values: BTreeMap<String, String>,
    /// Modifier class added to `<main class="chat">` when a `bool` is `true`.
    #[serde(default)]
    pub class: Option<String>,
    /// Modifier class added to `<main class="chat">` when a `bool` is `false`.
    #[serde(default)]
    pub class_when_false: Option<String>,
    /// Maps a `choice` option label to a modifier class on `<main class="chat">`
    /// (empty string or omitted means no class for that choice).
    #[serde(default)]
    pub class_values: BTreeMap<String, String>,
}

/// One imported font file that must be loaded via `@font-face` / `FontFace`
/// before CSS variables reference its `family` name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedFontFace {
    /// CSS `font-family` identifier used inside `--chat-font` (etc.).
    pub family: String,
    /// Validated filename inside the server's `fonts/` directory.
    pub file: String,
}

/// Concrete CSS custom properties, `.chat` classes, and imported font faces
/// resolved from a theme's manifest and saved values.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct ResolvedStyle {
    /// `--var-name` -> CSS value.
    pub vars: BTreeMap<String, String>,
    /// Modifier classes active on `<main class="chat">`.
    pub classes: Vec<String>,
    /// Imported font files referenced by `vars`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fonts: Vec<ResolvedFontFace>,
}

/// Theme options parsed from an overlay URL's query string (`?preset=…` and/or
/// individual option names).
///
/// When present on a request, these start from the theme's defaults rather than
/// the profile's saved options, so each OBS browser source keeps its own pinned
/// configuration even if the theme is edited later for another scene.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UrlThemeOptions {
    /// Effective non-default option values resolved from the query parameters.
    pub values: ThemeValues,
    /// Canonical encoded query string (without leading `?`) to append to the
    /// `<link href="…/overlay.css">` tag so the stylesheet receives the same
    /// pinned options (even when all resolved values equal defaults, e.g.
    /// `?preset=Classic`).
    pub css_query: String,
}

impl ResolvedStyle {
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.classes.is_empty() && self.fonts.is_empty()
    }

    /// Formats `fonts` (`@font-face`) and `vars` (`:root { … }`) as an override
    /// block suitable for appending to `/theme/overlay.css`.
    pub fn to_css(&self) -> String {
        self.to_css_with_font_base("../fonts/")
    }

    /// Like [`Self::to_css`], using `font_base` (e.g. `"../fonts/"` from
    /// `/theme/overlay.css` or `"../../fonts/"` from `/themes/<name>/overlay.css`)
    /// for `@font-face` `src` URLs.
    pub fn to_css_with_font_base(&self, font_base: &str) -> String {
        if self.vars.is_empty() && self.fonts.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n/* theme options */\n");
        for face in &self.fonts {
            let encoded = encode_query_value(&face.file);
            let _ = writeln!(
                out,
                "@font-face {{\n  font-family: \"{}\";\n  src: url(\"{font_base}{encoded}\");\n  font-display: swap;\n}}",
                face.family
            );
        }
        if !self.vars.is_empty() {
            out.push_str(":root {\n");
            for (var, val) in &self.vars {
                let _ = writeln!(out, "  {var}: {val};");
            }
            out.push_str("}\n");
        }
        out
    }
}

impl ThemeOption {
    /// The option's default value as a [`ThemeValue`].
    pub fn default_value(&self) -> ThemeValue {
        match &self.kind {
            OptionKind::Color { default } => ThemeValue::Text(
                normalize_hex_color(default).unwrap_or_else(|| default.to_ascii_lowercase()),
            ),
            OptionKind::Number { default, .. } => ThemeValue::Number(*default),
            OptionKind::Bool { default } => ThemeValue::Bool(*default),
            OptionKind::Choice { default, .. } | OptionKind::Font { default, .. } => {
                ThemeValue::Text(default.clone())
            }
        }
    }

    /// Whether `value` is valid for this option's type and constraints.
    pub fn accepts(&self, value: &ThemeValue) -> bool {
        match (&self.kind, value) {
            (OptionKind::Color { .. }, ThemeValue::Text(s)) => normalize_hex_color(s).is_some(),
            (OptionKind::Number { min, max, .. }, ThemeValue::Number(n)) => {
                n.is_finite() && (*min..=*max).contains(n)
            }
            (OptionKind::Bool { .. }, ThemeValue::Bool(_)) => true,
            (OptionKind::Choice { options, .. }, ThemeValue::Text(s)) => {
                options.iter().any(|o| o == s)
            }
            (OptionKind::Font { options, .. }, ThemeValue::Text(s)) => {
                match fonts::parse_selection(s) {
                    FontSelection::Preset(p) => options.iter().any(|o| o == p),
                    FontSelection::System(family) => fonts::is_valid_system_font(family),
                    FontSelection::File(file) => fonts::is_valid_filename(file),
                }
            }
            _ => false,
        }
    }

    /// Normalizes a valid value (e.g. hex color casing or font prefix spacing)
    /// so equality against [`Self::default_value`] is consistent.
    pub fn normalize(&self, value: &ThemeValue) -> Option<ThemeValue> {
        if !self.accepts(value) {
            return None;
        }
        match (&self.kind, value) {
            (OptionKind::Color { .. }, ThemeValue::Text(s)) => {
                normalize_hex_color(s).map(ThemeValue::Text)
            }
            (OptionKind::Font { .. }, ThemeValue::Text(s)) => match fonts::parse_selection(s) {
                FontSelection::Preset(_) => Some(value.clone()),
                FontSelection::System(family) => {
                    Some(ThemeValue::Text(fonts::format_system_value(family)))
                }
                FontSelection::File(file) => Some(ThemeValue::Text(fonts::format_file_value(file))),
            },
            _ => Some(value.clone()),
        }
    }

    /// Parses a single URL query parameter value according to this option's
    /// type and bounds. Accepts bare hex colors (`ff85b3`) in addition to
    /// `#ff85b3`, `1`/`0`/`true`/`false` for booleans, case-insensitive
    /// choice labels, and `system:<Family>` / `file:<filename>` for fonts.
    pub fn parse_query_value(&self, raw: &str) -> Option<ThemeValue> {
        let trimmed = raw.trim();
        match &self.kind {
            OptionKind::Color { .. } => {
                let candidate = if trimmed.starts_with('#') {
                    trimmed.to_string()
                } else {
                    format!("#{trimmed}")
                };
                normalize_hex_color(&candidate).map(ThemeValue::Text)
            }
            OptionKind::Number { min, max, .. } => {
                let n: f64 = trimmed.parse().ok()?;
                (n.is_finite() && (*min..=*max).contains(&n)).then_some(ThemeValue::Number(n))
            }
            OptionKind::Bool { .. } => match trimmed.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Some(ThemeValue::Bool(true)),
                "false" | "0" | "no" | "off" => Some(ThemeValue::Bool(false)),
                _ => None,
            },
            OptionKind::Choice { options, .. } => options
                .iter()
                .find(|o| o.as_str() == trimmed)
                .or_else(|| options.iter().find(|o| o.eq_ignore_ascii_case(trimmed)))
                .cloned()
                .map(ThemeValue::Text),
            OptionKind::Font { options, .. } => match fonts::parse_selection(trimmed) {
                FontSelection::System(_) | FontSelection::File(_) => {
                    self.normalize(&ThemeValue::Text(trimmed.to_string()))
                }
                FontSelection::Preset(_) => options
                    .iter()
                    .find(|o| o.as_str() == trimmed)
                    .or_else(|| options.iter().find(|o| o.eq_ignore_ascii_case(trimmed)))
                    .cloned()
                    .map(ThemeValue::Text),
            },
        }
    }

    /// Formats a normalized value compactly for a URL query parameter (hex
    /// colors drop the leading `#` to avoid `%23`).
    fn format_query_value(&self, value: &ThemeValue) -> String {
        match (&self.kind, value) {
            (OptionKind::Color { .. }, ThemeValue::Text(hex)) => {
                hex.strip_prefix('#').unwrap_or(hex).to_string()
            }
            (_, ThemeValue::Bool(b)) => b.to_string(),
            (_, ThemeValue::Number(n)) => format_number(*n),
            (_, ThemeValue::Text(s)) => s.clone(),
        }
    }

    fn apply_to(&self, value: &ThemeValue, out: &mut ResolvedStyle) {
        match (&self.kind, value) {
            (OptionKind::Color { .. }, ThemeValue::Text(hex)) => {
                if let (Some(var), Some(norm)) = (&self.target.css_var, normalize_hex_color(hex)) {
                    out.vars.insert(var.clone(), norm);
                }
            }
            (OptionKind::Number { unit, css_unit, .. }, ThemeValue::Number(n)) => {
                if let Some(var) = &self.target.css_var {
                    let suffix = css_unit.as_deref().or(unit.as_deref()).unwrap_or_default();
                    out.vars
                        .insert(var.clone(), format!("{}{suffix}", format_number(*n)));
                }
            }
            (OptionKind::Bool { .. }, ThemeValue::Bool(on)) => {
                if let Some(var) = &self.target.css_var {
                    let key = if *on { "true" } else { "false" };
                    if let Some(val) = self.target.css_values.get(key) {
                        out.vars.insert(var.clone(), val.clone());
                    }
                }
                let cls = if *on {
                    self.target.class.as_deref()
                } else {
                    self.target.class_when_false.as_deref()
                };
                if let Some(cls) = cls
                    && !out.classes.iter().any(|c| c == cls)
                {
                    out.classes.push(cls.to_string());
                }
            }
            (OptionKind::Choice { .. }, ThemeValue::Text(choice)) => {
                if let Some(var) = &self.target.css_var
                    && let Some(val) = self.target.css_values.get(choice)
                {
                    out.vars.insert(var.clone(), val.clone());
                }
                if let Some(cls) = self.target.class_values.get(choice)
                    && !cls.is_empty()
                    && !out.classes.iter().any(|c| c == cls)
                {
                    out.classes.push(cls.clone());
                }
            }
            (OptionKind::Font { .. }, ThemeValue::Text(picked)) => {
                let Some(var) = &self.target.css_var else {
                    return;
                };
                match fonts::parse_selection(picked) {
                    FontSelection::Preset(choice) => {
                        if let Some(val) = self.target.css_values.get(choice) {
                            out.vars.insert(var.clone(), val.clone());
                        }
                        if let Some(cls) = self.target.class_values.get(choice)
                            && !cls.is_empty()
                            && !out.classes.iter().any(|c| c == cls)
                        {
                            out.classes.push(cls.clone());
                        }
                    }
                    FontSelection::System(family) if fonts::is_valid_system_font(family) => {
                        out.vars
                            .insert(var.clone(), format!(r#""{family}", system-ui, sans-serif"#));
                    }
                    FontSelection::File(file) if fonts::is_valid_filename(file) => {
                        let family = fonts::css_family_name(file);
                        out.vars
                            .insert(var.clone(), format!(r#""{family}", system-ui, sans-serif"#));
                        if !out.fonts.iter().any(|f| f.family == family) {
                            out.fonts.push(ResolvedFontFace {
                                family,
                                file: file.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl ThemeManifest {
    /// Parses and validates a `theme.toml` manifest.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let mut manifest: Self = toml::from_str(text).context("invalid theme.toml")?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Resolves *all* options in the manifest (using saved values where valid,
    /// otherwise each option's default). Used for live SSE `style` updates so
    /// resetting an option back to default overrides any value previously baked
    /// into the stylesheet.
    pub fn resolve(&self, values: Option<&ThemeValues>) -> ResolvedStyle {
        let mut out = ResolvedStyle::default();
        for opt in &self.options {
            let effective = values
                .and_then(|v| v.get(&opt.name))
                .and_then(|v| opt.normalize(v))
                .unwrap_or_else(|| opt.default_value());
            opt.apply_to(&effective, &mut out);
        }
        out
    }

    /// Resolves *only* options whose saved value differs from the option's
    /// default. Used when serving `overlay.css` so an uncustomized theme is
    /// served byte-for-byte unchanged.
    pub fn resolve_overrides(&self, values: Option<&ThemeValues>) -> ResolvedStyle {
        let Some(values) = values else {
            return ResolvedStyle::default();
        };
        let mut out = ResolvedStyle::default();
        for opt in &self.options {
            let Some(saved) = values.get(&opt.name).and_then(|v| opt.normalize(v)) else {
                continue;
            };
            if saved != opt.default_value() {
                opt.apply_to(&saved, &mut out);
            }
        }
        out
    }

    /// Whether `current` saved values match every setting declared by `preset`.
    /// Options not mentioned in `preset.values` (such as font size or timestamp
    /// visibility) are ignored so users can keep their layout preferences when
    /// trying color palettes.
    pub fn matches_preset(&self, preset: &ThemePreset, current: &ThemeValues) -> bool {
        if preset.values.is_empty() {
            return false;
        }
        preset.values.iter().all(|(name, wanted)| {
            let Some(opt) = self.options.iter().find(|o| o.name == *name) else {
                return false;
            };
            let effective = current
                .get(name)
                .and_then(|v| opt.normalize(v))
                .unwrap_or_else(|| opt.default_value());
            effective == *wanted
        })
    }

    /// Up to 3 representative hex colors (`#rrggbb`) for drawing swatch dots
    /// on a preset button in the UI.
    pub fn preset_swatches(&self, preset: &ThemePreset) -> Vec<String> {
        let mut swatches = Vec::new();
        for opt in &self.options {
            if !matches!(opt.kind, OptionKind::Color { .. }) {
                continue;
            }
            let val = preset
                .values
                .get(&opt.name)
                .cloned()
                .unwrap_or_else(|| opt.default_value());
            if let ThemeValue::Text(hex) = val
                && let Some(norm) = normalize_hex_color(&hex)
                && !swatches.contains(&norm)
            {
                swatches.push(norm);
                if swatches.len() == 3 {
                    break;
                }
            }
        }
        swatches
    }

    /// Parses URL query parameters into pinned theme options.
    ///
    /// Returns `None` when no valid `preset` or manifest option parameter is
    /// present (so the overlay falls back to the profile's live saved options).
    /// When `Some`, values start from the theme's defaults, apply `preset` (if
    /// matched case-insensitively), and then apply individual option overrides
    /// in order.
    pub fn parse_query_options(&self, params: &[(String, String)]) -> Option<UrlThemeOptions> {
        let mut matched_any = false;
        let mut values = ThemeValues::new();
        let mut matched_preset: Option<String> = None;
        let mut first_matched_pair: Option<(String, String)> = None;

        // Apply preset first (last `preset=` wins if repeated).
        if let Some((_, preset_name)) = params.iter().rev().find(|(k, _)| k == "preset") {
            let trimmed = preset_name.trim();
            if let Some(preset) = self
                .presets
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(trimmed))
            {
                matched_any = true;
                matched_preset = Some(preset.name.clone());
                for (name, val) in &preset.values {
                    if let Some(opt) = self.options.iter().find(|o| o.name == *name) {
                        if *val == opt.default_value() {
                            values.remove(name);
                        } else {
                            values.insert(name.clone(), val.clone());
                        }
                    }
                }
            }
        }

        // Apply individual option overrides on top of any preset.
        for (key, raw) in params {
            if matches!(
                key.as_str(),
                "theme" | "hide" | "newest" | "demo" | "preset"
            ) {
                continue;
            }
            let Some(opt) = self.options.iter().find(|o| o.name == *key) else {
                continue;
            };
            let Some(parsed) = opt.parse_query_value(raw) else {
                continue;
            };
            matched_any = true;
            if first_matched_pair.is_none() {
                first_matched_pair = Some((opt.name.clone(), opt.format_query_value(&parsed)));
            }
            if parsed == opt.default_value() {
                values.remove(&opt.name);
            } else {
                values.insert(opt.name.clone(), parsed);
            }
        }

        if !matched_any {
            return None;
        }

        let mut encoded_pairs = self.encode_query_options(&values);
        // If the URL explicitly pinned everything back to defaults (e.g.
        // `?preset=Classic` or `?max_messages=20`), keep a marker pair so
        // `/overlay.css?...` also knows to serve defaults rather than profile
        // overrides.
        if encoded_pairs.is_empty() {
            if let Some(name) = matched_preset {
                encoded_pairs.push(("preset".into(), name));
            } else if let Some(pair) = first_matched_pair {
                encoded_pairs.push(pair);
            }
        }

        Some(UrlThemeOptions {
            values,
            css_query: encode_query_string(&encoded_pairs),
        })
    }

    /// Encodes `values` into the shortest list of URL query `(key, value)`
    /// pairs that round-trips back to the exact same options via
    /// [`Self::parse_query_options`].
    ///
    /// Returns an empty vector when all options are at their defaults.
    /// Automatically uses `preset=<Name>` plus only the differing options when
    /// that produces a shorter URL than listing every non-default option.
    pub fn encode_query_options(&self, values: &ThemeValues) -> Vec<(String, String)> {
        let mut target = ThemeValues::new();
        for opt in &self.options {
            if let Some(norm) = values.get(&opt.name).and_then(|v| opt.normalize(v))
                && norm != opt.default_value()
            {
                target.insert(opt.name.clone(), norm);
            }
        }
        if target.is_empty() {
            return Vec::new();
        }

        // Candidate 0: direct list of non-default options.
        let mut best: Vec<(String, String)> = self
            .options
            .iter()
            .filter_map(|opt| {
                let val = target.get(&opt.name)?;
                Some((opt.name.clone(), opt.format_query_value(val)))
            })
            .collect();
        let mut best_len = encode_query_string(&best).len();

        // Candidates 1..N: each preset plus only the delta from that preset.
        for preset in &self.presets {
            let mut candidate = vec![("preset".to_string(), preset.name.clone())];
            for opt in &self.options {
                let want = target
                    .get(&opt.name)
                    .cloned()
                    .unwrap_or_else(|| opt.default_value());
                let have = preset
                    .values
                    .get(&opt.name)
                    .cloned()
                    .unwrap_or_else(|| opt.default_value());
                if want != have {
                    candidate.push((opt.name.clone(), opt.format_query_value(&want)));
                }
            }
            let len = encode_query_string(&candidate).len();
            if len < best_len {
                best = candidate;
                best_len = len;
            }
        }

        best
    }

    fn validate(&mut self) -> anyhow::Result<()> {
        let mut seen = BTreeSet::new();
        for opt in &mut self.options {
            if !is_valid_option_name(&opt.name) {
                bail!(
                    "invalid option name {:?}: use 1–40 ASCII letters, digits, '-' or '_', starting with a letter",
                    opt.name
                );
            }
            if !seen.insert(opt.name.clone()) {
                bail!("duplicate option name {:?}", opt.name);
            }
            if opt.label.trim().is_empty() {
                bail!("option {:?} needs a non-empty label", opt.name);
            }

            // Validate targets.
            if let Some(var) = &opt.target.css_var
                && !is_valid_css_var(var)
            {
                bail!(
                    "option {:?}: invalid css_var {:?} (expected --lowercase-name)",
                    opt.name,
                    var
                );
            }
            for (k, val) in &opt.target.css_values {
                if !is_safe_css_value(val) {
                    bail!(
                        "option {:?}: css_values[{k:?}] contains forbidden characters",
                        opt.name
                    );
                }
            }
            for (field, cls) in [
                ("class", opt.target.class.as_deref()),
                ("class_when_false", opt.target.class_when_false.as_deref()),
            ] {
                if let Some(cls) = cls
                    && !is_valid_class(cls)
                {
                    bail!("option {:?}: invalid {field} {cls:?}", opt.name);
                }
            }
            for (k, cls) in &opt.target.class_values {
                if !cls.is_empty() && !is_valid_class(cls) {
                    bail!("option {:?}: invalid class_values[{k:?}] {cls:?}", opt.name);
                }
            }

            let has_target = opt.target.css_var.is_some()
                || opt.target.class.is_some()
                || opt.target.class_when_false.is_some()
                || opt.target.class_values.values().any(|c| !c.is_empty());
            if !has_target {
                bail!(
                    "option {:?} must set css_var, class, class_when_false, or class_values",
                    opt.name
                );
            }

            // Validate kind-specific constraints.
            let is_font = matches!(opt.kind, OptionKind::Font { .. });
            match &mut opt.kind {
                OptionKind::Color { default } => {
                    let Some(norm) = normalize_hex_color(default) else {
                        bail!(
                            "option {:?}: color default {default:?} must be #rgb or #rrggbb",
                            opt.name
                        );
                    };
                    *default = norm;
                    if opt.target.css_var.is_none() {
                        bail!("color option {:?} must set css_var", opt.name);
                    }
                }
                OptionKind::Number {
                    default,
                    min,
                    max,
                    step,
                    unit,
                    css_unit,
                } => {
                    if !default.is_finite() || !min.is_finite() || !max.is_finite() {
                        bail!("number option {:?} must use finite bounds", opt.name);
                    }
                    if *min >= *max {
                        bail!("number option {:?}: min must be less than max", opt.name);
                    }
                    if !(*min..=*max).contains(default) {
                        bail!(
                            "number option {:?}: default {default} is outside {min}..={max}",
                            opt.name
                        );
                    }
                    if let Some(s) = step
                        && (!s.is_finite() || *s <= 0.0)
                    {
                        bail!("number option {:?}: step must be positive", opt.name);
                    }
                    for (field, u) in [("unit", unit.as_deref()), ("css_unit", css_unit.as_deref())]
                    {
                        if let Some(u) = u
                            && !is_safe_css_unit(u)
                        {
                            bail!("number option {:?}: invalid {field} {u:?}", opt.name);
                        }
                    }
                    if opt.target.css_var.is_none() {
                        bail!("number option {:?} must set css_var", opt.name);
                    }
                }
                OptionKind::Bool { .. } => {
                    if opt.target.css_var.is_some()
                        && (!opt.target.css_values.contains_key("true")
                            || !opt.target.css_values.contains_key("false"))
                    {
                        bail!(
                            "bool option {:?} with css_var must define css_values for \"true\" and \"false\"",
                            opt.name
                        );
                    }
                }
                OptionKind::Choice { default, options } | OptionKind::Font { default, options } => {
                    if is_font && opt.target.css_var.is_none() {
                        bail!("font option {:?} must set css_var", opt.name);
                    }
                    let min_options = if is_font { 1 } else { 2 };
                    if options.len() < min_options {
                        bail!(
                            "option {:?} must have at least {min_options} option(s)",
                            opt.name
                        );
                    }
                    let mut choices = BTreeSet::new();
                    for item in options.iter() {
                        if item.trim().is_empty() {
                            bail!("option {:?} has an empty choice", opt.name);
                        }
                        if is_font
                            && (item.starts_with(fonts::SYSTEM_FONT_PREFIX)
                                || item.starts_with(fonts::FILE_FONT_PREFIX))
                        {
                            bail!(
                                "font option {:?}: curated choice {item:?} cannot use reserved prefix",
                                opt.name
                            );
                        }
                        if !choices.insert(item.as_str()) {
                            bail!("option {:?} has duplicate choice {item:?}", opt.name);
                        }
                    }
                    if !choices.contains(default.as_str()) {
                        bail!(
                            "option {:?}: default {default:?} is not in options",
                            opt.name
                        );
                    }
                    if opt.target.css_var.is_some() {
                        for item in options.iter() {
                            if !opt.target.css_values.contains_key(item) {
                                bail!(
                                    "option {:?}: css_values is missing entry for {item:?}",
                                    opt.name
                                );
                            }
                        }
                    }
                    for key in opt.target.css_values.keys() {
                        if !choices.contains(key.as_str()) {
                            bail!(
                                "option {:?}: css_values has unknown choice {key:?}",
                                opt.name
                            );
                        }
                    }
                    for key in opt.target.class_values.keys() {
                        if !choices.contains(key.as_str()) {
                            bail!(
                                "option {:?}: class_values has unknown choice {key:?}",
                                opt.name
                            );
                        }
                    }
                }
            }
        }

        // Validate presets against the declared options.
        let mut seen_presets = BTreeSet::new();
        for preset in &mut self.presets {
            let trimmed = preset.name.trim();
            if trimmed.is_empty() || trimmed.chars().count() > 40 {
                bail!("preset name {:?} must be 1–40 characters", preset.name);
            }
            if !seen_presets.insert(trimmed.to_lowercase()) {
                bail!("duplicate preset name {:?}", preset.name);
            }
            if preset.values.is_empty() {
                bail!("preset {:?} must set at least one option", preset.name);
            }
            let mut normalized = ThemeValues::new();
            for (opt_name, val) in &preset.values {
                let Some(opt) = self.options.iter().find(|o| o.name == *opt_name) else {
                    bail!(
                        "preset {:?} references unknown option {opt_name:?}",
                        preset.name
                    );
                };
                let Some(norm) = opt.normalize(val) else {
                    bail!(
                        "preset {:?} has invalid value for option {opt_name:?}",
                        preset.name
                    );
                };
                normalized.insert(opt_name.clone(), norm);
            }
            preset.values = normalized;
        }
        Ok(())
    }
}

/// Percent-encodes one URL query key or value (UTF-8, keeping unreserved
/// ASCII characters `A-Za-z0-9-_.~` unescaped).
pub fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Encodes `(key, value)` pairs into a `k1=v1&k2=v2` query string.
pub fn encode_query_string(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&encode_query_value(k));
        out.push('=');
        out.push_str(&encode_query_value(v));
    }
    out
}

/// Parses a raw URL query string (`k1=v1&k2=v2`, without leading `?`) into
/// decoded `(key, value)` pairs.
pub fn parse_raw_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (decode_query_component(k), decode_query_component(v)),
            None => (decode_query_component(part), String::new()),
        })
        .collect()
}

fn decode_query_component(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Looks up a theme's saved option values by its canonical key, tolerating
/// hand-edited capitalisation for built-in theme names (e.g. `Minimal`).
pub fn lookup_theme_values<'a>(
    all: &'a BTreeMap<String, ThemeValues>,
    key: &str,
) -> Option<&'a ThemeValues> {
    all.get(key).or_else(|| {
        Builtin::from_name(key).and_then(|builtin| {
            all.iter()
                .find(|(k, _)| Builtin::from_name(k) == Some(builtin))
                .map(|(_, v)| v)
        })
    })
}

/// Parses `#rgb` or `#rrggbb` (case-insensitive) and returns lowercase `#rrggbb`.
pub fn normalize_hex_color(input: &str) -> Option<String> {
    let hex = input.trim().strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        3 => {
            let b = hex.as_bytes();
            Some(
                format!(
                    "#{}{}{}{}{}{}",
                    b[0] as char,
                    b[0] as char,
                    b[1] as char,
                    b[1] as char,
                    b[2] as char,
                    b[2] as char
                )
                .to_ascii_lowercase(),
            )
        }
        6 => Some(format!("#{}", hex.to_ascii_lowercase())),
        _ => None,
    }
}

fn format_number(n: f64) -> String {
    if n == 0.0 {
        "0".to_string()
    } else {
        n.to_string()
    }
}

fn is_valid_option_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && name.len() <= 40
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn is_valid_css_var(var: &str) -> bool {
    let Some(rest) = var.strip_prefix("--") else {
        return false;
    };
    !rest.is_empty()
        && var.len() <= 64
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_valid_class(cls: &str) -> bool {
    if matches!(cls, "chat" | "chat--newest-top") {
        return false;
    }
    let mut bytes = cls.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && cls.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_safe_css_value(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 256
        && !trimmed.contains(['{', '}', ';', '\r', '\n'])
        && !trimmed.contains("/*")
        && !trimmed.contains("*/")
}

fn is_safe_css_unit(unit: &str) -> bool {
    unit.len() <= 16 && unit.bytes().all(|b| b.is_ascii_alphabetic() || b == b'%')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifests_parse_and_resolve_cleanly() {
        for builtin in Builtin::ALL {
            let manifest = ThemeManifest::parse(builtin.manifest_toml())
                .unwrap_or_else(|e| panic!("{builtin:?} manifest invalid: {e:#}"));
            assert!(
                !manifest.options.is_empty(),
                "{builtin:?} should expose customization options"
            );
            // Uncustomized overrides are empty so raw CSS stays unchanged.
            let overrides = manifest.resolve_overrides(None);
            assert!(overrides.is_empty(), "{builtin:?}: {overrides:?}");
            // Full resolve always populates defaults cleanly.
            let full = manifest.resolve(None);
            assert!(!full.vars.is_empty(), "{builtin:?} should resolve CSS vars");
            assert!(
                !manifest.presets.is_empty(),
                "{builtin:?} should declare curated presets"
            );
            for preset in &manifest.presets {
                assert!(
                    !manifest.preset_swatches(preset).is_empty(),
                    "{builtin:?} preset {:?} should produce swatch colors",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn preset_validation_rejects_unknown_options_and_matches_current_values() {
        let manifest = ThemeManifest::parse(Builtin::Default.manifest_toml()).unwrap();
        let sakura = manifest
            .presets
            .iter()
            .find(|p| p.name == "Sakura")
            .expect("Sakura preset");
        assert!(!manifest.matches_preset(sakura, &ThemeValues::new()));
        assert!(manifest.matches_preset(sakura, &sakura.values));

        // Unknown option in a preset fails parse.
        let bad = r##"
[[options]]
name = "text"
label = "Text"
type = "color"
default = "#ffffff"
css_var = "--chat-text"

[[presets]]
name = "Broken"
[presets.values]
nonexistent = "#000000"
"##;
        assert!(ThemeManifest::parse(bad).is_err());
    }

    #[test]
    fn hex_normalization_handles_short_and_upper() {
        assert_eq!(normalize_hex_color("#AbC"), Some("#aabbcc".into()));
        assert_eq!(normalize_hex_color(" #9146FF "), Some("#9146ff".into()));
        assert_eq!(normalize_hex_color("9146ff"), None);
        assert_eq!(normalize_hex_color("#12345"), None);
        assert_eq!(normalize_hex_color("#gggggg"), None);
        assert_eq!(normalize_hex_color("red; color: blue"), None);
    }

    #[test]
    fn resolve_applies_overrides_and_ignores_invalid_values() {
        let toml = r##"
[[options]]
name = "accent"
label = "Accent"
type = "color"
default = "#9146ff"
css_var = "--chat-accent"

[[options]]
name = "size"
label = "Size"
type = "number"
default = 20
min = 12
max = 40
unit = "px"
css_var = "--chat-font-size"

[[options]]
name = "show_avatars"
label = "Avatars"
type = "bool"
default = true
class_when_false = "chat--hide-avatars"

[[options]]
name = "corners"
label = "Corners"
type = "choice"
default = "Rounded"
options = ["Sharp", "Rounded", "Pill"]
css_var = "--chat-radius"
[options.css_values]
Sharp = "4px"
Rounded = "12px"
Pill = "20px"
"##;
        let manifest = ThemeManifest::parse(toml).unwrap();
        let mut values = ThemeValues::new();
        values.insert("accent".into(), ThemeValue::Text("#FF00AA".into()));
        values.insert("size".into(), ThemeValue::Number(999.0)); // out of range -> ignored
        values.insert("show_avatars".into(), ThemeValue::Bool(false));
        values.insert("corners".into(), ThemeValue::Text("Pill".into()));

        let overrides = manifest.resolve_overrides(Some(&values));
        assert_eq!(overrides.vars.get("--chat-accent").unwrap(), "#ff00aa");
        assert_eq!(overrides.vars.get("--chat-radius").unwrap(), "20px");
        assert!(
            !overrides.vars.contains_key("--chat-font-size"),
            "invalid number must be ignored"
        );
        assert_eq!(overrides.classes, vec!["chat--hide-avatars"]);

        let css = overrides.to_css();
        assert!(css.contains("--chat-accent: #ff00aa;"), "{css}");
        assert!(css.contains("--chat-radius: 20px;"), "{css}");

        // Full resolve fills the invalid size with its default.
        let full = manifest.resolve(Some(&values));
        assert_eq!(full.vars.get("--chat-font-size").unwrap(), "20px");
    }

    #[test]
    fn rejects_unsafe_css_vars_classes_and_values() {
        // Bad css_var
        assert!(
            ThemeManifest::parse(
                r##"
[[options]]
name = "a"
label = "A"
type = "color"
default = "#ffffff"
css_var = "color"
"##
            )
            .is_err()
        );
        // Reserved class name
        assert!(
            ThemeManifest::parse(
                r#"
[[options]]
name = "a"
label = "A"
type = "bool"
default = false
class = "chat--newest-top"
"#
            )
            .is_err()
        );
        // Injection in css_values
        assert!(
            ThemeManifest::parse(
                r#"
[[options]]
name = "a"
label = "A"
type = "choice"
default = "One"
options = ["One", "Two"]
css_var = "--x"
[options.css_values]
One = "1px; } body { display: none"
Two = "2px"
"#
            )
            .is_err()
        );
    }

    #[test]
    fn url_query_options_parse_and_encode_compactly() {
        let manifest = ThemeManifest::parse(Builtin::Default.manifest_toml()).unwrap();

        // Reserved and unknown parameters alone return None (use live profile options).
        let plain = parse_raw_query("theme=default&newest=top&hide=notice&bogus=123");
        assert!(manifest.parse_query_options(&plain).is_none());

        // Preset alone resolves all preset options cleanly.
        let sakura_params = parse_raw_query("theme=default&preset=Sakura");
        let parsed = manifest
            .parse_query_options(&sakura_params)
            .expect("Sakura preset should parse");
        assert_eq!(
            parsed.values.get("accent_color"),
            Some(&ThemeValue::Text("#ff85b3".into()))
        );
        assert_eq!(
            parsed.values.get("accent_mode"),
            Some(&ThemeValue::Text("Custom".into()))
        );
        assert_eq!(parsed.css_query, "preset=Sakura");

        // Preset + override (including overriding a preset option back to default
        // and bare hex colors without `#`).
        let tweaked = parse_raw_query(
            "preset=sakura&accent_color=00ccff&accent_mode=Platform&max_messages=10&show_avatars=0",
        );
        let parsed = manifest
            .parse_query_options(&tweaked)
            .expect("preset + overrides should parse");
        assert_eq!(
            parsed.values.get("accent_color"),
            Some(&ThemeValue::Text("#00ccff".into()))
        );
        assert!(
            !parsed.values.contains_key("accent_mode"),
            "overriding back to default removes the override"
        );
        assert_eq!(
            parsed.values.get("max_messages"),
            Some(&ThemeValue::Number(10.0))
        );
        assert_eq!(
            parsed.values.get("show_avatars"),
            Some(&ThemeValue::Bool(false))
        );

        // Encoding chooses `preset=` + delta when shorter than listing all options.
        let sakura = manifest
            .presets
            .iter()
            .find(|p| p.name == "Sakura")
            .unwrap();
        let mut with_tweak = sakura.values.clone();
        with_tweak.insert("max_messages".into(), ThemeValue::Number(10.0));
        let encoded = manifest.encode_query_options(&with_tweak);
        assert_eq!(
            encoded,
            vec![
                ("preset".into(), "Sakura".into()),
                ("max_messages".into(), "10".into()),
            ]
        );

        // Round-trip every built-in preset through encode -> query -> parse.
        for builtin in Builtin::ALL {
            let m = builtin.manifest();
            for preset in &m.presets {
                let pairs = m.encode_query_options(&preset.values);
                let query = encode_query_string(&pairs);
                if pairs.is_empty() {
                    // Preset equals all defaults: explicit `?preset=<name>` still pins defaults.
                    let explicit = parse_raw_query(&encode_query_string(&[(
                        "preset".into(),
                        preset.name.clone(),
                    )]));
                    let got = m.parse_query_options(&explicit).expect("explicit preset");
                    assert_eq!(
                        m.resolve(Some(&got.values)),
                        m.resolve(Some(&preset.values))
                    );
                    assert!(
                        !got.css_query.is_empty(),
                        "default-matching preset must still forward css_query"
                    );
                } else {
                    let reparsed = parse_raw_query(&query);
                    let got = m.parse_query_options(&reparsed).expect("round-trip");
                    assert_eq!(
                        m.resolve(Some(&got.values)),
                        m.resolve(Some(&preset.values)),
                        "{builtin:?} preset {:?}",
                        preset.name
                    );
                }
            }
        }
    }

    #[test]
    fn font_option_supports_curated_system_and_imported_files() {
        let toml = r##"
[[options]]
name = "font"
label = "Font"
type = "font"
default = "Sans"
options = ["Sans", "Serif"]
css_var = "--chat-font"
[options.css_values]
Sans = '"Inter", system-ui, sans-serif'
Serif = '"Georgia", serif'
"##;
        let manifest = ThemeManifest::parse(toml).unwrap();

        // System font resolves to quoted CSS family and round-trips through URL query.
        let mut sys = ThemeValues::new();
        sys.insert(
            "font".into(),
            ThemeValue::Text("system:  Fira Sans ".into()),
        );
        let resolved = manifest.resolve_overrides(Some(&sys));
        assert_eq!(
            resolved.vars.get("--chat-font").unwrap(),
            r#""Fira Sans", system-ui, sans-serif"#
        );
        assert!(resolved.fonts.is_empty());
        let encoded = encode_query_string(&manifest.encode_query_options(&sys));
        let reparsed = manifest
            .parse_query_options(&parse_raw_query(&encoded))
            .unwrap();
        assert_eq!(
            reparsed.values.get("font"),
            Some(&ThemeValue::Text("system:Fira Sans".into()))
        );

        // Imported font emits @font-face with proper base path and round-trips through URL query.
        let mut imported = ThemeValues::new();
        imported.insert(
            "font".into(),
            ThemeValue::Text("file:My Comic.woff2".into()),
        );
        let resolved = manifest.resolve_overrides(Some(&imported));
        assert_eq!(
            resolved.vars.get("--chat-font").unwrap(),
            r#""chat-font-My Comic.woff2", system-ui, sans-serif"#
        );
        assert_eq!(
            resolved.fonts,
            vec![ResolvedFontFace {
                family: "chat-font-My Comic.woff2".into(),
                file: "My Comic.woff2".into(),
            }]
        );
        let css = resolved.to_css_with_font_base("../../fonts/");
        assert!(
            css.contains(r#"font-family: "chat-font-My Comic.woff2";"#),
            "{css}"
        );
        assert!(
            css.contains(r#"src: url("../../fonts/My%20Comic.woff2");"#),
            "{css}"
        );
        let encoded = encode_query_string(&manifest.encode_query_options(&imported));
        let reparsed = manifest
            .parse_query_options(&parse_raw_query(&encoded))
            .unwrap();
        assert_eq!(
            reparsed.values.get("font"),
            Some(&ThemeValue::Text("file:My Comic.woff2".into()))
        );

        // Unsafe system font names and path-traversal filenames are ignored.
        let mut unsafe_vals = ThemeValues::new();
        unsafe_vals.insert(
            "font".into(),
            ThemeValue::Text("system:Bad\"; } body { display: none".into()),
        );
        assert!(manifest.resolve_overrides(Some(&unsafe_vals)).is_empty());
        unsafe_vals.insert("font".into(), ThemeValue::Text("file:../secret.ttf".into()));
        assert!(manifest.resolve_overrides(Some(&unsafe_vals)).is_empty());
    }
}
