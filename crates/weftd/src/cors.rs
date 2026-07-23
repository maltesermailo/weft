//! A minimal permissive CORS layer for the HTTP data plane (`/media`,
//! `/unfurl`). These endpoints authenticate with a **query-string bearer**, not
//! cookies, so `Access-Control-Allow-Origin: *` carries no ambient-authority
//! risk — and it is *required*: the desktop client's webview origin
//! (`tauri://…`) differs from the media origin, so an upload's custom
//! `Content-Type` header makes it a non-simple cross-origin request. Without a
//! preflight response the browser reports a bare `TypeError: Load failed` and
//! the upload never reaches the server.

use axum::{
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Answer `OPTIONS` preflights directly and stamp the CORS headers on every
/// response. Applied via `middleware::from_fn`, so it runs before routing and
/// can short-circuit preflights even on paths with no `OPTIONS` handler.
pub(crate) async fn cors(req: Request, next: Next) -> Response {
    let preflight = req.method() == Method::OPTIONS;
    let mut resp = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };

    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type"),
    );
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
    resp
}
