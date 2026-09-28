//! The time grammar (DESIGN.md §9.1–§9.2): one parser, three result types.
//! Call sites ask for the type they expect — an instant (`at`, `Until`), a
//! duration (`remind "1h"`, `after`, `timeout`), or a calendar rule
//! (`every day 9am`). Same tokens everywhere; no per-flag dialects.
//!
//! Rules the implementation must honor (§9.1):
//! - durations: `s m h d w`, compounds (`1h30m`); NO `mo`/`yr` units
//! - numeric dates are ISO only; month names are the friendly form
//! - a past time-of-day rolls to the next occurrence; an explicitly dated
//!   past instant is an error — a year-less date (`jun 25`) counts as
//!   time-of-day-like and rolls to next year
//! - ambiguity is rejected with a suggestion, never guessed silently
//! - the caller always echoes the resolved interpretation back to the user
//!   (`describe_instant` renders the §9.1 echo line body)
//!
//! DST resolution happens where civil datetimes become zoned: jiff's
//! compatible disambiguation — a spring-forward gap shifts forward by the
//! gap's length, a fall-back repeat takes its first occurrence.

use anyhow::{Context, Result, bail, ensure};
use jiff::civil::{Date, DateTime, Time};
use jiff::{SignedDuration, Span, Timestamp, Zoned};

use crate::model::{CalendarSpec, MonthDay, Weekday};

// ---------------------------------------------------------------------------
// Durations (§9.1): elapsed real time, units s m h d w
// ---------------------------------------------------------------------------

/// "90m", "1h30m", "2w" → elapsed real time (suspend counts, §9).
pub fn parse_duration(input: &str) -> Result<SignedDuration> {
    let mut rest = input.trim();
    ensure!(!rest.is_empty(), "empty duration — try 90m, 1h30m, 2w");

    let mut seconds: i64 = 0;
    // Loop multiple times for cases like 1h30m, which require converting both 1h and 30m into seconds
    while !rest.is_empty() {
        // Find the range of the digit elements (0 - digits_end)
        let digits_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        // Cannot be ranged [0:0]
        ensure!(
            digits_end > 0,
            "expected a number in duration {input:?}, found {rest:?}"
        );
        // Extract the numerical value
        let value: i64 = rest[..digits_end]
            .parse()
            .with_context(|| format!("number out of range in duration {input:?}"))?;

        // Remove the collected value
        rest = &rest[digits_end..];

        if rest.starts_with('.') {
            bail!("no fractional durations — write smaller units instead (90m, not 1.5h)");
        }

        // Each numerical value should have a unit of measure. Find its range (0 - unit_end)
        let unit_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        // Collect unit
        let unit = rest[..unit_end].to_ascii_lowercase();

        // Remove the collected unit
        rest = rest[unit_end..].trim_start();

        // Translate unit into seconds
        let unit_seconds: i64 = match unit.as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "wk" | "wks" | "week" | "weeks" => 604_800,
            "" => bail!("missing unit after {value} in {input:?} — units are s m h d w"),
            "mo" | "mos" | "mon" | "month" | "months" | "y" | "yr" | "yrs" | "year" | "years" => {
                bail!(
                    "months and years aren't fixed lengths, so they aren't duration units \
                 (§9.1) — use a calendar rule instead, e.g. `every month on 1 at 9am`"
                )
            }
            other => bail!("unknown duration unit {other:?} in {input:?} — units are s m h d w"),
        };

        // Multiply collected value by the seconds-per-unit, then add to stored seconds value
        seconds = value
            .checked_mul(unit_seconds)
            .and_then(|v| seconds.checked_add(v))
            .with_context(|| format!("duration {input:?} overflows"))?;
    }
    // Return seconds
    Ok(SignedDuration::from_secs(seconds))
}

// ---------------------------------------------------------------------------
// Instants (§9.1)
// ---------------------------------------------------------------------------

/// What the date-ish tokens of an instant expression resolved to.
enum DateSpec {
    Today,
    Tomorrow,
    Weekday(Weekday),
    /// Year-less "jun 25" — rolls to next year if past (§9.1: only a *full*
    /// date in the past is an error).
    MonthDay {
        month: i8,
        day: i8,
    },
    /// ISO "2026-06-25" — explicitly dated; past is an error.
    Dated(Date),
}

/// "9am tomorrow", "in 90m", "2026-06-25 09:00" → a concrete zoned instant,
/// resolved against `now` (the injected clock, §11) in `now`'s zone.
/// The machine's own zone — what an unqualified time means, and what output
/// is rendered back into (§9).
pub fn local_zone() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::system()
}

/// The zone to interpret a submission in: the one the user named, else the
/// machine's. This is the whole of the override — "tell me a schedule in
/// Eastern while I live in Mountain" is `resolve_zone(Some("America/New_York"))`
/// and nothing else changes.
pub fn resolve_zone(name: Option<&str>) -> Result<jiff::tz::TimeZone> {
    match name {
        Some(name) => jiff::tz::TimeZone::get(name).with_context(|| {
            format!("unknown time zone {name:?} — use an IANA name like \"America/New_York\"")
        }),
        None => Ok(local_zone()),
    }
}

/// "Now", as seen from `zone` — the reference every relative and
/// wall-clock form is resolved against.
pub fn now_in(zone: &jiff::tz::TimeZone) -> Zoned {
    Timestamp::now().to_zoned(zone.clone())
}

/// Inbound half of the translation layer (§9): a user's words become a UTC
/// instant. `now` carries the zone to read them in — a bare "9am" means 9am
/// *there*, and the instant that comes back has no zone left on it.
pub fn parse_instant(input: &str, now: &Zoned) -> Result<Timestamp> {
    let trimmed = input.trim();
    ensure!(
        !trimmed.is_empty(),
        "empty time — try \"9am\", \"in 90m\", \"tomorrow 5pm\""
    );
    let lower = trimmed.to_ascii_lowercase();

    // Relative forms: "in 90m", "+2h", and a bare duration ("1h") where a
    // time is expected — all mean "that far from now" in elapsed real time.
    // Try the two explicit markers ("in ", "+") first; if neither is present,
    // see whether the whole input happens to read as a duration on its own.
    let relative = if let Some(rest) = lower.strip_prefix("in ") {
        Some(parse_duration(rest)?)
    } else if let Some(rest) = lower.strip_prefix('+') {
        Some(parse_duration(rest)?)
    } else {
        parse_duration(&lower).ok()
    };
    // If any relative form matched, the answer is simply now + duration.
    // checked_add works in absolute time, so DST transitions don't stretch
    // or shrink the wait.
    if let Some(duration) = relative {
        return now
            .timestamp()
            .checked_add(duration)
            .with_context(|| format!("{input:?} lands out of range"));
    }

    // Absolute forms: walk the whitespace-separated words and classify each
    // one as either a date part or a time part. Order doesn't matter
    // ("9am tomorrow" == "tomorrow 9am"), but each slot may only be filled
    // once — set_date/set_time reject a second date or a second time.
    let mut date: Option<DateSpec> = None;
    let mut time: Option<Time> = None;
    let mut tokens = trimmed.split_whitespace().peekable();
    while let Some(raw) = tokens.next() {
        // Keywords are compared case-insensitively; keep the original for
        // error messages.
        let tok = raw.to_ascii_lowercase();
        // "at" is connective filler ("tomorrow at 9am") — skip it.
        if tok == "at" {
            continue;
        }
        // Slash dates are banned outright: 6/25 vs 25/6 can't be told apart.
        if tok.contains('/') {
            bail!(
                "{raw:?} is ambiguous (6/25 vs 25/6) — numeric dates are ISO only \
                 (2026-06-25), or use a month name (jun 25)"
            );
        }
        // Try each date-shaped reading first, then time-shaped, then fail.
        if tok == "today" {
            set_date(&mut date, DateSpec::Today, raw)?;
        } else if tok == "tomorrow" {
            set_date(&mut date, DateSpec::Tomorrow, raw)?;
        } else if let Some(weekday) = weekday_word(&tok) {
            // A weekday name ("mon", "friday") — which calendar date it
            // means is decided later, in resolve_instant.
            set_date(&mut date, DateSpec::Weekday(weekday), raw)?;
        } else if let Some(month) = month_word(&tok) {
            // A month name consumes the FOLLOWING token as its day number:
            // "jun 25" is one date spread across two words.
            let day_tok = tokens
                .next()
                .with_context(|| format!("{raw} needs a day — e.g. \"{raw} 25\""))?;
            let day: i8 = day_tok
                .parse()
                .ok()
                .filter(|d| (1..=31).contains(d))
                .with_context(|| format!("can't read {day_tok:?} as a day of the month"))?;
            set_date(&mut date, DateSpec::MonthDay { month, day }, raw)?;
        } else if iso_shaped(&tok) {
            // Four digits and a dash: an ISO date. With a 'T' in it, it's a
            // full datetime in a single token ("2026-06-25T09:00") and fills
            // both the date slot and the time slot at once.
            if tok.contains('t') {
                let dt: DateTime = raw.parse().with_context(|| {
                    format!("can't read {raw:?} as an ISO datetime (2026-06-25T09:00)")
                })?;
                set_date(&mut date, DateSpec::Dated(dt.date()), raw)?;
                set_time(&mut time, dt.time(), raw)?;
            } else {
                let d: Date = raw
                    .parse()
                    .with_context(|| format!("can't read {raw:?} as an ISO date (YYYY-MM-DD)"))?;
                set_date(&mut date, DateSpec::Dated(d), raw)?;
            }
        } else if let Some(t) = time_token(&tok)? {
            // A time of day ("9am", "17:00", "noon").
            set_time(&mut time, t, raw)?;
        } else if tok.bytes().all(|b| b.is_ascii_digit()) {
            // A bare number reached here without being claimed by a month
            // name — "9" alone could mean 9am, 9pm, or the 9th; refuse to
            // guess (§9.1).
            bail!("bare number {raw:?} is ambiguous — write {raw}am, {raw}pm, or {raw}:00");
        } else {
            bail!(
                "can't understand {raw:?} in a time — forms: \"9am\", \"tomorrow 5pm\", \
                 \"mon 9am\", \"jun 25 17:00\", \"2026-06-25 09:00\", \"in 90m\""
            );
        }
    }

    // Every word is now classified; turn the (date?, time?) pair into one
    // concrete instant.
    resolve_instant(date, time, now, input)
}

/// The §9.1 resolution rules: roll a past time-of-day forward, error on an
/// explicitly dated past instant.
fn resolve_instant(
    date: Option<DateSpec>,
    time: Option<Time>,
    now: &Zoned,
    input: &str,
) -> Result<Timestamp> {
    // Glue a calendar date and a wall-clock time together in now's zone.
    // This is the single point where DST resolution happens: a nonexistent
    // local time shifts forward by the gap, an ambiguous one takes its
    // first occurrence (module docs).
    // …and this is where a wall-clock reading becomes an instant: the one
    // conversion the inbound half of the translation layer performs.
    let zoned = |date: Date, time: Time| -> Result<Timestamp> {
        Ok(DateTime::from_parts(date, time)
            .to_zoned(now.time_zone().clone())
            .with_context(|| format!("{input:?} lands out of range"))?
            .timestamp())
    };

    // Each arm below pairs one kind of date part with the presence/absence
    // of a time part and applies that combination's resolution rule.
    match (date, time) {
        (None, None) => bail!("no time found in {input:?} — try \"9am\" or \"tomorrow 5pm\""),

        // Bare time-of-day: today, rolling to tomorrow if already past.
        // Build today's candidate; if the clock has already passed it, the
        // user means the next one — same time tomorrow.
        (None, Some(t)) => {
            let today = zoned(now.date(), t)?;
            if today > now.timestamp() {
                Ok(today)
            } else {
                zoned(now.date().tomorrow().context("date out of range")?, t)
            }
        }

        // "today 9am": the user pinned the day explicitly, so a past time
        // can't roll — that would contradict the word they typed.
        (Some(DateSpec::Today), Some(t)) => {
            let candidate = zoned(now.date(), t)?;
            ensure!(
                candidate > now.timestamp(),
                "\"today {t}\" is already past — drop \"today\" and it rolls to tomorrow"
            );
            Ok(candidate)
        }
        // "tomorrow 9am": always in the future by construction; no checks.
        (Some(DateSpec::Tomorrow), Some(t)) => {
            zoned(now.date().tomorrow().context("date out of range")?, t)
        }

        // Rejected-with-a-suggestion rather than guessed (§9.1): a day word
        // without a time has no obvious meaning.
        (Some(DateSpec::Today), None) => bail!("\"today\" needs a time — try \"today 5pm\""),
        (Some(DateSpec::Tomorrow), None) => {
            bail!("\"tomorrow\" needs a time — try \"tomorrow 9am\"")
        }
        (Some(DateSpec::Weekday(_)), None) => {
            bail!("a weekday needs a time — try \"mon 9am\"")
        }

        // Next occurrence of that weekday, today included if still future.
        (Some(DateSpec::Weekday(weekday)), Some(t)) => {
            let target = jiff::civil::Weekday::from(weekday);
            // Days until the target weekday, counting on a Monday=0 wheel:
            // subtracting today's position can go negative (target already
            // passed this week), so wrap with rem_euclid into 0–6. 0 means
            // the target IS today.v
            let ahead = (target.to_monday_zero_offset()
                - now.date().weekday().to_monday_zero_offset())
            .rem_euclid(7);
            // Step forward that many days to land on the right weekday.
            let date = now
                .date()
                .checked_add(Span::new().days(i64::from(ahead)))
                .context("date out of range")?;
            let candidate = zoned(date, t)?;
            // If that instant already passed (only possible when ahead == 0,
            // i.e. today, with the time gone by), take the same weekday one
            // week later.
            if candidate > now.timestamp() {
                Ok(candidate)
            } else {
                zoned(
                    date.checked_add(Span::new().days(7))
                        .context("date out of range")?,
                    t,
                )
            }
        }

        // Year-less date: this year, rolling to next year if past.
        (Some(DateSpec::MonthDay { month, day }), t) => {
            // No time given means the start of that day.
            let t = t.unwrap_or(Time::midnight());
            let year = now.date().year();
            // Build the date in the current year first. Date::new rejects
            // impossible dates ("jun 31") right here.
            let this_year = Date::new(year, month, day)
                .with_context(|| format!("there's no {} {day}", month_name(month)))?;
            let candidate = zoned(this_year, t)?;
            if candidate > now.timestamp() {
                Ok(candidate)
            } else {
                // Already behind us this year → the user means the next one.
                // Rebuilding the date can fail for exactly one input: feb 29
                // rolling into a non-leap year.
                let next_year = Date::new(year + 1, month, day).with_context(|| {
                    format!(
                        "{} {day} doesn't exist in {} — use an ISO date with a year",
                        month_name(month),
                        year + 1
                    )
                })?;
                zoned(next_year, t)
            }
        }

        // Explicitly dated: the past is an error, never a roll (§9.1 — a
        // full date in the past is a typo, not an intent).
        (Some(DateSpec::Dated(d)), t) => {
            // Remember whether the user gave a time, purely to tailor the
            // error message below.
            let had_time = t.is_some();
            let candidate = zoned(d, t.unwrap_or(Time::midnight()))?;
            ensure!(
                candidate > now.timestamp(),
                // Shown back in the zone the input was read against — an
                // instant has no wall clock of its own to quote (§9).
                "{} is in the past — explicit dates don't roll forward{}",
                candidate
                    .to_zoned(now.time_zone().clone())
                    .strftime("%Y-%m-%d %H:%M %Z"),
                if had_time {
                    ""
                } else {
                    "; a bare date means midnight — add a time if you meant later that day"
                }
            );
            Ok(candidate)
        }
    }
}

// ---------------------------------------------------------------------------
// Calendar rules (§9.2 v1 tier: daily / weekly / monthly-by-date)
// ---------------------------------------------------------------------------

/// "day 09:00", "weekdays 9am", "month on 1,15 at 9am" → a calendar rule.
/// A leading "every" is tolerated (call sites may pass the full phrase).
pub fn parse_calendar(input: &str) -> Result<CalendarSpec> {
    const FORMS: &str = "forms: \"day 9am\", \"weekdays 9am\", \"mon,wed,fri 17:30\", \
                         \"month on 1,15 at 9am\", \"month on last at 23:00\"";
    // Calendar rules have no case-sensitive parts, so lowercase the whole
    // thing once and split into words.
    let lower = input.trim().to_ascii_lowercase();
    let mut tokens: Vec<&str> = lower.split_whitespace().collect();
    // Callers may pass the full phrase ("every day 9am"); drop the "every".
    if tokens.first() == Some(&"every") {
        tokens.remove(0);
    }
    ensure!(!tokens.is_empty(), "empty calendar rule — {FORMS}");

    // The first word picks the rule family; the rest of each arm collects
    // that family's day part, then hands the remaining tokens to tail_time
    // for the mandatory time.
    match tokens[0] {
        // "day 9am": no day part to collect — every day is implied.
        "day" | "daily" => Ok(CalendarSpec::Daily {
            at: tail_time(&tokens[1..], "day at 9am")?,
        }),
        // "weekdays"/"weekends" are sugar for fixed weekly day sets.
        "weekdays" => Ok(CalendarSpec::Weekly {
            days: vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ],
            at: tail_time(&tokens[1..], "weekdays 9am")?,
        }),
        "weekends" => Ok(CalendarSpec::Weekly {
            days: vec![Weekday::Sat, Weekday::Sun],
            at: tail_time(&tokens[1..], "weekends 10am")?,
        }),
        // "month on 1,15 at 9am": the day part is a list of month days
        // (numbers or "last") introduced by a mandatory "on".
        "month" | "monthly" => {
            ensure!(
                tokens.get(1) == Some(&"on"),
                "monthly rules are \"month on <days> at <time>\" — e.g. \"month on 1,15 at 9am\""
            );
            // Collect list items after the "on"; each piece is either the
            // "last" keyword or a day number that must fit in 1–31.
            let (days, rest) = gather_list(&tokens[2..], |piece| {
                if piece == "last" {
                    return Ok(MonthDay::Last);
                }
                piece
                    .parse::<u8>()
                    .ok()
                    .filter(|d| (1..=31).contains(d))
                    .map(MonthDay::Day)
                    .with_context(|| {
                        format!("{piece:?} is not a day of the month — 1–31 or \"last\"")
                    })
            })?;
            ensure!(
                !days.is_empty(),
                "\"month on\" needs at least one day — e.g. \"month on 1,15 at 9am\""
            );
            Ok(CalendarSpec::Monthly {
                days,
                at: tail_time(rest, "month on 1 at 9am")?,
            })
        }
        // Anything else must be a weekday list ("mon,wed,fri 17:30") — the
        // only family without a leading keyword.
        _ => {
            let (days, rest) = gather_list(&tokens, |piece| {
                weekday_word(piece).with_context(|| {
                    format!("can't understand {piece:?} in a calendar rule — {FORMS}")
                })
            })?;
            ensure!(
                !days.is_empty(),
                "a calendar rule needs a day part — {FORMS}"
            );
            Ok(CalendarSpec::Weekly {
                days,
                at: tail_time(rest, "mon,wed,fri 17:30")?,
            })
        }
    }
}

/// Comma-separated list items from the leading tokens (handles both
/// "mon,wed,fri" and "mon, wed, fri"), deduped, order preserved. Returns the
/// unconsumed tail (which should be the time).
fn gather_list<'t, T: PartialEq>(
    tokens: &'t [&'t str],
    parse_item: impl Fn(&str) -> Result<T>,
) -> Result<(Vec<T>, &'t [&'t str])> {
    let mut items = Vec::new();
    let mut consumed = 0;
    for tok in tokens {
        // The list ends where the time part begins — either an explicit
        // "at" or the first token that reads as a time.
        if *tok == "at" || time_token(tok)?.is_some() {
            break;
        }
        // A single token may hold several items ("mon,wed,fri"); split on
        // commas. "mon, wed" arrives as tokens "mon," and "wed" — the split
        // then leaves an empty piece after "mon", which the filter drops.
        for piece in tok.split(',').filter(|p| !p.is_empty()) {
            let item = parse_item(piece)?;
            // Ignore repeats ("mon,mon") instead of storing them twice.
            if !items.contains(&item) {
                items.push(item);
            }
        }
        consumed += 1;
    }
    // Hand back what's left after the list — the caller expects the time.
    Ok((items, &tokens[consumed..]))
}

/// The trailing time of a calendar rule, with an optional "at" before it.
fn tail_time(tokens: &[&str], example: &str) -> Result<Time> {
    // Skip the connective "at" if present ("month on 1 AT 9am").
    let tokens = if tokens.first() == Some(&"at") {
        &tokens[1..]
    } else {
        tokens
    };
    // After the day part and the optional "at", exactly one token — the
    // time — may remain. None is a missing time; more than one is junk.
    match tokens {
        [] => bail!("calendar rule needs a time — e.g. \"{example}\""),
        [tok] => time_token(tok)?
            .with_context(|| format!("can't read {tok:?} as a time — try 9am or 17:00")),
        [_, extra @ ..] => bail!(
            "unexpected trailing input {:?} in calendar rule",
            extra.join(" ")
        ),
    }
}

// ---------------------------------------------------------------------------
// The echo rule (§9.1): render what the parser decided
// ---------------------------------------------------------------------------

/// "Thu 2026-07-16 09:00 EDT (in 6d 23h)" — the body of the echo line the
/// CLI prints at submit, so every judgment call is immediately visible.
/// Outbound half of the translation layer (§9): a stored UTC instant becomes
/// words, in `zone`. Taking the zone as an argument is the point — an instant
/// has none, so every rendering site has to say which one it means rather
/// than inheriting whichever happened to be attached.
pub fn describe_instant(instant: &Timestamp, now: &Timestamp, zone: &jiff::tz::TimeZone) -> String {
    // The absolute half: weekday, date, wall-clock time, zone abbreviation.
    let local = instant.to_zoned(zone.clone());
    let stamp = local.strftime("%a %Y-%m-%d %H:%M %Z");
    // The relative half: how far from now, as a signed number of seconds.
    // The sign picks the phrasing ("in …" vs "… ago"); the magnitude gets
    // humanized.
    let seconds = instant.as_second() - now.as_second();
    if seconds.abs() < 1 {
        return format!("{stamp} (now)");
    }
    let human = describe_duration(SignedDuration::from_secs(seconds.abs()));
    if seconds > 0 {
        format!("{stamp} (in {human})")
    } else {
        format!("{stamp} ({human} ago)")
    }
}

/// Humanize a duration to its two largest units: "6d 23h", "1h 30m", "45s".
pub fn describe_duration(duration: SignedDuration) -> String {
    let mut seconds = duration.as_secs().abs();
    let units = [
        ("w", 604_800),
        ("d", 86_400),
        ("h", 3600),
        ("m", 60),
        ("s", 1),
    ];
    // Greedy conversion from the largest unit down: divide out how many of
    // the unit fit, keep the remainder for the smaller units. Stop after
    // two parts — "6d 23h" reads well, "6d 23h 12m 9s" is noise.
    let mut parts = Vec::new();
    for (name, unit_seconds) in units {
        if seconds >= unit_seconds && parts.len() < 2 {
            parts.push(format!("{}{name}", seconds / unit_seconds));
            seconds %= unit_seconds;
        }
    }
    // Nothing accumulated means a sub-second duration — call it 0s.
    if parts.is_empty() {
        "0s".to_string()
    } else {
        parts.join(" ")
    }
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

// Fill the date/time slot, refusing if something already claimed it — a
// second date ("mon 9am tue") or second time ("9am 5pm") is a contradiction,
// not something to silently overwrite.
fn set_date(slot: &mut Option<DateSpec>, value: DateSpec, raw: &str) -> Result<()> {
    ensure!(
        slot.is_none(),
        "two dates in one time expression (at {raw:?})"
    );
    *slot = Some(value);
    Ok(())
}

fn set_time(slot: &mut Option<Time>, value: Time, raw: &str) -> Result<()> {
    ensure!(
        slot.is_none(),
        "two times in one time expression (at {raw:?})"
    );
    *slot = Some(value);
    Ok(())
}

/// Ok(None): not time-shaped (caller keeps classifying). Err: time-shaped
/// but invalid ("13pm"). A bare number ("9") is NOT a time — ambiguous,
/// rejected upstream with a suggestion (§9.1).
fn time_token(token: &str) -> Result<Option<Time>> {
    // The two word-form times need no digits at all.
    match token {
        "noon" => return Ok(Some(Time::new(12, 0, 0, 0).expect("noon is valid"))),
        "midnight" => return Ok(Some(Time::midnight())),
        _ => {}
    }
    // Peel off a trailing am/pm marker, remembering which one (false = am,
    // true = pm) — it changes how the hour is interpreted below.
    let (body, meridiem) = if let Some(b) = token.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = token.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (token, None)
    };
    // Whatever remains must be digits and colons only; anything else means
    // this token isn't a time at all — hand it back for reclassification.
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return Ok(None);
    }
    // Digits with no colon and no am/pm is a bare number ("9") — could be
    // 9am, 9pm, or a day of the month, so it is NOT a time (§9.1); the
    // caller rejects it with a suggestion.
    if meridiem.is_none() && !body.contains(':') {
        return Ok(None);
    }

    // Split on colons into hour[:minute[:second]] — at most three fields,
    // none of them empty (catches "9:" and "9::30").
    let parts: Vec<&str> = body.split(':').collect();
    ensure!(
        parts.len() <= 3 && parts.iter().all(|p| !p.is_empty()),
        "can't read {token:?} as a time — try 9am, 9:30pm, or 17:00"
    );
    let field = |s: &str| -> Result<i8> {
        s.parse()
            .with_context(|| format!("can't read {token:?} as a time"))
    };
    // Missing minute/second fields default to zero ("9am" = 9:00:00).
    let mut hour = field(parts[0])?;
    let minute = if parts.len() > 1 { field(parts[1])? } else { 0 };
    let second = if parts.len() > 2 { field(parts[2])? } else { 0 };
    // Convert a 12-hour clock reading to 24-hour: the special case is 12
    // itself — 12am is hour 0, 12pm stays 12; otherwise pm adds 12.
    if let Some(pm) = meridiem {
        ensure!((1..=12).contains(&hour), "{token:?}: am/pm hours run 1–12");
        if pm && hour != 12 {
            hour += 12;
        } else if !pm && hour == 12 {
            hour = 0;
        }
    }
    // Time::new validates the ranges (hour 0–23, minute/second 0–59), so
    // "25:00" and "9:75" fail here.
    Time::new(hour, minute, second, 0)
        .map(Some)
        .with_context(|| format!("{token:?} is out of range for a time of day"))
}

fn weekday_word(token: &str) -> Option<Weekday> {
    Some(match token {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn month_word(token: &str) -> Option<i8> {
    Some(match token {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    })
}

fn month_name(month: i8) -> &'static str {
    match month {
        1 => "jan",
        2 => "feb",
        3 => "mar",
        4 => "apr",
        5 => "may",
        6 => "jun",
        7 => "jul",
        8 => "aug",
        9 => "sep",
        10 => "oct",
        11 => "nov",
        12 => "dec",
        _ => "?",
    }
}

/// "2026-…" — four digits then a dash. Slash dates were already rejected.
fn iso_shaped(token: &str) -> bool {
    let b = token.as_bytes();
    b.len() > 4 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-'
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    fn tz() -> TimeZone {
        TimeZone::get("America/New_York").expect("tzdb has America/New_York")
    }

    /// A wall-clock reading in the test zone — the *input* side.
    fn wall(y: i16, mo: i8, d: i8, h: i8, min: i8) -> Zoned {
        date(y, mo, d).at(h, min, 0, 0).to_zoned(tz()).unwrap()
    }

    /// The same moment as the instant `parse_instant` now returns — the
    /// *output* side. Written as a wall clock because that is how the test
    /// reads; compared as UTC because that is what is stored.
    fn at(y: i16, mo: i8, d: i8, h: i8, min: i8) -> Timestamp {
        wall(y, mo, d, h, min).timestamp()
    }

    /// Thu 2026-07-09 10:00 EDT — every instant test resolves against this.
    fn now() -> Zoned {
        wall(2026, 7, 9, 10, 0)
    }

    fn secs(input: &str) -> i64 {
        parse_duration(input).unwrap().as_secs()
    }

    fn err(result: Result<impl std::fmt::Debug>) -> String {
        format!("{:#}", result.unwrap_err())
    }

    // -- durations ----------------------------------------------------------

    #[test]
    fn durations_parse() {
        assert_eq!(secs("90m"), 5400);
        assert_eq!(secs("1h30m"), 5400);
        assert_eq!(secs("1h 30m"), 5400);
        assert_eq!(secs("2w"), 1_209_600);
        assert_eq!(secs("1d"), 86_400);
        assert_eq!(secs("45s"), 45);
        assert_eq!(secs("10min"), 600);
        assert_eq!(secs("2hrs"), 7200);
    }

    #[test]
    fn durations_compound_across_many_segments() {
        // Any number of number+unit pairs concatenate and sum.
        assert_eq!(secs("2d1h30m100s"), 2 * 86_400 + 3600 + 1800 + 100);
        assert_eq!(secs("2d 1h 30m 100s"), secs("2d1h30m100s"));
        assert_eq!(
            secs("1w2d3h4m5s"),
            604_800 + 2 * 86_400 + 3 * 3600 + 4 * 60 + 5
        );
        // Segments need not be in descending order, and units may repeat.
        assert_eq!(secs("30m1h"), 5400);
        assert_eq!(secs("1h1h"), 7200);
        // Values larger than the next unit up are fine — it's all just seconds.
        assert_eq!(secs("100s"), 100);
        assert_eq!(secs("36h"), 129_600);
        // A malformed segment anywhere fails the whole parse.
        assert!(parse_duration("1h30").is_err());
        assert!(parse_duration("1h x 30m").is_err());
        assert!(parse_duration("1h30mo").is_err());
    }

    #[test]
    fn durations_reject_month_year_fractions_and_junk() {
        assert!(err(parse_duration("1mo")).contains("calendar rule"));
        assert!(err(parse_duration("2years")).contains("calendar rule"));
        assert!(err(parse_duration("1.5h")).contains("fractional"));
        assert!(err(parse_duration("90")).contains("missing unit"));
        assert!(err(parse_duration("1x")).contains("unknown duration unit"));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
    }

    // -- instants: relative -------------------------------------------------

    #[test]
    fn relative_instants() {
        let now = now();
        assert_eq!(
            parse_instant("in 90m", &now).unwrap(),
            at(2026, 7, 9, 11, 30)
        );
        assert_eq!(parse_instant("+2h", &now).unwrap(), at(2026, 7, 9, 12, 0));
        // bare duration where a time is expected = that far from now
        assert_eq!(parse_instant("1h", &now).unwrap(), at(2026, 7, 9, 11, 0));
    }

    // -- instants: time-of-day rolls forward ---------------------------------

    #[test]
    fn time_of_day_rolls_to_next_occurrence() {
        let now = now(); // 10:00
        assert_eq!(parse_instant("9am", &now).unwrap(), at(2026, 7, 10, 9, 0));
        assert_eq!(parse_instant("11am", &now).unwrap(), at(2026, 7, 9, 11, 0));
        assert_eq!(parse_instant("17:00", &now).unwrap(), at(2026, 7, 9, 17, 0));
        assert_eq!(parse_instant("noon", &now).unwrap(), at(2026, 7, 9, 12, 0));
        assert_eq!(
            parse_instant("midnight", &now).unwrap(),
            at(2026, 7, 10, 0, 0)
        );
        assert_eq!(
            parse_instant("9:30pm", &now).unwrap(),
            at(2026, 7, 9, 21, 30)
        );
    }

    #[test]
    fn day_words_combine_in_any_order() {
        let now = now();
        assert_eq!(
            parse_instant("tomorrow 5pm", &now).unwrap(),
            at(2026, 7, 10, 17, 0)
        );
        assert_eq!(
            parse_instant("9am tomorrow", &now).unwrap(),
            at(2026, 7, 10, 9, 0)
        );
        assert_eq!(
            parse_instant("tomorrow at 9am", &now).unwrap(),
            at(2026, 7, 10, 9, 0)
        );
        assert_eq!(
            parse_instant("today 5pm", &now).unwrap(),
            at(2026, 7, 9, 17, 0)
        );
    }

    #[test]
    fn weekdays_mean_next_occurrence() {
        let now = now(); // Thursday 10:00
        assert_eq!(
            parse_instant("mon 9am", &now).unwrap(),
            at(2026, 7, 13, 9, 0)
        );
        // today's weekday, time already past → next week
        assert_eq!(
            parse_instant("thu 9am", &now).unwrap(),
            at(2026, 7, 16, 9, 0)
        );
        // today's weekday, time still future → today
        assert_eq!(
            parse_instant("thu 11am", &now).unwrap(),
            at(2026, 7, 9, 11, 0)
        );
        assert_eq!(
            parse_instant("monday 9am", &now).unwrap(),
            at(2026, 7, 13, 9, 0)
        );
    }

    // -- instants: dated ------------------------------------------------------

    #[test]
    fn iso_dates_are_exact_and_do_not_roll() {
        let now = now();
        assert_eq!(
            parse_instant("2026-12-25 09:00", &now).unwrap(),
            at(2026, 12, 25, 9, 0)
        );
        assert_eq!(
            parse_instant("2026-12-25T09:00", &now).unwrap(),
            at(2026, 12, 25, 9, 0)
        );
        // bare ISO date = midnight
        assert_eq!(
            parse_instant("2026-12-25", &now).unwrap(),
            at(2026, 12, 25, 0, 0)
        );
        // explicitly dated past instant is an error, not a roll (§9.1)
        assert!(err(parse_instant("2026-01-01 09:00", &now)).contains("past"));
        assert!(err(parse_instant("2026-07-09", &now)).contains("add a time"));
    }

    #[test]
    fn yearless_month_dates_roll_to_next_year() {
        let now = now(); // 2026-07-09
        assert_eq!(
            parse_instant("jun 25 9am", &now).unwrap(),
            at(2027, 6, 25, 9, 0)
        );
        assert_eq!(
            parse_instant("dec 25", &now).unwrap(),
            at(2026, 12, 25, 0, 0)
        );
        assert_eq!(
            parse_instant("jul 10 9am", &now).unwrap(),
            at(2026, 7, 10, 9, 0)
        );
        assert!(parse_instant("jun 31 9am", &now).is_err());
    }

    // -- instants: ambiguity is rejected with a suggestion --------------------

    #[test]
    fn ambiguous_input_is_rejected_with_suggestions() {
        let now = now();
        assert!(err(parse_instant("6/25", &now)).contains("ISO"));
        assert!(err(parse_instant("9", &now)).contains("9am"));
        assert!(err(parse_instant("tomorrow", &now)).contains("tomorrow 9am"));
        assert!(err(parse_instant("mon", &now)).contains("mon 9am"));
        assert!(err(parse_instant("today", &now)).contains("today 5pm"));
        assert!(err(parse_instant("today 9am", &now)).contains("already past"));
        assert!(err(parse_instant("mon 9am tue", &now)).contains("two dates"));
        assert!(err(parse_instant("9am 5pm", &now)).contains("two times"));
        assert!(parse_instant("banana", &now).is_err());
    }

    // -- instants: DST (§9) ---------------------------------------------------

    #[test]
    fn dst_gap_shifts_forward_and_fold_takes_first_occurrence() {
        // The instant comes back bare, so reading its wall clock means
        // naming a zone — which is the whole point of the split (§9).
        let civil = |t: Timestamp| t.to_zoned(tz());

        // Spring forward (US 2026): 2026-03-08 02:00 EST → 03:00 EDT.
        let before_gap = wall(2026, 3, 7, 12, 0);
        let gap = civil(parse_instant("tomorrow 2:30am", &before_gap).unwrap());
        assert_eq!(gap.datetime(), date(2026, 3, 8).at(3, 30, 0, 0));
        assert_eq!(gap.offset(), jiff::tz::offset(-4)); // EDT

        // Fall back (US 2026): 2026-11-01 01:30 happens twice; first wins.
        let before_fold = wall(2026, 10, 31, 12, 0);
        let fold = civil(parse_instant("tomorrow 1:30am", &before_fold).unwrap());
        assert_eq!(fold.datetime(), date(2026, 11, 1).at(1, 30, 0, 0));
        assert_eq!(fold.offset(), jiff::tz::offset(-4)); // still EDT: first occurrence
    }

    #[test]
    fn durations_are_elapsed_real_time_across_dst() {
        // 24h before the spring-forward gap lands at civil 13:00, not 12:00 —
        // elapsed real time, not calendar arithmetic (§9).
        let before_gap = wall(2026, 3, 7, 12, 0);
        let after = parse_instant("in 24h", &before_gap).unwrap().to_zoned(tz());
        assert_eq!(after.datetime(), date(2026, 3, 8).at(13, 0, 0, 0));
    }

    // -- calendar rules (§9.2) -------------------------------------------------

    #[test]
    fn calendar_daily_weekly_monthly() {
        let nine = Time::new(9, 0, 0, 0).unwrap();
        assert_eq!(
            parse_calendar("day 09:00").unwrap(),
            CalendarSpec::Daily { at: nine }
        );
        assert_eq!(
            parse_calendar("every day at 9am").unwrap(),
            CalendarSpec::Daily { at: nine }
        );
        assert_eq!(
            parse_calendar("weekdays 9am").unwrap(),
            CalendarSpec::Weekly {
                days: vec![
                    Weekday::Mon,
                    Weekday::Tue,
                    Weekday::Wed,
                    Weekday::Thu,
                    Weekday::Fri
                ],
                at: nine,
            }
        );
        assert_eq!(
            parse_calendar("weekends at 10:00").unwrap(),
            CalendarSpec::Weekly {
                days: vec![Weekday::Sat, Weekday::Sun],
                at: Time::new(10, 0, 0, 0).unwrap(),
            }
        );
        assert_eq!(
            parse_calendar("mon,wed,fri 17:30").unwrap(),
            CalendarSpec::Weekly {
                days: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                at: Time::new(17, 30, 0, 0).unwrap(),
            }
        );
        // spaces after commas are fine
        assert_eq!(
            parse_calendar("mon, wed at 5:30pm").unwrap(),
            CalendarSpec::Weekly {
                days: vec![Weekday::Mon, Weekday::Wed],
                at: Time::new(17, 30, 0, 0).unwrap(),
            }
        );
        assert_eq!(
            parse_calendar("month on 1,15 at 9am").unwrap(),
            CalendarSpec::Monthly {
                days: vec![MonthDay::Day(1), MonthDay::Day(15)],
                at: nine,
            }
        );
        assert_eq!(
            parse_calendar("month on last at 23:00").unwrap(),
            CalendarSpec::Monthly {
                days: vec![MonthDay::Last],
                at: Time::new(23, 0, 0, 0).unwrap(),
            }
        );
        // 31 is valid — short months clamp at fire time (§9.2)
        assert_eq!(
            parse_calendar("month on 31 at 9am").unwrap(),
            CalendarSpec::Monthly {
                days: vec![MonthDay::Day(31)],
                at: nine,
            }
        );
    }

    #[test]
    fn calendar_rejects_bad_forms() {
        assert!(err(parse_calendar("month on 32 at 9am")).contains("1–31"));
        assert!(err(parse_calendar("day")).contains("needs a time"));
        assert!(err(parse_calendar("9am")).contains("day part"));
        assert!(err(parse_calendar("month 1 at 9am")).contains("month on"));
        assert!(err(parse_calendar("mon,wed 9am extra")).contains("trailing"));
        assert!(parse_calendar("").is_err());
        assert!(parse_calendar("30m").is_err()); // an interval, not a calendar rule
    }

    // -- the echo rule (§9.1) ----------------------------------------------------

    #[test]
    fn echo_renders_resolved_interpretation() {
        let now = now();
        let echoed = describe_instant(&at(2026, 7, 16, 9, 0), &now.timestamp(), &tz());
        assert_eq!(echoed, "Thu 2026-07-16 09:00 EDT (in 6d 23h)");
        assert_eq!(
            describe_instant(&at(2026, 7, 9, 9, 0), &now.timestamp(), &tz()),
            "Thu 2026-07-09 09:00 EDT (1h ago)"
        );
        assert_eq!(describe_duration(SignedDuration::from_secs(5400)), "1h 30m");
        assert_eq!(describe_duration(SignedDuration::from_secs(45)), "45s");
        assert_eq!(describe_duration(SignedDuration::from_secs(0)), "0s");
    }
}
