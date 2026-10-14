//! Terminal colour. Output is coloured only when it goes to a terminal, never with
//! `NO_COLOR` set or `TERM=dumb`, and always with `CLICOLOR_FORCE` set (to anything but `0`).

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::OnceLock;

use owo_colors::OwoColorize;

#[derive(Debug, Clone, Copy)]
pub enum Style {
    Red,
    Green,
    Yellow,
    Cyan,
    Bold,
    /// Bold bright white, for workspace names.
    Name,
}

/// `text` in `style`, if standard output takes colour.
pub fn paint(text: impl Display, style: Style) -> String {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    apply(
        *STDOUT.get_or_init(|| wanted(std::io::stdout().is_terminal())),
        text,
        style,
    )
}

/// `text` in `style`, if standard error takes colour.
pub fn paint_stderr(text: impl Display, style: Style) -> String {
    static STDERR: OnceLock<bool> = OnceLock::new();
    apply(
        *STDERR.get_or_init(|| wanted(std::io::stderr().is_terminal())),
        text,
        style,
    )
}

fn wanted(terminal: bool) -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if set("NO_COLOR") {
        return false;
    }
    if set("CLICOLOR_FORCE") && std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
        return true;
    }
    terminal && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

fn apply(colour: bool, text: impl Display, style: Style) -> String {
    if !colour {
        return text.to_string();
    }
    match style {
        Style::Red => text.red().to_string(),
        Style::Green => text.green().to_string(),
        Style::Yellow => text.yellow().to_string(),
        Style::Cyan => text.cyan().to_string(),
        Style::Bold => text.bold().to_string(),
        Style::Name => text.bold().bright_white().to_string(),
    }
}
