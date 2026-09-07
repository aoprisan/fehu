//! Event ingestion: raw simulator events, the game-event catalogue that maps
//! semantic game happenings onto them, and the audit log record.

use std::fmt;
use std::time::Duration;

use fehu::{Event, EventError, EventKind, Simulator, Timestamp};
use serde::{Deserialize, Serialize};

use crate::save::Symbol;

/// A raw simulator event as accepted by `POST /api/symbols/{symbol}/events`.
/// Mirrors [`fehu::EventKind`] with durations in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SimEvent {
    /// Immediate multiplicative move: `0.10` is +10 %. Must be greater than −1.
    Jump { pct: f64 },
    /// Annualised drift added to the price, halving every `half_life_secs`.
    DriftShift { delta: f64, half_life_secs: f64 },
    /// A drift shift sized so its integrated effect is a log move of `total`.
    DriftForTotalMove { total: f64, half_life_secs: f64 },
    /// Annualised volatility added (or removed), halving every `half_life_secs`.
    VolShift { delta: f64, half_life_secs: f64 },
    /// Permanently multiplies the fundamental value by `e^delta`.
    FundamentalShift { delta: f64 },
    /// Sets the fundamental's target price outright. Applied immediately,
    /// regardless of scheduling.
    FundamentalTarget { target_cents: i64 },
}

/// Why an event was rejected before reaching the simulator.
#[derive(Clone, Debug, PartialEq)]
pub enum EventRejected {
    NotFinite(&'static str),
    HalfLife,
    Target,
    Sim(EventError),
}

impl fmt::Display for EventRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFinite(field) => write!(f, "`{field}` must be a finite number"),
            Self::HalfLife => f.write_str("`half_life_secs` must be a positive number of seconds"),
            Self::Target => f.write_str("`target_cents` must be at least 1"),
            Self::Sim(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EventRejected {}

/// A validated event ready to be applied.
#[derive(Clone, Copy, Debug)]
pub enum Prepared {
    Queue(EventKind),
    SetFundamental(i64),
}

impl Prepared {
    /// Apply to `sim`; queued kinds take effect at the first tick at or after `at`.
    pub fn apply(self, sim: &mut Simulator, at: Timestamp) -> Result<(), EventRejected> {
        match self {
            Self::Queue(kind) => sim
                .push_event(Event { at, kind })
                .map_err(EventRejected::Sim),
            Self::SetFundamental(cents) => {
                sim.set_fundamental_input(cents);
                Ok(())
            }
        }
    }
}

fn half_life(secs: f64) -> Result<Duration, EventRejected> {
    if secs.is_finite() && secs > 0.0 && secs <= 1e12 {
        Ok(Duration::from_secs_f64(secs))
    } else {
        Err(EventRejected::HalfLife)
    }
}

fn finite(field: &'static str, v: f64) -> Result<f64, EventRejected> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(EventRejected::NotFinite(field))
    }
}

impl SimEvent {
    /// Validate every field, so applying to several simulators cannot fail
    /// half-way through.
    pub fn prepare(&self) -> Result<Prepared, EventRejected> {
        let kind = match *self {
            Self::Jump { pct } => {
                finite("pct", pct)?;
                if pct <= -1.0 {
                    return Err(EventRejected::Sim(EventError::JumpBelowMinusOne));
                }
                EventKind::Jump(pct)
            }
            Self::DriftShift {
                delta,
                half_life_secs,
            } => EventKind::DriftShift {
                delta: finite("delta", delta)?,
                half_life: half_life(half_life_secs)?,
            },
            Self::DriftForTotalMove {
                total,
                half_life_secs,
            } => {
                EventKind::drift_for_total_move(finite("total", total)?, half_life(half_life_secs)?)
            }
            Self::VolShift {
                delta,
                half_life_secs,
            } => EventKind::VolShift {
                delta: finite("delta", delta)?,
                half_life: half_life(half_life_secs)?,
            },
            Self::FundamentalShift { delta } => {
                EventKind::FundamentalShift(finite("delta", delta)?)
            }
            Self::FundamentalTarget { target_cents } => {
                if target_cents < 1 {
                    return Err(EventRejected::Target);
                }
                return Ok(Prepared::SetFundamental(target_cents));
            }
        };
        Ok(Prepared::Queue(kind))
    }

    /// Short human-readable description for logs and the UI.
    pub fn summary(&self) -> String {
        match *self {
            Self::Jump { pct } => format!("jump {:+.1}%", pct * 100.0),
            Self::DriftShift {
                delta,
                half_life_secs,
            } => format!(
                "drift {delta:+.2}/yr, half-life {}",
                human_secs(half_life_secs)
            ),
            Self::DriftForTotalMove {
                total,
                half_life_secs,
            } => format!(
                "drift for {:+.1}% over ~{}",
                total * 100.0,
                human_secs(half_life_secs)
            ),
            Self::VolShift {
                delta,
                half_life_secs,
            } => format!(
                "vol {:+.0} pp, half-life {}",
                delta * 100.0,
                human_secs(half_life_secs)
            ),
            Self::FundamentalShift { delta } => {
                format!("fundamental {:+.1}%", (delta.exp() - 1.0) * 100.0)
            }
            Self::FundamentalTarget { target_cents } => {
                format!("fundamental target ${:.2}", target_cents as f64 / 100.0)
            }
        }
    }
}

fn human_secs(secs: f64) -> String {
    if secs >= 86_400.0 {
        format!("{:.0}d", secs / 86_400.0)
    } else if secs >= 3_600.0 {
        format!("{:.0}h", secs / 3_600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

/// When an event takes effect. Both fields absent means "now".
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Timing {
    /// Absolute simulated time, milliseconds since the epoch.
    pub at_ms: Option<i64>,
    /// Simulated seconds from now.
    pub delay_secs: Option<f64>,
}

impl Timing {
    pub fn resolve(self, now: Timestamp) -> Result<Timestamp, String> {
        match (self.at_ms, self.delay_secs) {
            (Some(_), Some(_)) => Err("give either `at_ms` or `delay_secs`, not both".into()),
            (Some(at), None) => Ok(Timestamp(at)),
            (None, Some(d)) if d.is_finite() && (0.0..=1e12).contains(&d) => {
                Ok(now + Duration::from_secs_f64(d))
            }
            (None, Some(_)) => Err("`delay_secs` must be a non-negative number".into()),
            (None, None) => Ok(now),
        }
    }
}

/// Body of `POST /api/symbols/{symbol}/events`.
#[derive(Clone, Debug, Deserialize)]
pub struct PushEventRequest {
    #[serde(flatten)]
    pub event: SimEvent,
    #[serde(flatten)]
    pub timing: Timing,
    /// Who sent it, e.g. `"quest-engine"`. Defaults to `"api"`.
    pub source: Option<String>,
    /// Free text for the log.
    pub note: Option<String>,
}

/// Semantic events a game emits. Each maps to a bundle of simulator events;
/// `magnitude` scales every number in the bundle (1.0 is the reference size).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameEventKind {
    ProductLaunch,
    EarningsBeat,
    EarningsMiss,
    Scandal,
    Lawsuit,
    Buyback,
    CeoResigns,
    Hype,
    MarketCrash,
    MarketRally,
    RateHike,
    RateCut,
}

/// Whether a game event hits one company or the whole market.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Company,
    Market,
}

/// Largest accepted `magnitude`. Keeps every scaled jump above −100 %.
pub const MAX_MAGNITUDE: f64 = 5.0;

impl GameEventKind {
    pub const ALL: [GameEventKind; 12] = [
        Self::ProductLaunch,
        Self::EarningsBeat,
        Self::EarningsMiss,
        Self::Scandal,
        Self::Lawsuit,
        Self::Buyback,
        Self::CeoResigns,
        Self::Hype,
        Self::MarketCrash,
        Self::MarketRally,
        Self::RateHike,
        Self::RateCut,
    ];

    pub fn scope(self) -> Scope {
        match self {
            Self::MarketCrash | Self::MarketRally | Self::RateHike | Self::RateCut => Scope::Market,
            _ => Scope::Company,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ProductLaunch => "Product launch",
            Self::EarningsBeat => "Earnings beat",
            Self::EarningsMiss => "Earnings miss",
            Self::Scandal => "Scandal",
            Self::Lawsuit => "Lawsuit",
            Self::Buyback => "Buyback",
            Self::CeoResigns => "CEO resigns",
            Self::Hype => "Hype",
            Self::MarketCrash => "Market crash",
            Self::MarketRally => "Market rally",
            Self::RateHike => "Rate hike",
            Self::RateCut => "Rate cut",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::ProductLaunch => {
                "Lasting value gain, a couple of hours of buying, mild excitement."
            }
            Self::EarningsBeat => "Gap up, value re-rated higher, brief volatility.",
            Self::EarningsMiss => "Gap down, value re-rated lower, an hour of turbulence.",
            Self::Scandal => "Sharp drop, lasting damage, hours of panic and selling.",
            Self::Lawsuit => "Small drop and a half day of nervousness; value slightly impaired.",
            Self::Buyback => "Slow steady bid over a day; value slightly higher.",
            Self::CeoResigns => "Drop and a couple of hours of volatility; value unchanged.",
            Self::Hype => "Pump with no change in value: the move fades as excitement decays.",
            Self::MarketCrash => "Every symbol gaps down and stays volatile for hours.",
            Self::MarketRally => "Every symbol gaps up with a day of follow-through.",
            Self::RateHike => "Every symbol's value marked down, selling over a day.",
            Self::RateCut => "Every symbol's value marked up, buying over a day.",
        }
    }

    /// The simulator events this game event expands to at `magnitude`.
    pub fn effects(self, magnitude: f64) -> Vec<SimEvent> {
        const HOUR: f64 = 3_600.0;
        let m = magnitude;
        match self {
            Self::ProductLaunch => vec![
                SimEvent::FundamentalShift { delta: 0.06 * m },
                SimEvent::DriftForTotalMove {
                    total: 0.04 * m,
                    half_life_secs: 2.0 * HOUR,
                },
                SimEvent::VolShift {
                    delta: 0.10 * m,
                    half_life_secs: HOUR,
                },
            ],
            Self::EarningsBeat => vec![
                SimEvent::Jump { pct: 0.05 * m },
                SimEvent::FundamentalShift { delta: 0.04 * m },
                SimEvent::VolShift {
                    delta: 0.15 * m,
                    half_life_secs: 0.5 * HOUR,
                },
            ],
            Self::EarningsMiss => vec![
                SimEvent::Jump { pct: -0.07 * m },
                SimEvent::FundamentalShift { delta: -0.05 * m },
                SimEvent::VolShift {
                    delta: 0.25 * m,
                    half_life_secs: HOUR,
                },
            ],
            Self::Scandal => vec![
                SimEvent::Jump { pct: -0.10 * m },
                SimEvent::FundamentalShift { delta: -0.08 * m },
                SimEvent::VolShift {
                    delta: 0.40 * m,
                    half_life_secs: 4.0 * HOUR,
                },
                SimEvent::DriftForTotalMove {
                    total: -0.05 * m,
                    half_life_secs: 6.0 * HOUR,
                },
            ],
            Self::Lawsuit => vec![
                SimEvent::Jump { pct: -0.03 * m },
                SimEvent::FundamentalShift { delta: -0.02 * m },
                SimEvent::VolShift {
                    delta: 0.20 * m,
                    half_life_secs: 12.0 * HOUR,
                },
            ],
            Self::Buyback => vec![
                SimEvent::FundamentalShift { delta: 0.02 * m },
                SimEvent::DriftForTotalMove {
                    total: 0.03 * m,
                    half_life_secs: 24.0 * HOUR,
                },
            ],
            Self::CeoResigns => vec![
                SimEvent::Jump { pct: -0.04 * m },
                SimEvent::VolShift {
                    delta: 0.30 * m,
                    half_life_secs: 2.0 * HOUR,
                },
            ],
            Self::Hype => vec![
                SimEvent::DriftForTotalMove {
                    total: 0.08 * m,
                    half_life_secs: HOUR,
                },
                SimEvent::VolShift {
                    delta: 0.30 * m,
                    half_life_secs: 2.0 * HOUR,
                },
            ],
            Self::MarketCrash => vec![
                SimEvent::Jump { pct: -0.08 * m },
                SimEvent::FundamentalShift { delta: -0.03 * m },
                SimEvent::VolShift {
                    delta: 0.50 * m,
                    half_life_secs: 6.0 * HOUR,
                },
            ],
            Self::MarketRally => vec![
                SimEvent::Jump { pct: 0.04 * m },
                SimEvent::DriftForTotalMove {
                    total: 0.03 * m,
                    half_life_secs: 6.0 * HOUR,
                },
            ],
            Self::RateHike => vec![
                SimEvent::FundamentalShift { delta: -0.02 * m },
                SimEvent::DriftForTotalMove {
                    total: -0.02 * m,
                    half_life_secs: 24.0 * HOUR,
                },
            ],
            Self::RateCut => vec![
                SimEvent::FundamentalShift { delta: 0.02 * m },
                SimEvent::DriftForTotalMove {
                    total: 0.02 * m,
                    half_life_secs: 24.0 * HOUR,
                },
            ],
        }
    }
}

/// Body of `POST /api/game/events`.
#[derive(Clone, Debug, Deserialize)]
pub struct GameEventRequest {
    pub kind: GameEventKind,
    /// Required for company-scoped kinds; ignored for market-wide ones.
    pub symbol: Option<String>,
    /// Scales the effect bundle. `(0, MAX_MAGNITUDE]`, default 1.
    pub magnitude: Option<f64>,
    #[serde(flatten)]
    pub timing: Timing,
    pub source: Option<String>,
    pub note: Option<String>,
}

/// One entry of `GET /api/game/catalog`.
#[derive(Clone, Debug, Serialize)]
pub struct CatalogEntry {
    pub kind: GameEventKind,
    pub label: &'static str,
    pub scope: Scope,
    pub description: &'static str,
    /// The bundle at magnitude 1.
    pub effects: Vec<SimEvent>,
}

/// An accepted event, as stored in the log and pushed to the stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: u64,
    /// Wall-clock time the request was accepted.
    pub received_at_ms: i64,
    /// Simulated time the event takes effect.
    pub at_ms: i64,
    /// Symbols affected.
    #[serde(with = "crate::save::symbol_vec")]
    pub symbols: Vec<Symbol>,
    /// `"game:scandal"` or `"sim:jump"`.
    pub kind: String,
    pub source: String,
    pub note: Option<String>,
    pub magnitude: Option<f64>,
    /// The simulator events applied to each symbol.
    pub effects: Vec<SimEvent>,
    /// Human-readable summaries of `effects`.
    pub summary: Vec<String>,
}
