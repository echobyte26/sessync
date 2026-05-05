//! ANSI style helpers.
//!
//! All functions accept `&str` and return `String`.
//! Output is plain (no escapes) when:
//!   - stdout is not a TTY (`std::io::IsTerminal`)
//!   - `NO_COLOR` env var is set (https://no-color.org)

use owo_colors::OwoColorize;
use std::io::IsTerminal;

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err()
}

pub fn success(s: &str) -> String {
    if use_color() {
        s.green().to_string()
    } else {
        s.to_string()
    }
}

pub fn warn(s: &str) -> String {
    if use_color() {
        s.yellow().to_string()
    } else {
        s.to_string()
    }
}

pub fn error(s: &str) -> String {
    if use_color() {
        s.red().to_string()
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    if use_color() {
        s.dimmed().to_string()
    } else {
        s.to_string()
    }
}

pub fn hint(s: &str) -> String {
    if use_color() {
        s.dimmed().italic().to_string()
    } else {
        s.to_string()
    }
}

pub fn header(s: &str) -> String {
    if use_color() {
        s.bold().to_string()
    } else {
        s.to_string()
    }
}

pub fn value(s: &str) -> String {
    if use_color() {
        s.cyan().to_string()
    } else {
        s.to_string()
    }
}

pub fn key(s: &str) -> String {
    if use_color() {
        s.white().dimmed().to_string()
    } else {
        s.to_string()
    }
}

/// Unicode check mark, styled green when color is available.
pub fn check_ok() -> &'static str {
    "✓"
}

/// Unicode cross, styled red when color is available.
pub fn cross_fail() -> &'static str {
    "✗"
}

/// Right arrow.
pub fn arrow() -> &'static str {
    "→"
}
