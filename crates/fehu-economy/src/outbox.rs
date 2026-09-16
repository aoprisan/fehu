//! The outbox: the facts a game backend must not miss.
//!
//! The server already tells everyone what happens, over SSE
//! ([`Stream`](crate::market::Stream)). That is the right shape for a chart
//! in a browser and the wrong one for the service that owns the rest of the
//! game: a stream is best-effort, its `?since=` buffer is small and in
//! memory, and a backend that was restarting while a job came due has no way
//! to find out that it did. What a game backend needs is a *log* it can read
//! at its own pace, from a place it chooses, that survives both ends being
//! restarted.
//!
//! So the same facts go a second way. Every economic fact the server
//! publishes is also appended here, numbered, retained, and handed out
//! against a cursor. Delivery is at-least-once: a consumer that crashes
//! before recording what it read asks again from where it last acknowledged
//! and gets the same facts a second time. It is never at-most-once, which is
//! the property that matters — a fact delivered twice is a duplicate the
//! consumer can recognise by its `seq`, and a fact delivered never is a
//! world that has quietly diverged.
//!
//! # What is in it, and what is not
//!
//! **Everything that moves currency or units into or out of a player's
//! hands, and everything the market does on its own.** The second half is
//! what has no requester: a resting order that filled, a job that came due,
//! an order the venue withdrew, a stop that fired, a symbol that halted or
//! was delisted, an accepted game event. The first half is the commands —
//! a purchase, a consumption, a transfer, a reward, a mint, a burn, a
//! dividend, a job starting or being cancelled — and it is here because the
//! requester is not always the backend. A player buys from the catalogue in
//! a browser, transfers to a friend, starts a job; the backend that owns the
//! rest of the game learns of none of it from a response it never saw, and
//! before these were facts it could only poll every wallet.
//!
//! The backend's *own* commands come back to it here too, and that is not a
//! problem to solve: every entry names the journal sequence of the command
//! that caused it, and every committed response carries that same number in
//! `Fehu-Journal-Seq`, so a consumer that wants to skip what it already
//! knows matches the two. The rule for what is a fact is then about the
//! world and not about who asked, which is the only rule a consumer can
//! reason about.
//!
//! What is left out is the world's own bookkeeping — a budget opened or
//! funded, takings swept to treasury, a rule rewritten, a symbol listed —
//! which the operator did and the operator knows, and ticks. Ticks are
//! market data — the highest volume thing the server produces,
//! reconstructible at any time from `/api/symbols/{symbol}/bars` — and a
//! durable log of them would be a time-series database that the outbox is
//! not trying to be. The facts are exactly the [`StreamMessage`] kinds
//! other than the per-connection `hello` and the price `tick`; on the
//! stream each goes only to the party it happened to, and here the backend
//! sees all of them.
//!
//! # A fact is stored as it will be sent
//!
//! An entry keeps its payload as JSON rather than as the typed message. The
//! server never reads one back: an entry is written once, handed on
//! unchanged, and dropped. Rendering it at the point it is noted means the
//! outbox is not a second reason for every DTO on the stream to become
//! `Deserialize`, and that the shape in the log is the shape the consumer
//! saw. [`CommandRecord`](crate::journal::CommandRecord) keeps a response
//! the same way, for the same reason.
//!
//! # It is part of the state, not beside it
//!
//! Entries are appended inside the command that caused them, and only once
//! that command is in the journal — so the outbox holds what was
//! *committed*, never what was merely attempted. It is saved with the
//! snapshot and rebuilt by replay, which is what makes a restart invisible
//! to a consumer: the same facts come back with the same numbers, and a
//! cursor from before the restart still means what it meant.
//!
//! A fact published outside a command is not journaled and so cannot be
//! replayed; noting one would put an entry in the log that a restart would
//! silently lose. [`Market::announce`](crate::market::Market) refuses to,
//! and the guard is that a command is being applied at all.
//!
//! # Bounds, and the honesty about them
//!
//! The log holds `FEHU_OUTBOX` entries and evicts the oldest first, because
//! a consumer that never comes back must not be able to exhaust memory. An
//! entry evicted before it was acknowledged is counted in
//! [`Outbox::dropped`] and is gone for good; a read that starts before the
//! oldest entry still held says so with [`Page::gap`], the same admission
//! the stream makes when a reconnect asks for more than its buffer kept. A
//! consumer that sees a gap has to resynchronise from the snapshot
//! endpoints rather than believe its own state.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Entries kept by default. Facts are rare next to ticks — a busy world
/// produces a few per engine step — so this is hours of history, not
/// minutes.
pub const DEFAULT_OUTBOX: usize = 10_000;

/// Entries one read may return, however large a `limit` asks for.
pub const MAX_PAGE: usize = 500;

/// Entries one read returns when the caller does not say.
pub const DEFAULT_PAGE: usize = 100;

/// One committed fact, in the order it was committed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// Dense from 1, never reused. This is the cursor.
    pub seq: u64,
    /// The journal sequence of the command that caused it. Several entries
    /// share one: an engine step can fill a dozen orders at once.
    pub command_seq: u64,
    /// The `type` of the message, lifted out so a consumer can route on it
    /// without parsing the payload.
    pub kind: String,
    /// Simulated instant the command ran at.
    pub at_ms: i64,
    /// Wall-clock instant it arrived.
    pub wall_ms: i64,
    /// The fact itself, exactly as the SSE stream sent it.
    pub event: Value,
}

/// The log, the consumer's cursor, and what has been lost.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outbox {
    /// Retained entries, oldest first.
    entries: VecDeque<Entry>,
    /// The number the next entry will take. Starts at 1, so 0 means
    /// "everything" as a cursor and "nothing yet" as a high-water mark.
    next_seq: u64,
    /// How far the consumer has acknowledged. Never decreases.
    cursor: u64,
    /// Entries evicted before they were acknowledged: facts the consumer
    /// will never see. Saved, because it is a fact about the world and not
    /// about this process.
    dropped: u64,
    /// How many entries are kept. Not saved: it is an option of the world,
    /// not a fact about it.
    #[serde(skip, default = "default_cap")]
    cap: usize,
}

fn default_cap() -> usize {
    DEFAULT_OUTBOX
}

impl Default for Outbox {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
            cursor: 0,
            dropped: 0,
            cap: DEFAULT_OUTBOX,
        }
    }
}

impl Outbox {
    /// An outbox keeping `cap` entries. `0` keeps none, which switches the
    /// log off: nothing is appended and nothing can be read back.
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            ..Self::default()
        }
    }

    /// Set how many entries are kept, dropping any now over the bound.
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        self.evict();
    }

    /// Whether anything is being kept at all.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.cap > 0
    }

    /// Entries kept right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is being held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The highest sequence ever appended; `0` before the first.
    #[must_use]
    pub fn latest(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// The lowest sequence still held; `0` when nothing is.
    #[must_use]
    pub fn oldest(&self) -> u64 {
        self.entries.front().map_or(0, |e| e.seq)
    }

    /// How far the consumer has acknowledged.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Facts evicted before they were acknowledged.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// How many entries are held past `after`.
    #[must_use]
    pub fn pending_after(&self, after: u64) -> u64 {
        self.entries.iter().filter(|e| e.seq > after).count() as u64
    }

    /// Append the facts one command produced, all under its journal
    /// sequence. Returns how many were appended.
    ///
    /// `facts` are `(kind, payload)` in the order they were published.
    pub fn commit(
        &mut self,
        command_seq: u64,
        at_ms: i64,
        wall_ms: i64,
        facts: impl IntoIterator<Item = (String, Value)>,
    ) -> usize {
        if !self.is_enabled() {
            return 0;
        }
        let mut kept = 0;
        for (kind, event) in facts {
            let seq = self.next_seq;
            self.next_seq += 1;
            self.entries.push_back(Entry {
                seq,
                command_seq,
                kind,
                at_ms,
                wall_ms,
                event,
            });
            kept += 1;
        }
        self.evict();
        kept
    }

    /// The entries after `after`, oldest first, at most `limit` of them.
    #[must_use]
    pub fn after(&self, after: u64, limit: usize) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|e| e.seq > after)
            .take(limit.clamp(1, MAX_PAGE))
            .cloned()
            .collect()
    }

    /// Whether a read from `after` has already missed something: the log
    /// starts after the first entry the caller wanted.
    ///
    /// Asking from the latest sequence is never a gap even when nothing is
    /// retained — there was nothing after it to lose.
    #[must_use]
    pub fn gap_after(&self, after: u64) -> bool {
        after < self.oldest().saturating_sub(1) || (self.is_empty() && after < self.latest())
    }

    /// Record that the consumer has processed everything through `through`.
    ///
    /// The cursor never goes backwards: an acknowledgement that arrives out
    /// of order, or one for an entry that has not been written yet, moves it
    /// no further than the latest entry there is.
    pub fn ack(&mut self, through: u64) -> u64 {
        self.cursor = self.cursor.max(through.min(self.latest()));
        self.cursor
    }

    /// Drop the oldest entries until the bound is met, counting the ones the
    /// consumer had not acknowledged.
    fn evict(&mut self) {
        while self.entries.len() > self.cap {
            let Some(gone) = self.entries.pop_front() else {
                break;
            };
            if gone.seq > self.cursor {
                self.dropped += 1;
            }
        }
    }
}

/// One read of the outbox: the facts, and everything needed to ask again.
#[derive(Clone, Debug, Serialize)]
pub struct Page {
    pub events: Vec<Entry>,
    /// What to send as `after` next time: the last entry returned, or where
    /// the read started when it returned none.
    pub next: u64,
    /// The cursor the server has recorded as acknowledged.
    pub cursor: u64,
    /// The lowest sequence still held; `0` when nothing is.
    pub oldest: u64,
    /// The highest ever appended.
    pub latest: u64,
    /// Entries still waiting after `next`.
    pub pending: u64,
    /// Facts evicted before they were acknowledged, ever.
    pub dropped: u64,
    /// The read started further back than the log reaches: whatever fell out
    /// of it is gone, and the consumer should resynchronise rather than
    /// carry on from its own state.
    pub gap: bool,
    /// Entries this world keeps. `0` means the outbox is switched off.
    pub cap: usize,
}

/// Where the consumer is, with no facts attached: the answer to an
/// acknowledgement.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Cursor {
    pub cursor: u64,
    pub oldest: u64,
    pub latest: u64,
    pub pending: u64,
    pub dropped: u64,
    pub cap: usize,
}

impl Outbox {
    /// Read from `after`, at most `limit` entries.
    #[must_use]
    pub fn page(&self, after: u64, limit: usize) -> Page {
        let gap = self.gap_after(after);
        let events = self.after(after, limit);
        let next = events.last().map_or(after, |e| e.seq);
        Page {
            next,
            cursor: self.cursor,
            oldest: self.oldest(),
            latest: self.latest(),
            pending: self.pending_after(next),
            dropped: self.dropped,
            gap,
            cap: self.cap,
            events,
        }
    }

    /// Where the consumer is.
    #[must_use]
    pub fn position(&self) -> Cursor {
        Cursor {
            cursor: self.cursor,
            oldest: self.oldest(),
            latest: self.latest(),
            pending: self.pending_after(self.cursor),
            dropped: self.dropped,
            cap: self.cap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts(kinds: &[&str]) -> Vec<(String, Value)> {
        kinds
            .iter()
            .map(|k| ((*k).to_string(), json!({ "type": k })))
            .collect()
    }

    #[test]
    fn entries_are_numbered_densely_across_commands() {
        let mut outbox = Outbox::with_cap(10);
        outbox.commit(7, 100, 1_000, facts(&["fill", "fill"]));
        outbox.commit(8, 200, 2_000, facts(&["job_done"]));
        let page = outbox.page(0, 10);
        assert_eq!(
            page.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(page.events[0].command_seq, 7);
        assert_eq!(page.events[2].command_seq, 8, "one step, one sequence");
        assert_eq!(page.events[2].at_ms, 200);
        assert_eq!(page.next, 3);
        assert_eq!(page.pending, 0);
        assert!(!page.gap);
    }

    #[test]
    fn a_cursor_reads_only_what_it_has_not_seen() {
        let mut outbox = Outbox::with_cap(10);
        outbox.commit(1, 0, 0, facts(&["fill", "fill", "fill"]));
        let first = outbox.page(0, 2);
        assert_eq!(first.events.len(), 2);
        assert_eq!(first.next, 2);
        assert_eq!(first.pending, 1, "one left");
        let second = outbox.page(first.next, 2);
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].seq, 3);
        assert_eq!(outbox.page(second.next, 2).events.len(), 0);
    }

    #[test]
    fn a_read_is_repeatable_until_it_is_acknowledged() {
        let mut outbox = Outbox::with_cap(10);
        outbox.commit(1, 0, 0, facts(&["fill", "job_done"]));
        // At-least-once: reading does not consume.
        assert_eq!(outbox.page(0, 10).events.len(), 2);
        assert_eq!(outbox.page(0, 10).events.len(), 2);
        assert_eq!(outbox.ack(2), 2);
        assert_eq!(
            outbox.page(0, 10).events.len(),
            2,
            "an acknowledgement moves the cursor, it does not delete"
        );
        assert_eq!(outbox.page(outbox.cursor(), 10).events.len(), 0);
    }

    #[test]
    fn a_cursor_never_goes_backwards_or_past_the_end() {
        let mut outbox = Outbox::with_cap(10);
        outbox.commit(1, 0, 0, facts(&["fill", "fill", "fill"]));
        assert_eq!(outbox.ack(2), 2);
        assert_eq!(outbox.ack(1), 2, "a late acknowledgement is not a rewind");
        assert_eq!(outbox.ack(99), 3, "and never past what exists");
    }

    #[test]
    fn eviction_is_counted_and_admitted() {
        let mut outbox = Outbox::with_cap(2);
        outbox.commit(1, 0, 0, facts(&["a", "b", "c", "d"]));
        assert_eq!(outbox.len(), 2, "the bound holds");
        assert_eq!(outbox.dropped(), 2, "two facts nobody will ever see");
        assert_eq!(outbox.oldest(), 3);
        let page = outbox.page(0, 10);
        assert!(page.gap, "a consumer starting from nothing has missed some");
        assert_eq!(page.events.len(), 2);
        // Reading from where the log now starts is not a gap.
        assert!(!outbox.page(2, 10).gap);
        assert!(!outbox.page(3, 10).gap);
    }

    #[test]
    fn an_acknowledged_entry_that_ages_out_is_not_a_loss() {
        let mut outbox = Outbox::with_cap(2);
        outbox.commit(1, 0, 0, facts(&["a", "b"]));
        outbox.ack(2);
        outbox.commit(2, 0, 0, facts(&["c", "d"]));
        assert_eq!(outbox.dropped(), 0, "the consumer already had them");
        assert_eq!(outbox.len(), 2);
    }

    #[test]
    fn a_switched_off_outbox_keeps_nothing() {
        let mut outbox = Outbox::with_cap(0);
        assert!(!outbox.is_enabled());
        assert_eq!(outbox.commit(1, 0, 0, facts(&["fill"])), 0);
        assert!(outbox.is_empty());
        assert_eq!(outbox.latest(), 0);
        let page = outbox.page(0, 10);
        assert!(page.events.is_empty());
        assert!(!page.gap, "nothing was lost: nothing was ever kept");
        assert_eq!(page.cap, 0);
    }

    #[test]
    fn a_page_is_bounded_however_much_is_asked_for() {
        let mut outbox = Outbox::with_cap(2_000);
        let many: Vec<_> = (0..1_200)
            .map(|i| ("fill".to_string(), json!({ "n": i })))
            .collect();
        outbox.commit(1, 0, 0, many);
        assert_eq!(outbox.page(0, usize::MAX).events.len(), MAX_PAGE);
        assert_eq!(outbox.page(0, 0).events.len(), 1, "a page is never empty");
    }

    #[test]
    fn the_log_and_its_cursor_survive_a_round_trip() {
        let mut outbox = Outbox::with_cap(4);
        outbox.commit(1, 5, 6, facts(&["fill", "job_done", "delisted"]));
        outbox.ack(1);
        let json = serde_json::to_string(&outbox).unwrap();
        let mut back: Outbox = serde_json::from_str(&json).unwrap();
        back.set_cap(4);
        assert_eq!(back.latest(), 3);
        assert_eq!(back.cursor(), 1);
        assert_eq!(back.page(1, 10).events.len(), 2);
        // And the numbering carries on rather than starting again.
        back.commit(2, 7, 8, facts(&["fill"]));
        assert_eq!(back.latest(), 4);
    }

    #[test]
    fn shrinking_the_bound_evicts_at_once() {
        let mut outbox = Outbox::with_cap(10);
        outbox.commit(1, 0, 0, facts(&["a", "b", "c", "d"]));
        outbox.set_cap(2);
        assert_eq!(outbox.len(), 2);
        assert_eq!(outbox.dropped(), 2);
        assert_eq!(outbox.oldest(), 3);
    }
}
