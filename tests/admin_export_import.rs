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
            "base_url": "http://127.0.0.1:1", "api_key": "sk-real", "upstream_model": "m"
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

/// `Provider`/`PoolMember` gained `dataset_logging`/`dataset_logging_override`
/// after the round-trip above was written; `import_config` has its own
/// hand-rolled INSERT SQL separate from the admin CRUD endpoints, so this
/// specifically proves *that* path carries the new fields, not just that
/// the struct round-trips through serde.
#[tokio::test]
async fn export_import_round_trips_dataset_logging_fields() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let dump = json!({
        "providers": [{
            "id": "p1", "name": "P1", "wire_format": "openai", "kind": "passthrough",
            "base_url": "http://127.0.0.1:1", "api_key": "sk-real", "upstream_model": "m",
            "dataset_logging": true,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }],
        "pools": [{
            "id": "gpt-4o", "wire_format": "openai",
            "created_at": "2026-01-01T00:00:00Z", "sticky_limit": null
        }],
        "members": [{
            "pool_id": "gpt-4o", "provider_id": "p1", "priority": 1, "model_override": null,
            "dataset_logging_override": false
        }]
    });

    let imp = client
        .post(format!("{}/admin/import", app.base_url))
        .header(&k, &v)
        .json(&dump)
        .send()
        .await
        .unwrap();
    assert_eq!(imp.status(), 200);

    let exported: serde_json::Value = client
        .get(format!("{}/admin/export", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exported["providers"][0]["dataset_logging"], true);
    assert_eq!(exported["members"][0]["dataset_logging_override"], false);
}

/// A dump that predates this feature (no `dataset_logging`/
/// `dataset_logging_override` keys at all) must still import successfully,
/// with both fields defaulting rather than the request being rejected.
#[tokio::test]
async fn import_accepts_a_dump_missing_the_dataset_logging_keys() {
    let app = spawn_app().await;
    let client = reqwest::Client::new();
    let (k, v) = auth_header(&app.secret);

    let dump = json!({
        "providers": [{
            "id": "p1", "name": "P1", "wire_format": "openai", "kind": "passthrough",
            "base_url": "http://127.0.0.1:1", "api_key": "sk-real", "upstream_model": "m",
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }],
        "pools": [{
            "id": "gpt-4o", "wire_format": "openai",
            "created_at": "2026-01-01T00:00:00Z", "sticky_limit": null
        }],
        "members": [{
            "pool_id": "gpt-4o", "provider_id": "p1", "priority": 1, "model_override": null
        }]
    });

    let imp = client
        .post(format!("{}/admin/import", app.base_url))
        .header(&k, &v)
        .json(&dump)
        .send()
        .await
        .unwrap();
    assert_eq!(imp.status(), 200);

    let exported: serde_json::Value = client
        .get(format!("{}/admin/export", app.base_url))
        .header(&k, &v)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exported["providers"][0]["dataset_logging"], false);
    assert!(exported["members"][0]["dataset_logging_override"].is_null());
}
