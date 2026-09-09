//! Service credentials: what the game backend may do, and only that.
//!
//! A player proves who they are with the key their sign-up returned
//! ([`crate::auth`]); the operator proves it with `FEHU_ADMIN_KEY`. Between
//! the two sits the game backend, which is neither: it acts for every player
//! at once — paying quest rewards, granting items, pushing world events —
//! but has no business minting currency, freezing an account or rewriting the
//! catalogue.
//!
//! Until this module existed it had to hold the operator key to do any of it,
//! because the operator key was the only credential the server understood
//! that was not a player's. A *service* is that missing third principal: a
//! key with a fixed set of [`Scope`]s and nothing else.
//!
//! # Scopes narrow a credential; they do not narrow the operator
//!
//! A service key is a weaker way in, never a new requirement. Every route a
//! scope opens is still open to the operator exactly as it was, so issuing
//! one takes nothing away from a world that never asks for it, and a server
//! with no services behaves as it did before. What a scope buys is the
//! ability to hand the game backend a credential that cannot mint.
//!
//! The four are the ones the API has callers for:
//!
//! | Scope | Opens |
//! |---|---|
//! | [`Scope::Provision`] | `POST /api/v1/economy/players` — map a player the game already has onto a user, an account and a trader |
//! | [`Scope::Reward`] | `POST /api/v1/economy/rewards` — pay a configured reward out of a budget |
//! | [`Scope::Inventory`] | `POST /api/v1/economy/purchases` and `/consume` — issue and destroy units of a good |
//! | [`Scope::Events`] | `POST /api/game/events` — push a catalogue game event at the world |
//!
//! Raw simulator events (`POST /api/symbols/{symbol}/events`) are deliberately
//! *not* a scope. They are a lever on the price process rather than a fact
//! about the game, they are how the determinism tests drive a symbol, and a
//! backend that wants to move a price has [`Scope::Events`] and the semantic
//! catalogue to do it with. So they stay the operator's.
//!
//! # No credential is held here either
//!
//! A service key is generated once, by the route that issues it, returned in
//! that one response, and reaches this module only as the same
//! domain-separated digest a player's key does. The registry, the journal and
//! the save file carry digests, so none of them is a list of live
//! credentials, and a service that loses its key is issued a new service
//! rather than handed the old one back.
//!
//! Revoking is a tombstone rather than a delete: the service stays, marked
//! [`Service::revoked`], so a key that was taken away is refused as revoked
//! rather than as unknown, and the id in a journal entry still resolves to
//! the thing that sent it.

use std::collections::BTreeMap;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::auth::key_digest;

/// The longest name a service may be given.
pub const MAX_NAME: usize = 64;

/// A service's id, dense and increasing from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceId(pub u64);

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One thing a service credential may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Map an external player onto a user, an account and a trader.
    Provision,
    /// Pay a configured reward out of a budget wallet.
    Reward,
    /// Issue and destroy units of a good.
    Inventory,
    /// Push a game event from the catalogue at the world.
    Events,
}

impl Scope {
    /// Every scope, in the order they are listed.
    pub const ALL: [Self; 4] = [Self::Provision, Self::Reward, Self::Inventory, Self::Events];

    /// How the scope is spelled on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provision => "provision",
            Self::Reward => "reward",
            Self::Inventory => "inventory",
            Self::Events => "events",
        }
    }

    /// The bit this scope occupies in a [`ScopeSet`].
    const fn bit(self) -> u8 {
        match self {
            Self::Provision => 1,
            Self::Reward => 1 << 1,
            Self::Inventory => 1 << 2,
            Self::Events => 1 << 3,
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Scope {
    type Err = UnknownScope;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|scope| scope.as_str() == s)
            .ok_or_else(|| UnknownScope(s.to_owned()))
    }
}

/// A scope name this server does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownScope(pub String);

impl std::fmt::Display for UnknownScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let known: Vec<&str> = Scope::ALL.iter().map(|s| s.as_str()).collect();
        write!(
            f,
            "no such scope: {:?} (this server has {})",
            self.0,
            known.join(", ")
        )
    }
}

impl std::error::Error for UnknownScope {}

/// The scopes one credential carries.
///
/// A bitset rather than a `BTreeSet`, so it is `Copy` and the published
/// directory a request reads on its way in costs no allocation to clone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScopeSet(u8);

impl ScopeSet {
    /// The empty set: a credential that may do nothing.
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// Every scope at once.
    #[must_use]
    pub fn all() -> Self {
        Scope::ALL.into_iter().collect()
    }

    /// Add `scope`.
    pub fn insert(&mut self, scope: Scope) {
        self.0 |= scope.bit();
    }

    /// Whether this credential carries `scope`.
    #[must_use]
    pub const fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// Whether it carries nothing at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many scopes it carries.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// The scopes it carries, in [`Scope::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Scope> {
        Scope::ALL.into_iter().filter(move |s| self.contains(*s))
    }
}

impl FromIterator<Scope> for ScopeSet {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        let mut set = Self::none();
        for scope in iter {
            set.insert(scope);
        }
        set
    }
}

impl Serialize for ScopeSet {
    /// As the list of names it is written as in a request, so a round trip
    /// through the save file or the journal is the same JSON the client sent.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter().map(Scope::as_str))
    }
}

impl<'de> Deserialize<'de> for ScopeSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Names;

        impl<'de> Visitor<'de> for Names {
            type Value = ScopeSet;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a list of scope names")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ScopeSet, A::Error> {
                let mut set = ScopeSet::none();
                while let Some(name) = seq.next_element::<String>()? {
                    set.insert(name.parse().map_err(de::Error::custom)?);
                }
                Ok(set)
            }
        }

        deserializer.deserialize_seq(Names)
    }
}

/// A credential the game backend speaks with, and the scopes it carries.
///
/// The key itself is not here — see the module docs — only the digest of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Service {
    pub id: ServiceId,
    /// What it is for, so a list of them is readable by an operator.
    pub name: String,
    pub scopes: ScopeSet,
    /// The domain-separated SHA-256 of the key issued for it.
    pub digest: String,
    /// Set when the key was taken away. A revoked service is kept so that
    /// its id still resolves and its key is refused as revoked.
    pub revoked: bool,
    pub created_ms: i64,
    pub revoked_ms: Option<i64>,
}

impl Service {
    /// Whether this credential may act at `scope` right now.
    #[must_use]
    pub fn may(&self, scope: Scope) -> bool {
        !self.revoked && self.scopes.contains(scope)
    }
}

/// What resolving a key against the registry found: enough to authorise a
/// request without borrowing from the published directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceAuth {
    pub id: ServiceId,
    pub scopes: ScopeSet,
    pub revoked: bool,
}

impl ServiceAuth {
    /// Whether this credential may act at `scope` right now.
    #[must_use]
    pub fn may(&self, scope: Scope) -> bool {
        !self.revoked && self.scopes.contains(scope)
    }
}

/// Every service credential the world has issued, live and revoked.
#[derive(Clone, Debug, Default)]
pub struct Services {
    by_id: BTreeMap<ServiceId, Service>,
    by_digest: BTreeMap<String, ServiceId>,
}

impl Services {
    /// Add `service`, replacing any earlier one with its id.
    pub fn install(&mut self, service: Service) {
        if let Some(old) = self.by_id.insert(service.id, service.clone()) {
            self.by_digest.remove(&old.digest);
        }
        self.by_digest.insert(service.digest.clone(), service.id);
    }

    /// The service `key` speaks for, revoked or not.
    ///
    /// A revoked key still resolves, so the caller can be told its key was
    /// taken away rather than that it never existed.
    #[must_use]
    pub fn resolve(&self, key: &str) -> Option<ServiceAuth> {
        let id = *self.by_digest.get(&key_digest(key))?;
        let service = self.by_id.get(&id)?;
        Some(ServiceAuth {
            id,
            scopes: service.scopes,
            revoked: service.revoked,
        })
    }

    /// The service with this id.
    #[must_use]
    pub fn get(&self, id: ServiceId) -> Option<&Service> {
        self.by_id.get(&id)
    }

    /// Take a service's key away. Returns whether there was one to revoke;
    /// revoking a revoked service changes nothing and is not an error.
    pub fn revoke(&mut self, id: ServiceId, at_ms: i64) -> bool {
        let Some(service) = self.by_id.get_mut(&id) else {
            return false;
        };
        if !service.revoked {
            service.revoked = true;
            service.revoked_ms = Some(at_ms);
        }
        true
    }

    /// Every service, by id.
    pub fn iter(&self) -> impl Iterator<Item = &Service> {
        self.by_id.values()
    }

    /// How many have been issued, revoked ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Every service, for a save file.
    #[must_use]
    pub fn to_saved(&self) -> Vec<Service> {
        self.by_id.values().cloned().collect()
    }

    /// Rebuild from what [`Services::to_saved`] wrote down.
    pub fn from_saved(saved: impl IntoIterator<Item = Service>) -> Self {
        let mut services = Self::default();
        for service in saved {
            services.install(service);
        }
        services
    }
}

/// `POST /api/v1/economy/admin/services`.
///
/// The digest is not in here: nothing outside the server has any use for it,
/// and a response is not the place to start handing out the one thing the
/// registry does hold.
#[derive(Clone, Debug, Serialize)]
pub struct ServiceDto {
    pub id: u64,
    pub name: String,
    pub scopes: ScopeSet,
    pub revoked: bool,
    pub created_ms: i64,
    pub revoked_ms: Option<i64>,
    /// The key this service speaks with, shown **once**: in the response
    /// that issued it, and `null` everywhere after.
    pub api_key: Option<String>,
}

impl From<&Service> for ServiceDto {
    fn from(service: &Service) -> Self {
        Self {
            id: service.id.0,
            name: service.name.clone(),
            scopes: service.scopes,
            revoked: service.revoked,
            created_ms: service.created_ms,
            revoked_ms: service.revoked_ms,
            api_key: None,
        }
    }
}

/// `GET /api/v1/economy/admin/services`.
#[derive(Clone, Debug, Serialize)]
pub struct ServicesResponse {
    pub services: Vec<ServiceDto>,
}

/// Why a service could not be issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceError {
    /// The name is empty or longer than [`MAX_NAME`].
    Name(String),
    /// A credential that may do nothing is a mistake, not a credential.
    NoScopes,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(why) => write!(f, "a service name {why}"),
            Self::NoScopes => write!(
                f,
                "a service needs at least one scope: it could do nothing without one"
            ),
        }
    }
}

impl std::error::Error for ServiceError {}

/// Clean a name for a service, or say why it is not one.
pub fn clean_name(name: &str) -> Result<String, ServiceError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ServiceError::Name("cannot be empty".into()));
    }
    if name.chars().count() > MAX_NAME {
        return Err(ServiceError::Name(format!(
            "is longer than {MAX_NAME} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(ServiceError::Name("cannot hold control characters".into()));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::new_api_key;

    fn service(id: u64, scopes: ScopeSet, digest: String) -> Service {
        Service {
            id: ServiceId(id),
            name: format!("service {id}"),
            scopes,
            digest,
            revoked: false,
            created_ms: 0,
            revoked_ms: None,
        }
    }

    #[test]
    fn a_scope_set_holds_what_was_put_in_it() {
        let mut set = ScopeSet::none();
        assert!(set.is_empty());
        set.insert(Scope::Reward);
        set.insert(Scope::Reward);
        assert_eq!(set.len(), 1, "a scope added twice is held once");
        assert!(set.contains(Scope::Reward));
        assert!(!set.contains(Scope::Events));
        set.insert(Scope::Events);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![Scope::Reward, Scope::Events],
            "iteration is in the order the scopes are declared"
        );
        assert_eq!(ScopeSet::all().len(), Scope::ALL.len() as u32);
    }

    #[test]
    fn a_scope_set_round_trips_as_the_names_it_was_sent_as() {
        let set: ScopeSet = [Scope::Events, Scope::Provision].into_iter().collect();
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, r#"["provision","events"]"#);
        assert_eq!(serde_json::from_str::<ScopeSet>(&json).unwrap(), set);
        assert_eq!(
            serde_json::from_str::<ScopeSet>("[]").unwrap(),
            ScopeSet::none()
        );
        let bad = serde_json::from_str::<ScopeSet>(r#"["mint"]"#).unwrap_err();
        assert!(bad.to_string().contains("no such scope"), "{bad}");
    }

    #[test]
    fn a_key_resolves_to_its_service_and_never_to_its_digest() {
        let key = new_api_key();
        let mut services = Services::default();
        services.install(service(1, ScopeSet::all(), key_digest(&key)));
        let auth = services.resolve(&key).expect("the key is one of ours");
        assert_eq!(auth.id, ServiceId(1));
        assert!(auth.may(Scope::Reward));
        assert_eq!(
            services.resolve(&key_digest(&key)),
            None,
            "the digest is not itself a credential"
        );
        assert_eq!(services.resolve(&new_api_key()), None);
    }

    #[test]
    fn a_scope_a_service_does_not_carry_is_not_open_to_it() {
        let key = new_api_key();
        let mut services = Services::default();
        let scopes = [Scope::Reward].into_iter().collect();
        services.install(service(1, scopes, key_digest(&key)));
        let auth = services.resolve(&key).unwrap();
        assert!(auth.may(Scope::Reward));
        assert!(!auth.may(Scope::Inventory));
        assert!(!auth.may(Scope::Provision));
        assert!(!auth.may(Scope::Events));
    }

    #[test]
    fn a_revoked_key_still_resolves_but_may_nothing() {
        let key = new_api_key();
        let mut services = Services::default();
        services.install(service(1, ScopeSet::all(), key_digest(&key)));
        assert!(services.revoke(ServiceId(1), 99));
        let auth = services
            .resolve(&key)
            .expect("a revoked key still resolves");
        assert!(auth.revoked);
        assert!(
            !auth.may(Scope::Reward),
            "revoked is revoked for every scope"
        );
        let held = services.get(ServiceId(1)).unwrap();
        assert_eq!(held.revoked_ms, Some(99));
        assert!(
            services.revoke(ServiceId(1), 200),
            "revoking twice is not an error"
        );
        assert_eq!(
            services.get(ServiceId(1)).unwrap().revoked_ms,
            Some(99),
            "and does not move when it happened"
        );
        assert!(!services.revoke(ServiceId(7), 0), "there is no service 7");
    }

    #[test]
    fn services_round_trip_through_what_a_save_holds() {
        let (one, two) = (new_api_key(), new_api_key());
        let mut services = Services::default();
        services.install(service(1, ScopeSet::all(), key_digest(&one)));
        services.install(service(
            2,
            [Scope::Events].into_iter().collect(),
            key_digest(&two),
        ));
        services.revoke(ServiceId(2), 5);
        let saved = services.to_saved();
        assert!(
            !serde_json::to_string(&saved).unwrap().contains(&one),
            "a save holds digests, never keys"
        );
        let back = Services::from_saved(saved);
        assert_eq!(back.len(), 2);
        assert!(back.resolve(&one).unwrap().may(Scope::Provision));
        assert!(back.resolve(&two).unwrap().revoked);
    }

    #[test]
    fn a_name_is_cleaned_or_refused() {
        assert_eq!(clean_name("  quests  ").unwrap(), "quests");
        assert_eq!(
            clean_name("  ").unwrap_err(),
            ServiceError::Name("cannot be empty".into())
        );
        assert!(clean_name(&"x".repeat(MAX_NAME + 1)).is_err());
        assert!(clean_name("quest\nbot").is_err());
    }
}
