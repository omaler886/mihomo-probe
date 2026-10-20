//! Shared validated geographical metadata; no lookup or IO lives here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitIdentity {
    pub ip: Option<String>,
    pub country: Option<String>,
    pub colo: Option<String>,
    pub asn: Option<String>,
}

pub const COUNTRY_CODES: &[&str] = &[
    "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT", "AU", "AW", "AX", "AZ",
    "BA", "BB", "BD", "BE", "BF", "BG", "BH", "BI", "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS",
    "BT", "BV", "BW", "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM", "CN",
    "CO", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM", "DO", "DZ", "EC", "EE",
    "EG", "EH", "ER", "ES", "ET", "FI", "FJ", "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF",
    "GG", "GH", "GI", "GL", "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM",
    "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ", "IR", "IS", "IT", "JE", "JM",
    "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC",
    "LI", "LK", "LR", "LS", "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH", "MK",
    "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU", "MV", "MW", "MX", "MY", "MZ", "NA",
    "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG",
    "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU", "RW",
    "SA", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "SS",
    "ST", "SV", "SX", "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK", "TL", "TM", "TN", "TO",
    "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG", "VI",
    "VN", "VU", "WF", "WS", "YE", "YT", "ZA", "ZM", "ZW",
];

/// Return whether a country is an assigned uppercase ISO alpha-2 code.
pub fn is_country_code(country: &str) -> bool {
    COUNTRY_CODES.binary_search(&country).is_ok()
}

/// Normalize a provider's numeric or "AS<number> organization" ASN without its name.
pub fn normalize_asn(value: &str) -> Option<String> {
    let raw = value.split_whitespace().next()?;
    let number = raw
        .strip_prefix("AS")
        .or_else(|| raw.strip_prefix("as"))
        .unwrap_or(raw);
    let number: u32 = number.parse().ok()?;
    (number != 0).then(|| format!("AS{number}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Provider descriptions cannot become unstable ASN identifiers.
    #[test]
    fn asn_normalization_accepts_only_valid_numeric_identifiers() {
        assert_eq!(normalize_asn("AS13335 Example"), Some("AS13335".into()));
        assert_eq!(normalize_asn("64512"), Some("AS64512".into()));
        for invalid in ["", "AS0", "AS-1", "AS4294967296", "unknown"] {
            assert!(normalize_asn(invalid).is_none());
        }
    }
}
