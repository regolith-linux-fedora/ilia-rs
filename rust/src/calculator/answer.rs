//! How a value worked out by the libqalculate helper is shown.
//!
//! The helper writes a value as its leading significant digits, cut off
//! (not rounded) after [`DIGITS`] of them, the power of ten of the first
//! digit, and whether the value is an exact whole number. Cutting off rather
//! than rounding keeps the rounding below exact: the digits after the
//! sixteenth only decide between rounding down and rounding away from zero,
//! and a value whose sixteenth digit is a 5 rounds away from zero whether or
//! not anything follows it.
//!
//! The helper also writes the decimal mark of the locale's numbers
//! (`LC_ALL`, else `LC_NUMERIC`, else `LANG`), which it reads from the
//! environment it shares with the launcher.

/// The significant digits the helper writes.
pub const DIGITS: usize = 40;
/// The significant digits shown, unless a whole number is shown in full.
const SHOWN_DIGITS: usize = 15;
/// Whole numbers with up to this many digits are shown in full.
const FULL_WHOLE_DIGITS: i64 = 40;
/// Values shown without an exponent lie in `[1e-6, 1e40)` after rounding.
const PLAIN_EXPONENTS: std::ops::Range<i64> = -6..40;

/// A real, finite value as the helper writes it.
#[derive(Debug, PartialEq, Eq)]
pub struct Value {
    negative: bool,
    /// Significant digits, without leading zeros; empty for zero.
    digits: Vec<u8>,
    /// The power of ten of the first digit.
    exponent: i64,
    /// An exact whole number, rather than an approximation or a fraction.
    whole: bool,
    /// Between the whole and the fractional digits.
    decimal_mark: String,
}

impl Value {
    /// Reads the helper's reply,
    /// `value <+|-> <digits> <exponent> <whole|part> <decimal mark>` for a
    /// value, `none` (or anything else) for no value.
    pub fn parse(reply: &str) -> Option<Self> {
        let mut fields = reply.trim_end_matches('\n').split(' ');
        if fields.next()? != "value" {
            return None;
        }
        let negative = match fields.next()? {
            "+" => false,
            "-" => true,
            _ => return None,
        };
        let digits = fields.next()?;
        if digits.is_empty() || digits.len() > DIGITS || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let exponent = fields.next()?.parse().ok()?;
        let whole = match fields.next()? {
            "whole" => true,
            "part" => false,
            _ => return None,
        };
        let decimal_mark = fields.next().filter(|mark| !mark.is_empty())?;
        if fields.next().is_some() {
            return None;
        }
        let digits = digits
            .trim_end_matches('0')
            .bytes()
            .map(|b| b - b'0')
            .collect::<Vec<_>>();
        if digits.first() == Some(&0) {
            return None;
        }
        Some(Self {
            negative,
            digits,
            exponent,
            whole,
            decimal_mark: decimal_mark.to_owned(),
        })
    }

    /// The value as shown.
    pub fn show(&self) -> String {
        let decimal_mark = self.decimal_mark.as_str();
        if self.digits.is_empty() {
            // Zero, negative zero included.
            return "0".to_owned();
        }
        let sign = if self.negative { "-" } else { "" };
        if self.whole && self.exponent < FULL_WHOLE_DIGITS {
            // The helper writes every digit of a whole number this short.
            return format!("{sign}{}", plain(&self.digits, self.exponent, decimal_mark));
        }
        let (digits, exponent) = round(&self.digits, self.exponent);
        let shown = if PLAIN_EXPONENTS.contains(&exponent) {
            plain(&digits, exponent, decimal_mark)
        } else {
            scientific(&digits, exponent, decimal_mark)
        };
        format!("{sign}{shown}")
    }
}

/// The digits rounded to [`SHOWN_DIGITS`], half away from zero, without
/// trailing zeros, and the power of ten of the first one.
fn round(digits: &[u8], exponent: i64) -> (Vec<u8>, i64) {
    let mut rounded: Vec<u8> = digits.iter().copied().take(SHOWN_DIGITS).collect();
    let mut exponent = exponent;
    if digits.get(SHOWN_DIGITS).is_some_and(|digit| *digit >= 5) {
        // Add one in the last place, carrying through the nines.
        while rounded.last() == Some(&9) {
            rounded.pop();
        }
        match rounded.last_mut() {
            Some(last) => *last += 1,
            None => {
                rounded.push(1);
                exponent += 1;
            }
        }
    }
    while rounded.last() == Some(&0) {
        rounded.pop();
    }
    (rounded, exponent)
}

fn text(digits: &[u8]) -> String {
    digits
        .iter()
        .map(|digit| char::from(b'0' + digit))
        .collect()
}

/// `12.5`, `0.000001` or `3333333333333330`.
fn plain(digits: &[u8], exponent: i64, decimal_mark: &str) -> String {
    if exponent < 0 {
        let zeros = "0".repeat((-exponent - 1) as usize);
        return format!("0{decimal_mark}{zeros}{}", text(digits));
    }
    let whole_digits = exponent as usize + 1;
    if digits.len() <= whole_digits {
        format!(
            "{}{}",
            text(digits),
            "0".repeat(whole_digits - digits.len())
        )
    } else {
        let (whole, fraction) = digits.split_at(whole_digits);
        format!("{}{decimal_mark}{}", text(whole), text(fraction))
    }
}

/// `1.42724769270596e45`, `7e-7`.
fn scientific(digits: &[u8], exponent: i64, decimal_mark: &str) -> String {
    let (first, rest) = digits.split_at(1);
    if rest.is_empty() {
        format!("{}e{exponent}", text(first))
    } else {
        format!("{}{decimal_mark}{}e{exponent}", text(first), text(rest))
    }
}

#[cfg(test)]
mod tests {
    use super::Value;

    /// The reply for an exact decimal value written out in `number`, in a
    /// locale whose decimal mark is `decimal_mark`.
    fn exact_in(number: &str, decimal_mark: &str) -> String {
        let (negative, unsigned) = match number.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, number),
        };
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let all = format!("{whole}{fraction}");
        let leading = all.len() - all.trim_start_matches('0').len();
        let significant = &all[leading..];
        let exponent = whole.len() as i64 - 1 - leading as i64;
        let kind = if fraction.trim_end_matches('0').is_empty() {
            "whole"
        } else {
            "part"
        };
        let digits = if significant.is_empty() {
            "0"
        } else {
            &significant[..significant.len().min(40)]
        };
        let sign = if negative { '-' } else { '+' };
        format!("value {sign} {digits} {exponent} {kind} {decimal_mark}")
    }

    fn exact(number: &str) -> String {
        exact_in(number, ".")
    }

    fn shown(number: &str) -> String {
        Value::parse(&exact(number)).expect(number).show()
    }

    #[test]
    fn whole_numbers_up_to_forty_digits_are_shown_in_full() {
        assert_eq!(shown("14"), "14");
        assert_eq!(shown("-9"), "-9");
        assert_eq!(
            shown("1267650600228229401496703205376"),
            "1267650600228229401496703205376"
        );
        assert_eq!(
            shown("9999999999999999999999999999999999999999"),
            "9999999999999999999999999999999999999999"
        );
        assert_eq!(
            shown("-9999999999999999999999999999999999999999"),
            "-9999999999999999999999999999999999999999"
        );
        assert_eq!(
            shown("1000000000000000000000000000000000000000"),
            "1000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn longer_whole_numbers_are_rounded_and_scientific() {
        assert_eq!(shown("10000000000000000000000000000000000000000"), "1e40");
        assert_eq!(shown("-10000000000000000000000000000000000000000"), "-1e40");
        assert_eq!(
            shown("1427247692705959881058285969449495136382746624"),
            "1.42724769270596e45"
        );
        assert_eq!(
            shown("30414093201713378043612608166064768844377641568960512000000000000"),
            "3.04140932017134e64"
        );
        assert_eq!(
            shown("125000000000000000000000000000000000000000"),
            "1.25e41"
        );
    }

    #[test]
    fn other_values_are_rounded_to_fifteen_digits_half_away_from_zero() {
        assert_eq!(
            shown("0.142857142857142857142857142857142857142857"),
            "0.142857142857143"
        );
        assert_eq!(
            shown("3.333333333333333333333333333333"),
            "3.33333333333333"
        );
        assert_eq!(
            shown("3333333333333333.3333333333333333"),
            "3333333333333330"
        );
        assert_eq!(shown("9.999999999999995"), "10");
        assert_eq!(shown("-9.999999999999995"), "-10");
        assert_eq!(shown("2.000000000000005"), "2.00000000000001");
        assert_eq!(shown("-2.000000000000005"), "-2.00000000000001");
        assert_eq!(shown("2.000000000000004"), "2");
        assert_eq!(shown("2.0000000000000049999999999999999999999999"), "2");
        assert_eq!(shown("1.230000"), "1.23");
        assert_eq!(shown("-1.75"), "-1.75");
        assert_eq!(shown("0.8"), "0.8");
    }

    #[test]
    fn small_and_large_values_switch_to_scientific_after_rounding() {
        assert_eq!(shown("0.000001"), "0.000001");
        assert_eq!(shown("-0.000001"), "-0.000001");
        assert_eq!(shown("0.0000007"), "7e-7");
        assert_eq!(shown("0.0000009999999999999995"), "0.000001");
        assert_eq!(shown("0.0000009999999999999994"), "9.99999999999999e-7");
        assert_eq!(shown("0.0000000333333333333333333"), "3.33333333333333e-8");
        assert_eq!(
            shown("-0.000000000666666666666666666"),
            "-6.66666666666667e-10"
        );
        assert_eq!(shown("10000000000000000000000000000000000000000.5"), "1e40");
        assert_eq!(shown("9999999999999999999999999999999999999999.5"), "1e40");
        // Rounding carries into a fortieth digit, which still fits.
        assert_eq!(
            shown("999999999999999900000000000000000000000.5"),
            "1000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn zero_has_no_sign() {
        assert_eq!(Value::parse("value + 0 0 whole ,").unwrap().show(), "0");
        assert_eq!(Value::parse("value - 0 0 whole ,").unwrap().show(), "0");
    }

    #[test]
    fn the_locale_decimal_mark_is_used_in_mantissas_too() {
        let shown_in = |number: &str| Value::parse(&exact_in(number, ",")).unwrap().show();
        assert_eq!(shown_in("0.125"), "0,125");
        assert_eq!(
            shown_in("2.302585092994045684017991454684364207601"),
            "2,30258509299405"
        );
        assert_eq!(
            shown_in("0.0000000333333333333333333"),
            "3,33333333333333e-8"
        );
        assert_eq!(shown_in("5"), "5");
        // Any mark the locale has, whatever its length, as it comes.
        let arabic = Value::parse(&exact_in("-1.5", "\u{66b}")).unwrap();
        assert_eq!(arabic.show(), "-1\u{66b}5");
    }

    #[test]
    fn malformed_replies_have_no_value() {
        for reply in [
            "",
            "none",
            "value",
            "value + 14",
            "value + 14 1",
            "value * 14 1 whole .",
            "value + 1x 1 whole .",
            "value + 14 x whole .",
            "value + 14 1 maybe .",
            "value + 014 2 whole .",
            "value +  1 0 whole .",
            // No decimal mark, an empty one, or more after it.
            "value + 14 1 whole",
            "value + 14 1 whole ",
            "value + 14 1 whole . extra",
            "value + 14 1 whole  .",
        ] {
            assert_eq!(Value::parse(reply), None, "{reply:?}");
        }
        let too_long = format!("value + {} 40 whole .", "1".repeat(41));
        assert_eq!(Value::parse(&too_long), None);
    }
}
