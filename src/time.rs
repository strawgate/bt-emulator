use chrono::{DateTime, SecondsFormat, Utc};

/// Current time in Braintrust's wire format: RFC 3339, millisecond precision, `Z`.
pub fn now_iso() -> String {
    format_iso(Utc::now())
}

pub fn format_iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Normalize a client-supplied timestamp to the wire format; unparseable values pass through.
pub fn normalize(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map_or_else(|_| s.to_owned(), |t| format_iso(t.with_timezone(&Utc)))
}

pub fn unix_secs() -> i64 {
    Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_python_timestamps_to_millis_z() {
        assert_eq!(
            normalize("2026-08-19T18:46:04.996084+00:00"),
            "2026-08-19T18:46:04.996Z"
        );
        assert_eq!(
            normalize("2026-08-19T20:46:04+02:00"),
            "2026-08-19T18:46:04.000Z"
        );
        assert_eq!(normalize("not a date"), "not a date");
    }

    #[test]
    fn now_is_in_wire_format() {
        let now = now_iso();
        assert!(now.ends_with('Z') && now.len() == 24, "{now}");
        assert!(unix_secs() > 1_700_000_000);
    }
}
