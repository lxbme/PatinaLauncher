//! Colours: crypto_pro's dark tokens put on the gpui-component `Theme`, plus
//! the product colours the theme has no role for ([`ProductColors`]).
//!
//! This is the only module that may name a raw colour. Views read the
//! semantic roles from `cx.theme()` and the product colours from
//! [`ProductColors::get`]. Patina has a dark theme only.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Window, rgb};
use patina_core::loader::LoaderKind;

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
            vanilla: c(tokens::ACCENT),
            fabric: c(tokens::TEAL),
            forge: c(FORGE),
            neoforge: c(NEOFORGE),
            tints: TINTS.map(c),
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

/// Switches gpui-component to its dark theme, puts Patina's tokens on it and
/// sets [`ProductColors`]. Call in a window's `open_window` builder, before
/// the first frame, so the window never flashes the default theme; then
/// apply the UI scale (`scale::apply_scale`), which builds on these sizes.
pub fn apply(window: &mut Window, cx: &mut App) {
    Theme::change(ThemeMode::Dark, Some(window), cx);
    // A second `update`: a mode change reloads the colours, so they are
    // edited after it (see `Theme::update`).
    Theme::update(cx, theme_colors);
    cx.set_global(ProductColors::dark());
    window.refresh();
}
