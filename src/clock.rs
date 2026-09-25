//! Local wall-clock time without a chrono dependency.

use std::time::{SystemTime, UNIX_EPOCH};

pub struct LocalTime {
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
}

impl LocalTime {
    pub fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as libc::time_t)
            .unwrap_or(0);
        // glibc loads the timezone once per process: after a timezone change,
        // the time is right again on the next restart (journald's own
        // timestamps are always right)
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&secs, &mut tm) };
        Self {
            year: tm.tm_year + 1900,
            month: tm.tm_mon + 1,
            day: tm.tm_mday,
            hour: tm.tm_hour,
            minute: tm.tm_min,
            second: tm.tm_sec,
        }
    }

    /// `HH:MM:SS`
    pub fn time(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// `YYYY-MM-DDTHH:MM:SS`
    pub fn iso(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{}",
            self.year,
            self.month,
            self.day,
            self.time()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_zero_padded() {
        let t = LocalTime {
            year: 2026,
            month: 9,
            day: 5,
            hour: 8,
            minute: 3,
            second: 7,
        };
        assert_eq!(t.time(), "08:03:07");
        assert_eq!(t.iso(), "2026-09-05T08:03:07");
    }
}
