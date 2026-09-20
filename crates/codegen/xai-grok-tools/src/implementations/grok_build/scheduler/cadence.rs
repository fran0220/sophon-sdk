//! Calendar recurrence is resolved here, never by host timers or polling phase.
use chrono::{DateTime, Datelike, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::types::SchedulerError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SchedulerCadence {
    Once {
        at: DateTime<Utc>,
    },
    /// The anchor itself is an occurrence; subsequent ones are exact multiples.
    Interval {
        every_secs: u64,
        anchor: DateTime<Utc>,
    },
    Daily {
        time: String,
        time_zone: String,
        weekdays: Option<Vec<u8>>,
    },
}

impl SchedulerCadence {
    pub fn validate(&self) -> Result<(), SchedulerError> {
        match self {
            Self::Once { .. } => Ok(()),
            Self::Interval { every_secs, .. } => {
                if *every_secs == 0 || *every_secs > i64::MAX as u64 / 1_000 {
                    return Err(SchedulerError::InvalidInterval(
                        "everySecs must be positive and representable".into(),
                    ));
                }
                Ok(())
            }
            Self::Daily {
                time,
                time_zone,
                weekdays,
            } => {
                Self::local_time(time)?;
                Self::zone(time_zone)?;
                if weekdays.as_ref().is_some_and(|days| {
                    days.is_empty()
                        || days.iter().any(|day| !(1..=7).contains(day))
                        || days
                            .iter()
                            .enumerate()
                            .any(|(i, day)| days[..i].contains(day))
                }) {
                    return Err(SchedulerError::InvalidInterval(
                        "weekdays must be nonempty unique ISO weekdays 1-7".into(),
                    ));
                }
                Ok(())
            }
        }
    }

    fn local_time(time: &str) -> Result<NaiveTime, SchedulerError> {
        if time.len() != 5
            || time.as_bytes().get(2) != Some(&b':')
            || !time
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 2 || b.is_ascii_digit())
        {
            return Err(SchedulerError::InvalidInterval(
                "daily time must be HH:MM".into(),
            ));
        }
        NaiveTime::parse_from_str(time, "%H:%M")
            .map_err(|_| SchedulerError::InvalidInterval("daily time must be valid HH:MM".into()))
    }

    fn zone(zone: &str) -> Result<chrono_tz::Tz, SchedulerError> {
        zone.parse()
            .map_err(|_| SchedulerError::InvalidInterval(format!("unknown IANA timezone: {zone}")))
    }

    fn on_date(zone: chrono_tz::Tz, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
        let mut local = date.and_time(time);
        // A civil date can be skipped entirely (e.g. Pacific/Apia). Walk seconds,
        // not UTC hours: historical IANA transitions need not be minute aligned.
        for _ in 0..=172_800 {
            match zone.from_local_datetime(&local) {
                LocalResult::Single(at) => return Some(at.with_timezone(&Utc)),
                LocalResult::Ambiguous(first, second) => {
                    return Some(first.min(second).with_timezone(&Utc));
                }
                LocalResult::None => {
                    local = local.checked_add_signed(chrono::Duration::seconds(1))?
                }
            }
        }
        None
    }

    /// First occurrence strictly after `after`. A fold's second occurrence is
    /// deliberately never considered, including when resuming inside the fold.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Once { at } => (*at > after).then_some(*at),
            Self::Interval { every_secs, anchor } => {
                self.validate().ok()?;
                if after < *anchor {
                    return Some(*anchor);
                }
                let periods = (after - *anchor)
                    .num_seconds()
                    .checked_div(*every_secs as i64)?
                    .checked_add(1)?;
                anchor.checked_add_signed(chrono::Duration::try_seconds(
                    periods.checked_mul(*every_secs as i64)?,
                )?)
            }
            Self::Daily {
                time,
                time_zone,
                weekdays,
            } => {
                self.validate().ok()?;
                let zone = Self::zone(time_zone).ok()?;
                let time = Self::local_time(time).ok()?;
                let mut date = after.with_timezone(&zone).date_naive();
                for _ in 0..9 {
                    if weekdays.as_ref().is_none_or(|days| {
                        days.contains(&(date.weekday().number_from_monday() as u8))
                    }) {
                        if let Some(at) = Self::on_date(zone, date, time) {
                            if at > after {
                                return Some(at);
                            }
                        }
                    }
                    date = date.succ_opt()?;
                }
                None
            }
        }
    }

    pub fn recurring(&self) -> bool {
        !matches!(self, Self::Once { .. })
    }

    pub fn human_schedule(&self) -> String {
        match self {
            Self::Once { at } => format!("once at {}", at.to_rfc3339()),
            Self::Interval { every_secs, .. } => super::interval::interval_to_human(*every_secs),
            Self::Daily {
                time,
                time_zone,
                weekdays,
            } => match weekdays {
                Some(days) => format!("at {time} {time_zone}, ISO weekdays {days:?}"),
                None => format!("daily at {time} {time_zone}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    fn daily(time: &str, days: Option<Vec<u8>>) -> SchedulerCadence {
        SchedulerCadence::Daily {
            time: time.into(),
            time_zone: "America/New_York".into(),
            weekdays: days,
        }
    }

    #[test]
    fn anchored_interval_does_not_drift_or_burst() {
        let cadence = SchedulerCadence::Interval {
            every_secs: 137,
            anchor: at("2026-01-01T00:00:17Z"),
        };
        assert_eq!(
            cadence.next_after(at("2026-01-01T00:00:00Z")),
            Some(at("2026-01-01T00:00:17Z"))
        );
        assert_eq!(
            cadence.next_after(at("2026-01-01T00:00:17Z")),
            Some(at("2026-01-01T00:02:34Z"))
        );
        assert_eq!(
            cadence.next_after(at("2026-01-01T00:10:00Z")),
            Some(at("2026-01-01T00:11:42Z"))
        );
    }

    #[test]
    fn gap_uses_first_valid_instant_not_shifted_clock() {
        let cadence = daily("02:37", None);
        assert_eq!(
            cadence.next_after(at("2026-03-08T00:00:00Z")),
            Some(at("2026-03-08T07:00:00Z"))
        );
        assert_eq!(
            cadence.next_after(at("2026-03-08T07:00:00Z")),
            Some(at("2026-03-09T06:37:00Z"))
        );
    }

    #[test]
    fn fold_uses_first_occurrence_only() {
        let cadence = daily("01:23", None);
        assert_eq!(
            cadence.next_after(at("2026-11-01T00:00:00Z")),
            Some(at("2026-11-01T05:23:00Z"))
        );
        assert_eq!(
            cadence.next_after(at("2026-11-01T05:23:00Z")),
            Some(at("2026-11-02T06:23:00Z"))
        );
        assert_eq!(
            cadence.next_after(at("2026-11-01T06:00:00Z")),
            Some(at("2026-11-02T06:23:00Z"))
        );
    }

    #[test]
    fn weekdays_roll_over_week_and_dst() {
        let cadence = daily("09:17", Some(vec![1, 5]));
        assert_eq!(
            cadence.next_after(at("2026-03-06T14:17:00Z")),
            Some(at("2026-03-09T13:17:00Z"))
        );
    }

    #[test]
    fn once_and_invalid_cadences() {
        let instant = at("2026-04-19T01:02:03Z");
        let once = SchedulerCadence::Once { at: instant };
        assert_eq!(once.next_after(at("2026-04-19T01:02:02Z")), Some(instant));
        assert_eq!(once.next_after(instant), None);
        for time in ["9:00", "24:00", "12:60", "12:30:00", "garbage"] {
            assert!(daily(time, None).validate().is_err());
        }
        for days in [vec![], vec![0], vec![8], vec![2, 2]] {
            assert!(daily("12:30", Some(days)).validate().is_err());
        }
        assert!(
            SchedulerCadence::Daily {
                time: "12:30".into(),
                time_zone: "Mars/Olympus".into(),
                weekdays: None
            }
            .validate()
            .is_err()
        );
        assert!(
            serde_json::from_str::<SchedulerCadence>(r#"{"kind":"once","at":"tomorrow"}"#).is_err()
        );
    }
}
