//! Authentication and authorization module.
//!
//! Handles JWT token generation/validation, user management,
//! and gRPC request interception.

pub mod interceptor;

use crate::persist::Store;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Claims embedded in JWT tokens.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    /// Username
    pub sub: String,
    /// Role: "admin" or "user"
    pub role: String,
    /// Issued at (Unix timestamp)
    pub iat: usize,
    /// Expiration (Unix timestamp)
    pub exp: usize,
}

/// Authentication service for user management and JWT operations.
pub struct AuthService {
    /// JWT secret key
    secret: String,
    /// Reference to the persistence store
    store: Arc<Store>,
    /// Token expiry duration in seconds (default 24h)
    token_expiry_secs: usize,
}

impl AuthService {
    /// Create a new AuthService.
    pub fn new(secret: String, store: Arc<Store>) -> Self {
        Self {
            secret,
            store,
            token_expiry_secs: 86400, // 24 hours
        }
    }

    /// Login: validate username/password against the users table.
    /// Returns a JWT token on success.
    pub fn login(&self, username: &str, password: &str) -> anyhow::Result<String> {
        let user = self
            .store
            .get_user(username)?
            .ok_or_else(|| anyhow::anyhow!("Invalid username or password"))?;

        // Verify password against bcrypt hash
        let valid = bcrypt::verify(password, &user.password_hash)?;
        if !valid {
            anyhow::bail!("Invalid username or password");
        }

        // Generate JWT
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims {
            sub: user.username,
            role: user.role,
            iat: now,
            exp: now + self.token_expiry_secs,
        };

        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.secret.as_bytes()),
        )?;

        Ok(token)
    }

    /// Validate a JWT token and return its claims.
    pub fn validate_token(&self, token: &str) -> anyhow::Result<Claims> {
        let token_data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.secret.as_bytes()),
            &Validation::default(),
        )?;
        Ok(token_data.claims)
    }

    /// Create a new user with a bcrypt-hashed password.
    pub fn create_user(
        &self,
        username: &str,
        password: &str,
        role: &str,
    ) -> anyhow::Result<()> {
        // Validate role
        if role != "admin" && role != "user" {
            anyhow::bail!("Role must be 'admin' or 'user'");
        }

        // Hash password
        let password_hash = bcrypt::hash(password, bcrypt::DEFAULT_COST)?;

        self.store.create_user(username, &password_hash, role)?;
        tracing::info!("User '{}' created with role '{}'", username, role);
        Ok(())
    }

    /// List all users.
    pub fn list_users(&self) -> anyhow::Result<Vec<crate::persist::UserRecord>> {
        self.store.list_users()
    }

    /// Delete a user.
    pub fn delete_user(&self, username: &str) -> anyhow::Result<()> {
        self.store.delete_user(username)?;
        tracing::info!("User '{}' deleted", username);
        Ok(())
    }

    /// Ensure the default admin user exists.
    /// If BOSUN_ADMIN_PASSWORD env var is set, use that; otherwise generate a random password.
    /// Returns the admin password if one was generated (so it can be displayed).
    pub fn ensure_admin_user(&self) -> anyhow::Result<Option<String>> {
        // Check if admin already exists
        if self.store.get_user("admin")?.is_some() {
            tracing::info!("Admin user already exists");
            return Ok(None);
        }

        // Generate or read admin password
        let password = std::env::var("BOSUN_ADMIN_PASSWORD").unwrap_or_else(|_| {
            generate_random_password()
        });

        let password_hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)?;
        self.store
            .create_user("admin", &password_hash, "admin")?;

        tracing::info!("Default admin user created");
        Ok(Some(password))
    }

    /// Get the token expiry duration in seconds.
    #[allow(dead_code)]
    pub fn token_expiry_secs(&self) -> usize {
        self.token_expiry_secs
    }
}

/// Generate a random 16-character alphanumeric password using OS randomness.
fn generate_random_password() -> String {
    use std::io::Read;
    let mut buf = [0u8; 12];
    let mut f = std::fs::File::open("/dev/urandom").expect("Failed to open /dev/urandom");
    f.read_exact(&mut buf).expect("Failed to read random bytes");

    // Encode as hex, take first 16 chars
    hex_encode(&buf)[..16].to_string()
}

/// Simple hex encoding without external crates.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX_CHARS: &[u8] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        result.push(HEX_CHARS[(b >> 4) as usize] as char);
        result.push(HEX_CHARS[(b & 0x0f) as usize] as char);
    }
    result
}
