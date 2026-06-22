//! Money handling for Stellar amounts.
//!
//! Stellar represents every balance as an integer number of *stroops*, where
//! `1 unit = 10_000_000 stroops` (7 decimal places). Doing arithmetic and
//! comparisons in stroops avoids the rounding pitfalls of binary floating point,
//! which is why we never compare payment amounts as `f64`.

/// Number of stroops in one whole unit of any Stellar asset.
pub const STROOPS_PER_UNIT: i64 = 10_000_000;

/// Maximum number of decimal places a Stellar amount may carry.
pub const MAX_DECIMALS: usize = 7;

/// Parse a positive decimal amount string into stroops.
///
/// Returns `None` if the value is empty, malformed, signed, non-positive,
/// carries more than [`MAX_DECIMALS`] decimal places, or overflows `i64`.
///
/// ```
/// use stellargate::money::parse_stroops;
/// assert_eq!(parse_stroops("1"), Some(10_000_000));
/// assert_eq!(parse_stroops("0.0000001"), Some(1));
/// assert_eq!(parse_stroops("10.50"), Some(105_000_000));
/// assert_eq!(parse_stroops("0"), None);
/// assert_eq!(parse_stroops("-1"), None);
/// assert_eq!(parse_stroops("1.00000001"), None);
/// ```
pub fn parse_stroops(input: &str) -> Option<i64> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }

    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };

    // Reject signs, whitespace, exponents — only plain digits in each segment.
    if !int_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if frac_part.len() > MAX_DECIMALS {
        return None;
    }

    let int_val: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };

    // Right-pad the fractional part to exactly 7 digits so it reads as stroops.
    let mut frac = String::with_capacity(MAX_DECIMALS);
    frac.push_str(frac_part);
    while frac.len() < MAX_DECIMALS {
        frac.push('0');
    }
    let frac_val: i64 = frac.parse().ok()?;

    let stroops = int_val
        .checked_mul(STROOPS_PER_UNIT)?
        .checked_add(frac_val)?;

    if stroops <= 0 {
        return None;
    }
    Some(stroops)
}

/// Returns `true` if `input` is a valid, strictly-positive Stellar amount.
pub fn is_valid_amount(input: &str) -> bool {
    parse_stroops(input).is_some()
}

/// Format a stroop count as a minimal-decimal Stellar amount string.
///
/// Trailing fractional zeros are stripped so the result is compact:
/// `10_000_000` → `"1"`, `15_500_000` → `"1.55"`, `5_000_000` → `"0.5"`.
pub fn stroops_to_string(stroops: i64) -> String {
    let whole = stroops / STROOPS_PER_UNIT;
    let frac = stroops % STROOPS_PER_UNIT;
    if frac == 0 {
        format!("{whole}")
    } else {
        let padded = format!("{frac:07}");
        let trimmed = padded.trim_end_matches('0');
        format!("{whole}.{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whole_and_fractional() {
        assert_eq!(parse_stroops("1"), Some(10_000_000));
        assert_eq!(parse_stroops("10"), Some(100_000_000));
        assert_eq!(parse_stroops("10.00"), Some(100_000_000));
        assert_eq!(parse_stroops("10.50"), Some(105_000_000));
        assert_eq!(parse_stroops("0.0000001"), Some(1));
        assert_eq!(parse_stroops(".5"), Some(5_000_000));
        assert_eq!(parse_stroops("  2.5  "), Some(25_000_000));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(parse_stroops(""), None);
        assert_eq!(parse_stroops("0"), None);
        assert_eq!(parse_stroops("0.0"), None);
        assert_eq!(parse_stroops("-1"), None);
        assert_eq!(parse_stroops("+1"), None);
        assert_eq!(parse_stroops("abc"), None);
        assert_eq!(parse_stroops("1.2.3"), None);
        assert_eq!(parse_stroops("1e3"), None);
        assert_eq!(parse_stroops("1.00000001"), None); // 8 decimals
        assert_eq!(parse_stroops("9999999999999999999"), None); // overflow
    }

    #[test]
    fn stroops_to_string_works() {
        assert_eq!(stroops_to_string(10_000_000), "1");
        assert_eq!(stroops_to_string(100_000_000), "10");
        assert_eq!(stroops_to_string(15_000_000), "1.5");
        assert_eq!(stroops_to_string(15_500_000), "1.55");
        assert_eq!(stroops_to_string(5_000_000), "0.5");
        assert_eq!(stroops_to_string(1), "0.0000001");
        assert_eq!(stroops_to_string(105_000_000), "10.5");
    }

    #[test]
    fn comparisons_are_exact() {
        // The classic float trap: 0.1 + 0.2 != 0.3 in f64, but exact in stroops.
        let a = parse_stroops("0.1").unwrap() + parse_stroops("0.2").unwrap();
        assert_eq!(a, parse_stroops("0.3").unwrap());
    }
}
