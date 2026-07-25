use axum::body::Body;
use bytes::Bytes;
use http_body_util::BodyExt;

use crate::core::error::AppError;

pub async fn buffer_body(body: Body, cap: usize) -> Result<Bytes, AppError> {
    let collected = body
        .collect()
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read body: {e}")))?
        .to_bytes();

    if collected.len() > cap {
        return Err(AppError::BadRequest("request body exceeds limit".into()));
    }

    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[tokio::test]
    async fn buffers_small_body() {
        let b = Body::from("hello");
        let bytes = buffer_body(b, 1024).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn rejects_oversized_body() {
        let big = vec![b'x'; 100];
        let b = Body::from(big);
        let res = buffer_body(b, 10).await;
        assert!(matches!(
            res,
            Err(crate::core::error::AppError::BadRequest(_))
        ));
    }
}
