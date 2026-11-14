//! The colour scheme, and resolving emulator colours into RGB.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb as VteRgb};
use connecto_core::Rgb;

pub(crate) const FOREGROUND: Rgb = Rgb::new(0xd8, 0xdc, 0xe3);
pub(crate) const BACKGROUND: Rgb = Rgb::new(0x14, 0x16, 0x1a);
const CURSOR: Rgb = Rgb::new(0xd8, 0xdc, 0xe3);

const ANSI: [Rgb; 16] = [
    Rgb::new(0x1d, 0x1f, 0x24),
    Rgb::new(0xe0, 0x6c, 0x75),
    Rgb::new(0x98, 0xc3, 0x79),
    Rgb::new(0xe5, 0xc0, 0x7b),
    Rgb::new(0x61, 0xaf, 0xef),
    Rgb::new(0xc6, 0x78, 0xdd),
    Rgb::new(0x56, 0xb6, 0xc2),
    Rgb::new(0xab, 0xb2, 0xbf),
    Rgb::new(0x5c, 0x63, 0x70),
    Rgb::new(0xf0, 0x8a, 0x93),
    Rgb::new(0xb5, 0xdb, 0x98),
    Rgb::new(0xf2, 0xd3, 0x9a),
    Rgb::new(0x8c, 0xc8, 0xf7),
    Rgb::new(0xdc, 0x9b, 0xee),
    Rgb::new(0x7e, 0xcf, 0xd9),
    Rgb::new(0xf2, 0xf4, 0xf8),
];

/// Colour of a 256-colour palette index, before any application override.
fn indexed(index: u8) -> Rgb {
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Rgb::new(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let grey = 8 + (index - 232) * 10;
            Rgb::new(grey, grey, grey)
        }
    }
}

fn dim(color: Rgb) -> Rgb {
    let scale = |v: u8| (u16::from(v) * 2 / 3) as u8;
    Rgb::new(scale(color.r), scale(color.g), scale(color.b))
}

fn named_default(named: NamedColor) -> Rgb {
    match named {
        NamedColor::Foreground | NamedColor::BrightForeground => FOREGROUND,
        NamedColor::Background => BACKGROUND,
        NamedColor::Cursor => CURSOR,
        NamedColor::DimForeground => dim(FOREGROUND),
        NamedColor::DimBlack => dim(ANSI[0]),
        NamedColor::DimRed => dim(ANSI[1]),
        NamedColor::DimGreen => dim(ANSI[2]),
        NamedColor::DimYellow => dim(ANSI[3]),
        NamedColor::DimBlue => dim(ANSI[4]),
        NamedColor::DimMagenta => dim(ANSI[5]),
        NamedColor::DimCyan => dim(ANSI[6]),
        NamedColor::DimWhite => dim(ANSI[7]),
        other => indexed(other as u8),
    }
}

/// The colour reported for an OSC 4, 10, 11 or 12 query.
pub(crate) fn reported_color(index: usize) -> VteRgb {
    let color = match index {
        0..=255 => u8::try_from(index).map_or(FOREGROUND, indexed),
        i if i == NamedColor::Background as usize => BACKGROUND,
        i if i == NamedColor::Cursor as usize => CURSOR,
        _ => FOREGROUND,
    };
    VteRgb {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

fn from_vte(color: VteRgb) -> Rgb {
    Rgb::new(color.r, color.g, color.b)
}

/// Resolve a cell colour, honouring colours the application redefined.
pub(crate) fn resolve(color: Color, overrides: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => from_vte(rgb),
        Color::Named(named) => overrides[named].map_or_else(|| named_default(named), from_vte),
        Color::Indexed(index) => {
            overrides[usize::from(index)].map_or_else(|| indexed(index), from_vte)
        }
    }
}

/// The bright variant used for bold text in one of the eight base colours.
pub(crate) fn brighten(color: Color) -> Color {
    match color {
        Color::Named(named) => Color::Named(named.to_bright()),
        Color::Indexed(index @ 0..=7) => Color::Indexed(index + 8),
        other => other,
    }
}

/// The dimmed variant used for faint text.
pub(crate) fn dimmed(color: Rgb) -> Rgb {
    dim(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_cube_matches_xterm() {
        assert_eq!(indexed(16), Rgb::new(0, 0, 0));
        assert_eq!(indexed(196), Rgb::new(255, 0, 0));
        assert_eq!(indexed(231), Rgb::new(255, 255, 255));
    }

    #[test]
    fn greyscale_ramp_matches_xterm() {
        assert_eq!(indexed(232), Rgb::new(8, 8, 8));
        assert_eq!(indexed(255), Rgb::new(238, 238, 238));
    }

    #[test]
    fn background_query_reports_the_real_background() {
        let reported = reported_color(NamedColor::Background as usize);
        assert_eq!(from_vte(reported), BACKGROUND);
    }

    #[test]
    fn every_palette_query_is_answered_without_panicking() {
        for index in 0..300 {
            let _ = reported_color(index);
        }
    }

    #[test]
    fn bold_brightens_only_the_base_colours() {
        assert_eq!(
            brighten(Color::Named(NamedColor::Red)),
            Color::Named(NamedColor::BrightRed)
        );
        assert_eq!(brighten(Color::Indexed(1)), Color::Indexed(9));
        assert_eq!(brighten(Color::Indexed(200)), Color::Indexed(200));
    }
}
