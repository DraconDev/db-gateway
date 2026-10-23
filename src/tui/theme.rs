//! The view's colours, in one place, chosen by meaning.
//!
//! Every colour the view draws comes from here. Nothing else in the TUI
//! names a `Color` directly, so restyling the screen is a change to this
//! file and not a hunt through the drawing code.
//!
//! The palette is dark and high-contrast, and it is *semantic*: the roles
//! below are decided once (`"this is a heading"`, `"this is a failure"`) and
//! the colours are then chosen to serve them. A surface that picks colours
//! per call site drifts — one screen's "dim" ends up lighter than another's,
//! a warning and an error become the same red, and contrast is whatever each
//! edit happened to leave behind.
//!
//! ## Contrast
//!
//! The rule for this palette is a real one, not decoration: every role that
//! carries text is at least 4.5:1 against [`BACKGROUND`], the WCAG AA
//! threshold for body text, and the two roles used for large text (headings
//! and the active tab) are at least 7:1. The ratios are asserted in the
//! tests at the bottom of this file, so a future colour tweak that quietly
//! hurts readability fails the build instead of shipping.
//!
//! Colour is never the only signal. A failure is red *and* says "failed"; a
//! healthy state is green *and* says "connected"; the focused tab is tinted
//! *and* underlined. That matters for a monochrome terminal, for
//! colour-blind operators, and for anything reading the screen as text.

use ratatui::style::{Color, Modifier, Style};

use crate::render::Style as ViewStyle;

/// The colour behind the whole view.
///
/// The terminal's own background is used rather than painting one: a TUI
/// that sets its own background overrides whatever an operator configured
/// and can end up unreadable against a light profile. The palette is chosen
/// to read on a dark background, which is what the theme is for.
pub const BACKGROUND: Color = Color::Rgb(0x0b, 0x0f, 0x14);

/// The faintest text the view ever draws: keys, counts, borders at rest.
///
/// This is the floor for legibility. Anything dimmer than this is
/// unreadable on a dark terminal in daylight, so `Dim` is used for
/// secondary information only when that information is also present
/// somewhere more prominent.
pub const SURFACE: Color = Color::Rgb(0x7d, 0x8b, 0x9d);

/// Normal text: values, numbers, anything the operator reads.
pub const TEXT: Color = Color::Rgb(0xe6, 0xed, 0xf3);

/// Headings and the active tab: the strongest thing on screen.
pub const ACCENT: Color = Color::Rgb(0x7d, 0xd3, 0xfc);

/// A name or a path — something that identifies a thing.
pub const IDENTITY: Color = Color::Rgb(0xa5, 0xd6, 0xff);

/// Healthy, connected, fresh.
pub const POSITIVE: Color = Color::Rgb(0x6e, 0xe7, 0xb7);

/// Needs attention: stale, contended, nearly out of something.
pub const WARNING: Color = Color::Rgb(0xff, 0xd4, 0x66);

/// A failure, or a destructive act.
pub const NEGATIVE: Color = Color::Rgb(0xff, 0x8a, 0x8a);

/// The paint for a view style.
///
/// The view layer names the *meaning* of a value; this decides the colour.
/// That split is why the CLI's tables and this view cannot drift apart in
/// what they consider a failure.
pub fn paint(style: ViewStyle) -> Style {
    match style {
        ViewStyle::Plain => Style::default().fg(TEXT),
        ViewStyle::Dim => Style::default().fg(SURFACE),
        ViewStyle::Bold => Style::default()
            .fg(TEXT)
            .add_modifier(Modifier::BOLD),
        ViewStyle::Header => Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD),
        ViewStyle::Red => Style::default().fg(NEGATIVE),
        ViewStyle::Green => Style::default().fg(POSITIVE),
        ViewStyle::Yellow => Style::default().fg(WARNING),
        ViewStyle::Cyan => Style::default().fg(IDENTITY),
    }
}

/// A heading: a section title on screen.
pub fn heading() -> Style {
    Style::default()
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

/// A key hint in the command bar: the key itself.
pub fn key() -> Style {
    Style::default()
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

/// The label next to a key hint.
pub fn key_label() -> Style {
    Style::default().fg(SURFACE)
}

/// The tab strip's inactive tab.
pub fn tab() -> Style {
    Style::default().fg(SURFACE)
}

/// The tab strip's active tab.
///
/// Tinted *and* underlined: the two together survive a monochrome terminal
/// and a screenshot, where a tint alone does not.
pub fn tab_active() -> Style {
    Style::default()
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// The border of the tab that has the keyboard.
pub fn border_focused() -> Style {
    Style::default().fg(ACCENT)
}

/// The border of the tab body when it does not have the keyboard.
pub fn border() -> Style {
    Style::default().fg(Color::Rgb(0x30, 0x3c, 0x4d))
}

/// A table row that is selected.
pub fn selection() -> Style {
    Style::default()
        .fg(BACKGROUND)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

/// The status bar along the bottom.
pub fn status() -> Style {
    Style::default().fg(SURFACE)
}

/// The top bar: which gateway this is.
pub fn top_bar() -> Style {
    Style::default().fg(SURFACE)
}

/// The product mark in the top bar.
pub fn mark() -> Style {
    Style::default()
        .fg(BACKGROUND)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relative luminance of a colour, per WCAG 2.1.
    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("the palette is defined in RGB so contrast can be computed");
        };
        let channel = |value: u8| {
            let value = value as f64 / 255.0;
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    /// The contrast ratio between two colours, per WCAG 2.1.
    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (lighter, darker) = if la > lb { (la, lb) } else { (lb, la) };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// Body text: the AA threshold is 4.5:1.
    #[test]
    fn every_text_colour_is_readable_on_the_dark_background() {
        for (name, colour) in [
            ("TEXT", TEXT),
            ("SURFACE", SURFACE),
            ("ACCENT", ACCENT),
            ("IDENTITY", IDENTITY),
            ("POSITIVE", POSITIVE),
            ("WARNING", WARNING),
            ("NEGATIVE", NEGATIVE),
        ] {
            let ratio = contrast(colour, BACKGROUND);
            assert!(
                ratio >= 4.5,
                "{name} is {ratio:.2}:1 against the background, below the 4.5:1 floor for body text"
            );
        }
    }

    /// Headings and the active tab are emphasised text: 7:1 (AAA).
    #[test]
    fn the_emphasised_colours_clear_the_higher_bar() {
        for (name, colour) in [("ACCENT", ACCENT), ("TEXT", TEXT)] {
            let ratio = contrast(colour, BACKGROUND);
            assert!(
                ratio >= 7.0,
                "{name} is {ratio:.2}:1 against the background, below the 7:1 bar for emphasised text"
            );
        }
    }

    /// The one place text is drawn *on* a colour rather than on the
    /// background: the product mark and a selected row. Both must stay
    /// readable, which is why they invert rather than tint.
    #[test]
    fn text_on_the_accent_stays_readable() {
        let ratio = contrast(BACKGROUND, ACCENT);
        assert!(
            ratio >= 4.5,
            "text drawn on the accent is {ratio:.2}:1, below the 4.5:1 floor"
        );
    }

    /// Every meaning a view style can carry gets a distinct colour, so two
    /// different states cannot look the same.
    #[test]
    fn the_semantic_roles_do_not_collide() {
        let roles = [
            ("TEXT", TEXT),
            ("SURFACE", SURFACE),
            ("ACCENT", ACCENT),
            ("IDENTITY", IDENTITY),
            ("POSITIVE", POSITIVE),
            ("WARNING", WARNING),
            ("NEGATIVE", NEGATIVE),
        ];
        for (index, (name_a, a)) in roles.iter().enumerate() {
            for (name_b, b) in roles.iter().skip(index + 1) {
                assert_ne!(a, b, "{name_a} and {name_b} are the same colour");
            }
        }
    }

    /// A failure must never be painted as a success, and vice versa: the
    /// view layer's `Red`/`Green` are meanings, not suggestions.
    #[test]
    fn a_failure_and_a_success_do_not_look_alike() {
        let red = paint(ViewStyle::Red).fg;
        let green = paint(ViewStyle::Green).fg;
        assert_eq!(red, Some(NEGATIVE));
        assert_eq!(green, Some(POSITIVE));
        assert_ne!(red, green);
        // And they are distinguishable in luminance, not just in hue, so a
        // colour-blind operator still reads the difference.
        let (lr, lg) = (luminance(NEGATIVE), luminance(POSITIVE));
        assert!(
            (lr - lg).abs() > 0.02,
            "failure and success have near-identical luminance ({lr:.3} vs {lg:.3})"
        );
    }
}
