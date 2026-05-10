//! `sessync logs` — print recent push outcomes from the queue.

use crate::queue::Queue;
use crate::ui::style;
use anyhow::{bail, Result};
use chrono::{DateTime, Local, TimeZone, Utc};

/// Parse a human-readable duration string into seconds.
///
/// Accepted formats: `<N>s`, `<N>m`, `<N>h`, `<N>d` where N is a positive integer.
/// Examples: `"30s"`, `"5m"`, `"1h"`, `"2d"`.
pub fn parse_since(s: &str) -> Result<i64> {
    if s.is_empty() {
        bail!("duration string is empty");
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let n: i64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {:?}: expected format like 30s, 5m, 1h, 2d", s))?;
    if n <= 0 {
        bail!("duration must be positive, got {:?}", s);
    }
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => bail!("unknown unit {:?} in {:?}; expected s, m, h, or d", unit, s),
    };
    Ok(secs)
}

pub fn run(limit: usize, since: Option<String>, failed: bool) -> Result<()> {
    let since_secs: Option<i64> = match since {
        Some(ref s) => Some(parse_since(s)?),
        None => None,
    };

    // Fetch more than `limit` so filters can reduce the set before applying the cap.
    // Using OUTCOME_CAP (100) as an upper bound is fine — it's what the DB retains.
    let fetch_limit = 100usize;
    let raw_outcomes = match Queue::open_default() {
        Ok(q) => match q.recent_outcomes(fetch_limit) {
            Ok(v) => v,
            Err(_) => vec![],
        },
        Err(_) => vec![],
    };

    let now_ts = Utc::now().timestamp();

    let outcomes: Vec<_> = raw_outcomes
        .into_iter()
        .filter(|o| {
            if failed && o.success {
                return false;
            }
            if let Some(duration) = since_secs {
                if now_ts - o.at > duration {
                    return false;
                }
            }
            true
        })
        .take(limit)
        .collect();

    if outcomes.is_empty() {
        if failed || since_secs.is_some() {
            println!("No matching entries.");
        } else {
            println!("No push history yet. Run sessync push to get started.");
        }
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

    // ── parse_since ───────────────────────────────────────────────────────────

    #[test]
    fn parse_since_handles_common_formats() {
        assert_eq!(parse_since("30s").unwrap(), 30);
        assert_eq!(parse_since("5m").unwrap(), 300);
        assert_eq!(parse_since("1h").unwrap(), 3600);
        assert_eq!(parse_since("2d").unwrap(), 172800);
        assert_eq!(parse_since("1m").unwrap(), 60);
    }

    #[test]
    fn parse_since_rejects_garbage() {
        assert!(parse_since("").is_err(), "empty string should fail");
        assert!(parse_since("abc").is_err(), "no numeric prefix");
        assert!(parse_since("5x").is_err(), "unknown unit");
        assert!(parse_since("0m").is_err(), "zero duration");
        assert!(parse_since("-1h").is_err(), "negative duration");
        assert!(parse_since("5").is_err(), "no unit suffix");
    }

    // ── format_relative ───────────────────────────────────────────────────────

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
