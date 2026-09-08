//! The background loop that keeps the simulators in step with the wall clock.
//!
//! A step is one job on the market actor ([`crate::market::Market::step`]):
//! it fans the advance out to every symbol actor, which run at once, then
//! settles what each did — books the fills, sweeps the expired orders,
//! places the stops that fired — and publishes it. Nothing else touches a
//! book while a step is in flight, and nothing waits on a lock.
//!
//! The step goes through the journal like every other change, as
//! [`Command::Step`] carrying the simulated instant it is advancing to. That
//! is the only reading of the clock in the whole apply path: on a restart
//! the steps are replayed from the journal instead, so an order that filled
//! against the price at 12:04:31 fills against it again, and replay never
//! reads a clock of its own. See [`crate::journal`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use fehu::Timestamp;
use tokio::time::MissedTickBehavior;

use crate::journal::{Command, Principal};
use crate::market::{App, wall_now_ms};

/// Advance every symbol to `target`, book the traders' fills, and publish
/// the resulting ticks and fills. Returns the number of ticks emitted across
/// all symbols. When this returns, everything the step did has been booked
/// and published.
pub async fn advance_to(app: &App, target: Timestamp) -> u64 {
    let started = Instant::now();
    let total = app
        .market
        .call_async(move |m| {
            Box::pin(async move {
                m.run_command(
                    Principal::Engine,
                    None,
                    target,
                    wall_now_ms(),
                    Command::Step,
                )
                .await
            })
        })
        .await
        .ok()
        .and_then(|out| match out {
            Ok(out) => out.body.get("ticks").and_then(serde_json::Value::as_u64),
            Err(e) => {
                // The one place a step can be refused: the journal stopped
                // taking entries, so the market must stop changing. Said
                // once a second at most, and loudly, because the server is
                // now serving reads and nothing else.
                tracing::error!(error = %e.message(), "engine step refused");
                None
            }
        })
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
