//! HTTP Basic authentication guard.
//!
//! KIO / Dolphin send HTTP Basic credentials on WebDAVS connections, so a
//! simple header check is all that's needed. Passwords are compared in
//! constant time to avoid trivial timing attacks.

use base64::Engine;
use dav_server::body::Body;
use hyper::header::{AUTHORIZATION, CONTENT_LENGTH, WWW_AUTHENTICATE};
use hyper::{HeaderMap, Response, StatusCode};
use subtle::ConstantTimeEq;

/// Basic-auth credentials + realm for the 401 challenge.
#[derive(Clone, Debug)]
pub struct BasicAuth {
    user: String,
    pass: String,
    realm: String,
}

impl BasicAuth {
    pub fn new(user: String, pass: String, realm: impl Into<String>) -> Self {
        Self {
            user,
            pass,
            realm: realm.into(),
        }
    }

    /// Constant-time check of the `Authorization: Basic ...` header.
    pub fn is_authorized(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        let Some(encoded) = value.strip_prefix("Basic ") else {
            return false;
        };
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            return false;
        };
        let Some(pos) = decoded.iter().position(|&b| b == b':') else {
            return false;
        };
        let (user, pass) = decoded.split_at(pos);
        let pass = &pass[1..];

        let user_ok = user.len() == self.user.len() && bool::from(user.ct_eq(self.user.as_bytes()));
        let pass_ok = pass.len() == self.pass.len() && bool::from(pass.ct_eq(self.pass.as_bytes()));
        user_ok && pass_ok
    }

    /// Build a `401 Unauthorized` response with a Basic challenge.
    pub fn challenge(&self) -> Response<Body> {
        let mut res = Response::new(Body::empty());
        *res.status_mut() = StatusCode::UNAUTHORIZED;
        res.headers_mut().insert(
            WWW_AUTHENTICATE,
            format!("Basic realm=\"{}\", charset=\"UTF-8\"", self.realm)
                .parse()
                .expect("valid WWW-Authenticate header"),
        );
        res.headers_mut()
            .insert(CONTENT_LENGTH, "0".parse().expect("valid Content-Length"));
        res
    }
}
