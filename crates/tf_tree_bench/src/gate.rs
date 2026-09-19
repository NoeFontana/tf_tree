//! One spelling for a gate's outcome, and one exit code per meaning.
//!
//! CI reads only the process exit code, so "this host cannot evaluate the
//! criterion" (`2`) must not arrive byte-identical to "the code regressed" (`1`).
//!
//! [`Outcome::Refused`] is [`Status::Unavailable`] for a whole run. `Pass` and
//! `Fail` have no `Status` analogue: a row records a measurement, a gate a
//! verdict. Both refusal constructors quote a probed [`Fitness`] reason, never a
//! literal, so a refusal cannot be typed to make a job green.

use core::fmt;

#[cfg(doc)]
use crate::report::Status;
use crate::report::{Fitness, Ground, Sensitivity};

/// `0` — the gate was evaluated and the criterion holds.
pub const EXIT_PASS: i32 = 0;
/// `1` — the gate was evaluated and the criterion does **not** hold.
pub const EXIT_FAIL: i32 = 1;
/// `2` — the gate was **not** evaluated. Never a pass and never a failure of
/// the code under test.
pub const EXIT_REFUSED: i32 = 2;

/// What a gate binary concluded.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Evaluated; the criterion holds.
    Pass(String),
    /// Evaluated; the criterion does not hold. The string is the operator-facing
    /// reason and should carry the measured number that missed.
    Fail(String),
    /// Not evaluated. `ground` is the machine-checked claim this rests on and
    /// `why` is the probe's own words.
    Refused {
        /// The machine-checked claim this refusal rests on.
        ground: Ground,
        /// The probe's own words for why. Never a literal — see the module doc.
        why: String,
    },
}

impl Outcome {
    /// Refused because this host cannot produce a trustworthy number on the
    /// criterion's sensitivity axis, or [`None`] if it can.
    ///
    /// Returns `Option` so a fit host (always `HostIndependent`) cannot be
    /// handed a refusal with an empty reason.
    #[must_use]
    pub fn refused_on_host(fitness: &Fitness, sensitivity: Sensitivity) -> Option<Outcome> {
        let (fit, _axis, why) = fitness.axis(sensitivity);
        if fit {
            return None;
        }
        Some(Outcome::Refused {
            ground: Ground::HostFitness,
            why,
        })
    }

    /// Refused because the host has fewer physical cores than the criterion
    /// needs, or [`None`]. Quotes [`Fitness::core_reason`] and nothing else.
    #[must_use]
    pub fn refused_on_cores(fitness: &Fitness) -> Option<Outcome> {
        fitness.core_reason.clone().map(|why| Outcome::Refused {
            ground: Ground::HostCores,
            why,
        })
    }

    /// The exit code this outcome leaves the process with.
    #[must_use]
    pub fn code(&self) -> i32 {
        match self {
            Outcome::Pass(_) => EXIT_PASS,
            Outcome::Fail(_) => EXIT_FAIL,
            Outcome::Refused { .. } => EXIT_REFUSED,
        }
    }

    /// Print the verdict on stdout and leave the process with [`Self::code`].
    ///
    /// The one exit point a gate binary should have, so it cannot print
    /// `REFUSED` and then exit `1` through `anyhow`. Uses a locked handle rather
    /// than `println!`: library modules return a `String`, this is the exception.
    pub fn report_and_exit(self) -> ! {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{self}");
        let _ = out.flush();
        std::process::exit(self.code())
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Pass(why) => write!(f, "PASS — {why}"),
            Outcome::Fail(why) => write!(f, "FAIL — {why}"),
            Outcome::Refused { ground, why } => {
                write!(f, "REFUSED ({ground:?}) — {why}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::*;

    #[test]
    fn pass_fail_and_refused_have_three_distinct_codes() {
        let codes = [
            Outcome::Pass("x".into()).code(),
            Outcome::Fail("x".into()).code(),
            Outcome::Refused {
                ground: Ground::HostFitness,
                why: "x".into(),
            }
            .code(),
        ];
        assert_eq!(codes, [0, 1, 2], "the three meanings must not collide");
        assert_ne!(
            EXIT_REFUSED, EXIT_FAIL,
            "a host-starved refusal must not read as a regression"
        );
        assert_ne!(
            EXIT_REFUSED, EXIT_PASS,
            "a refusal must not read as a passing gate"
        );
    }

    #[test]
    fn a_host_refusal_quotes_the_probe_and_not_a_literal() {
        let f = Fitness::probe(1);
        let (fit, _, expected) = f.axis(Sensitivity::AbsoluteTiming);
        match Outcome::refused_on_host(&f, Sensitivity::AbsoluteTiming) {
            Some(Outcome::Refused { ground, why }) => {
                assert!(!fit, "a refusal was produced for a fit host");
                assert!(matches!(ground, Ground::HostFitness));
                assert_eq!(
                    why, expected,
                    "the refusal text must come from Fitness::axis, so it moves                      when the host does"
                );
                assert!(!why.is_empty(), "a refusal must state a reason");
            }
            None => assert!(fit, "no refusal was produced for an unfit host"),
            Some(other) => panic!("expected a refusal, got {other}"),
        }
    }

    #[test]
    fn a_host_independent_axis_cannot_be_refused_for_unfitness() {
        let f = Fitness::probe(1);
        assert!(
            Outcome::refused_on_host(&f, Sensitivity::HostIndependent).is_none(),
            "HostIndependent is fit on every host, so there are no grounds to refuse"
        );
    }

    #[test]
    fn a_core_refusal_is_none_when_the_host_has_enough() {
        let f = Fitness::probe(1);
        match Outcome::refused_on_cores(&f) {
            None => assert!(
                f.core_reason.is_none(),
                "no refusal, but the probe named a core shortfall"
            ),
            Some(Outcome::Refused { why, .. }) => {
                assert_eq!(
                    Some(&why),
                    f.core_reason.as_ref(),
                    "the refusal must be the probe's own string"
                );
            }
            Some(other) => panic!("expected a refusal, got {other}"),
        }
    }

    #[test]
    fn display_names_the_ground_so_a_log_says_what_was_checked() {
        let o = Outcome::Refused {
            ground: Ground::HostCores,
            why: "two cores".into(),
        };
        let s = o.to_string();
        assert!(s.starts_with("REFUSED"), "{s}");
        assert!(s.contains("HostCores"), "the ground must be legible: {s}");
        assert!(s.contains("two cores"), "the reason must survive: {s}");
    }
}
