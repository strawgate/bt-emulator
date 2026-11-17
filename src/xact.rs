//! Transaction ids. Braintrust encodes `_xact_id` as
//! `(0x0DE1 << 48) | (unix_seconds << 16) | seq16`, sent as a decimal string.
//! Ids must be strictly increasing and fixed-width so string order matches
//! numeric order (the SDKs compare versions as strings).

pub const PREFIX: i64 = 0x0DE1 << 48;

/// The smallest xact id that may be issued at `unix_secs`.
pub fn floor_at(unix_secs: i64) -> i64 {
    PREFIX | (unix_secs << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_recorded_braintrust_xact_ids() {
        // From a recorded /logs3 response at 2026-08-19T18:46:05Z with seq 65529.
        assert_eq!(floor_at(1_787_165_165) | 65529, 1_000_197_715_909_279_737);
    }

    #[test]
    fn ids_are_fixed_width_for_the_foreseeable_future() {
        assert_eq!(floor_at(1_700_000_000).to_string().len(), 19);
        assert_eq!(floor_at(4_000_000_000).to_string().len(), 19);
    }
}
