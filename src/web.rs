use axum::{
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};

const INDEX: &str = include_str!("web/index.html");
const STYLES: &str = include_str!("web/app.css");
const SCRIPT: &str = include_str!("web/app.js");

pub async fn index() -> Response {
    asset_response("text/html; charset=utf-8", INDEX)
}

pub async fn styles() -> Response {
    asset_response("text/css; charset=utf-8", STYLES)
}

pub async fn script() -> Response {
    asset_response("text/javascript; charset=utf-8", SCRIPT)
}

fn asset_response(content_type: &'static str, body: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    response
}
