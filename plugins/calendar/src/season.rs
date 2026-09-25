//! Seasonal awareness — proactive task surfacing (SPEC §7.7, "seasonal
//! awareness").
//!
//! A troop's year has a shape: registration and recharter, winter skills, the
//! maple-season and Earth Day service windows, summer camp, the annual
//! Congress, dues and winter gear. Software that knows the shape can put the
//! next thing in front of people *before* somebody remembers it in a group
//! chat — which is the whole point of the calendar being the place a troop
//! lives (docs/plugin-roadmap.md §4).
//!
//! These are **prompts, not obligations**: each one names a task and why the
//! season is when it happens, and none of them asserts policy (what the
//! Accords require of a Congress, or what dues are) — a troop sets those. The
//! surface is deliberately a list with a `detail` line the client can show
//! beside the reason, so a Chief can dismiss it and nobody mistakes the
//! software's calendar knowledge for the troop's rules.

use chrono::{Datelike, Duration, NaiveDate};

/// One seasonally-surfaced task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeasonalTask {
    /// Stable identifier (`winter_skills`, …) — a client keys dismissals on it.
    pub key: &'static str,
    /// Month of the troop year it belongs to (1–12).
    pub month: u32,
    /// Day of that month it is worth surfacing on.
    pub on_day: u32,
    pub title: &'static str,
    /// Why this season is when it happens — one line, shown with the title.
    pub detail: &'static str,
}

/// The troop year, one entry per thing worth surfacing.
pub const SEASONAL_TASKS: &[SeasonalTask] = &[
    SeasonalTask {
        key: "recharter_prep",
        month: 1,
        on_day: 5,
        title: "Recharter and registration renewals",
        detail: "Charters and youth registrations run on the calendar year; the paperwork \
                 usually has a January deadline the troop does not control.",
    },
    SeasonalTask {
        key: "winter_skills",
        month: 1,
        on_day: 12,
        title: "Schedule winter skills training",
        detail: "Cold-weather travel, layering and shelter are taught while the conditions \
                 are actually there.",
    },
    SeasonalTask {
        key: "winter_survival",
        month: 2,
        on_day: 9,
        title: "Winter survival and ice-safety session",
        detail: "Ice and cold-water safety is only trainable in season, and it needs an \
                 indoor backup date on the calendar.",
    },
    SeasonalTask {
        key: "annual_planning",
        month: 2,
        on_day: 20,
        title: "Plan the year's program and budget",
        detail: "A program calendar agreed before spring is what the rest of the year gets \
                 planned against.",
    },
    SeasonalTask {
        key: "spring_inventory",
        month: 3,
        on_day: 15,
        title: "Inventory personal and troop gear",
        detail: "Seasonal changeover is when gear is found missing, damaged, or still wet \
                 from the last trip.",
    },
    SeasonalTask {
        key: "maple_service",
        month: 3,
        on_day: 22,
        title: "Maple-season service window",
        detail: "Sugarhouse and woodlot work is a short, weather-bound window that fills a \
                 service-hours gap early in the year.",
    },
    SeasonalTask {
        key: "trail_maintenance",
        month: 4,
        on_day: 18,
        title: "Trail maintenance and Earth Day service",
        detail: "Frost heave and spring runoff damage trails in March–April, so the repair \
                 window is before the summer use season.",
    },
    SeasonalTask {
        key: "spring_camporee",
        month: 4,
        on_day: 25,
        title: "Prepare for the spring camporee",
        detail: "Campsite reservations, permits and a duty roster take weeks, not days.",
    },
    SeasonalTask {
        key: "camp_registration",
        month: 5,
        on_day: 10,
        title: "Close out summer camp registrations",
        detail: "Camp rosters and physical forms are usually due well before the session \
                 starts, and late additions are the common failure.",
    },
    SeasonalTask {
        key: "first_aid_renewal",
        month: 5,
        on_day: 20,
        title: "Renew first-aid and CPR certifications",
        detail: "Certifications have fixed terms; renewing in late spring keeps them valid \
                 through the summer season when they are actually needed.",
    },
    SeasonalTask {
        key: "summer_camp",
        month: 6,
        on_day: 15,
        title: "Summer camp session",
        detail: "The session dominates the calendar — publish drop-off, pickup and \
                 in-camp contact details as events so nobody asks twice.",
    },
    SeasonalTask {
        key: "storm_readiness",
        month: 6,
        on_day: 1,
        title: "Storm-season readiness",
        detail: "Check the emergency contacts, shelter plan and call-tree before the \
                 season that exercises them.",
    },
    SeasonalTask {
        key: "high_adventure",
        month: 7,
        on_day: 10,
        title: "High-adventure trips and summer service",
        detail: "Long trips and the biggest service days land when school is out and \
                 daylight is long.",
    },
    SeasonalTask {
        key: "midyear_service_review",
        month: 7,
        on_day: 25,
        title: "Mid-year service-hours review",
        detail: "Half the year is gone: reviewing hours now leaves a whole season to close \
                 a shortfall.",
    },
    SeasonalTask {
        key: "fall_planning",
        month: 8,
        on_day: 10,
        title: "Plan the fall program and recruit adults",
        detail: "Adult leadership and drivers must be lined up before the school-year \
                 schedule starts, not after.",
    },
    SeasonalTask {
        key: "school_year_schedule",
        month: 9,
        on_day: 5,
        title: "Publish the school-year meeting schedule",
        detail: "The recurring meeting nights belong on the calendar as a series so \
                 families can plan around them.",
    },
    SeasonalTask {
        key: "congress_prep",
        month: 9,
        on_day: 20,
        title: "Confirm the annual Congress date and delegates",
        detail: "The Congress is the troop's annual decision-making body; the date, \
                 delegates and any motions belong on the calendar ahead of it.",
    },
    SeasonalTask {
        key: "dues_collection",
        month: 10,
        on_day: 5,
        title: "Annual dues collection",
        detail: "Dues season pairs with recharter paperwork — collecting in one window \
                 keeps the two from chasing each other.",
    },
    SeasonalTask {
        key: "winter_gear",
        month: 10,
        on_day: 20,
        title: "Issue and check winter gear",
        detail: "Boots, bags and layers sized before the cold arrives is the difference \
                 between a trip and a cancelled trip.",
    },
    SeasonalTask {
        key: "holiday_service",
        month: 11,
        on_day: 15,
        title: "Holiday service and food drive",
        detail: "The community expects it in this window, and it is the easiest service \
                 hours of the year to book.",
    },
    SeasonalTask {
        key: "accords_review",
        month: 11,
        on_day: 25,
        title: "Review the Accords and any pending motions",
        detail: "Reading the governing document before the year ends is what keeps the \
                 next Congress from being the first time anyone opens it.",
    },
    SeasonalTask {
        key: "year_end_report",
        month: 12,
        on_day: 10,
        title: "Year-end impact report and budget close-out",
        detail: "Service hours, participation and spending are close-out items, and the \
                 figures are only fresh in December.",
    },
];

/// Every task belonging to `month` (1–12), ascending by day.
pub fn seasonal_tasks_for_month(month: u32) -> Vec<&'static SeasonalTask> {
    let mut out: Vec<&SeasonalTask> = SEASONAL_TASKS.iter().filter(|t| t.month == month).collect();
    out.sort_by_key(|t| t.on_day);
    out
}

/// The tasks whose surface date falls in `[from, to]`, ascending by date.
///
/// The window is inclusive at both ends and may cross a year boundary: a
/// December→January window returns the December tasks of the starting year and
/// the January tasks of the next.
pub fn seasonal_tasks_between(
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<(NaiveDate, &'static SeasonalTask)> {
    let mut out: Vec<(NaiveDate, &'static SeasonalTask)> = Vec::new();
    if to < from {
        return out;
    }
    for year in from.year()..=to.year() {
        for task in SEASONAL_TASKS {
            let Some(date) = due_date(year, task) else {
                continue;
            };
            if date >= from && date <= to {
                out.push((date, task));
            }
        }
    }
    out.sort_by_key(|(date, task)| (*date, task.on_day));
    out
}

/// The date a task surfaces in `year`: the day it names, clamped to the end of
/// a short month (so a February 30th is never a panic or a missing task).
pub fn due_date(year: i32, task: &SeasonalTask) -> Option<NaiveDate> {
    let next_month = if task.month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, task.month + 1, 1)?
    };
    let last_day = (next_month - Duration::days(1)).day();
    NaiveDate::from_ymd_opt(year, task.month, task.on_day.min(last_day))
}

/// The season a date falls in, as a stable code — a client can render the
/// troop's programme by season without reimplementing the month split.
///
/// Deliberately meteorological (whole months), not astronomical: a meeting on
/// the 20th of March is a spring meeting for planning purposes whatever the
/// equinox says.
pub fn season_of(date: NaiveDate) -> &'static str {
    match date.month() {
        12 | 1 | 2 => "winter",
        3..=5 => "spring",
        6..=8 => "summer",
        _ => "fall",
    }
}
