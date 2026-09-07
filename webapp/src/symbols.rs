//! Tickers, and how a `&'static str` can name a symbol the build never heard
//! of.
//!
//! The whole server keys on the ticker: positions, share reservations, order
//! records, fills, ledger entries, stops and the event log all hold a
//! [`crate::save::Symbol`], which is `&'static str`. That is what makes them
//! cheap to copy, cheap to compare, ordered by ticker in every `BTreeMap`,
//! and plain strings in JSON. It used to also mean the symbol set was fixed
//! at build time, because the only `&'static str`s in existence were the
//! literals in [`crate::market::seeded_symbols`].
//!
//! This module breaks that link without touching the representation. A
//! ticker is *registered* once — validated, canonicalised to upper case, and
//! leaked with [`Box::leak`] so it lives as long as the process — and every
//! later mention of it, from an API path or a save file, resolves to the same
//! `&'static str`. Registering is therefore idempotent and pointer-stable:
//! two `Symbol`s naming the same ticker are the same pointer.
//!
//! Leaking is deliberate. A ticker has to outlive every record that mentions
//! it, and records outlive listings: a delisted symbol's fills and ledger
//! entries still name it, and re-listing the ticker later must land on the
//! same string. Nothing here is ever freed, so the registry is capped
//! ([`MAX_TICKERS`]) and every ticker is bounded ([`MAX_TICKER_LEN`]) — a
//! hostile or corrupt save can waste a few kilobytes, not the heap.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

/// The longest ticker the registry accepts.
pub const MAX_TICKER_LEN: usize = 8;

/// Tickers one process will ever register. Listing and delisting the same
/// ticker repeatedly costs nothing — it is registered once — so this is a
/// bound on distinct names, not on churn.
pub const MAX_TICKERS: usize = 1_024;

/// Why a ticker was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TickerError {
    /// Empty, or nothing but whitespace.
    Empty,
    /// Longer than [`MAX_TICKER_LEN`].
    TooLong,
    /// Not a letter, digit, `.` or `-`, or it did not start with a letter.
    Malformed,
    /// The registry is full ([`MAX_TICKERS`]).
    Full,
}

impl std::fmt::Display for TickerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "a ticker cannot be empty"),
            Self::TooLong => write!(f, "a ticker is at most {MAX_TICKER_LEN} characters"),
            Self::Malformed => write!(
                f,
                "a ticker must start with a letter and hold only letters, digits, '.' and '-'"
            ),
            Self::Full => write!(
                f,
                "this server has registered {MAX_TICKERS} tickers already"
            ),
        }
    }
}

impl std::error::Error for TickerError {}

fn registry() -> &'static Mutex<BTreeSet<&'static str>> {
    static REGISTRY: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// `ticker` as this process spells it: trimmed and upper-cased.
///
/// # Errors
/// [`TickerError`] if it is empty, too long, or not a well-formed ticker.
pub fn canonical(ticker: &str) -> Result<String, TickerError> {
    let trimmed = ticker.trim();
    if trimmed.is_empty() {
        return Err(TickerError::Empty);
    }
    if trimmed.chars().count() > MAX_TICKER_LEN {
        return Err(TickerError::TooLong);
    }
    let mut chars = trimmed.chars();
    let first = chars.next().ok_or(TickerError::Empty)?;
    if !first.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(TickerError::Malformed);
    }
    Ok(trimmed.to_ascii_uppercase())
}

/// The registered ticker `ticker` names, matched case-insensitively, or
/// `None` if this process has never registered it.
///
/// This is the lookup an unknown name gets: it never registers anything, so a
/// typo in a request path stays a typo rather than becoming a ticker.
#[must_use]
pub fn lookup(ticker: &str) -> Option<&'static str> {
    let canon = canonical(ticker).ok()?;
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.get(canon.as_str()).copied()
}

/// Register `ticker`, or return the registration it already has.
///
/// The returned `&'static str` is the one every other mention of the ticker
/// resolves to, whether it came from a listing request or a save file.
///
/// # Errors
/// [`TickerError`] if the ticker is malformed, or if the registry is full.
pub fn register(ticker: &str) -> Result<&'static str, TickerError> {
    let canon = canonical(ticker)?;
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = reg.get(canon.as_str()) {
        return Ok(existing);
    }
    if reg.len() >= MAX_TICKERS {
        return Err(TickerError::Full);
    }
    // Never freed: see the module comment. Every record that names a ticker
    // outlives the listing, so the string has to outlive both.
    let leaked: &'static str = Box::leak(canon.into_boxed_str());
    reg.insert(leaked);
    Ok(leaked)
}

/// How many distinct tickers this process has registered. Listings come and
/// go; this only ever grows.
#[must_use]
pub fn registered() -> usize {
    registry().lock().unwrap_or_else(|e| e.into_inner()).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_idempotent_and_pointer_stable() {
        let first = register("zzta").unwrap();
        let again = register("ZZTA").unwrap();
        assert_eq!(first, "ZZTA");
        assert!(
            std::ptr::eq(first, again),
            "the same ticker, the same string"
        );
        assert!(std::ptr::eq(lookup("  zzTa ").unwrap(), first));
    }

    #[test]
    fn lookup_never_registers() {
        let before = registered();
        assert_eq!(lookup("ZZNOPE"), None);
        assert_eq!(registered(), before, "a lookup is not a listing");
    }

    #[test]
    fn malformed_tickers_are_refused() {
        assert_eq!(canonical(""), Err(TickerError::Empty));
        assert_eq!(canonical("   "), Err(TickerError::Empty));
        assert_eq!(canonical("TOOLONGTICKER"), Err(TickerError::TooLong));
        assert_eq!(canonical("1ABC"), Err(TickerError::Malformed));
        assert_eq!(canonical("A B"), Err(TickerError::Malformed));
        assert_eq!(canonical("A_B"), Err(TickerError::Malformed));
        assert_eq!(canonical("BRK.B").unwrap(), "BRK.B");
    }
}
