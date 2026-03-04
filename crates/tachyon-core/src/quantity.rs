use std::fmt;

/// Fixed-point quantity representation using an unsigned 64-bit integer.
///
/// Quantities are always non-negative, hence `u64`. The scale (number of
/// decimal places) comes from `SymbolConfig::qty_scale`.
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
pub struct Quantity(pub u64);

impl Quantity {
    /// The zero quantity.
    pub const ZERO: Quantity = Quantity(0);

    /// Creates a new Quantity from a raw u64 value.
    #[inline]
    pub const fn new(raw: u64) -> Self {
        Quantity(raw)
    }

    /// Returns the raw inner value.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns true if this quantity is zero.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Checked addition. Returns `None` on overflow.
    #[inline]
    pub const fn checked_add(self, rhs: Quantity) -> Option<Quantity> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Quantity(v)),
            None => None,
        }
    }

    /// Checked subtraction. Returns `None` if `rhs > self`.
    #[inline]
    pub const fn checked_sub(self, rhs: Quantity) -> Option<Quantity> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Quantity(v)),
            None => None,
        }
    }

    /// Returns the minimum of two quantities.
    #[inline]
    pub const fn min(self, other: Quantity) -> Quantity {
        if self.0 <= other.0 {
            self
        } else {
            other
        }
    }

    /// Saturating subtraction. Result never goes below zero.
    #[inline]
    pub const fn saturating_sub(self, rhs: Quantity) -> Quantity {
        Quantity(self.0.saturating_sub(rhs.0))
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Quantity({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quantity_new_and_raw() {
        let q = Quantity::new(999);
        assert_eq!(q.raw(), 999);
    }

    #[test]
    fn test_quantity_is_zero() {
        assert!(Quantity::ZERO.is_zero());
        assert!(Quantity::new(0).is_zero());
        assert!(!Quantity::new(1).is_zero());
    }

    #[test]
    fn test_quantity_checked_add() {
        assert_eq!(
            Quantity::new(100).checked_add(Quantity::new(200)),
            Some(Quantity::new(300))
        );
        // Overflow
        assert_eq!(Quantity::new(u64::MAX).checked_add(Quantity::new(1)), None);
    }

    #[test]
    fn test_quantity_checked_sub() {
        assert_eq!(
            Quantity::new(300).checked_sub(Quantity::new(200)),
            Some(Quantity::new(100))
        );
        assert_eq!(
            Quantity::new(100).checked_sub(Quantity::new(100)),
            Some(Quantity::ZERO)
        );
        // Underflow
        assert_eq!(Quantity::new(0).checked_sub(Quantity::new(1)), None);
        assert_eq!(Quantity::new(5).checked_sub(Quantity::new(10)), None);
    }

    #[test]
    fn test_quantity_min() {
        assert_eq!(Quantity::new(5).min(Quantity::new(10)), Quantity::new(5));
        assert_eq!(Quantity::new(10).min(Quantity::new(5)), Quantity::new(5));
        assert_eq!(Quantity::new(5).min(Quantity::new(5)), Quantity::new(5));
    }

    #[test]
    fn test_quantity_saturating_sub() {
        assert_eq!(
            Quantity::new(10).saturating_sub(Quantity::new(3)),
            Quantity::new(7)
        );
        assert_eq!(
            Quantity::new(3).saturating_sub(Quantity::new(10)),
            Quantity::ZERO
        );
    }

    #[test]
    fn test_quantity_display() {
        assert_eq!(format!("{}", Quantity::new(42)), "Quantity(42)");
    }

    #[test]
    fn test_quantity_ordering() {
        assert!(Quantity::new(100) > Quantity::new(50));
        assert!(Quantity::new(0) < Quantity::new(1));
        assert_eq!(Quantity::new(42), Quantity::new(42));
    }

    #[test]
    fn test_quantity_default() {
        assert_eq!(Quantity::default(), Quantity::ZERO);
    }
}
