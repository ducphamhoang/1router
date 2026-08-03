mod common;
use common::{auth_header, spawn_app};
use serde_json::json;

#[tokio::test]
async fn create_list_and_mask_api_key() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let create = client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "P1", "wire_format": "openai",
            "kind": "passthrough", "base_url": "http://127.0.0.1:1",
            "api_key": "sk-supersecret", "upstream_model": "gpt-4o"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 201);
    let body: serde_json::Value = create.json().await.unwrap();
    assert_ne!(body["api_key"], "sk-supersecret"); // masked
    assert!(body["api_key"].as_str().unwrap().contains("***"));

    let list = client
        .get(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    let arr: serde_json::Value = list.json().await.unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert!(arr[0]["api_key"].as_str().unwrap().contains("***"));
}

#[tokio::test]
async fn get_missing_provider_is_404() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);
    let resp = client
        .get(format!("{}/admin/providers/nope", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// Regression test for the axum path-param syntax bug (`{id}` vs `:id`) caught
// by the Phase 1 review: without this, get_missing_provider_is_404 above and
// the export/import roundtrip test both passed for the wrong reason, because
// EVERY /admin/providers/:id request 404'd, existing or not.
#[tokio::test]
async fn get_patch_delete_existing_provider_by_id_succeeds() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "P1", "wire_format": "openai",
            "kind": "passthrough", "base_url": "http://127.0.0.1:1",
            "api_key": "sk-secret", "upstream_model": "gpt-4o"
        }))
        .send()
        .await
        .unwrap();

    let get = client
        .get(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 200);

    let patch = client
        .patch(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .json(&json!({ "name": "P1 renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 200);

    let delete = client
        .delete(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(delete.status(), 204);

    let get_after = client
        .get(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(get_after.status(), 404);
}
