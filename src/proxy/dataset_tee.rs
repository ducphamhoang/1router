use axum::body::Body;
use bytes::Bytes;
use futures::StreamExt;

/// Accumulates chunks and fires `on_complete` exactly once. A plain
/// `futures::stream::unfold` closure only runs again on the *next* poll,
/// which a dropped/abandoned body (the client disconnecting mid-stream,
/// by far the most common truncation in practice) never gets - so the
/// guarantee has to live in a value's `Drop` impl, not just in the two
/// "reached a real terminal stream item" cases (`None`/`Err`).
struct FireOnDrop<F: FnOnce(Bytes, bool)> {
    acc: Vec<u8>,
    cb: Option<F>,
}

impl<F: FnOnce(Bytes, bool)> FireOnDrop<F> {
    /// Explicit terminal firing (stream ended cleanly or errored) - disarms
    /// the drop guard so it doesn't fire a second time when this value is
    /// later dropped.
    fn fire_now(&mut self, complete: bool) {
        if let Some(cb) = self.cb.take() {
            cb(Bytes::from(std::mem::take(&mut self.acc)), complete);
        }
    }
}

impl<F: FnOnce(Bytes, bool)> Drop for FireOnDrop<F> {
    /// Safety net for "the body was dropped without ever reaching a
    /// terminal stream item" - a client disconnect. If `fire_now` already
    /// ran, `cb` is `None` and this is a no-op.
    fn drop(&mut self) {
        self.fire_now(false);
    }
}

enum TeeState<S, F: FnOnce(Bytes, bool)> {
    Live(S, FireOnDrop<F>),
    Done,
}

/// Wraps `body` so every chunk still reaches the client unchanged, while a
/// full copy is accumulated and handed to `on_complete` exactly once:
/// `complete: true` if the stream ran to its natural end, `false` if the
/// upstream connection errored mid-stream *or* the client disconnected and
/// the body was dropped before ending. Works identically for a
/// single-chunk (non-streaming) body and a genuinely streamed one - both
/// go through the same `Body::into_data_stream()`/`from_stream()` path,
/// so there is no special-casing needed at this layer for either case.
pub fn tee(
    body: Body,
    on_complete: impl FnOnce(Bytes, bool) + Send + 'static,
) -> Body {
    let inner = body.into_data_stream();
    let guard = FireOnDrop { acc: Vec::new(), cb: Some(on_complete) };
    let stream = futures::stream::unfold(TeeState::Live(inner, guard), |state| async move {
        match state {
            TeeState::Live(mut inner, mut guard) => match inner.next().await {
                Some(Ok(chunk)) => {
                    guard.acc.extend_from_slice(&chunk);
                    Some((Ok(chunk), TeeState::Live(inner, guard)))
                }
                Some(Err(e)) => {
                    guard.fire_now(false);
                    Some((Err(e), TeeState::Done))
                }
                None => {
                    guard.fire_now(true);
                    None
                }
            },
            TeeState::Done => None,
        }
    });
    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::sync::{Arc, Mutex};

    fn recorder() -> (Arc<Mutex<Vec<(Bytes, bool)>>>, impl FnOnce(Bytes, bool) + Send + 'static) {
        let calls: Arc<Mutex<Vec<(Bytes, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let cb = move |bytes: Bytes, complete: bool| {
            calls2.lock().unwrap().push((bytes, complete));
        };
        (calls, cb)
    }

    async fn collect_body(body: Body) -> Vec<u8> {
        let bytes = body.collect().await.unwrap().to_bytes();
        bytes.to_vec()
    }

    #[tokio::test]
    async fn tee_forwards_every_chunk_unchanged() {
        let chunks = vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"hello ")),
            Ok(Bytes::from_static(b"world")),
        ];
        let body = Body::from_stream(futures::stream::iter(chunks));
        let (_calls, cb) = recorder();
        let wrapped = tee(body, cb);
        let out = collect_body(wrapped).await;
        assert_eq!(out, b"hello world");
    }

    #[tokio::test]
    async fn tee_fires_the_callback_once_with_the_full_accumulated_bytes_and_complete_true_when_the_stream_ends_cleanly() {
        let chunks = vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"a")),
            Ok(Bytes::from_static(b"b")),
        ];
        let body = Body::from_stream(futures::stream::iter(chunks));
        let (calls, cb) = recorder();
        let wrapped = tee(body, cb);
        let _ = collect_body(wrapped).await;

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0.as_ref(), b"ab");
        assert!(recorded[0].1, "complete should be true");
    }

    #[tokio::test]
    async fn tee_fires_the_callback_with_complete_false_on_a_mid_stream_upstream_error() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"a")),
            Ok(Bytes::from_static(b"b")),
            Err(std::io::Error::other("boom")),
        ];
        let body = Body::from_stream(futures::stream::iter(chunks));
        let (calls, cb) = recorder();
        let wrapped = tee(body, cb);

        // Drain manually (collect() would bail on the first Err) so we can
        // assert the stream returns None on the *next* poll after the
        // error, rather than re-polling the already-errored inner stream.
        let mut stream = wrapped.into_data_stream();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0.as_ref(), b"ab");
        assert!(!recorded[0].1, "complete should be false on error");
    }

    #[tokio::test]
    async fn tee_fires_the_callback_with_complete_false_if_the_body_is_dropped_before_it_ever_ends() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"partial")),
            Ok(Bytes::from_static(b"-never-read")),
        ];
        let body = Body::from_stream(futures::stream::iter(chunks));
        let (calls, cb) = recorder();
        let wrapped = tee(body, cb);

        // Poll partway (consume exactly one chunk), then drop without
        // reaching the end - simulates a client disconnect.
        let mut stream = wrapped.into_data_stream();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.as_ref(), b"partial");
        drop(stream);

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1, "the drop guard must fire exactly once");
        assert_eq!(recorded[0].0.as_ref(), b"partial");
        assert!(!recorded[0].1, "complete should be false on an abandoned stream");
    }

    #[tokio::test]
    async fn tee_never_fires_the_callback_twice() {
        let chunks = vec![Ok::<_, std::io::Error>(Bytes::from_static(b"x"))];
        let body = Body::from_stream(futures::stream::iter(chunks));
        let (calls, cb) = recorder();
        let wrapped = tee(body, cb);
        let mut stream = wrapped.into_data_stream();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.is_none()); // reaches the natural end -> fire_now(true)
        drop(stream); // must NOT fire again

        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn tee_handles_a_single_fixed_chunk_body_identically_to_a_streamed_one() {
        let body = Body::from(Bytes::from_static(b"whole thing at once"));
        let (calls, cb) = recorder();
        let wrapped = tee(body, cb);
        let out = collect_body(wrapped).await;
        assert_eq!(out, b"whole thing at once");

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0.as_ref(), b"whole thing at once");
        assert!(recorded[0].1);
    }
}
