use std::{
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use jiff::{civil::DateTime, tz::TimeZone};

/// Converts a wall-clock time in the system time zone (how ZIP and RAR store DOS times).
pub(crate) fn local(year: u16, month: u8, day: u8, hour: u8, min: u8, sec: u8) -> Option<SystemTime> {
    static TZ: OnceLock<TimeZone> = OnceLock::new();
    let tz = TZ.get_or_init(TimeZone::system);
    let dt = DateTime::new(
        i16::try_from(year).ok()?,
        i8::try_from(month).ok()?,
        i8::try_from(day).ok()?,
        i8::try_from(hour).ok()?,
        i8::try_from(min).ok()?,
        i8::try_from(sec).ok()?,
        0,
    )
    .ok()?;
    Some(dt.to_zoned(tz.clone()).ok()?.timestamp().into())
}

/// Decodes a packed MS-DOS date/time (date in the high 16 bits).
pub(crate) fn dos(packed: u32) -> Option<SystemTime> {
    let (date, time) = ((packed >> 16) as u16, packed as u16);
    local(
        1980 + (date >> 9),
        ((date >> 5) & 0xF) as u8,
        (date & 0x1F) as u8,
        (time >> 11) as u8,
        ((time >> 5) & 0x3F) as u8,
        ((time & 0x1F) * 2) as u8,
    )
}

/// Windows FILETIME: 100ns ticks since 1601-01-01 UTC.
pub(crate) fn filetime(ticks: u64) -> Option<SystemTime> {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let since_1601 = Duration::from_nanos(ticks.checked_mul(100)?);
    let since_unix = since_1601.checked_sub(Duration::from_secs(EPOCH_DIFF_SECS))?;
    UNIX_EPOCH.checked_add(since_unix)
}

pub(crate) fn unix(secs: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}
