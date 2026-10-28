//! Colour tokens (crypto_pro's dark palette, plus loader colours) and how
//! they are put on the gpui-component `Theme`.
//!
//! Panels draw with `Palette` (a gpui `Global`); gpui-component widgets
//! (inputs, buttons, menus, tooltips) read the `Theme`, which `apply` fills
//! from the same tokens, so both always match.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Window, rgb};

use crate::data::Loader;

/// crypto_pro `DARK` tokens (`crates/app/src/appearance.rs`), unchanged.
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

    /// Patina additions: loader colours that sit next to the tokens above.
    pub const NEOFORGE: u32 = 0xf27a45;
    pub const FORGE: u32 = 0x7d93c4;
    pub const QUILT: u32 = 0xa98bf5;
}

/// Icon-tile tints for instances (index into this with `Instance::tint`).
const TINTS: [u32; 8] = [
    0x1aaaa7, 0xf27a45, 0x7d93c4, 0x2a99fb, 0xe8a317, 0x14b39f, 0xc678dd, 0xff5c8a,
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Window and panel background.
    pub bg: Hsla,
    /// Active rail item, chip hover, secondary buttons.
    pub raised: Hsla,
    /// Title bar, rail, status bar.
    pub header: Hsla,
    /// Row hover.
    pub hover: Hsla,
    /// Selected row.
    pub selected: Hsla,
    /// 1 px dividers.
    pub border: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    /// Links, selected chip, focus, the rail indicator.
    pub accent: Hsla,
    /// Tab underline, the launch button.
    pub teal: Hsla,
    pub amber: Hsla,
    pub green: Hsla,
    pub red: Hsla,
    neoforge: Hsla,
    forge: Hsla,
    quilt: Hsla,
}

impl Global for Palette {}

fn c(v: u32) -> Hsla {
    Hsla::from(rgb(v))
}

impl Palette {
    pub fn dark() -> Self {
        use tokens::*;
        Self {
            bg: c(BG),
            raised: c(RAISED),
            header: c(HEADER),
            hover: c(HOVER),
            selected: c(SELECTED),
            border: c(BORDER),
            text: c(TEXT),
            muted: c(MUTED),
            accent: c(ACCENT),
            teal: c(TEAL),
            amber: c(AMBER),
            green: c(GREEN),
            red: c(RED),
            neoforge: c(NEOFORGE),
            forge: c(FORGE),
            quilt: c(QUILT),
        }
    }

    pub fn get(cx: &App) -> &Palette {
        cx.global::<Palette>()
    }

    /// Tag colour of a mod loader.
    pub fn loader(&self, l: Loader) -> Hsla {
        match l {
            Loader::Vanilla => self.accent,
            Loader::Fabric => self.amber,
            Loader::Forge => self.forge,
            Loader::NeoForge => self.neoforge,
            Loader::Quilt => self.quilt,
        }
    }

    /// Icon-tile tint `i` (wraps).
    pub fn tint(&self, i: usize) -> Hsla {
        c(TINTS[i % TINTS.len()])
    }
}

/// `color` with its lightness moved by `d` (for hover / pressed fills).
pub fn shade(color: Hsla, d: f32) -> Hsla {
    Hsla {
        l: (color.l + d).clamp(0., 1.),
        ..color
    }
}

/// Same mapping as crypto_pro's `appearance::theme_colors`.
fn theme_colors(t: &mut Theme, p: &Palette) {
    let white = c(0xffffff);
    t.background = p.bg;
    t.foreground = p.text;
    t.muted = p.raised;
    t.muted_foreground = p.muted;
    t.border = p.border;
    t.input = p.border;
    t.ring = p.accent;
    t.caret = p.accent;
    t.title_bar = p.header;
    t.title_bar_border = p.border;
    t.list_hover = p.hover;
    t.list_active = p.selected;
    t.list_active_border = p.accent;
    t.list_head = p.header;
    t.primary = p.accent;
    t.primary_hover = p.accent.opacity(0.85);
    t.primary_active = p.accent.opacity(0.75);
    t.primary_foreground = white;
    t.accent = p.raised;
    t.accent_foreground = p.text;
    t.secondary = p.raised;
    t.secondary_hover = p.hover;
    t.secondary_active = p.selected;
    t.secondary_foreground = p.text;
    t.popover = p.raised;
    t.popover_foreground = p.text;
    t.tab_bar = p.bg;
    t.tab = p.bg;
    t.tab_active = p.bg;
    t.tab_active_foreground = p.text;
    t.tab_foreground = p.muted;
    t.link = p.accent;
}

/// Switches gpui-component to dark mode and puts the palette on it. Call
/// before the first frame (in the `open_window` builder) so the window
/// never flashes the default theme.
pub fn apply(window: &mut Window, cx: &mut App) {
    Theme::change(ThemeMode::Dark, Some(window), cx);
    let palette = Palette::dark();
    Theme::update(cx, |t| theme_colors(t, &palette));
    cx.set_global(palette);
    window.refresh();
}
