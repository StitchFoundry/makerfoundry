//! Money (MF-4), always whole US cents in an `i64`, never a float.
//!
//! The owner's decision (2026-10-19): MakerFoundry keeps 10% of each sale, the card fee (2.9% +
//! 30¢, Stripe's standard US card price, https://stripe.com/pricing) also comes out of the seller's
//! share, and sellers are paid monthly.

/// The lowest price of a paid design: $1.
pub const MIN_PRICE: i64 = 100;
/// The highest: $200.
pub const MAX_PRICE: i64 = 20_000;

/// A sale's price, split three ways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    pub price: i64,
    /// MakerFoundry's 10%.
    pub fee: i64,
    /// The card fee: 2.9% + 30¢.
    pub processing: i64,
    /// What the seller is paid.
    pub seller: i64,
}

impl Split {
    /// What Stripe sends MakerFoundry out of the sale (the rest goes to the seller's account).
    pub fn application_fee(&self) -> i64 {
        self.fee + self.processing
    }
}

/// `price` (cents, `MIN_PRICE..=MAX_PRICE`) split: each part rounded to the nearest cent (halves
/// up), the seller getting what's left, so the parts always add up to the price.
pub fn split(price: i64) -> Option<Split> {
    split_at(price, STANDARD_FEE_PERCENT)
}

/// MakerFoundry's standard fee, in whole percent.
pub const STANDARD_FEE_PERCENT: i64 = 10;
/// The Founding Seller fee for the first 12 months (Bruce approved 5%, issue #132).
pub const FOUNDING_SELLER_FEE_PERCENT: i64 = 5;
/// How many Founding Sellers there are.
pub const FOUNDING_SELLER_LIMIT: i64 = 50;

/// Like `split`, with MakerFoundry's fee at `fee_percent` (0..=100) instead of 10%. The card fee
/// is never discounted. `None` for a price out of range or a percent out of range.
pub fn split_at(price: i64, fee_percent: i64) -> Option<Split> {
    if !(0..=100).contains(&fee_percent) {
        return None;
    }
    if !(MIN_PRICE..=MAX_PRICE).contains(&price) {
        return None;
    }
    let fee = (price * fee_percent + 50) / 100;
    let processing = (price * 29 + 500) / 1000 + 30;
    let seller = price - fee - processing;
    (seller > 0).then_some(Split {
        price,
        fee,
        processing,
        seller,
    })
}

/// Cents as dollars: `$4.50`, `$12`, `-$0.33`.
pub fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let c = cents.unsigned_abs();
    if c.is_multiple_of(100) {
        format!("{sign}${}", c / 100)
    } else {
        format!("{sign}${}.{:02}", c / 100, c % 100)
    }
}

/// Dollars as typed (`4`, `4.5`, `$4.50`, `1,000`) → cents; `None` if it isn't an amount with at
/// most two decimals. Empty is 0.
pub fn parse_dollars(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let t: String = trimmed
        .strip_prefix('$')
        .unwrap_or(trimmed)
        .chars()
        .filter(|c| *c != ',')
        .collect();
    if t.is_empty() {
        return Some(0);
    }
    let (whole, part) = t.split_once('.').unwrap_or((&t, ""));
    if whole.len() > 7 || part.len() > 2 || !(whole.bytes().chain(part.bytes())).all(|b| b.is_ascii_digit()) {
        return None;
    }
    if whole.is_empty() && part.is_empty() {
        return None;
    }
    let whole: i64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let part: i64 = match part.len() {
        0 => 0,
        1 => part.parse::<i64>().ok()? * 10,
        _ => part.parse().ok()?,
    };
    Some(whole * 100 + part)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_founding_seller_pays_the_lower_fee_and_the_card_fee_is_unchanged() {
        let std = split(500).unwrap();
        let f = split_at(500, FOUNDING_SELLER_FEE_PERCENT).unwrap();
        assert_eq!((std.fee, f.fee), (50, 25));
        assert_eq!(std.processing, f.processing);
        assert_eq!(f.seller, 500 - 25 - f.processing);
        assert_eq!(split_at(500, STANDARD_FEE_PERCENT), Some(std));
    }

    #[test]
    fn split_at_always_adds_up_and_rejects_bad_percents() {
        for price in (MIN_PRICE..=MAX_PRICE).step_by(37) {
            for pct in [0, 5, 10, 100] {
                if let Some(s) = split_at(price, pct) {
                    assert_eq!(s.fee + s.processing + s.seller, price, "{price} {pct}");
                    assert!(s.seller > 0);
                }
            }
        }
        assert_eq!(split_at(500, -1), None);
        assert_eq!(split_at(500, 101), None);
        assert_eq!(split_at(99, 5), None);
        assert_eq!(split_at(105, 5).map(|s| s.fee), Some(5));
    }

    #[test]
    fn a_sale_is_split_to_the_cent() {
        assert_eq!(
            split(500),
            Some(Split {
                price: 500,
                fee: 50,
                processing: 45,
                seller: 405
            })
        );
        assert_eq!(
            split(100),
            Some(Split {
                price: 100,
                fee: 10,
                processing: 33,
                seller: 57
            })
        );
        assert_eq!(
            split(20_000).map(|s| (s.fee, s.processing, s.seller)),
            Some((2000, 610, 17_390))
        );
        assert_eq!(split(99), None);
        assert_eq!(split(0), None);
        assert_eq!(split(20_001), None);
    }

    #[test]
    fn every_price_splits_into_parts_that_add_up() {
        for price in MIN_PRICE..=MAX_PRICE {
            let s = split(price).unwrap();
            assert_eq!(s.fee + s.processing + s.seller, price);
            assert!(s.seller > 0 && s.fee >= 0 && s.processing >= 30);
            // within half a cent of the exact shares
            assert!((s.fee * 100 - price * 10).abs() <= 50, "{price}");
            assert!(((s.processing - 30) * 1000 - price * 29).abs() <= 500, "{price}");
            assert_eq!(s.application_fee(), s.fee + s.processing);
        }
    }

    #[test]
    fn dollars_are_written_and_read() {
        assert_eq!(dollars(450), "$4.50");
        assert_eq!(dollars(1200), "$12");
        assert_eq!(dollars(5), "$0.05");
        assert_eq!(dollars(-33), "-$0.33");
        for (text, cents) in [
            ("4", 400),
            ("4.5", 450),
            ("$4.50", 450),
            (" 12.05 ", 1205),
            ("1,000", 100_000),
            (".99", 99),
            ("", 0),
            ("0", 0),
        ] {
            assert_eq!(parse_dollars(text), Some(cents), "{text}");
        }
        for bad in ["4.505", "-4", "four", "4.5.0", ".", "1e3", "99999999", "4 50"] {
            assert_eq!(parse_dollars(bad), None, "{bad}");
        }
    }

    #[test]
    fn money_edges_never_panic_or_go_negative() {
        for price in [i64::MIN, -1, 0, 1, 99, 20_001, i64::MAX] {
            assert_eq!(split(price), None, "{price}");
        }
        assert_eq!(dollars(0), "$0");
        assert_eq!(dollars(i64::MIN), "-$92233720368547758.08");
        assert_eq!(dollars(i64::MAX), "$92233720368547758.07");
        // the largest accepted amount stays inside the price range
        assert_eq!(parse_dollars("9,999,999.99"), Some(999_999_999));
        assert_eq!(parse_dollars("$"), Some(0));
        assert_eq!(parse_dollars("+4"), None);
    }

    #[test]
    fn split_refuses_out_of_range_prices_without_overflowing() {
        for price in [i64::MIN, -1, 0, MIN_PRICE - 1, MAX_PRICE + 1, i64::MAX / 10, i64::MAX] {
            assert_eq!(split(price), None, "{price}");
        }
        assert!(split(MIN_PRICE).is_some() && split(MAX_PRICE).is_some());
    }

    #[test]
    fn split_rounds_halves_up_at_the_boundaries() {
        // 10% of $1.05 is 10.5c -> 11c; of $1.04 is 10.4c -> 10c.
        assert_eq!(split(105).map(|s| s.fee), Some(11));
        assert_eq!(split(104).map(|s| s.fee), Some(10));
        // 2.9% of $1.00 is 2.9c -> 3c, plus the 30c.
        assert_eq!(split(100).map(|s| s.processing), Some(33));
        // 2.9% of 1050c is 30.45c -> 30c; of 1052c is 30.508c -> 31c (plus the 30c).
        assert_eq!(split(1050).map(|s| s.processing), Some(60));
        assert_eq!(split(1052).map(|s| s.processing), Some(61));
    }

    #[test]
    fn dollars_never_panics_at_the_extremes() {
        assert_eq!(dollars(0), "$0");
        assert_eq!(dollars(i64::MAX), "$92233720368547758.07");
        assert_eq!(dollars(i64::MIN), "-$92233720368547758.08");
    }

    #[test]
    fn dollars_text_reads_back_as_the_same_cents() {
        for cents in (0..=MAX_PRICE).chain([99_999_999, 100_000_000]) {
            let text = dollars(cents);
            assert_eq!(parse_dollars(&text), Some(cents), "{text}");
        }
    }

    #[test]
    fn parse_dollars_caps_the_size_and_rejects_junk() {
        assert_eq!(parse_dollars("9999999.99"), Some(999_999_999));
        assert_eq!(parse_dollars("10000000"), None);
        for bad in [
            "+4", "4.", "١٢", "4,5.0.0", "NaN", "inf", "\u{0}", "0x10", "$ 4", "$$4", "$$",
        ] {
            // "4." has an empty fractional part and is accepted as 4; everything else is junk.
            if bad == "4." {
                assert_eq!(parse_dollars(bad), Some(400));
            } else {
                assert_eq!(parse_dollars(bad), None, "{bad}");
            }
        }
    }
}
