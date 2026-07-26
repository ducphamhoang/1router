//! Interactive terminal onboarding wizard.
//!
//! Design: docs/superpowers/specs/2026-07-26-onboarding-wizard-design.md
//!
//! This module contains no business logic of its own: it sequences calls into
//! `providers::queries`, `pools::queries` and
//! `providers::adapter::codex::oauth`, and owns only the prompt UI plus a few
//! pure helpers (which is where all of its unit tests live).

use crate::core::model::PoolMember;

/// Candidate Codex models, in probe order.
///
/// ChatGPT-subscription auth only accepts a backend-specific, account/plan-
/// specific allowlist that is not discoverable from this codebase - the only
/// way to find the right value is to try candidates against a live login.
/// Kept in sync BY HAND with tests/e2e_real_providers.rs::codex_end_to_end_real;
/// if you update one, update the other (see the spec's accepted-risk section).
pub const CANDIDATE_MODELS: [&str; 5] =
    ["gpt-5.4", "gpt-5-codex", "gpt-5.1-codex", "gpt-5", "codex-mini-latest"];

/// Placeholder `upstream_model` for a Codex provider whose real model is not
/// known yet (set at create time, and left in place if every probe fails).
pub const PENDING_MODEL: &str = "pending";

/// Priority for a newly added pool member: 1 in a fresh pool, else
/// max(existing) + 1. Deliberately NOT `len + 1`, which would outrank an
/// existing member whose priority is sparse (e.g. [1, 10] -> 3 jumps 10).
pub fn next_priority(existing: &[PoolMember]) -> i64 {
    existing.iter().map(|m| m.priority).max().unwrap_or(0) + 1
}

/// Accept either a full pasted redirect URL or a bare `code=..&state=..`
/// fragment (users paste both; the browser's address bar gives the former).
pub fn parse_code_and_state(input: &str) -> anyhow::Result<(String, String)> {
    let trimmed = input.trim();
    let query = trimmed.split_once('?').map(|(_, q)| q).unwrap_or(trimmed);
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k.trim() {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                _ => {}
            }
        }
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => anyhow::bail!(
            "could not find both `code` and `state` in the pasted input; \
             paste the full redirect URL, or just `code=...&state=...`"
        ),
    }
}

#[derive(Debug)]
pub enum ProbeOutcome {
    Found(String),
    AllFailed(Vec<(String, u16, String)>),
}

/// Try each model in order, stop at the first HTTP 200.
///
/// Generic over the attempt so the control flow is unit-testable with no
/// network and no real provider; the wizard passes a closure that builds a
/// real adapter request (see P5-6).
pub async fn probe_first_success<F, Fut>(models: &[&str], mut attempt: F) -> ProbeOutcome
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<(u16, String), String>>,
{
    let mut failures = Vec::new();
    for model in models {
        match attempt(model.to_string()).await {
            Ok((200, _)) => return ProbeOutcome::Found(model.to_string()),
            Ok((status, body)) => failures.push((model.to_string(), status, body)),
            // A transport error is just another failed attempt - keep going,
            // the next model may hit a different backend path.
            Err(e) => failures.push((model.to_string(), 0, e)),
        }
    }
    ProbeOutcome::AllFailed(failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::PoolMember;

    fn member(priority: i64) -> PoolMember {
        PoolMember { pool_id: "p".into(), provider_id: "x".into(), priority }
    }

    #[test]
    fn next_priority_is_one_for_an_empty_pool() {
        assert_eq!(next_priority(&[]), 1);
    }

    #[test]
    fn next_priority_is_max_plus_one_not_len_plus_one() {
        // len+1 would return 3 here and silently outrank the priority-10 member.
        assert_eq!(next_priority(&[member(1), member(10)]), 11);
    }

    #[test]
    fn next_priority_ignores_ordering_of_input() {
        assert_eq!(next_priority(&[member(10), member(1)]), 11);
    }

    #[test]
    fn parses_full_redirect_url() {
        let (c, s) = parse_code_and_state(
            "  http://localhost:1455/auth/callback?code=abc123&state=st-9&scope=openid\n",
        )
        .unwrap();
        assert_eq!(c, "abc123");
        assert_eq!(s, "st-9");
    }

    #[test]
    fn parses_bare_query_fragment() {
        let (c, s) = parse_code_and_state("code=abc123&state=st-9").unwrap();
        assert_eq!(c, "abc123");
        assert_eq!(s, "st-9");
    }

    #[test]
    fn parse_errors_when_code_or_state_missing() {
        assert!(parse_code_and_state("state=only").is_err());
        assert!(parse_code_and_state("code=only").is_err());
        assert!(parse_code_and_state("total garbage").is_err());
    }

    #[tokio::test]
    async fn probe_stops_at_first_success_and_skips_the_rest() {
        let tried = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let t = tried.clone();
        let out = probe_first_success(&["a", "b", "c"], move |m| {
            let t = t.clone();
            async move {
                t.lock().unwrap().push(m.clone());
                if m == "b" { Ok((200, "{}".into())) } else { Ok((400, "nope".into())) }
            }
        })
        .await;

        assert!(matches!(out, ProbeOutcome::Found(ref m) if m == "b"));
        assert_eq!(&*tried.lock().unwrap(), &["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn probe_reports_every_failure_when_none_succeed() {
        let out = probe_first_success(&["a", "b"], |m| async move {
            Ok((404, format!("no {m}")))
        })
        .await;

        match out {
            ProbeOutcome::AllFailed(fs) => {
                assert_eq!(fs.len(), 2);
                assert_eq!(fs[0], ("a".into(), 404, "no a".into()));
                assert_eq!(fs[1], ("b".into(), 404, "no b".into()));
            }
            ProbeOutcome::Found(m) => panic!("unexpected success: {m}"),
        }
    }

    #[tokio::test]
    async fn probe_treats_transport_error_as_a_failed_attempt_and_continues() {
        let out = probe_first_success(&["a", "b"], |m| async move {
            if m == "a" { Err("connection reset".into()) } else { Ok((200, "{}".into())) }
        })
        .await;
        assert!(matches!(out, ProbeOutcome::Found(ref m) if m == "b"));
    }

    #[test]
    fn candidate_list_matches_the_e2e_test() {
        // If this list changes, tests/e2e_real_providers.rs must change too -
        // the spec calls them out as a pair that goes stale together.
        assert_eq!(
            CANDIDATE_MODELS,
            ["gpt-5.4", "gpt-5-codex", "gpt-5.1-codex", "gpt-5", "codex-mini-latest"]
        );
    }
}
