/// Self-Trade Prevention mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StpMode {
    /// No self-trade prevention.
    None,
    /// Cancel the incoming (taker) order.
    CancelNewest,
    /// Cancel the resting (maker) order.
    CancelOldest,
    /// Cancel both orders.
    CancelBoth,
}

/// Action to take after an STP check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StpAction {
    /// No self-trade: proceed with matching.
    Proceed,
    /// Cancel the incoming order.
    CancelIncoming,
    /// Cancel the resting order, continue matching incoming.
    CancelResting,
    /// Cancel both orders.
    CancelBoth,
}

/// Checks whether a self-trade would occur and returns the action to take.
#[inline]
pub fn check_self_trade(incoming_account: u64, resting_account: u64, mode: StpMode) -> StpAction {
    if incoming_account != resting_account || mode == StpMode::None {
        return StpAction::Proceed;
    }
    match mode {
        StpMode::None => StpAction::Proceed,
        StpMode::CancelNewest => StpAction::CancelIncoming,
        StpMode::CancelOldest => StpAction::CancelResting,
        StpMode::CancelBoth => StpAction::CancelBoth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stp_different_accounts() {
        assert_eq!(
            check_self_trade(1, 2, StpMode::CancelNewest),
            StpAction::Proceed
        );
        assert_eq!(
            check_self_trade(1, 2, StpMode::CancelOldest),
            StpAction::Proceed
        );
        assert_eq!(
            check_self_trade(1, 2, StpMode::CancelBoth),
            StpAction::Proceed
        );
    }

    #[test]
    fn test_stp_same_account_none() {
        assert_eq!(check_self_trade(1, 1, StpMode::None), StpAction::Proceed);
    }

    #[test]
    fn test_stp_same_account_cancel_newest() {
        assert_eq!(
            check_self_trade(1, 1, StpMode::CancelNewest),
            StpAction::CancelIncoming
        );
    }

    #[test]
    fn test_stp_same_account_cancel_oldest() {
        assert_eq!(
            check_self_trade(1, 1, StpMode::CancelOldest),
            StpAction::CancelResting
        );
    }

    #[test]
    fn test_stp_same_account_cancel_both() {
        assert_eq!(
            check_self_trade(1, 1, StpMode::CancelBoth),
            StpAction::CancelBoth
        );
    }
}
