use std::fmt;

/// Fixed-point price representation using a signed 64-bit integer.
///
/// The scale (number of decimal places) is NOT stored in the Price type itself;
/// it comes from `SymbolConfig::price_scale`. For example, with `price_scale = 2`,
/// the value 50001.50 USDT is represented as `Price(5000150)`.
///
/// Uses `i64` rather than `u64` because price differences (spreads) can be negative.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[repr(transparent)]
pub struct Price(pub i64);

impl Price {
    /// The zero price.
    pub const ZERO: Price = Price(0);

    /// Creates a new Price from a raw i64 value.
    #[inline]
    pub const fn new(raw: i64) -> Self {
        Price(raw)
    }

    /// Returns the raw inner value.
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }

    /// Returns true if this price is zero.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Checked addition. Returns `None` on overflow.
    #[inline]
    pub const fn checked_add(self, rhs: Price) -> Option<Price> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Price(v)),
            None => None,
        }
    }

    /// Checked subtraction. Returns `None` on overflow.
    #[inline]
    pub const fn checked_sub(self, rhs: Price) -> Option<Price> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Price(v)),
            None => None,
        }
    }

    /// Checked multiplication of Price by Quantity.
    ///
    /// Uses i128 intermediate value to avoid overflow.
    /// Divides by `scale_factor` (e.g., 10^8 for price_scale=8) to normalize the result.
    ///
    /// Returns `None` if the result cannot fit in an i64.
    #[inline]
    pub const fn checked_mul_qty(self, qty: crate::Quantity, scale_factor: u64) -> Option<Price> {
        if scale_factor == 0 {
            return None;
        }
        let intermediate = (self.0 as i128) * (qty.raw() as i128);
        let result = intermediate / (scale_factor as i128);
        if result > i64::MAX as i128 || result < i64::MIN as i128 {
            None
        } else {
            Some(Price(result as i64))
        }
    }

    /// Formats the price as a human-readable decimal string with the given scale.
    ///
    /// For example, `Price(5000150).format_with_scale(2)` returns `"50001.50"`.
    pub fn format_with_scale(self, scale: u8) -> String {
        if scale == 0 {
            return self.0.to_string();
        }
        let divisor = 10i64.pow(scale as u32);
        let abs_val = self.0.unsigned_abs();
        let whole = abs_val / divisor as u64;
        let frac = abs_val % divisor as u64;
        let sign = if self.0 < 0 { "-" } else { "" };
        format!("{sign}{whole}.{frac:0>width$}", width = scale as usize)
    }

    /// Checks if the price is aligned to the given tick size.
    #[inline]
    pub const fn is_aligned(self, tick_size: Price) -> bool {
        if tick_size.0 == 0 {
            return false;
        }
        self.0 % tick_size.0 == 0
    }

    /// Aligns the price down to the nearest tick size.
    #[inline]
    pub const fn align_down(self, tick_size: Price) -> Price {
        if tick_size.0 == 0 {
            return self;
        }
        Price(self.0 - self.0 % tick_size.0)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Price({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Quantity;

    #[test]
    fn test_price_new_and_raw() {
        let p = Price::new(12345);
        assert_eq!(p.raw(), 12345);
    }

    #[test]
    fn test_price_is_zero() {
        assert!(Price::ZERO.is_zero());
        assert!(Price::new(0).is_zero());
        assert!(!Price::new(1).is_zero());
        assert!(!Price::new(-1).is_zero());
    }

    #[test]
    fn test_price_checked_add() {
        assert_eq!(
            Price::new(100).checked_add(Price::new(200)),
            Some(Price::new(300))
        );
        assert_eq!(
            Price::new(-50).checked_add(Price::new(50)),
            Some(Price::ZERO)
        );
        // Overflow
        assert_eq!(Price::new(i64::MAX).checked_add(Price::new(1)), None);
    }

    #[test]
    fn test_price_checked_sub() {
        assert_eq!(
            Price::new(300).checked_sub(Price::new(200)),
            Some(Price::new(100))
        );
        assert_eq!(
            Price::new(100).checked_sub(Price::new(100)),
            Some(Price::ZERO)
        );
        // Underflow
        assert_eq!(Price::new(i64::MIN).checked_sub(Price::new(1)), None);
    }

    #[test]
    fn test_price_checked_mul_qty() {
        // price=50000.00 (5000000 at scale=2), qty=1.50 (150 at scale=2), scale=100
        // result = (5000000 * 150) / 100 = 7500000 = 75000.00
        let price = Price::new(5_000_000);
        let qty = Quantity::new(150);
        assert_eq!(price.checked_mul_qty(qty, 100), Some(Price::new(7_500_000)));
    }

    #[test]
    fn test_price_checked_mul_qty_large_values() {
        // Test with values that would overflow i64 if multiplied directly
        let price = Price::new(5_000_000_000_000); // 50000.00000000 at scale=8
        let qty = Quantity::new(150_000_000); // 1.50000000 at scale=8
        let scale = 100_000_000u64; // 10^8
        let result = price.checked_mul_qty(qty, scale);
        assert_eq!(result, Some(Price::new(7_500_000_000_000))); // 75000.00000000
    }

    #[test]
    fn test_price_checked_mul_qty_overflow() {
        // Result too large for i64
        let price = Price::new(i64::MAX);
        let qty = Quantity::new(2);
        assert_eq!(price.checked_mul_qty(qty, 1), None);
    }

    #[test]
    fn test_price_checked_mul_qty_zero_scale() {
        let price = Price::new(100);
        let qty = Quantity::new(10);
        assert_eq!(price.checked_mul_qty(qty, 0), None);
    }

    #[test]
    fn test_price_format_with_scale() {
        assert_eq!(Price::new(5000150).format_with_scale(2), "50001.50");
        assert_eq!(Price::new(100).format_with_scale(0), "100");
        assert_eq!(Price::new(1).format_with_scale(4), "0.0001");
        assert_eq!(Price::new(-5000150).format_with_scale(2), "-50001.50");
        assert_eq!(Price::new(0).format_with_scale(2), "0.00");
    }

    #[test]
    fn test_price_display() {
        assert_eq!(format!("{}", Price::new(42)), "Price(42)");
    }

    #[test]
    fn test_price_ordering() {
        assert!(Price::new(100) > Price::new(50));
        assert!(Price::new(-1) < Price::new(0));
        assert_eq!(Price::new(42), Price::new(42));
    }

    #[test]
    fn test_price_default() {
        assert_eq!(Price::default(), Price::ZERO);
    }

    #[test]
    fn test_price_is_aligned() {
        assert!(Price::new(100).is_aligned(Price::new(50)));
        assert!(Price::new(100).is_aligned(Price::new(10)));
        assert!(!Price::new(105).is_aligned(Price::new(10)));
    }

    #[test]
    fn test_price_align_down() {
        assert_eq!(Price::new(105).align_down(Price::new(10)), Price::new(100));
        assert_eq!(Price::new(100).align_down(Price::new(10)), Price::new(100));
    }
}
