//! Toaba's brand colours, from `docs/brand.md` in the landing page repo. Tangerine is a fill: text
//! on it is Ink, and text elsewhere keeps the terminal's own colour, light or dark. Terminals
//! without truecolor get the nearest of their 256 colours.

use console::Style;
use dialoguer::theme::ColorfulTheme;

const TANGERINE: (u8, u8, u8) = (0xFD, 0x7C, 0x39);
const TANGERINE_256: u8 = 209;
const INK: (u8, u8, u8) = (0x29, 0x2B, 0x35);
const INK_256: u8 = 236;

/// Ink on Tangerine, for what a person reads off the screen, such as a one-time code.
pub fn badge() -> Style {
    let style = if console::true_colors_enabled_stderr() {
        Style::new().true_color(INK.0, INK.1, INK.2).on_true_color(
            TANGERINE.0,
            TANGERINE.1,
            TANGERINE.2,
        )
    } else {
        Style::new().color256(INK_256).on_color256(TANGERINE_256)
    };
    style.bold().for_stderr()
}

/// The empty part of a bar, dimmed in the terminal's own colour.
pub fn track() -> Style {
    Style::new().dim().for_stderr()
}

/// The terminal's own yellow, for the ! of a warning.
pub fn warning() -> Style {
    Style::new().yellow().for_stderr()
}

/// The terminal's own red, for the ✗ of a failed step.
pub fn failure() -> Style {
    Style::new().red().for_stderr()
}

/// Tangerine, for symbols only, such as the spinner, a bar's fill and the check mark.
pub fn accent() -> Style {
    let style = if console::true_colors_enabled_stderr() {
        Style::new().true_color(TANGERINE.0, TANGERINE.1, TANGERINE.2)
    } else {
        Style::new().color256(TANGERINE_256)
    };
    style.for_stderr()
}

/// The look of a picker: Tangerine marks, and the terminal's own colour for the text.
pub fn picker() -> ColorfulTheme {
    ColorfulTheme {
        prompt_prefix: accent().apply_to("?".to_string()),
        success_prefix: accent().apply_to("✓".to_string()),
        active_item_prefix: accent().apply_to("❯".to_string()),
        active_item_style: Style::new().bold().for_stderr(),
        values_style: Style::new().bold().for_stderr(),
        ..ColorfulTheme::default()
    }
}
