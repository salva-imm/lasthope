//! Days and the chain of seals (the streak). Pure logic, no clock and no
//! database: the caller supplies today's date, the memory layer stores the
//! result. The streak only ever changes through `apply_verdict`, i.e. from
//! my own "kept"/"broken" answer and real dates, never from the model.

use std::fmt;

/// A calendar day, as days since 1970-01-01 (proleptic Gregorian).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(pub i64);

impl Day {
    pub fn from_ymd(y: i64, m: u32, d: u32) -> Self {
        // Howard Hinnant's days_from_civil
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = m as i64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Day(era * 146_097 + doe - 719_468)
    }

    pub fn ymd(self) -> (i64, u32, u32) {
        let z = self.0 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
        (y, m, d)
    }

    /// Parse "YYYY-MM-DD".
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s.trim().splitn(3, '-');
        let y = it.next()?.parse().ok()?;
        let m: u32 = it.next()?.parse().ok()?;
        let d: u32 = it.next()?.get(..2)?.parse().ok()?;
        ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| Day::from_ymd(y, m, d))
    }

    pub fn next(self) -> Self {
        Day(self.0 + 1)
    }

    pub fn weekday(self) -> &'static str {
        // 1970-01-01 was a Thursday
        ["Thursday", "Friday", "Saturday", "Sunday", "Monday", "Tuesday", "Wednesday"]
            [self.0.rem_euclid(7) as usize]
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

/// The chain of seals: how many consecutive days of kept oaths, ending on `last`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streak {
    pub count: u32,
    /// The day of the most recent judged oath (kept or broken).
    pub last: Option<Day>,
}

impl Streak {
    /// Record my verdict on the oath that was due on `oath_day`.
    /// - kept, and the previous seal was the day before: the chain grows
    /// - kept, after a gap or a broken oath: a new chain of one
    /// - broken: the chain is gone
    /// A verdict for a day already judged (or earlier) changes nothing.
    pub fn apply_verdict(self, oath_day: Day, kept: bool) -> Streak {
        if self.last.is_some_and(|last| oath_day <= last) {
            return self;
        }
        if !kept {
            return Streak { count: 0, last: Some(oath_day) };
        }
        let count = match self.last {
            Some(last) if last.0 == oath_day.0 - 1 && self.count > 0 => self.count + 1,
            _ => 1,
        };
        Streak { count, last: Some(oath_day) }
    }

    /// The chain as it stands today. Today's oath may still be unjudged, so a
    /// chain ending yesterday is alive; one ending earlier was broken by a
    /// missed day.
    pub fn current(&self, today: Day) -> u32 {
        match self.last {
            Some(last) if today.0 - last.0 <= 1 => self.count,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Day {
        Day::parse(s).unwrap()
    }

    #[test]
    fn days_round_trip_and_weekdays() {
        for s in ["1970-01-01", "2000-02-29", "2026-11-19", "2027-01-01", "1999-12-31"] {
            assert_eq!(d(s).to_string(), s);
        }
        assert_eq!(d("2026-11-20").0 - d("2026-11-19").0, 1);
        assert_eq!(d("2027-01-01").0 - d("2026-12-31").0, 1);
        assert_eq!(d("2026-11-19").weekday(), "Thursday");
        assert_eq!(d("2026-11-23").weekday(), "Monday");
        assert_eq!(Day::parse("2026-11-19 22:10:00"), Some(d("2026-11-19")));
        assert_eq!(Day::parse("nonsense"), None);
        assert_eq!(Day::parse("2026-13-01"), None);
    }

    #[test]
    fn consecutive_kept_oaths_grow_the_chain() {
        let mut s = Streak::default();
        s = s.apply_verdict(d("2026-11-19"), true);
        assert_eq!(s.count, 1);
        s = s.apply_verdict(d("2026-11-20"), true);
        s = s.apply_verdict(d("2026-11-21"), true);
        assert_eq!(s, Streak { count: 3, last: Some(d("2026-11-21")) });
        // crossing a year boundary
        let s = Streak { count: 4, last: Some(d("2026-12-31")) }.apply_verdict(d("2027-01-01"), true);
        assert_eq!(s.count, 5);
    }

    #[test]
    fn broken_oath_resets_and_next_kept_starts_over() {
        let s = Streak { count: 5, last: Some(d("2026-11-19")) };
        let s = s.apply_verdict(d("2026-11-20"), false);
        assert_eq!(s, Streak { count: 0, last: Some(d("2026-11-20")) });
        let s = s.apply_verdict(d("2026-11-21"), true);
        assert_eq!(s.count, 1);
    }

    #[test]
    fn a_gap_starts_a_new_chain() {
        let s = Streak { count: 5, last: Some(d("2026-11-19")) };
        assert_eq!(s.apply_verdict(d("2026-11-22"), true).count, 1);
    }

    #[test]
    fn old_or_repeated_verdicts_change_nothing() {
        let s = Streak { count: 3, last: Some(d("2026-11-21")) };
        assert_eq!(s.apply_verdict(d("2026-11-20"), false), s);
        assert_eq!(s.apply_verdict(d("2026-11-21"), true), s);
        assert_eq!(s.apply_verdict(d("2026-11-21"), false), s);
    }

    #[test]
    fn current_chain_survives_until_a_day_is_missed() {
        let s = Streak { count: 4, last: Some(d("2026-11-20")) };
        assert_eq!(s.current(d("2026-11-20")), 4);
        assert_eq!(s.current(d("2026-11-21")), 4); // today's oath not judged yet
        assert_eq!(s.current(d("2026-11-22")), 0); // the 21st was never sealed
        assert_eq!(Streak::default().current(d("2026-11-22")), 0);
    }
}
