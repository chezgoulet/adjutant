//! iCal `RRULE` recurrence (RFC 5545 §3.3.10) — the subset a troop calendar
//! actually uses, with the arithmetic in plain Rust.
//!
//! # Why not a crate
//!
//! A plugin is a `cdylib` loaded into the core's process, and the boundary rule
//! (SDK docs, "host-mediated I/O") is that a plugin links nothing that needs a
//! runtime the core never set. A pure-Rust RRULE crate *is* allowed under that
//! rule, but the subset a troop needs — weekly meetings, monthly meetings, the
//! occasional "third Thursday" or "last Friday" — is small, and keeping it here
//! makes the schedule arithmetic testable with no database and no server
//! (`docs/plugin-development.md` §Testing). Unsupported parts are **rejected at
//! write time** rather than silently ignored: a calendar that quietly drops
//! `BYSETPOS` is worse than one that says it cannot do that yet.
//!
//! # Wall clock, not UTC
//!
//! Occurrences are computed on the **naive wall clock** (a datetime with no
//! zone) of the event's own timezone, because that is what "Tuesdays at 7pm"
//! means to a troop: the local time is fixed and its UTC offset moves with DST.
//! `calendar.events.starts_at` is wall-clock input converted by PostgreSQL
//! (`$n::timestamp AT TIME ZONE timezone`), so the timezone database stays in
//! the database and this crate stays dependency-free arithmetic.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Weekday};

/// Occurrence frequencies this implementation supports. `HOURLY`, `MINUTELY`
/// and `SECONDLY` are refused on purpose: a troop calendar has no sub-daily
/// events, and pretending otherwise would mean a second recurrence engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    pub fn as_str(&self) -> &'static str {
        match self {
            Freq::Daily => "DAILY",
            Freq::Weekly => "WEEKLY",
            Freq::Monthly => "MONTHLY",
            Freq::Yearly => "YEARLY",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_uppercase().as_str() {
            "DAILY" => Ok(Freq::Daily),
            "WEEKLY" => Ok(Freq::Weekly),
            "MONTHLY" => Ok(Freq::Monthly),
            "YEARLY" => Ok(Freq::Yearly),
            other => Err(format!(
                "FREQ={other} is not supported (DAILY, WEEKLY, MONTHLY, YEARLY)"
            )),
        }
    }
}

/// One `BYDAY` entry: a weekday, optionally with an ordinal (`1MO`, `-1FR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByDay {
    /// `Some(n)` for the nth weekday of the month (negative counts from the
    /// end); `None` for every matching weekday.
    pub ordinal: Option<i32>,
    pub weekday: Weekday,
}

impl ByDay {
    pub fn weekday_code(&self) -> &'static str {
        match self.weekday {
            Weekday::Mon => "MO",
            Weekday::Tue => "TU",
            Weekday::Wed => "WE",
            Weekday::Thu => "TH",
            Weekday::Fri => "FR",
            Weekday::Sat => "SA",
            Weekday::Sun => "SU",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim().to_ascii_uppercase();
        if value.len() < 2 {
            return Err(format!("BYDAY entry {value:?} is not a weekday"));
        }
        let (ordinal, code) = value.split_at(value.len() - 2);
        let weekday = match code {
            "MO" => Weekday::Mon,
            "TU" => Weekday::Tue,
            "WE" => Weekday::Wed,
            "TH" => Weekday::Thu,
            "FR" => Weekday::Fri,
            "SA" => Weekday::Sat,
            "SU" => Weekday::Sun,
            other => return Err(format!("BYDAY entry {other:?} is not a weekday")),
        };
        let ordinal = if ordinal.is_empty() {
            None
        } else {
            Some(ordinal.parse::<i32>().map_err(|_| {
                format!("BYDAY ordinal {ordinal:?} is not a number (e.g. 1MO, -1FR)")
            })?)
        };
        if ordinal == Some(0) {
            return Err("BYDAY ordinal 0 is not valid (1MO is the first Monday)".into());
        }
        Ok(Self { ordinal, weekday })
    }

    fn to_ical(self) -> String {
        match self.ordinal {
            Some(n) => format!("{n}{}", self.weekday_code()),
            None => self.weekday_code().to_string(),
        }
    }
}

/// A parsed, validated recurrence rule.
///
/// Only the fields listed in [`PART_NAMES`] exist: a rule that carries anything
/// else does not parse, so a stored rule is always one this crate can expand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    pub freq: Freq,
    pub interval: i64,
    /// `BYDAY`, canonicalised (ascending weekday, then ordinal).
    pub by_day: Vec<ByDay>,
    /// `BYMONTHDAY` (negative counts from the end of the month).
    pub by_month_day: Vec<i32>,
    /// `BYMONTH`.
    pub by_month: Vec<u32>,
    /// `COUNT` — total occurrences **from DTSTART**, not from the window start.
    pub count: Option<i64>,
    /// `UNTIL` — inclusive, as RFC 5545 specifies.
    pub until: Option<NaiveDateTime>,
}

/// The `RRULE` parts this crate understands, for error messages and docs.
pub const PART_NAMES: [&str; 8] = [
    "FREQ",
    "INTERVAL",
    "COUNT",
    "UNTIL",
    "BYDAY",
    "BYMONTHDAY",
    "BYMONTH",
    "WKST",
];

/// How many recurrence periods one expansion will walk before giving up. A
/// sparse rule (`BYMONTHDAY=31`) legitimately produces empty periods, so the
/// bound is on *periods*, not occurrences; the caller sees [`Expansion::truncated`]
/// when it is hit and can say so instead of pretending the series ended.
pub const MAX_PERIODS: i64 = 4000;

/// The result of expanding a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// Occurrences at or after the window start, ascending, at most `limit`.
    pub occurrences: Vec<NaiveDateTime>,
    /// The period walk hit [`MAX_PERIODS`] before reaching the window end.
    pub truncated: bool,
    /// Periods examined — the cost signal, exposed for tests and diagnostics.
    pub periods: i64,
}

impl Recurrence {
    /// The canonical `RRULE` content line (no `RRULE:` prefix), as stored.
    pub fn to_ical(&self) -> String {
        let mut parts = vec![
            format!("FREQ={}", self.freq.as_str()),
            format!("INTERVAL={}", self.interval),
        ];
        if let Some(count) = self.count {
            parts.push(format!("COUNT={count}"));
        }
        if let Some(until) = self.until {
            parts.push(format!("UNTIL={}", until.format("%Y%m%dT%H%M%SZ")));
        }
        if !self.by_month.is_empty() {
            let days: Vec<String> = self.by_month.iter().map(|m| m.to_string()).collect();
            parts.push(format!("BYMONTH={}", days.join(",")));
        }
        if !self.by_month_day.is_empty() {
            let days: Vec<String> = self.by_month_day.iter().map(|d| d.to_string()).collect();
            parts.push(format!("BYMONTHDAY={}", days.join(",")));
        }
        if !self.by_day.is_empty() {
            let days: Vec<String> = self.by_day.iter().map(|d| d.to_ical()).collect();
            parts.push(format!("BYDAY={}", days.join(",")));
        }
        parts.join(";")
    }

    /// Whether this rule can produce a second occurrence (i.e. it recurs).
    pub fn recurs(&self) -> bool {
        self.count.map(|c| c > 1).unwrap_or(true)
    }
}

/// Parse an `RRULE` content line. A leading `RRULE:` is accepted (clients paste
/// whole lines), and part names are case-insensitive, as RFC 5545 requires.
pub fn parse_rrule(raw: &str) -> Result<Recurrence, String> {
    let body = raw
        .trim()
        .strip_prefix("RRULE:")
        .or_else(|| raw.trim().strip_prefix("rrule:"))
        .unwrap_or(raw.trim())
        .trim();
    if body.is_empty() {
        return Err("rrule is empty".into());
    }

    let mut freq: Option<Freq> = None;
    let mut interval: i64 = 1;
    let mut count: Option<i64> = None;
    let mut until: Option<NaiveDateTime> = None;
    let mut by_day: Vec<ByDay> = Vec::new();
    let mut by_month_day: Vec<i32> = Vec::new();
    let mut by_month: Vec<u32> = Vec::new();

    for part in body.split(';').filter(|p| !p.trim().is_empty()) {
        let (name, value) = part
            .split_once('=')
            .ok_or_else(|| format!("RRULE part {part:?} is not NAME=VALUE"))?;
        let name = name.trim().to_ascii_uppercase();
        let value = value.trim();
        match name.as_str() {
            "FREQ" => freq = Some(Freq::parse(value)?),
            "INTERVAL" => {
                interval = value
                    .parse::<i64>()
                    .map_err(|_| format!("INTERVAL={value} is not a number"))?;
                if interval < 1 {
                    return Err("INTERVAL must be at least 1".into());
                }
            }
            "COUNT" => {
                let n = value
                    .parse::<i64>()
                    .map_err(|_| format!("COUNT={value} is not a number"))?;
                if n < 1 {
                    return Err("COUNT must be at least 1".into());
                }
                count = Some(n);
            }
            "UNTIL" => until = Some(parse_until(value)?),
            "BYDAY" => {
                for entry in value.split(',').filter(|e| !e.trim().is_empty()) {
                    by_day.push(ByDay::parse(entry)?);
                }
            }
            "BYMONTHDAY" => {
                for entry in value.split(',').filter(|e| !e.trim().is_empty()) {
                    let day: i32 = entry
                        .trim()
                        .parse()
                        .map_err(|_| format!("BYMONTHDAY entry {entry:?} is not a number"))?;
                    if day == 0 || !(-31..=31).contains(&day) {
                        return Err(format!(
                            "BYMONTHDAY entry {day} is out of range (1..31, or -1..-31)"
                        ));
                    }
                    by_month_day.push(day);
                }
                if by_month_day.is_empty() {
                    return Err("BYMONTHDAY is empty".into());
                }
            }
            "BYMONTH" => {
                for entry in value.split(',').filter(|e| !e.trim().is_empty()) {
                    let month: u32 = entry
                        .trim()
                        .parse()
                        .map_err(|_| format!("BYMONTH entry {entry:?} is not a number"))?;
                    if !(1..=12).contains(&month) {
                        return Err(format!("BYMONTH entry {month} is out of range (1..12)"));
                    }
                    by_month.push(month);
                }
                if by_month.is_empty() {
                    return Err("BYMONTH is empty".into());
                }
            }
            // Accepted for client compatibility and ignored: every week this
            // implementation generates starts on Monday, which is the RFC's
            // default. Ignoring it silently would be the trap, so anything else
            // is refused.
            "WKST" => {
                if !value.eq_ignore_ascii_case("MO") {
                    return Err(format!(
                        "WKST={value} is not implemented (only WKST=MO, the RFC default)"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "RRULE part {other} is not implemented (supported: {})",
                    PART_NAMES.join(", ")
                ))
            }
        }
    }

    let freq = freq.ok_or_else(|| "RRULE is missing FREQ".to_string())?;
    if count.is_some() && until.is_some() {
        return Err("RRULE cannot carry both COUNT and UNTIL (RFC 5545 §3.3.10)".into());
    }

    // The parts that are a *filter* in one frequency are genuinely different
    // arithmetic in another. Refusing beats expanding to the wrong days.
    match freq {
        Freq::Daily => {
            if !by_day.is_empty() {
                return Err(
                    "BYDAY with FREQ=DAILY is not implemented — use FREQ=WEEKLY with BYDAY".into(),
                );
            }
            if !by_month_day.is_empty() || !by_month.is_empty() {
                return Err(
                    "BYMONTHDAY/BYMONTH with FREQ=DAILY are not implemented — use \
                     FREQ=WEEKLY or FREQ=MONTHLY"
                        .into(),
                );
            }
        }
        Freq::Weekly => {
            if !by_month_day.is_empty() {
                return Err("BYMONTHDAY with FREQ=WEEKLY is not implemented".into());
            }
            if !by_month.is_empty() {
                return Err("BYMONTH with FREQ=WEEKLY is not implemented".into());
            }
            if by_day.iter().any(|d| d.ordinal.is_some()) {
                return Err("a BYDAY ordinal (1MO, -1FR) needs FREQ=MONTHLY or FREQ=YEARLY".into());
            }
        }
        Freq::Monthly | Freq::Yearly => {
            if !by_day.is_empty() && !by_month_day.is_empty() {
                return Err(
                    "BYDAY and BYMONTHDAY together are not implemented for monthly/yearly rules"
                        .into(),
                );
            }
        }
    }

    by_day.sort_by_key(|d| (d.weekday.num_days_from_monday(), d.ordinal.unwrap_or(0)));
    by_day.dedup();
    by_month_day.sort_unstable();
    by_month_day.dedup();
    by_month.sort_unstable();
    by_month.dedup();

    Ok(Recurrence {
        freq,
        interval,
        by_day,
        by_month_day,
        by_month,
        count,
        until,
    })
}

/// `UNTIL` in either RFC form — `20261231T235959Z`, `20261231` — plus the
/// RFC 3339 spelling, because pasting `2026-12-31T23:59:59Z` is what a human
/// does and refusing it would teach nothing.
fn parse_until(value: &str) -> Result<NaiveDateTime, String> {
    let value = value.trim();
    if let Ok(dt) = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ") {
        return Ok(dt);
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y%m%d") {
        // A date-only UNTIL is inclusive of the whole day.
        return Ok(date
            .and_hms_opt(23, 59, 59)
            .expect("midnight is a valid time"));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(dt.naive_utc());
    }
    Err(format!(
        "UNTIL={value:?} is not a timestamp (expected 20261231T235959Z or 20261231)"
    ))
}

/// Expand a rule into occurrences at or after `from`, ascending, at most
/// `limit`.
///
/// `dtstart` is the series start on the wall clock; occurrences before it are
/// never returned (RFC 5545: DTSTART is the first occurrence), but they still
/// count against `COUNT`, which is why the enumeration always walks from
/// `dtstart`.
pub fn expand(
    rule: &Recurrence,
    dtstart: NaiveDateTime,
    from: NaiveDateTime,
    limit: usize,
) -> Expansion {
    let mut occurrences: Vec<NaiveDateTime> = Vec::new();
    let mut produced: i64 = 0;
    let mut periods: i64 = 0;
    let mut truncated = false;

    if limit == 0 {
        return Expansion {
            occurrences,
            truncated,
            periods,
        };
    }

    let mut k: i64 = 0;
    'periods: while periods < MAX_PERIODS {
        periods += 1;
        let candidates = period_candidates(rule, dtstart, k);
        k += 1;
        if candidates.is_empty() {
            // A legitimate empty period (the 31st in February). Keep walking:
            // the period bound above is what stops a hopeless rule.
            continue;
        }
        for date in candidates {
            let candidate = NaiveDateTime::new(date, dtstart.time());
            if candidate < dtstart {
                continue;
            }
            if let Some(until) = rule.until {
                if candidate > until {
                    // Candidates ascend within a period and periods ascend, so
                    // the first one past UNTIL ends the series.
                    break 'periods;
                }
            }
            produced += 1;
            if let Some(count) = rule.count {
                if produced > count {
                    break 'periods;
                }
            }
            if candidate >= from {
                occurrences.push(candidate);
                if occurrences.len() >= limit {
                    break 'periods;
                }
            }
        }
    }
    if periods >= MAX_PERIODS && occurrences.len() < limit {
        truncated = true;
    }
    Expansion {
        occurrences,
        truncated,
        periods,
    }
}

/// Convenience wrapper: parse and expand in one step.
pub fn expand_rrule(
    rrule: &str,
    dtstart: NaiveDateTime,
    from: NaiveDateTime,
    limit: usize,
) -> Result<Expansion, String> {
    let rule = parse_rrule(rrule)?;
    Ok(expand(&rule, dtstart, from, limit))
}

/// The candidate dates of period `k` (0 = the period containing `dtstart`),
/// ascending and deduplicated by the caller's sort.
fn period_candidates(rule: &Recurrence, dtstart: NaiveDateTime, k: i64) -> Vec<NaiveDate> {
    let mut dates = match rule.freq {
        Freq::Daily => vec![dtstart.date() + Duration::days(k * rule.interval)],
        Freq::Weekly => {
            let week_start = week_start_of(dtstart.date());
            let base = week_start + Duration::days(k * rule.interval * 7);
            let weekdays: Vec<Weekday> = if rule.by_day.is_empty() {
                vec![dtstart.weekday()]
            } else {
                rule.by_day.iter().map(|d| d.weekday).collect()
            };
            let mut out: Vec<NaiveDate> = weekdays
                .iter()
                .map(|w| base + Duration::days(w.num_days_from_monday() as i64))
                .collect();
            out.sort_unstable();
            out.dedup();
            out
        }
        Freq::Monthly => {
            let (year, month) = add_months(dtstart.year(), dtstart.month(), k * rule.interval);
            month_candidates(rule, dtstart, year, month)
        }
        Freq::Yearly => {
            let year = dtstart.year() + (k * rule.interval) as i32;
            let months: Vec<u32> = if rule.by_month.is_empty() {
                vec![dtstart.month()]
            } else {
                rule.by_month.clone()
            };
            let mut out = Vec::new();
            for month in months {
                out.extend(month_candidates(rule, dtstart, year, month));
            }
            out
        }
    };
    dates.sort_unstable();
    dates.dedup();
    dates
}

/// The candidate dates inside one month: `BYMONTHDAY`, else `BYDAY`, else
/// DTSTART's day-of-month (skipping months where that day does not exist, which
/// is what RFC 5545 specifies).
fn month_candidates(
    rule: &Recurrence,
    dtstart: NaiveDateTime,
    year: i32,
    month: u32,
) -> Vec<NaiveDate> {
    let mut out: Vec<NaiveDate> = Vec::new();
    if !rule.by_month_day.is_empty() {
        for day in &rule.by_month_day {
            if let Some(date) = resolve_month_day(year, month, *day) {
                out.push(date);
            }
        }
    } else if !rule.by_day.is_empty() {
        for by_day in &rule.by_day {
            match by_day.ordinal {
                Some(ordinal) => {
                    if let Some(date) = nth_weekday(year, month, by_day.weekday, ordinal) {
                        out.push(date);
                    }
                }
                None => {
                    out.extend(every_weekday(year, month, by_day.weekday));
                }
            }
        }
    } else if let Some(date) = NaiveDate::from_ymd_opt(year, month, dtstart.day()) {
        out.push(date);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Monday-based week start (this implementation's fixed `WKST`).
fn week_start_of(date: NaiveDate) -> NaiveDate {
    date - Duration::days(date.weekday().num_days_from_monday() as i64)
}

fn add_months(year: i32, month: u32, delta: i64) -> (i32, u32) {
    let total = i64::from(year) * 12 + i64::from(month) - 1 + delta;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) + 1;
    (year as i32, month as u32)
}

fn last_day_of_month(year: i32, month: u32) -> Option<u32> {
    let (next_year, next_month) = add_months(year, month, 1);
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some((first_of_next - Duration::days(1)).day())
}

/// `BYMONTHDAY`: positive from the start, negative from the end, `None` when
/// the month is too short (which skips the occurrence, per RFC 5545).
fn resolve_month_day(year: i32, month: u32, day: i32) -> Option<NaiveDate> {
    if day > 0 {
        return NaiveDate::from_ymd_opt(year, month, day as u32);
    }
    let last = last_day_of_month(year, month)? as i32;
    let resolved = last + 1 + day;
    if resolved < 1 {
        return None;
    }
    NaiveDate::from_ymd_opt(year, month, resolved as u32)
}

/// The nth weekday of a month (negative from the end), e.g. `1MO`, `-1FR`.
fn nth_weekday(year: i32, month: u32, weekday: Weekday, ordinal: i32) -> Option<NaiveDate> {
    if ordinal > 0 {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        let offset = (weekday.num_days_from_monday() as i64
            - first.weekday().num_days_from_monday() as i64)
            .rem_euclid(7);
        let day = 1 + offset + (i64::from(ordinal) - 1) * 7;
        if day > i64::from(last_day_of_month(year, month)?) {
            return None;
        }
        NaiveDate::from_ymd_opt(year, month, day as u32)
    } else {
        let last_day = last_day_of_month(year, month)?;
        let last = NaiveDate::from_ymd_opt(year, month, last_day)?;
        let offset = (last.weekday().num_days_from_monday() as i64
            - weekday.num_days_from_monday() as i64)
            .rem_euclid(7);
        let day = i64::from(last_day) - offset + (i64::from(ordinal) + 1) * 7;
        if day < 1 {
            return None;
        }
        NaiveDate::from_ymd_opt(year, month, day as u32)
    }
}

/// Every `weekday` in the month (`BYDAY=MO` with FREQ=MONTHLY).
fn every_weekday(year: i32, month: u32, weekday: Weekday) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut day = 1u32;
    while let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
        if date.weekday() == weekday {
            out.push(date);
        }
        day += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Wall-clock timestamps
// ---------------------------------------------------------------------------

/// Parse a **wall-clock** timestamp: `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM`,
/// `YYYY-MM-DDTHH:MM:SS` (a space instead of `T` is accepted), or `YYYY-MM`
/// for a month.
///
/// A UTC designator or numeric offset is refused with an explanation rather
/// than reinterpreted: the API contract is "local time + the event's
/// `timezone`", so silently treating `18:00:00Z` as 6pm local would move a
/// meeting by hours across a DST boundary.
pub fn parse_local_timestamp(raw: &str) -> Result<NaiveDateTime, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("timestamp is empty".into());
    }
    if value.ends_with(['Z', 'z']) || offset_after_time(value) {
        return Err(format!(
            "{value:?} carries a UTC designator or offset; send a wall-clock time \
             (YYYY-MM-DDTHH:MM:SS) and put the zone in the event's `timezone`"
        ));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, format) {
            return Ok(dt);
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date.and_hms_opt(0, 0, 0).expect("midnight is a valid time"));
    }
    Err(format!(
        "{value:?} is not a wall-clock timestamp (expected YYYY-MM-DD, \
         YYYY-MM-DDTHH:MM or YYYY-MM-DDTHH:MM:SS)"
    ))
}

/// True when a `+`/`-` offset follows the time part (so the `-` separators in
/// the date itself are not mistaken for a negative offset).
fn offset_after_time(value: &str) -> bool {
    let time_start = value.find(['T', ' ']).map(|i| i + 1).unwrap_or(value.len());
    value[time_start..].contains(['+', '-'])
}

/// Render a wall-clock datetime the way the API states it: second precision,
/// `T` separator, no zone (the zone is a separate field).
pub fn render_local(dt: NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Parse the `exdates` column: a comma-separated list of wall-clock timestamps
/// (a bare date means "that whole day").
pub fn parse_exdates(raw: &str) -> Result<Vec<NaiveDateTime>, String> {
    let mut out = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let dt = if entry.len() == 10 && !entry.contains(['T', ' ']) {
            let date = NaiveDate::parse_from_str(entry, "%Y-%m-%d")
                .map_err(|_| format!("exdate {entry:?} is not a date"))?;
            date.and_hms_opt(0, 0, 0).expect("midnight is a valid time")
        } else {
            parse_local_timestamp(entry).map_err(|e| format!("exdate: {e}"))?
        };
        out.push(dt);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Render a list of excluded dates back into the stored column format.
pub fn render_exdates(dates: &[NaiveDateTime]) -> String {
    let mut dates = dates.to_vec();
    dates.sort_unstable();
    dates.dedup();
    dates
        .iter()
        .map(|d| render_local(*d))
        .collect::<Vec<_>>()
        .join(",")
}

/// Is `candidate` excluded by an `exdates` list? A date-only entry excludes the
/// whole day, which is how a troop cancels a meeting without also asserting the
/// exact minute.
pub fn is_excluded(exdates: &[NaiveDateTime], candidate: NaiveDateTime, all_day: bool) -> bool {
    exdates.iter().any(|ex| {
        if all_day || is_midnight(*ex) {
            ex.date() == candidate.date()
        } else {
            *ex == candidate
        }
    })
}

/// Is this wall-clock time exactly midnight? (A midnight `exdate` is read as a
/// whole day.)
pub fn is_midnight(dt: NaiveDateTime) -> bool {
    dt.time() == chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid")
}

/// Normalise an all-day event's start to midnight, so a day-long event does not
/// depend on the minute the operator typed.
pub fn midnight(dt: NaiveDateTime) -> NaiveDateTime {
    NaiveDateTime::new(
        dt.date(),
        chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("valid"),
    )
}
