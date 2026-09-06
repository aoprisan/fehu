//! The background loop that keeps the simulators in step with the wall clock.

use std::sync::Arc;
use std::time::Duration;

use fehu::Timestamp;
use tokio::time::MissedTickBehavior;

use crate::market::{App, StreamMessage};

/// Advance every symbol to `target` and publish the resulting ticks. Returns
/// the number of ticks emitted across all symbols.
pub fn advance_to(app: &App, target: Timestamp) -> u64 {
    let mut total = 0;
    let messages: Vec<StreamMessage> = {
        let mut market = app.market();
        market
            .symbols
            .iter_mut()
            .filter_map(|s| {
                let advanced = s.advance_to(target);
                total += advanced.ticks;
                advanced.last.map(|t| StreamMessage::Tick {
                    symbol: s.info.symbol,
                    ts_ms: t.ts.0,
                    price_cents: t.price_cents,
                    volume: t.volume,
                    closed: advanced.closed_intervals(),
                })
            })
            .collect()
    };
    for m in messages {
        // `Err` only means nobody is listening right now.
        let _ = app.tx.send(m);
    }
    total
}

/// One engine step: advance to the simulated time that corresponds to now.
pub fn step(app: &App) -> u64 {
    advance_to(app, app.clock.now())
}

/// Run [`step`] every `period` until the task is dropped.
pub async fn run(app: Arc<App>, period: Duration) {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let ticks = step(&app);
        if ticks > 0 {
            tracing::trace!(ticks, "engine step");
        }
    }
}
