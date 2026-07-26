//! Interactive terminal onboarding wizard.
//!
//! Design: docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md
//!
//! This module contains no business logic of its own: it sequences calls into
//! `providers::queries`, `pools::queries` and
//! `providers::adapter::codex::oauth`, and owns only the prompt UI plus a few
//! pure helpers (which is where all of its unit tests live).

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_reachable() {
        // Placeholder: replaced by real helper tests in P5-3.
        assert!(true);
    }
}
