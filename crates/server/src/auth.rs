//! Who may use the API: local user accounts with session cookies (the web
//! UI), and the admin token (`mnix`).
//!
//! Passwords are hashed with Argon2id. A login creates a session whose
//! random token is the `mnix_session` cookie (HttpOnly, SameSite=Strict);
//! the database stores only its SHA-256. Requests authenticated by the
//! cookie that change something must also carry the [`CSRF_HEADER`], which
//! a cross-site page cannot add without a CORS preflight the server never
//! allows.

use std::path::Path;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use mnix_protocol::api::{ChangePassword, Login, Session};
use mnix_store::{NewUserSession, Store, User};
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::app::{ApiError, State as AppState};
use crate::secret;

/// The session cookie's name.
pub const SESSION_COOKIE: &str = "mnix_session";
/// Prefix of session tokens.
pub const SESSION_TOKEN_PREFIX: &str = "mnix_session_";
/// Cookie-authenticated requests other than GET and HEAD must send this
/// header (any value).
pub const CSRF_HEADER: &str = "x-mnix-csrf";
/// The user the server creates on first start.
pub const INITIAL_USER: &str = "admin";

const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 1024;

/// Argon2 is deliberately slow and memory-hungry; at most this many hashes
/// run at once, so a burst of logins cannot exhaust the server.
static HASHING: Semaphore = Semaphore::const_new(4);

/// Who made a request.
#[derive(Debug, Clone)]
pub enum Principal {
    /// The admin token (`mnix`).
    AdminToken,
    /// A logged-in user, and the hash of their session token.
    User { user: User, session_hash: String },
}

impl Principal {
    /// The user's name, for `requested_by`; `None` for the admin token,
    /// whose clients say who they are themselves.
    pub fn username(&self) -> Option<&str> {
        match self {
            Self::AdminToken => None,
            Self::User { user, .. } => Some(&user.username),
        }
    }
}

/// Middleware for the API: accepts the admin token or a session cookie,
/// and adds the [`Principal`] to the request.
pub async fn authenticate(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let headers = request.headers();
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(secret::bearer);
    let principal = if let Some(token) = bearer {
        // Comparing hashes keeps the comparison's timing independent of the
        // token's contents.
        if secret::hash(token) != state.admin_token_hash {
            return Err(ApiError::unauthorized("invalid admin token"));
        }
        Principal::AdminToken
    } else if let Some(token) = session_cookie(headers) {
        let session_hash = secret::hash(token);
        let user = state
            .store
            .session_user(&session_hash, OffsetDateTime::now_utc())
            .await?
            .filter(|u| u.organization_id == state.organization.id)
            .ok_or_else(|| ApiError::unauthorized("your session has expired; log in again"))?;
        let safe = matches!(*request.method(), Method::GET | Method::HEAD);
        if !safe && !headers.contains_key(CSRF_HEADER) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                format!("missing the {CSRF_HEADER} header"),
            ));
        }
        Principal::User { user, session_hash }
    } else {
        return Err(ApiError::unauthorized(
            "log in, or send the admin token as a bearer token",
        ));
    };
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

/// The session token from the request's cookies.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.trim())
        .filter(|v| !v.is_empty())
}

fn set_cookie(state: &AppState, value: &str, max_age: Duration) -> HeaderValue {
    let secure = if state.settings.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        max_age.as_secs()
    ))
    .expect("session cookies are ASCII")
}

/// `POST /api/session`
pub async fn login(
    State(state): State<AppState>,
    Json(request): Json<Login>,
) -> Result<Response, ApiError> {
    let user = state
        .store
        .user_by_name(state.organization.id, &request.username)
        .await?;
    // Verify against a dummy hash for unknown users too, so the response
    // time does not tell which names exist.
    let hash = user
        .as_ref()
        .map_or_else(|| DUMMY_HASH.to_owned(), |u| u.password_hash.clone());
    let valid = verify_password(&hash, &request.password).await;
    let Some(user) = user.filter(|_| valid) else {
        tracing::info!("failed login for {:?}", request.username);
        return Err(ApiError::unauthorized("wrong username or password"));
    };
    let token = secret::generate(SESSION_TOKEN_PREFIX);
    let now = OffsetDateTime::now_utc();
    state
        .store
        .create_user_session(&NewUserSession {
            user_id: user.id,
            token_hash: secret::hash(&token),
            created_at: now,
            expires_at: now + state.settings.session_lifetime,
        })
        .await?;
    tracing::info!("{} logged in", user.username);
    Ok((
        [(
            header::SET_COOKIE,
            set_cookie(&state, &token, state.settings.session_lifetime),
        )],
        Json(Session {
            username: user.username,
        }),
    )
        .into_response())
}

/// `DELETE /api/session`
pub async fn logout(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Response, ApiError> {
    if let Principal::User { session_hash, .. } = &principal {
        state.store.delete_user_session(session_hash).await?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, set_cookie(&state, "", Duration::ZERO))],
    )
        .into_response())
}

/// `GET /api/session`
pub async fn session(Extension(principal): Extension<Principal>) -> Json<Session> {
    Json(Session {
        username: principal.username().unwrap_or("admin token").to_owned(),
    })
}

/// `PUT /api/session/password`
pub async fn change_password(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<ChangePassword>,
) -> Result<StatusCode, ApiError> {
    let Principal::User { user, session_hash } = principal else {
        return Err(ApiError::bad_request(
            "the admin token has no password; log in as a user",
        ));
    };
    if !verify_password(&user.password_hash, &request.current_password).await {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "the current password is wrong",
        ));
    }
    validate_password(&request.new_password).map_err(ApiError::bad_request)?;
    let hash = hash_password(&request.new_password)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state
        .store
        .set_user_password(
            user.id,
            &hash,
            Some(&session_hash),
            OffsetDateTime::now_utc(),
        )
        .await?;
    tracing::info!("{} changed their password", user.username);
    Ok(StatusCode::NO_CONTENT)
}

pub fn validate_password(password: &str) -> Result<(), String> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(format!(
            "passwords need at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(format!(
            "passwords may have at most {MAX_PASSWORD_LEN} characters"
        ));
    }
    Ok(())
}

pub async fn hash_password(password: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let _permit = HASHING.acquire().await?;
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt)?;
        let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(argon2::Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("hashing the password: {e}"))?
            .to_string())
    })
    .await?
}

/// Whether `password` matches the PHC string `hash`. False for malformed
/// hashes.
pub async fn verify_password(hash: &str, password: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(_permit) = HASHING.acquire().await else {
        return false;
    };
    let (hash, password) = (hash.to_owned(), password.to_owned());
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|parsed| {
            argon2::Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .unwrap_or(false)
}

/// The Argon2id hash of a random password nobody knows, verified for
/// unknown users.
const DUMMY_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Ck1jLmo8Y6Yq9dO9Pu4o1bHXjpUBcf9Tz4+hkwRA1zA";

/// Creates the first user on a server without any, with the password in
/// `password_file` or else a generated one written to `generated_file`
/// (readable only by the server's user). Several replicas may race to do
/// this; one wins.
pub async fn ensure_initial_user(
    store: &dyn Store,
    organization_id: Uuid,
    password_file: Option<&Path>,
    generated_file: &Path,
) -> anyhow::Result<()> {
    use anyhow::Context;
    if store.user_count(organization_id).await? > 0 {
        return Ok(());
    }
    let (password, generated) = match password_file {
        Some(path) => {
            let password = std::fs::read_to_string(path)
                .with_context(|| format!("reading the initial password from {}", path.display()))?;
            (password.trim_end_matches(['\n', '\r']).to_owned(), false)
        }
        None => (secret::generate(""), true),
    };
    validate_password(&password).map_err(anyhow::Error::msg)?;
    let hash = hash_password(&password).await?;
    match store
        .create_user(
            organization_id,
            INITIAL_USER,
            &hash,
            OffsetDateTime::now_utc(),
        )
        .await
    {
        Ok(_) => {}
        // Another replica created it first.
        Err(mnix_store::Error::Conflict(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    if generated {
        crate::write_private(generated_file, &format!("{password}\n"))?;
        tracing::warn!(
            "created the user {INITIAL_USER:?}; its password is in {}. Log in and change it",
            generated_file.display()
        );
    } else {
        tracing::info!("created the user {INITIAL_USER:?}");
    }
    Ok(())
}

/// Sets a user's password, creating the user if needed, and ends their
/// sessions (`mnix-server set-password`).
pub async fn set_password(
    store: &dyn Store,
    organization_id: Uuid,
    username: &str,
    password: &str,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        crate::api::is_valid_name(username),
        "invalid user name {username:?}: use up to 63 letters, digits, '-', '_', or '.'"
    );
    validate_password(password).map_err(anyhow::Error::msg)?;
    let hash = hash_password(password).await?;
    let now = OffsetDateTime::now_utc();
    match store.user_by_name(organization_id, username).await? {
        Some(user) => {
            store.set_user_password(user.id, &hash, None, now).await?;
            Ok(false)
        }
        None => {
            store
                .create_user(organization_id, username, &hash, now)
                .await?;
            Ok(true)
        }
    }
}

/// Deletes expired sessions every hour.
pub async fn delete_expired_sessions(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        match state
            .store
            .delete_expired_user_sessions(OffsetDateTime::now_utc())
            .await
        {
            Ok(0) => {}
            Ok(n) => tracing::debug!("deleted {n} expired session(s)"),
            Err(e) => tracing::error!("deleting expired sessions: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passwords_hash_and_verify() {
        let hash = hash_password("correct horse").await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "correct horse").await);
        assert!(!verify_password(&hash, "wrong horse").await);
        assert!(!verify_password("not a hash", "correct horse").await);
        // Salted: the same password hashes differently.
        assert_ne!(hash_password("correct horse").await.unwrap(), hash);
    }

    #[tokio::test]
    async fn the_dummy_hash_is_valid_and_matches_nothing_obvious() {
        use argon2::password_hash::PasswordHash;
        assert!(PasswordHash::new(DUMMY_HASH).is_ok());
        assert!(!verify_password(DUMMY_HASH, "").await);
        assert!(!verify_password(DUMMY_HASH, "password").await);
    }

    #[test]
    fn passwords_have_a_minimum_length() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("long enough").is_ok());
        assert!(validate_password(&"x".repeat(MAX_PASSWORD_LEN + 1)).is_err());
    }

    #[test]
    fn finds_the_session_cookie() {
        let mut headers = HeaderMap::new();
        assert_eq!(session_cookie(&headers), None);
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; mnix_session=abc ; other=1"),
        );
        assert_eq!(session_cookie(&headers), Some("abc"));
        headers.insert(header::COOKIE, HeaderValue::from_static("mnix_session="));
        assert_eq!(session_cookie(&headers), None);
    }
}
