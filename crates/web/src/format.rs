//! Display helpers for pages.

pub(crate) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// UTC time to the minute.
pub(crate) fn time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// The offsets from UTC a time can be given in, for a form's choice: `-12:00` to `+14:00` by
/// quarter hours where any place uses them.
pub(crate) fn offsets() -> Vec<String> {
    let mut out = Vec::new();
    for quarters in -48..=56i32 {
        let minutes = quarters * 15;
        if minutes % 60 != 0
            && ![-570, -210, 210, 270, 330, 345, 390, 525, 570, 630, 765, 825].contains(&minutes)
        {
            continue;
        }
        let sign = if minutes < 0 { '-' } else { '+' };
        out.push(format!(
            "{sign}{:02}:{:02}",
            minutes.abs() / 60,
            minutes.abs() % 60
        ));
    }
    out
}

/// Milliseconds since the Unix epoch of `at`, a form's date and time (`YYYY-MM-DDTHH:MM`,
/// seconds optional), written at `offset` from UTC such as `+02:00`.
pub(crate) fn parse_time(at: &str, offset: &str) -> Option<i64> {
    let (date, clock) = at.trim().split_once(['T', ' '])?;
    let mut date = date.split('-').map(|p| p.parse::<i64>().ok());
    let (Some(Some(y)), Some(Some(m)), Some(Some(d)), None) =
        (date.next(), date.next(), date.next(), date.next())
    else {
        return None;
    };
    let mut clock = clock.split(':').map(|p| p.parse::<i64>().ok());
    let (Some(Some(h)), Some(Some(min))) = (clock.next(), clock.next()) else {
        return None;
    };
    let s = match clock.next() {
        None => 0,
        Some(s) => s?,
    };
    let sign = match offset.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let (oh, om) = offset[1..].split_once(':')?;
    let (oh, om): (i64, i64) = (oh.parse().ok()?, om.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || s > 59 || om > 59 {
        return None;
    }
    // Days since 1970-01-01, the inverse of the reckoning in `time`.
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((days * 86_400 + h * 3600 + min * 60 + s - sign * (oh * 3600 + om * 60)) * 1000)
}

/// Percent-encodes a value for a URL query string.
pub(crate) fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(size(2048), "2.0 KiB");
        assert_eq!(time(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(query_value("a b/ü&x"), "a%20b/%C3%BC%26x");
    }

    #[test]
    fn form_times() {
        assert_eq!(
            parse_time("2000-02-29T00:00", "+00:00"),
            Some(951_782_400_000)
        );
        assert_eq!(
            time(parse_time("2026-10-16T16:30", "+02:00").unwrap()),
            "2026-10-16 14:30 UTC"
        );
        assert_eq!(
            time(parse_time("2026-10-16T09:30:15", "-05:30").unwrap()),
            "2026-10-16 15:00 UTC"
        );
        for (at, offset) in [
            ("2026-13-01T00:00", "+00:00"),
            ("2026-10-16", "+00:00"),
            ("2026-10-16T24:00", "+00:00"),
            ("2026-10-16T10:00", "02:00"),
            ("", "+00:00"),
        ] {
            assert_eq!(parse_time(at, offset), None, "{at} {offset}");
        }
        let offsets = offsets();
        for o in ["-12:00", "+00:00", "+05:30", "+05:45", "+14:00"] {
            assert!(offsets.contains(&o.to_owned()), "{o}");
        }
        assert!(!offsets.contains(&"+01:15".to_owned()));
    }
}
