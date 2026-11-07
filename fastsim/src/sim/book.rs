//! `abides_markets.order_book.OrderBook` + `PriceLevel`, for visible limit orders only (the only
//! kind the scenario agents create), including the Track 3 self-trade-prevention patch.
//!
//! Levels are kept with the BEST price at the END of each vector so that the hot operations
//! (match against the top, add near the top) touch the tail. Outgoing exchange reports are
//! handed to `emit` in exactly the order ABIDES sends them, as they arise (nothing the book does
//! depends on a report having been sent), which preserves the latency-draw and message-id
//! sequences.

use std::collections::VecDeque;

use super::{Order, Side};

#[derive(Clone, Copy, Debug)]
pub struct Resting {
    pub order_id: i64,
    pub agent_id: u32,
    pub quantity: i64,
}

#[derive(Debug)]
pub struct Level {
    pub price: i64,
    /// `PriceLevel.total_quantity`, maintained incrementally.
    pub total: i64,
    pub orders: VecDeque<Resting>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stp {
    CancelNewest,
    CancelOldest,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Report {
    Accepted,
    Executed,
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
pub struct Outgoing {
    pub to: u32,
    pub report: Report,
    pub order: Order,
}

pub struct Book {
    /// Ascending by price: best (highest) bid last.
    bids: Vec<Level>,
    /// Descending by price: best (lowest) ask last.
    asks: Vec<Level>,
    /// Order queues of removed levels, reused by new levels instead of reallocating.
    spare: Vec<VecDeque<Resting>>,
}

impl Book {
    pub fn new() -> Self {
        Book { bids: Vec::new(), asks: Vec::new(), spare: Vec::new() }
    }

    #[inline]
    fn levels(&self, side: Side) -> &Vec<Level> {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    /// `get_l2_bid_data(depth=1)[0]`, i.e. best bid `(price, total)`.
    #[inline]
    pub fn best_bid(&self) -> Option<(i64, i64)> {
        self.bids.last().map(|l| (l.price, l.total))
    }

    #[inline]
    pub fn best_ask(&self) -> Option<(i64, i64)> {
        self.asks.last().map(|l| (l.price, l.total))
    }

    /// Index where a level at `price` is or would be inserted on `side`. Orders and cancels mostly
    /// land within a few levels of the best price, at the vector's end, so the last `PROBE` levels
    /// are scanned from there first; a deeper price is found by a binary search of the rest.
    #[inline]
    fn position(levels: &[Level], side: Side, price: i64) -> usize {
        match side {
            Side::Bid => Self::search(levels, |l| l.price < price),
            Side::Ask => Self::search(levels, |l| l.price > price),
        }
    }

    /// `levels.partition_point(before)`, probing from the end first (see [`Book::position`]).
    #[inline(always)]
    fn search(levels: &[Level], before: impl Fn(&Level) -> bool) -> usize {
        const PROBE: usize = 8;
        let mut j = levels.len();
        let stop = j.saturating_sub(PROBE);
        while j > stop && !before(&levels[j - 1]) {
            j -= 1;
        }
        // Stopped above `stop`: the level below `j` sorts before the price. Otherwise every level
        // from `stop` on sorts at or after it, and the boundary lies at or below `stop`.
        if j > stop || j == 0 { j } else { levels[..j].partition_point(before) }
    }

    /// `PriceLevel.order_is_match` for an order against the opposite level at `level_price`.
    #[inline(always)]
    fn crosses(order: &Order, level_price: i64) -> bool {
        match order.side {
            Side::Bid => order.limit_price >= level_price,
            Side::Ask => order.limit_price <= level_price,
        }
    }

    /// `handle_limit_order`. Returns false when the order is discarded by validation, in which case
    /// ABIDES also skips the BEST_BID / BEST_ASK logging.
    ///
    /// ABIDES also keeps the fills' average price as the book's last trade, for `QUERY_LAST_TRADE`
    /// and the close price. No scenario agent asks for either and no output records them, so it is
    /// not computed.
    pub fn handle_limit_order(&mut self, mut order: Order, stp: Option<Stp>, emit: &mut impl FnMut(Outgoing)) -> bool {
        if order.quantity <= 0 || order.limit_price < 0 {
            return false;
        }
        loop {
            if let Some(stp) = stp {
                let opp_side = order.side.opposite();
                if let Some(best) = self.levels(opp_side).last() {
                    if Self::crosses(&order, best.price) {
                        let resting = *best.orders.front().expect("non-empty level");
                        if resting.agent_id == order.agent_id {
                            let price = best.price;
                            if stp == Stp::CancelOldest
                                && self.cancel(opp_side, price, resting.order_id, resting.agent_id, emit)
                            {
                                continue;
                            }
                            if stp != Stp::CancelOldest {
                                emit(Outgoing { to: order.agent_id, report: Report::Cancelled, order });
                                break;
                            }
                        }
                    }
                }
            }
            match self.execute(&mut order, emit) {
                Some(_) => {
                    if order.quantity <= 0 {
                        break;
                    }
                }
                None => {
                    self.enter(&order);
                    emit(Outgoing { to: order.agent_id, report: Report::Accepted, order });
                    break;
                }
            }
        }
        true
    }

    /// `execute_order`: one match against the best opposite level, or `None`.
    fn execute(&mut self, order: &mut Order, emit: &mut impl FnMut(Outgoing)) -> Option<(i64, i64)> {
        let opp_side = order.side.opposite();
        let (levels, spare) = match opp_side {
            Side::Bid => (&mut self.bids, &mut self.spare),
            Side::Ask => (&mut self.asks, &mut self.spare),
        };
        let best = levels.last_mut()?;
        if !Self::crosses(order, best.price) {
            return None;
        }
        let price = best.price;
        let front = best.orders.front_mut().expect("non-empty level");
        let resting_id = front.order_id;
        let resting_agent = front.agent_id;
        let qty;
        if order.quantity >= front.quantity {
            qty = front.quantity;
            best.orders.pop_front();
            best.total -= qty;
            if best.orders.is_empty() {
                spare.push(levels.pop().expect("best level").orders);
            }
        } else {
            qty = order.quantity;
            front.quantity -= qty;
            best.total -= qty;
        }
        // The resting side's report first, then the aggressor's (ABIDES send order).
        emit(Outgoing {
            to: resting_agent,
            report: Report::Executed,
            order: Order {
                order_id: resting_id,
                agent_id: resting_agent,
                side: opp_side,
                limit_price: price,
                quantity: qty,
                fill_price: price,
            },
        });
        emit(Outgoing {
            to: order.agent_id,
            report: Report::Executed,
            order: Order { quantity: qty, fill_price: price, ..*order },
        });
        order.quantity -= qty;
        Some((qty, price))
    }

    /// `enter_order`: append to the level at the order's price, creating it if needed.
    fn enter(&mut self, order: &Order) {
        let resting = Resting { order_id: order.order_id, agent_id: order.agent_id, quantity: order.quantity };
        let (levels, spare) = match order.side {
            Side::Bid => (&mut self.bids, &mut self.spare),
            Side::Ask => (&mut self.asks, &mut self.spare),
        };
        let idx = Self::position(levels, order.side, order.limit_price);
        if idx < levels.len() && levels[idx].price == order.limit_price {
            let level = &mut levels[idx];
            level.orders.push_back(resting);
            level.total += order.quantity;
        } else {
            let mut orders = spare.pop().unwrap_or_else(|| VecDeque::with_capacity(4));
            orders.push_back(resting);
            levels.insert(idx, Level { price: order.limit_price, total: order.quantity, orders });
        }
    }

    /// `cancel_order`: remove `order_id` from the level at `price`; report to `notify`.
    /// Returns false (and reports nothing) when the order is not resting, e.g. already filled.
    pub fn cancel(&mut self, side: Side, price: i64, order_id: i64, notify: u32, emit: &mut impl FnMut(Outgoing)) -> bool {
        let (levels, spare) = match side {
            Side::Bid => (&mut self.bids, &mut self.spare),
            Side::Ask => (&mut self.asks, &mut self.spare),
        };
        let idx = Self::position(levels, side, price);
        if idx >= levels.len() || levels[idx].price != price {
            return false;
        }
        let level = &mut levels[idx];
        let Some(pos) = level.orders.iter().position(|o| o.order_id == order_id) else {
            return false;
        };
        let removed = level.orders.remove(pos).expect("position is in range");
        level.total -= removed.quantity;
        if level.orders.is_empty() {
            spare.push(levels.remove(idx).orders);
        }
        emit(Outgoing {
            to: notify,
            report: Report::Cancelled,
            order: Order {
                order_id,
                agent_id: removed.agent_id,
                side,
                limit_price: price,
                quantity: removed.quantity,
                fill_price: 0,
            },
        });
        true
    }
}
