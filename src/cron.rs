//! Schedule cadences: fixed intervals and five-field cron expressions, with
//! upstream's semantics (all five fields must match; no timezone means UTC).
//! IANA zones are read from the system tz database (TZif files), no crate needed.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Cadence {
    Every {
        #[serde(rename = "everyMs")]
        every_ms: u64,
    },
    Cron {
        expression: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

/// 100 years: keeps every computed timestamp inside what RFC 3339 can print.
const MAX_EVERY_MS: u64 = 3_155_760_000_000;
const SEARCH_MINUTES: i64 = 366 * 24 * 60;

impl Cadence {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Cadence::Every { every_ms } if *every_ms == 0 || *every_ms > MAX_EVERY_MS => Err("everyMs must be between 1 and 100 years".into()),
            Cadence::Every { .. } => Ok(()),
            Cadence::Cron { expression, timezone } => {
                Cron::parse(expression)?;
                Zone::load(timezone.as_deref()).map(|_| ())
            }
        }
    }

    /// The first run strictly after both `slot` and `now` (milliseconds since
    /// the epoch). For an interval that stays on the `slot` grid.
    pub fn next_run(&self, slot: i64, now: i64) -> Result<i64, String> {
        let after = slot.max(now);
        match self {
            Cadence::Every { every_ms } => {
                let every = *every_ms as i64;
                Ok(slot + every * ((after - slot) / every + 1))
            }
            Cadence::Cron { expression, timezone } => {
                let cron = Cron::parse(expression)?;
                let zone = Zone::load(timezone.as_deref())?;
                let mut t = (after.div_euclid(60_000) + 1) * 60_000;
                for _ in 0..SEARCH_MINUTES {
                    if cron.matches(&zone.local(t.div_euclid(1000))) {
                        return Ok(t);
                    }
                    t += 60_000;
                }
                Err(format!("Unable to compute next run time for cron expression: {expression}"))
            }
        }
    }
}

struct Local {
    minute: u32,
    hour: u32,
    day: u32,
    month: u32,
    weekday: u32,
}

struct Cron {
    minute: u64,
    hour: u64,
    day: u64,
    month: u64,
    weekday: u64,
}

fn digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A bit set of the values a field allows: `*`, `a-b`, `n`, lists, and `/step` on `*` and ranges.
fn parse_field(source: &str, min: u32, max: u32, name: &str) -> Result<u64, String> {
    let mut bits = 0u64;
    for part in source.trim().split(',') {
        let part = part.trim();
        let mut pieces = part.split('/');
        let base = pieces.next().unwrap_or("");
        let step = match (pieces.next(), pieces.next()) {
            (None, _) => 1,
            (Some(s), None) => {
                let s = s.trim();
                digits(s).filter(|n| *n > 0 && n.to_string() == s).ok_or_else(|| format!("Invalid cron {name} step"))?
            }
            _ => return Err(format!("Invalid cron {name} step")),
        };
        let (start, end) = if base.is_empty() {
            return Err(format!("Invalid cron {name} field"));
        } else if base == "*" {
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            let (a, b) = digits(a).zip(digits(b)).ok_or_else(|| format!("Invalid cron {name} value"))?;
            if a > b || a < min || b > max {
                return Err(format!("Invalid cron {name} range"));
            }
            (a, b)
        } else {
            let v = digits(base).filter(|v| (min..=max).contains(v)).ok_or_else(|| format!("Invalid cron {name} value"))?;
            bits |= 1 << v;
            continue;
        };
        for v in (start..=end).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

impl Cron {
    fn parse(expression: &str) -> Result<Cron, String> {
        let parts: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = parts[..] else {
            return Err("Cron expressions must have 5 fields".into());
        };
        Ok(Cron {
            minute: parse_field(minute, 0, 59, "minute")?,
            hour: parse_field(hour, 0, 23, "hour")?,
            day: parse_field(day, 1, 31, "day-of-month")?,
            month: parse_field(month, 1, 12, "month")?,
            weekday: parse_field(weekday, 0, 6, "day-of-week")?,
        })
    }

    fn matches(&self, t: &Local) -> bool {
        let has = |bits: u64, v: u32| bits >> v & 1 == 1;
        has(self.minute, t.minute) && has(self.hour, t.hour) && has(self.day, t.day) && has(self.month, t.month) && has(self.weekday, t.weekday)
    }
}

/// Days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((i64::from(month) + 9) % 12) + 2) / 5 + i64::from(day) - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Time zone: UTC, or a zone from the system tz database.
enum Zone {
    Utc,
    Tz(Tzif),
}

impl Zone {
    fn load(name: Option<&str>) -> Result<Zone, String> {
        let invalid = || format!("Invalid cron time zone: {}", name.unwrap_or_default());
        let Some(name) = name else { return Ok(Zone::Utc) };
        if matches!(name, "UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z") {
            return Ok(Zone::Utc);
        }
        let plain = !name.is_empty() && !name.starts_with('/') && !name.contains("..") && name.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c));
        if !plain {
            return Err(invalid());
        }
        let dir = std::env::var("TZDIR").unwrap_or_else(|_| "/usr/share/zoneinfo".into());
        std::fs::read(std::path::Path::new(&dir).join(name)).ok().and_then(|bytes| Tzif::parse(&bytes)).map(Zone::Tz).ok_or_else(invalid)
    }

    fn local(&self, t: i64) -> Local {
        let secs = t + match self {
            Zone::Utc => 0,
            Zone::Tz(tz) => i64::from(tz.offset(t)),
        };
        let days = secs.div_euclid(86_400);
        let of_day = secs.rem_euclid(86_400) as u32;
        let (_, month, day) = civil_from_days(days);
        Local { minute: of_day % 3600 / 60, hour: of_day / 3600, day, month, weekday: (days + 4).rem_euclid(7) as u32 }
    }
}

/// A parsed TZif file: its transitions, then the POSIX rule that covers later times.
struct Tzif {
    transitions: Vec<i64>,
    kinds: Vec<u8>,
    offsets: Vec<i32>,
    rule: Option<Posix>,
}

impl Tzif {
    fn parse(b: &[u8]) -> Option<Tzif> {
        if b.get(..4)? != b"TZif" {
            return None;
        }
        // Counts in header order: isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt.
        let counts = |at: usize| -> Option<[usize; 6]> {
            let mut c = [0; 6];
            for (i, slot) in c.iter_mut().enumerate() {
                *slot = u32::from_be_bytes(b.get(at + 20 + 4 * i..at + 24 + 4 * i)?.try_into().ok()?) as usize;
            }
            Some(c)
        };
        let v2 = *b.get(4)? >= b'2';
        let (mut at, mut c, mut size) = (0, counts(0)?, 4);
        if v2 {
            at = 44 + size * c[3] + c[3] + 6 * c[4] + c[5] + (size + 4) * c[2] + c[1] + c[0];
            c = counts(at)?;
            size = 8;
        }
        let [isut, isstd, leaps, times, types, chars] = c;
        let mut p = at + 44;
        let transitions = (0..times)
            .map(|i| {
                let s = b.get(p + i * size..p + (i + 1) * size)?;
                Some(if size == 8 { i64::from_be_bytes(s.try_into().ok()?) } else { i64::from(i32::from_be_bytes(s.try_into().ok()?)) })
            })
            .collect::<Option<Vec<i64>>>()?;
        p += times * size;
        let kinds = b.get(p..p + times)?.to_vec();
        p += times;
        let offsets = (0..types).map(|i| Some(i32::from_be_bytes(b.get(p + 6 * i..p + 6 * i + 4)?.try_into().ok()?))).collect::<Option<Vec<i32>>>()?;
        p += 6 * types + chars + (size + 4) * leaps + isstd + isut;
        if offsets.is_empty() || kinds.iter().any(|k| usize::from(*k) >= offsets.len()) {
            return None;
        }
        let rule = if v2 { b.get(p..).and_then(|f| f.strip_prefix(b"\n")).and_then(|f| f.split(|c| *c == b'\n').next()).and_then(|f| std::str::from_utf8(f).ok()).and_then(Posix::parse) } else { None };
        Some(Tzif { transitions, kinds, offsets, rule })
    }

    /// Seconds east of UTC at `t`.
    fn offset(&self, t: i64) -> i32 {
        let passed = self.transitions.partition_point(|x| *x <= t);
        if passed == 0 {
            return self.offsets[0];
        }
        match &self.rule {
            Some(rule) if passed == self.transitions.len() => rule.offset(t),
            _ => self.offsets[usize::from(self.kinds[passed - 1])],
        }
    }
}

/// `Mm.w.d/time`: weekday `d` of week `w` (5 = last) of month `m`, at `time` seconds into the day.
struct Rule {
    month: u32,
    week: i64,
    weekday: i64,
    time: i64,
}

impl Rule {
    fn local_secs(&self, year: i64) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        let next_month = if self.month == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, self.month + 1, 1) };
        let mut day = first + (self.weekday - (first + 4).rem_euclid(7)).rem_euclid(7) + (self.week - 1) * 7;
        if day >= next_month {
            day -= 7;
        }
        day * 86_400 + self.time
    }
}

/// A POSIX TZ string such as `EST5EDT,M3.2.0,M11.1.0`. Only the `M` rule form is read.
struct Posix {
    std: i32,
    dst: Option<(i32, Rule, Rule)>,
}

struct Scan<'a>(&'a [u8]);

impl Scan<'_> {
    fn eat(&mut self, c: u8) -> bool {
        let hit = self.0.first() == Some(&c);
        if hit {
            self.0 = &self.0[1..];
        }
        hit
    }

    fn name(&mut self) -> Option<()> {
        if self.eat(b'<') {
            while !self.eat(b'>') {
                self.0 = self.0.get(1..)?;
            }
            return Some(());
        }
        let n = self.0.iter().take_while(|c| c.is_ascii_alphabetic()).count();
        self.0 = &self.0[n..];
        (n >= 3).then_some(())
    }

    fn num(&mut self) -> Option<i64> {
        let n = self.0.iter().take_while(|c| c.is_ascii_digit()).count();
        let v = std::str::from_utf8(&self.0[..n]).ok()?.parse().ok()?;
        self.0 = &self.0[n..];
        Some(v)
    }

    /// `[+-]hh[:mm[:ss]]` in seconds.
    fn hms(&mut self) -> Option<i64> {
        let sign = if self.eat(b'-') { -1 } else { self.eat(b'+'); 1 };
        let mut secs = self.num()? * 3600;
        if self.eat(b':') {
            secs += self.num()? * 60;
            if self.eat(b':') {
                secs += self.num()?;
            }
        }
        Some(sign * secs)
    }

    fn rule(&mut self) -> Option<Rule> {
        if !self.eat(b'M') {
            return None;
        }
        let month = self.num()?;
        if !self.eat(b'.') {
            return None;
        }
        let week = self.num()?;
        if !self.eat(b'.') {
            return None;
        }
        let weekday = self.num()?;
        let time = if self.eat(b'/') { self.hms()? } else { 7200 };
        ((1..=12).contains(&month) && (1..=5).contains(&week) && (0..=6).contains(&weekday)).then_some(Rule { month: month as u32, week, weekday, time })
    }
}

impl Posix {
    fn parse(s: &str) -> Option<Posix> {
        let mut scan = Scan(s.as_bytes());
        scan.name()?;
        let std = -scan.hms()? as i32;
        if scan.0.is_empty() {
            return Some(Posix { std, dst: None });
        }
        scan.name()?;
        let dst = if scan.0.first().is_some_and(|c| *c != b',') { -scan.hms()? as i32 } else { std + 3600 };
        if !scan.eat(b',') {
            return None;
        }
        let start = scan.rule()?;
        if !scan.eat(b',') {
            return None;
        }
        let end = scan.rule()?;
        Some(Posix { std, dst: Some((dst, start, end)) })
    }

    fn offset(&self, t: i64) -> i32 {
        let Some((dst, start, end)) = &self.dst else { return self.std };
        let year = civil_from_days((t + i64::from(self.std)).div_euclid(86_400)).0;
        let begin = start.local_secs(year) - i64::from(self.std);
        let finish = end.local_secs(year) - i64::from(*dst);
        let in_dst = if begin < finish { (begin..finish).contains(&t) } else { t >= begin || t < finish };
        if in_dst {
            *dst
        } else {
            self.std
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
        (days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60) * 1000
    }

    fn cron(expression: &str, timezone: Option<&str>) -> Cadence {
        Cadence::Cron { expression: expression.into(), timezone: timezone.map(String::from) }
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for days in [-800_000, -1, 0, 59, 60, 11_016, 19_782, 800_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn every_stays_on_its_grid_and_skips_missed_slots() {
        let every = Cadence::Every { every_ms: 1000 };
        assert_eq!(every.next_run(5_000, 5_000), Ok(6_000));
        assert_eq!(every.next_run(5_000, 8_500), Ok(9_000));
        assert_eq!(every.next_run(5_000, 9_000), Ok(10_000));
        assert_eq!(every.next_run(5_000, 1_000), Ok(6_000));
    }

    #[test]
    fn cron_steps_and_lists() {
        let from = ms(2024, 1, 1, 0, 7);
        assert_eq!(cron("*/15 * * * *", None).next_run(from, from), Ok(ms(2024, 1, 1, 0, 15)));
        assert_eq!(cron("5,20-22 3 * * *", None).next_run(from, from), Ok(ms(2024, 1, 1, 3, 5)));
        let from = ms(2024, 1, 1, 3, 5);
        assert_eq!(cron("5,20-22 3 * * *", None).next_run(from, from), Ok(ms(2024, 1, 1, 3, 20)));
        let from = ms(2024, 1, 1, 3, 22);
        assert_eq!(cron("5,20-22 3 * * *", None).next_run(from, from), Ok(ms(2024, 1, 2, 3, 5)));
        assert_eq!(cron("10-20/5 * * * *", None).next_run(ms(2024, 1, 1, 0, 10), 0), Ok(ms(2024, 1, 1, 0, 15)));
    }

    #[test]
    fn cron_is_strictly_after() {
        let at = ms(2024, 1, 1, 9, 0);
        assert_eq!(cron("0 9 * * *", None).next_run(at, at), Ok(ms(2024, 1, 2, 9, 0)));
        assert_eq!(cron("* * * * *", None).next_run(at + 30_000, at + 30_000), Ok(at + 60_000));
    }

    #[test]
    fn cron_weekdays_and_both_day_fields_must_match() {
        let friday = ms(2024, 1, 5, 9, 0);
        assert_eq!(cron("0 9 * * 1-5", None).next_run(friday, friday), Ok(ms(2024, 1, 8, 9, 0)));
        let from = ms(2024, 1, 1, 0, 0);
        assert_eq!(cron("0 0 13 * 5", None).next_run(from, from), Ok(ms(2024, 9, 13, 0, 0)));
    }

    #[test]
    fn cron_search_is_bounded_to_a_year() {
        let from = ms(2023, 3, 1, 0, 0);
        assert_eq!(cron("0 0 29 2 *", None).next_run(from, from), Ok(ms(2024, 2, 29, 0, 0)));
        let from = ms(2024, 3, 1, 0, 0);
        assert!(cron("0 0 29 2 *", None).next_run(from, from).unwrap_err().starts_with("Unable to compute next run time"));
    }

    #[test]
    fn cron_rejects_what_upstream_rejects() {
        for bad in ["* * * *", "* * * * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "* * * 13 *", "* * * * 7", "*/0 * * * *", "*/05 * * * *", "5-1 * * * *", "1,,2 * * * *", "1/2/3 * * * *", "a * * * *", "-1 * * * *", "1- * * * *", ""] {
            assert!(cron(bad, None).validate().is_err(), "{bad}");
        }
        for good in ["* * * * *", "0 0 1 1 *", " 0  9 * * 1-5 ", "*/5 0-6/2 1,15 */3 0,6", "5/2 * * * *"] {
            assert_eq!(cron(good, None).validate(), Ok(()), "{good}");
        }
    }

    #[test]
    fn a_single_value_with_a_step_matches_only_itself() {
        let from = ms(2024, 1, 1, 0, 0);
        assert_eq!(cron("5/2 * * * *", None).next_run(from, from), Ok(ms(2024, 1, 1, 0, 5)));
        assert_eq!(cron("5/2 * * * *", None).next_run(ms(2024, 1, 1, 0, 5), 0), Ok(ms(2024, 1, 1, 1, 5)));
    }

    #[test]
    fn every_must_be_positive_and_bounded() {
        assert!(Cadence::Every { every_ms: 0 }.validate().is_err());
        assert!(Cadence::Every { every_ms: u64::MAX }.validate().is_err());
        assert_eq!(Cadence::Every { every_ms: 1 }.validate(), Ok(()));
    }

    #[test]
    fn utc_aliases_are_accepted_and_odd_names_are_not() {
        for tz in ["UTC", "Etc/UTC", "GMT", "Z"] {
            assert_eq!(cron("0 0 * * *", Some(tz)).validate(), Ok(()), "{tz}");
        }
        for tz in ["", "../etc/passwd", "/etc/passwd", "No/Such_Zone", "A b"] {
            assert_eq!(cron("0 0 * * *", Some(tz)).validate(), Err(format!("Invalid cron time zone: {tz}")), "{tz}");
        }
    }

    #[test]
    fn cadence_json_has_the_upstream_shape() {
        let every: Cadence = serde_json::from_str(r#"{"type":"every","everyMs":60000}"#).unwrap();
        assert_eq!(every, Cadence::Every { every_ms: 60_000 });
        assert_eq!(serde_json::to_string(&every).unwrap(), r#"{"type":"every","everyMs":60000}"#);
        let utc = cron("0 9 * * *", None);
        assert_eq!(serde_json::to_string(&utc).unwrap(), r#"{"type":"cron","expression":"0 9 * * *"}"#);
        let zoned: Cadence = serde_json::from_str(r#"{"type":"cron","expression":"0 9 * * *","timezone":"UTC"}"#).unwrap();
        assert_eq!(zoned, cron("0 9 * * *", Some("UTC")));
    }

    fn posix_offset(rule: &str, y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i32 {
        Posix::parse(rule).expect("rule").offset(ms(y, mo, d, h, mi) / 1000)
    }

    #[test]
    fn posix_rules_switch_on_the_local_transition_instant() {
        let us = "EST5EDT,M3.2.0,M11.1.0";
        assert_eq!(posix_offset(us, 2024, 1, 15, 12, 0), -5 * 3600);
        assert_eq!(posix_offset(us, 2024, 7, 1, 12, 0), -4 * 3600);
        assert_eq!(posix_offset(us, 2024, 3, 10, 6, 59), -5 * 3600);
        assert_eq!(posix_offset(us, 2024, 3, 10, 7, 0), -4 * 3600);
        assert_eq!(posix_offset(us, 2024, 11, 3, 5, 59), -4 * 3600);
        assert_eq!(posix_offset(us, 2024, 11, 3, 6, 0), -5 * 3600);
    }

    #[test]
    fn posix_rules_cover_last_week_southern_hemisphere_and_no_dst() {
        let eu = "CET-1CEST,M3.5.0,M10.5.0/3";
        assert_eq!(posix_offset(eu, 2024, 3, 31, 0, 59), 3600);
        assert_eq!(posix_offset(eu, 2024, 3, 31, 1, 0), 7200);
        assert_eq!(posix_offset(eu, 2024, 10, 27, 0, 59), 7200);
        assert_eq!(posix_offset(eu, 2024, 10, 27, 1, 0), 3600);
        let sydney = "AEST-10AEDT,M10.1.0,M4.1.0/3";
        assert_eq!(posix_offset(sydney, 2024, 1, 15, 0, 0), 11 * 3600);
        assert_eq!(posix_offset(sydney, 2024, 7, 15, 0, 0), 10 * 3600);
        assert_eq!(posix_offset("<+07>-7", 2024, 7, 15, 0, 0), 7 * 3600);
        assert_eq!(posix_offset("UTC0", 2024, 7, 15, 0, 0), 0);
        assert!(Posix::parse("EST5EDT").is_none());
        assert!(Posix::parse("").is_none());
    }

    #[test]
    fn system_zones_shift_cron_runs() {
        let dir = std::env::var("TZDIR").unwrap_or_else(|_| "/usr/share/zoneinfo".into());
        if !std::path::Path::new(&dir).join("America/New_York").exists() {
            return;
        }
        let jakarta = cron("0 9 * * *", Some("Asia/Jakarta"));
        let from = ms(2024, 1, 1, 0, 0);
        assert_eq!(jakarta.next_run(from, from), Ok(ms(2024, 1, 1, 2, 0)));
        let new_york = cron("0 9 * * *", Some("America/New_York"));
        let winter = ms(2024, 1, 1, 0, 0);
        let summer = ms(2024, 7, 1, 0, 0);
        assert_eq!(new_york.next_run(winter, winter), Ok(ms(2024, 1, 1, 14, 0)));
        assert_eq!(new_york.next_run(summer, summer), Ok(ms(2024, 7, 1, 13, 0)));
        let far = ms(2045, 7, 1, 0, 0);
        assert_eq!(new_york.next_run(far, far), Ok(ms(2045, 7, 1, 13, 0)));
    }
}
