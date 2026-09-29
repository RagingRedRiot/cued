//! Next-fire computation (DESIGN.md §4.1, §9).
//!
//! Everything here speaks UTC `Timestamp`s. A zone enters only where it is
//! *meaning* rather than presentation: `Calendar`'s rule lives in one, and
//! the walk below converts into it and straight back out.
//!
//! Two cadence semantics, deliberately distinct:
//!
//! - `Every` is **elapsed real time** from a fixed anchor — pure instant
//!   arithmetic, drift-free, DST-blind ("every 6h" stays every 6h through a
//!   transition).
//! - `Calendar` is **civil wall-clock** in the rule's own zone —
//!   "every day 09:00" means 9am on that wall, across DST, with the §9
//!   disambiguation rules (gap shifts forward, fold takes the first
//!   occurrence) and the §9.2 short-month clamp.

use anyhow::{Context, Result, bail, ensure};
use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;

use crate::model::{CalendarSpec, MonthDay, Schedule};

/// The next firing instant STRICTLY after `after`, or None when the
/// schedule has no future firing (`Once` already past, `until` reached).
/// The §4.1 `count` cap is the caller's to enforce — only the store knows
/// how many firings are on record.
pub fn next_fire(schedule: &Schedule, after: &Timestamp) -> Result<Option<Timestamp>> {
    let next = match schedule {
        Schedule::Once { at } => (at > after).then_some(*at),
        Schedule::Every {
            interval,
            anchor,
            until,
            ..
        } => {
            ensure!(
                interval.is_positive(),
                "an Every interval must be positive (got {interval:#})"
            );
            clip_until(Some(next_every(interval, anchor, after)?), until)
        }
        Schedule::Calendar {
            spec, zone, until, ..
        } => {
            let tz = TimeZone::get(zone).with_context(|| format!("unknown time zone {zone:?}"))?;
            clip_until(Some(next_calendar(spec, &tz, after)?), until)
        }
    };
    Ok(next)
}

fn clip_until(next: Option<Timestamp>, until: &Option<Timestamp>) -> Option<Timestamp> {
    match (next, until) {
        (Some(next), Some(until)) if next > *until => None,
        (next, _) => next,
    }
}

/// anchor, anchor+i, anchor+2i, … (§4.1) — the first term strictly after
/// `after`, computed directly rather than stepped, so an every-minute job
/// that was off for a year costs the same as one that never slept.
fn next_every(
    interval: &jiff::SignedDuration,
    anchor: &Timestamp,
    after: &Timestamp,
) -> Result<Timestamp> {
    let step = interval.as_nanos();
    let anchor_ns = anchor.as_nanosecond();
    let after_ns = after.as_nanosecond();
    // Terms at or before `after` don't count — "strictly after" is what
    // makes anchor == submit time mean "first run one interval from now".
    let periods = if after_ns < anchor_ns {
        0
    } else {
        (after_ns - anchor_ns) / step + 1
    };
    Timestamp::from_nanosecond(anchor_ns + periods * step)
        .context("Every schedule ran off the end of representable time")
}

/// Walk civil days in the rule's zone until one matches the spec and its
/// wall-clock time lands strictly after `after`. Every v1 rule (§9.2) fires
/// at least monthly, so the bound is never near.
fn next_calendar(spec: &CalendarSpec, tz: &TimeZone, after: &Timestamp) -> Result<Timestamp> {
    // Into the rule's zone to walk civil days, and back to UTC to return.
    let mut date = after.to_zoned(tz.clone()).date();
    for _ in 0..1000 {
        if date_matches(spec, date) {
            let civil = DateTime::from_parts(date, spec_time(spec));
            // §9: to_zoned resolves DST per the "compatible" convention —
            // a nonexistent time shifts forward by the gap, an ambiguous
            // one takes its first occurrence.
            let candidate = civil
                .to_zoned(tz.clone())
                .with_context(|| format!("resolving {civil} in {tz:?}"))?;
            if candidate.timestamp() > *after {
                return Ok(candidate.timestamp());
            }
        }
        date = date.tomorrow().context("calendar walked past year 9999")?;
    }
    bail!("no firing within 1000 days of {after} — spec {spec:?}")
}

fn spec_time(spec: &CalendarSpec) -> jiff::civil::Time {
    match spec {
        CalendarSpec::Daily { at }
        | CalendarSpec::Weekly { at, .. }
        | CalendarSpec::Monthly { at, .. } => *at,
    }
}

fn date_matches(spec: &CalendarSpec, date: Date) -> bool {
    match spec {
        CalendarSpec::Daily { .. } => true,
        CalendarSpec::Weekly { days, .. } => days
            .iter()
            .any(|day| jiff::civil::Weekday::from(*day) == date.weekday()),
        CalendarSpec::Monthly { days, .. } => {
            let last = date.days_in_month();
            days.iter().any(|day| match day {
                // §9.2 clamp: "on 31" fires on the last day of months that
                // lack a 31st — as late as possible, never a silent skip.
                MonthDay::Day(n) => date.day() == (*n as i8).min(last),
                MonthDay::Last => date.day() == last,
            })
        }
    }
}

/// The instants of `schedule` in `[scheduled, now]`: the latest one, the
/// one just before it, and how many there are.
///
/// `limit` caps how many of them count — the §4.1 `count` budget still
/// unspent. Truncating *here* rather than after the fact is what keeps the
/// cap honest: catch-up compacts whatever range it is handed and runs the
/// last instant of it, so handing it instants the schedule was never
/// entitled to fire means running one of them.
pub(crate) fn due_instants(
    schedule: &Schedule,
    scheduled: &Timestamp,
    now: &Timestamp,
    limit: Option<u32>,
) -> Result<(Timestamp, Option<Timestamp>, u32)> {
    // Every is closed-form — an every-second job that slept a month must not
    // walk 2.6 million steps. `until` doesn't change that: all it does is
    // truncate the sequence, which is a smaller horizon, not a different
    // shape. (It used to send `until` schedules down the bounded walk below,
    // where a short interval plus real downtime could blow the walk's cap
    // and leave the job erroring on every tick forever.)
    if let Schedule::Every {
        interval, until, ..
    } = schedule
    {
        let step = interval.as_nanos();
        ensure!(
            step > 0,
            "an Every interval must be positive (got {interval:#})"
        );
        let start = scheduled.as_nanosecond();
        let horizon = match until {
            Some(until) => (*until).min(*now),
            None => *now,
        };
        let mut periods = (horizon.as_nanosecond() - start).max(0) / step;
        // The budget bounds the range as firmly as `until` or `now` does.
        if let Some(limit) = limit {
            periods = periods.min(i128::from(limit.saturating_sub(1)));
        }
        let instant = |n: i128| -> Result<Timestamp> {
            Timestamp::from_nanosecond(start + n * step).context("Every schedule out of range")
        };
        let latest = instant(periods)?;
        let previous = (periods >= 1).then(|| instant(periods - 1)).transpose()?;
        return Ok((latest, previous, (periods + 1).min(u32::MAX as i128) as u32));
    }

    // Calendar rules fire at most daily (§9.2), so the walk is cheap and the
    // cap below is a corruption guard rather than a real bound.
    let mut latest = *scheduled;
    let mut previous = None;
    let mut count = 1u32;
    while let Some(next) = next_fire(schedule, &latest)? {
        if next > *now {
            break;
        }
        if limit.is_some_and(|limit| count >= limit) {
            break;
        }
        previous = Some(std::mem::replace(&mut latest, next));
        count += 1;
        ensure!(
            count < 200_000,
            "over 200k missed firings — refusing to walk them"
        );
    }
    Ok((latest, previous, count))
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;
    use crate::model::Weekday;

    /// An instant, written in whatever offset reads clearest at the call
    /// site — the stored form is UTC either way.
    fn at(s: &str) -> Timestamp {
        s.parse().expect(s)
    }

    #[test]
    fn every_is_anchored_arithmetic() -> Result<()> {
        let anchor = at("2026-07-16T09:00:00-04:00");
        let schedule = Schedule::Every {
            interval: SignedDuration::from_secs(6 * 3600),
            anchor,
            until: None,
            count: None,
        };
        // Before the anchor → the anchor itself.
        let before = at("2026-07-16T05:00:00-04:00");
        assert_eq!(next_fire(&schedule, &before)?.unwrap(), anchor);
        // Exactly on a term → strictly after → the next term.
        assert_eq!(
            next_fire(&schedule, &anchor)?.unwrap(),
            at("2026-07-16T15:00:00-04:00")
        );
        // Long after: one jump, not a walk — anchor + k·i.
        let year_later = at("2027-07-20T10:30:00-04:00");
        let next = next_fire(&schedule, &year_later)?.unwrap();
        assert!(next > year_later);
        let since_anchor = next.as_nanosecond() - anchor.as_nanosecond();
        assert_eq!(since_anchor % (6 * 3600 * 1_000_000_000), 0, "on the grid");
        Ok(())
    }

    #[test]
    fn every_is_dst_blind() -> Result<()> {
        // Across the US spring-forward (2027-03-14 02:00 EST → 03:00 EDT):
        // "every 6h" stays 6 real hours; the wall-clock label shifts.
        let anchor = at("2027-03-13T21:00:00-05:00");
        let schedule = Schedule::Every {
            interval: SignedDuration::from_secs(6 * 3600),
            anchor,
            until: None,
            count: None,
        };
        let next = next_fire(&schedule, &anchor)?.unwrap();
        // 21:00 EST + 6 real hours = 04:00 EDT (only 5 wall hours later).
        assert_eq!(next, at("2027-03-14T04:00:00-04:00"));
        Ok(())
    }

    #[test]
    fn calendar_daily_rides_the_wall_clock_through_dst() -> Result<()> {
        let schedule = Schedule::Calendar {
            spec: CalendarSpec::Daily {
                at: jiff::civil::time(9, 0, 0, 0),
            },
            zone: "America/New_York".into(),
            until: None,
            count: None,
        };
        // The day before spring-forward: 9am EST today → 9am EDT tomorrow.
        let after = at("2027-03-13T10:00:00-05:00");
        let next = next_fire(&schedule, &after)?.unwrap();
        assert_eq!(next, at("2027-03-14T09:00:00-04:00"));
        Ok(())
    }

    #[test]
    fn calendar_gap_time_shifts_forward() -> Result<()> {
        // 02:30 doesn't exist on 2027-03-14 in New York (02:00→03:00 gap);
        // the compatible convention lands it at 03:30 (§9).
        let schedule = Schedule::Calendar {
            spec: CalendarSpec::Daily {
                at: jiff::civil::time(2, 30, 0, 0),
            },
            zone: "America/New_York".into(),
            until: None,
            count: None,
        };
        let after = at("2027-03-14T00:00:00-05:00");
        let next = next_fire(&schedule, &after)?.unwrap();
        assert_eq!(next, at("2027-03-14T03:30:00-04:00"));
        Ok(())
    }

    #[test]
    fn calendar_weekly_and_monthly_clamp() -> Result<()> {
        // Weekly: 2026-07-16 is a Thursday; next mon/fri is Friday the 17th.
        let weekly = Schedule::Calendar {
            spec: CalendarSpec::Weekly {
                days: vec![Weekday::Mon, Weekday::Fri],
                at: jiff::civil::time(17, 30, 0, 0),
            },
            zone: "UTC".into(),
            until: None,
            count: None,
        };
        let after = at("2026-07-16T12:00:00+00:00");
        assert_eq!(
            next_fire(&weekly, &after)?.unwrap(),
            at("2026-07-17T17:30:00+00:00")
        );

        // Monthly "on 31" clamps: after Jan 31 2027 comes Feb 28 2027.
        let monthly = Schedule::Calendar {
            spec: CalendarSpec::Monthly {
                days: vec![MonthDay::Day(31)],
                at: jiff::civil::time(9, 0, 0, 0),
            },
            zone: "UTC".into(),
            until: None,
            count: None,
        };
        let after = at("2027-01-31T10:00:00+00:00");
        assert_eq!(
            next_fire(&monthly, &after)?.unwrap(),
            at("2027-02-28T09:00:00+00:00")
        );

        // "on last" in a leap February.
        let last = Schedule::Calendar {
            spec: CalendarSpec::Monthly {
                days: vec![MonthDay::Last],
                at: jiff::civil::time(23, 0, 0, 0),
            },
            zone: "UTC".into(),
            until: None,
            count: None,
        };
        let after = at("2028-02-01T00:00:00+00:00");
        assert_eq!(
            next_fire(&last, &after)?.unwrap(),
            at("2028-02-29T23:00:00+00:00")
        );
        Ok(())
    }

    #[test]
    fn until_clips_and_once_is_one_shot() -> Result<()> {
        let anchor = at("2026-07-16T09:00:00+00:00");
        let schedule = Schedule::Every {
            interval: SignedDuration::from_secs(3600),
            anchor,
            until: Some(at("2026-07-16T10:00:00+00:00")),
            count: None,
        };
        // 10:00 is within until…
        assert!(next_fire(&schedule, &anchor)?.is_some());
        // …but nothing after it is.
        let at_until = at("2026-07-16T10:00:00+00:00");
        assert_eq!(next_fire(&schedule, &at_until)?, None);

        let once = Schedule::Once { at: anchor };
        let before = at("2026-07-16T08:00:00+00:00");
        assert_eq!(next_fire(&once, &before)?.unwrap(), anchor);
        assert_eq!(next_fire(&once, &anchor)?, None);
        Ok(())
    }
}
