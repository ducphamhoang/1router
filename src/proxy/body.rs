use axum::body::Body;
use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};

use crate::core::error::AppError;

/// Buffers a request body, enforcing `cap` *while reading* - the stream is
/// abandoned as soon as it passes `cap`, so an oversized (e.g. chunked,
/// no `Content-Length`) body can't be pulled into memory in full first
/// (SEC-02). The raw `Body` extractor bypasses axum's `DefaultBodyLimit`,
/// so this is the only limit on `/v1/*` bodies.
pub async fn buffer_body(body: Body, cap: usize) -> Result<Bytes, AppError> {
    match Limited::new(body, cap).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            Err(AppError::BadRequest("request body exceeds limit".into()))
        }
        Err(e) => Err(AppError::BadRequest(format!("failed to read body: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[tokio::test]
    async fn accepts_a_body_up_to_the_cap() {
        let got = buffer_body(Body::from("12345"), 5).await.unwrap();
        assert_eq!(&got[..], b"12345");
    }

    #[tokio::test]
    async fn rejects_an_oversized_body_without_reading_all_of_it() {
        // An endless chunked stream: collecting it without a limit would
        // never finish, so returning at all proves the cap is enforced
        // mid-stream rather than after buffering.
        let endless = stream::repeat_with(|| Ok::<_, std::io::Error>(Bytes::from_static(&[b'x'; 1024])));
        let err = buffer_body(Body::from_stream(endless), 64 * 1024).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(m) if m.contains("exceeds limit")));
    }
}
