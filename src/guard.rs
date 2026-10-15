//! Run-guard vocabulary: the verdict a run is under, and the signal an ingest
//! raises about it.
//!
//! A run (see [`crate::semconv::run_id`]) can be allowed to continue, warned,
//! or told to stop. The ingest path reports those verdicts on
//! [`crate::Admission::signals`], one [`Signal`] per run in the batch whose
//! verdict is not [`Verdict::Continue`].
//!
//! Only this shared vocabulary is defined so far: nothing in this build
//! evaluates runs, so every admission's signal list is empty.

use serde::{Deserialize, Serialize};

/// What a run should do next.
///
/// Ordered by severity — `Continue < Warn < Stop` — so the most severe of
/// several verdicts is their `max()`.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Nothing to report; the run may proceed.
    #[default]
    Continue,
    /// The run is approaching a limit; it may proceed.
    Warn,
    /// The run crossed a limit and should stop.
    Stop,
}

impl Verdict {
    /// The lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Warn => "warn",
            Self::Stop => "stop",
        }
    }
}

/// A verdict about one run, raised while admitting a batch that touched it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// The run's tenant; empty is the default tenant.
    pub tenant: String,
    /// The run id ([`crate::semconv::run_id`]).
    pub run: String,
    /// The verdict.
    pub verdict: Verdict,
    /// Why, one human-readable reason per line of evidence.
    pub reasons: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verdicts_order_by_severity_and_serialize_lowercase() {
        assert!(Verdict::Continue < Verdict::Warn && Verdict::Warn < Verdict::Stop);
        assert_eq!(
            [Verdict::Warn, Verdict::Stop, Verdict::Continue]
                .into_iter()
                .max(),
            Some(Verdict::Stop)
        );
        for verdict in [Verdict::Continue, Verdict::Warn, Verdict::Stop] {
            assert_eq!(
                serde_json::to_value(verdict).expect("serializes"),
                json!(verdict.as_str())
            );
        }
        assert_eq!(Verdict::default(), Verdict::Continue);
    }

    #[test]
    fn a_signal_has_the_documented_wire_shape() {
        let signal = Signal {
            tenant: "acme".into(),
            run: "sess-1".into(),
            verdict: Verdict::Stop,
            reasons: vec!["max_steps 50 reached".into()],
        };
        let wire = serde_json::to_value(&signal).expect("serializes");
        assert_eq!(
            wire,
            json!({"tenant": "acme", "run": "sess-1", "verdict": "stop",
                   "reasons": ["max_steps 50 reached"]})
        );
        let back: Signal = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(back, signal);
    }
}
