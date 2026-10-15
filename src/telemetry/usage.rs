//! Token usage accounting: pull `usage` out of proxied responses as they
//! stream by, store it per request (`usage_log`), and report it per person /
//! model with an estimated cost (`GET /admin/usage`).

use std::collections::{BTreeMap, BTreeSet};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::core::error::AppError;
use crate::core::state::AppState;

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Total input tokens, cached ones included.
    pub input: i64,
    pub output: i64,
    /// The cached subset of `input`.
    pub cached: i64,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0
    }

    fn merge(&mut self, o: Usage) {
        self.input = self.input.max(o.input);
        self.output = self.output.max(o.output);
        self.cached = self.cached.max(o.cached);
    }
}

/// Read a usage object from any of the shapes a proxied reply can have:
/// OpenAI chat (`usage.prompt_tokens/completion_tokens`), Responses
/// (`usage` or `response.usage` with `input_tokens/output_tokens`) and
/// Anthropic (`usage` or `message.usage`, where cache reads/writes are
/// counted outside `input_tokens` and are added back in here).
pub fn usage_of(v: &Value) -> Option<Usage> {
    let u = [&v["usage"], &v["response"]["usage"], &v["message"]["usage"]]
        .into_iter()
        .find(|u| u.is_object())?;
    let n = |k: &str| u[k].as_i64();
    let input = n("prompt_tokens").or_else(|| n("input_tokens"));
    let output = n("completion_tokens").or_else(|| n("output_tokens"));
    if input.is_none() && output.is_none() {
        return None;
    }
    let (mut input, output) = (input.unwrap_or(0), output.unwrap_or(0));
    let cached = if u.get("cache_read_input_tokens").is_some()
        || u.get("cache_creation_input_tokens").is_some()
    {
        let read = n("cache_read_input_tokens").unwrap_or(0);
        input += read + n("cache_creation_input_tokens").unwrap_or(0);
        read
    } else {
        u["prompt_tokens_details"]["cached_tokens"]
            .as_i64()
            .or_else(|| u["input_tokens_details"]["cached_tokens"].as_i64())
            .unwrap_or(0)
    };
    Some(Usage { input, output, cached })
}

/// Incremental reader for a response body: understands SSE (`data: {json}`
/// lines, usage taken as the max seen per field since providers report it
/// cumulatively or split across events), NDJSON, and a plain JSON body.
#[derive(Default)]
pub struct Scanner {
    line: Vec<u8>,
    line_overflow: bool,
    body: Vec<u8>,
    body_overflow: bool,
    saw_line_usage: bool,
    best: Usage,
}

/// A single SSE/NDJSON line or a whole JSON body larger than this is skipped.
const MAX_LINE: usize = 1 << 20;
const MAX_BODY: usize = 4 << 20;

impl Scanner {
    pub fn feed(&mut self, chunk: &[u8]) {
        if !self.body_overflow {
            if self.body.len() + chunk.len() > MAX_BODY {
                self.body_overflow = true;
                self.body = Vec::new();
            } else {
                self.body.extend_from_slice(chunk);
            }
        }
        for part in chunk.split_inclusive(|b| *b == b'\n') {
            if !self.line_overflow {
                if self.line.len() + part.len() > MAX_LINE {
                    self.line_overflow = true;
                    self.line = Vec::new();
                } else {
                    self.line.extend_from_slice(part);
                }
            }
            if part.ends_with(b"\n") {
                if !self.line_overflow {
                    let line = std::mem::take(&mut self.line);
                    self.on_line(&line);
                }
                self.line_overflow = false;
                self.line.clear();
            }
        }
    }

    fn on_line(&mut self, raw: &[u8]) {
        let Ok(s) = std::str::from_utf8(raw) else { return };
        let s = s.trim();
        let payload = s.strip_prefix("data:").map(str::trim).unwrap_or(s);
        if !payload.starts_with('{') || !payload.contains("usage") {
            return;
        }
        if let Some(u) = serde_json::from_str::<Value>(payload).ok().as_ref().and_then(usage_of) {
            self.saw_line_usage = true;
            self.best.merge(u);
        }
    }

    pub fn finish(&mut self) -> Usage {
        if !self.line.is_empty() && !self.line_overflow {
            let line = std::mem::take(&mut self.line);
            self.on_line(&line);
        }
        if !self.saw_line_usage && !self.body_overflow {
            if let Some(u) = serde_json::from_slice::<Value>(&self.body).ok().as_ref().and_then(usage_of) {
                self.best.merge(u);
            }
        }
        self.best
    }
}

struct Guard<F: FnOnce(Usage)> {
    sc: Scanner,
    cb: Option<F>,
}

impl<F: FnOnce(Usage)> Drop for Guard<F> {
    /// Fires when the body is dropped: after a clean end, an upstream error,
    /// or a client disconnect (with whatever usage had been seen by then).
    fn drop(&mut self) {
        if let Some(cb) = self.cb.take() {
            cb(self.sc.finish());
        }
    }
}

/// Pass `body` through unchanged while scanning it for token usage;
/// `on_done` runs exactly once when the body is dropped.
pub fn tap(body: Body, on_done: impl FnOnce(Usage) + Send + 'static) -> Body {
    let mut guard = Guard { sc: Scanner::default(), cb: Some(on_done) };
    let stream = body.into_data_stream().map(move |item| {
        if let Ok(chunk) = &item {
            guard.sc.feed(chunk);
        }
        item
    });
    Body::from_stream(stream)
}

pub async fn record(
    db: &SqlitePool,
    user_id: Option<&str>,
    pool_id: Option<&str>,
    provider_id: &str,
    model: &str,
    u: Usage,
) {
    let r = sqlx::query(
        "INSERT INTO usage_log (created_at, user_id, pool_id, provider_id, model, input_tokens, output_tokens, cached_tokens)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(Utc::now())
    .bind(user_id)
    .bind(pool_id)
    .bind(provider_id)
    .bind(model)
    .bind(u.input)
    .bind(u.output)
    .bind(u.cached.min(u.input))
    .execute(db)
    .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "usage_log: insert failed");
    }
}

// ------------------------------------------------------------------ report

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/usage", get(report))
        .route(
            "/admin/usage/people",
            get(list_people).put(put_person).delete(delete_person),
        )
        .route(
            "/admin/usage/prices",
            get(list_prices).put(put_price).delete(delete_price),
        )
}

#[derive(Debug, Deserialize)]
struct ReportQuery {
    /// Inclusive lower bound, `YYYY-MM-DD` or RFC 3339.
    from: Option<String>,
    /// Exclusive upper bound, same formats.
    to: Option<String>,
    person: Option<String>,
}

/// The person a credential id belongs to: an explicit `usage_people` row,
/// else the email of a `cf:<email>` caller, else the id itself.
fn person_of(user_id: Option<&str>, aliases: &BTreeMap<String, String>) -> String {
    let Some(id) = user_id else { return "(anonymous)".into() };
    if let Some(p) = aliases.get(id) {
        return p.clone();
    }
    match id.strip_prefix("cf:") {
        Some(rest) if rest.contains('@') => rest.to_string(),
        _ => id.to_string(),
    }
}

struct Price {
    input: f64,
    output: f64,
    cached: Option<f64>,
}

/// The price of the longest configured model name that prefixes `model`.
fn price_for<'a>(model: &str, prices: &'a [(String, Price)]) -> Option<&'a Price> {
    prices
        .iter()
        .filter(|(m, _)| model.starts_with(m.as_str()))
        .max_by_key(|(m, _)| m.len())
        .map(|(_, p)| p)
}

fn cost_usd(u: Usage, p: &Price) -> f64 {
    let fresh = (u.input - u.cached).max(0) as f64;
    let cached = u.cached as f64 * p.cached.unwrap_or(p.input);
    (fresh * p.input + cached + u.output as f64 * p.output) / 1_000_000.0
}

/// Cached share of input tokens, in percent to one decimal; null with no input.
fn cache_hit_pct(u: Usage) -> Value {
    if u.input <= 0 {
        return Value::Null;
    }
    json!((u.cached as f64 / u.input as f64 * 1000.0).round() / 10.0)
}

pub async fn build_report(
    db: &SqlitePool,
    from: Option<&str>,
    to: Option<&str>,
    only_person: Option<&str>,
) -> Result<Value, AppError> {
    let aliases: BTreeMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT user_id, person FROM usage_people")
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    let prices: Vec<(String, Price)> = sqlx::query_as::<_, (String, f64, f64, Option<f64>)>(
        "SELECT model, input_per_mtok, output_per_mtok, cached_per_mtok FROM model_prices",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|(m, i, o, c)| (m, Price { input: i, output: o, cached: c }))
    .collect();

    let rows: Vec<(Option<String>, String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT user_id, model, count(*), sum(input_tokens), sum(output_tokens), sum(cached_tokens)
         FROM usage_log
         WHERE (?1 IS NULL OR created_at >= ?1) AND (?2 IS NULL OR created_at < ?2)
         GROUP BY user_id, model",
    )
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?;

    #[derive(Default)]
    struct Acc {
        requests: i64,
        u: Usage,
        cost: f64,
        unpriced: bool,
    }
    let mut by_pm: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for (uid, model, n, i, o, c) in rows {
        let person = person_of(uid.as_deref(), &aliases);
        if only_person.is_some_and(|p| p != person) {
            continue;
        }
        let a = by_pm.entry((person, model.clone())).or_default();
        let u = Usage { input: i, output: o, cached: c };
        a.requests += n;
        a.u.input += u.input;
        a.u.output += u.output;
        a.u.cached += u.cached;
        match price_for(&model, &prices) {
            Some(p) => a.cost += cost_usd(u, p),
            None => a.unpriced = true,
        }
    }

    let mut people: BTreeMap<String, Acc> = BTreeMap::new();
    let mut unpriced_models: BTreeSet<String> = BTreeSet::new();
    let mut rows_out = Vec::new();
    for ((person, model), a) in &by_pm {
        let t = people.entry(person.clone()).or_default();
        t.requests += a.requests;
        t.u.input += a.u.input;
        t.u.output += a.u.output;
        t.u.cached += a.u.cached;
        t.cost += a.cost;
        t.unpriced |= a.unpriced;
        if a.unpriced {
            unpriced_models.insert(model.clone());
        }
        rows_out.push(json!({
            "person": person, "model": model, "requests": a.requests,
            "input_tokens": a.u.input, "output_tokens": a.u.output, "cached_tokens": a.u.cached,
            "cache_hit_pct": cache_hit_pct(a.u),
            "cost_usd": if a.unpriced { Value::Null } else { json!(a.cost) },
        }));
    }
    let mut people_out: Vec<Value> = people
        .iter()
        .map(|(p, a)| {
            json!({
                "person": p, "requests": a.requests,
                "input_tokens": a.u.input, "output_tokens": a.u.output, "cached_tokens": a.u.cached,
                "cache_hit_pct": cache_hit_pct(a.u),
                // Cost of the priced models only; `partial` says some usage had no price.
                "cost_usd": a.cost, "partial": a.unpriced,
            })
        })
        .collect();
    people_out.sort_by(|a, b| {
        b["cost_usd"].as_f64().partial_cmp(&a["cost_usd"].as_f64()).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(json!({
        "from": from, "to": to,
        "people": people_out,
        "rows": rows_out,
        "unpriced_models": unpriced_models,
    }))
}

async fn report(
    State(s): State<AppState>,
    Query(q): Query<ReportQuery>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(build_report(&s.db, q.from.as_deref(), q.to.as_deref(), q.person.as_deref()).await?))
}

async fn list_people(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT user_id, person FROM usage_people ORDER BY person, user_id")
            .fetch_all(&s.db)
            .await?;
    Ok(Json(json!(rows.into_iter().map(|(u, p)| json!({"user_id": u, "person": p})).collect::<Vec<_>>())))
}

#[derive(Debug, Deserialize)]
struct PersonBody {
    user_id: String,
    person: String,
}

async fn put_person(
    State(s): State<AppState>,
    Json(b): Json<PersonBody>,
) -> Result<Json<Value>, AppError> {
    let (id, person) = (b.user_id.trim(), b.person.trim());
    if id.is_empty() || person.is_empty() {
        return Err(AppError::BadRequest("user_id and person are required".into()));
    }
    sqlx::query(
        "INSERT INTO usage_people (user_id, person) VALUES (?, ?)
         ON CONFLICT(user_id) DO UPDATE SET person = excluded.person",
    )
    .bind(id)
    .bind(person)
    .execute(&s.db)
    .await?;
    Ok(Json(json!({"user_id": id, "person": person})))
}

#[derive(Debug, Deserialize)]
struct KeyQuery {
    user_id: Option<String>,
    model: Option<String>,
}

async fn delete_person(
    State(s): State<AppState>,
    Query(q): Query<KeyQuery>,
) -> Result<Json<Value>, AppError> {
    let id = q.user_id.ok_or_else(|| AppError::BadRequest("user_id is required".into()))?;
    sqlx::query("DELETE FROM usage_people WHERE user_id = ?").bind(&id).execute(&s.db).await?;
    Ok(Json(json!({"deleted": id})))
}

async fn list_prices(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows: Vec<(String, f64, f64, Option<f64>)> = sqlx::query_as(
        "SELECT model, input_per_mtok, output_per_mtok, cached_per_mtok FROM model_prices ORDER BY model",
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(m, i, o, c)| json!({"model": m, "input_per_mtok": i, "output_per_mtok": o, "cached_per_mtok": c}))
        .collect::<Vec<_>>())))
}

#[derive(Debug, Deserialize)]
struct PriceBody {
    model: String,
    input_per_mtok: f64,
    output_per_mtok: f64,
    #[serde(default)]
    cached_per_mtok: Option<f64>,
}

async fn put_price(
    State(s): State<AppState>,
    Json(b): Json<PriceBody>,
) -> Result<Json<Value>, AppError> {
    let model = b.model.trim();
    let prices = [Some(b.input_per_mtok), Some(b.output_per_mtok), b.cached_per_mtok];
    if model.is_empty() || prices.iter().flatten().any(|p| !p.is_finite() || *p < 0.0) {
        return Err(AppError::BadRequest("model and non-negative prices are required".into()));
    }
    sqlx::query(
        "INSERT INTO model_prices (model, input_per_mtok, output_per_mtok, cached_per_mtok) VALUES (?,?,?,?)
         ON CONFLICT(model) DO UPDATE SET input_per_mtok = excluded.input_per_mtok,
           output_per_mtok = excluded.output_per_mtok, cached_per_mtok = excluded.cached_per_mtok",
    )
    .bind(model)
    .bind(b.input_per_mtok)
    .bind(b.output_per_mtok)
    .bind(b.cached_per_mtok)
    .execute(&s.db)
    .await?;
    Ok(Json(json!({"model": model})))
}

async fn delete_price(
    State(s): State<AppState>,
    Query(q): Query<KeyQuery>,
) -> Result<Json<Value>, AppError> {
    let m = q.model.ok_or_else(|| AppError::BadRequest("model is required".into()))?;
    sqlx::query("DELETE FROM model_prices WHERE model = ?").bind(&m).execute(&s.db).await?;
    Ok(Json(json!({"deleted": m})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::init_pool;
    use http_body_util::BodyExt;
    use std::sync::{Arc, Mutex};

    fn scan(chunks: &[&str]) -> Usage {
        let mut s = Scanner::default();
        for c in chunks {
            s.feed(c.as_bytes());
        }
        s.finish()
    }

    #[test]
    fn openai_chat_json_body() {
        let u = scan(&[r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6}}}"#]);
        assert_eq!(u, Usage { input: 10, output: 4, cached: 6 });
    }

    #[test]
    fn openai_stream_usage_in_last_chunk_even_when_split_across_reads() {
        let u = scan(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usa",
            "ge\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n",
        ]);
        assert_eq!(u, Usage { input: 7, output: 2, cached: 0 });
    }

    #[test]
    fn anthropic_stream_adds_cache_tokens_to_input_and_takes_final_output() {
        let u = scan(&[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":20,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n",
        ]);
        assert_eq!(u, Usage { input: 125, output: 42, cached: 100 });
    }

    #[test]
    fn responses_completed_event() {
        let u = scan(&["data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n"]);
        assert_eq!(u, Usage { input: 9, output: 3, cached: 2 });
    }

    #[test]
    fn no_usage_is_empty() {
        assert!(scan(&["data: {\"choices\":[]}\n\n"]).is_empty());
        assert!(scan(&["not json"]).is_empty());
    }

    #[tokio::test]
    async fn tap_passes_bytes_through_and_reports_usage_on_drop() {
        let body = Body::from_stream(futures::stream::iter(vec![
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n")),
        ]));
        let seen = Arc::new(Mutex::new(None));
        let s2 = seen.clone();
        let out = tap(body, move |u| *s2.lock().unwrap() = Some(u));
        let got = out.collect().await.unwrap().to_bytes();
        assert!(got.starts_with(b"data: "));
        assert_eq!(*seen.lock().unwrap(), Some(Usage { input: 3, output: 1, cached: 0 }));
    }

    #[tokio::test]
    async fn report_groups_by_person_prices_by_longest_prefix_and_flags_unpriced() {
        let db = init_pool(":memory:").await.unwrap();
        let rec = |uid: &'static str, model: &'static str, i, o, c| {
            let db = db.clone();
            async move { record(&db, Some(uid), None, "p", model, Usage { input: i, output: o, cached: c }).await }
        };
        rec("cf:abc.access", "gpt-5.6-sol", 1_000_000, 500_000, 400_000).await;
        rec("cf:dev@x.com", "gpt-5.6-sol", 1_000_000, 0, 0).await;
        rec("cf:dev@x.com", "mystery", 10, 10, 0).await;
        sqlx::query("INSERT INTO usage_people VALUES ('cf:abc.access', 'dev@x.com')").execute(&db).await.unwrap();
        sqlx::query("INSERT OR REPLACE INTO model_prices VALUES ('gpt-5', 100.0, 100.0, NULL), ('gpt-5.6-sol', 2.0, 10.0, 0.5)")
            .execute(&db).await.unwrap();

        let r = build_report(&db, None, None, None).await.unwrap();
        assert_eq!(r["people"].as_array().unwrap().len(), 1, "alias merged into the email");
        let p = &r["people"][0];
        assert_eq!(p["person"], "dev@x.com");
        assert_eq!(p["requests"], 3);
        // 600k fresh*2 + 400k cached*0.5 + 500k out*10, plus 1M in*2, per million
        let want = (600_000.0 * 2.0 + 400_000.0 * 0.5 + 500_000.0 * 10.0 + 1_000_000.0 * 2.0) / 1e6;
        assert!((p["cost_usd"].as_f64().unwrap() - want).abs() < 1e-9);
        assert_eq!(p["partial"], true);
        // 400k cached of 2_000_010 input
        assert_eq!(p["cache_hit_pct"], 20.0);
        let sol = r["rows"].as_array().unwrap().iter()
            .find(|x| x["model"] == "gpt-5.6-sol").unwrap();
        assert_eq!(sol["cache_hit_pct"], 20.0);
        assert_eq!(r["unpriced_models"], json!(["mystery"]));
        let only = build_report(&db, None, None, Some("nobody")).await.unwrap();
        assert!(only["people"].as_array().unwrap().is_empty());
    }
}
