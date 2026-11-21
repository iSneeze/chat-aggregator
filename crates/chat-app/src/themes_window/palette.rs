//! Extracts harmonious, contrast-checked overlay palettes from a scene
//! screenshot or character/brand artwork.
//!
//! Runs off the UI thread (`cx.background_executor()`): opens a PNG/JPEG/WebP,
//! downscales to at most 96×96, clusters colors via a 5-bit RGB histogram, and
//! derives three readable overlay palettes (tinted dark, deep neutral, and
//! soft light) guaranteed to meet WCAG AA contrast (≥ 4.5:1) for chat text.

use std::path::Path;

use anyhow::{Context, bail};
use chat_engine::theme_options::{OptionKind, ThemeManifest, ThemeValue, ThemeValues};
use image::ImageReader;

/// Refuse unexpectedly huge files before decoding so picking a giant asset
/// cannot exhaust memory.
const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

/// One ready-to-apply color palette derived from an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SuggestedPalette {
    pub(super) label: String,
    pub(super) accent: String,
    pub(super) bg: String,
    pub(super) text: String,
}

impl SuggestedPalette {
    pub(super) fn swatches(&self) -> [&str; 3] {
        [&self.bg, &self.accent, &self.text]
    }
}

/// Reads an image from disk and extracts 3 harmonious, readable palettes.
pub(super) fn extract_from_file(path: &Path) -> anyhow::Result<Vec<SuggestedPalette>> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > MAX_IMAGE_BYTES {
        bail!("image is larger than 50 MB");
    }
    let img = ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("detecting format of {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?;
    Ok(extract_from_image(&img))
}

/// Pure extraction from a decoded image (also used directly by unit tests).
pub(super) fn extract_from_image(img: &image::DynamicImage) -> Vec<SuggestedPalette> {
    let small = img.thumbnail(96, 96).to_rgba8();

    // 5-bit per channel histogram (32^3 = 32,768 buckets).
    const BINS: usize = 32;
    let mut counts = vec![0u32; BINS * BINS * BINS];
    let mut sum_r = vec![0u64; BINS * BINS * BINS];
    let mut sum_g = vec![0u64; BINS * BINS * BINS];
    let mut sum_b = vec![0u64; BINS * BINS * BINS];

    for px in small.pixels() {
        if px[3] < 128 {
            continue;
        }
        let idx =
            ((px[0] as usize >> 3) << 10) | ((px[1] as usize >> 3) << 5) | (px[2] as usize >> 3);
        counts[idx] += 1;
        sum_r[idx] += px[0] as u64;
        sum_g[idx] += px[1] as u64;
        sum_b[idx] += px[2] as u64;
    }

    let mut buckets: Vec<(u32, Rgb)> = counts
        .into_iter()
        .enumerate()
        .filter(|(_, c)| *c > 0)
        .map(|(i, c)| {
            let rgb = Rgb::new(
                (sum_r[i] / c as u64) as u8,
                (sum_g[i] / c as u64) as u8,
                (sum_b[i] / c as u64) as u8,
            );
            (c, rgb)
        })
        .collect();

    if buckets.is_empty() {
        return default_fallback();
    }

    buckets.sort_by_key(|a| std::cmp::Reverse(a.0));
    let dominant = buckets[0].1;

    // Pick the most vibrant prominent color for the accent.
    let vibrant = buckets
        .iter()
        .map(|(count, rgb)| {
            let hsl = rgb.to_hsl();
            // Favor mid-lightness, saturated tones with meaningful coverage.
            let lightness_weight = 1.0 - (hsl.l - 0.52).abs() * 1.4;
            let score = hsl.s * lightness_weight.max(0.15) * (*count as f32).sqrt();
            (score, *rgb, hsl)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .filter(|(_, _, hsl)| hsl.s >= 0.12)
        .map(|(_, rgb, _)| rgb)
        .unwrap_or(dominant);

    build_suggestions(dominant, vibrant)
}

fn default_fallback() -> Vec<SuggestedPalette> {
    build_suggestions(Rgb::new(20, 20, 29), Rgb::new(145, 70, 255))
}

fn build_suggestions(dominant: Rgb, vibrant: Rgb) -> Vec<SuggestedPalette> {
    let dom_hsl = dominant.to_hsl();
    let vib_hsl = vibrant.to_hsl();

    // Accent tuned for dark surfaces: keep hue, ensure vividness & legibility.
    let dark_accent = Hsl {
        h: vib_hsl.h,
        s: vib_hsl.s.clamp(0.45, 0.90),
        l: vib_hsl.l.clamp(0.56, 0.72),
    }
    .to_rgb();

    // Accent tuned for light surfaces: deeper so it stays crisp on pale cards.
    let light_accent = Hsl {
        h: vib_hsl.h,
        s: vib_hsl.s.clamp(0.45, 0.85),
        l: vib_hsl.l.clamp(0.36, 0.48),
    }
    .to_rgb();

    // 1. Tinted dark: dark card carrying the scene's dominant hue.
    let tinted_bg = Hsl {
        h: dom_hsl.h,
        s: (dom_hsl.s * 0.45).clamp(0.08, 0.32),
        l: 0.11,
    }
    .to_rgb();
    let tinted_text = ensure_contrast(
        Hsl {
            h: dom_hsl.h,
            s: 0.16,
            l: 0.95,
        }
        .to_rgb(),
        tinted_bg,
        4.5,
    );

    // 2. Deep neutral: clean charcoal card + vibrant scene accent.
    let neutral_bg = Rgb::new(19, 20, 28);
    let neutral_text = ensure_contrast(Rgb::new(244, 244, 248), neutral_bg, 4.5);

    // 3. Soft light: pale tinted card + deep readable ink + scene accent.
    let light_bg = Hsl {
        h: dom_hsl.h,
        s: (dom_hsl.s * 0.35).clamp(0.08, 0.28),
        l: 0.94,
    }
    .to_rgb();
    let light_text = ensure_contrast(
        Hsl {
            h: dom_hsl.h,
            s: 0.25,
            l: 0.16,
        }
        .to_rgb(),
        light_bg,
        4.5,
    );

    vec![
        SuggestedPalette {
            label: "Tinted dark".into(),
            accent: dark_accent.to_hex(),
            bg: tinted_bg.to_hex(),
            text: tinted_text.to_hex(),
        },
        SuggestedPalette {
            label: "Deep neutral".into(),
            accent: dark_accent.to_hex(),
            bg: neutral_bg.to_hex(),
            text: neutral_text.to_hex(),
        },
        SuggestedPalette {
            label: "Soft light".into(),
            accent: light_accent.to_hex(),
            bg: light_bg.to_hex(),
            text: light_text.to_hex(),
        },
    ]
}

/// Maps a [`SuggestedPalette`] onto whichever options `manifest` declares.
pub(super) fn apply_to_manifest(
    manifest: &ThemeManifest,
    palette: &SuggestedPalette,
    current: &ThemeValues,
) -> ThemeValues {
    let mut changes = ThemeValues::new();
    let mut matched_any_color = false;

    for opt in &manifest.options {
        match opt.name.as_str() {
            "accent_mode" => {
                let custom = ThemeValue::Text("Custom".into());
                if opt.accepts(&custom) {
                    changes.insert(opt.name.clone(), custom);
                }
            }
            "accent_color" | "accent" | "viewer_name_color" => {
                if matches!(opt.kind, OptionKind::Color { .. }) {
                    changes.insert(opt.name.clone(), ThemeValue::Text(palette.accent.clone()));
                    matched_any_color = true;
                }
            }
            "bg_color" | "background" | "card_color" => {
                if matches!(opt.kind, OptionKind::Color { .. }) {
                    changes.insert(opt.name.clone(), ThemeValue::Text(palette.bg.clone()));
                    matched_any_color = true;
                }
            }
            "text_color" | "text" => {
                if matches!(opt.kind, OptionKind::Color { .. }) {
                    // For cardless themes (no bg_color option, e.g. Minimal),
                    // keep text light/crisp rather than applying a dark ink.
                    let has_bg = manifest.options.iter().any(|o| {
                        matches!(o.name.as_str(), "bg_color" | "background" | "card_color")
                            && matches!(o.kind, OptionKind::Color { .. })
                    });
                    let color = if has_bg {
                        palette.text.clone()
                    } else {
                        "#ffffff".to_string()
                    };
                    changes.insert(opt.name.clone(), ThemeValue::Text(color));
                    matched_any_color = true;
                }
            }
            "bg_opacity" => {
                // If the card was hidden (e.g. Floating preset at 0%), make it
                // visible again so the chosen card color shows up.
                if let Some(ThemeValue::Number(n)) = current.get(&opt.name)
                    && *n < 20.0
                {
                    changes.insert(opt.name.clone(), opt.default_value());
                }
            }
            _ => {}
        }
    }

    // Fallback for custom themes with arbitrary color option names.
    if !matched_any_color {
        let slots = [&palette.accent, &palette.bg, &palette.text];
        for (opt, hex) in manifest
            .options
            .iter()
            .filter(|o| matches!(o.kind, OptionKind::Color { .. }))
            .zip(slots)
        {
            changes.insert(opt.name.clone(), ThemeValue::Text((*hex).clone()));
        }
    }

    changes
}

#[derive(Debug, Clone, Copy)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

#[derive(Debug, Clone, Copy)]
struct Hsl {
    h: f32, // 0..1
    s: f32, // 0..1
    l: f32, // 0..1
}

impl Rgb {
    fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    fn to_hsl(self) -> Hsl {
        let r = self.r as f32 / 255.0;
        let g = self.g as f32 / 255.0;
        let b = self.b as f32 / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) * 0.5;
        let d = max - min;
        if d < 1e-6 {
            return Hsl { h: 0.0, s: 0.0, l };
        }
        let s = if l > 0.5 {
            d / (2.0 - max - min)
        } else {
            d / (max + min)
        };
        let mut h = if (max - r).abs() < 1e-6 {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if (max - g).abs() < 1e-6 {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        h /= 6.0;
        Hsl { h, s, l }
    }

    fn luminance(self) -> f32 {
        let linear = |c: u8| {
            let v = c as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }
}

impl Hsl {
    fn to_rgb(self) -> Rgb {
        if self.s < 1e-6 {
            let v = (self.l.clamp(0.0, 1.0) * 255.0).round() as u8;
            return Rgb::new(v, v, v);
        }
        let q = if self.l < 0.5 {
            self.l * (1.0 + self.s)
        } else {
            self.l + self.s - self.l * self.s
        };
        let p = 2.0 * self.l - q;
        let hue = |mut t: f32| {
            if t < 0.0 {
                t += 1.0;
            }
            if t > 1.0 {
                t -= 1.0;
            }
            let c = if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 0.5 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            };
            (c.clamp(0.0, 1.0) * 255.0).round() as u8
        };
        Rgb::new(
            hue(self.h + 1.0 / 3.0),
            hue(self.h),
            hue(self.h - 1.0 / 3.0),
        )
    }
}

fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let la = a.luminance();
    let lb = b.luminance();
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn ensure_contrast(fg: Rgb, bg: Rgb, min_ratio: f32) -> Rgb {
    if contrast_ratio(fg, bg) >= min_ratio {
        return fg;
    }
    let white = Rgb::new(255, 255, 255);
    let black = Rgb::new(16, 16, 22);
    if contrast_ratio(white, bg) >= contrast_ratio(black, bg) {
        white
    } else {
        black
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_engine::Builtin;
    use image::{DynamicImage, Rgba, RgbaImage};

    fn parse_rgb(hex: &str) -> Rgb {
        Rgb::new(
            u8::from_str_radix(&hex[1..3], 16).unwrap(),
            u8::from_str_radix(&hex[3..5], 16).unwrap(),
            u8::from_str_radix(&hex[5..7], 16).unwrap(),
        )
    }

    #[test]
    fn extracts_three_wcag_readable_palettes_from_colorful_scene() {
        let mut img = RgbaImage::new(64, 64);
        for (x, _, px) in img.enumerate_pixels_mut() {
            *px = if x < 48 {
                Rgba([30, 42, 74, 255]) // deep navy backdrop
            } else {
                Rgba([255, 92, 168, 255]) // vibrant pink accent
            };
        }
        let palettes = extract_from_image(&DynamicImage::ImageRgba8(img));
        assert_eq!(palettes.len(), 3);
        for p in &palettes {
            let ratio = contrast_ratio(parse_rgb(&p.text), parse_rgb(&p.bg));
            assert!(
                ratio >= 4.5,
                "{}: contrast {ratio:.2} too low ({} on {})",
                p.label,
                p.text,
                p.bg
            );
        }
    }

    #[test]
    fn maps_suggested_palette_onto_builtin_manifests() {
        let palettes = default_fallback();
        let default_manifest = Builtin::Default.manifest();
        let mut current = ThemeValues::new();
        current.insert("bg_opacity".into(), ThemeValue::Number(0.0));
        let applied = apply_to_manifest(default_manifest, &palettes[0], &current);
        assert_eq!(
            applied.get("accent_mode"),
            Some(&ThemeValue::Text("Custom".into()))
        );
        assert!(applied.contains_key("accent_color"));
        assert!(applied.contains_key("bg_color"));
        assert!(applied.contains_key("text_color"));
        assert_eq!(
            applied.get("bg_opacity"),
            Some(&ThemeValue::Number(78.0)),
            "restores card opacity from 0% when applying a card color palette"
        );

        let minimal_manifest = Builtin::Minimal.manifest();
        let applied_min = apply_to_manifest(minimal_manifest, &palettes[2], &ThemeValues::new());
        assert_eq!(
            applied_min.get("text_color"),
            Some(&ThemeValue::Text("#ffffff".into())),
            "cardless Minimal theme keeps light message text"
        );
        assert!(applied_min.contains_key("viewer_name_color"));
    }
}
