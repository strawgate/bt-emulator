//! Transaction ids. Braintrust encodes `_xact_id` as
//! `(0x0DE1 << 48) | (unix_seconds << 16) | seq16`, sent as a decimal string.
//! Ids must be strictly increasing and fixed-width so string order matches
//! numeric order (the SDKs compare versions as strings).

pub const PREFIX: i64 = 0x0DE1 << 48;

/// Versions may also be given in the SDKs' display form: `xact * 205891132094649
/// mod 2^64` as 16 hex digits. This is the multiplicative inverse of that factor.
const COPRIME_INVERSE: u64 = 1_522_336_535_492_693_385;

/// The smallest xact id that may be issued at `unix_secs`.
pub fn floor_at(unix_secs: i64) -> i64 {
    PREFIX | (unix_secs << 16)
}

/// A version as clients send it: a decimal xact id, or its 16-digit pretty form.
pub fn parse_version(s: &str) -> Option<i64> {
    match u64::from_str_radix(s, 16) {
        Ok(pretty) if s.len() == 16 => Some((pretty.wrapping_mul(COPRIME_INVERSE) as i64) | PREFIX),
        _ => s.parse().ok(),
    }
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

    #[test]
    fn pretty_versions_round_trip() {
        // The two equivalent examples in the OpenAPI spec's `version` parameter.
        let xact = 1_000_192_656_880_881_099;
        assert_eq!(parse_version("81cd05ee665fdfb3"), Some(xact));
        assert_eq!(parse_version("1000192656880881099"), Some(xact));
        assert_eq!(parse_version("nope"), None);
    }
}
