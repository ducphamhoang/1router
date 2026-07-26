mod common;

// These tests run against REAL provider APIs and cost money / can be rate-limited.
// They are excluded from the fast loop via #[ignore] and must be run explicitly:
//   cargo test --test e2e_real_providers -- --ignored
//
// Requires env: E2E_OPENAI_KEY, E2E_OPENAI_BASE, E2E_ANTHROPIC_KEY, E2E_ANTHROPIC_BASE.

#[tokio::test]
#[ignore = "real-provider e2e; run manually with sample keys"]
async fn openai_passthrough_real() {
    let key = std::env::var("E2E_OPENAI_KEY").expect("E2E_OPENAI_KEY");
    let base = std::env::var("E2E_OPENAI_BASE").expect("E2E_OPENAI_BASE");
    let _ = (key, base);
    // 1. spawn_app, create a passthrough provider pointing at `base` with `key`,
    // 2. create pool "gpt-real" wire_format=openai with that member,
    // 3. POST /v1/chat/completions and assert 200 + a choices[] payload.
    unimplemented!("fill in when sample keys are provided");
}

#[tokio::test]
#[ignore = "real-provider e2e; run manually with sample keys"]
async fn anthropic_passthrough_real() {
    let key = std::env::var("E2E_ANTHROPIC_KEY").expect("E2E_ANTHROPIC_KEY");
    let base = std::env::var("E2E_ANTHROPIC_BASE").expect("E2E_ANTHROPIC_BASE");
    let _ = (key, base);
    // Same shape via /v1/messages against a real Anthropic-compatible upstream.
    unimplemented!("fill in when sample keys are provided");
}

#[tokio::test]
#[ignore = "real-provider e2e; failover with an intentionally invalid key first"]
async fn failover_real() {
    // Pool: [invalid-key provider @priority 1, valid provider @priority 2];
    // assert the request still succeeds via the second provider.
    unimplemented!("fill in when sample keys are provided");
}

#[tokio::test]
#[ignore = "real-provider e2e; Codex against a real ChatGPT account"]
async fn codex_end_to_end_real() {
    // OAuth start/complete, one real Responses-API chat request through the transform,
    // and (if feasible) a manual refresh trigger.
    unimplemented!("fill in when a ChatGPT account is available");
}
