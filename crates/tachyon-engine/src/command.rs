use tachyon_core::*;

/// Commands that can be sent to the matching engine.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Command {
    PlaceOrder(Order),
    CancelOrder(OrderId),
    ModifyOrder {
        order_id: OrderId,
        new_price: Option<Price>,
        new_qty: Option<Quantity>,
    },
}

/// An inbound command with metadata before sequencing.
#[derive(Clone, Debug)]
pub struct InboundCommand {
    pub symbol: Symbol,
    pub account_id: u64,
    pub command: Command,
    pub timestamp: u64,
}

/// A command that has been assigned a global sequence number.
#[derive(Clone, Debug)]
pub struct SequencedCommand {
    pub sequence: u64,
    pub command: InboundCommand,
}
