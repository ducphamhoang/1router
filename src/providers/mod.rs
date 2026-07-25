pub mod adapter;
pub mod oauth_routes;
pub mod queries;
pub mod refresh_lock;
pub mod routes;

use axum::Router;

use crate::core::state::AppState;

pub fn routes() -> Router<AppState> {
    routes::routes()
}
