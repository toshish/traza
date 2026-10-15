//! Fuse policies: the limits a run is held to, and which policy a run gets.
//!
//! A policy is a set of optional limits plus what to do when one is reached.
//! Each tenant may keep several, told apart by name; a run is held to exactly
//! ONE of them, resolved fresh at every evaluation so a policy edit applies to
//! live runs immediately:
//!
//! 1. the tenant's policy whose `match.service` is the service of the run's
//!    first span (several matching: the most recently created, then by name);
//! 2. else the tenant's policy named [`DEFAULT_POLICY_NAME`];
//! 3. else the server-wide default from [`FuseConfig`], reported under the
//!    name [`GLOBAL_POLICY_NAME`].
//!
//! Limits do not inherit between those levels. A tenant policy that sets only
//! `max_cost_usd` holds its runs to that alone — the global default's repeat
//! limit included — because a limit that silently reappears from another
//! level is a limit nobody can read off the policy they are looking at.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The name a tenant gives its fallback policy: the one a run gets when no
/// service-matching policy fits.
pub const DEFAULT_POLICY_NAME: &str = "default";

/// The name reported for the server-wide default policy ([`FuseConfig`]).
/// Reserved: no tenant policy may take it, so a verdict naming it is never
/// ambiguous about where its limits came from.
pub const GLOBAL_POLICY_NAME: &str = "global";

/// The global default's repeat limit: a run that makes the same call with
/// the same input this many times is looping. Twelve sits above the retry
/// counts client libraries use (single digits) and far below what a stuck
/// agent reaches before anyone notices.
pub const DEFAULT_MAX_REPEATS: u64 = 12;

/// Default ceiling on live runs held in memory (`--fuse-max-runs`).
pub const DEFAULT_MAX_RUNS: usize = 100_000;

/// Longest a policy name may be, in bytes.
const MAX_NAME_BYTES: usize = 64;

/// Longest a `match.service` may be, in bytes. Service names are short; the
/// bound keeps one policy from holding megabytes in memory and in
/// `fuses.jsonl`, which is replayed whole at startup.
pub const MAX_SERVICE_BYTES: usize = 256;

/// Most policies one tenant may keep. A run's policy is resolved among its
/// tenant's policies on every acknowledged batch, so the bound is what keeps
/// one tenant's policy count out of every other tenant's ingest path.
pub const MAX_POLICIES_PER_TENANT: usize = 256;

/// What a policy does when a run reaches one of its limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// The run's verdict becomes `stop`, and stays `stop` until it is reset.
    Stop,
    /// The run's verdict becomes `warn`; nothing is sticky. The default —
    /// the same default as `--fuse-action` — so a policy written without
    /// thinking about enforcement reports before it refuses.
    #[default]
    Warn,
}

impl Action {
    /// The lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Warn => "warn",
        }
    }
}

/// A run's limits. Every limit is optional; an absent one is unbounded.
///
/// Counts are compared as `usage >= limit`: a run holding `max_steps: 50` has
/// used its budget once its fiftieth step is recorded, which is what makes
/// the post-acknowledgement verdict stop the fifty-first rather than allow
/// it. Every present limit must be positive (a zero limit would stop a run
/// before its first step, which is a disabled run, not a budget), and
/// `max_repeats` must be at least 2 — one occurrence is not a repeat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Ceiling on the run's summed cost, in USD (priced: a metered cost, else
    /// one derived from the store's pricing table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Ceiling on the run's summed tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Ceiling on the run's steps: LLM, tool and agent spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u64>,
    /// Ceiling on the run's LLM steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_llm_calls: Option<u64>,
    /// Ceiling on the run's tool steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u64>,
    /// Ceiling on the run's span-clock duration, first start to last end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_seconds: Option<u64>,
    /// Ceiling on how often one step fingerprint (same call, same input) may
    /// occur. Trips at `count >= max_repeats`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repeats: Option<u64>,
    /// Ceiling on consecutive failed steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_error_streak: Option<u64>,
}

impl Limits {
    /// Whether no limit is set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// These limits with every limit `grant` sets replacing this one's.
    pub fn overlaid(&self, grant: &Limits) -> Limits {
        Limits {
            max_cost_usd: grant.max_cost_usd.or(self.max_cost_usd),
            max_tokens: grant.max_tokens.or(self.max_tokens),
            max_steps: grant.max_steps.or(self.max_steps),
            max_llm_calls: grant.max_llm_calls.or(self.max_llm_calls),
            max_tool_calls: grant.max_tool_calls.or(self.max_tool_calls),
            max_duration_seconds: grant.max_duration_seconds.or(self.max_duration_seconds),
            max_repeats: grant.max_repeats.or(self.max_repeats),
            max_error_streak: grant.max_error_streak.or(self.max_error_streak),
        }
    }

    /// Refuses a limit that cannot mean a budget; see the type docs.
    pub(crate) fn validate(&self, what: &str) -> Result<()> {
        if let Some(cost) = self.max_cost_usd {
            if !cost.is_finite() || cost <= 0.0 {
                return Err(Error::Invalid(format!(
                    "{what}: max_cost_usd must be a positive finite number (got {cost})"
                )));
            }
        }
        let counts = [
            ("max_tokens", self.max_tokens),
            ("max_steps", self.max_steps),
            ("max_llm_calls", self.max_llm_calls),
            ("max_tool_calls", self.max_tool_calls),
            ("max_duration_seconds", self.max_duration_seconds),
            ("max_repeats", self.max_repeats),
            ("max_error_streak", self.max_error_streak),
        ];
        for (name, value) in counts {
            if value == Some(0) {
                return Err(Error::Invalid(format!(
                    "{what}: {name} must be positive; omit a limit to leave it unbounded"
                )));
            }
        }
        if self.max_repeats == Some(1) {
            return Err(Error::Invalid(format!(
                "{what}: max_repeats must be at least 2 — a single occurrence is not a repeat"
            )));
        }
        Ok(())
    }
}

/// Which runs a policy is for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyMatch {
    /// The service of a run's first span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// A stored fuse policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FusePolicy {
    /// `fp_` and 16 lowercase hex characters. Stable across a replace: a
    /// policy rewritten under the same `(tenant, name)` keeps its id.
    pub id: String,
    /// The tenant whose runs it governs; empty is the default tenant.
    #[serde(default)]
    pub tenant: String,
    /// Unique within the tenant. [`DEFAULT_POLICY_NAME`] is the fallback.
    pub name: String,
    /// Which runs it is for. Empty exactly when the name is
    /// [`DEFAULT_POLICY_NAME`].
    #[serde(rename = "match", default)]
    pub matches: PolicyMatch,
    /// The limits; see [`Limits`].
    #[serde(default)]
    pub limits: Limits,
    /// What reaching a limit does.
    #[serde(default)]
    pub action: Action,
    /// When the policy was first created, Unix nanoseconds. A replace keeps
    /// it.
    #[serde(default)]
    pub created_at_ns: u64,
}

/// A policy as submitted for create-or-replace: everything but the identity
/// the store assigns.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    /// The policy's name within its tenant.
    pub name: String,
    /// The tenant; empty is the default tenant. A bound credential has this
    /// forced to its binding.
    #[serde(default)]
    pub tenant: String,
    /// Which runs it is for.
    #[serde(rename = "match", default)]
    pub matches: PolicyMatch,
    /// The limits.
    #[serde(default)]
    pub limits: Limits,
    /// What reaching a limit does. Defaults to `warn`.
    #[serde(default)]
    pub action: Action,
}

impl PolicySpec {
    /// Refuses a spec whose resolution would be ambiguous or silently inert:
    /// a non-default policy that matches nothing would never be chosen, and a
    /// default policy with a match would be both a fallback and not one.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.name.is_empty() || self.name.len() > MAX_NAME_BYTES {
            return Err(Error::Invalid(format!(
                "policy name must be 1 to {MAX_NAME_BYTES} bytes"
            )));
        }
        if self.name.chars().any(char::is_control) {
            return Err(Error::Invalid(
                "policy name must not contain control characters".to_owned(),
            ));
        }
        if self.name == GLOBAL_POLICY_NAME {
            return Err(Error::Invalid(format!(
                "`{GLOBAL_POLICY_NAME}` names the server-wide default policy and cannot be \
                 taken by a tenant policy"
            )));
        }
        if !self.tenant.is_empty() && !crate::valid_tenant(&self.tenant) {
            return Err(Error::Invalid(
                "policy tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes".to_owned(),
            ));
        }
        match (&self.matches.service, self.name == DEFAULT_POLICY_NAME) {
            (Some(service), _) if service.is_empty() => {
                return Err(Error::Invalid(
                    "match.service must not be empty; omit it instead".to_owned(),
                ))
            }
            (Some(service), _) if service.len() > MAX_SERVICE_BYTES => {
                return Err(Error::Invalid(format!(
                    "match.service must be at most {MAX_SERVICE_BYTES} bytes"
                )))
            }
            (Some(_), true) => {
                return Err(Error::Invalid(format!(
                    "the `{DEFAULT_POLICY_NAME}` policy is the fallback for every run no other \
                     policy matches, so it takes no match"
                )))
            }
            (None, false) => {
                return Err(Error::Invalid(format!(
                    "policy `{}` matches nothing: give it match.service, or name it \
                     `{DEFAULT_POLICY_NAME}` to make it the tenant's fallback",
                    self.name
                )))
            }
            _ => {}
        }
        self.limits.validate("policy limits")
    }
}

/// The server-wide fuse settings: whether runs are observed at all, the
/// default policy, and how many runs are kept live.
#[derive(Clone, Debug, PartialEq)]
pub struct FuseConfig {
    /// Whether ingest feeds the run guard. `false` (`--no-fuses`) disables
    /// the observer, the fuse log and every fuse surface.
    pub enabled: bool,
    /// The default policy's limits, for runs no tenant policy governs.
    pub limits: Limits,
    /// The default policy's action.
    pub action: Action,
    /// Most live runs held in memory; past it the least recently active is
    /// evicted and rebuilt from the store when next asked about.
    pub max_runs: usize,
}

impl Default for FuseConfig {
    /// On, warning on [`DEFAULT_MAX_REPEATS`] repeats and nothing else: the
    /// one limit that needs no knowledge of the workload to be safe, and a
    /// warning rather than a stop, because a default that stops someone's
    /// agent would be a policy decision nobody made.
    fn default() -> Self {
        Self {
            enabled: true,
            limits: Limits {
                max_repeats: Some(DEFAULT_MAX_REPEATS),
                ..Limits::default()
            },
            action: Action::Warn,
            max_runs: DEFAULT_MAX_RUNS,
        }
    }
}

/// The policy a run is held to at one evaluation, borrowed or synthesized.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolved {
    pub(crate) name: String,
    pub(crate) limits: Limits,
    pub(crate) action: Action,
}

/// Resolves the policy for a run of `tenant` whose first span had `service`,
/// among `policies` — the tenant's own, which the guard keeps indexed by
/// tenant so this never walks another tenant's; see the module docs for the
/// order.
pub(crate) fn resolve(
    policies: &[FusePolicy],
    config: &FuseConfig,
    tenant: &str,
    service: &str,
) -> Resolved {
    let mut matching: Option<&FusePolicy> = None;
    let mut fallback: Option<&FusePolicy> = None;
    for policy in policies.iter().filter(|policy| policy.tenant == tenant) {
        if policy.name == DEFAULT_POLICY_NAME {
            fallback = Some(policy);
        } else if policy.matches.service.as_deref() == Some(service) {
            let newer = matching.map_or(true, |current| {
                (policy.created_at_ns, &policy.name) > (current.created_at_ns, &current.name)
            });
            if newer {
                matching = Some(policy);
            }
        }
    }
    match matching.or(fallback) {
        Some(policy) => Resolved {
            name: policy.name.clone(),
            limits: policy.limits,
            action: policy.action,
        },
        None => Resolved {
            name: GLOBAL_POLICY_NAME.to_owned(),
            limits: config.limits,
            action: config.action,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(tenant: &str, name: &str, service: Option<&str>, created: u64) -> FusePolicy {
        FusePolicy {
            id: format!("fp_{created:016x}"),
            tenant: tenant.into(),
            name: name.into(),
            matches: PolicyMatch {
                service: service.map(str::to_owned),
            },
            limits: Limits {
                max_steps: Some(created),
                ..Limits::default()
            },
            action: Action::Stop,
            created_at_ns: created,
        }
    }

    #[test]
    fn resolution_prefers_a_service_match_then_the_default_then_the_flags() {
        let config = FuseConfig::default();
        let policies = vec![
            policy("acme", "default", None, 1),
            policy("acme", "search-a", Some("search"), 2),
            policy("acme", "search-b", Some("search"), 3),
            policy("other", "billing", Some("billing"), 4),
        ];
        let search = resolve(&policies, &config, "acme", "search");
        assert_eq!(search.name, "search-b", "the newest matching policy wins");
        assert_eq!(search.limits.max_steps, Some(3));

        let unmatched = resolve(&policies, &config, "acme", "billing");
        assert_eq!(
            unmatched.name, "default",
            "another tenant's match is not ours"
        );

        let none = resolve(&policies, &config, "", "billing");
        assert_eq!(none.name, GLOBAL_POLICY_NAME);
        assert_eq!(none.limits.max_repeats, Some(DEFAULT_MAX_REPEATS));
        assert_eq!(none.action, Action::Warn);
    }

    #[test]
    fn a_grant_replaces_only_the_limits_it_names() {
        let base = Limits {
            max_cost_usd: Some(1.0),
            max_steps: Some(10),
            ..Limits::default()
        };
        let grant = Limits {
            max_cost_usd: Some(5.0),
            ..Limits::default()
        };
        let effective = base.overlaid(&grant);
        assert_eq!(effective.max_cost_usd, Some(5.0));
        assert_eq!(effective.max_steps, Some(10));
        assert!(Limits::default().is_empty() && !effective.is_empty());
    }

    #[test]
    fn limits_that_cannot_be_budgets_are_refused() {
        for (limits, needle) in [
            (json!({"max_steps": 0}), "max_steps must be positive"),
            (
                json!({"max_cost_usd": -1.0}),
                "max_cost_usd must be a positive",
            ),
            (
                json!({"max_cost_usd": 0.0}),
                "max_cost_usd must be a positive",
            ),
            (json!({"max_repeats": 1}), "at least 2"),
        ] {
            let parsed: Limits = serde_json::from_value(limits).expect("parses");
            match parsed.validate("test") {
                Err(Error::Invalid(message)) => assert!(message.contains(needle), "{message}"),
                other => panic!("expected a refusal naming {needle}, got {other:?}"),
            }
        }
        let typo = serde_json::from_value::<Limits>(json!({"max_step": 3}));
        assert!(typo.is_err(), "an unknown limit is a typo, not a no-op");
        let fine: Limits =
            serde_json::from_value(json!({"max_repeats": 2, "max_cost_usd": 0.5})).expect("parses");
        assert!(fine.validate("test").is_ok());
    }

    #[test]
    fn a_policy_must_say_which_runs_it_is_for() {
        let spec = |name: &str, service: Option<&str>| PolicySpec {
            name: name.into(),
            matches: PolicyMatch {
                service: service.map(str::to_owned),
            },
            ..PolicySpec::default()
        };
        assert!(spec("default", None).validate().is_ok());
        assert!(spec("search", Some("search")).validate().is_ok());
        for (bad, needle) in [
            (spec("search", None), "matches nothing"),
            (spec("default", Some("search")), "takes no match"),
            (spec("global", None), "server-wide default"),
            (spec("", Some("x")), "1 to 64 bytes"),
            (spec("x", Some("")), "must not be empty"),
            (spec("x", Some(&"s".repeat(257))), "at most 256 bytes"),
        ] {
            match bad.validate() {
                Err(Error::Invalid(message)) => assert!(message.contains(needle), "{message}"),
                other => panic!("expected {needle}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_policy_has_the_documented_wire_shape() {
        let wire =
            serde_json::to_value(policy("acme", "search", Some("search"), 7)).expect("serializes");
        assert_eq!(
            wire,
            json!({"id": "fp_0000000000000007", "tenant": "acme", "name": "search",
                   "match": {"service": "search"}, "limits": {"max_steps": 7},
                   "action": "stop", "created_at_ns": 7})
        );
    }
}
