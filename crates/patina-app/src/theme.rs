//! Colours: crypto_pro's dark tokens put on the gpui-component `Theme`, plus
//! the product colours the theme has no role for ([`ProductColors`]).
//!
//! This is the only module that may name a raw colour. Views read the
//! semantic roles from `cx.theme()` and the product colours from
//! [`ProductColors::get`]. Dark retains crypto_pro; Light uses neutral surfaces,
//! blue interaction and restrained verdigris accents.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Window, rgb};
use patina_core::loader::LoaderKind;

use crate::settings::{SettingsStore, ThemePreference};

/// crypto_pro's `DARK` tokens (`crates/app/src/appearance.rs`), unchanged.
mod tokens {
    pub const BG: u32 = 0x1a1b24;
    pub const RAISED: u32 = 0x24262f;
    pub const HEADER: u32 = 0x20222c;
    pub const HOVER: u32 = 0x22252e;
    pub const SELECTED: u32 = 0x1f2633;
    pub const BORDER: u32 = 0x2c2e38;
    pub const TEXT: u32 = 0xd1d4dc;
    pub const MUTED: u32 = 0x8e939d;
    pub const ACCENT: u32 = 0x2a99fb;
    pub const TEAL: u32 = 0x1aaaa7;
    pub const AMBER: u32 = 0xf0a020;
    pub const GREEN: u32 = 0x14b39f;
    pub const RED: u32 = 0xff3a76;
    /// Text on an accent fill.
    pub const ON_ACCENT: u32 = 0xffffff;
}

/// Shared Light interactions and brand fill; darker than the initial proposal
/// so Kit's 60%-white inset focus line also clears 3:1 on filled buttons.
mod light_tokens {
    pub const INTERACTION: u32 = 0x0f64b0;
    pub const SIGNATURE: u32 = 0x076e69;
}

/// Loader tag colours that sit next to the tokens.
const NEOFORGE: u32 = 0xf27a45;
const FORGE: u32 = 0x7d93c4;

/// Instance icon-tile tints.
const TINTS: [u32; 8] = [
    0x1aaaa7, 0xf27a45, 0x7d93c4, 0x2a99fb, 0xe8a317, 0x14b39f, 0xc678dd, 0xff5c8a,
];

fn c(v: u32) -> Hsla {
    Hsla::from(rgb(v))
}

/// Patina's window background (the `BG` token), for checks that [`apply`]
/// put the tokens on the theme.
pub fn background() -> Hsla {
    c(tokens::BG)
}

/// Product colours outside the theme's roles (a gpui `Global`, set by
/// [`apply`]).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ProductColors {
    signature: Hsla,
    signature_hover: Hsla,
    signature_active: Hsla,
    link_focus_background: Hsla,
    link_focus_foreground: Hsla,
    vanilla: Hsla,
    fabric: Hsla,
    forge: Hsla,
    neoforge: Hsla,
    tints: [Hsla; 8],
}

impl Global for ProductColors {}

impl ProductColors {
    fn dark() -> Self {
        Self {
            signature: c(tokens::TEAL),
            signature_hover: c(tokens::TEAL).opacity(0.9),
            signature_active: c(tokens::TEAL).opacity(0.8),
            link_focus_background: c(tokens::BORDER),
            link_focus_foreground: c(tokens::ACCENT),
            vanilla: c(tokens::ACCENT),
            fabric: c(tokens::TEAL),
            forge: c(FORGE),
            neoforge: c(NEOFORGE),
            tints: TINTS.map(c),
        }
    }

    fn light() -> Self {
        Self {
            signature: c(light_tokens::SIGNATURE),
            signature_hover: c(0x065e5a),
            signature_active: c(0x054f4b),
            link_focus_background: c(light_tokens::INTERACTION),
            link_focus_foreground: c(0xffffff),
            vanilla: c(0x0b578f),
            fabric: c(0x066660),
            forge: c(0x435882),
            neoforge: c(0x9a3c15),
            tints: [
                0x066660, 0x9a3c15, 0x435882, 0x0b578f, 0x775000, 0x146139, 0x73428a, 0xa32a50,
            ]
            .map(c),
        }
    }

    pub fn get(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// Patina's teal: the launch button, the selected tab's underline and
    /// progress lines.
    pub fn signature(&self) -> Hsla {
        self.signature
    }

    /// Launch-action states are opaque in Light, keeping white labels readable.
    pub fn signature_hover(&self) -> Hsla {
        self.signature_hover
    }

    pub fn signature_active(&self) -> Hsla {
        self.signature_active
    }

    /// Colors of application-owned external-link keyboard focus.
    pub fn link_focus(&self) -> (Hsla, Hsla) {
        (self.link_focus_background, self.link_focus_foreground)
    }

    /// Tag colour of an instance's loader; `None` is vanilla Minecraft.
    pub fn loader(&self, kind: Option<LoaderKind>) -> Hsla {
        match kind {
            None => self.vanilla,
            Some(LoaderKind::Fabric) => self.fabric,
            Some(LoaderKind::Forge) => self.forge,
            Some(LoaderKind::Neoforge) => self.neoforge,
        }
    }

    /// Icon-tile tint `i` (wraps).
    pub fn tint(&self, i: usize) -> Hsla {
        self.tints[i % self.tints.len()]
    }
}

/// The tokens on gpui-component's semantic roles (crypto_pro's
/// `appearance::theme_colors`, plus the status and palette roles).
fn theme_colors(t: &mut Theme) {
    use tokens::*;
    let (bg, raised, header, hover, selected) =
        (c(BG), c(RAISED), c(HEADER), c(HOVER), c(SELECTED));
    let (border, text, muted, accent) = (c(BORDER), c(TEXT), c(MUTED), c(ACCENT));
    let (teal, amber, green, red) = (c(TEAL), c(AMBER), c(GREEN), c(RED));

    t.background = bg;
    t.foreground = text;
    t.muted = raised;
    t.muted_foreground = muted;
    t.border = border;
    t.input = border;
    t.ring = accent;
    t.caret = accent;
    t.selection = accent.opacity(0.3);
    t.title_bar = header;
    t.title_bar_border = border;
    t.status_bar = header;
    t.status_bar_border = border;
    t.sidebar = header;
    t.sidebar_border = border;
    t.sidebar_foreground = text;
    t.list_hover = hover;
    t.list_active = selected;
    t.list_active_border = accent;
    t.list_head = header;
    t.primary = accent;
    t.primary_hover = accent.opacity(0.85);
    t.primary_active = accent.opacity(0.75);
    t.primary_foreground = c(ON_ACCENT);
    t.accent = raised;
    t.accent_foreground = text;
    t.secondary = raised;
    t.secondary_hover = hover;
    t.secondary_active = selected;
    t.secondary_foreground = text;
    t.popover = raised;
    t.popover_foreground = text;
    t.tab_bar = bg;
    t.tab = bg;
    t.tab_active = bg;
    t.tab_active_foreground = text;
    t.tab_foreground = muted;
    t.link = accent;
    t.progress_bar = teal;
    t.success = green;
    t.warning = amber;
    t.danger = red;
    t.info = accent;
    t.blue = accent;
    t.cyan = teal;
    t.green = green;
    t.red = red;
    t.yellow = amber;
}

/// Patina Light roles, not colors prescribed by Fluent or WCAG. Those guides
/// inform role separation and contrast; the contrast tests check final pairs.
fn light_colors(t: &mut Theme) {
    let bg = c(0xf7f8fa);
    let surface = c(0xffffff);
    let header = c(0xf0f2f5);
    let hover = c(0xedf0f3);
    let selected = c(0xe3eff9);
    let text = c(0x242424);
    let muted = c(0x61666f);
    let border = c(0xd5d9df); // Decorative boundaries, not control outlines.
    let input = c(0x7a808a);
    let blue = c(light_tokens::INTERACTION);
    let blue_hover = c(0x0d5da5);
    let blue_active = c(0x0b4f8c);
    let teal = c(light_tokens::SIGNATURE);
    let red = c(0xb42318);
    let red_hover = c(0x9c1e15);
    let red_active = c(0x851a12);
    let amber = c(0x8a5700);
    let green = c(0x187246);

    t.background = bg;
    t.foreground = text;
    t.muted = header;
    t.muted_foreground = muted;
    t.border = border;
    t.input = input;
    t.ring = blue;
    // Kit's outward ring is always 50% alpha. Use its solid focused border /
    // inset fallback so focus remains >=3:1 and is not clipped by our panels.
    t.focus_ring = false;
    t.caret = blue;
    t.selection = selected;
    t.title_bar = header;
    t.title_bar_border = border;
    t.status_bar = header;
    t.status_bar_border = border;
    t.sidebar = header;
    t.sidebar_foreground = text;
    t.sidebar_border = border;
    t.sidebar_accent = selected;
    t.sidebar_accent_foreground = blue;
    t.sidebar_primary = blue;
    t.sidebar_primary_foreground = surface;
    t.colors.list = surface;
    t.list_even = bg;
    t.list_head = header;
    t.list_hover = hover;
    t.list_active = selected;
    t.list_active_border = blue;
    t.primary = blue;
    t.primary_hover = blue_hover;
    t.primary_active = blue_active;
    t.primary_foreground = surface;
    t.accent = hover;
    t.accent_foreground = text;
    t.secondary = surface;
    t.secondary_hover = hover;
    t.secondary_active = selected;
    t.secondary_foreground = text;
    t.button = surface;
    t.button_hover = hover;
    t.button_active = selected;
    t.button_foreground = text;
    t.button_primary = blue;
    t.button_primary_hover = blue_hover;
    t.button_primary_active = blue_active;
    t.button_primary_foreground = surface;
    t.button_secondary = surface;
    t.button_secondary_hover = hover;
    t.button_secondary_active = selected;
    t.button_secondary_foreground = text;
    t.popover = surface;
    t.popover_foreground = text;
    t.group_box = surface;
    t.group_box_foreground = text;
    t.accordion = surface;
    t.description_list_label = header;
    t.description_list_label_foreground = muted;
    t.tab_bar = bg;
    t.tab_bar_segmented = header;
    t.tab = bg;
    t.tab_active = surface;
    t.tab_active_foreground = text;
    t.tab_foreground = muted;
    t.link = blue;
    t.link_hover = blue_hover;
    t.link_active = blue_active;
    t.progress_bar = teal;
    t.danger = red;
    t.danger_hover = red_hover;
    t.danger_active = red_active;
    t.danger_foreground = surface;
    t.button_danger = red;
    t.button_danger_hover = red_hover;
    t.button_danger_active = red_active;
    t.button_danger_foreground = surface;
    t.info = blue;
    t.info_hover = blue_hover;
    t.info_active = blue_active;
    t.info_foreground = surface;
    t.button_info = blue;
    t.button_info_hover = blue_hover;
    t.button_info_active = blue_active;
    t.button_info_foreground = surface;
    t.warning = amber;
    t.warning_hover = c(0x754a00);
    t.warning_active = c(0x603d00);
    t.warning_foreground = surface;
    t.button_warning = amber;
    t.button_warning_hover = t.warning_hover;
    t.button_warning_active = t.warning_active;
    t.button_warning_foreground = surface;
    t.success = green;
    t.success_hover = c(0x145f3a);
    t.success_active = c(0x104e30);
    t.success_foreground = surface;
    t.button_success = green;
    t.button_success_hover = t.success_hover;
    t.button_success_active = t.success_active;
    t.button_success_foreground = surface;
    t.switch = input;
    t.switch_thumb = surface;
    t.slider_bar = input;
    t.slider_thumb = surface;
    t.scrollbar_thumb = input;
    t.scrollbar_thumb_hover = muted;
    t.drag_border = blue;
    t.drop_target = selected;
    t.table = surface;
    t.table_head = header;
    t.table_head_foreground = muted;
    t.table_foot = header;
    t.table_foot_foreground = muted;
    t.table_even = bg;
    t.table_hover = hover;
    t.table_active = selected;
    t.table_active_border = blue;
    t.table_row_border = border;
    t.blue = blue;
    t.cyan = teal;
    t.green = green;
    t.red = red;
    t.yellow = amber;
}

/// Applies the saved manual theme before a window's first frame. A theme reload
/// may carry font settings: keep current fonts and sizes, including UI zoom.
/// Call `scale::apply_scale` afterwards when opening the main window.
pub fn apply(window: &mut Window, cx: &mut App) {
    let preference = if cx.has_global::<SettingsStore>() {
        SettingsStore::get(cx).theme
    } else {
        ThemePreference::default()
    };
    let before = Theme::global(cx);
    let fonts = (
        before.font_size,
        before.mono_font_size,
        before.font_family.clone(),
        before.mono_font_family.clone(),
    );
    let mode = match preference {
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::Light => ThemeMode::Light,
    };
    Theme::change(mode, Some(window), cx);
    Theme::update(cx, |t| {
        match preference {
            ThemePreference::Dark => {
                theme_colors(t);
                t.focus_ring = true;
            }
            ThemePreference::Light => light_colors(t),
        }
        (
            t.font_size,
            t.mono_font_size,
            t.font_family,
            t.mono_font_family,
        ) = fonts;
    });
    cx.set_global(match preference {
        ThemePreference::Dark => ProductColors::dark(),
        ThemePreference::Light => ProductColors::light(),
    });
    window.refresh();
}

/// Immediate appearance edits share the settings saver with language and zoom.
pub fn set_theme(preference: ThemePreference, window: &mut Window, cx: &mut App) {
    if SettingsStore::get(cx).theme == preference {
        return;
    }
    SettingsStore::update(cx, |settings| settings.theme = preference);
    apply(window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Composite sRGB first, then linearize for WCAG relative luminance.
    fn over(foreground: Hsla, background: Hsla) -> Hsla {
        let (f, b) = (foreground.to_rgb(), background.to_rgb());
        gpui_kit::Rgba {
            r: f.r * f.a + b.r * (1. - f.a),
            g: f.g * f.a + b.g * (1. - f.a),
            b: f.b * f.a + b.b * (1. - f.a),
            a: 1.,
        }
        .into()
    }

    fn contrast(a: Hsla, b: Hsla) -> f64 {
        let luminance = |color: Hsla| {
            let color = color.to_rgb();
            [color.r, color.g, color.b]
                .into_iter()
                .zip([0.2126, 0.7152, 0.0722])
                .map(|(channel, weight)| {
                    let channel = f64::from(channel);
                    weight
                        * if channel <= 0.04045 {
                            channel / 12.92
                        } else {
                            ((channel + 0.055) / 1.055).powf(2.4)
                        }
                })
                .sum::<f64>()
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn check(name: &str, foreground: Hsla, background: Hsla, minimum: f64) {
        let ratio = contrast(over(foreground, background), background);
        println!("{name}: {ratio:.5}:1 (minimum {minimum})");
        assert!(ratio >= minimum, "{name}: {ratio}:1 < {minimum}:1");
    }

    #[test]
    fn light_link_focus_has_text_and_state_contrast() {
        let mut t = Theme::default();
        light_colors(&mut t);
        let (background, foreground) = ProductColors::light().link_focus();
        check("focused external link text", foreground, background, 4.5);
        check("external link focus cue", background, t.background, 3.);
    }

    #[test]
    fn light_text_and_necessary_state_cues_have_contrast() {
        let mut t = Theme::default();
        light_colors(&mut t);
        let product = ProductColors::light();
        for (name, bg) in [
            ("main", t.background),
            ("surface", t.group_box),
            ("chrome", t.sidebar),
            ("hover", t.list_hover),
            ("selected", t.list_active),
        ] {
            check(&format!("body/{name}"), t.foreground, bg, 4.5);
            check(&format!("muted/{name}"), t.muted_foreground, bg, 4.5);
            check(&format!("control outline/{name}"), t.input, bg, 3.);
            check(&format!("focus/{name}"), t.ring, bg, 3.);
            for (label, color) in [
                ("danger", t.danger),
                ("warning", t.warning),
                ("success", t.success),
                ("info", t.info),
            ] {
                check(&format!("{label}/{name}"), color, bg, 4.5);
            }
            for kind in [
                None,
                Some(LoaderKind::Fabric),
                Some(LoaderKind::Forge),
                Some(LoaderKind::Neoforge),
            ] {
                let color = product.loader(kind);
                check(
                    &format!("loader {kind:?}/tint/{name}"),
                    color,
                    over(color.opacity(0.14), bg),
                    4.5,
                );
            }
            for ix in 0..8 {
                let color = product.tint(ix);
                check(
                    &format!("tile {ix}/{name}"),
                    color,
                    over(color.opacity(0.16), bg),
                    4.5,
                );
            }
        }
        for (label, foreground, backgrounds) in [
            (
                "primary",
                t.button_primary_foreground,
                [
                    t.button_primary,
                    t.button_primary_hover,
                    t.button_primary_active,
                ],
            ),
            (
                "danger",
                t.button_danger_foreground,
                [
                    t.button_danger,
                    t.button_danger_hover,
                    t.button_danger_active,
                ],
            ),
            (
                "warning",
                t.button_warning_foreground,
                [
                    t.button_warning,
                    t.button_warning_hover,
                    t.button_warning_active,
                ],
            ),
            (
                "success",
                t.button_success_foreground,
                [
                    t.button_success,
                    t.button_success_hover,
                    t.button_success_active,
                ],
            ),
            (
                "info",
                t.button_info_foreground,
                [t.button_info, t.button_info_hover, t.button_info_active],
            ),
            (
                "launch",
                t.primary_foreground,
                [
                    product.signature(),
                    product.signature_hover(),
                    product.signature_active(),
                ],
            ),
        ] {
            for bg in backgrounds {
                check(label, foreground, over(bg, t.background), 4.5);
            }
        }
        check("popover", t.popover_foreground, t.popover, 4.5);
        check("input text", t.foreground, t.background, 4.5);
        check("selection text", t.foreground, t.selection, 4.5);
        check("rail selection marker", t.primary, t.sidebar, 3.);
        check("progress", t.progress_bar, t.background, 3.);
        check(
            "progress indicator/track",
            t.progress_bar,
            over(t.progress_bar.opacity(0.2), t.background),
            3.,
        );
        check(
            "filled button inset focus",
            t.primary_foreground.opacity(0.6),
            t.primary,
            3.,
        );
        check(
            "launch inset focus",
            t.primary_foreground.opacity(0.6),
            product.signature(),
            3.,
        );
        check("switch thumb", t.switch_thumb, t.switch, 3.);
    }
}
