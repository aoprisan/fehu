//! The background loop that keeps the simulators in step with the wall clock.
//!
//! A step is one job on the market actor ([`crate::market::Market::step`]):
//! it fans the advance out to every symbol actor, which run at once, then
//! settles what each did — books the fills, sweeps the expired orders,
//! places the stops that fired — and publishes it. Nothing else touches a
//! book while a step is in flight, and nothing waits on a lock.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fehu::Timestamp;
use tokio::time::MissedTickBehavior;

use crate::market::App;

/// Advance every symbol to `target`, book the traders' fills, and publish
/// the resulting ticks and fills. Returns the number of ticks emitted across
/// all symbols. When this returns, everything the step did has been booked
/// and published.
pub async fn advance_to(app: &App, target: Timestamp) -> u64 {
    let started = Instant::now();
    let total = app
        .market
        .call_async(move |m| Box::pin(m.step(target)))
        .await
        .unwrap_or(0);
    // Timed around the whole job, not just the simulators: what matters is
    // how long the market spends on the step rather than on orders.
    app.metrics.engine_step(started.elapsed());
    total
}

/// One engine step: advance to the simulated time that corresponds to now.
pub async fn step(app: &App) -> u64 {
    advance_to(app, app.clock.now()).await
}

/// Run [`step`] every `period` until the task is dropped.
pub async fn run(app: Arc<App>, period: Duration) {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let ticks = step(&app).await;
        if ticks > 0 {
            tracing::trace!(ticks, "engine step");
        }
    }
}
