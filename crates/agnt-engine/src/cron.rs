use crate::task::CronSchedule;
use chrono::{Datelike, NaiveDateTime, Timelike, Utc};
use std::time::Duration;

/// Parses a 5-field cron expression and computes the next fire time.
/// Supports: `*`, specific values, ranges (1-5), steps (*/5), lists (1,3,5).
/// Fields: minute hour day-of-month month day-of-week
pub fn next_fire(schedule: &CronSchedule) -> Option<Duration> {
    let fields: Vec<&str> = schedule.expression.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }

    let now = Utc::now().naive_utc();
    let minute_set = parse_field(fields[0], 0, 59)?;
    let hour_set = parse_field(fields[1], 0, 23)?;
    let dom_set = parse_field(fields[2], 1, 31)?;
    let month_set = parse_field(fields[3], 1, 12)?;
    let dow_set = parse_field(fields[4], 0, 6)?;

    // Brute-force search forward from now, up to 366 days.
    let mut candidate = now + chrono::Duration::minutes(1);
    // Round down to the start of the minute.
    candidate = candidate
        .date()
        .and_hms_opt(candidate.hour(), candidate.minute(), 0)?;

    for _ in 0..(366 * 24 * 60) {
        let m = candidate.minute();
        let h = candidate.hour();
        let dom = candidate.day();
        let month = candidate.month();
        let dow = candidate.weekday().num_days_from_sunday(); // 0=Sun

        if minute_set.contains(&m)
            && hour_set.contains(&h)
            && dom_set.contains(&dom)
            && month_set.contains(&month)
            && dow_set.contains(&dow)
        {
            let delta = candidate - now;
            return Some(delta.to_std().ok()?);
        }

        candidate += chrono::Duration::minutes(1);
    }

    None
}

/// Parse a single cron field into a set of valid values.
fn parse_field(field: &str, min: u32, max: u32) -> Option<Vec<u32>> {
    let mut values = Vec::new();

    for part in field.split(',') {
        if part == "*" {
            return Some((min..=max).collect());
        } else if let Some(step_str) = part.strip_prefix("*/") {
            let step: u32 = step_str.parse().ok()?;
            if step == 0 {
                return None;
            }
            let mut v = min;
            while v <= max {
                values.push(v);
                v += step;
            }
        } else if part.contains('-') {
            let mut parts = part.split('-');
            let start: u32 = parts.next()?.parse().ok()?;
            let end: u32 = parts.next()?.parse().ok()?;
            // Reject ranges that exceed the field's valid bounds before
            // allocating a Vec — an unchecked range like "0-999999999"
            // would cause a multi-gigabyte allocation (M1).
            if start < min || end > max || start > end {
                return None;
            }
            for v in start..=end {
                values.push(v);
            }
        } else {
            let v: u32 = part.parse().ok()?;
            // Reject single values outside the valid field range (M1).
            if v < min || v > max {
                return None;
            }
            values.push(v);
        }
    }

    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_minute() {
        let sched = CronSchedule {
            expression: "* * * * *".into(),
            timezone: "UTC".into(),
            run_on_start: false,
            max_concurrent: 1,
        };
        let next = next_fire(&sched).unwrap();
        assert!(next.as_secs() <= 60);
    }

    #[test]
    fn every_5_minutes() {
        let sched = CronSchedule {
            expression: "*/5 * * * *".into(),
            timezone: "UTC".into(),
            run_on_start: false,
            max_concurrent: 1,
        };
        let next = next_fire(&sched).unwrap();
        assert!(next.as_secs() <= 300);
    }

    #[test]
    fn parse_field_star() {
        let values = parse_field("*", 0, 59).unwrap();
        assert_eq!(values.len(), 60);
    }

    #[test]
    fn parse_field_step() {
        let values = parse_field("*/15", 0, 59).unwrap();
        assert_eq!(values, vec![0, 15, 30, 45]);
    }

    #[test]
    fn parse_field_range() {
        let values = parse_field("1-5", 0, 6).unwrap();
        assert_eq!(values, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_field_list() {
        let values = parse_field("1,3,5", 0, 6).unwrap();
        assert_eq!(values, vec![1, 3, 5]);
    }
}
