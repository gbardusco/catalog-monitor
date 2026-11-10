//! Erros da API: o corpo da resposta é um código (`err.xxx`) que a página traduz.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

pub type ApiResult<T = axum::Json<serde_json::Value>> = Result<T, ApiError>;

pub fn internal<E: std::fmt::Display>(e: E) -> ApiError {
    eprintln!("erro interno: {e}");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "err.internal".into())
}

pub fn bad<T: Into<String>>(code: T) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, code.into())
}

pub fn not_found<T: Into<String>>(code: T) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, code.into())
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
