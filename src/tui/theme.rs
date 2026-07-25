//! Runtime-switchable color themes.
//!
//! Palette data (the `accent`/`secondary`/`bg`/`fg`/`muted`/`selection`/
//! `error`/`warning`/`success`/`info` values per theme) is ported from
//! [ratatui-themes](https://github.com/ricardodantas/ratatui-themes)
//! (MIT-licensed) rather than taken on as a dependency — that crate is
//! pinned to ratatui 0.30, and this app is on 0.29, so pulling it in would
//! either give two incompatible copies of `ratatui::style::Color` or force
//! an app-wide ratatui upgrade just for a cosmetic feature. The color
//! values themselves are just data, so they're copied in directly and
//! credited here instead.
//!
//! atk's own screens were built around a fixed 11-color palette (3
//! background tiers, 2 foreground tiers, a border tone, a title tone, an
//! accent, and 3 semantic colors) that predates and doesn't line up 1:1
//! with ratatui-themes' 10-field semantic palette, so [`Palette::derive`]
//! maps one onto the other with a documented rule per field rather than
//! guessing per-theme.

use ratatui::style::Color;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::config_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeName {
    /// atk's original hand-picked palette (Tailwind slate/sky), predating
    /// the theme system — kept as its own selectable entry, and the
    /// default, so adding themes doesn't change anyone's screen by
    /// default; it's just the first stop on the cycle.
    Classic,
    Dracula,
    OneDarkPro,
    Nord,
    CatppuccinMocha,
    CatppuccinLatte,
    GruvboxDark,
    GruvboxLight,
    TokyoNight,
    SolarizedDark,
    SolarizedLight,
    MonokaiPro,
    RosePine,
    Kanagawa,
    Everforest,
    Cyberpunk,
}

impl ThemeName {
    pub const ALL: [ThemeName; 16] = [
        ThemeName::Classic,
        ThemeName::Dracula,
        ThemeName::OneDarkPro,
        ThemeName::Nord,
        ThemeName::CatppuccinMocha,
        ThemeName::CatppuccinLatte,
        ThemeName::GruvboxDark,
        ThemeName::GruvboxLight,
        ThemeName::TokyoNight,
        ThemeName::SolarizedDark,
        ThemeName::SolarizedLight,
        ThemeName::MonokaiPro,
        ThemeName::RosePine,
        ThemeName::Kanagawa,
        ThemeName::Everforest,
        ThemeName::Cyberpunk,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Classic => "Classic",
            ThemeName::Dracula => "Dracula",
            ThemeName::OneDarkPro => "One Dark Pro",
            ThemeName::Nord => "Nord",
            ThemeName::CatppuccinMocha => "Catppuccin Mocha",
            ThemeName::CatppuccinLatte => "Catppuccin Latte",
            ThemeName::GruvboxDark => "Gruvbox Dark",
            ThemeName::GruvboxLight => "Gruvbox Light",
            ThemeName::TokyoNight => "Tokyo Night",
            ThemeName::SolarizedDark => "Solarized Dark",
            ThemeName::SolarizedLight => "Solarized Light",
            ThemeName::MonokaiPro => "Monokai Pro",
            ThemeName::RosePine => "Rosé Pine",
            ThemeName::Kanagawa => "Kanagawa",
            ThemeName::Everforest => "Everforest",
            ThemeName::Cyberpunk => "Cyberpunk",
        }
    }

    /// Kebab-case slug, used in the persisted `theme.json` so the config
    /// file stays readable/hand-editable, matching every other config file
    /// this app writes.
    fn slug(self) -> &'static str {
        match self {
            ThemeName::Classic => "classic",
            ThemeName::Dracula => "dracula",
            ThemeName::OneDarkPro => "one-dark-pro",
            ThemeName::Nord => "nord",
            ThemeName::CatppuccinMocha => "catppuccin-mocha",
            ThemeName::CatppuccinLatte => "catppuccin-latte",
            ThemeName::GruvboxDark => "gruvbox-dark",
            ThemeName::GruvboxLight => "gruvbox-light",
            ThemeName::TokyoNight => "tokyo-night",
            ThemeName::SolarizedDark => "solarized-dark",
            ThemeName::SolarizedLight => "solarized-light",
            ThemeName::MonokaiPro => "monokai-pro",
            ThemeName::RosePine => "rose-pine",
            ThemeName::Kanagawa => "kanagawa",
            ThemeName::Everforest => "everforest",
            ThemeName::Cyberpunk => "cyberpunk",
        }
    }

    fn from_slug(s: &str) -> Option<ThemeName> {
        Self::ALL.into_iter().find(|t| t.slug() == s)
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// The raw semantic palette this theme was defined with, before
    /// deriving atk's own 11-color shape from it.
    fn semantic(self) -> Semantic {
        // Values ported from ratatui-themes' src/theme.rs (MIT license).
        match self {
            ThemeName::Classic => unreachable!("Classic has its own Palette::classic(), bypassing semantic derivation entirely"),
            ThemeName::Dracula => Semantic {
                accent: rgb(189, 147, 249),
                secondary: rgb(255, 121, 198),
                bg: rgb(40, 42, 54),
                fg: rgb(248, 248, 242),
                muted: rgb(98, 114, 164),
                selection: rgb(68, 71, 90),
                error: rgb(255, 85, 85),
                warning: rgb(255, 184, 108),
                success: rgb(80, 250, 123),
                info: rgb(139, 233, 253),
            },
            ThemeName::OneDarkPro => Semantic {
                accent: rgb(97, 175, 239),
                secondary: rgb(198, 120, 221),
                bg: rgb(40, 44, 52),
                fg: rgb(171, 178, 191),
                muted: rgb(92, 99, 112),
                selection: rgb(62, 68, 81),
                error: rgb(224, 108, 117),
                warning: rgb(229, 192, 123),
                success: rgb(152, 195, 121),
                info: rgb(86, 182, 194),
            },
            ThemeName::Nord => Semantic {
                accent: rgb(136, 192, 208),
                secondary: rgb(129, 161, 193),
                bg: rgb(46, 52, 64),
                fg: rgb(236, 239, 244),
                muted: rgb(76, 86, 106),
                selection: rgb(67, 76, 94),
                error: rgb(191, 97, 106),
                warning: rgb(235, 203, 139),
                success: rgb(163, 190, 140),
                info: rgb(94, 129, 172),
            },
            ThemeName::CatppuccinMocha => Semantic {
                accent: rgb(137, 180, 250),
                secondary: rgb(245, 194, 231),
                bg: rgb(30, 30, 46),
                fg: rgb(205, 214, 244),
                muted: rgb(108, 112, 134),
                selection: rgb(49, 50, 68),
                error: rgb(243, 139, 168),
                warning: rgb(249, 226, 175),
                success: rgb(166, 227, 161),
                info: rgb(148, 226, 213),
            },
            ThemeName::CatppuccinLatte => Semantic {
                accent: rgb(30, 102, 245),
                secondary: rgb(234, 118, 203),
                bg: rgb(239, 241, 245),
                fg: rgb(76, 79, 105),
                muted: rgb(140, 143, 161),
                selection: rgb(204, 208, 218),
                error: rgb(210, 15, 57),
                warning: rgb(223, 142, 29),
                success: rgb(64, 160, 43),
                info: rgb(23, 146, 153),
            },
            ThemeName::GruvboxDark => Semantic {
                accent: rgb(250, 189, 47),
                secondary: rgb(211, 134, 155),
                bg: rgb(40, 40, 40),
                fg: rgb(235, 219, 178),
                muted: rgb(146, 131, 116),
                selection: rgb(80, 73, 69),
                error: rgb(251, 73, 52),
                warning: rgb(254, 128, 25),
                success: rgb(184, 187, 38),
                info: rgb(131, 165, 152),
            },
            ThemeName::GruvboxLight => Semantic {
                accent: rgb(181, 118, 20),
                secondary: rgb(143, 63, 113),
                bg: rgb(251, 241, 199),
                fg: rgb(60, 56, 54),
                muted: rgb(146, 131, 116),
                selection: rgb(213, 196, 161),
                error: rgb(157, 0, 6),
                warning: rgb(175, 58, 3),
                success: rgb(121, 116, 14),
                info: rgb(66, 123, 88),
            },
            ThemeName::TokyoNight => Semantic {
                accent: rgb(122, 162, 247),
                secondary: rgb(187, 154, 247),
                bg: rgb(26, 27, 38),
                fg: rgb(192, 202, 245),
                muted: rgb(86, 95, 137),
                selection: rgb(41, 46, 66),
                error: rgb(247, 118, 142),
                warning: rgb(224, 175, 104),
                success: rgb(158, 206, 106),
                info: rgb(125, 207, 255),
            },
            ThemeName::SolarizedDark => Semantic {
                accent: rgb(38, 139, 210),
                secondary: rgb(108, 113, 196),
                bg: rgb(0, 43, 54),
                fg: rgb(131, 148, 150),
                muted: rgb(88, 110, 117),
                selection: rgb(7, 54, 66),
                error: rgb(220, 50, 47),
                warning: rgb(181, 137, 0),
                success: rgb(133, 153, 0),
                info: rgb(42, 161, 152),
            },
            ThemeName::SolarizedLight => Semantic {
                accent: rgb(38, 139, 210),
                secondary: rgb(108, 113, 196),
                bg: rgb(253, 246, 227),
                fg: rgb(101, 123, 131),
                muted: rgb(147, 161, 161),
                selection: rgb(238, 232, 213),
                error: rgb(220, 50, 47),
                warning: rgb(181, 137, 0),
                success: rgb(133, 153, 0),
                info: rgb(42, 161, 152),
            },
            ThemeName::MonokaiPro => Semantic {
                accent: rgb(255, 216, 102),
                secondary: rgb(171, 157, 242),
                bg: rgb(45, 42, 46),
                fg: rgb(252, 252, 250),
                muted: rgb(114, 113, 105),
                selection: rgb(81, 80, 79),
                error: rgb(255, 97, 136),
                warning: rgb(252, 152, 103),
                success: rgb(169, 220, 118),
                info: rgb(120, 220, 232),
            },
            ThemeName::RosePine => Semantic {
                accent: rgb(235, 188, 186),
                secondary: rgb(196, 167, 231),
                bg: rgb(25, 23, 36),
                fg: rgb(224, 222, 244),
                muted: rgb(110, 106, 134),
                selection: rgb(38, 35, 58),
                error: rgb(235, 111, 146),
                warning: rgb(246, 193, 119),
                success: rgb(156, 207, 216),
                info: rgb(49, 116, 143),
            },
            ThemeName::Kanagawa => Semantic {
                accent: rgb(127, 180, 202),
                secondary: rgb(149, 127, 184),
                bg: rgb(31, 31, 40),
                fg: rgb(220, 215, 186),
                muted: rgb(84, 84, 109),
                selection: rgb(54, 54, 70),
                error: rgb(195, 64, 67),
                warning: rgb(255, 169, 107),
                success: rgb(118, 148, 106),
                info: rgb(126, 156, 216),
            },
            ThemeName::Everforest => Semantic {
                accent: rgb(131, 193, 120),
                secondary: rgb(214, 153, 182),
                bg: rgb(47, 53, 55),
                fg: rgb(211, 198, 170),
                muted: rgb(133, 146, 137),
                selection: rgb(68, 78, 79),
                error: rgb(230, 126, 128),
                warning: rgb(219, 188, 127),
                success: rgb(167, 192, 128),
                info: rgb(124, 195, 191),
            },
            ThemeName::Cyberpunk => Semantic {
                accent: rgb(0, 255, 255),
                secondary: rgb(255, 0, 255),
                bg: rgb(13, 2, 33),
                fg: rgb(240, 240, 240),
                muted: rgb(100, 100, 140),
                selection: rgb(40, 20, 80),
                error: rgb(255, 0, 60),
                warning: rgb(255, 230, 0),
                success: rgb(0, 255, 100),
                info: rgb(0, 180, 255),
            },
        }
    }

    pub fn palette(self) -> Palette {
        match self {
            ThemeName::Classic => Palette::classic(),
            other => Palette::derive(other.semantic()),
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// A theme's palette in ratatui-themes' own 10-field semantic shape.
struct Semantic {
    accent: Color,
    #[allow(dead_code)] // kept for fidelity to the source data; atk's derived palette doesn't use a distinct "secondary" tier
    secondary: Color,
    bg: Color,
    fg: Color,
    muted: Color,
    selection: Color,
    error: Color,
    warning: Color,
    success: Color,
    info: Color,
}

/// atk's own 11-color shape (3 background tiers, 2 foreground tiers, a
/// border tone, a title tone, an accent, and 3 semantic colors) — this
/// predates the theme system, so every screen was written against exactly
/// these 11 names.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub bg2: Color,
    pub bg3: Color,
    pub border: Color,
    pub title: Color,
    pub fg: Color,
    pub fg2: Color,
    pub accent: Color,
    pub green: Color,
    pub red: Color,
    pub yellow: Color,
}

impl Palette {
    /// atk's original hand-picked palette, verbatim — not derived, so
    /// `ThemeName::Classic` (the default) looks exactly like every
    /// screenshot and screen recording of this app made before the theme
    /// system existed.
    fn classic() -> Palette {
        Palette {
            bg: rgb(15, 23, 42),
            bg2: rgb(30, 41, 59),
            bg3: rgb(51, 65, 85),
            border: rgb(100, 116, 139),
            title: rgb(125, 211, 252),
            fg: rgb(226, 232, 240),
            fg2: rgb(148, 163, 184),
            accent: rgb(56, 189, 248),
            green: rgb(74, 222, 128),
            red: rgb(248, 113, 113),
            yellow: rgb(250, 204, 21),
        }
    }

    /// One documented rule per field, applied the same way to every theme
    /// rather than hand-tuned per theme — that's what keeps this correct
    /// (and maintainable) across all 15 without eyeballing each one:
    ///
    /// - `bg`/`fg` map straight across.
    /// - `bg2`/`bg3` are two steps away from `bg` *toward* `selection`
    ///   (never toward `fg`, so this works the same direction on light and
    ///   dark themes alike) — `bg2` a subtle half-step for panel/header
    ///   backgrounds, `bg3` the full `selection` tone for the more
    ///   prominent unfocused-input-field / "this row is selected" tier.
    /// - `fg2` is `muted` directly (its documented purpose — "dimmed text,
    ///   comments, placeholders" — is exactly atk's FG2 role).
    /// - `border` is `muted` pulled part-way back toward `bg`, so it stays
    ///   visually subtler than `fg2` instead of the two being identical.
    /// - `title` is `info` (a themed accent distinct from the interactive
    ///   `accent` color, matching the two being close-but-different
    ///   shades in atk's original hand-picked palette).
    /// - `accent`/`green`/`red`/`yellow` map straight from
    ///   `accent`/`success`/`error`/`warning`.
    fn derive(s: Semantic) -> Palette {
        Palette {
            bg: s.bg,
            bg2: blend(s.bg, s.selection, 0.45),
            bg3: s.selection,
            border: blend(s.bg, s.muted, 0.7),
            title: s.info,
            fg: s.fg,
            fg2: s.muted,
            accent: s.accent,
            green: s.success,
            red: s.error,
            yellow: s.warning,
        }
    }
}

/// Linear interpolation between two `Color::Rgb` values; `t=0` returns
/// `a`, `t=1` returns `b`. Any non-RGB `Color` (not something any theme
/// here produces) just falls back to `a` rather than panicking.
fn blend(a: Color, b: Color, t: f32) -> Color {
    if let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (a, b) {
        let lerp = |x1: u8, x2: u8| -> u8 { (f32::from(x1) + (f32::from(x2) - f32::from(x1)) * t).round().clamp(0.0, 255.0) as u8 };
        Color::Rgb(lerp(r1, r2), lerp(g1, g2), lerp(b1, b2))
    } else {
        a
    }
}

static CURRENT: AtomicUsize = AtomicUsize::new(0);

fn theme_path() -> PathBuf {
    config_file("theme.json")
}

/// Loads the persisted theme choice, if any — called once at startup.
/// A missing/corrupt/unrecognized file quietly falls back to the default
/// (`Dracula`, index 0) rather than erroring, same as every other config
/// file this app reads.
pub fn load() {
    if let Ok(data) = std::fs::read_to_string(theme_path()) {
        if let Some(slug) = data.lines().next().map(str::trim) {
            if let Some(t) = ThemeName::from_slug(slug.trim_matches('"')) {
                CURRENT.store(t.index(), Ordering::Relaxed);
            }
        }
    }
}

fn save(t: ThemeName) {
    let _ = std::fs::write(theme_path(), format!("\"{}\"\n", t.slug()));
}

pub fn current() -> ThemeName {
    ThemeName::ALL[CURRENT.load(Ordering::Relaxed) % ThemeName::ALL.len()]
}

pub fn palette() -> Palette {
    current().palette()
}

/// Cycles to the next (`delta = 1`) or previous (`delta = -1`) theme and
/// persists the choice immediately — there's no separate "save" step, same
/// as the home menu's own tool-reordering.
pub fn cycle(delta: i32) {
    let len = ThemeName::ALL.len() as i32;
    let cur = CURRENT.load(Ordering::Relaxed) as i32;
    let new = (cur + delta).rem_euclid(len) as usize;
    CURRENT.store(new, Ordering::Relaxed);
    save(ThemeName::ALL[new]);
}
