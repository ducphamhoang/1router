mod common;
use common::{auth_header, spawn_app};
use serde_json::json;

#[tokio::test]
async fn export_then_import_roundtrip() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    client
        .post(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .json(&json!({
            "id": "p1", "name": "P1", "wire_format": "openai", "kind": "passthrough",
            "base_url": "https://x", "api_key": "sk-real", "upstream_model": "m"
        }))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{}/admin/pools", app.base_url))
        .header(&k, &v)
        .json(&json!({ "id": "gpt-4o", "wire_format": "openai" }))
        .send()
        .await
        .unwrap();
    client
        .put(format!("{}/admin/pools/gpt-4o/members", app.base_url))
        .header(&k, &v)
        .json(&json!({ "provider_id": "p1", "priority": 1 }))
        .send()
        .await
        .unwrap();

    let export = client
        .get(format!("{}/admin/export", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    assert_eq!(export.status(), 200);
    let dump: serde_json::Value = export.json().await.unwrap();
    assert_eq!(dump["providers"].as_array().unwrap().len(), 1);
    // export includes the real key for backup fidelity
    assert_eq!(dump["providers"][0]["api_key"], "sk-real");
    assert_eq!(dump["members"].as_array().unwrap().len(), 1);

    // wipe and re-import
    client
        .delete(format!("{}/admin/pools/gpt-4o", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();
    client
        .delete(format!("{}/admin/providers/p1", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap();

    let imp = client
        .post(format!("{}/admin/import", app.base_url))
        .header(&k, &v)
        .json(&dump)
        .send()
        .await
        .unwrap();
    assert_eq!(imp.status(), 200);

    let list: serde_json::Value = client
        .get(format!("{}/admin/providers", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
}
