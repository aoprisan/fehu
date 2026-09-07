//! Saving the market to disk and starting it up again.
//!
//! Everything the server knows lives in memory: the simulators, the books,
//! the users and their money. A restart without this module loses all of it —
//! not just the prices, which are reproducible from a seed, but the accounts,
//! which are not. So the whole [`Market`] is written to one file, and read
//! back at start-up in place of the warm-up.
//!
//! The file is versioned ([`STATE_VERSION`]) and self-describing JSON: the
//! `fehu` crate's own `Exchange`/`Candles` representations nested inside the
//! web app's users, accounts, traders and logs. A file from another version,
//! or one listing symbols this build does not have, is refused rather than
//! guessed at — a save that only half-loads is worse than none.
//!
//! Writes are atomic: the snapshot goes to a temporary file beside the
//! target, which is then renamed over it, so a crash mid-write leaves the
//! previous save intact.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fehu::{Candle, Candles, Exchange, Tick, Trade};
use serde::{Deserialize, Serialize};

use crate::account::{Account, User};
use crate::events::EventRecord;
use crate::market::{App, Halt};
use crate::trading::{OrderRecord, Trader};

/// A listed symbol's ticker.
///
/// This is `&'static str` — every ticker is one of the four the build lists —
/// but spelled as an alias so `serde`'s derive does not mistake it for data
/// borrowed from the input and demand a `'de: 'static` bound. The [`symbol`]
/// modules below turn a ticker on disk back into one of ours.
pub type Symbol = &'static str;

/// Bumped whenever the save format changes in a way older files cannot be
/// read as. There is no migration path: a file from another version is
/// refused.
pub const STATE_VERSION: u32 = 2;

/// Everything needed to carry on where the server left off.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Save {
    /// [`STATE_VERSION`] at the time of writing.
    pub version: u32,
    /// Wall-clock time the file was written.
    pub saved_at_ms: i64,
    /// Simulated time the market had reached. Start-up continues from here.
    pub sim_now_ms: i64,
    /// One entry per listed symbol, in listing order.
    pub symbols: Vec<SymbolSave>,
    /// The users, their money, their traders and the logs.
    pub market: MarketSave,
}

/// One symbol's simulator, book and bar history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolSave {
    /// Ticker. The rest of the symbol's metadata comes from the build, so a
    /// file listing symbols this build does not have is refused.
    pub symbol: String,
    pub exchange: Exchange,
    pub candles: Candles,
    pub coarse_daily: Vec<Candle>,
    pub last_tick: Option<Tick>,
    pub ticks_total: u64,
    pub tape: Vec<Trade>,
    pub trades_total: u64,
    /// Set if trading in this symbol was stopped when the file was written.
    pub halt: Option<Halt>,
    /// The price the limit band is measured from.
    pub band_cents: i64,
}

/// The people, their money and everything written down about them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MarketSave {
    pub users: Vec<User>,
    pub accounts: Vec<Account>,
    pub traders: Vec<Trader>,
    pub orders: Vec<OrderRecord>,
    /// The response each order was accepted with, by order id. It is not
    /// part of an order's own JSON — no client asked for it twice over — but
    /// it is what a `client_order_id` replays, so it has to survive a
    /// restart with the order.
    #[serde(default)]
    pub order_responses: Vec<(u64, crate::trading::OrderResponse)>,
    /// Accepted events, oldest first.
    pub events: Vec<EventRecord>,
    /// `(api key, user id)`, so a player's key still works after a restart.
    pub api_keys: Vec<(String, u64)>,
    pub next_user_id: u64,
    pub next_account_id: u64,
    pub next_trader_id: u64,
    pub next_event_id: u64,
}

/// Why a save could not be written or read.
#[derive(Debug)]
pub enum SaveError {
    /// The file could not be read or written.
    Io(io::Error),
    /// The file is not the JSON this module writes.
    Format(serde_json::Error),
    /// Written by a different version of the format.
    Version { found: u32, expected: u32 },
    /// The symbols in the file are not the symbols this build lists.
    Symbols {
        found: Vec<String>,
        expected: Vec<String>,
    },
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "state file: {e}"),
            Self::Format(e) => write!(f, "state file is not readable: {e}"),
            Self::Version { found, expected } => write!(
                f,
                "state file is version {found}, this build reads {expected}"
            ),
            Self::Symbols { found, expected } => write!(
                f,
                "state file lists {found:?}, this build lists {expected:?}"
            ),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for SaveError {
    fn from(e: serde_json::Error) -> Self {
        Self::Format(e)
    }
}

/// Write `app`'s state to `path`, through a temporary file so an interrupted
/// write cannot destroy the previous save.
pub fn write(app: &App, path: &Path) -> Result<(), SaveError> {
    let save = app.save();
    let tmp = temp_path(path);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(&tmp)?;
    serde_json::to_writer(io::BufWriter::new(&file), &save)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read a save from `path`, checking that this build can use it.
pub fn read(path: &Path) -> Result<Save, SaveError> {
    let file = std::fs::File::open(path)?;
    let save: Save = serde_json::from_reader(io::BufReader::new(file))?;
    if save.version != STATE_VERSION {
        return Err(SaveError::Version {
            found: save.version,
            expected: STATE_VERSION,
        });
    }
    let found: Vec<String> = save.symbols.iter().map(|s| s.symbol.clone()).collect();
    let expected: Vec<String> = crate::market::TICKERS.iter().map(|s| (*s).into()).collect();
    if found != expected {
        return Err(SaveError::Symbols { found, expected });
    }
    Ok(save)
}

/// Where the temporary file for `path` goes: beside it, so the rename that
/// replaces it stays on one filesystem.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Save `app` to `path` every `period` until the task is dropped. A failed
/// save is logged and retried at the next tick: it must never take the
/// server down.
pub async fn autosave(app: Arc<App>, path: PathBuf, period: Duration) {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // The first tick is immediate; the state is fresh.
    loop {
        interval.tick().await;
        match write(&app, &path) {
            Ok(()) => tracing::debug!(path = %path.display(), "state saved"),
            Err(e) => tracing::error!(path = %path.display(), error = %e, "state not saved"),
        }
    }
}

// ---------------------------------------------------------------------------
// Symbols are `&'static str` in memory — one of the four the build lists — and
// plain strings on disk. These modules do the swap, and refuse a ticker the
// build does not have.

/// A `&'static str` ticker.
pub mod symbol {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(s: &&'static str, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<&'static str, D::Error> {
        let ticker = String::deserialize(de)?;
        super::intern(&ticker).map_err(serde::de::Error::custom)
    }
}

/// An optional `&'static str` ticker.
pub mod symbol_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(s: &Option<&'static str>, ser: S) -> Result<S::Ok, S::Error> {
        match s {
            Some(s) => ser.serialize_some(s),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<&'static str>, D::Error> {
        let ticker = Option::<String>::deserialize(de)?;
        ticker
            .map(|t| super::intern(&t))
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

/// A list of `&'static str` tickers.
pub mod symbol_vec {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[&'static str], ser: S) -> Result<S::Ok, S::Error> {
        v.serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<&'static str>, D::Error> {
        Vec::<String>::deserialize(de)?
            .iter()
            .map(|t| super::intern(t))
            .collect::<Result<_, _>>()
            .map_err(serde::de::Error::custom)
    }
}

/// A map keyed by `&'static str` ticker.
pub mod symbol_map {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, V: Serialize>(
        map: &BTreeMap<&'static str, V>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        map.serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, V: Deserialize<'de>>(
        de: D,
    ) -> Result<BTreeMap<&'static str, V>, D::Error> {
        BTreeMap::<String, V>::deserialize(de)?
            .into_iter()
            .map(|(k, v)| super::intern(&k).map(|s| (s, v)))
            .collect::<Result<_, _>>()
            .map_err(serde::de::Error::custom)
    }
}

/// The listed symbol `ticker` names, as the `&'static str` the rest of the
/// server uses.
fn intern(ticker: &str) -> Result<&'static str, String> {
    crate::market::intern(ticker).ok_or_else(|| format!("unknown symbol {ticker:?}"))
}
