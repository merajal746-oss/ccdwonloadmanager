//! Download window scheduler (cf. XDM `DownloadSchedule`).
//!
//! A weekly bitmask plus a daily time window in **local** time. The CLI
//! refuses (or waits out) out-of-window starts; the GUI Start buttons do
//! the same. Times are minutes since midnight so the shape stays serde
//! friendly without extra nacht-time crates beyond chrono.

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};

/// Bit `i` (0 = Monday … 6 = Sunday) means scheduled that day.
pub const ALL_DAYS: u8 = 0x7F;
pub const WEEKDAYS: u8 = 0x1F;
pub const WEEKEND: u8 = 0x60;

/// Run downloads only inside this weekly window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Weekday bitmask, bit0 = Monday.
    pub days: u8,
    /// Window start, minutes since midnight.
    pub start_minutes: u16,
    /// Window end, minutes since midnight. Earlier than start = overnight.
    pub end_minutes: u16,
}

impl Schedule {
    /// Daily 01:00–06:00 night window (kind to daytime bandwidth).
    pub fn nightly() -> Self {
        Self {
            days: ALL_DAYS,
            start_minutes: 60,
            end_minutes: 360,
        }
    }

    /// Whether `moment` falls inside the window.
    pub fn allows(&self, moment: &NaiveDateTime) -> bool {
        let day_bit = 1u8 << moment.weekday().num_days_from_monday();
        if self.days & day_bit == 0 {
            return false;
        }
        let now = moment.hour() as u16 * 60 + moment.minute() as u16;
        let (start, end) = (self.start_minutes.min(1439), self.end_minutes.min(1439));
        if start <= end {
            now >= start && now < end
        } else {
            now >= start || now < end
        }
    }

    /// Whether downloading may start right now (local time).
    pub fn allows_now(&self) -> bool {
        self.allows(&chrono::Local::now().naive_local())
    }

    /// Parse `HH:MM` (24h) to minutes since midnight.
    pub fn parse_hhmm(s: &str) -> Option<u16> {
        let (hours, minutes) = s.trim().split_once(':')?;
        let hours: u16 = hours.trim().parse().ok()?;
        let minutes: u16 = minutes.trim().parse().ok()?;
        if hours < 24 && minutes < 60 {
            Some(hours * 60 + minutes)
        } else {
            None
        }
    }

    /// Short human form, e.g. `weekdays 01:00–06:00`.
    pub fn describe(&self) -> String {
        let days = if self.days == ALL_DAYS {
            "daily".to_string()
        } else if self.days == WEEKDAYS {
            "weekdays".to_string()
        } else if self.days == WEEKEND {
            "weekends".to_string()
        } else {
            const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            let mut parts = Vec::new();
            for (i, name) in NAMES.iter().enumerate() {
                if self.days & (1u8 << i) != 0 {
                    parts.push(*name);
                }
            }
            if parts.is_empty() {
                "never".to_string()
            } else {
                parts.join(",")
            }
        };
        format!(
            "{days} {:02}:{:02}–{:02}:{:02}",
            self.start_minutes / 60,
            self.start_minutes % 60,
            self.end_minutes / 60,
            self.end_minutes % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn nightly_window() {
        // 2026-09-07 is a Monday.
        let sched = Schedule::nightly();
        assert!(sched.allows(&at(2026, 9, 7, 2, 0)));
        assert!(!sched.allows(&at(2026, 9, 7, 12, 0)));
        assert!(!sched.allows(&at(2026, 9, 7, 6, 0))); // end exclusive
        assert!(sched.allows(&at(2026, 9, 7, 1, 0)));
    }

    #[test]
    fn weekday_mask() {
        let sched = Schedule {
            days: WEEKDAYS,
            start_minutes: 0,
            end_minutes: 1439,
        };
        assert!(sched.allows(&at(2026, 9, 9, 12, 0))); // Wednesday
        assert!(!sched.allows(&at(2026, 9, 12, 12, 0))); // Saturday
    }

    #[test]
    fn overnight_window() {
        let sched = Schedule {
            days: ALL_DAYS,
            start_minutes: 1320, // 22:00
            end_minutes: 360,    // 06:00
        };
        assert!(sched.allows(&at(2026, 9, 7, 23, 30)));
        assert!(sched.allows(&at(2026, 9, 8, 5, 59)));
        assert!(!sched.allows(&at(2026, 9, 8, 12, 0)));
    }

    #[test]
    fn describe_shapes() {
        let nightly = Schedule::nightly().describe();
        assert!(nightly.starts_with("daily 01:00"), "got {nightly}");
        let never = Schedule { days: 0, start_minutes: 0, end_minutes: 60 }.describe();
        assert!(never.starts_with("never 00:00"), "got {never}");
    }

    #[test]
    fn hhmm_parses() {
        assert_eq!(Schedule::parse_hhmm("01:00"), Some(60));
        assert_eq!(Schedule::parse_hhmm("23:59"), Some(1439));
        assert_eq!(Schedule::parse_hhmm(" 6:05 "), Some(365));
        assert_eq!(Schedule::parse_hhmm("24:00"), None);
        assert_eq!(Schedule::parse_hhmm("12:60"), None);
        assert_eq!(Schedule::parse_hhmm("nope"), None);
    }
}
}
