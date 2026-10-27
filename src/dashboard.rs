//! Read-only web dashboard served by the daemon. See assets/dashboard/CONTRACT.md.
//!
//! Scaffold: implemented by the "dashboard server" brief.

use crate::http::AppState;
use axum::Router;

/// Dashboard routes, merged into the daemon router behind the same guard.
pub fn routes() -> Router<AppState> {
    Router::new()
}
