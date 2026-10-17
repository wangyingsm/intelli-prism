//! The arithmetic that says a match is what its shape suggested.
//!
//! A pattern alone cannot tell a card number from eighteen digits of telemetry. Each
//! function here is the issuer's own check, so a kind that has one fires on about one
//! random string in a hundred rather than on every number in a code sample. That is the
//! whole reason the checked kinds can be switched on by default and the others cannot.

/// Whether a payment card number passes Luhn, separators ignored.
pub(crate) fn luhn(text: &str) -> bool {
    let digits: Vec<u32> = text.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    // A card of one repeated digit passes Luhn as readily as a real one, and test data
    // is full of them, so the arithmetic alone is not enough to call it a card.
    if digits.windows(2).all(|pair| pair[0] == pair[1]) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(at, digit)| match at % 2 {
            1 if *digit > 4 => digit * 2 - 9,
            1 => digit * 2,
            _ => *digit,
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Whether a mainland China resident identity number's check character agrees with its
/// first seventeen digits, by ISO 7064 mod 11-2, and whether the date inside it could be
/// a birth date.
pub(crate) fn prc_id(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() != 18 {
        return false;
    }
    let Some(digits) = chars[..17]
        .iter()
        .map(|c| c.to_digit(10))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    if !plausible_birth(&digits[6..14]) {
        return false;
    }
    const WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECKS: [char; 11] = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];
    let sum: u32 = digits
        .iter()
        .zip(WEIGHTS)
        .map(|(digit, weight)| digit * weight)
        .sum();
    CHECKS[(sum % 11) as usize] == chars[17].to_ascii_uppercase()
}

/// Whether eight digits read as a date anyone could have been born on.
///
/// The range is wider than any living person because this decides whether to *redact*:
/// a year floor set at the oldest plausible birth would quietly pass over real
/// identities in historical records, and missing one costs more than carrying a range
/// three centuries wide. It still leaves random digits almost no way through — a year
/// in range, a month in twelve and a day in thirty-one, before the check character.
fn plausible_birth(digits: &[u32]) -> bool {
    let number = |at: usize, width: usize| {
        digits[at..at + width]
            .iter()
            .fold(0, |sum, digit| sum * 10 + digit)
    };
    let (year, month, day) = (number(0, 4), number(4, 2), number(6, 2));
    (1800..=2100).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Whether an international bank account number passes mod-97, spaces ignored.
pub(crate) fn iban(text: &str) -> bool {
    let account: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !(15..=34).contains(&account.len()) {
        return false;
    }
    if !account[..2].iter().all(char::is_ascii_uppercase)
        || !account[2..4].iter().all(char::is_ascii_digit)
    {
        return false;
    }
    // The country and check digits move to the end before the sum, and each letter
    // becomes two digits. The remainder is carried rather than the number, which has no
    // integer wide enough to hold it.
    let mut rest = 0u32;
    for c in account[4..].iter().chain(account[..4].iter()) {
        let value = if c.is_ascii_digit() {
            u32::from(*c) - u32::from(b'0')
        } else if c.is_ascii_uppercase() {
            u32::from(*c) - u32::from(b'A') + 10
        } else {
            return false;
        };
        rest = if value > 9 {
            (rest * 100 + value) % 97
        } else {
            (rest * 10 + value) % 97
        };
    }
    rest == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published test numbers every card network gives out, which are what a reader
    /// can check this against without trusting the implementation.
    #[test]
    fn the_networks_own_test_numbers_pass_luhn() {
        for number in [
            "4242424242424242",
            "4012888888881881",
            "5555555555554444",
            "378282246310005",
            "6011111111111117",
            "3530111333300000",
        ] {
            assert!(luhn(number), "{number} is a published test card");
        }
    }

    #[test]
    fn separators_do_not_change_whether_a_card_passes() {
        assert!(luhn("4242 4242 4242 4242"));
        assert!(luhn("4242-4242-4242-4242"));
    }

    #[test]
    fn one_digit_wrong_fails_luhn() {
        assert!(!luhn("4242424242424243"));
        assert!(!luhn("4012888888881882"));
    }

    /// Eighteen digits of telemetry, a timestamp in nanoseconds, an order number — the
    /// strings a code sample is full of and a filter must not shred.
    #[test]
    fn ordinary_long_numbers_are_not_cards() {
        let mut shredded = 0;
        for number in [
            "1234567890123",
            "1699999999999999",
            "20260101120000000",
            "9007199254740991",
            "1111111111111111111",
        ] {
            if luhn(number) {
                shredded += 1;
            }
        }
        assert_eq!(shredded, 0, "{shredded} ordinary numbers read as cards");
    }

    /// A repeated digit passes the arithmetic, and test fixtures are full of them.
    #[test]
    fn a_run_of_one_digit_is_not_a_card_however_the_sum_falls() {
        assert!(!luhn("0000000000000000"));
        assert!(!luhn("5555555555555555"));
    }

    #[test]
    fn too_short_and_too_long_are_not_cards() {
        assert!(!luhn("424242424242"));
        assert!(!luhn("42424242424242424242"));
    }

    /// The check character is the last, and `X` is one of its eleven values.
    #[test]
    fn a_resident_id_passes_when_its_check_character_agrees() {
        assert!(prc_id("11010519491231002X"));
        assert!(
            prc_id("11010519491231002x"),
            "the check character is not cased"
        );
        assert!(prc_id("440524188001010014"));
    }

    #[test]
    fn a_resident_id_with_the_wrong_check_character_fails() {
        assert!(!prc_id("110105194912310021"));
        assert!(!prc_id("440524188001010015"));
    }

    /// Eighteen digits that are not an id: the date inside them is the cheapest thing
    /// that says so, and it rejects most of them before the checksum is reached.
    #[test]
    fn eighteen_digits_with_no_date_in_them_are_not_an_id() {
        assert!(!prc_id("999999999999999999"));
        assert!(!prc_id("110105999913310029"));
        assert!(
            !prc_id("110105194913310029"),
            "there is no thirteenth month"
        );
    }

    #[test]
    fn a_resident_id_is_exactly_eighteen_characters() {
        assert!(!prc_id("11010519491231002"));
        assert!(!prc_id("11010519491231002X9"));
    }

    /// The registry's own published examples.
    #[test]
    fn published_ibans_pass_mod_97() {
        for account in [
            "GB82WEST12345698765432",
            "DE89370400440532013000",
            "FR1420041010050500013M02606",
            "NL91ABNA0417164300",
        ] {
            assert!(iban(account), "{account} is a published example");
        }
    }

    #[test]
    fn spaces_do_not_change_whether_an_iban_passes() {
        assert!(iban("GB82 WEST 1234 5698 7654 32"));
    }

    #[test]
    fn one_character_wrong_fails_mod_97() {
        assert!(!iban("GB82WEST12345698765433"));
        assert!(!iban("DE89370400440532013001"));
    }

    #[test]
    fn a_string_that_is_not_shaped_like_an_iban_is_not_one() {
        assert!(!iban("gb82west12345698765432"), "the country is upper case");
        assert!(!iban("GBXXWEST12345698765432"), "the check is two digits");
        assert!(!iban("GB82"), "too short to be an account");
        assert!(
            !iban("GB82WEST1234569876543!"),
            "that is not an account character"
        );
    }
}
