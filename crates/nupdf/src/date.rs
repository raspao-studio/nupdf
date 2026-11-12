// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-parse.c (pdf_parse_date),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! PDF dates (`D:YYYYMMDDHHmmSS+HH'mm'`) as seconds since 1970, as
//! `pdf_parse_date` reads them.

use crate::diag::Diagnostics;

/// Days from 1970-01-01 to the first of `month` (1-12) of `year`, months
/// past 12 or before 1 carried into the year, as `timegm` normalizes.
fn days_to_month(year: i64, month: i64) -> i64 {
    let y = year + (month - 1).div_euclid(12);
    let m = (month - 1).rem_euclid(12) + 1;
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `pdf_parse_date`: the time `s` names, in seconds since 1970 UTC, or
/// None where MuPDF gives -1. Its warnings go to `diag`.
pub fn parse_date(s: &[u8], diag: &Diagnostics) -> Option<i64> {
    // A C string: up to the first NUL.
    let s = &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())];
    if s.is_empty() {
        return None;
    }
    let mut s = s.strip_prefix(b"D:").unwrap_or(s);
    let digit = |s: &[u8], i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
    let num = |s: &[u8], i: usize| i64::from(s[i] - b'0');
    let two = |s: &[u8]| digit(s, 0) && digit(s, 1);
    if !(0..4).all(|i| digit(s, i)) {
        diag.warn("date.invalid", "invalid date format (missing year)");
        return None;
    }
    let year = num(s, 0) * 1000 + num(s, 1) * 100 + num(s, 2) * 10 + num(s, 3);
    s = &s[4..];
    if year < 1970 {
        diag.warn("date.invalid", "invalid date (year out of range)");
        return None;
    }
    let (mut mon, mut mday, mut hour, mut min, mut sec) = (0i64, 1i64, 0i64, 0i64, 0i64);
    let take = |s: &mut &[u8], v: &mut i64, off: i64| -> bool {
        if two(s) {
            *v = num(s, 0) * 10 + num(s, 1) + off;
            *s = &s[2..];
            true
        } else {
            false
        }
    };
    if take(&mut s, &mut mon, -1)
        && take(&mut s, &mut mday, 0)
        && take(&mut s, &mut hour, 0)
        && take(&mut s, &mut min, 0)
    {
        take(&mut s, &mut sec, 0);
    }
    if sec > 60 || min > 59 || hour > 23 || mday > 31 || mon > 11 {
        diag.warn("date.invalid", "invalid date (a field is out of range)");
        return None;
    }
    let (mut tz_sign, mut tz_hour, mut tz_min) = (1i64, 0i64, 0i64);
    if s.first() == Some(&b'Z') {
        if s.get(1) == Some(&b'0') && s.get(2) == Some(&b'0') {
            s = &s[3..];
            if s.first() == Some(&b'\'') && s.get(1) == Some(&b'0') && s.get(2) == Some(&b'0') {
                s = &s[3..];
                if s.first() == Some(&b'\'') {
                    s = &s[1..];
                }
            }
        } else {
            s = &s[1..];
        }
    } else if matches!(s.first(), Some(b'-' | b'+')) && digit(s, 1) && digit(s, 2) {
        tz_sign = if s[0] == b'-' { -1 } else { 1 };
        tz_hour = num(s, 1) * 10 + num(s, 2);
        s = &s[3..];
        if s.first() == Some(&b'\'') && digit(s, 1) && digit(s, 2) {
            tz_min = num(s, 1) * 10 + num(s, 2);
            s = &s[3..];
            if s.first() == Some(&b'\'') {
                s = &s[1..];
            }
        }
    }
    // ISO/IEC 8824 limits time zones to -15 to +16 hours.
    if (tz_sign < 0 && (tz_hour > 15 || (tz_hour == 15 && tz_min > 0)))
        || (tz_sign > 0 && (tz_hour > 16 || (tz_hour == 16 && tz_min > 0)))
    {
        diag.warn(
            "date.invalid",
            "invalid date format (time zone out of range)",
        );
        return None;
    }
    if !s.is_empty() {
        diag.warn("date.invalid", "invalid date format (garbage at end)");
    }
    let days = days_to_month(year, mon + 1) + mday - 1;
    let utc = days * 86_400 + hour * 3600 + min * 60 + sec;
    Some(utc - tz_sign * (tz_hour * 3600 + tz_min * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> (Option<i64>, usize) {
        let d = Diagnostics::new();
        let v = parse_date(s.as_bytes(), &d);
        (v, d.snapshot().len())
    }

    #[test]
    fn dates_read_as_mupdf_reads_them() {
        assert_eq!(date("D:19700101000000Z"), (Some(0), 0));
        assert_eq!(date("D:2026111200"), (Some(1_794_441_600), 0));
        assert_eq!(date("D:20260301120000+05'30'"), (Some(1_772_346_600), 0));
        assert_eq!(date("2000"), (Some(946_684_800), 0));
        // Day 31 of February is carried into March, as timegm does.
        assert_eq!(date("D:20260231"), date("D:20260303"));
        assert_eq!(date("D:1969"), (None, 1));
        assert_eq!(date("D:20261301"), (None, 1));
        assert_eq!(date("D:2026010112+17"), (None, 1));
        assert_eq!(date("D:20260101x"), (Some(1_767_225_600), 1));
        assert_eq!(date("").0, None);
        assert_eq!(date("D:20x").0, None);
    }
}
