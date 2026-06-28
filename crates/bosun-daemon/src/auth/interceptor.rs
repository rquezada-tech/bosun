//! gRPC auth interceptor.
//!
//! Extracts and validates JWT tokens from requests. If a token is present
//! it is validated and claims are injected into request extensions.
//! Requests without a token pass through — each handler is responsible
//! for enforcing its own authentication requirements via get_claims() or
//! require_admin() helpers.
//!
//! NOTE: The Login RPC path exclusion was removed because tonic 0.13
//! does not expose request.uri() on interceptor Request<()>. Instead,
//! the interceptor passes all requests through and handlers enforce
//! their own auth. Login is implicitly allowed since it has no auth guard
//! in its handler.

use crate::auth::AuthService;
use crate::auth::Claims;
use std::sync::Arc;
use tonic::{Request, Status};

pub fn create_interceptor(
    auth_service: Arc<AuthService>,
) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |mut request: Request<()>| {
        // Extract Authorization header
        if let Some(token) = extract_bearer_token(request.metadata()) {
            // Validate token
            let claims = auth_service.validate_token(&token).map_err(|e| {
                tracing::warn!("JWT validation failed: {}", e);
                Status::unauthenticated(format!("Invalid or expired token: {}", e))
            })?;

            // Inject claims into request extensions for handlers
            request.extensions_mut().insert(claims);
        }
        // Missing or invalid token = pass through.
        // Each handler enforces its own auth via get_claims() / require_admin().

        Ok(request)
    }
}

/// Extract a Bearer token from gRPC metadata.
fn extract_bearer_token(metadata: &tonic::metadata::MetadataMap) -> Option<String> {
    let auth_header = metadata.get("authorization")?;
    let auth_str = auth_header.to_str().ok()?;

    // Expect: "Bearer <token>"
    if auth_str.len() < 8 || !auth_str[..7].eq_ignore_ascii_case("bearer ") {
        return None;
    }

    Some(auth_str[7..].trim().to_string())
}

/// Helper to extract Claims from a request's extensions.
/// Returns an error if no claims are present.
pub fn get_claims<T>(request: &Request<T>) -> Result<&Claims, Status> {
    request
        .extensions()
        .get::<Claims>()
        .ok_or_else(|| Status::unauthenticated("Authentication required. Use 'bosun login' first."))
}

/// Helper to check if the claims have the admin role.
pub fn require_admin<T>(request: &Request<T>) -> Result<&Claims, Status> {
    let claims = get_claims(request)?;
    if claims.role != "admin" {
        return Err(Status::permission_denied(
            "Admin role required for this operation",
        ));
    }
    Ok(claims)
}
