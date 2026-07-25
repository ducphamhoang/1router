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
            "kind": "passthrough", "base_url": "https://api.example.com",
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
