//! `sessync logs` — print recent push outcomes from the queue.

use crate::queue::Queue;
use crate::ui::style;
use anyhow::Result;
use chrono::{DateTime, Local, TimeZone, Utc};

pub fn run(limit: usize) -> Result<()> {
    let outcomes = match Queue::open_default() {
        Ok(q) => match q.recent_outcomes(limit) {
            Ok(v) => v,
            Err(_) => vec![],
        },
        Err(_) => vec![],
    };

    if outcomes.is_empty() {
        println!("No push history yet. Run sessync push to get started.");
        return Ok(());
    }

    // Build time strings first so we know the max width for alignment.
    let time_strings: Vec<String> = outcomes
        .iter()
        .map(|o| {
            let dt = Utc.timestamp_opt(o.at, 0).single().unwrap_or_else(Utc::now);
            format_relative(Utc::now(), dt)
        })
        .collect();

    let max_time_len = time_strings.iter().map(|s| s.len()).max().unwrap_or(0);

    for (o, time_str) in outcomes.iter().zip(time_strings.iter()) {
        let padded_time = format!("{:<width$}", time_str, width = max_time_len);
        println!("{}", format_outcome_line(&padded_time, o.success, &o.summary));
    }

    Ok(())
}

/// Format a UTC timestamp relative to `now`.
/// - < 60s  → "just now"
/// - < 60m  → "N minute(s) ago"
/// - < 24h  → "N hour(s) ago"
/// - else   → "YYYY-MM-DD HH:MM" in the user's local timezone
pub fn format_relative(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let diff = now.signed_duration_since(then);
    let secs = diff.num_seconds();

    if secs < 60 {
        return "just now".to_string();
    }
    let mins = diff.num_minutes();
    if mins < 60 {
        return format!("{} {} ago", mins, if mins == 1 { "minute" } else { "minutes" });
    }
    let hours = diff.num_hours();
    if hours < 24 {
        return format!("{} {} ago", hours, if hours == 1 { "hour" } else { "hours" });
    }
    then.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

/// Format a single outcome line with marker + padded time column + summary.
pub fn format_outcome_line(time_col: &str, success: bool, summary: &str) -> String {
    let marker = if success {
        style::success(style::check_ok())
    } else {
        style::error(style::cross_fail())
    };
    format!("{} {}   {}", marker, time_col, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn format_relative_just_now() {
        let now = Utc::now();
        let then = now - Duration::seconds(30);
        assert_eq!(format_relative(now, then), "just now");
    }

    #[test]
    fn format_relative_just_now_boundary() {
        let now = Utc::now();
        let then = now - Duration::seconds(59);
        assert_eq!(format_relative(now, then), "just now");
    }

    #[test]
    fn format_relative_minutes() {
        let now = Utc::now();
        let then = now - Duration::minutes(5);
        assert_eq!(format_relative(now, then), "5 minutes ago");
    }

    #[test]
    fn format_relative_one_minute() {
        let now = Utc::now();
        let then = now - Duration::minutes(1);
        assert_eq!(format_relative(now, then), "1 minute ago");
    }

    #[test]
    fn format_relative_hours() {
        let now = Utc::now();
        let then = now - Duration::hours(2);
        assert_eq!(format_relative(now, then), "2 hours ago");
    }

    #[test]
    fn format_relative_one_hour() {
        let now = Utc::now();
        let then = now - Duration::hours(1);
        assert_eq!(format_relative(now, then), "1 hour ago");
    }

    #[test]
    fn format_relative_falls_back_to_absolute_after_24h() {
        let now = Utc::now();
        let then = now - Duration::hours(25);
        let result = format_relative(now, then);
        // Should be YYYY-MM-DD HH:MM format, not "ago"
        assert!(!result.contains("ago"), "got: {result}");
        assert!(result.contains('-'), "expected date format, got: {result}");
    }
}
