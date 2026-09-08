//! The command journal: what was asked for, in the order it was granted.
//!
//! A snapshot ([`crate::save`]) is written every so often; everything that
//! happens between two snapshots is lost when the process stops without one.
//! This module closes that gap, and it does it by writing down the
//! *commands* rather than the state they produced.
//!
//! That works because the library underneath is deterministic and the server
//! above it is single-threaded where it matters: every change to money or to
//! a book is one job on the market actor, so there is exactly one order in
//! which things happened. Write the commands down in that order and a
//! checkpoint plus the commands after it reproduces the state exactly.
//!
//! # The shape of a command
//!
//! A [`Command`] is a request with **every non-deterministic input already
//! resolved**. The simulated instant it runs at and the wall-clock time it
//! arrived are fields of the [`JournalEntry`], not readings the apply path
//! takes for itself; a new user's API key is journaled as the digest that
//! was generated for it, never as a fresh one; a listing carries its seed.
//! [`Market::pinned_now`](Market) is what enforces the first of those: while
//! a command is being applied, [`Market::now`] answers with the entry's
//! instant rather than the clock, so a command replayed a week later behaves
//! as it did when it was accepted.
//!
//! [`apply`] is the only implementation. The live path and the replay path
//! are the same code over the same input, which is the only way the two can
//! be trusted to agree.
//!
//! # How a mutation runs
//!
//! [`Market::run_command`], inside the one market job that already
//! serialises money and books:
//!
//! 1. If the request carried an `Idempotency-Key` that has been seen before
//!    with the same payload, the recorded response is returned and nothing
//!    is applied. The same key over a *different* payload is a conflict.
//! 2. The command is applied. Every apply path validates before it mutates,
//!    so a refused command leaves nothing behind and is not journaled: a
//!    retry of a refusal is simply tried again.
//! 3. The entry is appended and flushed, and only then is the caller
//!    answered. If the append fails, the journal is [`Journal::broken`] —
//!    memory is ahead of disk and must not be served as committed — and
//!    every later mutation is refused until the process is restarted.
//!
//! # Durability
//!
//! An entry a client is waiting on is `fsync`ed before it is acknowledged,
//! so an acknowledged command is on disk. The engine's [`Command::Step`] is
//! not: nobody is waiting on it, and the appends are sequential, so the next
//! acknowledged command flushes every step before it. What a crash can lose
//! is therefore only steps that no acknowledged command depended on, which
//! replay reconstructs by stepping to wall time again.
//!
//! # Bounds
//!
//! The file holds the entries since the last snapshot and is rewritten when
//! the next one lands ([`Journal::truncate_to`]). The idempotency index is
//! *not* in the file — a snapshot may be taken at any time — so it lives in
//! the market and is saved with it, capped at
//! [`Options::command_log`](crate::market::Options::command_log) entries,
//! oldest evicted first.
//!
//! # What is deliberately not here
//!
//! One writer, one file, no outbox and no queryable history: this is the
//! plan's tier one. Tier two replaces the file with SQLite without changing
//! the command model.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use fehu::{Order, Owner, Timestamp, TraderId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::account::{AccountId, AccountStatus, UserId};
use crate::api::ApiError;
use crate::events::{EventRecord, GameEventKind, MAX_MAGNITUDE, Prepared, Scope, SimEvent};
use crate::market::{
    Amendment, DelistError, Market, PlaceRequest, Placed, SymbolInfo, SymbolSpec, wall_now_ms,
};
use crate::trading::{AmendRequest, AmendResponse, OpenOrderDto, OrderRequest, StopRequest};

/// Format of the journal file. A file written by another version is refused
/// rather than half-understood, exactly as a save file is.
pub const JOURNAL_VERSION: u32 = 1;

/// Who asked for a command.
///
/// Resolved from the credential on the request, never from its body, and
/// written down so that a replayed command is applied under the same
/// authority it was accepted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// A player, proved by their API key.
    User { id: u64 },
    /// The operator, proved by `FEHU_ADMIN_KEY` (or by there being none).
    Operator,
    /// Nobody: a route that needs no credential, such as signing up.
    Anonymous,
    /// The engine loop. Only ever [`Command::Step`].
    Engine,
}

impl Principal {
    /// The user this speaks for, if it speaks for one.
    #[must_use]
    pub fn user(self) -> Option<UserId> {
        match self {
            Self::User { id } => Some(UserId(id)),
            _ => None,
        }
    }
}

/// A listing, as the journal keeps it: the cleaned request, with the seed
/// resolved so that replay builds the same company.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub symbol: String,
    pub name: String,
    pub sector: String,
    pub description: String,
    pub shares_outstanding: u64,
    pub seed: u64,
    pub start_price_cents: i64,
    pub drift: f64,
    pub volatility: f64,
    pub history_days: usize,
    pub source: String,
    pub note: Option<String>,
}

/// Everything that changes the market, as one enum.
///
/// Each variant is what a route accepted once it had been authorised,
/// validated and cleaned — trimmed strings, resolved defaults — so the
/// journal holds canonical values and the apply path does not have to guess
/// which form it is looking at.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Register a user and install the API key digest generated for them.
    /// The key itself is never journaled: the response that created the user
    /// holds the only copy.
    CreateUser {
        name: Option<String>,
        email: Option<String>,
        key_digest: String,
    },
    /// Open another account for a user and fund it from the faucet.
    OpenAccount {
        user_id: u64,
        name: Option<String>,
        cash_cents: i64,
    },
    /// The one-call sign-up, or a trader on an existing user or account.
    CreateTrader {
        caller: Option<u64>,
        user_id: Option<u64>,
        account_id: Option<u64>,
        name: Option<String>,
        email: Option<String>,
        cash_cents: i64,
        /// Set when a user is created alongside the trader.
        key_digest: Option<String>,
    },
    /// Operator: create currency into an account.
    Mint {
        account_id: u64,
        amount_cents: i64,
        memo: Option<String>,
    },
    /// Operator: destroy currency out of an account.
    Burn {
        account_id: u64,
        amount_cents: i64,
        memo: Option<String>,
    },
    /// Operator: [`Command::Mint`] addressed by trader instead of account.
    MintToTrader {
        trader_id: u64,
        amount_cents: i64,
        memo: Option<String>,
    },
    /// Freeze or unfreeze (operator), or close (owner).
    SetAccountStatus {
        account_id: u64,
        status: AccountStatus,
        /// The owner closing their own account; `None` for an operator.
        owner: Option<u64>,
    },
    PlaceOrder {
        symbol: String,
        order: OrderRequest,
    },
    AmendOrder {
        symbol: String,
        order_id: u64,
        amend: AmendRequest,
    },
    CancelOrder {
        symbol: String,
        trader_id: u64,
        order_id: u64,
    },
    CancelAll {
        trader_id: u64,
    },
    PlaceStop {
        symbol: String,
        stop: StopRequest,
    },
    CancelStop {
        symbol: String,
        trader_id: u64,
        stop_id: u64,
    },
    ListSymbol {
        listing: Listing,
    },
    Delist {
        symbol: String,
        cents_per_share: Option<i64>,
        source: String,
        note: Option<String>,
    },
    Dividend {
        symbol: String,
        cents_per_share: i64,
        source: String,
        note: Option<String>,
    },
    Halt {
        symbol: String,
    },
    Resume {
        symbol: String,
    },
    /// A raw simulator event pushed at one symbol.
    SimEvent {
        symbol: String,
        event: SimEvent,
        source: String,
        note: Option<String>,
    },
    /// A game event from the catalogue, at one symbol or at all of them.
    GameEvent {
        kind: GameEventKind,
        magnitude: f64,
        symbol: Option<String>,
        source: String,
        note: Option<String>,
    },
    /// The engine tick: advance every symbol to the entry's instant.
    ///
    /// It carries no fields of its own because it needs none —
    /// [`JournalEntry::at_ms`] *is* its target, and journaling it is what
    /// keeps replay off the clock.
    Step,
}

impl Command {
    /// A short name for logs and for `GET /api/commands/{key}`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CreateUser { .. } => "create_user",
            Self::OpenAccount { .. } => "open_account",
            Self::CreateTrader { .. } => "create_trader",
            Self::Mint { .. } => "mint",
            Self::Burn { .. } => "burn",
            Self::MintToTrader { .. } => "mint_to_trader",
            Self::SetAccountStatus { .. } => "set_account_status",
            Self::PlaceOrder { .. } => "place_order",
            Self::AmendOrder { .. } => "amend_order",
            Self::CancelOrder { .. } => "cancel_order",
            Self::CancelAll { .. } => "cancel_all",
            Self::PlaceStop { .. } => "place_stop",
            Self::CancelStop { .. } => "cancel_stop",
            Self::ListSymbol { .. } => "list_symbol",
            Self::Delist { .. } => "delist",
            Self::Dividend { .. } => "dividend",
            Self::Halt { .. } => "halt",
            Self::Resume { .. } => "resume",
            Self::SimEvent { .. } => "sim_event",
            Self::GameEvent { .. } => "game_event",
            Self::Step => "step",
        }
    }

    /// A digest of the command, for telling a retry from a different request
    /// that reused its key.
    ///
    /// Taken over what the *client* asked for. Values this server generated
    /// while accepting the request — the API key digest a sign-up carries —
    /// are blanked first, because a retry generates fresh ones and would
    /// otherwise never match the request it is retrying.
    #[must_use]
    pub fn payload_hash(&self) -> String {
        let bytes = serde_json::to_vec(&self.as_asked()).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(b"fehu-command-v1\0");
        hasher.update(&bytes);
        let mut digest = String::from("sha256:");
        for byte in hasher.finalize() {
            use std::fmt::Write as _;
            let _ = write!(digest, "{byte:02x}");
        }
        digest
    }

    /// This command with everything the server chose for it blanked: what
    /// two deliveries of one request have in common.
    fn as_asked(&self) -> Self {
        let mut asked = self.clone();
        match &mut asked {
            Self::CreateUser { key_digest, .. } => key_digest.clear(),
            Self::CreateTrader { key_digest, .. } => *key_digest = None,
            _ => {}
        }
        asked
    }
}

/// One accepted command, in the order it was accepted.
///
/// Refusals are not entries: a command that was refused changed nothing, so
/// there is nothing for replay to redo.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Dense and increasing, continuing from the snapshot this file follows.
    pub seq: u64,
    pub principal: Principal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub payload_hash: String,
    /// Simulated time the command runs at. Replay pins the market to it.
    pub at_ms: i64,
    /// Wall-clock time it arrived, for the timestamps a record carries.
    pub wall_ms: i64,
    pub command: Command,
}

/// The response a command was acknowledged with, kept so a client that lost
/// the answer can have it back.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandRecord {
    pub key: String,
    pub seq: u64,
    pub principal: Principal,
    pub kind: String,
    pub payload_hash: String,
    pub status: u16,
    pub wall_ms: i64,
    /// The response body, minus any one-time credential in it: a journal
    /// that held API keys would be a list of them.
    pub result: Value,
}

/// A new listing, and the event it was recorded as.
#[derive(Serialize)]
pub struct ListingResponse {
    pub quote: crate::market::Quote,
    pub event: EventRecord,
}

/// What a delisting undid, and the event it was recorded as.
#[derive(Serialize)]
pub struct DelistResponse {
    pub delisting: crate::market::Delisting,
    pub event: EventRecord,
}

/// What a dividend paid, and the event it was recorded as.
#[derive(Serialize)]
pub struct DividendResponse {
    pub dividend: crate::market::Dividend,
    pub event: EventRecord,
}

/// What a command did, on its way back out to HTTP.
pub struct Outcome {
    pub status: u16,
    pub body: Value,
    /// The journal sequence the command was written at.
    pub seq: u64,
    /// This is the recorded answer to a command that already ran.
    pub replayed: bool,
}

/// The result of applying a command, before it is recorded.
pub struct Applied {
    status: u16,
    body: Value,
    /// What is safe to keep. Defaults to `body`; a command that hands out a
    /// credential overrides it.
    recorded: Value,
}

impl Applied {
    fn new(status: u16, body: &impl Serialize) -> Result<Self, ApiError> {
        let body = serde_json::to_value(body)
            .map_err(|e| ApiError::internal(format!("a response could not be serialised: {e}")))?;
        Ok(Self {
            status,
            recorded: body.clone(),
            body,
        })
    }

    /// Record this response without the field named: the response is the one
    /// and only time a credential is shown.
    fn without(mut self, field: &str) -> Self {
        if let Some(map) = self.recorded.as_object_mut() {
            map.remove(field);
        }
        self
    }
}

/// The idempotency index: which keys have been used, and what they answered.
///
/// Bounded, oldest evicted first. An evicted key is one whose retry would
/// have to be re-applied rather than replayed, so the cap is a trade between
/// memory and how late a retry may arrive; it is saved with the market
/// because a snapshot may be taken between any two commands.
#[derive(Clone, Debug)]
pub struct CommandLog {
    by_key: BTreeMap<String, CommandRecord>,
    order: VecDeque<String>,
    cap: usize,
}

impl Default for CommandLog {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl CommandLog {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            by_key: BTreeMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// What `key` answered, if it is still held.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&CommandRecord> {
        self.by_key.get(key)
    }

    /// Remember `record`, evicting the oldest key if the log is full.
    pub fn record(&mut self, record: CommandRecord) {
        if self.by_key.contains_key(&record.key) {
            return;
        }
        self.order.push_back(record.key.clone());
        self.by_key.insert(record.key.clone(), record);
        while self.order.len() > self.cap {
            if let Some(evicted) = self.order.pop_front() {
                self.by_key.remove(&evicted);
            }
        }
    }

    /// Oldest first, for a save file.
    #[must_use]
    pub fn records(&self) -> Vec<CommandRecord> {
        self.order
            .iter()
            .filter_map(|k| self.by_key.get(k).cloned())
            .collect()
    }

    /// Rebuild from what [`CommandLog::records`] wrote down.
    pub fn restore(&mut self, records: impl IntoIterator<Item = CommandRecord>) {
        for record in records {
            self.record(record);
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// Why a journal could not be read.
#[derive(Debug)]
pub enum JournalError {
    Io(std::io::Error),
    /// A line that is not an entry, before the end of the file. A torn line
    /// at the very end is a half-written append and is dropped instead.
    Format {
        line: usize,
        error: String,
    },
    Version {
        found: u32,
        expected: u32,
    },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "journal: {e}"),
            Self::Format { line, error } => {
                write!(f, "journal line {line} is not an entry: {error}")
            }
            Self::Version { found, expected } => {
                write!(f, "journal is version {found}, this build reads {expected}")
            }
        }
    }
}

impl std::error::Error for JournalError {}

impl From<std::io::Error> for JournalError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// The first line of a journal file: what wrote it, and where it starts.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Header {
    journal: u32,
    /// The snapshot sequence this file continues from.
    after_seq: u64,
}

/// The append-only file, and the entries in it since the last snapshot.
///
/// Held by the market, so appending is part of the job that made the change
/// and the order on disk is the order things happened in. A journal with no
/// path is disabled: a server with no state file keeps nothing, and there is
/// nothing for a journal to add to a market that is thrown away.
pub struct Journal {
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
    seq: u64,
    /// Entries written since the snapshot the file follows, so truncating
    /// after the next snapshot does not have to read the file back.
    pending: Vec<JournalEntry>,
    /// Appended but not yet flushed to disk.
    unsynced: bool,
    /// Set once an append has failed. The market is then ahead of the disk,
    /// so it refuses to change any further.
    broken: Option<String>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("path", &self.path)
            .field("seq", &self.seq)
            .field("pending", &self.pending.len())
            .field("broken", &self.broken)
            .finish()
    }
}

impl Default for Journal {
    fn default() -> Self {
        Self::disabled()
    }
}

impl Journal {
    /// A journal that keeps nothing. Every append succeeds and nothing
    /// reaches a disk: a server with no state file throws its market away at
    /// shutdown, and there is nothing for a journal to add to that.
    #[must_use]
    pub fn disabled() -> Self {
        Self::at(0)
    }

    /// A journal with no file, continuing after `seq`. What a restored
    /// market starts with until [`crate::market::App::attach_journal`] gives
    /// it a file to append to.
    #[must_use]
    pub fn at(seq: u64) -> Self {
        Self {
            path: None,
            file: None,
            seq,
            pending: Vec::new(),
            unsynced: false,
            broken: None,
        }
    }

    /// Open `path` for appending, continuing after `seq`. An existing file is
    /// left in place: `after_seq` in its header, not this argument, says
    /// where it began, and [`Journal::truncate_to`] rewrites it at the next
    /// snapshot.
    pub fn open(path: &Path, seq: u64) -> Result<Self, JournalError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let fresh = !path.exists() || std::fs::metadata(path)?.len() == 0;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        if fresh {
            let header = Header {
                journal: JOURNAL_VERSION,
                after_seq: seq,
            };
            writeln!(
                file,
                "{}",
                serde_json::to_string(&header).unwrap_or_default()
            )?;
            file.sync_data()?;
        }
        Ok(Self {
            path: Some(path.to_path_buf()),
            file: Some(file),
            seq,
            pending: Vec::new(),
            unsynced: false,
            broken: None,
        })
    }

    /// Whether this journal keeps anything.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// Why the journal stopped taking appends, if it did.
    #[must_use]
    pub fn broken(&self) -> Option<&str> {
        self.broken.as_deref()
    }

    /// Stop taking entries, for `why`.
    ///
    /// What a failed append does to itself, and the only honest response to
    /// one: from here on the market refuses every command, because it can no
    /// longer promise to remember one. Reads carry on. Nothing clears this
    /// but a restart, which replays what did reach the disk.
    pub fn stop(&mut self, why: impl Into<String>) {
        self.broken = Some(why.into());
    }

    /// The sequence of the last entry written.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Entries written since the file's starting point.
    #[must_use]
    pub fn pending(&self) -> &[JournalEntry] {
        &self.pending
    }

    /// Carry the sequence forward to `seq` if it is ahead. Used by replay,
    /// which must not reissue a sequence a journaled entry already has.
    pub(crate) fn reached(&mut self, seq: u64) {
        self.seq = self.seq.max(seq);
    }

    /// Take the next sequence number.
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Write `entry` down. `durable` flushes it to the platter, which is what
    /// makes an acknowledgement mean something; the engine's step does not
    /// need it, because the next durable append flushes it too.
    ///
    /// # Errors
    /// The write or the flush. A failure is remembered: the market is then
    /// ahead of the disk and must not accept anything more.
    pub fn append(&mut self, entry: &JournalEntry, durable: bool) -> Result<(), JournalError> {
        // A journal with no file keeps nothing at all: the market it belongs
        // to is thrown away at shutdown, and holding every entry it ever saw
        // would be a leak the length of the process.
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let line = serde_json::to_string(entry).map_err(|e| JournalError::Format {
            line: 0,
            error: e.to_string(),
        })?;
        let mut write = || -> std::io::Result<()> {
            writeln!(file, "{line}")?;
            if durable {
                file.sync_data()?;
            }
            Ok(())
        };
        if let Err(e) = write() {
            self.stop(e.to_string());
            return Err(JournalError::Io(e));
        }
        self.unsynced = !durable;
        self.pending.push(entry.clone());
        Ok(())
    }

    /// Flush anything appended without a flush of its own.
    pub fn sync(&mut self) -> Result<(), JournalError> {
        if !self.unsynced {
            return Ok(());
        }
        if let Some(file) = self.file.as_mut() {
            file.sync_data()?;
        }
        self.unsynced = false;
        Ok(())
    }

    /// Drop everything up to and including `seq`: a snapshot now holds it.
    ///
    /// The file is rewritten from the entries still in memory, through a
    /// temporary file and a rename, so an interrupted truncation leaves the
    /// previous journal intact.
    pub fn truncate_to(&mut self, seq: u64) -> Result<(), JournalError> {
        self.pending.retain(|e| e.seq > seq);
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        let tmp = path.with_file_name(name);
        {
            let mut file = std::fs::File::create(&tmp)?;
            let header = Header {
                journal: JOURNAL_VERSION,
                after_seq: seq,
            };
            writeln!(
                file,
                "{}",
                serde_json::to_string(&header).unwrap_or_default()
            )?;
            for entry in &self.pending {
                writeln!(file, "{}", serde_json::to_string(entry).unwrap_or_default())?;
            }
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        self.file = Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?,
        );
        self.unsynced = false;
        Ok(())
    }
}

/// Read the entries in a journal file, oldest first.
///
/// A malformed final line is a half-written append — the process stopped
/// between the write and the flush, so nothing was ever acknowledged for it
/// — and is dropped. A malformed line anywhere else is corruption and is
/// refused: a journal that only half-applies is worse than none, which is the
/// same rule the save file follows.
pub fn read(path: &Path) -> Result<Vec<JournalEntry>, JournalError> {
    let file = std::fs::File::open(path)?;
    let mut lines = BufReader::new(file).lines();
    let Some(header) = lines.next().transpose()? else {
        return Ok(Vec::new());
    };
    let header: Header = serde_json::from_str(&header).map_err(|e| JournalError::Format {
        line: 1,
        error: e.to_string(),
    })?;
    if header.journal != JOURNAL_VERSION {
        return Err(JournalError::Version {
            found: header.journal,
            expected: JOURNAL_VERSION,
        });
    }
    let rest: Vec<String> = lines.collect::<Result<_, _>>()?;
    let last = rest.len();
    let mut entries = Vec::with_capacity(last);
    for (i, line) in rest.into_iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalEntry>(&line) {
            Ok(entry) => entries.push(entry),
            Err(e) if i + 1 == last => {
                tracing::warn!(
                    error = %e,
                    "journal ends in a half-written entry; it was never acknowledged, so it is dropped"
                );
            }
            Err(e) => {
                return Err(JournalError::Format {
                    line: i + 2,
                    error: e.to_string(),
                });
            }
        }
    }
    Ok(entries)
}

/// Where the journal for a state file lives: beside it, so the rename that
/// truncates it stays on one filesystem.
#[must_use]
pub fn path_for(state_file: &Path) -> PathBuf {
    let mut name = state_file.file_name().unwrap_or_default().to_os_string();
    name.push(".journal");
    state_file.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Running a command

impl Market {
    /// Apply `command` and write it down, or return the answer it was given
    /// the first time.
    ///
    /// This is the only way anything in the market changes from outside it.
    /// See the module docs for the order the steps run in and why.
    ///
    /// # Errors
    /// Whatever the command was refused with. A refusal changes nothing and
    /// is not journaled.
    pub async fn run_command(
        &mut self,
        principal: Principal,
        idempotency_key: Option<String>,
        at: Timestamp,
        wall_ms: i64,
        command: Command,
    ) -> Result<Outcome, ApiError> {
        if let Some(why) = self.journal.broken() {
            return Err(ApiError::journal_broken(why));
        }
        let payload_hash = command.payload_hash();
        if let Some(key) = idempotency_key.as_deref()
            && let Some(record) = self.commands.get(key)
        {
            if record.payload_hash != payload_hash {
                return Err(ApiError::idempotency_conflict(key));
            }
            return Ok(Outcome {
                status: record.status,
                body: record.result.clone(),
                seq: record.seq,
                replayed: true,
            });
        }
        let applied = self.applying(at, wall_ms, &command).await?;
        let entry = JournalEntry {
            seq: self.journal.next_seq(),
            principal,
            idempotency_key: idempotency_key.clone(),
            payload_hash: payload_hash.clone(),
            at_ms: at.0,
            wall_ms,
            command,
        };
        if let Err(e) = self
            .journal
            .append(&entry, !matches!(entry.command, Command::Step))
        {
            tracing::error!(seq = entry.seq, error = %e, "the journal stopped taking entries");
            return Err(ApiError::journal_broken(&e.to_string()));
        }
        if let Some(key) = idempotency_key {
            self.commands.record(CommandRecord {
                key,
                seq: entry.seq,
                principal,
                kind: entry.command.kind().to_string(),
                payload_hash,
                status: applied.status,
                wall_ms,
                result: applied.recorded,
            });
        }
        Ok(Outcome {
            status: applied.status,
            body: applied.body,
            seq: entry.seq,
            replayed: false,
        })
    }

    /// Apply `command` with the market pinned to `at`, and unpin afterwards
    /// whatever it did.
    async fn applying(
        &mut self,
        at: Timestamp,
        wall_ms: i64,
        command: &Command,
    ) -> Result<Applied, ApiError> {
        self.pinned_now = Some(at);
        let applied = apply(self, wall_ms, command).await;
        self.pinned_now = None;
        applied
    }

    /// Re-apply a journaled command during start-up.
    ///
    /// The same code the live path runs, so a replayed world is the world
    /// that was acknowledged. A refusal here means the entry no longer fits
    /// the state it is being applied to, which is a bug in this module
    /// rather than something a restart can fix: it is logged and skipped so
    /// that one bad entry does not cost every entry after it.
    pub async fn replay(&mut self, entry: JournalEntry) {
        let at = Timestamp(entry.at_ms);
        match self.applying(at, entry.wall_ms, &entry.command).await {
            Ok(applied) => {
                self.journal.reached(entry.seq);
                if let Some(key) = entry.idempotency_key {
                    self.commands.record(CommandRecord {
                        key,
                        seq: entry.seq,
                        principal: entry.principal,
                        kind: entry.command.kind().to_string(),
                        payload_hash: entry.payload_hash,
                        status: applied.status,
                        wall_ms: entry.wall_ms,
                        result: applied.recorded,
                    });
                }
            }
            Err(e) => {
                self.journal.reached(entry.seq);
                tracing::error!(
                    seq = entry.seq,
                    command = entry.command.kind(),
                    error = %e.message(),
                    "a journaled command was refused on replay"
                );
            }
        }
    }
}

/// Apply one command to the market.
///
/// The single implementation of what every mutating route does, shared by
/// the live path and by replay. It runs inside the market's own job, so it
/// may call the symbol actors and nothing else touches a book while it does.
///
/// `wall_ms` is the wall-clock time the command arrived; the simulated
/// instant is already pinned on the market, so [`Market::now`] is the right
/// answer everywhere below.
///
/// # Errors
/// Every refusal a route can give. Nothing has changed when one is returned.
async fn apply(m: &mut Market, wall_ms: i64, command: &Command) -> Result<Applied, ApiError> {
    match command {
        Command::CreateUser {
            name,
            email,
            key_digest,
        } => {
            let id = m.create_user(name.clone(), email.clone(), wall_ms, key_digest.clone());
            let views = m.views().await;
            Applied::new(201, &crate::api::user_dto(m, &views, id)?).map(|a| a.without("api_key"))
        }

        Command::OpenAccount {
            user_id,
            name,
            cash_cents,
        } => {
            let user = UserId(*user_id);
            if !m.users.contains_key(&user) {
                return Err(ApiError::unknown_user(*user_id));
            }
            let id = m
                .open_account(user, name.clone(), *cash_cents, wall_ms)
                .map_err(ApiError::money)?;
            Applied::new(201, &crate::api::account_dto(m, id)?)
        }

        Command::CreateTrader {
            caller,
            user_id,
            account_id,
            name,
            email,
            cash_cents,
            key_digest,
        } => {
            let views = m.views().await;
            let id = match (user_id, account_id) {
                (None, None) => {
                    let digest = key_digest.clone().ok_or_else(|| {
                        ApiError::internal("a sign-up needs a key digest to install")
                    })?;
                    m.sign_up(name.clone(), email.clone(), *cash_cents, wall_ms, digest)
                        .map_err(ApiError::money)?
                }
                (None, Some(_)) => {
                    return Err(ApiError::bad_request(
                        "`account_id` needs the `user_id` that owns it",
                    ));
                }
                (Some(user_id), account_id) => {
                    let user = UserId(*user_id);
                    if !m.users.contains_key(&user) {
                        return Err(ApiError::unknown_user(*user_id));
                    }
                    // Joining an existing user is that user's business alone.
                    if *caller != Some(*user_id) {
                        return Err(ApiError::forbidden("that user is not yours"));
                    }
                    let account = match account_id {
                        Some(account_id) => {
                            let account = AccountId(*account_id);
                            let held = m
                                .accounts
                                .get(&account)
                                .ok_or_else(|| ApiError::unknown_account(*account_id))?;
                            if held.user_id != user {
                                return Err(ApiError::bad_request(format!(
                                    "account {account_id} belongs to user {}",
                                    held.user_id.0
                                )));
                            }
                            account
                        }
                        None => m
                            .open_account(user, name.clone(), *cash_cents, wall_ms)
                            .map_err(ApiError::money)?,
                    };
                    m.create_trader(user, account, name.clone(), wall_ms)
                }
            };
            Applied::new(201, &crate::api::portfolio(m, &views, id)?).map(|a| a.without("api_key"))
        }

        Command::Mint {
            account_id,
            amount_cents,
            memo,
        } => {
            let id = AccountId(*account_id);
            if !m.accounts.contains_key(&id) {
                return Err(ApiError::unknown_account(*account_id));
            }
            let entry = m
                .mint_into(id, *amount_cents, memo.clone(), wall_ms)
                .map_err(ApiError::money)?;
            Applied::new(
                200,
                &crate::account::LedgerResponse {
                    account: crate::api::account_dto(m, id)?,
                    entries: vec![entry],
                },
            )
        }

        Command::Burn {
            account_id,
            amount_cents,
            memo,
        } => {
            let id = AccountId(*account_id);
            if !m.accounts.contains_key(&id) {
                return Err(ApiError::unknown_account(*account_id));
            }
            let entry = m
                .burn_from(id, *amount_cents, memo.clone(), wall_ms)
                .map_err(ApiError::money)?;
            Applied::new(
                200,
                &crate::account::LedgerResponse {
                    account: crate::api::account_dto(m, id)?,
                    entries: vec![entry],
                },
            )
        }

        Command::MintToTrader {
            trader_id,
            amount_cents,
            memo,
        } => {
            let trader = TraderId(*trader_id);
            let views = m.views().await;
            let account_id = m
                .traders
                .get(&trader)
                .ok_or_else(|| ApiError::unknown_trader(*trader_id))?
                .account_id;
            m.mint_into(account_id, *amount_cents, memo.clone(), wall_ms)
                .map_err(ApiError::money)?;
            Applied::new(200, &crate::api::portfolio(m, &views, trader)?)
        }

        Command::SetAccountStatus {
            account_id,
            status,
            owner,
        } => {
            let id = AccountId(*account_id);
            match owner {
                Some(owner) => {
                    crate::api::owned_account(m, crate::api::Caller(UserId(*owner)), id)?
                }
                None => {
                    if !m.accounts.contains_key(&id) {
                        return Err(ApiError::unknown_account(*account_id));
                    }
                }
            }
            match status {
                AccountStatus::Frozen => m.freeze_account(id).await,
                AccountStatus::Active => m.unfreeze_account(id),
                AccountStatus::Closed => m.close_account(id),
            }
            .map_err(ApiError::money)?;
            Applied::new(200, &crate::api::account_dto(m, id)?)
        }

        Command::PlaceOrder { symbol, order } => {
            let trader = TraderId(order.trader_id);
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let request = PlaceRequest {
                order: Order {
                    owner: Owner::Trader(trader),
                    side: order.side,
                    kind: order.kind,
                    tif: order.tif,
                    qty: order.qty,
                },
                client_order_id: order.client_order_id.clone(),
                post_only: order.post_only,
                day: order.day,
                expires_at_ms: order.expires_at_ms,
                display_qty: order.display_qty,
            };
            let placed = m
                .place(&handle, trader, request)
                .await
                .map_err(|e| ApiError::place(symbol, e))?;
            match placed {
                // The same order sent twice, told apart by its
                // `client_order_id` rather than by an idempotency key.
                Placed::Replayed(response) => Applied::new(200, &response),
                Placed::New(response) => Applied::new(201, &response),
            }
        }

        Command::AmendOrder {
            symbol,
            order_id,
            amend,
        } => {
            let trader = TraderId(amend.trader_id);
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let amendment = Amendment {
                order_id: *order_id,
                price_cents: amend.price_cents,
                qty: amend.qty,
                post_only: amend.post_only,
                client_order_id: amend.client_order_id.clone(),
            };
            let amended = m
                .amend(&handle, trader, amendment)
                .await
                .map_err(|e| ApiError::place(symbol, e))?;
            Applied::new(
                200,
                &AmendResponse {
                    replaced_order_id: amended.replaced_order_id,
                    replaced_filled: amended.replaced_filled,
                    order: amended.order,
                },
            )
        }

        Command::CancelOrder {
            symbol,
            trader_id,
            order_id,
        } => {
            let trader = TraderId(*trader_id);
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let sym = handle.ticker;
            let cancelled = m
                .cancel(&handle, trader, *order_id)
                .await
                .map_err(|e| ApiError::cancel(*order_id, e))?
                .ok_or_else(|| ApiError::not_found(symbol))?;
            Applied::new(200, &OpenOrderDto::from_resting(sym, &cancelled))
        }

        Command::CancelAll { trader_id } => {
            Applied::new(200, &m.cancel_all(TraderId(*trader_id)).await)
        }

        Command::PlaceStop { symbol, stop } => {
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let stop = m
                .place_stop(&handle, stop.clone())
                .await
                .map_err(|e| ApiError::place(symbol, e))?;
            Applied::new(201, &stop)
        }

        Command::CancelStop {
            symbol,
            trader_id,
            stop_id,
        } => {
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let stop = m
                .cancel_stop(&handle, TraderId(*trader_id), *stop_id)
                .await
                .ok_or_else(|| ApiError::unknown_stop(*stop_id))?;
            Applied::new(200, &stop)
        }

        Command::ListSymbol { listing } => apply_listing(m, wall_ms, listing).await,

        Command::Delist {
            symbol,
            cents_per_share,
            source,
            note,
        } => {
            let at = m.now();
            let delisting = m
                .delist(symbol, *cents_per_share, note.clone(), at.0)
                .await
                .map_err(|e| match e {
                    DelistError::Unknown => ApiError::not_found(symbol),
                    DelistError::Price => ApiError::invalid_event(e.to_string()),
                    DelistError::Unfunded(_) => ApiError::payout_not_funded(e.to_string()),
                })?;
            let event = m.record(EventRecord {
                id: 0,
                received_at_ms: wall_ms,
                at_ms: at.0,
                symbols: vec![delisting.symbol],
                kind: "corporate:delisting".into(),
                source: source.clone(),
                note: note.clone(),
                magnitude: None,
                effects: Vec::new(),
                summary: vec![format!(
                    "{} delisted at {} cents a share: {} shares bought out for {} cents \
                     across {} account(s), {} resting order(s) and {} stop(s) withdrawn",
                    delisting.symbol,
                    delisting.cents_per_share,
                    delisting.shares_bought_out,
                    delisting.total_cents,
                    delisting.accounts_paid,
                    delisting.orders_cancelled,
                    delisting.stops_cancelled,
                )],
            });
            Applied::new(202, &DelistResponse { delisting, event })
        }

        Command::Dividend {
            symbol,
            cents_per_share,
            source,
            note,
        } => {
            let at = m.now();
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let price = handle
                .ask_listed(|s| s.price_cents())
                .await?
                .ok_or_else(|| ApiError::not_found(handle.ticker))?;
            let (paid, effects) = m
                .pay_dividend(&handle, *cents_per_share, note.clone(), at.0)
                .await
                .map_err(ApiError::payout)?
                .ok_or_else(|| {
                    ApiError::invalid_event(format!(
                        "a dividend must be between 1 and {} cents a share, one less than \
                         the price it is declared against",
                        price - 1
                    ))
                })?;
            let event = m.record(EventRecord {
                id: 0,
                received_at_ms: wall_ms,
                at_ms: at.0,
                symbols: vec![paid.symbol],
                kind: "corporate:dividend".into(),
                source: source.clone(),
                note: note.clone(),
                magnitude: Some(paid.cents_per_share as f64 / 100.0),
                effects,
                summary: vec![format!(
                    "dividend of {} cents a share on {} shares, {} cents to {} account(s)",
                    paid.cents_per_share, paid.shares_paid, paid.total_cents, paid.accounts_paid
                )],
            });
            Applied::new(
                202,
                &DividendResponse {
                    dividend: paid,
                    event,
                },
            )
        }

        Command::Halt { symbol } => {
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let status = m
                .halt(&handle)
                .await
                .ok_or_else(|| ApiError::not_found(symbol))?;
            Applied::new(200, &status)
        }

        Command::Resume { symbol } => {
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let status = m
                .resume(&handle)
                .await
                .ok_or_else(|| ApiError::not_found(symbol))?;
            Applied::new(200, &status)
        }

        Command::SimEvent {
            symbol,
            event,
            source,
            note,
        } => {
            let at = m.now();
            let prepared = event
                .prepare()
                .map_err(|e| ApiError::invalid_event(e.to_string()))?;
            let handle = m
                .symbol(symbol)
                .cloned()
                .ok_or_else(|| ApiError::not_found(symbol))?;
            let ticker = handle.ticker;
            m.apply_events(&handle, vec![prepared], at)
                .await?
                .ok_or_else(|| ApiError::not_found(ticker))?
                .map_err(ApiError::invalid_event)?;
            let record = m.record(EventRecord {
                id: 0,
                received_at_ms: wall_ms,
                at_ms: at.0,
                symbols: vec![ticker],
                kind: format!("sim:{}", sim_event_name(event)),
                source: source.clone(),
                note: note.clone(),
                magnitude: None,
                effects: vec![*event],
                summary: vec![event.summary()],
            });
            Applied::new(202, &record)
        }

        Command::GameEvent {
            kind,
            magnitude,
            symbol,
            source,
            note,
        } => {
            let at = m.now();
            if !(magnitude.is_finite() && *magnitude > 0.0 && *magnitude <= MAX_MAGNITUDE) {
                return Err(ApiError::invalid_event(format!(
                    "`magnitude` must be in (0, {MAX_MAGNITUDE}]"
                )));
            }
            let effects = kind.effects(*magnitude);
            let prepared: Vec<Prepared> = effects
                .iter()
                .map(|e| {
                    e.prepare()
                        .map_err(|e| ApiError::invalid_event(e.to_string()))
                })
                .collect::<Result<_, _>>()?;
            let symbols = match kind.scope() {
                Scope::Market => m
                    .apply_events_everywhere(prepared, at)
                    .await
                    .map_err(ApiError::invalid_event)?,
                Scope::Company => {
                    let sym = symbol.as_deref().ok_or_else(|| {
                        ApiError::bad_request("`symbol` is required for a company-scoped event")
                    })?;
                    let handle = m
                        .symbol(sym)
                        .cloned()
                        .ok_or_else(|| ApiError::not_found(sym))?;
                    m.apply_events(&handle, prepared, at)
                        .await?
                        .ok_or_else(|| ApiError::not_found(handle.ticker))?
                        .map_err(ApiError::invalid_event)?;
                    vec![handle.ticker]
                }
            };
            let record = m.record(EventRecord {
                id: 0,
                received_at_ms: wall_ms,
                at_ms: at.0,
                symbols,
                kind: format!("game:{}", game_kind_name(*kind)),
                source: source.clone(),
                note: note.clone(),
                magnitude: Some(*magnitude),
                summary: effects.iter().map(SimEvent::summary).collect(),
                effects,
            });
            Applied::new(202, &record)
        }

        Command::Step => {
            let target = m.now();
            let ticks = m.step(target).await;
            Applied::new(200, &serde_json::json!({ "ticks": ticks }))
        }
    }
}

/// List a symbol: warm its simulator up and hand it to the market.
///
/// The warm-up runs here, inside the market's job, rather than on the
/// request's own task as it used to. That costs a listing request the time
/// it takes to generate its history, during which nothing else changes the
/// market — but it is what makes the listing a command like any other, and
/// so replayable. Listing is rare and an operator's; a fill is neither.
async fn apply_listing(
    m: &mut Market,
    wall_ms: i64,
    listing: &Listing,
) -> Result<Applied, ApiError> {
    let symbol = crate::symbols::register(&listing.symbol)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if listing.shares_outstanding == 0 {
        return Err(ApiError::bad_request(
            "a listing needs shares: shares_outstanding must be positive",
        ));
    }
    let at = m.now();
    if m.symbol(symbol).is_some() {
        return Err(ApiError::conflict(format!("{symbol} is already listed")));
    }
    let spec = SymbolSpec {
        info: SymbolInfo {
            symbol,
            name: listing.name.clone(),
            sector: listing.sector.clone(),
            description: listing.description.clone(),
            shares_outstanding: listing.shares_outstanding,
            seed: listing.seed,
        },
        config: fehu::Config {
            start_price_cents: listing.start_price_cents,
            drift: listing.drift,
            volatility: listing.volatility,
            ..fehu::Config::default()
        },
        trading: fehu::TradingParams::default(),
    };
    let state = m
        .prepare_listing(spec, listing.history_days, at)
        .map_err(|e| ApiError::invalid_event(e.to_string()))?;
    let quote = m
        .list(state)
        .map_err(|e| ApiError::conflict(e.to_string()))?;
    let event = m.record(EventRecord {
        id: 0,
        received_at_ms: wall_ms,
        at_ms: at.0,
        symbols: vec![symbol],
        kind: "corporate:listing".into(),
        source: listing.source.clone(),
        note: listing.note.clone(),
        magnitude: None,
        effects: Vec::new(),
        summary: vec![format!(
            "{symbol} listed at {} cents, {} shares outstanding",
            quote.price_cents, quote.shares_outstanding
        )],
    });
    Applied::new(201, &ListingResponse { quote, event })
}

fn sim_event_name(e: &SimEvent) -> &'static str {
    match e {
        SimEvent::Jump { .. } => "jump",
        SimEvent::DriftShift { .. } => "drift_shift",
        SimEvent::DriftForTotalMove { .. } => "drift_for_total_move",
        SimEvent::VolShift { .. } => "vol_shift",
        SimEvent::FundamentalShift { .. } => "fundamental_shift",
        SimEvent::FundamentalTarget { .. } => "fundamental_target",
    }
}

fn game_kind_name(kind: GameEventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A seed from a ticker, so a listing without one is still reproducible: the
/// same ticker on two servers gives the same company. FNV-1a, which is
/// nothing but a spread of the letters — it is a seed, not a digest.
#[must_use]
pub fn seed_from_ticker(ticker: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in ticker.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The wall-clock reading a command is stamped with. One place, so a test
/// can see what a live request would have written.
#[must_use]
pub fn now_ms() -> i64 {
    wall_now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of our own, removed when the test ends. A dependency for
    /// this would be a dependency in the shipped binary.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path =
                std::env::temp_dir().join(format!("{prefix}-{unique}-{}", std::process::id()));
            std::fs::create_dir_all(&path).expect("a temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(seq: u64, command: Command) -> JournalEntry {
        JournalEntry {
            seq,
            principal: Principal::Operator,
            idempotency_key: Some(format!("key-{seq}")),
            payload_hash: command.payload_hash(),
            at_ms: 1_000 * seq as i64,
            wall_ms: 5,
            command,
        }
    }

    fn halt(symbol: &str) -> Command {
        Command::Halt {
            symbol: symbol.to_string(),
        }
    }

    #[test]
    fn entries_round_trip_through_the_file() {
        let dir = TempDir::new("fehu-journal");
        let path = dir.path().join("state.json.journal");
        let mut journal = Journal::open(&path, 7).unwrap();
        journal.append(&entry(8, halt("ACME")), true).unwrap();
        journal.append(&entry(9, Command::Step), false).unwrap();
        journal.sync().unwrap();
        let back = read(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].seq, 8);
        assert_eq!(back[0].command.kind(), "halt");
        assert_eq!(back[1].command.kind(), "step");
        assert_eq!(back[1].at_ms, 9_000);
    }

    #[test]
    fn a_half_written_last_line_is_dropped_and_an_earlier_one_is_not() {
        let dir = TempDir::new("fehu-journal");
        let path = dir.path().join("j");
        let mut journal = Journal::open(&path, 0).unwrap();
        journal.append(&entry(1, halt("ACME")), true).unwrap();
        drop(journal);
        let torn = format!(
            "{}\n{{\"seq\":2,\"princ",
            std::fs::read_to_string(&path).unwrap().trim_end()
        );
        std::fs::write(&path, &torn).unwrap();
        assert_eq!(read(&path).unwrap().len(), 1, "the torn tail is dropped");

        let corrupt = format!(
            "{torn}\n{}",
            serde_json::to_string(&entry(3, halt("ACME"))).unwrap()
        );
        std::fs::write(&path, corrupt).unwrap();
        assert!(
            matches!(read(&path), Err(JournalError::Format { .. })),
            "a bad line in the middle is corruption, not a torn write"
        );
    }

    #[test]
    fn truncating_keeps_only_what_the_snapshot_missed() {
        let dir = TempDir::new("fehu-journal");
        let path = dir.path().join("j");
        let mut journal = Journal::open(&path, 0).unwrap();
        for seq in 1..=4 {
            journal.append(&entry(seq, halt("ACME")), true).unwrap();
        }
        journal.truncate_to(2).unwrap();
        let kept = read(&path).unwrap();
        assert_eq!(
            kept.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3, 4],
            "everything the snapshot holds is gone"
        );
        journal.append(&entry(5, halt("ACME")), true).unwrap();
        assert_eq!(read(&path).unwrap().len(), 3, "and appending still works");
    }

    #[test]
    fn a_journal_from_another_version_is_refused() {
        let dir = TempDir::new("fehu-journal");
        let path = dir.path().join("j");
        std::fs::write(&path, "{\"journal\":99,\"after_seq\":0}\n").unwrap();
        assert!(matches!(
            read(&path),
            Err(JournalError::Version { found: 99, .. })
        ));
    }

    #[test]
    fn the_payload_hash_tells_a_retry_from_a_different_request() {
        assert_eq!(halt("ACME").payload_hash(), halt("ACME").payload_hash());
        assert_ne!(halt("ACME").payload_hash(), halt("NBLA").payload_hash());
    }

    #[test]
    fn a_key_this_server_generated_is_not_part_of_what_was_asked_for() {
        let signup = |digest: &str| Command::CreateUser {
            name: Some("ada".into()),
            email: None,
            key_digest: digest.to_string(),
        };
        assert_eq!(
            signup("sha256:aaa").payload_hash(),
            signup("sha256:bbb").payload_hash(),
            "a retried sign-up carries a fresh key, and is still the same request"
        );
        let other = Command::CreateUser {
            name: Some("bea".into()),
            email: None,
            key_digest: "sha256:aaa".into(),
        };
        assert_ne!(signup("sha256:aaa").payload_hash(), other.payload_hash());
    }

    #[test]
    fn the_command_log_evicts_the_oldest_key() {
        let mut log = CommandLog::new(2);
        for i in 0..3 {
            log.record(CommandRecord {
                key: format!("k{i}"),
                seq: i,
                principal: Principal::Anonymous,
                kind: "halt".into(),
                payload_hash: "sha256:x".into(),
                status: 200,
                wall_ms: 0,
                result: Value::Null,
            });
        }
        assert_eq!(log.len(), 2);
        assert!(log.get("k0").is_none(), "the oldest key went");
        assert!(log.get("k2").is_some());
        assert_eq!(log.records().len(), 2);
    }

    #[test]
    fn a_disabled_journal_keeps_nothing_and_refuses_nothing() {
        let mut journal = Journal::disabled();
        assert!(!journal.is_enabled());
        assert!(journal.append(&entry(1, Command::Step), true).is_ok());
        assert!(journal.pending().is_empty(), "and holds on to nothing");
        assert!(journal.broken().is_none());
        assert!(journal.truncate_to(1).is_ok());
    }
}
