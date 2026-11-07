use fastsim::sim::trace::{TraceBuilder, TraceType};
use fastsim::sim::{Order, Side};

fn order(id: i64, quantity: i64) -> Order {
    Order { order_id: id, agent_id: 1, side: Side::Bid, limit_price: 105, quantity, fill_price: 99 }
}

#[test]
fn delayed_quotes_keep_last_values_and_first_side_order_before_later_orders() {
    let mut trace = TraceBuilder::new(0);
    trace.order_event(100, TraceType::Submitted, &order(7, 12));
    trace.quote(100, Side::Ask, 110, 10);
    trace.quote(100, Side::Bid, 90, 20);
    trace.order_event(100, TraceType::Executed, &order(3, 2));
    trace.order_event(100, TraceType::Accepted, &order(7, 12));
    trace.order_event(100, TraceType::Executed, &order(3, 1));
    trace.quote(100, Side::Ask, 111, 9);
    // No new quote triggers a flush before these later lifecycle events.
    trace.order_event(101, TraceType::Cancelled, &order(7, 12));
    trace.order_event(102, TraceType::Submitted, &order(4, 6));
    trace.quote(103, Side::Bid, 92, 8);
    trace.finish();

    let got: Vec<_> = trace.rows.iter().map(|r| (r.t_ns, r.order_id, r.kind, r.side, r.price, r.size)).collect();
    assert_eq!(got, vec![
        (100, -1, TraceType::Quote, Side::Ask, 111, 9),
        (100, -1, TraceType::Quote, Side::Bid, 90, 20),
        (100, 3, TraceType::PartialFill, Side::Bid, 99, 2),
        (100, 3, TraceType::Filled, Side::Bid, 99, 1),
        (100, 7, TraceType::Submitted, Side::Bid, 105, 12),
        (100, 7, TraceType::Accepted, Side::Bid, 105, 12),
        (101, 7, TraceType::Cancelled, Side::Bid, 105, 12),
        (102, 4, TraceType::Submitted, Side::Bid, 105, 6),
        (103, -1, TraceType::Quote, Side::Bid, 92, 8),
    ]);
}

#[test]
fn final_execution_is_labelled_across_timestamps_even_if_quantity_remains() {
    let mut trace = TraceBuilder::new(0);
    trace.order_event(100, TraceType::Submitted, &order(0, 10));
    trace.order_event(100, TraceType::Executed, &order(0, 1));
    trace.quote(100, Side::Bid, 98, 2);
    trace.order_event(200, TraceType::Executed, &order(0, 2));
    trace.order_event(200, TraceType::Cancelled, &order(0, 7));
    trace.finish();
    let got: Vec<_> = trace.rows.iter().map(|r| r.kind).collect();
    assert_eq!(got, vec![TraceType::Quote, TraceType::Submitted, TraceType::PartialFill,
                        TraceType::Filled, TraceType::Cancelled]);
}
