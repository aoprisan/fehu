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
//!
//! # The snapshot is half of the persistence
//!
//! A snapshot is a checkpoint, not the record. Everything between two of
//! them is in the command journal beside it ([`crate::journal`]), which is
//! written before each change is acknowledged; a snapshot records the
//! journal sequence it includes ([`MarketSave::journal_seq`]), start-up
//! replays what came after, and the journal is rewritten from that point
//! once the new snapshot is safely renamed into place. So the file is
//! allowed to be minutes out of date without anything acknowledged being at
//! risk, and it is what keeps the journal short.

use std::collections::BTreeSet;
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

/// Current save format. Any other version, older or newer, is refused.
///
/// Version 9 gave the world the things it makes and the things it pays for:
/// recipes and the jobs running under them ([`crate::jobs`]), the budgets
/// rewards are paid from and every game event id already paid
/// ([`crate::rewards`]), and what events are still doing to production and
/// demand ([`crate::world`]). A version 8 file carries none of it — and a
/// world restored without its running jobs would have taken payment for
/// promises it no longer knows about — so it is refused like the rest.
///
/// Version 8 gave every listing an [`AssetKind`](crate::symbol::AssetKind):
/// a company with a fixed float, or a good with a unit and a count that
/// production and consumption move. A version 7 file names a
/// `shares_outstanding` and nothing else, which is a stock — but its
/// exchange also predates the switch that stops synthetic liquidity
/// printing units nobody issued, so the file is refused rather than read as
/// half a world.
///
/// Version 7 added the command journal's sequence and its idempotency index,
/// so a snapshot says exactly which commands it already contains and a retry
/// that arrives after a restart is still a retry. A version 6 file has
/// neither, and starting from one would replay a journal from the beginning
/// or lose it; there is no data to migrate, so it is refused like the rest.
///
/// Version 6 moved the money into the currency ledger. Every earlier version
/// keeps a balance on each account and no supply behind it, so there is
/// nothing to migrate one *to* without inventing where its currency came
/// from. The migrations that used to bring versions 2, 3 and 4 forward all
/// arrived at that shape, so they no longer lead anywhere and are gone with
/// it.
///
/// The plan's answer for a world worth keeping is an importer: post the old
/// balances as one labelled [`Migration`](fehu::ledger::Reason::Migration)
/// transaction out of issuance, so the currency has a recorded origin and
/// the books still add up. It is a day's work when there is such a world.
/// There is not: the current file is a demo, regenerated from its seeds.
pub const STATE_VERSION: u32 = 9;

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
    /// Every wallet in the world and the supply behind them. Since version 6
    /// this, and not the accounts, is where the money is.
    #[serde(default)]
    pub ledger: fehu::ledger::Ledger,
    /// The four wallets the world always has.
    #[serde(default)]
    pub wallets: crate::market::Wallets,
    /// Per symbol, the wallet its payouts come out of.
    #[serde(default)]
    pub issuers: Vec<(String, fehu::ledger::WalletId)>,
    /// What the world will make and what it charges. Since version 8.
    #[serde(default)]
    pub catalog: crate::catalog::Catalog,
    /// The traders the world runs itself. Since version 8.
    #[serde(default)]
    pub npcs: Vec<crate::npc::Npc>,
    /// What the world knows how to make. Since version 9.
    #[serde(default)]
    pub recipes: crate::jobs::RecipeBook,
    /// Every job, running and finished, oldest first. Since version 9.
    #[serde(default)]
    pub jobs: Vec<crate::jobs::Job>,
    /// The id the next job will take.
    #[serde(default)]
    pub next_job_id: u64,
    /// Budgets, reward rules, and the game event ids already paid. Since
    /// version 9.
    #[serde(default)]
    pub rewards: crate::rewards::RewardBook,
    /// What events are still doing to production and demand. Since version 9.
    #[serde(default)]
    pub world: crate::world::WorldEffects,
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
    /// The journal sequence this snapshot includes. Start-up replays the
    /// entries after it and nothing before it, and the journal is rewritten
    /// from here once the file is safely in place.
    #[serde(default)]
    pub journal_seq: u64,
    /// What each `Idempotency-Key` answered, oldest first. It lives here
    /// rather than in the journal because a snapshot may fall between any
    /// two commands, and a retry must be a retry on either side of one.
    #[serde(default)]
    pub commands: Vec<crate::journal::CommandRecord>,
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
pub async fn write(app: &App, path: &Path) -> Result<(), SaveError> {
    let save = app.save().await;
    let seq = save.market.journal_seq;
    let tmp = temp_path(path);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(&tmp)?;
    write_snapshot(&file, &save)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    // Only once the snapshot is in place: the commands up to `seq` are in it
    // now, so the journal no longer has to carry them. A truncation that
    // fails costs nothing but a replay of entries the snapshot already has.
    app.truncate_journal(seq).await;
    Ok(())
}

fn write_snapshot(writer: impl Write, save: &Save) -> Result<(), SaveError> {
    let mut writer = io::BufWriter::new(writer);
    serde_json::to_writer(&mut writer, save)?;
    writer.flush()?;
    Ok(())
}

/// Read a save from `path`, checking that this build can use it.
///
/// The version is read before anything else is, so a file this build cannot
/// use is turned away saying so rather than failing somewhere in the middle
/// of a shape that has since changed.
pub fn read(path: &Path) -> Result<Save, SaveError> {
    let file = std::fs::File::open(path)?;
    let value: serde_json::Value = serde_json::from_reader(io::BufReader::new(file))?;
    let found = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    if found != STATE_VERSION {
        return Err(SaveError::Version {
            found,
            expected: STATE_VERSION,
        });
    }
    let save: Save = serde_json::from_value(value)?;
    validate_accounting(&save)?;
    Ok(save)
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
    // Every user has exactly one key, except the ones nobody can sign in
    // as: an NPC's identity is created without a credential on purpose, so
    // that there is none to leak and no request can arrive claiming to be
    // it. A user with no key and no NPC behind it is a user nothing can
    // reach, which is a broken file rather than a design.
    let house: BTreeSet<u64> = save.market.npcs.iter().map(|n| n.user_id.0).collect();
    let keyless: BTreeSet<u64> = users.difference(&key_users).copied().collect();
    if !keyless.is_subset(&house) {
        issues.push("every user must have exactly one API key digest, or be an NPC's".into());
    }
    if !issues.is_empty() {
        return Err(SaveError::Invalid(issues));
    }
    // Reconcile the file as it stands, before anything is built from it.
    // This checks the retained ledgers and all books, even when logs are
    // bounded.
    let report = crate::reconcile::reconcile(save);
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
        match write(&app, &path).await {
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

    #[tokio::test]
    async fn snapshot_flush_errors_are_reported() {
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
            write_snapshot(FailsOnFlush, &app.save().await),
            Err(SaveError::Io(_))
        ));
    }
}
