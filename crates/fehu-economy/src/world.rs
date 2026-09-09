//! What a game event does to the world beyond the price.
//!
//! A game event has always expanded into simulator events — a jump, a shift
//! in the fundamental, a spell of volatility ([`crate::events`]). That moves
//! the *reference price*, which is exactly the right thing for a market of
//! shares and exactly half of what an economy needs: a scandal at the mine
//! should also mean the mine produces less, and a hyped good should mean the
//! merchant standing behind it quotes for more of it.
//!
//! So an event pushes a **modifier** here as well. A modifier is one number,
//! in basis points, on one of two things:
//!
//! * [`Effect::Production`] — what a job's recipe yields. It is read once,
//!   when the job starts, and written into the job ([`crate::jobs`]), so an
//!   event that lands halfway through does not change what a player was
//!   told they would get.
//! * [`Effect::Demand`] — how much an NPC merchant quotes at each level
//!   ([`crate::npc`]). It is read every time the merchant re-quotes, which
//!   is how the world's appetite shows up in the book.
//!
//! # Why the decay is a straight line
//!
//! The simulator's own effects decay exponentially, in `f64`, inside the
//! deterministic core that `libm` exists to keep bit-identical. These do
//! not: a modifier is at full strength when it lands and ramps down to
//! nothing at its end, in integer basis points. That is not an
//! approximation of the price model but a different thing measured
//! differently — a yield is a count of units and a quote size is a count of
//! units, and both are integers all the way down. It also means replay needs
//! no floating point at all to arrive at the same batch.
//!
//! # Determinism
//!
//! A modifier is created by a journaled command ([`Command::GameEvent`] and
//! [`Command::SimEvent`] both carry the instant they were accepted at), and
//! read at instants that are themselves journaled — a job's start, an engine
//! step. Nothing here reads a clock.
//!
//! [`Command::GameEvent`]: crate::journal::Command::GameEvent
//! [`Command::SimEvent`]: crate::journal::Command::SimEvent

use serde::{Deserialize, Serialize};

use crate::jobs::{BPS, MAX_YIELD_BPS, MIN_YIELD_BPS};
use crate::save::Symbol;

/// Modifiers the world holds at once. Expired ones are dropped as they are
/// read, so this is a bound on how many events can be in force, not on how
/// many have ever happened.
pub const MAX_MODIFIERS: usize = 256;

/// What a modifier changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// What a recipe yields when a job starts.
    Production,
    /// How much an NPC merchant quotes.
    Demand,
}

impl Effect {
    /// A short, stable name for logs and audits.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Demand => "demand",
        }
    }
}

/// One event's pull on production or demand, and how long it lasts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectSpec {
    pub effect: Effect,
    /// At full strength, in basis points: `2_000` is a fifth more,
    /// `-3_000` a third less.
    pub delta_bps: i32,
    /// How long it takes to ramp down to nothing, in simulated seconds.
    pub secs: u64,
}

/// A modifier in force: what it changes, where, and until when.
#[derive(Clone, Debug, Serialize, Deserialize)]
// The ticker is registered on the way in rather than borrowed from the
// input, so the derive needs no `'de: 'static`.
#[serde(bound(deserialize = ""))]
pub struct Modifier {
    pub id: u64,
    pub effect: Effect,
    /// The good or company it hits, or `null` for every symbol.
    #[serde(with = "crate::save::symbol_opt")]
    pub symbol: Option<Symbol>,
    /// Its pull at [`Modifier::from_ms`], in basis points.
    pub delta_bps: i32,
    /// Simulated instants: when it landed and when it is spent.
    pub from_ms: i64,
    pub until_ms: i64,
    /// `"game:scandal"`, as the event log spells it.
    pub kind: String,
    /// The event id the game gave it.
    pub source: String,
}

impl Modifier {
    /// The pull left at `now_ms`, ramped down in a straight line.
    ///
    /// Full strength at the instant it landed, nothing at its end, and
    /// nothing at all outside its window.
    #[must_use]
    pub fn pull_bps(&self, now_ms: i64) -> i64 {
        if now_ms < self.from_ms || now_ms >= self.until_ms {
            return 0;
        }
        let span = self.until_ms.saturating_sub(self.from_ms);
        if span <= 0 {
            return 0;
        }
        let left = self.until_ms.saturating_sub(now_ms);
        i64::from(self.delta_bps).saturating_mul(left) / span
    }

    /// Whether this modifier still has anything to say at `now_ms`.
    #[must_use]
    pub fn is_live(&self, now_ms: i64) -> bool {
        now_ms < self.until_ms
    }

    /// Whether it applies to `symbol`. A market-wide modifier applies to
    /// every one.
    #[must_use]
    pub fn hits(&self, symbol: &str) -> bool {
        self.symbol.is_none_or(|s| s == symbol)
    }
}

/// Every modifier in force, and the counter that names the next one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(bound(deserialize = ""))]
pub struct WorldEffects {
    modifiers: Vec<Modifier>,
    next_id: u64,
}

impl WorldEffects {
    /// Put a modifier in force at `from_ms`.
    ///
    /// A spec of zero length or zero pull is dropped rather than filed: it
    /// would be a row in every audit that never changed a number.
    pub fn push(
        &mut self,
        spec: EffectSpec,
        symbol: Option<Symbol>,
        from_ms: i64,
        kind: &str,
        source: &str,
    ) -> Option<&Modifier> {
        if spec.delta_bps == 0 || spec.secs == 0 {
            return None;
        }
        self.next_id += 1;
        let modifier = Modifier {
            id: self.next_id,
            effect: spec.effect,
            symbol,
            delta_bps: spec.delta_bps,
            from_ms,
            until_ms: from_ms.saturating_add(
                i64::try_from(spec.secs)
                    .unwrap_or(i64::MAX / 1_000)
                    .saturating_mul(1_000),
            ),
            kind: kind.to_owned(),
            source: source.to_owned(),
        };
        self.modifiers.push(modifier);
        self.forget(from_ms);
        self.modifiers.last()
    }

    /// What `effect` multiplies by for `symbol` at `now_ms`, in basis
    /// points, with `10_000` meaning "as written".
    ///
    /// Modifiers add: two scandals are worse than one. The total is clamped
    /// to the range a yield may take, so stacking events makes for an
    /// unusually bad day rather than a world that produces nothing.
    #[must_use]
    pub fn multiplier_bps(&self, effect: Effect, symbol: &str, now_ms: i64) -> i64 {
        let pull: i64 = self
            .modifiers
            .iter()
            .filter(|m| m.effect == effect && m.hits(symbol))
            .map(|m| m.pull_bps(now_ms))
            .fold(0, i64::saturating_add);
        BPS.saturating_add(pull).clamp(MIN_YIELD_BPS, MAX_YIELD_BPS)
    }

    /// Every modifier still in force at `now_ms`, newest last.
    pub fn active(&self, now_ms: i64) -> impl Iterator<Item = &Modifier> {
        self.modifiers.iter().filter(move |m| m.is_live(now_ms))
    }

    /// Drop what is spent, and the oldest of what is left if there is still
    /// too much of it.
    pub fn forget(&mut self, now_ms: i64) {
        self.modifiers.retain(|m| m.is_live(now_ms));
        if self.modifiers.len() > MAX_MODIFIERS {
            let excess = self.modifiers.len() - MAX_MODIFIERS;
            self.modifiers.drain(..excess);
        }
    }

    /// Modifiers held, spent ones that have not been swept included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.modifiers.len()
    }

    /// Nothing has happened to the world.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modifiers.is_empty()
    }
}

/// `GET /api/world`: what is pulling on production and demand right now.
#[derive(Debug, Serialize)]
pub struct WorldResponse {
    /// Simulated instant these were read at.
    pub at_ms: i64,
    /// Every modifier still in force, newest last.
    pub modifiers: Vec<Modifier>,
    /// Per symbol, what production and demand multiply by right now.
    pub symbols: Vec<SymbolEffects>,
}

/// One symbol's standing with the world.
#[derive(Clone, Debug, Serialize)]
#[serde(bound(deserialize = ""))]
pub struct SymbolEffects {
    #[serde(with = "crate::save::symbol")]
    pub symbol: Symbol,
    /// What a recipe making this yields, in basis points of the recipe.
    pub production_bps: i64,
    /// What a merchant in this quotes, in basis points of its policy size.
    pub demand_bps: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn spec(effect: Effect, delta_bps: i32, secs: u64) -> EffectSpec {
        EffectSpec {
            effect,
            delta_bps,
            secs,
        }
    }

    #[test]
    fn a_modifier_ramps_down_to_nothing() {
        let mut world = WorldEffects::default();
        world.push(
            spec(Effect::Production, 4_000, 100),
            Some("ORE"),
            NOW,
            "game:hype",
            "e1",
        );
        assert_eq!(world.multiplier_bps(Effect::Production, "ORE", NOW), 14_000);
        assert_eq!(
            world.multiplier_bps(Effect::Production, "ORE", NOW + 50_000),
            12_000,
            "half way through, half the pull"
        );
        assert_eq!(
            world.multiplier_bps(Effect::Production, "ORE", NOW + 100_000),
            BPS,
            "and then it is over"
        );
    }

    #[test]
    fn a_modifier_stays_on_its_own_symbol_and_its_own_effect() {
        let mut world = WorldEffects::default();
        world.push(
            spec(Effect::Production, -3_000, 100),
            Some("ORE"),
            NOW,
            "game:scandal",
            "e1",
        );
        assert_eq!(world.multiplier_bps(Effect::Production, "ORE", NOW), 7_000);
        assert_eq!(
            world.multiplier_bps(Effect::Production, "INGOT", NOW),
            BPS,
            "another good is another good"
        );
        assert_eq!(
            world.multiplier_bps(Effect::Demand, "ORE", NOW),
            BPS,
            "and production is not demand"
        );
    }

    #[test]
    fn a_market_wide_modifier_hits_everything_and_they_add_up() {
        let mut world = WorldEffects::default();
        world.push(
            spec(Effect::Demand, -2_000, 100),
            None,
            NOW,
            "game:market_crash",
            "e1",
        );
        world.push(
            spec(Effect::Demand, -1_000, 100),
            Some("ORE"),
            NOW,
            "game:scandal",
            "e2",
        );
        assert_eq!(world.multiplier_bps(Effect::Demand, "ORE", NOW), 7_000);
        assert_eq!(world.multiplier_bps(Effect::Demand, "ACME", NOW), 8_000);
    }

    #[test]
    fn stacking_is_bounded_at_both_ends() {
        let mut world = WorldEffects::default();
        for i in 0..20 {
            world.push(
                spec(Effect::Production, -9_000, 100),
                None,
                NOW,
                "game:scandal",
                &format!("e{i}"),
            );
        }
        assert_eq!(
            world.multiplier_bps(Effect::Production, "ORE", NOW),
            MIN_YIELD_BPS,
            "a terrible day, not an impossible one"
        );
        let mut world = WorldEffects::default();
        for i in 0..20 {
            world.push(
                spec(Effect::Production, 9_000, 100),
                None,
                NOW,
                "game:hype",
                &format!("e{i}"),
            );
        }
        assert_eq!(
            world.multiplier_bps(Effect::Production, "ORE", NOW),
            MAX_YIELD_BPS
        );
    }

    #[test]
    fn what_is_spent_is_forgotten() {
        let mut world = WorldEffects::default();
        world.push(
            spec(Effect::Demand, 1_000, 60),
            None,
            NOW,
            "game:hype",
            "e1",
        );
        assert_eq!(world.active(NOW).count(), 1);
        world.forget(NOW + 60_000);
        assert!(world.is_empty());
    }

    #[test]
    fn an_empty_spec_is_not_filed() {
        let mut world = WorldEffects::default();
        assert!(
            world
                .push(spec(Effect::Demand, 0, 60), None, NOW, "game:hype", "e1")
                .is_none()
        );
        assert!(
            world
                .push(spec(Effect::Demand, 500, 0), None, NOW, "game:hype", "e1")
                .is_none()
        );
        assert!(world.is_empty());
    }
}
