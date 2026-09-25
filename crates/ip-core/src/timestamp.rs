use std::fmt;

use chrono::{DateTime, Datelike, SubsecRound, TimeZone, Utc};

use crate::error::CoreError;

/// A point in time, held to whole seconds so what is stored is what is read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    /// Name of this kind, as it appears in errors.
    pub const KIND: &'static str = "timestamp";

    /// The current moment, truncated to a whole second.
    pub fn now() -> Self {
        Self(Utc::now().trunc_subsecs(0))
    }

    /// Wraps a count of whole seconds since the unix epoch.
    pub fn from_unix_seconds(seconds: i64) -> Result<Self, CoreError> {
        Utc.timestamp_opt(seconds, 0)
            .single()
            .map(Self)
            .ok_or(CoreError::Timestamp { seconds })
    }

    /// Whole seconds since the unix epoch, as it is stored.
    pub fn unix_seconds(&self) -> i64 {
        self.0.timestamp()
    }

    /// Midnight utc on the first of the month this moment falls in.
    pub fn month_began(&self) -> Self {
        Self(
            self.0
                .with_day(1)
                .and_then(|first| first.with_time(chrono::NaiveTime::MIN).single())
                .unwrap_or(self.0),
        )
    }

    /// Midnight utc on the first of the month after the one this moment falls in.
    pub fn next_month(&self) -> Self {
        let first = self.month_began().0;
        let next = match first.month() {
            12 => first
                .with_year(first.year() + 1)
                .and_then(|y| y.with_month(1)),
            month => first.with_month(month + 1),
        };
        Self(next.unwrap_or(first))
    }

    /// The moment in rfc 3339 form, for logs and audit records.
    pub fn to_rfc3339(&self) -> String {
        self.0.to_rfc3339()
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.unix_seconds())
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let seconds = i64::deserialize(deserializer)?;
        Self::from_unix_seconds(seconds).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_carries_no_sub_second_part() {
        let now = Timestamp::now();
        assert_eq!(
            Timestamp::from_unix_seconds(now.unix_seconds()).unwrap(),
            now
        );
    }

    #[test]
    fn round_trips_through_unix_seconds() {
        let stamp = Timestamp::from_unix_seconds(1_767_225_600).unwrap();
        assert_eq!(stamp.unix_seconds(), 1_767_225_600);
        assert_eq!(stamp.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn accepts_a_moment_before_the_epoch() {
        assert_eq!(Timestamp::from_unix_seconds(-1).unwrap().unix_seconds(), -1);
    }

    #[test]
    fn rejects_a_count_no_calendar_can_hold() {
        assert_eq!(
            Timestamp::from_unix_seconds(i64::MAX),
            Err(CoreError::Timestamp { seconds: i64::MAX })
        );
    }

    #[test]
    fn orders_by_the_moment() {
        let earlier = Timestamp::from_unix_seconds(1).unwrap();
        let later = Timestamp::from_unix_seconds(2).unwrap();
        assert!(earlier < later);
    }

    #[test]
    fn serializes_as_whole_seconds() {
        let stamp = Timestamp::from_unix_seconds(1_767_225_600).unwrap();
        assert_eq!(serde_json::to_string(&stamp).unwrap(), "1767225600");
        assert_eq!(
            serde_json::from_str::<Timestamp>("1767225600").unwrap(),
            stamp
        );
    }
}
