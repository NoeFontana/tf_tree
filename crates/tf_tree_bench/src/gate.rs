//! One spelling for a gate's outcome, and one exit code per meaning.
//!
//! # Why this exists
//!
//! This crate already classifies *rows*: [`Fitness`] decides whether the host
//! can produce a trustworthy number on a given axis, [`Ground`] is the
//! machine-checked claim a refusal rests on, and [`Status`] is
//! Measured/Indicative/Unavailable. What had no classification was the only
//! interface CI actually reads — **the process exit code**.
//!
//! `reclaim_latency` got this right and nothing else did: it exits `2` when the
//! run is INVALID and `1` when the gate FAILs. Every other gate binary in this
//! crate leaves through `anyhow`'s `Termination` path or a bare
//! `std::process::exit(1)`, so *"this host cannot evaluate the criterion"* and
//! *"the code regressed"* arrive at a workflow **byte-identical**.
//!
//! That distinction is the whole of what this project can offer on a host that
//! cannot run its own gates. A refusal that reads as a failure trains everyone
//! to ignore a red job; a refusal that reads as a pass is worse, because it is
//! a gate that cannot fail. `docs/PHASE5.md` §12's own history has both.
//!
//! # The correspondence, stated so nobody adds a fourth spelling
//!
//! [`Outcome::Refused`] is [`Status::Unavailable`] for a whole run.
//! [`Outcome::Pass`] and [`Outcome::Fail`] have **no** `Status` analogue,
//! deliberately: a report row records a measurement, and a gate records a
//! verdict about one. They are different questions and the types stay separate.
//!
//! # A refusal is a measurement, not a literal
//!
//! Both refusal constructors take a probed [`Fitness`] and quote *its* reason
//! string rather than a hand-written one. A refusal whose text is a literal
//! goes stale the day the host changes and cannot be distinguished from a
//! refusal somebody typed to make a job green.

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
    /// criterion's own sensitivity axis — or [`None`] if it can.
    ///
    /// The reason is [`Fitness::axis`]'s third element, the same call the
    /// `measured` arm makes, read the other way round.
    ///
    /// **`None` when the host is fit, and that is the whole signature.** An
    /// earlier version returned an `Outcome` unconditionally and threw
    /// `axis`'s verdict away, so `refused_on_host(&f, Sensitivity::HostIndependent)`
    /// — fit on every host by definition — produced
    /// `REFUSED (HostFitness) — ` with an **empty** reason and exit 2. Under
    /// `may-refuse` that is a permanently green gate whose recorded reason is
    /// the empty string; under `must-refuse` it is a gate that can never do
    /// anything else. The doc claimed the opposite ("a gate cannot claim the
    /// host is unfit on an axis the host passes") and the test named for it
    /// never called this function, so it asserted `axis`'s bool and would have
    /// passed against the bug. Returning `Option` makes the caller handle the
    /// fit case instead of being handed a refusal it did not earn.
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

    /// Refused because the host has fewer physical cores than the criterion's
    /// own budget needs — or [`None`] if it has enough.
    ///
    /// Quotes [`Fitness::core_reason`] and **only** that. It is `None` on a host
    /// with enough cores, and this returns `None` in step rather than
    /// substituting a hand-written string: a literal here would be the one
    /// thing the module doc forbids, since it is exactly the text a fit host
    /// would get. An earlier version carried such a literal while three
    /// separate prose claims said both constructors quote the probe.
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
    /// The one exit point a gate binary should have. Printing and exiting are
    /// one call so that a binary cannot report `REFUSED` and then leave through
    /// `anyhow`'s `Termination` path with `1`, which is the defect this module
    /// exists to remove.
    ///
    /// **Written through a locked `stdout` handle rather than `println!`**, and
    /// not to dodge the workspace's `print_stdout` lint: this crate's library
    /// convention is that a module returns a `String` and a binary prints it
    /// (`Fitness::reason_line` and `Report::to_json` are the shape). The
    /// exception is deliberate and is the whole point of the type — if the
    /// printing lived in each binary, so would the chance of printing
    /// `REFUSED` and then exiting `1`, which is the bug being removed. A
    /// caller that wants the string without the exit has [`core::fmt::Display`].
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
        // The whole point: a refusal is not a failure. Asserted rather than
        // left to the constants, because collapsing these two is exactly the
        // regression this module was written to prevent.
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
        // Anti-vacuity, and this test used to be vacuous itself: it asserted
        // `axis()`'s bool and never called the constructor it is named for, so
        // it passed while `refused_on_host` was throwing that bool away and
        // returning `REFUSED (HostFitness) — ` with an empty reason. It calls
        // the constructor now, which is the only form that could have failed.
        let f = Fitness::probe(1);
        assert!(
            Outcome::refused_on_host(&f, Sensitivity::HostIndependent).is_none(),
            "HostIndependent is fit on every host, so there are no grounds to refuse"
        );
    }

    #[test]
    fn a_core_refusal_is_none_when_the_host_has_enough() {
        // The same shape for the other constructor: `core_reason` is `None` on
        // a host with enough cores, and that must produce no refusal rather
        // than a refusal carrying an invented string.
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
