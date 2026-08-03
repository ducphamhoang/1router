mod common;
use common::{auth_header, spawn_app};
use serde_json::json;

#[tokio::test]
async fn creating_a_pool_with_an_empty_or_slash_containing_id_is_rejected() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let empty = client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400);

    let slash = client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "a/b", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();
    assert_eq!(slash.status(), 400);

    let list = client
        .get(format!("{}/admin/pools", app.base_url))
        .header(k, v)
        .send()
        .await
        .unwrap();
    let pools: serde_json::Value = list.json().await.unwrap();
    assert_eq!(pools.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn creating_a_provider_with_an_empty_or_slash_containing_id_is_rejected() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let empty = client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "", "name": "n", "wire_format": "openai", "kind": "passthrough",
            "base_url": "http://127.0.0.1:1", "api_key": "k", "upstream_model": "m"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400);

    let slash = client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "a/b", "name": "n", "wire_format": "openai", "kind": "passthrough",
            "base_url": "http://127.0.0.1:1", "api_key": "k", "upstream_model": "m"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(slash.status(), 400);
}
