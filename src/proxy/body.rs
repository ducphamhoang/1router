use std::error::Error;

use axum::body::Body;
use bytes::Bytes;
use http_body_util::BodyExt;

use crate::core::error::AppError;

fn is_length_limit_error(mut error: &(dyn Error + 'static)) -> bool {
    loop {
        if error.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        match error.source() {
            Some(source) => error = source,
            None => return false,
        }
    }
}

pub async fn buffer_body(body: Body, cap: usize) -> Result<Bytes, AppError> {
    let limited = http_body_util::Limited::new(body, cap);
    limited.collect().await.map(|c| c.to_bytes()).map_err(|e| {
        if is_length_limit_error(e.as_ref()) {
            AppError::PayloadTooLarge("request body exceeds limit".into())
        } else {
            AppError::BadRequest(format!("failed to read body: {e}"))
        }
    })
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
            Err(crate::core::error::AppError::PayloadTooLarge(_))
        ));
    }
}
