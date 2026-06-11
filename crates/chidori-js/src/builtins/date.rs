//! The `Date` builtin.
//!
//! Determinism contract: this engine has no host clock, so the time-of-
//! construction sentinel (`new Date()` / `Date.now()`) returns a *fixed* epoch
//! value of `0.0` rather than a wall-clock reading. This keeps run-to-suspend
//! agent programs reproducible.
//!
//! Time zone: we treat **local time == UTC**. `getTimezoneOffset()` therefore
//! always returns `0`, and every `getX` / `getUTCX` pair is identical. This is
//! deterministic and avoids depending on a host TZ database.
//!
//! Internally a Date carries `Internal::Date(ms)` where `ms` is milliseconds
//! since the Unix epoch (`NaN` == Invalid Date).

use super::arg;
use crate::value::*;
use crate::vm::Vm;

// =========================================================================
// Time constants (per spec)
// =========================================================================

const MS_PER_SECOND: f64 = 1000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_DAY: f64 = 86_400_000.0;

/// TimeClip (spec 21.4.1.31): finite and within +/- 8.64e15, else NaN.
fn time_clip(t: f64) -> f64 {
    if !t.is_finite() || t.abs() > 8.64e15 {
        f64::NAN
    } else {
        // The spec maps -0 to +0 here.
        let t = t.trunc();
        if t == 0.0 {
            0.0
        } else {
            t
        }
    }
}

// =========================================================================
// Civil-date <-> days-since-epoch (Howard Hinnant's algorithms).
// Valid for the full proleptic Gregorian calendar range we care about.
// =========================================================================

/// Number of days since 1970-01-01 for the given civil (year, month[1..=12],
/// day) triple. `y`/`m`/`d` may be out of normal range only insofar as the
/// caller has already normalized them; this expects month in 1..=12.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`: returns (year, month[1..=12], day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Floor-division of `a` by `b` (b > 0), matching the spec's use of `floor`.
fn floor_div(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

/// Positive-modulo matching the spec's `x - floor(x / y) * y`.
fn modulo(a: f64, b: f64) -> f64 {
    a - floor_div(a, b) * b
}

// ---- Decomposition (all in UTC; local == UTC for this engine) ----

fn day(t: f64) -> f64 {
    floor_div(t, MS_PER_DAY)
}

fn year_from_time(t: f64) -> f64 {
    let (y, _, _) = civil_from_days(day(t) as i64);
    y as f64
}

fn month_from_time(t: f64) -> f64 {
    let (_, m, _) = civil_from_days(day(t) as i64);
    (m - 1) as f64 // spec months are 0-based
}

fn date_from_time(t: f64) -> f64 {
    let (_, _, d) = civil_from_days(day(t) as i64);
    d as f64
}

/// 0 = Sunday .. 6 = Saturday. 1970-01-01 was a Thursday (4).
fn week_day(t: f64) -> f64 {
    modulo(day(t) + 4.0, 7.0)
}

fn hours_from_time(t: f64) -> f64 {
    modulo(floor_div(t, MS_PER_HOUR), 24.0)
}
fn min_from_time(t: f64) -> f64 {
    modulo(floor_div(t, MS_PER_MINUTE), 60.0)
}
fn sec_from_time(t: f64) -> f64 {
    modulo(floor_div(t, MS_PER_SECOND), 60.0)
}
fn ms_from_time(t: f64) -> f64 {
    modulo(t, MS_PER_SECOND)
}

// ---- Composition: MakeTime / MakeDay / MakeDate ----

fn make_time(hour: f64, min: f64, sec: f64, ms: f64) -> f64 {
    if !hour.is_finite() || !min.is_finite() || !sec.is_finite() || !ms.is_finite() {
        return f64::NAN;
    }
    hour.trunc() * MS_PER_HOUR
        + min.trunc() * MS_PER_MINUTE
        + sec.trunc() * MS_PER_SECOND
        + ms.trunc()
}

fn make_day(year: f64, month: f64, date: f64) -> f64 {
    if !year.is_finite() || !month.is_finite() || !date.is_finite() {
        return f64::NAN;
    }
    let y = year.trunc();
    let m = month.trunc();
    let dt = date.trunc();
    // Normalize month into [0, 11], rolling the excess into the year.
    let ym = y + floor_div(m, 12.0);
    let mn = modulo(m, 12.0); // 0-based month within year
    if !ym.is_finite() {
        return f64::NAN;
    }
    // days_from_civil expects 1-based month.
    let days = days_from_civil(ym as i64, (mn as i64) + 1, 1) as f64;
    days + dt - 1.0
}

fn make_date(day: f64, time: f64) -> f64 {
    if !day.is_finite() || !time.is_finite() {
        return f64::NAN;
    }
    day * MS_PER_DAY + time
}

// =========================================================================
// this-coercion
// =========================================================================

/// Read the `[[DateValue]]` (ms) from `this`, throwing TypeError if `this` is
/// not a Date object.
fn date_this(vm: &mut Vm, this: &Value) -> Result<f64, Value> {
    if let Value::Object(o) = this {
        if let Internal::Date(ms) = &o.borrow().internal {
            return Ok(*ms);
        }
    }
    Err(vm.throw_type("this is not a Date object"))
}

/// Get the Date object handle from `this`, or throw TypeError. Used by setters.
fn date_obj(vm: &mut Vm, this: &Value) -> Result<JsObject, Value> {
    if let Value::Object(o) = this {
        if matches!(o.borrow().internal, Internal::Date(_)) {
            return Ok(o.clone());
        }
    }
    Err(vm.throw_type("this is not a Date object"))
}

fn set_date_value(o: &JsObject, ms: f64) {
    o.borrow_mut().internal = Internal::Date(ms);
}

fn new_date_object(vm: &Vm, ms: f64) -> JsObject {
    vm.alloc(ObjectData::new(
        Some(vm.realm.date_proto.clone()),
        Internal::Date(ms),
    ))
}

// =========================================================================
// String parsing (ISO-8601 + a couple of common non-standard forms)
// =========================================================================

/// Number of days in `month` (0-based) of `year` (proleptic Gregorian).
fn days_in_month(year: i64, month0: i64) -> i64 {
    match month0 {
        0 | 2 | 4 | 6 | 7 | 9 | 11 => 31,
        3 | 5 | 8 | 10 => 30,
        1 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Parse a date string per the spec's Date Time String Format, plus a couple of
/// common non-standard forms (the `toUTCString` and `toString` outputs we emit,
/// and a space in place of `T`). Returns ms-since-epoch or NaN. Never throws.
///
/// Accepted ISO forms (date-only is interpreted as UTC):
///   - `YYYY`, `YYYY-MM`, `YYYY-MM-DD`
///   - `±YYYYYY`, `±YYYYYY-MM`, `±YYYYYY-MM-DD` (expanded year)
///   - any of the above + `THH:mm`, `THH:mm:ss`, `THH:mm:ss.sss`
///   - the time portion optionally suffixed by `Z`, `+HH:mm`, or `-HH:mm`.
pub fn parse_date_string(input: &str) -> f64 {
    let s = input.trim();
    if s.is_empty() {
        return f64::NAN;
    }
    if let Some(t) = parse_iso(s) {
        return t;
    }
    if let Some(t) = parse_legacy(s) {
        return t;
    }
    f64::NAN
}

/// Parse the spec Date Time String Format. Returns None if it does not match.
fn parse_iso(s: &str) -> Option<f64> {
    // Split into a date part and an optional time-with-zone part. The standard
    // separator is `T`; we also accept a single space as a pragmatic extension.
    let bytes = s.as_bytes();
    let sep = bytes.iter().position(|&b| b == b'T' || b == b' ');
    let (date_part, time_part) = match sep {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };

    let (year, month, day_of_month, date_only) = parse_iso_date(date_part)?;

    let (mut hour, mut minute, mut second, mut milli) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    // Per spec, a date-only string is UTC; a date+time without an explicit
    // offset is local time. Since local == UTC here, the distinction collapses,
    // but we still default the offset to 0.
    let mut offset_minutes = 0.0f64;
    let mut had_zone = false;

    match time_part {
        Some(tp) => {
            let (h, mi, se, ms, off, hz) = parse_iso_time_and_zone(tp)?;
            hour = h;
            minute = mi;
            second = se;
            milli = ms;
            offset_minutes = off;
            had_zone = hz;
        }
        None => {
            if !date_only {
                // A date_part that itself carried a designator but no time is
                // not a valid ISO string.
                return None;
            }
        }
    }
    let _ = had_zone;

    let day = make_day(year, month - 1.0, day_of_month);
    let time = make_time(hour, minute, second, milli);
    let t = make_date(day, time);
    if !t.is_finite() {
        return None;
    }
    // A `+HH:mm` zone means the wall clock is ahead of UTC, so subtract it.
    let t = t - offset_minutes * MS_PER_MINUTE;
    Some(time_clip(t))
}

/// Parse the date portion. Returns (year, month[1..=12], day[1..=31],
/// date_only) where `date_only` is true when the field was a bare calendar
/// date that may legally stand without a time component.
fn parse_iso_date(s: &str) -> Option<(f64, f64, f64, bool)> {
    let (sign, rest, expanded) = if let Some(r) = s.strip_prefix('+') {
        (1.0, r, true)
    } else if let Some(r) = s.strip_prefix('-') {
        (-1.0, r, true)
    } else {
        (1.0, s, false)
    };

    let parts: Vec<&str> = rest.split('-').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }

    // Year: 4 digits in the basic form, exactly 6 in the expanded `±YYYYYY`
    // form.
    let ydigits = parts[0];
    if expanded {
        if ydigits.len() != 6 || !ydigits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    } else if ydigits.len() != 4 || !ydigits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year_mag = parse_uint(ydigits)? as f64;
    let mut year = sign * year_mag;
    // `-000000` is not a valid signed year (spec disallows negative zero year).
    if expanded && sign < 0.0 && year_mag == 0.0 {
        return None;
    }
    if year == 0.0 {
        year = 0.0; // normalize -0
    }

    let month = if parts.len() >= 2 {
        if parts[1].len() != 2 {
            return None;
        }
        let m = parse_uint(parts[1])?;
        if !(1..=12).contains(&m) {
            return None;
        }
        m as f64
    } else {
        1.0
    };

    let day = if parts.len() == 3 {
        if parts[2].len() != 2 {
            return None;
        }
        let d = parse_uint(parts[2])?;
        let dim = days_in_month(year as i64, (month as i64) - 1);
        if (d as i64) < 1 || (d as i64) > dim {
            return None;
        }
        d as f64
    } else {
        1.0
    };

    Some((year, month, day, true))
}

/// Parse `HH:mm[:ss[.sss]]` with an optional trailing zone (`Z`, `+HH:mm`,
/// `-HH:mm`). Returns (hour, minute, second, ms, offset_minutes, had_zone).
fn parse_iso_time_and_zone(s: &str) -> Option<(f64, f64, f64, f64, f64, bool)> {
    let mut body = s;
    let mut offset_minutes = 0.0f64;
    let mut had_zone = false;

    if let Some(stripped) = body.strip_suffix('Z').or_else(|| body.strip_suffix('z')) {
        body = stripped;
        had_zone = true;
    } else if let Some(idx) = find_zone_sign(body) {
        let zone = &body[idx..];
        body = &body[..idx];
        offset_minutes = parse_zone_offset(zone)?;
        had_zone = true;
    }

    // Separate the fractional seconds.
    let (hms, frac) = match body.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (body, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    if parts[0].len() != 2 || parts[1].len() != 2 {
        return None;
    }
    let hour = parse_uint(parts[0])? as f64;
    let minute = parse_uint(parts[1])? as f64;
    let second = if parts.len() == 3 {
        if parts[2].len() != 2 {
            return None;
        }
        parse_uint(parts[2])? as f64
    } else {
        0.0
    };
    if hour > 24.0 || minute > 59.0 || second > 59.0 {
        return None;
    }
    let milli = match frac {
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Take up to 3 fractional digits, right-padded to milliseconds.
            let mut digits = f.to_string();
            digits.truncate(3);
            while digits.len() < 3 {
                digits.push('0');
            }
            digits.parse::<u32>().ok()? as f64
        }
        None => 0.0,
    };
    // Hour 24 is only valid as "24:00[:00][.000]" (midnight of the next day).
    if hour == 24.0 && (minute != 0.0 || second != 0.0 || milli != 0.0) {
        return None;
    }
    Some((hour, minute, second, milli, offset_minutes, had_zone))
}

/// Find the index of a zone sign (`+`/`-`) that introduces a `+HH:MM` suffix.
fn find_zone_sign(s: &str) -> Option<usize> {
    s.bytes()
        .enumerate()
        .find(|&(_, b)| b == b'+' || b == b'-')
        .map(|(i, _)| i)
}

/// Parse `+HH:MM` or `-HH:MM` (also bare `+HHMM`). Returns signed minutes.
fn parse_zone_offset(s: &str) -> Option<f64> {
    let (sign, rest) = if let Some(r) = s.strip_prefix('+') {
        (1.0, r)
    } else if let Some(r) = s.strip_prefix('-') {
        (-1.0, r)
    } else {
        return None;
    };
    let (h, m) = if let Some((a, b)) = rest.split_once(':') {
        (a, b)
    } else if rest.len() == 4 {
        (&rest[..2], &rest[2..])
    } else {
        return None;
    };
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let hh = parse_uint(h)? as f64;
    let mm = parse_uint(m)? as f64;
    if hh > 23.0 || mm > 59.0 {
        return None;
    }
    Some(sign * (hh * 60.0 + mm))
}

/// Parse a run of ASCII digits as an unsigned integer (no sign, no extras).
fn parse_uint(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

// ---- Non-standard (legacy) parsing ----

/// Accept the two human-readable forms this engine itself emits:
///   - `Www, DD Mmm YYYY HH:mm:ss GMT`            (toUTCString)
///   - `Www Mmm DD YYYY HH:mm:ss ...`             (toString / toDateString)
/// Both are interpreted as UTC. Returns None if neither matches.
fn parse_legacy(s: &str) -> Option<f64> {
    // Tokenize on whitespace and commas.
    let toks: Vec<&str> = s
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    if toks.len() < 4 {
        return None;
    }

    // Determine layout by looking for a 3-letter month name.
    // toUTCString:  [Www] DD Mmm YYYY HH:mm:ss [GMT]
    // toString:     [Www] Mmm DD YYYY HH:mm:ss ...
    let month_index = |name: &str| -> Option<i64> {
        MONTH_NAMES
            .iter()
            .position(|m| m.eq_ignore_ascii_case(name))
            .map(|i| i as i64)
    };

    // Try toUTCString layout: tok[0]=Www, tok[1]=DD, tok[2]=Mmm, tok[3]=YYYY.
    let mut year: Option<i64> = None;
    let mut month0: Option<i64> = None;
    let mut day_of_month: Option<i64> = None;
    let mut time_tok: Option<&str> = None;

    if let Some(m) = month_index(toks[2]) {
        // DD Mmm YYYY
        day_of_month = parse_uint(toks[1]).map(|d| d as i64);
        month0 = Some(m);
        year = parse_signed_int(toks[3]);
        time_tok = toks.get(4).copied();
    } else if let Some(m) = month_index(toks[1]) {
        // Mmm DD YYYY
        month0 = Some(m);
        day_of_month = parse_uint(toks[2]).map(|d| d as i64);
        year = parse_signed_int(toks[3]);
        time_tok = toks.get(4).copied();
    } else if toks.len() >= 3 {
        // Possibly no weekday prefix: Mmm DD YYYY ...
        if let Some(m) = month_index(toks[0]) {
            month0 = Some(m);
            day_of_month = parse_uint(toks[1]).map(|d| d as i64);
            year = parse_signed_int(toks[2]);
            time_tok = toks.get(3).copied();
        } else if let Some(m) = month_index(toks[1]) {
            // DD Mmm YYYY ... (no weekday)
            day_of_month = parse_uint(toks[0]).map(|d| d as i64);
            month0 = Some(m);
            year = parse_signed_int(toks[2]);
            time_tok = toks.get(3).copied();
        }
    }

    let (year, month0, day_of_month) = (year?, month0?, day_of_month?);
    if !(0..=11).contains(&month0) {
        return None;
    }
    let dim = days_in_month(year, month0);
    if day_of_month < 1 || day_of_month > dim {
        return None;
    }

    let (mut hour, mut minute, mut second) = (0.0f64, 0.0f64, 0.0f64);
    if let Some(tt) = time_tok {
        if tt.contains(':') {
            let parts: Vec<&str> = tt.split(':').collect();
            if parts.len() < 2 || parts.len() > 3 {
                return None;
            }
            hour = parse_uint(parts[0])? as f64;
            minute = parse_uint(parts[1])? as f64;
            second = if parts.len() == 3 {
                parse_uint(parts[2])? as f64
            } else {
                0.0
            };
            if hour > 23.0 || minute > 59.0 || second > 59.0 {
                return None;
            }
        }
    }

    let day = make_day(year as f64, month0 as f64, day_of_month as f64);
    let time = make_time(hour, minute, second, 0.0);
    let t = make_date(day, time);
    if !t.is_finite() {
        return None;
    }
    Some(time_clip(t))
}

/// Parse an optionally-signed integer string (digits only after the sign).
fn parse_signed_int(s: &str) -> Option<i64> {
    let (sign, rest) = if let Some(r) = s.strip_prefix('-') {
        (-1, r)
    } else if let Some(r) = s.strip_prefix('+') {
        (1, r)
    } else {
        (1, s)
    };
    let v = parse_uint(rest)? as i64;
    Some(sign * v)
}

// ---- Formatting ----

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn pad2(n: f64) -> String {
    format!("{:02}", n as i64)
}

/// `toISOString` body. Caller handles the NaN (RangeError) case.
fn to_iso_string(t: f64) -> String {
    let year = year_from_time(t) as i64;
    let month = (month_from_time(t) as i64) + 1;
    let day = date_from_time(t) as i64;
    let h = hours_from_time(t) as i64;
    let mi = min_from_time(t) as i64;
    let se = sec_from_time(t) as i64;
    let ms = ms_from_time(t) as i64;
    if (0..=9999).contains(&year) {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            year, month, day, h, mi, se, ms
        )
    } else {
        // Expanded year form (+/-YYYYYY).
        let sign = if year < 0 { '-' } else { '+' };
        format!(
            "{}{:06}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            sign,
            year.abs(),
            month,
            day,
            h,
            mi,
            se,
            ms
        )
    }
}

fn to_date_string(t: f64) -> String {
    let wd = DAY_NAMES[week_day(t) as usize];
    let mon = MONTH_NAMES[month_from_time(t) as usize];
    let day = date_from_time(t);
    let year = year_from_time(t) as i64;
    format!("{} {} {} {:04}", wd, mon, pad2(day), year)
}

fn to_time_string(t: f64) -> String {
    format!(
        "{}:{}:{} GMT+0000 (Coordinated Universal Time)",
        pad2(hours_from_time(t)),
        pad2(min_from_time(t)),
        pad2(sec_from_time(t))
    )
}

fn to_full_string(t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".to_string();
    }
    format!("{} {}", to_date_string(t), to_time_string(t))
}

fn to_utc_string(t: f64) -> String {
    if t.is_nan() {
        return "Invalid Date".to_string();
    }
    let wd = DAY_NAMES[week_day(t) as usize];
    let mon = MONTH_NAMES[month_from_time(t) as usize];
    format!(
        "{}, {} {} {:04} {}:{}:{} GMT",
        wd,
        pad2(date_from_time(t)),
        mon,
        year_from_time(t) as i64,
        pad2(hours_from_time(t)),
        pad2(min_from_time(t)),
        pad2(sec_from_time(t))
    )
}

// =========================================================================
// Constructor argument handling
// =========================================================================

/// Apply the legacy year coercion: an integral year in 0..=99 maps to
/// 1900..=1999. (Shared by `new Date(y, m, ...)`, `Date.UTC`, and `setYear`.)
fn coerce_legacy_year(y: f64) -> f64 {
    if y.is_nan() {
        return f64::NAN;
    }
    let yi = y.trunc();
    if (0.0..=99.0).contains(&yi) {
        1900.0 + yi
    } else {
        yi
    }
}

/// Build a timestamp from the multi-argument constructor form
/// `(year, month, date?, hours?, minutes?, seconds?, ms?)`. Every argument is
/// coerced via ToNumber (in order) before composition, per spec.
fn make_time_from_args(vm: &mut Vm, args: &[Value]) -> Result<f64, Value> {
    let y = vm.to_number(&arg(args, 0))?;
    let m = if args.len() > 1 {
        vm.to_number(&arg(args, 1))?
    } else {
        0.0
    };
    let dt = if args.len() > 2 {
        vm.to_number(&arg(args, 2))?
    } else {
        1.0
    };
    let h = if args.len() > 3 {
        vm.to_number(&arg(args, 3))?
    } else {
        0.0
    };
    let mi = if args.len() > 4 {
        vm.to_number(&arg(args, 4))?
    } else {
        0.0
    };
    let se = if args.len() > 5 {
        vm.to_number(&arg(args, 5))?
    } else {
        0.0
    };
    let ms = if args.len() > 6 {
        vm.to_number(&arg(args, 6))?
    } else {
        0.0
    };
    let year = coerce_legacy_year(y);
    let day = make_day(year, m, dt);
    let time = make_time(h, mi, se, ms);
    Ok(make_date(day, time))
}

// =========================================================================
// Installation
// =========================================================================

pub fn install(vm: &mut Vm) {
    let proto = vm.realm.date_proto.clone();
    // The prototype is an ordinary object (its [[DateValue]] is not used); per
    // spec Date.prototype is not itself a Date with a slot, so brand checks on
    // the prototype itself throw TypeError like a real engine.

    // ----- constructor -----
    let ctor = vm.new_native_ctor(
        "Date",
        7,
        // [[Call]] (without `new`) -> returns a date string for "now".
        |_vm, _this, _args| Ok(Value::str(to_full_string(0.0))),
        // [[Construct]] (`new Date(...)`).
        |vm, _this, args| {
            let ms = if args.is_empty() {
                // No host clock: fixed epoch.
                0.0
            } else if args.len() == 1 {
                let v = arg(args, 0);
                // new Date(dateObject) copies its time value.
                let copied = if let Value::Object(o) = &v {
                    if let Internal::Date(t) = &o.borrow().internal {
                        Some(*t)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(t) = copied {
                    time_clip(t)
                } else {
                    let prim = vm.to_primitive(&v, crate::vm::Hint::Default)?;
                    match prim {
                        Value::String(s) => parse_date_string(s.as_str()),
                        other => {
                            let n = vm.to_number(&other)?;
                            time_clip(n)
                        }
                    }
                }
            } else {
                let t = make_time_from_args(vm, args)?;
                time_clip(t)
            };
            Ok(Value::Object(new_date_object(vm, ms)))
        },
    );
    vm.install_ctor("Date", &ctor, &proto);

    // ----- static methods -----
    vm.define_method(&ctor, "now", 0, |_vm, _t, _a| {
        // No host clock: fixed epoch.
        Ok(Value::Number(0.0))
    });
    vm.define_method(&ctor, "parse", 1, |vm, _t, args| {
        let s = vm.to_js_string(&arg(args, 0))?;
        Ok(Value::Number(parse_date_string(s.as_str())))
    });
    vm.define_method(&ctor, "UTC", 7, |vm, _t, args| {
        if args.is_empty() {
            return Ok(Value::Number(f64::NAN));
        }
        let t = make_time_from_args(vm, args)?;
        Ok(Value::Number(time_clip(t)))
    });

    // ----- getters -----
    vm.define_method(&proto, "getTime", 0, |vm, this, _a| {
        Ok(Value::Number(date_this(vm, &this)?))
    });
    vm.define_method(&proto, "valueOf", 0, |vm, this, _a| {
        Ok(Value::Number(date_this(vm, &this)?))
    });

    macro_rules! getter {
        ($name:expr, $f:expr) => {
            vm.define_method(&proto, $name, 0, move |vm, this, _a| {
                let t = date_this(vm, &this)?;
                if t.is_nan() {
                    return Ok(Value::Number(f64::NAN));
                }
                Ok(Value::Number($f(t)))
            });
        };
    }
    // Local == UTC, so the local and UTC variants share an implementation.
    getter!("getFullYear", year_from_time);
    getter!("getUTCFullYear", year_from_time);
    getter!("getMonth", month_from_time);
    getter!("getUTCMonth", month_from_time);
    getter!("getDate", date_from_time);
    getter!("getUTCDate", date_from_time);
    getter!("getDay", week_day);
    getter!("getUTCDay", week_day);
    getter!("getHours", hours_from_time);
    getter!("getUTCHours", hours_from_time);
    getter!("getMinutes", min_from_time);
    getter!("getUTCMinutes", min_from_time);
    getter!("getSeconds", sec_from_time);
    getter!("getUTCSeconds", sec_from_time);
    getter!("getMilliseconds", ms_from_time);
    getter!("getUTCMilliseconds", ms_from_time);
    // Annex B legacy getYear: full year minus 1900.
    getter!("getYear", |t: f64| year_from_time(t) - 1900.0);

    vm.define_method(&proto, "getTimezoneOffset", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        if t.is_nan() {
            return Ok(Value::Number(f64::NAN));
        }
        // Local == UTC.
        Ok(Value::Number(0.0))
    });

    // ----- setters -----
    vm.define_method(&proto, "setTime", 1, |vm, this, args| {
        let o = date_obj(vm, &this)?;
        let n = vm.to_number(&arg(args, 0))?;
        let v = time_clip(n);
        set_date_value(&o, v);
        Ok(Value::Number(v))
    });

    // setFullYear(year, month?, date?) / setUTCFullYear(...).
    // On an invalid date the time component defaults to +0 (spec 21.4.4.21).
    let set_full_year = |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        let base = if t.is_nan() { 0.0 } else { t };
        // Coerce all present arguments first (in order).
        let year = vm.to_number(&arg(args, 0))?;
        let month = if args.len() > 1 {
            vm.to_number(&arg(args, 1))?
        } else {
            month_from_time(base)
        };
        let date = if args.len() > 2 {
            vm.to_number(&arg(args, 2))?
        } else {
            date_from_time(base)
        };
        let day = make_day(year, month, date);
        let v = time_clip(make_date(day, time_within_day(base)));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    };
    vm.define_method(&proto, "setFullYear", 3, set_full_year);
    vm.define_method(&proto, "setUTCFullYear", 3, set_full_year);

    // setMonth(month, date?) / setUTCMonth(...).
    let set_month = |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        // ToNumber on all present arguments runs regardless of NaN.
        let month = vm.to_number(&arg(args, 0))?;
        let date_arg = if args.len() > 1 {
            Some(vm.to_number(&arg(args, 1))?)
        } else {
            None
        };
        if t.is_nan() {
            set_date_value(&o, f64::NAN);
            return Ok(Value::Number(f64::NAN));
        }
        let date = date_arg.unwrap_or_else(|| date_from_time(t));
        let day = make_day(year_from_time(t), month, date);
        let v = time_clip(make_date(day, time_within_day(t)));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    };
    vm.define_method(&proto, "setMonth", 2, set_month);
    vm.define_method(&proto, "setUTCMonth", 2, set_month);

    // setDate(date) / setUTCDate(date).
    let set_date = |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        let date = vm.to_number(&arg(args, 0))?;
        if t.is_nan() {
            set_date_value(&o, f64::NAN);
            return Ok(Value::Number(f64::NAN));
        }
        let day = make_day(year_from_time(t), month_from_time(t), date);
        let v = time_clip(make_date(day, time_within_day(t)));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    };
    vm.define_method(&proto, "setDate", 1, set_date);
    vm.define_method(&proto, "setUTCDate", 1, set_date);

    // setHours(hours, min?, sec?, ms?) / setUTCHours(...).
    let set_hours = |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        let h = vm.to_number(&arg(args, 0))?;
        let mi_arg = if args.len() > 1 {
            Some(vm.to_number(&arg(args, 1))?)
        } else {
            None
        };
        let se_arg = if args.len() > 2 {
            Some(vm.to_number(&arg(args, 2))?)
        } else {
            None
        };
        let ms_arg = if args.len() > 3 {
            Some(vm.to_number(&arg(args, 3))?)
        } else {
            None
        };
        if t.is_nan() {
            set_date_value(&o, f64::NAN);
            return Ok(Value::Number(f64::NAN));
        }
        let mi = mi_arg.unwrap_or_else(|| min_from_time(t));
        let se = se_arg.unwrap_or_else(|| sec_from_time(t));
        let ms = ms_arg.unwrap_or_else(|| ms_from_time(t));
        let time = make_time(h, mi, se, ms);
        let v = time_clip(make_date(day(t), time));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    };
    vm.define_method(&proto, "setHours", 4, set_hours);
    vm.define_method(&proto, "setUTCHours", 4, set_hours);

    // setMinutes(min, sec?, ms?) / setUTCMinutes(...).
    let set_minutes = |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        let mi = vm.to_number(&arg(args, 0))?;
        let se_arg = if args.len() > 1 {
            Some(vm.to_number(&arg(args, 1))?)
        } else {
            None
        };
        let ms_arg = if args.len() > 2 {
            Some(vm.to_number(&arg(args, 2))?)
        } else {
            None
        };
        if t.is_nan() {
            set_date_value(&o, f64::NAN);
            return Ok(Value::Number(f64::NAN));
        }
        let se = se_arg.unwrap_or_else(|| sec_from_time(t));
        let ms = ms_arg.unwrap_or_else(|| ms_from_time(t));
        let time = make_time(hours_from_time(t), mi, se, ms);
        let v = time_clip(make_date(day(t), time));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    };
    vm.define_method(&proto, "setMinutes", 3, set_minutes);
    vm.define_method(&proto, "setUTCMinutes", 3, set_minutes);

    // setSeconds(sec, ms?) / setUTCSeconds(...).
    let set_seconds = |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        let se = vm.to_number(&arg(args, 0))?;
        let ms_arg = if args.len() > 1 {
            Some(vm.to_number(&arg(args, 1))?)
        } else {
            None
        };
        if t.is_nan() {
            set_date_value(&o, f64::NAN);
            return Ok(Value::Number(f64::NAN));
        }
        let ms = ms_arg.unwrap_or_else(|| ms_from_time(t));
        let time = make_time(hours_from_time(t), min_from_time(t), se, ms);
        let v = time_clip(make_date(day(t), time));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    };
    vm.define_method(&proto, "setSeconds", 2, set_seconds);
    vm.define_method(&proto, "setUTCSeconds", 2, set_seconds);

    // setMilliseconds(ms) / setUTCMilliseconds(ms).
    let set_milliseconds = |vm: &mut Vm, this: Value, args: &[Value]| -> Result<Value, Value> {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        let ms = vm.to_number(&arg(args, 0))?;
        if t.is_nan() {
            set_date_value(&o, f64::NAN);
            return Ok(Value::Number(f64::NAN));
        }
        let time = make_time(hours_from_time(t), min_from_time(t), sec_from_time(t), ms);
        let v = time_clip(make_date(day(t), time));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    };
    vm.define_method(&proto, "setMilliseconds", 1, set_milliseconds);
    vm.define_method(&proto, "setUTCMilliseconds", 1, set_milliseconds);

    // Annex B setYear(year): like setFullYear but 0..99 maps to 1900-based, and
    // a NaN base is treated as +0.
    vm.define_method(&proto, "setYear", 1, |vm, this, args| {
        let o = date_obj(vm, &this)?;
        let t = date_this(vm, &this)?;
        let y = vm.to_number(&arg(args, 0))?;
        if y.is_nan() {
            set_date_value(&o, f64::NAN);
            return Ok(Value::Number(f64::NAN));
        }
        let base = if t.is_nan() { 0.0 } else { t };
        let year = coerce_legacy_year(y);
        let day = make_day(year, month_from_time(base), date_from_time(base));
        let v = time_clip(make_date(day, time_within_day(base)));
        set_date_value(&o, v);
        Ok(Value::Number(v))
    });

    // ----- string conversions -----
    vm.define_method(&proto, "toISOString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        if t.is_nan() {
            return Err(vm.throw_range("Invalid time value"));
        }
        Ok(Value::str(to_iso_string(t)))
    });
    vm.define_method(&proto, "toJSON", 1, |vm, this, _a| {
        // toJSON coerces `this` to an object and reads its time value via
        // ToPrimitive(number); if the number is non-finite it returns null.
        let o = vm.to_object(&this)?;
        let prim = vm.to_primitive(&Value::Object(o.clone()), crate::vm::Hint::Number)?;
        if let Value::Number(n) = prim {
            if !n.is_finite() {
                return Ok(Value::Null);
            }
        }
        // Call toISOString on the object.
        let to_iso = vm.get_prop(&Value::Object(o.clone()), &PropertyKey::str("toISOString"))?;
        if !vm.is_callable(&to_iso) {
            return Err(vm.throw_type("toISOString is not callable"));
        }
        vm.call(to_iso, Value::Object(o), &[])
    });
    vm.define_method(&proto, "toString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        Ok(Value::str(to_full_string(t)))
    });
    vm.define_method(&proto, "toUTCString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        Ok(Value::str(to_utc_string(t)))
    });
    vm.define_method(&proto, "toGMTString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        Ok(Value::str(to_utc_string(t)))
    });
    vm.define_method(&proto, "toDateString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        if t.is_nan() {
            return Ok(Value::str("Invalid Date"));
        }
        Ok(Value::str(to_date_string(t)))
    });
    vm.define_method(&proto, "toTimeString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        if t.is_nan() {
            return Ok(Value::str("Invalid Date"));
        }
        Ok(Value::str(to_time_string(t)))
    });
    // toLocale* are deterministic aliases (no ICU): match the non-locale forms.
    vm.define_method(&proto, "toLocaleString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        Ok(Value::str(to_full_string(t)))
    });
    vm.define_method(&proto, "toLocaleDateString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        if t.is_nan() {
            return Ok(Value::str("Invalid Date"));
        }
        Ok(Value::str(to_date_string(t)))
    });
    vm.define_method(&proto, "toLocaleTimeString", 0, |vm, this, _a| {
        let t = date_this(vm, &this)?;
        if t.is_nan() {
            return Ok(Value::str("Invalid Date"));
        }
        Ok(Value::str(to_time_string(t)))
    });

    // Date.prototype[Symbol.toPrimitive]: hint "number" -> time value, hint
    // "string"/"default" -> string form.
    let to_primitive_sym = vm.realm.symbol_to_primitive.clone();
    let to_prim_fn = vm.new_native("[Symbol.toPrimitive]", 1, |vm, this, args| {
        if !matches!(this, Value::Object(_)) {
            return Err(vm.throw_type("Date.prototype[Symbol.toPrimitive] called on non-object"));
        }
        let hint = arg(args, 0);
        let hint = match &hint {
            Value::String(s) => s.as_str().to_string(),
            _ => return Err(vm.throw_type("invalid hint")),
        };
        match hint.as_str() {
            "string" | "default" => {
                let s = vm.get_prop(&this, &PropertyKey::str("toString"))?;
                vm.call(s, this, &[])
            }
            "number" => {
                let v = vm.get_prop(&this, &PropertyKey::str("valueOf"))?;
                vm.call(v, this, &[])
            }
            _ => Err(vm.throw_type("invalid hint")),
        }
    });
    {
        // Symbol.toPrimitive is non-enumerable, non-writable, configurable.
        let mut b = proto.borrow_mut();
        b.props.insert(
            PropertyKey::Sym(to_primitive_sym),
            Property {
                kind: PropertyKind::Data {
                    value: Value::Object(to_prim_fn),
                    writable: false,
                },
                enumerable: false,
                configurable: true,
            },
        );
    }
}

/// The within-day time component (ms since UTC midnight of that day).
fn time_within_day(t: f64) -> f64 {
    modulo(t, MS_PER_DAY)
}
