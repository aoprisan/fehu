//! `fehu-economy`: run the economy server.
//!
//! ```text
//! cargo run --release -p fehu-economy
//! ```
//!
//! Environment: `FEHU_BIND` (default `0.0.0.0:3000`), `FEHU_TIME_SCALE`,
//! `FEHU_HISTORY_DAYS`, `FEHU_WARMUP_HOURS`, `FEHU_MAX_BARS`, `FEHU_EVENT_LOG`,
//! `FEHU_TAPE`, `FEHU_FILL_LOG`, `FEHU_ORDER_LOG`, `FEHU_LEDGER_LOG`,
//! `FEHU_STARTING_CASH_CENTS`, `FEHU_ADMIN_KEY`, `FEHU_STATE_FILE`,
//! `FEHU_SAVE_SECS`, `FEHU_COMMAND_LOG`, `FEHU_JOB_LOG`, `FEHU_REWARD_LOG`,
//! `FEHU_SEED_MERCHANTS_CENTS`, `FEHU_MAX_SYMBOLS`, `RUST_LOG`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fehu_economy::{App, Options, engine, journal, router, save};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();

    let options = Options::from_env();
    let t0 = Instant::now();
    // A state file that exists is the market: it is read back instead of
    // warming a new one up. A file that cannot be read stops the server
    // rather than quietly starting a market without its accounts.
    let saved = match options.state_file.as_deref() {
        Some(path) if path.exists() => {
            tracing::info!(path = %path.display(), "restoring saved market");
            Some(save::read(path)?)
        }
        _ => None,
    };
    // The journal holds everything acknowledged since that snapshot was
    // written. A file that cannot be read stops the server for the same
    // reason an unreadable snapshot does: carrying on would serve a market
    // that is missing changes it promised to keep.
    let journal_path = options.state_file.as_deref().map(journal::path_for);
    let entries = match (&saved, journal_path.as_deref()) {
        (Some(_), Some(path)) if path.exists() => {
            let entries = journal::read(path)?;
            tracing::info!(path = %path.display(), entries = entries.len(), "replaying journal");
            entries
        }
        // No snapshot to replay onto: a journal beside a market that is
        // being warmed up fresh describes a different world, and applying it
        // to this one would be worse than losing it.
        (None, Some(path)) if path.exists() => {
            tracing::warn!(path = %path.display(), "no state file to replay onto; the journal is discarded");
            std::fs::remove_file(path)?;
            Vec::new()
        }
        _ => Vec::new(),
    };
    let fresh = saved.is_none();
    let seed_merchant_cents = options.seed_merchant_cents;
    let app = match saved {
        Some(save) => App::resume(options, save, entries).await,
        None => {
            tracing::info!(?options, "warming up");
            App::new(options)
        }
    };
    // Only now, with the replay done: a journal attached before it would
    // write every replayed command down a second time.
    if let Some(path) = journal_path.as_deref() {
        app.attach_journal(path).await?;
        tracing::info!(path = %path.display(), "journaling commands");
    }
    // A world that was warmed up rather than restored has no merchants, and
    // with `FEHU_SYNTHETIC=0` no liquidity at all. Seeding runs after the
    // journal is attached, so the merchants it makes are written down like
    // any other command.
    if fresh && seed_merchant_cents > 0 {
        let made = app.seed_merchants(seed_merchant_cents).await;
        tracing::info!(
            merchants = made,
            cents = seed_merchant_cents,
            "merchants seeded"
        );
    }
    for symbol in app.listings().all() {
        let Ok((q, daily_bars, minute_bars, ticks)) = symbol
            .ask(|s| {
                (
                    s.quote(),
                    s.bars(fehu::Interval::D1, usize::MAX).len(),
                    s.bars(fehu::Interval::M1, usize::MAX).len(),
                    s.ticks_total,
                )
            })
            .await
        else {
            continue;
        };
        tracing::info!(
            symbol = q.symbol,
            price = format!("{:.2}", q.price_cents as f64 / 100.0),
            daily_bars,
            minute_bars,
            ticks,
            bid = q.bid_cents.map(|c| format!("{:.2}", c as f64 / 100.0)),
            ask = q.ask_cents.map(|c| format!("{:.2}", c as f64 / 100.0)),
            "ready"
        );
    }
    tracing::info!(elapsed_ms = t0.elapsed().as_millis(), "warm-up done");

    tokio::spawn(engine::run(Arc::clone(&app), Duration::from_millis(250)));
    if let Some(path) = app.options.state_file.clone() {
        let secs = app.options.save_secs;
        tracing::info!(path = %path.display(), save_secs = secs, "saving state");
        tokio::spawn(save::autosave(
            Arc::clone(&app),
            path,
            Duration::from_secs(secs),
        ));
    }

    let bind = std::env::var("FEHU_BIND").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "listening; open http://localhost:{} in a browser", listener.local_addr()?.port());
    axum::serve(listener, router(Arc::clone(&app)))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // One last save, so a clean shutdown loses nothing at all.
    if let Some(path) = app.options.state_file.as_deref() {
        match save::write(&app, path).await {
            Ok(()) => tracing::info!(path = %path.display(), "state saved"),
            Err(e) => tracing::error!(path = %path.display(), error = %e, "state not saved"),
        }
    }
    tracing::info!("bye");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
