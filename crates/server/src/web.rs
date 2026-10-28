//! Serving the web UI: a single-page app built into a directory of static
//! files.
//!
//! Paths that name a file are served as that file; any other path outside
//! `/api/` gets `index.html`, and the app's router takes it from there.
//! Unknown API paths stay JSON 404s.

use std::convert::Infallible;
use std::path::Path;

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use tower::util::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::app::ApiError;

/// Files under this path have content hashes in their names, so they never
/// change.
const IMMUTABLE_PREFIX: &str = "/_app/immutable/";

pub fn service(
    dir: &Path,
) -> impl tower::Service<Request, Response = Response, Error = Infallible, Future: Send>
       + Clone
       + Send
       + Sync
       + 'static {
    let files = ServeDir::new(dir)
        .append_index_html_on_directories(false)
        .fallback(ServeFile::new(dir.join("index.html")));
    tower::service_fn(move |request: Request| {
        let files = files.clone();
        async move {
            let path = request.uri().path().to_owned();
            if path == "/api" || path.starts_with("/api/") {
                return Ok(ApiError::not_found("API endpoint").into_response());
            }
            let mut response = match files.oneshot(request).await {
                Ok(response) => response.into_response(),
                Err(e) => match e {},
            };
            let cache = if path.starts_with(IMMUTABLE_PREFIX) && response.status().is_success() {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            Ok(response)
        }
    })
}
