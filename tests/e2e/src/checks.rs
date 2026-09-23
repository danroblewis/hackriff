//! Several named assertions over **one** expensive fixture, inside one `#[test]`.
//!
//! **Why this exists.** Eight test files share an expensive replay through a `static OnceLock`.
//! Under `cargo test` that worked: one process, one binary, one decode. Under **nextest**, which
//! this repo has run hk-e2e on since T-631, *every test is its own process*, so a `OnceLock` is
//! initialised once **per test** and the sharing is silently a no-op. The evidence is in the gate
//! log: if the cache worked, one test per module would be slow and the rest instant, and instead
//! every test in a module costs the same — `acceptance_m0::fm_band_2026_09_15`'s six tests are
//! 19.2, 19.1, 19.0, 18.9, 18.9 and 18.6 s, six replays of one 216 MB recording per gate
//! (`docs/test-speed-review-2026-09-22.md` §2.4).
//!
//! **What this does about it.** The module's assertions move into one `#[test]` that builds the
//! fixture once and runs each former test as a named [`Checks::check`]. The point of the type,
//! rather than just concatenating the bodies, is that **the assertions stay separate and named**:
//! each check runs under `catch_unwind`, so one failing check does not hide the ones after it, and
//! the panic reports every failure by the name the `#[test]` used to have, with its own message.
//! That is strictly more than a straight merge would tell you, and only slightly less than
//! separate tests — what is lost is the ability to run one of them alone by name.
//!
//! **What it is NOT for.** Tests that are independently expensive, tests that must be able to run
//! alone, and anything where the shared value is cheap. The whole justification is one costly
//! setup; without that, separate `#[test]`s are better in every way and nextest will run them in
//! parallel.

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Named assertions over one shared, expensive fixture. See the module docs.
///
/// ```no_run
/// let mut c = hk_e2e::Checks::new("fm_band");
/// let run = (); // the expensive fixture, built once
/// c.check("every_measured_emission_is_found_blind", || { let _ = &run; });
/// c.check("measured_silence_is_not_catalogued", || { let _ = &run; });
/// c.finish();
/// ```
#[derive(Debug)]
pub struct Checks {
    subject: &'static str,
    ran: Vec<&'static str>,
    failures: Vec<(&'static str, String)>,
}

impl Checks {
    /// A new set of checks over `subject` (the module or fixture they all share).
    pub fn new(subject: &'static str) -> Self {
        Self {
            subject,
            ran: Vec::new(),
            failures: Vec::new(),
        }
    }

    /// Run one named check. A panic inside it is recorded against `name` and the next check still
    /// runs — the property that keeps this as informative as separate `#[test]`s.
    pub fn check(&mut self, name: &'static str, f: impl FnOnce()) {
        self.ran.push(name);
        if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
            let msg = payload
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            self.failures.push((name, msg));
        }
    }

    /// Whether any check has failed so far — for a caller that wants to stop early.
    pub fn failed(&self) -> bool {
        !self.failures.is_empty()
    }

    /// Panic if any check failed, naming every one of them and its message.
    ///
    /// Takes `self` by value so a set of checks cannot be built and then silently not asserted;
    /// forgetting the call is a `let _ =` a reviewer can see, not an invisible green.
    #[track_caller]
    pub fn finish(self) {
        if self.failures.is_empty() {
            return;
        }
        let mut out = format!(
            "{} of {} checks failed in `{}` (each was its own #[test] before they were merged \
             onto one shared fixture — see hk_e2e::Checks):\n",
            self.failures.len(),
            self.ran.len(),
            self.subject,
        );
        for (name, msg) in &self.failures {
            out.push_str(&format!(
                "\n  ✖ {}::{name}\n      {}\n",
                self.subject,
                msg.trim()
            ));
        }
        panic!("{out}");
    }
}

#[cfg(test)]
mod tests {
    use super::Checks;

    #[test]
    fn all_passing_checks_finish_quietly() {
        let mut c = Checks::new("subject");
        c.check("a", || {});
        c.check("b", || {});
        assert!(!c.failed());
        c.finish();
    }

    #[test]
    fn a_failing_check_does_not_stop_the_ones_after_it() {
        let mut c = Checks::new("subject");
        let mut later_ran = false;
        c.check("a", || panic!("first blew up"));
        c.check("b", || later_ran = true);
        assert!(later_ran, "the check after a failing one must still run");
        assert!(c.failed());
    }

    #[test]
    #[should_panic(expected = "2 of 3 checks failed")]
    fn finish_names_every_failure() {
        let mut c = Checks::new("subject");
        c.check("a", || panic!("first blew up"));
        c.check("b", || {});
        c.check("c", || assert_eq!(1, 2, "second blew up"));
        c.finish();
    }

    #[test]
    #[should_panic(expected = "subject::a")]
    fn finish_names_the_check_that_failed() {
        let mut c = Checks::new("subject");
        c.check("a", || panic!("boom"));
        c.finish();
    }
}
