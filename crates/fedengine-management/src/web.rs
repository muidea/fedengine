//! Build-time embedded management assets; no separate frontend runtime or filesystem access.
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

pub(crate) async fn index() -> Response {
    asset(
        "text/html; charset=utf-8",
        include_str!("../web/index.html"),
    )
}

pub(crate) async fn javascript() -> Response {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../web/app.js"),
    )
}

pub(crate) async fn stylesheet() -> Response {
    asset("text/css; charset=utf-8", include_str!("../web/style.css"))
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    (StatusCode::OK, headers, body).into_response()
}

/// Browser mutations must come from the management origin. CLI clients without
/// Origin remain supported; no CORS access or additional authentication is added.
pub(crate) fn same_origin(headers: &HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|site| site == "cross-site")
    {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin.to_str().is_ok_and(|origin| {
        ["http://", "https://"]
            .iter()
            .any(|scheme| origin.strip_prefix(scheme) == Some(host))
    })
}
