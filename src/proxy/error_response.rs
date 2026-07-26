use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::core::model::WireFormat;

pub fn wire_error(wire: WireFormat, status: StatusCode, message: &str) -> Response {
    let body = match wire {
        WireFormat::OpenAi => json!({
            "error": { "message": message, "type": "invalid_request_error", "code": null, "param": null }
        }),
        WireFormat::Anthropic => json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": message }
        }),
    };
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::WireFormat;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn openai_shape() {
        let resp = wire_error(WireFormat::OpenAi, StatusCode::BAD_REQUEST, "nope");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let j = body_json(resp).await;
        assert_eq!(j["error"]["message"], "nope");
        assert_eq!(j["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn anthropic_shape() {
        let resp = wire_error(
            WireFormat::Anthropic,
            StatusCode::SERVICE_UNAVAILABLE,
            "down",
        );
        let j = body_json(resp).await;
        assert_eq!(j["type"], "error");
        assert_eq!(j["error"]["message"], "down");
    }
}
