//! `fehu-webapp`: run the sample market backend.
//!
//! ```text
//! cargo run --release -p fehu-webapp
//! ```
//!
//! Environment: `FEHU_BIND` (default `0.0.0.0:3000`), `FEHU_TIME_SCALE`,
//! `FEHU_HISTORY_DAYS`, `FEHU_WARMUP_HOURS`, `FEHU_MAX_BARS`, `FEHU_EVENT_LOG`,
//! `FEHU_TAPE`, `FEHU_FILL_LOG`, `FEHU_STARTING_CASH_CENTS`, `RUST_LOG`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fehu_webapp::{App, Options, engine, router};
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
    tracing::info!(?options, "warming up");
    let t0 = Instant::now();
    let app = App::new(options);
    {
        let market = app.market();
        for s in &market.symbols {
            let q = s.quote();
            tracing::info!(
                symbol = s.info.symbol,
                price = format!("{:.2}", q.price_cents as f64 / 100.0),
                daily_bars = s.bars(fehu::Interval::D1, usize::MAX).len(),
                minute_bars = s.bars(fehu::Interval::M1, usize::MAX).len(),
                ticks = s.ticks_total,
                bid = q.bid_cents.map(|c| format!("{:.2}", c as f64 / 100.0)),
                ask = q.ask_cents.map(|c| format!("{:.2}", c as f64 / 100.0)),
                "ready"
            );
        }
    }
    tracing::info!(elapsed_ms = t0.elapsed().as_millis(), "warm-up done");

    tokio::spawn(engine::run(Arc::clone(&app), Duration::from_millis(250)));

    let bind = std::env::var("FEHU_BIND").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "listening; open http://localhost:{} in a browser", listener.local_addr()?.port());
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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
