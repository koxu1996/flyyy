//! An API request refused: the HTTP status and why. The control plane's handlers (and the mesh's) return
//! it as their error and it's answered once, at the top, as `{"error": "<message>"}`.

use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, header, Response, StatusCode};

use crate::Body;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl std::fmt::Display) -> Self {
        ApiError { status, message: message.to_string() }
    }

    /// The answer: its status, and the message as JSON.
    pub(crate) fn response(&self) -> Response<Body> {
        let body = serde_json::json!({ "error": self.message }).to_string();
        let mut r = Response::new(Full::new(Bytes::from(body)).map_err(|n| match n {}).boxed());
        *r.status_mut() = self.status;
        r.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().expect("static"));
        r
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {}", self.status.as_u16(), self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answered_as_json_with_its_status() {
        let r = ApiError::new(StatusCode::CONFLICT, "app blue exists").response();
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json");
    }
}
