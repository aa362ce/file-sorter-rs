use iced::widget::{button, container};
use iced::{Background, Border, Color, Shadow, Theme, Vector};

pub const ACCENT: Color = Color::from_rgb(0.35, 0.55, 0.98);
pub const SUCCESS: Color = Color::from_rgb(0.29, 0.73, 0.45);
pub const WARNING: Color = Color::from_rgb(0.95, 0.66, 0.22);
pub const DANGER: Color = Color::from_rgb(0.91, 0.32, 0.35);
pub const MUTED: Color = Color::from_rgb(0.6, 0.63, 0.7);

/// The app's outermost background -- a shade darker than the surface cards
/// sit on, so cards read as raised rather than blending into the page.
pub fn app_background(_theme: &Theme) -> iced::theme::Style {
    iced::theme::Style { background_color: Color::from_rgb(0.07, 0.08, 0.11), text_color: Color::from_rgb(0.92, 0.93, 0.95) }
}

/// A raised card/panel: rounded corners, a faint border, a soft shadow.
pub fn card(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.11, 0.12, 0.16))),
        border: Border { color: Color::from_rgba(1.0, 1.0, 1.0, 0.06), width: 1.0, radius: 12.0.into() },
        shadow: Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.35), offset: Vector::new(0.0, 2.0), blur_radius: 12.0 },
        text_color: None,
        snap: false,
    }
}

/// A slightly sunken panel used for inner lists/scroll areas inside a card.
pub fn well(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.18))),
        border: Border { color: Color::from_rgba(1.0, 1.0, 1.0, 0.05), width: 1.0, radius: 10.0.into() },
        shadow: Shadow::default(),
        text_color: None,
        snap: false,
    }
}

/// One row inside a group's file/folder list.
pub fn row_even(_theme: &Theme) -> container::Style {
    container::Style { background: Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.015))), ..container::Style::default() }
}

pub fn row_odd(_theme: &Theme) -> container::Style {
    container::Style::default()
}

fn pill(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.16, ..color })),
        text_color: Some(color),
        border: Border { color: Color { a: 0.4, ..color }, width: 1.0, radius: 999.0.into() },
        shadow: Shadow::default(),
        snap: false,
    }
}

pub fn pill_success_bg(_theme: &Theme) -> container::Style {
    pill(SUCCESS)
}

pub fn pill_warning_bg(_theme: &Theme) -> container::Style {
    pill(WARNING)
}

pub fn pill_danger_bg(_theme: &Theme) -> container::Style {
    pill(DANGER)
}

pub fn pill_muted(_theme: &Theme) -> container::Style {
    pill(MUTED)
}

pub fn pill_accent_bg(_theme: &Theme) -> container::Style {
    pill(ACCENT)
}

/// A borderless, low-emphasis button used for tabs and chip "x" buttons.
pub fn ghost_button(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::text(theme, status);
    if matches!(status, button::Status::Hovered) {
        style.background = Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.06)));
    }
    style
}

/// The active tab in the top tab bar.
pub fn tab_active(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = button::primary(theme, status);
    style.border.radius = 8.0.into();
    style
}

/// An inactive tab in the top tab bar.
pub fn tab_inactive(theme: &Theme, status: button::Status) -> button::Style {
    let mut style = ghost_button(theme, status);
    style.border.radius = 8.0.into();
    style
}
