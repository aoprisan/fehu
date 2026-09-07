//! API keys: who a request speaks for.
//!
//! Every user gets one key when they are created, and it is shown once, in
//! the response that created them. A request proves who it is with
//! `Authorization: Bearer <key>` (or `X-Api-Key: <key>`); market data needs
//! no key, but nothing that belongs to a user — their traders, orders,
//! accounts or money — moves without one.
//!
//! Authentication and persistence retain SHA-256 digests. Newly issued keys
//! are held only until the signup response takes them, and are never saved.

use std::collections::BTreeMap;

use crate::account::UserId;
use sha2::{Digest, Sha256};

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
#[derive(Clone, Default)]
pub struct Keyring {
    by_key: BTreeMap<String, UserId>,
    by_user: BTreeMap<UserId, String>,
    issued: BTreeMap<UserId, String>,
}

/// Domain-separated SHA-256 of a randomly generated bearer credential.
/// This is for high-entropy API keys, not user-chosen passwords.
pub(crate) fn key_digest(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"fehu-api-key-v1\0");
    hasher.update(key.as_bytes());
    let mut digest = String::from("sha256:");
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _ = write!(digest, "{byte:02x}");
    }
    digest
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl Keyring {
    /// Issue `user` a key, replacing any earlier one.
    pub fn issue(&mut self, user: UserId) -> String {
        let key = new_api_key();
        let digest = key_digest(&key);
        if let Some(old) = self.by_user.insert(user, digest.clone()) {
            self.by_key.remove(&old);
        }
        self.by_key.insert(digest, user);
        self.issued.insert(user, key.clone());
        key
    }

    /// The user `key` speaks for, if it is one of ours.
    pub fn user_of(&self, key: &str) -> Option<UserId> {
        self.by_key.get(&key_digest(key)).copied()
    }

    /// Take the newly issued key once for the signup response. Restored
    /// keyrings contain only digests and cannot return the original key.
    pub fn take_issued_key(&mut self, user: UserId) -> Option<String> {
        self.issued.remove(&user)
    }

    /// SHA-256 digests as `(digest, user id)` pairs, for a save file.
    pub fn pairs(&self) -> Vec<(String, u64)> {
        self.by_key.iter().map(|(k, u)| (k.clone(), u.0)).collect()
    }

    /// Rebuild a keyring from what [`Keyring::pairs`] wrote down, so a
    /// player's key still opens their account after a restart.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, u64)>) -> Self {
        let mut keys = Self::default();
        for (key, user) in pairs {
            let user = UserId(user);
            if let Some(old) = keys.by_user.insert(user, key.clone()) {
                keys.by_key.remove(&old);
            }
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
        assert_eq!(keys.take_issued_key(UserId(2)).unwrap(), two);
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn a_keyring_round_trips_through_its_pairs() {
        let mut keys = Keyring::default();
        let one = keys.issue(UserId(1));
        let two = keys.issue(UserId(7));
        let mut back = Keyring::from_pairs(keys.pairs());
        assert_eq!(back.user_of(&one), Some(UserId(1)));
        assert_eq!(back.user_of(&two), Some(UserId(7)));
        assert_eq!(back.take_issued_key(UserId(7)), None);
        assert_eq!(back.len(), 2);
    }

    #[test]
    fn credentials_are_handed_out_once_and_digests_are_not_credentials() {
        let mut keys = Keyring::default();
        let key = keys.issue(UserId(1));
        let pairs = keys.pairs();
        assert_ne!(pairs[0].0, key);
        assert!(pairs[0].0.starts_with("sha256:"));
        assert_eq!(keys.user_of(&pairs[0].0), None);
        assert!(!format!("{keys:?}").contains(&key));
        assert_eq!(keys.take_issued_key(UserId(1)), Some(key.clone()));
        assert_eq!(keys.take_issued_key(UserId(1)), None);
        assert_eq!(keys.user_of(&key), Some(UserId(1)));
        let mut restored = Keyring::from_pairs(pairs);
        let replacement = restored.issue(UserId(1));
        assert_eq!(restored.user_of(&key), None);
        assert_eq!(restored.user_of(&replacement), Some(UserId(1)));
        assert_eq!(restored.len(), 1);
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
