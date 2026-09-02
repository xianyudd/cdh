//! The picker's color system: the raw `Rgb`/`Palette` data, the `ThemeChoice`
//! cycle persisted to settings, and the `Theme` wrapper that resolves palette
//! slots into terminal styles while honoring the color-disabled path.

use ratatui::style::{Color, Modifier, Style};

use super::cube;

/// An RGB triple used to seed a palette. Kept separate from `Color` so palettes
/// stay plain data and the color-disabled path can ignore them uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rgb(pub(super) u8, pub(super) u8, pub(super) u8);

/// A named set of colors that drives every themed style. Adding a theme is a
/// matter of defining one `Palette`; the rendering code keeps calling the same
/// `Theme` methods regardless of which palette is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Palette {
    /// Full-screen canvas background. Makes theme switches visible even when
    /// the terminal default bg would otherwise swallow fg-only changes.
    pub(super) surface: Rgb,
    /// Elevated background for help/settings/confirm panels.
    pub(super) panel: Rgb,
    pub(super) border: Rgb,
    pub(super) title: Rgb,
    pub(super) primary: Rgb,
    pub(super) dim: Rgb,
    pub(super) accent: Rgb,
    pub(super) match_hit: Rgb,
    pub(super) warning: Rgb,
    pub(super) success: Rgb,
    pub(super) selected_fg: Rgb,
    pub(super) selected_bg: Rgb,
}

/// Default calm blue-graphite scheme. Surface is a deep navy so the flat UI
/// still reads as a solid canvas without a boxed border.
const PALETTE_GRAPHITE: Palette = Palette {
    surface: Rgb(0x14, 0x18, 0x22),
    panel: Rgb(0x1b, 0x22, 0x30),
    border: Rgb(0x51, 0x5f, 0x7d),
    title: Rgb(0xe8, 0xee, 0xff),
    primary: Rgb(0xd8, 0xe1, 0xf5),
    dim: Rgb(0x7d, 0x89, 0xa6),
    accent: Rgb(0xa8, 0xb8, 0xff),
    match_hit: Rgb(0xc3, 0xe8, 0x8d),
    warning: Rgb(0xff, 0xcb, 0x6b),
    success: Rgb(0x98, 0xc3, 0x79),
    selected_fg: Rgb(0xf7, 0xf9, 0xff),
    selected_bg: Rgb(0x35, 0x45, 0x6a),
};

/// Nord: cooler polar-night surface with teal accents.
const PALETTE_NORD: Palette = Palette {
    surface: Rgb(0x2e, 0x34, 0x40),
    panel: Rgb(0x3b, 0x42, 0x52),
    border: Rgb(0x4c, 0x56, 0x6a),
    title: Rgb(0xec, 0xef, 0xf4),
    primary: Rgb(0xd8, 0xde, 0xe9),
    dim: Rgb(0x7b, 0x88, 0xa1),
    accent: Rgb(0x88, 0xc0, 0xd0),
    match_hit: Rgb(0xa3, 0xbe, 0x8c),
    warning: Rgb(0xeb, 0xcb, 0x8b),
    success: Rgb(0xa3, 0xbe, 0x8c),
    selected_fg: Rgb(0xec, 0xef, 0xf4),
    selected_bg: Rgb(0x43, 0x4c, 0x5e),
};

/// Daylight: inverted light canvas for bright terminals.
const PALETTE_DAYLIGHT: Palette = Palette {
    surface: Rgb(0xf4, 0xf6, 0xfa),
    panel: Rgb(0xff, 0xff, 0xff),
    border: Rgb(0xc4, 0xc9, 0xd4),
    title: Rgb(0x1c, 0x22, 0x2b),
    primary: Rgb(0x2e, 0x35, 0x40),
    dim: Rgb(0x8a, 0x91, 0x9e),
    accent: Rgb(0x2f, 0x6f, 0xd0),
    match_hit: Rgb(0x2f, 0x8a, 0x4e),
    warning: Rgb(0xb0, 0x6a, 0x00),
    success: Rgb(0x2f, 0x8a, 0x4e),
    selected_fg: Rgb(0x1c, 0x22, 0x2b),
    selected_bg: Rgb(0xd5, 0xe2, 0xf7),
};

/// Mono: near-monochrome grays with a single cool accent.
const PALETTE_MONO: Palette = Palette {
    surface: Rgb(0x12, 0x12, 0x12),
    panel: Rgb(0x1c, 0x1c, 0x1c),
    border: Rgb(0x44, 0x44, 0x44),
    title: Rgb(0xf0, 0xf0, 0xf0),
    primary: Rgb(0xd0, 0xd0, 0xd0),
    dim: Rgb(0x80, 0x80, 0x80),
    accent: Rgb(0x9a, 0xb8, 0xe0),
    match_hit: Rgb(0x9a, 0xb8, 0xe0),
    warning: Rgb(0xc8, 0xc8, 0xc8),
    success: Rgb(0x9a, 0xb8, 0xe0),
    selected_fg: Rgb(0xf7, 0xf7, 0xf7),
    selected_bg: Rgb(0x3a, 0x3a, 0x3a),
};

/// Dracula: purple/pink high-contrast dark theme, clearly distinct from blue-gray sets.
const PALETTE_DRACULA: Palette = Palette {
    surface: Rgb(0x28, 0x2a, 0x36),
    panel: Rgb(0x31, 0x34, 0x44),
    border: Rgb(0x62, 0x72, 0xa4),
    title: Rgb(0xf8, 0xf8, 0xf2),
    primary: Rgb(0xf8, 0xf8, 0xf2),
    dim: Rgb(0x62, 0x72, 0xa4),
    accent: Rgb(0xbd, 0x93, 0xf9),
    match_hit: Rgb(0x50, 0xfa, 0x7b),
    warning: Rgb(0xff, 0xb8, 0x6c),
    success: Rgb(0x50, 0xfa, 0x7b),
    selected_fg: Rgb(0xf8, 0xf8, 0xf2),
    selected_bg: Rgb(0x44, 0x47, 0x5a),
};

/// Amber: warm terminal phosphor on deep brown, high visual distance from cool themes.
const PALETTE_AMBER: Palette = Palette {
    surface: Rgb(0x1a, 0x12, 0x08),
    panel: Rgb(0x24, 0x18, 0x0c),
    border: Rgb(0x8a, 0x5a, 0x20),
    title: Rgb(0xff, 0xd2, 0x7a),
    primary: Rgb(0xf0, 0xc8, 0x78),
    dim: Rgb(0x9a, 0x74, 0x3c),
    accent: Rgb(0xff, 0xb0, 0x40),
    match_hit: Rgb(0xff, 0xe0, 0x8a),
    warning: Rgb(0xff, 0x8a, 0x4c),
    success: Rgb(0xc8, 0xe0, 0x6a),
    selected_fg: Rgb(0x1a, 0x12, 0x08),
    selected_bg: Rgb(0xff, 0xb0, 0x40),
};

/// Forest: green-moss dark theme for a nature-leaning contrast set.
const PALETTE_FOREST: Palette = Palette {
    surface: Rgb(0x10, 0x18, 0x12),
    panel: Rgb(0x16, 0x22, 0x18),
    border: Rgb(0x3d, 0x5c, 0x45),
    title: Rgb(0xe4, 0xf0, 0xe6),
    primary: Rgb(0xc8, 0xdc, 0xcc),
    dim: Rgb(0x6f, 0x8a, 0x76),
    accent: Rgb(0x7d, 0xc4, 0x92),
    match_hit: Rgb(0xb8, 0xe0, 0x86),
    warning: Rgb(0xe0, 0xc0, 0x6a),
    success: Rgb(0x7d, 0xc4, 0x92),
    selected_fg: Rgb(0xe4, 0xf0, 0xe6),
    selected_bg: Rgb(0x2a, 0x42, 0x32),
};

/// The selectable color themes, in the order the settings panel and the
/// `Ctrl+T` hotkey cycle through them. Graphite is first so it stays the
/// default when no preference is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ThemeChoice {
    Graphite,
    Nord,
    Daylight,
    Mono,
    Dracula,
    Amber,
    Forest,
}

impl ThemeChoice {
    /// All themes in cycle order. The single source of truth for iteration,
    /// wrap-around, and count, so adding a theme only touches this array.
    const ALL: [ThemeChoice; 7] = [
        ThemeChoice::Graphite,
        ThemeChoice::Nord,
        ThemeChoice::Daylight,
        ThemeChoice::Mono,
        ThemeChoice::Dracula,
        ThemeChoice::Amber,
        ThemeChoice::Forest,
    ];

    fn palette(self) -> Palette {
        match self {
            ThemeChoice::Graphite => PALETTE_GRAPHITE,
            ThemeChoice::Nord => PALETTE_NORD,
            ThemeChoice::Daylight => PALETTE_DAYLIGHT,
            ThemeChoice::Mono => PALETTE_MONO,
            ThemeChoice::Dracula => PALETTE_DRACULA,
            ThemeChoice::Amber => PALETTE_AMBER,
            ThemeChoice::Forest => PALETTE_FOREST,
        }
    }

    /// The stable token persisted to `tui.toml` and accepted from `CDH_THEME`.
    pub(super) fn tag(self) -> &'static str {
        match self {
            ThemeChoice::Graphite => "graphite",
            ThemeChoice::Nord => "nord",
            ThemeChoice::Daylight => "daylight",
            ThemeChoice::Mono => "mono",
            ThemeChoice::Dracula => "dracula",
            ThemeChoice::Amber => "amber",
            ThemeChoice::Forest => "forest",
        }
    }

    pub(super) fn from_tag(value: &str) -> Option<ThemeChoice> {
        let normalized = value.trim().to_ascii_lowercase();
        ThemeChoice::ALL
            .into_iter()
            .find(|choice| choice.tag() == normalized)
    }

    /// Step `direction` positions through `ALL`, wrapping at both ends so the
    /// settings panel and hotkey cycle without a dead stop.
    pub(super) fn cycle(self, direction: isize) -> ThemeChoice {
        let index = ThemeChoice::ALL
            .iter()
            .position(|choice| *choice == self)
            .unwrap_or(0) as isize;
        let count = ThemeChoice::ALL.len() as isize;
        let next = (index + direction).rem_euclid(count) as usize;
        ThemeChoice::ALL[next]
    }
}

pub(super) struct Theme {
    on: bool,
    pub(super) palette: Palette,
}

impl Theme {
    #[cfg(test)]
    pub(super) fn new(on: bool) -> Self {
        Self::with_choice(on, ThemeChoice::Graphite)
    }

    pub(super) fn with_choice(on: bool, choice: ThemeChoice) -> Self {
        Self::with_palette(on, choice.palette())
    }

    fn with_palette(on: bool, palette: Palette) -> Self {
        Self { on, palette }
    }

    /// Full-screen canvas fill. Colorless mode leaves the terminal bg alone.
    pub(super) fn surface(&self) -> Style {
        if self.on {
            Style::default().bg(self.rgb(self.palette.surface))
        } else {
            Style::default()
        }
    }

    /// Elevated panel fill for help/settings/confirm overlays.
    pub(super) fn panel(&self) -> Style {
        if self.on {
            Style::default().bg(self.rgb(self.palette.panel))
        } else {
            Style::default()
        }
    }

    pub(super) fn rgb(&self, rgb: Rgb) -> Color {
        self.color(rgb.0, rgb.1, rgb.2)
    }

    /// Which palette slots the ambient cube shades between. Picking the slots is
    /// theme business, so it lives here; what the cube does with them (the
    /// shadow pull, the ramp, the sub-pixel rasterizer) is entirely its own and
    /// lives in `cube`.
    ///
    /// Hands over the raw seeds rather than resolved `Color`s because the ramp
    /// interpolates in RGB space, and skips the `on` check because the cube only
    /// ever draws when `App::corner_3d_enabled` already required color.
    pub(super) fn cube_ink(&self) -> cube::Ink {
        cube::Ink {
            accent: self.palette.accent,
            surface: self.palette.surface,
            highlight: self.palette.title,
        }
    }

    /// Resolve a raw RGB triple, honoring the color-disabled path. Retained so
    /// the few call sites that still pass literal colors keep working.
    fn color(&self, red: u8, green: u8, blue: u8) -> Color {
        if self.on {
            Color::Rgb(red, green, blue)
        } else {
            Color::Reset
        }
    }

    pub(super) fn border(&self) -> Style {
        Style::default().fg(self.rgb(self.palette.border))
    }

    pub(super) fn title(&self) -> Style {
        Style::default()
            .fg(self.rgb(self.palette.title))
            .add_modifier(Modifier::BOLD)
    }

    pub(super) fn primary(&self) -> Style {
        Style::default().fg(self.rgb(self.palette.primary))
    }

    pub(super) fn dim(&self) -> Style {
        Style::default().fg(self.dim_color())
    }

    pub(super) fn dim_color(&self) -> Color {
        self.rgb(self.palette.dim)
    }

    pub(super) fn accent(&self) -> Style {
        Style::default().fg(self.rgb(self.palette.accent))
    }

    pub(super) fn key_hint(&self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }

    pub(super) fn match_color(&self) -> Color {
        self.rgb(self.palette.match_hit)
    }

    pub(super) fn warning_color(&self) -> Color {
        self.rgb(self.palette.warning)
    }

    pub(super) fn warning(&self) -> Style {
        Style::default().fg(self.warning_color())
    }

    pub(super) fn success_color(&self) -> Color {
        self.rgb(self.palette.success)
    }

    pub(super) fn selected(&self) -> Style {
        if self.on {
            Style::default()
                .fg(self.rgb(self.palette.selected_fg))
                .bg(self.rgb(self.palette.selected_bg))
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    pub(super) fn selected_marker(&self) -> Style {
        let selected = self.selected().add_modifier(Modifier::BOLD);
        if self.on {
            selected.fg(self.match_color())
        } else {
            selected
        }
    }

    pub(super) fn matched(&self, base: Style) -> Style {
        base.fg(self.match_color())
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }
}
