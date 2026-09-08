//! Recipes and jobs: the world turning things into other things.
//!
//! The catalogue ([`crate::catalog`]) is the world *selling* a unit of a
//! good. A recipe is the world *making* one: an operator writes down that
//! two units of `ORE` and a few cents of furnace time become one `INGOT`
//! after a while, and a player who holds the ore and the cents starts a job
//! that says so. Nothing else in the server creates a unit of a good except
//! a catalogue purchase and an NPC endowment, and this is the third and last
//! of them.
//!
//! # A job takes what it needs up front
//!
//! Starting a job consumes its inputs and posts its cost in the same
//! command. It holds nothing back and reserves nothing: there is no
//! outstanding claim on a wallet or a position with a job behind it, which
//! is the invariant the acceptance scenario checks by asking whether any
//! hold has no order behind it. The alternative — reserving the inputs for
//! the duration — buys nothing a player can see and costs a reservation
//! kind that every audit would have to learn.
//!
//! What that buys is the ordinary refusal shape the rest of the server
//! uses: everything is checked, then the money moves, then the units do, and
//! a refusal leaves the world exactly as it found it.
//!
//! # A job is delivered by the clock it was started under
//!
//! [`Job::outputs`] is resolved when the job starts, not when it completes.
//! The recipe an operator rewrites afterwards does not change what a running
//! job will deliver, and neither does a game event that lands halfway
//! through: what the player was told they would get is what arrives. The
//! recipe's own version is recorded beside it so an audit can still say
//! which text the job was started under.
//!
//! The plan called a job's duration a number of *ticks*. A tick belongs to a
//! symbol — each simulator has its own interval — and a job belongs to no
//! symbol, so the duration here is in **simulated seconds** and a job is due
//! at an instant. The engine step completes everything due at the instant it
//! is advancing to, which is a field of the journal entry, so a replayed
//! step delivers the same jobs at the same moment.
//!
//! # Determinism
//!
//! Nothing here reads a clock or draws a random number. Starting a job takes
//! its instant from the command being applied, completion takes it from the
//! step, and a game event's effect on the yield is an integer ramp — so a
//! replayed world runs the same jobs to the same units.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use fehu::TraderId;

use crate::account::AccountId;
use crate::save::Symbol;

/// Recipes one world may hold.
pub const MAX_RECIPES: usize = 256;

/// Inputs, or outputs, one recipe may name.
pub const MAX_RECIPE_LINES: usize = 8;

/// The longest note a recipe carries.
pub const MAX_RECIPE_NOTE: usize = 140;

/// The longest a recipe id may be.
pub const MAX_RECIPE_ID: usize = 24;

/// The longest a recipe may take, in simulated seconds: thirty days.
pub const MAX_DURATION_SECS: u64 = 30 * 24 * 3_600;

/// Units one line of a recipe may name.
pub const MAX_LINE_QTY: u64 = 1_000_000;

/// Jobs the world will have in the furnace at once.
///
/// Every one of them is looked at on every engine step, so this is a bound
/// on how much work a step is as much as on how busy the world is.
pub const MAX_RUNNING_JOBS: usize = 1_024;

/// Basis points in one.
pub const BPS: i64 = 10_000;

/// How far a game event may push a yield: a quarter of the recipe, or four
/// times it. A multiplier outside this is clamped rather than refused — an
/// operator stacking events should get an unusually good day, not a world
/// that makes nothing or everything.
pub const MIN_YIELD_BPS: i64 = 2_500;
/// The other end of [`MIN_YIELD_BPS`].
pub const MAX_YIELD_BPS: i64 = 40_000;

/// One side of a recipe: how many units of what.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
// The ticker is registered on the way in rather than borrowed from the
// input, so the derive needs no `'de: 'static`.
#[serde(bound(deserialize = ""))]
pub struct Line {
    /// The good. Always a listed [`Good`](crate::symbol::AssetKind::Good):
    /// shares are floated, not made to order.
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    /// Units of it.
    pub qty: u64,
}

/// What the world knows how to make, and what making it takes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(deserialize = ""))]
pub struct Recipe {
    /// A short name, uppercased: `SMELT`.
    pub id: String,
    /// Bumped every time an operator rewrites the recipe, so a job started
    /// under an earlier text can still say which one it was.
    pub version: u32,
    /// Units consumed when the job starts.
    pub inputs: Vec<Line>,
    /// Units issued when it completes, before any event effect on the yield.
    pub outputs: Vec<Line>,
    /// What the furnace charges, in cents. Paid to the venue when the job
    /// starts; nothing is created or destroyed by it.
    pub cost_cents: i64,
    /// How long it takes, in simulated seconds.
    pub duration_secs: u64,
    /// What a cancellation gives back, in basis points of the cost. Zero by
    /// default: the inputs are already in the crucible and the furnace was
    /// lit.
    pub refund_bps: u32,
    /// What the recipe is, for whoever reads the book.
    pub note: Option<String>,
}

/// Why a recipe or a job was refused.
///
/// Every one of these is checked before anything moves: a refused job leaves
/// its inputs where they were and its owner's wallet untouched.
#[derive(Clone, Debug)]
pub enum JobError {
    /// The book is full ([`MAX_RECIPES`]).
    Full,
    /// No recipe by that name.
    UnknownRecipe(String),
    /// No such job.
    UnknownJob(u64),
    /// The recipe as written does not make sense.
    Recipe(String),
    /// No such trader.
    UnknownTrader(u64),
    /// The symbol is not listed, or its actor is gone.
    Unknown(String),
    /// The symbol is a stock: shares are floated and traded, not made.
    NotAGood(String),
    /// The owner does not hold that many units free of reservations.
    InsufficientUnits {
        /// The good.
        symbol: String,
        /// What the recipe takes.
        needed: u64,
        /// What is held and unreserved.
        available: u64,
    },
    /// The ledger would not move the money.
    Money(crate::account::MoneyError),
    /// The job has already finished, so there is nothing to cancel.
    NotRunning(u64),
    /// The world is running as many jobs as it will.
    TooManyJobs(usize),
    /// A count that will not fit.
    Quantity(String),
}

impl From<crate::account::MoneyError> for JobError {
    fn from(e: crate::account::MoneyError) -> Self {
        Self::Money(e)
    }
}

impl From<fehu::ledger::LedgerError> for JobError {
    fn from(e: fehu::ledger::LedgerError) -> Self {
        Self::Money(crate::account::MoneyError::Ledger(e))
    }
}

impl From<crate::actor::Gone> for JobError {
    fn from(_: crate::actor::Gone) -> Self {
        Self::Unknown("the symbol is no longer listed".into())
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "the world holds {MAX_RECIPES} recipes already"),
            Self::UnknownRecipe(id) => write!(f, "no recipe {id}"),
            Self::UnknownJob(id) => write!(f, "no job {id}"),
            Self::Recipe(why) => write!(f, "{why}"),
            Self::UnknownTrader(id) => write!(f, "no trader {id}"),
            Self::Unknown(sym) => write!(f, "{sym} is not listed"),
            Self::NotAGood(sym) => write!(
                f,
                "{sym} is a stock: its shares are traded, not made to order"
            ),
            Self::InsufficientUnits {
                symbol,
                needed,
                available,
            } => write!(
                f,
                "insufficient units of {symbol}: need {needed}, hold {available} free of reservations"
            ),
            Self::Money(e) => write!(f, "{e}"),
            Self::NotRunning(id) => write!(f, "job {id} has already finished"),
            Self::TooManyJobs(cap) => write!(f, "{cap} jobs are already running"),
            Self::Quantity(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for JobError {}

/// Clean a recipe id: trimmed, uppercased, and made of the characters a
/// ticker is made of.
///
/// # Errors
/// A message saying what was wrong with it.
pub fn clean_id(id: &str) -> Result<String, JobError> {
    let id = id.trim().to_ascii_uppercase();
    if id.is_empty() || id.len() > MAX_RECIPE_ID {
        return Err(JobError::Recipe(format!(
            "a recipe id is 1 to {MAX_RECIPE_ID} characters"
        )));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(JobError::Recipe(
            "a recipe id is letters, digits, '-' and '_'".into(),
        ));
    }
    Ok(id)
}

/// Every recipe, by id.
///
/// Ordered by id, like every other keyed map in the server, so what the
/// endpoint returns is stable between calls and between runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(bound(deserialize = ""), transparent)]
pub struct RecipeBook {
    recipes: BTreeMap<String, Recipe>,
}

impl RecipeBook {
    /// Write or replace a recipe.
    ///
    /// Replacing bumps the version. A job already running keeps the outputs
    /// it was started with, so rewriting a recipe changes what the *next*
    /// job makes and nothing that is already in the furnace.
    ///
    /// # Errors
    /// [`JobError::Recipe`] for anything wrong with the text,
    /// [`JobError::Full`] once the book is at [`MAX_RECIPES`].
    // A recipe is seven numbers and two lists; a struct to carry them to
    // their own setter would be the same fields with one more name.
    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &mut self,
        id: String,
        inputs: Vec<Line>,
        outputs: Vec<Line>,
        cost_cents: i64,
        duration_secs: u64,
        refund_bps: u32,
        note: Option<String>,
    ) -> Result<&Recipe, JobError> {
        if outputs.is_empty() {
            return Err(JobError::Recipe("a recipe makes at least one thing".into()));
        }
        if inputs.len() > MAX_RECIPE_LINES || outputs.len() > MAX_RECIPE_LINES {
            return Err(JobError::Recipe(format!(
                "a recipe names at most {MAX_RECIPE_LINES} inputs and {MAX_RECIPE_LINES} outputs"
            )));
        }
        for line in inputs.iter().chain(outputs.iter()) {
            if line.qty == 0 || line.qty > MAX_LINE_QTY {
                return Err(JobError::Recipe(format!(
                    "a recipe line is 1 to {MAX_LINE_QTY} units"
                )));
            }
        }
        if let Some(dup) = first_duplicate(&inputs).or_else(|| first_duplicate(&outputs)) {
            return Err(JobError::Recipe(format!(
                "{dup} is named twice on the same side; give it one line"
            )));
        }
        if cost_cents < 0 {
            return Err(JobError::Recipe(
                "a recipe cannot cost less than nothing".into(),
            ));
        }
        if duration_secs > MAX_DURATION_SECS {
            return Err(JobError::Recipe(format!(
                "a recipe takes at most {MAX_DURATION_SECS} simulated seconds"
            )));
        }
        if i64::from(refund_bps) > BPS {
            return Err(JobError::Recipe(
                "a refund is at most the whole cost (10000 bps)".into(),
            ));
        }
        let version = self.recipes.get(&id).map_or(0, |r| r.version) + 1;
        if !self.recipes.contains_key(&id) && self.recipes.len() >= MAX_RECIPES {
            return Err(JobError::Full);
        }
        self.recipes.insert(
            id.clone(),
            Recipe {
                id: id.clone(),
                version,
                inputs,
                outputs,
                cost_cents,
                duration_secs,
                refund_bps,
                note,
            },
        );
        Ok(&self.recipes[&id])
    }

    /// Take a recipe out of the book. Jobs already running finish: they carry
    /// what they will deliver.
    pub fn remove(&mut self, id: &str) -> Option<Recipe> {
        self.recipes.remove(id)
    }

    /// The recipe by that id, if there is one.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Recipe> {
        self.recipes.get(id)
    }

    /// Every recipe, ordered by id.
    pub fn recipes(&self) -> impl Iterator<Item = &Recipe> {
        self.recipes.values()
    }

    /// Recipes written.
    #[must_use]
    pub fn len(&self) -> usize {
        self.recipes.len()
    }

    /// Nothing can be made.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }
}

/// The first symbol named twice in `lines`, if any.
fn first_duplicate(lines: &[Line]) -> Option<Symbol> {
    for (i, line) in lines.iter().enumerate() {
        if lines[..i].iter().any(|other| other.symbol == line.symbol) {
            return Some(line.symbol);
        }
    }
    None
}

/// Where a job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Started, not yet due.
    Running,
    /// Delivered.
    Done,
    /// Stopped by its owner before it was due.
    Cancelled,
}

impl JobStatus {
    /// A short, stable name for logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One run of a recipe.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(deserialize = ""))]
pub struct Job {
    pub id: u64,
    /// The recipe it runs, and the version of it that was current when it
    /// started.
    pub recipe: String,
    pub recipe_version: u32,
    /// Who started it.
    pub trader_id: u64,
    /// The account the cost came out of and any refund goes back to.
    pub account_id: u64,
    /// What it took, and what it will deliver. Resolved at the start: see
    /// the module docs.
    pub inputs: Vec<Line>,
    pub outputs: Vec<Line>,
    /// The yield the world was running at when it started, in basis points.
    /// `10000` is the recipe as written.
    pub yield_bps: i64,
    /// What it cost, and the balanced transaction that moved it.
    pub cost_cents: i64,
    pub tx_id: u64,
    /// What the inputs cost their owner, taken out of their positions when
    /// the job started.
    ///
    /// It is not money that moved — the ore was already paid for — but it is
    /// what the ore was worth, and it goes into what the ingot is reckoned to
    /// have cost. See [`Trader::withdraw`](crate::trading::Trader::withdraw).
    #[serde(default)]
    pub inputs_cost_cents: i64,
    /// Simulated instants: when it started and when it is due.
    pub started_at_ms: i64,
    pub due_at_ms: i64,
    /// Simulated instant it was delivered or cancelled.
    pub finished_at_ms: Option<i64>,
    pub status: JobStatus,
    /// What a cancellation actually paid back. `0` on a running or delivered
    /// job.
    pub refunded_cents: i64,
}

impl Job {
    /// Whether the job is due at `now_ms`.
    #[must_use]
    pub fn is_due(&self, now_ms: i64) -> bool {
        self.status == JobStatus::Running && now_ms >= self.due_at_ms
    }
}

/// Jobs the world will hold at once, running and finished.
///
/// Finished jobs are evicted oldest first; a running one is never evicted,
/// because it is a promise the world has taken money for.
pub const DEFAULT_JOB_LOG: usize = 2_000;

/// Every job, by id, and the counter that names the next one.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(deserialize = ""))]
pub struct JobBook {
    jobs: BTreeMap<u64, Job>,
    next_id: u64,
    /// How many jobs are kept. Not saved with the book: it is an option of
    /// the world, not a fact about it.
    #[serde(skip, default = "default_cap")]
    cap: usize,
}

fn default_cap() -> usize {
    DEFAULT_JOB_LOG
}

impl Default for JobBook {
    fn default() -> Self {
        Self {
            jobs: BTreeMap::new(),
            next_id: 1,
            cap: DEFAULT_JOB_LOG,
        }
    }
}

impl JobBook {
    /// A book that keeps `cap` finished jobs.
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            ..Self::default()
        }
    }

    /// Set how many finished jobs are kept, without disturbing the jobs.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        self.evict();
    }

    /// The id the next job will take.
    #[must_use]
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Take the next id.
    pub fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// File a job. It is running by the time it gets here.
    pub fn insert(&mut self, job: Job) {
        self.jobs.insert(job.id, job);
        self.evict();
    }

    /// The job by that id.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Job> {
        self.jobs.get(&id)
    }

    /// The job by that id, to change.
    pub fn get_mut(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.get_mut(&id)
    }

    /// Every job, oldest first.
    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    /// Jobs of one trader, oldest first.
    pub fn of_trader(&self, trader: TraderId) -> impl Iterator<Item = &Job> {
        self.jobs.values().filter(move |j| j.trader_id == trader.0)
    }

    /// The ids of every job due at `now_ms`, oldest first.
    #[must_use]
    pub fn due(&self, now_ms: i64) -> Vec<u64> {
        self.jobs
            .values()
            .filter(|j| j.is_due(now_ms))
            .map(|j| j.id)
            .collect()
    }

    /// How many jobs are still running.
    #[must_use]
    pub fn running(&self) -> usize {
        self.jobs
            .values()
            .filter(|j| j.status == JobStatus::Running)
            .count()
    }

    /// Jobs held, running and finished.
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// No job has ever been started.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Rebuild a book from a snapshot.
    #[must_use]
    pub fn from_saved(jobs: Vec<Job>, next_id: u64, cap: usize) -> Self {
        let next = jobs
            .iter()
            .map(|j| j.id + 1)
            .max()
            .unwrap_or(1)
            .max(next_id);
        let mut book = Self {
            jobs: jobs.into_iter().map(|j| (j.id, j)).collect(),
            next_id: next,
            cap: cap.max(1),
        };
        book.evict();
        book
    }

    /// Every job, for the snapshot.
    #[must_use]
    pub fn to_saved(&self) -> Vec<Job> {
        self.jobs.values().cloned().collect()
    }

    /// Drop finished jobs beyond the cap, oldest first.
    fn evict(&mut self) {
        while self.jobs.len() > self.cap {
            let Some(oldest) = self
                .jobs
                .values()
                .find(|j| j.status != JobStatus::Running)
                .map(|j| j.id)
            else {
                // Every job is running: the cap gives way rather than a
                // promise the world has taken money for.
                return;
            };
            self.jobs.remove(&oldest);
        }
    }
}

/// Scale `qty` by `yield_bps`, rounding down but never to nothing.
///
/// A line that makes something makes at least one unit: a bad day is a
/// smaller batch, not a furnace that takes the ore and returns air.
#[must_use]
pub fn scaled_qty(qty: u64, yield_bps: i64) -> u64 {
    if qty == 0 {
        return 0;
    }
    let bps = yield_bps.clamp(MIN_YIELD_BPS, MAX_YIELD_BPS);
    let scaled = u128::from(qty).saturating_mul(u128::try_from(bps).unwrap_or(0))
        / u128::try_from(BPS).unwrap_or(1);
    u64::try_from(scaled).unwrap_or(u64::MAX).max(1)
}

/// `GET /api/recipes`: what the world knows how to make.
#[derive(Debug, Serialize)]
pub struct RecipesResponse {
    pub recipes: Vec<Recipe>,
}

/// `GET /api/jobs`: what is in the furnace.
#[derive(Debug, Serialize)]
pub struct JobsResponse {
    pub jobs: Vec<Job>,
}

/// What a completed job delivered.
#[derive(Clone, Debug, Serialize)]
#[serde(bound(deserialize = ""))]
pub struct JobDelivery {
    pub job_id: u64,
    pub trader_id: u64,
    pub recipe: String,
    /// What actually arrived. A line whose good was delisted while the job
    /// ran delivers nothing and is not listed.
    pub delivered: Vec<Line>,
    /// Simulated instant it was delivered.
    pub at_ms: i64,
}

/// The account a job's money moves through, resolved once at the start.
#[derive(Clone, Copy, Debug)]
pub struct JobOwner {
    pub trader: TraderId,
    pub account: AccountId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(symbol: &'static str, qty: u64) -> Line {
        Line { symbol, qty }
    }

    #[test]
    fn a_recipe_id_is_cleaned_like_a_ticker() {
        assert_eq!(clean_id(" smelt ").unwrap(), "SMELT");
        assert!(clean_id("").is_err());
        assert!(clean_id("a b").is_err());
        assert!(clean_id(&"x".repeat(MAX_RECIPE_ID + 1)).is_err());
    }

    #[test]
    fn a_recipe_makes_something_out_of_lines_that_add_up() {
        let mut book = RecipeBook::default();
        assert!(
            book.set("SMELT".into(), vec![], vec![], 0, 0, 0, None)
                .is_err(),
            "a recipe that makes nothing is not a recipe"
        );
        assert!(
            book.set(
                "SMELT".into(),
                vec![line("ORE", 0)],
                vec![line("INGOT", 1)],
                0,
                0,
                0,
                None
            )
            .is_err(),
            "a line of no units is not a line"
        );
        assert!(
            book.set(
                "SMELT".into(),
                vec![line("ORE", 1), line("ORE", 2)],
                vec![line("INGOT", 1)],
                0,
                0,
                0,
                None
            )
            .is_err(),
            "one good, one line"
        );
        let recipe = book
            .set(
                "SMELT".into(),
                vec![line("ORE", 2)],
                vec![line("INGOT", 1)],
                500,
                60,
                0,
                None,
            )
            .expect("a recipe that adds up");
        assert_eq!(recipe.version, 1);
    }

    #[test]
    fn rewriting_a_recipe_bumps_its_version() {
        let mut book = RecipeBook::default();
        book.set(
            "SMELT".into(),
            vec![line("ORE", 2)],
            vec![line("INGOT", 1)],
            500,
            60,
            0,
            None,
        )
        .unwrap();
        let again = book
            .set(
                "SMELT".into(),
                vec![line("ORE", 3)],
                vec![line("INGOT", 1)],
                500,
                60,
                0,
                None,
            )
            .unwrap();
        assert_eq!(again.version, 2);
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn a_yield_rounds_down_but_never_to_nothing() {
        assert_eq!(scaled_qty(10, 10_000), 10);
        assert_eq!(scaled_qty(10, 15_000), 15);
        assert_eq!(scaled_qty(10, 4_900), 4);
        assert_eq!(scaled_qty(1, 2_500), 1, "a bad day is a smaller batch");
        assert_eq!(scaled_qty(0, 20_000), 0);
        assert_eq!(
            scaled_qty(10, 1_000_000),
            40,
            "and a good one is bounded too"
        );
    }

    #[test]
    fn a_running_job_is_never_evicted() {
        let mut book = JobBook::with_cap(1);
        let mut running = Job {
            id: 0,
            recipe: "SMELT".into(),
            recipe_version: 1,
            trader_id: 1,
            account_id: 1,
            inputs: vec![],
            outputs: vec![line("INGOT", 1)],
            yield_bps: BPS,
            cost_cents: 0,
            tx_id: 0,
            inputs_cost_cents: 0,
            started_at_ms: 0,
            due_at_ms: 1_000,
            finished_at_ms: None,
            status: JobStatus::Running,
            refunded_cents: 0,
        };
        for _ in 0..3 {
            running.id = book.take_id();
            book.insert(running.clone());
        }
        assert_eq!(book.len(), 3, "three promises, none of them dropped");
        book.get_mut(1).unwrap().status = JobStatus::Done;
        book.get_mut(2).unwrap().status = JobStatus::Done;
        book.set_cap(1);
        assert_eq!(book.len(), 1);
        assert_eq!(
            book.jobs().next().unwrap().id,
            3,
            "the one still running is the one that stays"
        );
    }
}
