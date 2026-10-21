//! Credential pool: public key IDs, account/quota scopes, scoped cooldowns
//! and deterministic conversation affinity.
//!
//! Cooldowns are recorded at the scope that actually ran out:
//! - `401` disables only that credential (`auth_failure_cooldown`).
//! - `402` blocks the whole account scope for every model.
//! - `429` blocks the account scope (`rate_limit_scope = "account"`) or only
//!   the `(account scope, model)` pair (`rate_limit_scope = "model"`).
//!
//! Keys that share a scope therefore share one cooldown: a second key on the
//! same account never looks like fresh quota.
//!
//! Affinity is rendezvous hashing on `(conversation, scope)` and then
//! `(conversation, key)`: a conversation stays on the same account and key
//! while they are eligible, with no per-conversation state. The only mutable
//! state is the cooldown tables. The model table is capped at
//! `max_model_cooldowns` without ever dropping an active limit: when it is
//! full of active entries, a new model limit is enforced scope-wide instead
//! (see `record_model_cooldown`).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::http::HeaderValue;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    /// A 429 exhausts the whole account (all models, all keys in the scope).
    #[default]
    Account,
    /// A 429 exhausts only the requested model within the account scope.
    Model,
}

#[derive(Debug, Clone)]
pub struct CooldownPolicy {
    pub default_cooldown: Duration,
    pub auth_failure_cooldown: Duration,
    pub max_cooldown: Duration,
    pub rate_limit_scope: RateLimitScope,
    pub max_model_cooldowns: usize,
}

/// An upstream API key held only as sensitive header values.
pub struct Credential {
    bearer: HeaderValue,
    api_key: HeaderValue,
}

impl Credential {
    /// `None` unless `secret` is one non-empty run of printable ASCII.
    pub fn from_secret(secret: &str) -> Option<Self> {
        if secret.is_empty() || !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
            return None;
        }
        let mut bearer = HeaderValue::from_str(&format!("Bearer {secret}")).ok()?;
        bearer.set_sensitive(true);
        let mut api_key = HeaderValue::from_str(secret).ok()?;
        api_key.set_sensitive(true);
        Some(Self { bearer, api_key })
    }

    /// `Authorization: Bearer …` value for OpenAI-style routes.
    pub fn bearer(&self) -> &HeaderValue {
        &self.bearer
    }

    /// `x-api-key` value for the Anthropic Messages route.
    pub fn api_key(&self) -> &HeaderValue {
        &self.api_key
    }

    pub fn same_secret(&self, other: &Self) -> bool {
        self.api_key == other.api_key
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

#[derive(Debug)]
pub struct KeySpec {
    pub id: String,
    pub scope: String,
    pub credential: Credential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyIndex(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Key(KeyIndex),
    /// No untried key is eligible; `retry_after` is when the first one
    /// becomes eligible again (at least one second).
    Exhausted {
        retry_after: Duration,
    },
}

/// A pre-content upstream rejection attributed to a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    RateLimited { retry_after: Option<Duration> },
    QuotaExhausted { retry_after: Option<Duration> },
    Unauthorized,
}

#[derive(Debug, Serialize)]
pub struct KeyStatus {
    pub id: String,
    pub scope: String,
    pub credential_blocked_secs: Option<u64>,
    pub scope_blocked_secs: Option<u64>,
    pub blocked_models: BTreeMap<String, u64>,
}

#[derive(Debug)]
struct Key {
    id: String,
    scope: usize,
    credential: Credential,
}

#[derive(Debug, Default)]
struct State {
    key_until: Vec<Option<Instant>>,
    scope_until: Vec<Option<Instant>>,
    model_until: HashMap<(usize, String), Instant>,
}

#[derive(Debug)]
pub struct Pool {
    keys: Vec<Key>,
    scopes: Vec<String>,
    policy: CooldownPolicy,
    state: Mutex<State>,
}

impl Pool {
    pub fn new(specs: Vec<KeySpec>, policy: CooldownPolicy) -> Self {
        let mut scopes: Vec<String> = Vec::new();
        let mut keys = Vec::with_capacity(specs.len());
        for spec in specs {
            let scope = match scopes.iter().position(|scope| *scope == spec.scope) {
                Some(index) => index,
                None => {
                    scopes.push(spec.scope);
                    scopes.len() - 1
                }
            };
            keys.push(Key {
                id: spec.id,
                scope,
                credential: spec.credential,
            });
        }
        let state = State {
            key_until: vec![None; keys.len()],
            scope_until: vec![None; scopes.len()],
            model_until: HashMap::new(),
        };
        Self {
            keys,
            scopes,
            policy,
            state: Mutex::new(state),
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn scope_count(&self) -> usize {
        self.scopes.len()
    }

    pub fn key_id(&self, key: KeyIndex) -> &str {
        &self.keys[key.0].id
    }

    pub fn scope_id(&self, key: KeyIndex) -> &str {
        &self.scopes[self.keys[key.0].scope]
    }

    pub fn credential(&self, key: KeyIndex) -> &Credential {
        &self.keys[key.0].credential
    }

    /// Highest-ranked eligible key for `affinity`, skipping keys already
    /// tried for this request. Ties cannot reorder between calls because
    /// ranking is a pure function of `(affinity, scope id, key id)`.
    pub fn select(
        &self,
        model: &str,
        affinity: &str,
        tried: &[KeyIndex],
        now: Instant,
    ) -> Selection {
        let state = self.lock();
        let mut best: Option<((u64, u64), usize)> = None;
        for (index, key) in self.keys.iter().enumerate() {
            if tried.contains(&KeyIndex(index))
                || self.blocked_until(&state, index, model, now).is_some()
            {
                continue;
            }
            let rank = (
                rendezvous(affinity, &self.scopes[key.scope]),
                rendezvous(affinity, &key.id),
            );
            if best.is_none_or(|(current, _)| rank > current) {
                best = Some((rank, index));
            }
        }
        match best {
            Some((_, index)) => Selection::Key(KeyIndex(index)),
            None => Selection::Exhausted {
                retry_after: self.earliest_release(&state, model, now),
            },
        }
    }

    pub fn has_candidate(&self, model: &str, tried: &[KeyIndex], now: Instant) -> bool {
        matches!(self.select(model, "", tried, now), Selection::Key(_))
    }

    /// `Some(wait)` when every key is cooling down for `model`; `None` when
    /// at least one key is eligible right now.
    pub fn unavailable_for(&self, model: &str, now: Instant) -> Option<Duration> {
        let state = self.lock();
        let all_blocked = !self.keys.is_empty()
            && (0..self.keys.len())
                .all(|index| self.blocked_until(&state, index, model, now).is_some());
        all_blocked.then(|| self.earliest_release(&state, model, now))
    }

    /// Record a rejection at its actual scope; returns the applied cooldown.
    pub fn report(&self, key: KeyIndex, model: &str, outcome: Outcome, now: Instant) -> Duration {
        let scope = self.keys[key.0].scope;
        let mut state = self.lock();
        match outcome {
            Outcome::Unauthorized => {
                let cooldown = self
                    .policy
                    .auth_failure_cooldown
                    .min(self.policy.max_cooldown);
                extend(&mut state.key_until[key.0], now + cooldown);
                cooldown
            }
            Outcome::QuotaExhausted { retry_after } => {
                let cooldown = self.clamp(retry_after);
                extend(&mut state.scope_until[scope], now + cooldown);
                cooldown
            }
            Outcome::RateLimited { retry_after } => {
                let cooldown = self.clamp(retry_after);
                match self.policy.rate_limit_scope {
                    RateLimitScope::Account => {
                        extend(&mut state.scope_until[scope], now + cooldown)
                    }
                    RateLimitScope::Model => {
                        let escalated = record_model_cooldown(
                            &mut state,
                            scope,
                            model,
                            now + cooldown,
                            self.policy.max_model_cooldowns,
                            now,
                        );
                        if escalated {
                            tracing::warn!(
                                scope = %self.scopes[scope],
                                model,
                                "model cooldown table is full of active limits; enforcing this limit scope-wide"
                            );
                        }
                    }
                }
                cooldown
            }
        }
    }

    /// Non-secret per-key status for `/healthz`.
    pub fn snapshot(&self, now: Instant) -> Vec<KeyStatus> {
        let state = self.lock();
        let remaining = |slot: Option<Instant>| {
            slot.filter(|until| *until > now)
                .map(|until| ceil_secs(until.saturating_duration_since(now)))
        };
        self.keys
            .iter()
            .enumerate()
            .map(|(index, key)| KeyStatus {
                id: key.id.clone(),
                scope: self.scopes[key.scope].clone(),
                credential_blocked_secs: remaining(state.key_until[index]),
                scope_blocked_secs: remaining(state.scope_until[key.scope]),
                blocked_models: state
                    .model_until
                    .iter()
                    .filter(|((scope, _), until)| *scope == key.scope && **until > now)
                    .map(|((_, model), until)| {
                        (
                            model.clone(),
                            ceil_secs(until.saturating_duration_since(now)),
                        )
                    })
                    .collect(),
            })
            .collect()
    }

    fn clamp(&self, retry_after: Option<Duration>) -> Duration {
        retry_after
            .unwrap_or(self.policy.default_cooldown)
            .min(self.policy.max_cooldown)
    }

    fn blocked_until(
        &self,
        state: &State,
        index: usize,
        model: &str,
        now: Instant,
    ) -> Option<Instant> {
        let scope = self.keys[index].scope;
        let model_until = state.model_until.get(&(scope, model.to_owned())).copied();
        [
            state.key_until[index],
            state.scope_until[scope],
            model_until,
        ]
        .into_iter()
        .flatten()
        .filter(|until| *until > now)
        .max()
    }

    fn earliest_release(&self, state: &State, model: &str, now: Instant) -> Duration {
        (0..self.keys.len())
            .map(|index| {
                self.blocked_until(state, index, model, now)
                    .map_or(Duration::ZERO, |until| until.saturating_duration_since(now))
            })
            .min()
            .unwrap_or(Duration::ZERO)
            .max(Duration::from_secs(1))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn extend(slot: &mut Option<Instant>, until: Instant) {
    if slot.is_none_or(|current| current < until) {
        *slot = Some(until);
    }
}

/// Record a `(scope, model)` cooldown without ever forgetting an active one.
///
/// An existing entry is extended in place. Otherwise expired entries are
/// pruned when the table is full; if it is still full of active limits,
/// nothing is evicted and the new limit is enforced as a scope-wide block
/// until its own deadline (returns `true`). That is conservative — other
/// models of the same account also wait until then — but no limited model
/// or scope ever becomes eligible before its real deadline, and the table
/// never holds more than `cap` entries.
fn record_model_cooldown(
    state: &mut State,
    scope: usize,
    model: &str,
    until: Instant,
    cap: usize,
    now: Instant,
) -> bool {
    let key = (scope, model.to_owned());
    if let Some(current) = state.model_until.get_mut(&key) {
        if *current < until {
            *current = until;
        }
        return false;
    }
    if state.model_until.len() >= cap {
        state.model_until.retain(|_, expiry| *expiry > now);
    }
    if state.model_until.len() < cap {
        state.model_until.insert(key, until);
        false
    } else {
        extend(&mut state.scope_until[scope], until);
        true
    }
}

/// Stable 64-bit FNV-1a + splitmix64 finaliser. Unlike `DefaultHasher`, the
/// value is fixed across Rust releases, so affinity survives rebuilds.
fn rendezvous(affinity: &str, id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in affinity
        .bytes()
        .chain(std::iter::once(0xff))
        .chain(id.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// Short stable tag for logs, so raw conversation IDs stay out of them.
pub fn fingerprint(value: &str) -> String {
    format!("{:016x}", rendezvous(value, "orqen-log"))
}

pub fn ceil_secs(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn policy(scope: RateLimitScope, max_models: usize) -> CooldownPolicy {
        CooldownPolicy {
            default_cooldown: Duration::from_secs(60),
            auth_failure_cooldown: Duration::from_secs(600),
            max_cooldown: Duration::from_secs(3600),
            rate_limit_scope: scope,
            max_model_cooldowns: max_models,
        }
    }

    fn spec(id: &str, scope: &str) -> KeySpec {
        KeySpec {
            id: id.to_owned(),
            scope: scope.to_owned(),
            credential: Credential::from_secret(&format!("secret-{id}")).expect("valid secret"),
        }
    }

    fn chosen(selection: Selection) -> KeyIndex {
        match selection {
            Selection::Key(key) => key,
            Selection::Exhausted { .. } => panic!("pool unexpectedly exhausted"),
        }
    }

    #[test]
    fn affinity_is_deterministic_and_spreads() {
        let pool = Pool::new(
            vec![spec("a", "s1"), spec("b", "s2"), spec("c", "s3")],
            policy(RateLimitScope::Account, 8),
        );
        let now = Instant::now();
        let first = chosen(pool.select("m", "ses_1", &[], now));
        for _ in 0..16 {
            assert_eq!(chosen(pool.select("m", "ses_1", &[], now)), first);
        }
        let distinct: HashSet<KeyIndex> = (0..64)
            .map(|n| chosen(pool.select("m", &format!("ses_{n}"), &[], now)))
            .collect();
        assert!(distinct.len() > 1);
    }

    #[test]
    fn keys_in_one_scope_share_rate_limit_cooldown() {
        let pool = Pool::new(
            vec![spec("a", "acct"), spec("b", "acct")],
            policy(RateLimitScope::Account, 8),
        );
        let now = Instant::now();
        let first = chosen(pool.select("m", "ses", &[], now));
        let applied = pool.report(
            first,
            "m",
            Outcome::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
            now,
        );
        assert_eq!(applied, Duration::from_secs(30));
        let exhausted = Selection::Exhausted {
            retry_after: Duration::from_secs(30),
        };
        assert_eq!(pool.select("m", "ses", &[first], now), exhausted);
        assert_eq!(pool.select("m", "ses", &[], now), exhausted);
        assert_eq!(pool.select("other", "ses", &[], now), exhausted);
        assert_eq!(
            pool.unavailable_for("m", now),
            Some(Duration::from_secs(30))
        );
        let later = now + Duration::from_secs(31);
        assert_eq!(chosen(pool.select("m", "ses", &[], later)), first);
    }

    #[test]
    fn model_scope_limits_only_that_model() {
        let pool = Pool::new(
            vec![spec("a", "acct"), spec("b", "acct")],
            policy(RateLimitScope::Model, 8),
        );
        let now = Instant::now();
        let first = chosen(pool.select("m", "ses", &[], now));
        pool.report(
            first,
            "m",
            Outcome::RateLimited {
                retry_after: Some(Duration::from_secs(10)),
            },
            now,
        );
        assert_eq!(
            pool.select("m", "ses", &[], now),
            Selection::Exhausted {
                retry_after: Duration::from_secs(10)
            }
        );
        assert!(matches!(
            pool.select("n", "ses", &[], now),
            Selection::Key(_)
        ));
        assert_eq!(pool.unavailable_for("n", now), None);
    }

    #[test]
    fn quota_exhaustion_blocks_account_even_in_model_mode() {
        let pool = Pool::new(
            vec![spec("a", "acct"), spec("b", "other")],
            policy(RateLimitScope::Model, 8),
        );
        let now = Instant::now();
        let a = KeyIndex(0);
        pool.report(a, "m", Outcome::QuotaExhausted { retry_after: None }, now);
        for model in ["m", "n"] {
            for n in 0..16 {
                assert_eq!(
                    chosen(pool.select(model, &format!("ses_{n}"), &[], now)),
                    KeyIndex(1)
                );
            }
        }
    }

    #[test]
    fn unauthorized_blocks_only_that_credential() {
        let pool = Pool::new(
            vec![spec("a", "acct"), spec("b", "acct")],
            policy(RateLimitScope::Account, 8),
        );
        let now = Instant::now();
        let first = chosen(pool.select("m", "ses", &[], now));
        pool.report(first, "m", Outcome::Unauthorized, now);
        let second = chosen(pool.select("m", "ses", &[], now));
        assert_ne!(first, second);
        assert_eq!(chosen(pool.select("other", "ses", &[], now)), second);
    }

    #[test]
    fn failover_across_scopes_is_deterministic() {
        let pool = Pool::new(
            vec![spec("a", "s1"), spec("b", "s2")],
            policy(RateLimitScope::Account, 8),
        );
        let now = Instant::now();
        let first = chosen(pool.select("m", "conv", &[], now));
        pool.report(first, "m", Outcome::RateLimited { retry_after: None }, now);
        let second = chosen(pool.select("m", "conv", &[first], now));
        assert_ne!(first, second);
        for _ in 0..8 {
            assert_eq!(chosen(pool.select("m", "conv", &[], now)), second);
        }
    }

    #[test]
    fn cooldown_is_capped_by_policy() {
        let pool = Pool::new(vec![spec("a", "s1")], policy(RateLimitScope::Account, 8));
        let now = Instant::now();
        let applied = pool.report(
            KeyIndex(0),
            "m",
            Outcome::RateLimited {
                retry_after: Some(Duration::MAX),
            },
            now,
        );
        assert_eq!(applied, Duration::from_secs(3600));
    }

    fn limit(pool: &Pool, model: &str, secs: u64, now: Instant) {
        pool.report(
            KeyIndex(0),
            model,
            Outcome::RateLimited {
                retry_after: Some(Duration::from_secs(secs)),
            },
            now,
        );
    }

    fn waits(pool: &Pool, model: &str, now: Instant) -> Option<Duration> {
        match pool.select(model, "s", &[], now) {
            Selection::Key(_) => None,
            Selection::Exhausted { retry_after } => Some(retry_after),
        }
    }

    #[test]
    fn full_model_table_never_releases_an_active_limit_early() {
        let pool = Pool::new(vec![spec("a", "s1")], policy(RateLimitScope::Model, 2));
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        limit(&pool, "m1", 10, t0);
        limit(&pool, "m2", 20, t0);
        // Table full of active limits: m3 is enforced scope-wide and nothing
        // is evicted.
        limit(&pool, "m3", 30, t0);
        assert_eq!(pool.lock().model_until.len(), 2);
        assert_eq!(pool.lock().scope_until[0], Some(at(30)));

        assert_eq!(waits(&pool, "m1", at(5)), Some(Duration::from_secs(25)));
        assert_eq!(waits(&pool, "m3", at(5)), Some(Duration::from_secs(25)));
        assert_eq!(
            waits(&pool, "unlimited", at(5)),
            Some(Duration::from_secs(25))
        );
        // Past m1's own deadline the conservative scope block still holds;
        // nothing becomes eligible before its real deadline.
        assert_eq!(waits(&pool, "m1", at(15)), Some(Duration::from_secs(15)));
        assert_eq!(waits(&pool, "m2", at(25)), Some(Duration::from_secs(5)));
        assert_eq!(waits(&pool, "m3", at(29)), Some(Duration::from_secs(1)));
        for model in ["m1", "m2", "m3", "unlimited"] {
            assert_eq!(waits(&pool, model, at(30)), None, "{model}");
        }
    }

    #[test]
    fn model_table_reclaims_only_expired_entries() {
        let pool = Pool::new(vec![spec("a", "s1")], policy(RateLimitScope::Model, 2));
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        limit(&pool, "m1", 10, t0);
        limit(&pool, "m2", 20, t0);
        // A repeat limit extends the existing entry instead of escalating.
        limit(&pool, "m1", 10, at(5));
        assert_eq!(pool.lock().model_until.len(), 2);
        assert_eq!(pool.lock().scope_until[0], None);
        assert_eq!(waits(&pool, "m1", at(12)), Some(Duration::from_secs(3)));
        assert_eq!(waits(&pool, "m2", at(12)), Some(Duration::from_secs(8)));
        assert_eq!(waits(&pool, "m3", at(12)), None);
        // Both old limits have expired at t21: the new one prunes them and
        // stays model-scoped.
        limit(&pool, "m3", 10, at(21));
        assert_eq!(pool.lock().model_until.len(), 1);
        assert_eq!(pool.lock().scope_until[0], None);
        assert_eq!(waits(&pool, "m3", at(21)), Some(Duration::from_secs(10)));
        assert_eq!(waits(&pool, "m1", at(21)), None);
        assert_eq!(waits(&pool, "m2", at(21)), None);
        assert_eq!(waits(&pool, "m3", at(31)), None);
    }

    #[test]
    fn credential_debug_is_redacted() {
        let credential = Credential::from_secret("sk-very-secret").expect("valid secret");
        assert!(!format!("{credential:?}").contains("very-secret"));
        assert!(Credential::from_secret("two tokens").is_none());
    }
}
