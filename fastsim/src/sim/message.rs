//! A message has one kind tag, in its payload's spare bytes. The kind determines how the four
//! words are read: order id/price/quantity/fill price, bid/ask, or market open/close. This avoids
//! first dispatching on a payload enum and then on its nested message kind. All fields are
//! initialized, and all order and price fields retain their full widths.

use super::trace::MsgKind;
use super::{Order, Side};

#[derive(Clone, Copy, Debug)]
pub(super) struct Payload {
    words: [i64; 4],
    /// The order's agent, or bid/ask presence and market-closed flags for a spread reply.
    agent: u32,
    side: Side,
    kind: MsgKind,
}

impl Payload {
    #[inline]
    pub fn signal(kind: MsgKind) -> Self {
        Self { words: [0; 4], agent: 0, side: Side::Bid, kind }
    }

    #[inline]
    pub fn hours(open: i64, close: i64) -> Self {
        Self { words: [open, close, 0, 0], ..Self::signal(MsgKind::MarketHours) }
    }

    #[inline]
    pub fn spread(bid: Option<i64>, ask: Option<i64>, closed: bool) -> Self {
        let flags = (bid.is_some() as u32) | ((ask.is_some() as u32) << 1) | ((closed as u32) << 2);
        Self { words: [bid.unwrap_or(0), ask.unwrap_or(0), 0, 0], agent: flags,
               ..Self::signal(MsgKind::QuerySpreadResponse) }
    }

    #[inline]
    pub fn order(kind: MsgKind, order: Order) -> Self {
        debug_assert!(matches!(kind, MsgKind::LimitOrder | MsgKind::CancelOrder | MsgKind::OrderAccepted
                                  | MsgKind::OrderExecuted | MsgKind::OrderCancelled));
        Self { words: [order.order_id, order.limit_price, order.quantity, order.fill_price],
               agent: order.agent_id, side: order.side, kind }
    }

    #[inline]
    pub fn kind(self) -> MsgKind { self.kind }

    #[inline]
    fn is_order(self) -> bool {
        matches!(self.kind, MsgKind::LimitOrder | MsgKind::CancelOrder | MsgKind::OrderAccepted
                          | MsgKind::OrderExecuted | MsgKind::OrderCancelled)
    }

    #[inline]
    pub fn order_id(self) -> Option<i64> { self.is_order().then_some(self.words[0]) }

    #[inline]
    pub fn order_value(self) -> Order {
        // Callers dispatch on kind() first. No unchecked memory access or narrowing is needed.
        debug_assert!(self.is_order());
        Order { order_id: self.words[0], limit_price: self.words[1], quantity: self.words[2],
                fill_price: self.words[3], agent_id: self.agent, side: self.side }
    }

    #[cfg(test)]
    pub fn as_order(self) -> Option<Order> { self.is_order().then(|| self.order_value()) }

    #[inline]
    pub fn hours_value(self) -> (i64, i64) {
        debug_assert_eq!(self.kind, MsgKind::MarketHours);
        (self.words[0], self.words[1])
    }

    #[inline]
    pub fn spread_value(self) -> (Option<i64>, Option<i64>, bool) {
        debug_assert_eq!(self.kind, MsgKind::QuerySpreadResponse);
        ((self.agent & 1 != 0).then_some(self.words[0]),
         (self.agent & 2 != 0).then_some(self.words[1]), self.agent & 4 != 0)
    }
}

/// One cache line each: a 64-byte message at the allocator's 16-byte alignment would straddle
/// two lines in three slots out of four, and each delivery reads one and each send writes one.
#[derive(Clone, Copy, Debug)]
#[repr(align(64))]
pub(super) struct Msg {
    pub payload: Payload,
    /// Ledger fields fixed at send time (`NULL` t_send and `NONE` causal parent for wakeups).
    pub t_send: i64,
    pub t_recv: i64,
    pub causal_parent: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_message_uses_64_bytes_without_narrowing_order_fields() {
        assert_eq!(size_of::<Payload>(), 40);
        assert_eq!(size_of::<Msg>(), 64);
        for kind in [
            MsgKind::LimitOrder,
            MsgKind::CancelOrder,
            MsgKind::OrderAccepted,
            MsgKind::OrderExecuted,
            MsgKind::OrderCancelled,
        ] {
            for side in [Side::Bid, Side::Ask] {
                for value in [i64::MIN, -1, 0, 1, i64::MAX] {
                    let order = Order {
                        order_id: value,
                        agent_id: u32::MAX,
                        side,
                        limit_price: value,
                        quantity: value,
                        fill_price: value,
                    };
                    let payload = Payload::order(kind, order);
                    assert_eq!(payload.kind(), kind);
                    let got = payload.as_order().unwrap();
                    assert_eq!(got.order_id, order.order_id);
                    assert_eq!(got.agent_id, order.agent_id);
                    assert_eq!(got.side, order.side);
                    assert_eq!(got.limit_price, order.limit_price);
                    assert_eq!(got.quantity, order.quantity);
                    assert_eq!(got.fill_price, order.fill_price);
                }
            }
        }
    }

    #[test]
    fn spread_presence_and_hours_preserve_full_width_values() {
        let prices = [None, Some(i64::MIN), Some(-1), Some(0), Some(1), Some(i64::MAX)];
        for bid in prices {
            for ask in prices {
                for closed in [false, true] {
                    let p = Payload::spread(bid, ask, closed);
                    assert_eq!(p.kind(), MsgKind::QuerySpreadResponse);
                    assert_eq!(p.spread_value(), (bid, ask, closed));
                    assert_eq!(p.order_id(), None);
                    assert!(p.as_order().is_none());
                }
            }
        }
        for open in [i64::MIN, 0, i64::MAX] {
            for close in [i64::MIN, 0, i64::MAX] {
                let p = Payload::hours(open, close);
                assert_eq!(p.kind(), MsgKind::MarketHours);
                assert_eq!(p.hours_value(), (open, close));
                assert_eq!(p.order_id(), None);
            }
        }
        for kind in [MsgKind::MarketHoursRequest, MsgKind::MarketClosePriceRequest,
                     MsgKind::MarketClosePrice, MsgKind::MarketClosed, MsgKind::QuerySpread] {
            let p = Payload::signal(kind);
            assert_eq!(p.kind(), kind);
            assert_eq!(p.order_id(), None);
        }
    }
}
