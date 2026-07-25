pub mod queries;
pub mod routes;

use axum::Router;

use crate::core::state::AppState;

pub fn routes() -> Router<AppState> {
    routes::routes()
}
