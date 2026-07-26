mod common;

#[tokio::test]
async fn login_then_authenticated_providers_call_succeeds() {
    let app = common::spawn_app().await;
    let client = reqwest::Client::new();

    let login = client
        .post(format!("{}/admin/auth/login", app.base_url))
        .header("X-Requested-With", "1router-ui")
        .json(&serde_json::json!({
            "username": "admin",
            "password": app.admin_password,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let set_cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .expect("login response should set a session cookie")
        .to_str()
        .unwrap();
    assert!(
        set_cookie.starts_with("admin_session="),
        "login response should set admin_session cookie"
    );
    let session_cookie = set_cookie.split(';').next().unwrap();

    let providers = client
        .get(format!("{}/admin/providers", app.base_url))
        .header(reqwest::header::COOKIE, session_cookie)
        .send()
        .await
        .unwrap();

    assert_eq!(providers.status(), reqwest::StatusCode::OK);
}
