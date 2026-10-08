//! Upload history: one manifest file per upload, kept under the data
//! directory so every upload can be listed and downloaded again later.
//!
//! A record is an ordinary `.ant` file named `<timestamp>-<label>.ant`, for
//! example `20261006T140311Z-holiday-photos.ant`. The filename stem is the
//! record's id. The timestamp is UTC in a compact form that is safe on every
//! filesystem, and the label is the upload's name reduced to safe characters.
//! When an id is already taken, which happens when uploads with the same label
//! finish within one second, `-2`, `-3` and so on are appended. Records list
//! newest first, and within one second by that suffix compared as a number.
//! Nothing else is stored, so the history directory is just a folder of
//! manifests that any manifest tool can read.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::file::{read_manifest_file, write_manifest_file};
use super::{Manifest, ManifestError, MANIFEST_EXTENSION};

#[cfg(test)]
use super::{ContentRef, ManifestEntry};

/// Directory under the data directory that holds upload records.
pub const UPLOAD_HISTORY_DIR: &str = "uploads";
/// Label used when an upload has no name.
const DEFAULT_LABEL: &str = "upload";
/// Longest label kept in an id.
const MAX_LABEL_CHARS: usize = 64;
/// Characters a label may keep besides alphanumerics.
const LABEL_KEPT_PUNCTUATION: &[char] = &['-', '_', '.'];
/// Replacement for every other label character.
const LABEL_REPLACEMENT: char = '_';
/// Separator between the timestamp and the label in an id.
const ID_SEPARATOR: char = '-';
/// Length of the compact UTC timestamp, `YYYYMMDDTHHMMSSZ`.
const TIMESTAMP_LEN: usize = 16;
/// Attempts to find a free id when several uploads share a second.
const MAX_ID_ATTEMPTS: u32 = 1000;
/// Sequence number of an id without a collision suffix; the first suffix
/// written is `-2`.
const FIRST_ID_SEQUENCE: u32 = 1;
const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MINUTE;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;
/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar.
const DAYS_TO_UNIX_EPOCH: i64 = 719_468;
/// Days in a 400-year Gregorian cycle.
const DAYS_PER_ERA: i64 = 146_097;
const YEARS_PER_ERA: i64 = 400;
const DAYS_PER_YEAR: u64 = 365;
const DAYS_PER_4_YEARS: u64 = 1_460;
const DAYS_PER_100_YEARS: u64 = 36_524;
const MONTHS_PER_YEAR: u32 = 12;
// The date conversions below are Howard Hinnant's `days_from_civil` and
// `civil_from_days` (https://howardhinnant.github.io/date_algorithms.html).
// They count months from March so leap days fall at the end of the year.
/// Any five consecutive months counted from March span 153 days, so
/// `(153 * m + 2) / 5` is the day of the March-based year a month starts on.
const DAYS_PER_FIVE_MONTHS: u64 = 153;
/// Rounding offset in the month-start formula above.
const MONTH_START_ROUNDING: u64 = 2;
/// Months per `DAYS_PER_FIVE_MONTHS` span.
const MONTHS_PER_SPAN: u64 = 5;
/// Added to a calendar month, modulo twelve, to count it from March.
const MONTH_SHIFT_TO_MARCH: u32 = 9;
/// March-based month index of January: March..December are 0..=9.
const FIRST_MONTH_OF_NEXT_YEAR: u64 = 10;
/// Calendar number of March, the first March-based month.
const MARCH: u32 = 3;
/// Calendar number of February, the last month before the year rolls.
const FEBRUARY: u32 = 2;

/// One recorded upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRecord {
    /// Filename stem: `<timestamp>-<label>`.
    pub id: String,
    /// Where the manifest lives.
    pub path: PathBuf,
    /// When it was recorded, seconds since the Unix epoch.
    pub recorded_at: u64,
    /// The label part of the id.
    pub label: String,
    /// The manifest itself.
    pub manifest: Manifest,
}

/// Result of listing a history directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UploadListing {
    /// Readable records, newest first.
    pub records: Vec<UploadRecord>,
    /// `.ant` files in the directory that could not be read, with the reason.
    pub unreadable: Vec<(PathBuf, String)>,
}

/// The default history directory: `<data dir>/uploads`.
pub fn default_history_dir() -> Result<PathBuf, ManifestError> {
    let data_dir = crate::config::data_dir().map_err(|e| ManifestError::Build(e.to_string()))?;
    Ok(data_dir.join(UPLOAD_HISTORY_DIR))
}

/// Write `manifest` into `dir` as a new record and return it.
pub fn record_upload(
    dir: &Path,
    manifest: &Manifest,
    label: Option<&str>,
) -> Result<UploadRecord, ManifestError> {
    fs::create_dir_all(dir)?;
    let recorded_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let label = sanitize_label(label.or(manifest.name.as_deref()));
    let base = format!(
        "{}{ID_SEPARATOR}{label}",
        format_compact_timestamp(recorded_at)
    );
    for attempt in 0..MAX_ID_ATTEMPTS {
        let id = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}{ID_SEPARATOR}{}", attempt + 1)
        };
        let path = dir.join(format!("{id}.{MANIFEST_EXTENSION}"));
        match write_manifest_file(&path, manifest, false) {
            Ok(()) => {
                return Ok(UploadRecord {
                    id,
                    path,
                    recorded_at,
                    label,
                    manifest: manifest.clone(),
                })
            }
            Err(ManifestError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(ManifestError::Build(format!(
        "could not find a free record id after {MAX_ID_ATTEMPTS} attempts in {}",
        dir.display()
    )))
}

/// List every record in `dir`, newest first. A missing directory is an
/// empty history.
pub fn list_uploads(dir: &Path) -> Result<UploadListing, ManifestError> {
    let mut listing = UploadListing::default();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(listing),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some(MANIFEST_EXTENSION) {
            continue;
        }
        match load_record(&path) {
            Ok(record) => listing.records.push(record),
            Err(e) => listing.unreadable.push((path, e.to_string())),
        }
    }
    listing.records.sort_by(|a, b| {
        b.recorded_at
            .cmp(&a.recorded_at)
            .then_with(|| id_sort_key(&b.id).cmp(&id_sort_key(&a.id)))
    });
    Ok(listing)
}

/// An id split into its base and collision suffix, so `-10` sorts after
/// `-2`. An id without a numeric suffix is the first of its base.
fn id_sort_key(id: &str) -> (&str, u32) {
    id.rsplit_once(ID_SEPARATOR)
        .and_then(|(base, suffix)| {
            let all_digits = !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit());
            all_digits
                .then(|| suffix.parse().ok())
                .flatten()
                .map(|n| (base, n))
        })
        .unwrap_or((id, FIRST_ID_SEQUENCE))
}

/// Load the record with `id` from `dir`, or `None` when there is none.
pub fn load_upload(dir: &Path, id: &str) -> Result<Option<UploadRecord>, ManifestError> {
    if id.is_empty() || id.contains(['/', '\\']) {
        return Ok(None);
    }
    let path = dir.join(format!("{id}.{MANIFEST_EXTENSION}"));
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => load_record(&path).map(Some),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `YYYY-MM-DD HH:MM:SS UTC` for display.
pub fn format_timestamp(unix_secs: u64) -> String {
    let (year, month, day, hour, minute, second) = split_unix(unix_secs);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn load_record(path: &Path) -> Result<UploadRecord, ManifestError> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| ManifestError::Build(format!("{} has no UTF-8 name", path.display())))?;
    let manifest = read_manifest_file(path)?;
    let (recorded_at, label) = match parse_id(stem) {
        Some(parsed) => parsed,
        None => (
            fs::metadata(path)?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or_default(),
            stem.to_string(),
        ),
    };
    Ok(UploadRecord {
        id: stem.to_string(),
        path: path.to_path_buf(),
        recorded_at,
        label,
        manifest,
    })
}

fn sanitize_label(label: Option<&str>) -> String {
    let cleaned: String = label
        .unwrap_or(DEFAULT_LABEL)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || LABEL_KEPT_PUNCTUATION.contains(&c) {
                c
            } else {
                LABEL_REPLACEMENT
            }
        })
        .take(MAX_LABEL_CHARS)
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == LABEL_REPLACEMENT || c == '.');
    if trimmed.is_empty() {
        DEFAULT_LABEL.to_string()
    } else {
        trimmed.to_string()
    }
}

/// `YYYYMMDDTHHMMSSZ`.
fn format_compact_timestamp(unix_secs: u64) -> String {
    let (year, month, day, hour, minute, second) = split_unix(unix_secs);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Split an id into its timestamp and label, or `None` if it was not
/// produced by [`record_upload`].
fn parse_id(id: &str) -> Option<(u64, String)> {
    let (stamp, label) = id.split_at_checked(TIMESTAMP_LEN)?;
    let label = label.strip_prefix(ID_SEPARATOR)?;
    let bytes = stamp.as_bytes();
    if bytes[8] != b'T' || bytes[15] != b'Z' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| stamp[range].parse::<u64>().ok();
    let (year, month, day) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (hour, minute, second) = (num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=u64::from(MONTHS_PER_YEAR)).contains(&month) || day == 0 {
        return None;
    }
    let days = days_from_civil(year as i64, month as u32, day as u32);
    if days < 0 {
        return None;
    }
    let secs =
        days as u64 * SECS_PER_DAY + hour * SECS_PER_HOUR + minute * SECS_PER_MINUTE + second;
    Some((secs, label.to_string()))
}

fn split_unix(unix_secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let days = (unix_secs / SECS_PER_DAY) as i64;
    let rem = unix_secs % SECS_PER_DAY;
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        rem / SECS_PER_HOUR,
        (rem % SECS_PER_HOUR) / SECS_PER_MINUTE,
        rem % SECS_PER_MINUTE,
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= FEBRUARY { year - 1 } else { year };
    let era = year.div_euclid(YEARS_PER_ERA);
    let year_of_era = (year - era * YEARS_PER_ERA) as u64;
    let month_from_march = u64::from((month + MONTH_SHIFT_TO_MARCH) % MONTHS_PER_YEAR);
    let day_of_year = month_start_day(month_from_march) + u64::from(day) - 1;
    let day_of_era =
        year_of_era * DAYS_PER_YEAR + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * DAYS_PER_ERA + day_of_era as i64 - DAYS_TO_UNIX_EPOCH
}

/// Proleptic Gregorian date for days since 1970-01-01 (Hinnant).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + DAYS_TO_UNIX_EPOCH;
    let era = shifted.div_euclid(DAYS_PER_ERA);
    let day_of_era = (shifted - era * DAYS_PER_ERA) as u64;
    let year_of_era = (day_of_era - day_of_era / DAYS_PER_4_YEARS
        + day_of_era / DAYS_PER_100_YEARS
        - day_of_era / (DAYS_PER_ERA as u64 - 1))
        / DAYS_PER_YEAR;
    let year = year_of_era as i64 + era * YEARS_PER_ERA;
    let day_of_year =
        day_of_era - (DAYS_PER_YEAR * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march =
        (MONTHS_PER_SPAN * day_of_year + MONTH_START_ROUNDING) / DAYS_PER_FIVE_MONTHS;
    let day = (day_of_year - month_start_day(month_from_march) + 1) as u32;
    let month = if month_from_march < FIRST_MONTH_OF_NEXT_YEAR {
        month_from_march as u32 + MARCH
    } else {
        month_from_march as u32 - MONTH_SHIFT_TO_MARCH
    };
    (if month <= FEBRUARY { year + 1 } else { year }, month, day)
}

/// Day of the March-based year on which `month_from_march` starts.
fn month_start_day(month_from_march: u64) -> u64 {
    (DAYS_PER_FIVE_MONTHS * month_from_march + MONTH_START_ROUNDING) / MONTHS_PER_SPAN
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn manifest(name: Option<&str>, seed: u8) -> Manifest {
        Manifest {
            name: name.map(str::to_owned),
            torrent: None,
            entries: vec![ManifestEntry {
                path: Some("a.bin".into()),
                size: Some(1),
                source: ContentRef::Public {
                    address: [seed; 32],
                },
            }],
        }
    }

    #[test]
    fn timestamps_round_trip_through_ids() {
        for secs in [0u64, 951_782_400, 1_759_759_391, 4_102_444_800] {
            let stamp = format_compact_timestamp(secs);
            assert_eq!(stamp.len(), TIMESTAMP_LEN);
            let (parsed, label) = parse_id(&format!("{stamp}-x")).unwrap();
            assert_eq!(parsed, secs, "{stamp}");
            assert_eq!(label, "x");
        }
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_compact_timestamp(1_759_759_391), "20251006T140311Z");
        assert!(parse_id("not-an-id").is_none());
        assert!(parse_id("20261332T000000Z-x").is_none());
    }

    #[test]
    fn labels_are_sanitised() {
        assert_eq!(sanitize_label(None), DEFAULT_LABEL);
        assert_eq!(
            sanitize_label(Some("holiday photos/2026")),
            "holiday_photos_2026"
        );
        assert_eq!(sanitize_label(Some("///")), DEFAULT_LABEL);
        assert_eq!(sanitize_label(Some("..x..")), "x");
        assert_eq!(
            sanitize_label(Some(&"y".repeat(200))).len(),
            MAX_LABEL_CHARS
        );
    }

    #[test]
    fn collision_suffixes_sort_numerically() {
        let base = "20261006T140311Z-photo";
        let mut ids = vec![
            base.to_string(),
            format!("{base}-2"),
            format!("{base}-10"),
            format!("{base}-3"),
        ];
        ids.sort_by(|a, b| id_sort_key(b).cmp(&id_sort_key(a)));
        assert_eq!(
            ids,
            vec![
                format!("{base}-10"),
                format!("{base}-3"),
                format!("{base}-2"),
                base.to_string()
            ]
        );
    }

    #[test]
    fn record_list_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("uploads");
        assert!(list_uploads(&history).unwrap().records.is_empty());

        let first = record_upload(&history, &manifest(Some("one"), 1), None).unwrap();
        let second = record_upload(&history, &manifest(None, 2), Some("photo.jpg")).unwrap();
        let third = record_upload(&history, &manifest(None, 3), Some("photo.jpg")).unwrap();
        assert!(first.id.ends_with("-one"));
        assert!(second.id.ends_with("-photo.jpg"));
        assert_ne!(second.id, third.id, "same second, same label, distinct ids");
        assert!(first.path.exists());

        let listing = list_uploads(&history).unwrap();
        assert_eq!(listing.records.len(), 3);
        assert!(listing.unreadable.is_empty());
        if second.recorded_at == third.recorded_at {
            let ids: Vec<_> = listing.records.iter().map(|r| r.id.as_str()).collect();
            let second_pos = ids.iter().position(|id| *id == second.id).unwrap();
            let third_pos = ids.iter().position(|id| *id == third.id).unwrap();
            assert!(third_pos < second_pos, "newer collision suffix lists first");
        }
        assert!(listing
            .records
            .windows(2)
            .all(|w| w[0].recorded_at >= w[1].recorded_at));

        let loaded = load_upload(&history, &first.id).unwrap().unwrap();
        assert_eq!(loaded.manifest, manifest(Some("one"), 1));
        assert_eq!(loaded.label, "one");
        assert!(load_upload(&history, "missing").unwrap().is_none());
        assert!(load_upload(&history, "../escape").unwrap().is_none());

        fs::write(history.join("junk.ant"), b"nope").unwrap();
        fs::write(history.join("ignored.txt"), b"nope").unwrap();
        let listing = list_uploads(&history).unwrap();
        assert_eq!(listing.records.len(), 3);
        assert_eq!(listing.unreadable.len(), 1);
    }
}
