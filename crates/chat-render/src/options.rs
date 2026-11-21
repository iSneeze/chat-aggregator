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

/// Concrete CSS custom properties and `.chat` classes resolved from a theme's
/// manifest and saved values.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct ResolvedStyle {
    /// `--var-name` -> CSS value.
    pub vars: BTreeMap<String, String>,
    /// Modifier classes active on `<main class="chat">`.
    pub classes: Vec<String>,
}

impl ResolvedStyle {
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.classes.is_empty()
    }

    /// Formats `vars` as a `:root { … }` override block suitable for appending
    /// to `overlay.css`. Returns an empty string when there are no variables.
    pub fn to_css(&self) -> String {
        if self.vars.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n/* theme options */\n:root {\n");
        for (var, val) in &self.vars {
            let _ = writeln!(out, "  {var}: {val};");
        }
        out.push_str("}\n");
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
            OptionKind::Choice { default, .. } => ThemeValue::Text(default.clone()),
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
            _ => false,
        }
    }

    /// Normalizes a valid value (e.g. hex color casing) so equality against
    /// [`Self::default_value`] is consistent.
    pub fn normalize(&self, value: &ThemeValue) -> Option<ThemeValue> {
        if !self.accepts(value) {
            return None;
        }
        match (&self.kind, value) {
            (OptionKind::Color { .. }, ThemeValue::Text(s)) => {
                normalize_hex_color(s).map(ThemeValue::Text)
            }
            _ => Some(value.clone()),
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
                OptionKind::Choice { default, options } => {
                    if options.len() < 2 {
                        bail!(
                            "choice option {:?} must have at least two options",
                            opt.name
                        );
                    }
                    let mut choices = BTreeSet::new();
                    for item in options.iter() {
                        if item.trim().is_empty() {
                            bail!("choice option {:?} has an empty choice", opt.name);
                        }
                        if !choices.insert(item.as_str()) {
                            bail!("choice option {:?} has duplicate choice {item:?}", opt.name);
                        }
                    }
                    if !choices.contains(default.as_str()) {
                        bail!(
                            "choice option {:?}: default {default:?} is not in options",
                            opt.name
                        );
                    }
                    if opt.target.css_var.is_some() {
                        for item in options.iter() {
                            if !opt.target.css_values.contains_key(item) {
                                bail!(
                                    "choice option {:?}: css_values is missing entry for {item:?}",
                                    opt.name
                                );
                            }
                        }
                    }
                    for key in opt.target.css_values.keys() {
                        if !choices.contains(key.as_str()) {
                            bail!(
                                "choice option {:?}: css_values has unknown choice {key:?}",
                                opt.name
                            );
                        }
                    }
                    for key in opt.target.class_values.keys() {
                        if !choices.contains(key.as_str()) {
                            bail!(
                                "choice option {:?}: class_values has unknown choice {key:?}",
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
}
