//! ISO 3166-1 alpha-2 codes, for reading a country out of a server name.
//!
//! The list exists to keep a two-letter word from being mistaken for a country.
//! Providers name nodes things like `DE-2`, and reading `DE` off that is worth
//! doing — but a name beginning `AB-` or `XX-` must stay unknown rather than
//! produce a country that does not exist, because the code goes on to seed
//! alias suggestions and the country filter.

/// Every assigned alpha-2 code, sorted so a lookup is a binary search.
///
/// User-assigned ranges (`AA`, `QM`-`QZ`, `XA`-`XZ`, `ZZ`) are deliberately
/// absent: nothing is gained by resolving them, and `XX` in particular is a
/// common placeholder in provider lists.
const ALPHA2: &[&str] = &[
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

/// Whether `code` is an assigned ISO 3166-1 alpha-2 code. Case-insensitive.
pub fn is_alpha2(code: &str) -> bool {
    if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let upper = code.to_ascii_uppercase();
    ALPHA2.binary_search(&upper.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_sorted_so_the_search_is_correct() {
        assert!(
            ALPHA2.windows(2).all(|pair| pair[0] < pair[1]),
            "binary_search silently misses entries in an unsorted list"
        );
    }

    #[test]
    fn real_codes_are_recognised_in_either_case() {
        for code in ["DE", "fi", "Us", "ru", "GB"] {
            assert!(is_alpha2(code), "{code}");
        }
    }

    #[test]
    fn non_countries_are_not() {
        // `XX` and `AA` are user-assigned and appear as placeholders; the rest
        // are simply not codes.
        for code in ["XX", "AA", "QQ", "ZZ", "D", "DEU", "D1", "", "  "] {
            assert!(!is_alpha2(code), "{code}");
        }
    }
}
