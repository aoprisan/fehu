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
//! web app's users, accounts, traders and logs. A file from an unsupported
//! version is refused rather than guessed at — a save that only half-loads is
//! worse than none.
//!
//! Since version 5 the file is also the symbol table. Symbols are listed and
//! delisted while the server runs, so the build has no say in which ones a
//! restored market has: each [`SymbolSave`] carries its own [`SymbolInfo`],
//! every ticker in the file is registered as it is read ([`crate::symbols`]),
//! and the listing a file describes is the listing that comes back.
//!
//! Writes are atomic: the snapshot goes to a temporary file beside the
//! target, which is then renamed over it, so a crash mid-write leaves the
//! previous save intact.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fehu::{Candle, Candles, Exchange, Tick, Trade};
use serde::{Deserialize, Serialize};

use crate::account::{Account, User};
use crate::events::EventRecord;
use crate::market::{App, Halt, SymbolInfo};
use crate::trading::{OrderRecord, StopOrder, Trader};

/// A listed symbol's ticker.
///
/// This is `&'static str` — a ticker registered with [`crate::symbols`], and
/// so alive for as long as the process — but spelled as an alias so `serde`'s
/// derive does not mistake it for data borrowed from the input and demand a
/// `'de: 'static` bound. The [`symbol`] modules below turn a ticker on disk
/// back into one of ours.
pub type Symbol = &'static str;

/// Current save format. Version 2 is migrated by hashing its plaintext
/// credentials, version 3 by starting the stop store empty, and version 4 by
/// taking each symbol's metadata from the build it was written by; all other
/// older or newer versions are refused.
pub const STATE_VERSION: u32 = 5;

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

/// One symbol's listing, simulator, book and bar history.
#[derive(Clone, Debug, Serialize, Deserialize)]
// The listing's ticker is registered on the way in, not borrowed from the
// input, so the derive needs no `'de: 'static`.
#[serde(bound(deserialize = ""))]
pub struct SymbolSave {
    /// Ticker.
    pub symbol: String,
    /// What the listing is: name, sector, shares outstanding, seed. Version 4
    /// and earlier carried none of it — the build supplied it — so
    /// [`read`] fills it in from the build's seeded symbols when migrating,
    /// and every version-5 file has it.
    #[serde(default)]
    pub info: Option<SymbolInfo>,
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
    /// Triggers still waiting for a price. Version 3 files carry none.
    #[serde(default)]
    pub stops: Vec<StopOrder>,
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
    /// `(SHA-256 digest, user id)`, so credentials work without being saved.
    pub api_keys: Vec<(String, u64)>,
    pub next_user_id: u64,
    pub next_account_id: u64,
    pub next_trader_id: u64,
    pub next_event_id: u64,
    /// Ids for the stops held on the symbols. Version 3 files have none, so
    /// the counter starts where a fresh market would.
    #[serde(default)]
    pub next_stop_id: u64,
    /// The lowest id a new order may take. Books carry their own counters and
    /// the market allocates above all of them; this is what a delisted
    /// symbol's book leaves behind, so its orders' ids are never reissued.
    #[serde(default)]
    pub next_order_id: u64,
}

/// Why a save could not be written or read.
#[derive(Debug)]
pub enum SaveError {
    /// The file could not be read or written.
    Io(io::Error),
    /// The file is not the JSON this module writes.
    Format(serde_json::Error),
    /// The snapshot has inconsistent accounting, ownership or identity state.
    Invalid(Vec<String>),
    /// Written by a different version of the format.
    Version { found: u32, expected: u32 },
    /// A version-4 file names a symbol whose metadata this build does not
    /// have. Only migration can hit this: a version-5 file carries its own.
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
            Self::Invalid(issues) => write!(
                f,
                "state file has inconsistent state: {}",
                issues.join("; ")
            ),
            Self::Version { found, expected } => write!(
                f,
                "state file is version {found}, this build reads {expected}"
            ),
            Self::Symbols { found, expected } => write!(
                f,
                "state file is version 4 and lists {found:?}, whose metadata has to come from \
                 this build, which seeds {expected:?}"
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
    write_snapshot(&file, &save)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn write_snapshot(writer: impl Write, save: &Save) -> Result<(), SaveError> {
    let mut writer = io::BufWriter::new(writer);
    serde_json::to_writer(&mut writer, save)?;
    writer.flush()?;
    Ok(())
}

/// Read a save from `path`, checking that this build can use it.
pub fn read(path: &Path) -> Result<Save, SaveError> {
    let file = std::fs::File::open(path)?;
    let mut save: Save = serde_json::from_reader(io::BufReader::new(file))?;
    if save.version == 2 {
        // Version 2 held the keys themselves; the digests are all this build
        // ever wants, and hashing them here keeps every player's credential
        // working across the upgrade.
        for (key, _) in &mut save.market.api_keys {
            *key = crate::auth::key_digest(key);
        }
        save.version = 3;
    }
    if save.version == 3 {
        // Version 3 predates stops. `serde` has already defaulted the store
        // to empty; the counter has to start where a fresh market's does.
        save.market.next_stop_id = save.market.next_stop_id.max(1);
        save.version = 4;
    }
    if save.version == 4 {
        // Version 4 kept a symbol's metadata in the build, so a file could
        // only ever list the build's own symbols. That is where the metadata
        // for these has to come from — there is nowhere else — and a file
        // naming anything else cannot be migrated.
        adopt_build_metadata(&mut save)?;
        save.version = 5;
    }
    if save.version != STATE_VERSION {
        return Err(SaveError::Version {
            found: save.version,
            expected: STATE_VERSION,
        });
    }
    validate_accounting(&save)?;
    Ok(save)
}

/// Fill in a version-4 file's symbol metadata from the build's seeded
/// symbols, which is where it lived when the file was written.
fn adopt_build_metadata(save: &mut Save) -> Result<(), SaveError> {
    let mut seeded: BTreeMap<&'static str, crate::market::SymbolInfo> =
        crate::market::seeded_symbols(fehu::Timestamp(save.sim_now_ms))
            .into_iter()
            .map(|spec| (spec.info.symbol, spec.info))
            .collect();
    let mut unknown = Vec::new();
    for symbol in &mut save.symbols {
        match crate::symbols::lookup(&symbol.symbol).and_then(|t| seeded.remove(t)) {
            Some(info) => symbol.info = Some(info),
            None => unknown.push(symbol.symbol.clone()),
        }
    }
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(SaveError::Symbols {
            found: unknown,
            expected: crate::market::TICKERS.iter().map(|s| (*s).into()).collect(),
        })
    }
}

/// Validate before maps can silently discard duplicate identities on restore.
fn validate_accounting(save: &Save) -> Result<(), SaveError> {
    let mut issues = Vec::new();
    let mut listed = BTreeSet::new();
    for symbol in &save.symbols {
        if let Err(issue) = symbol.exchange.book().validate_state() {
            issues.push(format!("{} book: {issue}", symbol.symbol));
        }
        // The file is the symbol table, so it has to be one: every listing
        // names itself, and it names itself once. Two entries for a ticker
        // would give the market two books for one symbol, and the second
        // would be unreachable behind the first.
        match &symbol.info {
            None => issues.push(format!("{} carries no listing", symbol.symbol)),
            Some(info) => {
                if crate::symbols::lookup(&symbol.symbol) != Some(info.symbol) {
                    issues.push(format!(
                        "{} is filed under a listing for {}",
                        symbol.symbol, info.symbol
                    ));
                }
                if !listed.insert(info.symbol) {
                    issues.push(format!("{} is listed more than once", info.symbol));
                }
            }
        }
    }
    let users = check_ids(
        "user",
        save.market.users.iter().map(|u| u.id.0),
        save.market.next_user_id,
        &mut issues,
    );
    check_ids(
        "account",
        save.market.accounts.iter().map(|a| a.id.0),
        save.market.next_account_id,
        &mut issues,
    );
    check_ids(
        "trader",
        save.market.traders.iter().map(|t| t.id.0),
        save.market.next_trader_id,
        &mut issues,
    );
    check_ids(
        "event",
        save.market.events.iter().map(|e| e.id),
        save.market.next_event_id,
        &mut issues,
    );
    let mut stop_ids = BTreeSet::new();
    let traders: BTreeSet<u64> = save.market.traders.iter().map(|t| t.id.0).collect();
    if save.market.next_stop_id == 0 || save.market.next_stop_id == u64::MAX {
        issues.push("stop id counter is invalid or exhausted".into());
    }
    for symbol in &save.symbols {
        for stop in &symbol.stops {
            if stop.stop_id == 0
                || stop.stop_id >= save.market.next_stop_id
                || !stop_ids.insert(stop.stop_id)
            {
                issues.push("stop ids are duplicated, zero, or overlap the next id".into());
            }
            if !traders.contains(&stop.trader_id) {
                issues.push(format!("stop {} has no trader", stop.stop_id));
            }
            if stop.symbol != symbol.symbol {
                issues.push(format!(
                    "stop {} is filed under another symbol",
                    stop.stop_id
                ));
            }
            if stop.qty == 0
                || stop.stop_price_cents <= 0
                || stop.limit_price_cents.is_some_and(|p| p <= 0)
            {
                issues.push(format!(
                    "stop {} has an invalid size or price",
                    stop.stop_id
                ));
            }
        }
    }
    let mut key_users = BTreeSet::new();
    let mut digests = BTreeSet::new();
    for (digest, user) in &save.market.api_keys {
        if !users.contains(user) || !key_users.insert(*user) || !digests.insert(digest) {
            issues.push("API key ownership is missing or duplicated".into());
        }
        if digest.strip_prefix("sha256:").is_none_or(|hex| {
            hex.len() != 64
                || !hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            issues.push("API key digest has an invalid format".into());
        }
    }
    if users != key_users {
        issues.push("every user must have exactly one API key digest".into());
    }
    if !issues.is_empty() {
        return Err(SaveError::Invalid(issues));
    }
    // Restore without warming up or starting background tasks. Reconciliation
    // checks the retained ledgers and all books, even when logs are bounded.
    let app = App::restore(crate::market::Options::default(), save.clone());
    let report = app.market().reconcile();
    if report.valid {
        Ok(())
    } else {
        Err(SaveError::Invalid(report.issues))
    }
}

fn check_ids(
    name: &str,
    ids: impl Iterator<Item = u64>,
    next: u64,
    issues: &mut Vec<String>,
) -> BTreeSet<u64> {
    let mut seen = BTreeSet::new();
    if next == 0 || next == u64::MAX {
        issues.push(format!("{name} id counter is invalid or exhausted"));
    }
    for id in ids {
        if id == 0 || id >= next || !seen.insert(id) {
            issues.push(format!(
                "{name} ids are duplicated, zero, or overlap the next id"
            ));
        }
    }
    seen
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
// Symbols are `&'static str` in memory and plain strings on disk. These
// modules do the swap. A ticker read from a file is registered rather than
// looked up: the file is the symbol table, so the symbols it names are the
// symbols there are, and a record naming one no longer listed — the fills and
// ledger entries of a delisted company — still resolves to the same string it
// always did. Only a malformed ticker is refused.

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

/// `ticker`, as the one `&'static str` the rest of the server uses for it.
fn intern(ticker: &str) -> Result<&'static str, String> {
    crate::symbols::register(ticker).map_err(|e| format!("symbol {ticker:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_flush_errors_are_reported() {
        struct FailsOnFlush;
        impl Write for FailsOnFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("flush failed"))
            }
        }
        let app = App::new(crate::market::Options {
            history_days: 0,
            warmup_hours: 0,
            ..Default::default()
        });
        assert!(matches!(
            write_snapshot(FailsOnFlush, &app.save()),
            Err(SaveError::Io(_))
        ));
    }
}
