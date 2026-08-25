use anstyle::{AnsiColor, Style};

pub const ACCENT: Style = AnsiColor::Cyan.on_default().bold();
pub const SUCCESS: Style = AnsiColor::Green.on_default().bold();
pub const WARNING: Style = AnsiColor::Yellow.on_default().bold();
pub const ERROR: Style = AnsiColor::Red.on_default().bold();
pub const EMPHASIS: Style = Style::new().bold();
pub const MUTED: Style = Style::new().dimmed();
