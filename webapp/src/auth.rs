//! API keys: who a request speaks for.
//!
//! Every user gets one key when they are created, and it is shown once, in
//! the response that created them. A request proves who it is with
//! `Authorization: Bearer <key>` (or `X-Api-Key: <key>`); market data needs
//! no key, but nothing that belongs to a user — their traders, orders,
//! accounts or money — moves without one.
//!
//! The keys live in memory next to the rest of the state, as the keys
//! themselves rather than as hashes: nothing here is persisted, so a store
//! that could be stolen without the accounts beside it does not exist. A
//! deployment that adds persistence must hash them before writing them down.

use std::collections::BTreeMap;

use crate::account::UserId;

/// Bytes of entropy behind a key: 128 bits, as 32 hex characters.
const KEY_BYTES: usize = 16;

/// What every key starts with, so one is recognisable in a log or a config.
pub const KEY_PREFIX: &str = "fehu_";

/// A new key: [`KEY_PREFIX`] and 32 hex characters of operating-system
/// entropy. Panics only if the OS cannot produce randomness at all, which is
/// not a condition to serve traffic in.
pub fn new_api_key() -> String {
    let mut bytes = [0u8; KEY_BYTES];
    getrandom::fill(&mut bytes).expect("the operating system has randomness");
    let mut key = String::with_capacity(KEY_PREFIX.len() + KEY_BYTES * 2);
    key.push_str(KEY_PREFIX);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(key, "{b:02x}");
    }
    key
}

/// The keys the server has issued, and the user each one speaks for.
#[derive(Clone, Debug, Default)]
pub struct Keyring {
    by_key: BTreeMap<String, UserId>,
    by_user: BTreeMap<UserId, String>,
}

impl Keyring {
    /// Issue `user` a key, replacing any earlier one.
    pub fn issue(&mut self, user: UserId) -> String {
        let key = new_api_key();
        if let Some(old) = self.by_user.insert(user, key.clone()) {
            self.by_key.remove(&old);
        }
        self.by_key.insert(key.clone(), user);
        key
    }

    /// The user `key` speaks for, if it is one of ours.
    pub fn user_of(&self, key: &str) -> Option<UserId> {
        self.by_key.get(key).copied()
    }

    /// The key issued to `user`. Only ever handed back in the response that
    /// created them.
    pub fn key_of(&self, user: UserId) -> Option<&str> {
        self.by_user.get(&user).map(String::as_str)
    }

    /// The keys as `(key, user id)` pairs, for a save file.
    pub fn pairs(&self) -> Vec<(String, u64)> {
        self.by_key.iter().map(|(k, u)| (k.clone(), u.0)).collect()
    }

    /// Rebuild a keyring from what [`Keyring::pairs`] wrote down, so a
    /// player's key still opens their account after a restart.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, u64)>) -> Self {
        let mut keys = Self::default();
        for (key, user) in pairs {
            let user = UserId(user);
            keys.by_user.insert(user, key.clone());
            keys.by_key.insert(key, user);
        }
        keys
    }

    /// Number of keys issued.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_resolve_to_their_user() {
        let mut keys = Keyring::default();
        let one = keys.issue(UserId(1));
        let two = keys.issue(UserId(2));
        assert_ne!(one, two, "every key is its own");
        assert!(one.starts_with(KEY_PREFIX));
        assert_eq!(one.len(), KEY_PREFIX.len() + KEY_BYTES * 2);
        assert!(
            one[KEY_PREFIX.len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "{one}"
        );
        assert_eq!(keys.user_of(&one), Some(UserId(1)));
        assert_eq!(keys.user_of(&two), Some(UserId(2)));
        assert_eq!(keys.user_of("fehu_nope"), None);
        assert_eq!(keys.key_of(UserId(2)).unwrap(), two);
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn a_keyring_round_trips_through_its_pairs() {
        let mut keys = Keyring::default();
        let one = keys.issue(UserId(1));
        let two = keys.issue(UserId(7));
        let back = Keyring::from_pairs(keys.pairs());
        assert_eq!(back.user_of(&one), Some(UserId(1)));
        assert_eq!(back.user_of(&two), Some(UserId(7)));
        assert_eq!(back.key_of(UserId(7)).unwrap(), two);
        assert_eq!(back.len(), 2);
    }

    #[test]
    fn re_issuing_retires_the_old_key() {
        let mut keys = Keyring::default();
        let old = keys.issue(UserId(1));
        let new = keys.issue(UserId(1));
        assert_eq!(keys.user_of(&old), None, "the old key stops working");
        assert_eq!(keys.user_of(&new), Some(UserId(1)));
        assert_eq!(keys.len(), 1);
    }
}
