//! Case-insensitive natural ordering ("img2" before "img10").

use std::cmp::Ordering;

/// Compares two already-lowercased strings, treating runs of digits as numbers.
/// Names that differ only in leading zeros compare equal; callers break ties.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (na, rest_a) = split_number(a);
                let (nb, rest_b) = split_number(b);
                let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if ord != Ordering::Equal {
                    return ord;
                }
                (a, b) = (rest_a, rest_b);
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                (a, b) = (&a[1..], &b[1..]);
            }
        }
    }
}

/// Splits a leading digit run off `s`, returning it without leading zeros, and the rest.
fn split_number(s: &[u8]) -> (&[u8], &[u8]) {
    let len = s.iter().take_while(|c| c.is_ascii_digit()).count();
    let (digits, rest) = s.split_at(len);
    let zeros = digits.iter().take_while(|&&c| c == b'0').count();
    (&digits[zeros..], rest)
}

#[cfg(test)]
mod tests {
    use super::compare;
    use std::cmp::Ordering::*;

    #[test]
    fn numbers_compare_by_value() {
        assert_eq!(compare("img2.jpg", "img10.jpg"), Less);
        assert_eq!(compare("img10.jpg", "img9.jpg"), Greater);
        assert_eq!(compare("a99b", "a100a"), Less);
        assert_eq!(compare("x007", "x7"), Equal);
    }

    #[test]
    fn text_and_prefixes() {
        assert_eq!(compare("apple", "banana"), Less);
        assert_eq!(compare("file", "file1"), Less);
        assert_eq!(compare("", "a"), Less);
        assert_eq!(compare("same", "same"), Equal);
    }

    #[test]
    fn long_digit_runs_do_not_overflow() {
        assert_eq!(
            compare(
                "p123456789012345678901234567890",
                "p99999999999999999999999999999"
            ),
            Greater
        );
    }
}
