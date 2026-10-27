//! Password login: Argon2 password check, a signed session cookie, a lockout
//! after repeated failures, and the middleware that guards the protected routes.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::Extension;
use axum::Form;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, SameSite, SignedCookieJar};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use time::Duration;

use super::AppState;
use super::api::ApiError;
use crate::db;

const COOKIE_NAME: &str = "session";
const SESSION_SECS: i64 = 14 * 24 * 3600;
/// A session's expiry is pushed out at most this often.
const REFRESH_SECS: i64 = 3600;
/// Most client addresses remembered by the lockout map.
const MAX_TRACKED_IPS: usize = 1000;

/// This many failed logins within `FAILURE_WINDOW_SECS` lock logins.
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW_SECS: i64 = 15 * 60;
const LOCKOUT_SECS: i64 = 15 * 60;

/// Hashes a password with Argon2id. The result is a PHC string such as
/// `$argon2id$v=19$m=19456,t=2,p=1$...` that includes a random salt.
pub fn hash_password(password: &str) -> Result<String> {
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|err| anyhow!("could not hash password: {err}"))?;
    Ok(hash.to_string())
}

/// Checks that `hash` is a valid PHC string, so a typo fails at startup.
pub fn check_hash(hash: &str) -> Result<()> {
    PasswordHash::new(hash).map_err(|err| {
        anyhow!("password_hash in config.toml is not a valid hash ({err}); run `cargo run -- hash-password` to make one")
    })?;
    Ok(())
}

fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// One `Lockout` per client address, bounded in size.
#[derive(Debug, Default)]
pub struct LockoutMap {
    /// Each entry also remembers when it was last touched, for eviction.
    entries: HashMap<IpAddr, (Lockout, i64)>,
}

impl LockoutMap {
    pub fn is_locked(&self, ip: IpAddr, now: i64) -> bool {
        self.entries
            .get(&ip)
            .is_some_and(|(lockout, _)| lockout.is_locked(now))
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: i64) {
        if !self.entries.contains_key(&ip) && self.entries.len() >= MAX_TRACKED_IPS {
            self.evict(now);
        }
        let entry = self.entries.entry(ip).or_default();
        entry.0.record_failure(now);
        entry.1 = now;
    }

    pub fn record_success(&mut self, ip: IpAddr) {
        self.entries.remove(&ip);
    }

    /// Makes room for one entry: drops the least recently touched address,
    /// preferring ones that are not currently locked out.
    fn evict(&mut self, now: i64) {
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, (lockout, touched))| (lockout.is_locked(now), *touched))
            .map(|(ip, _)| *ip);
        if let Some(ip) = victim {
            self.entries.remove(&ip);
        }
    }
}

/// Tracks failed logins. It knows nothing about the clock: callers pass the
/// current time in, which keeps it easy to unit test.
#[derive(Debug, Default)]
pub struct Lockout {
    /// When the recent failures happened (unix seconds), oldest first.
    failures: VecDeque<i64>,
    /// Logins are refused until this time.
    locked_until: i64,
}

impl Lockout {
    pub fn is_locked(&self, now: i64) -> bool {
        now < self.locked_until
    }

    pub fn record_failure(&mut self, now: i64) {
        // Forget failures that are too old to count.
        while let Some(&oldest) = self.failures.front() {
            if now - oldest >= FAILURE_WINDOW_SECS {
                self.failures.pop_front();
            } else {
                break;
            }
        }
        self.failures.push_back(now);
        if self.failures.len() >= MAX_FAILURES {
            self.locked_until = now + LOCKOUT_SECS;
            self.failures.clear();
        }
    }
}

/// Where the session cookie's id comes from: 128 random bits as hex.
fn new_session_id() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// Outcome of looking a session up.
enum SessionCheck {
    Invalid,
    /// Valid; the expiry was just pushed out, so the cookie should be re-sent.
    Refreshed,
    Valid,
}

/// One primary-key lookup. The expiry is only rewritten when `last_seen` is
/// at least `REFRESH_SECS` old, so most requests do not write.
async fn check_session(pool: &SqlitePool, id: &str, now: i64) -> Result<SessionCheck> {
    let row: Option<(i64, i64)> =
        sqlx::query_as("SELECT last_seen, expires_at FROM session WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some((last_seen, expires_at)) = row else {
        return Ok(SessionCheck::Invalid);
    };
    if expires_at <= now {
        return Ok(SessionCheck::Invalid);
    }
    if now - last_seen < REFRESH_SECS {
        return Ok(SessionCheck::Valid);
    }
    sqlx::query("UPDATE session SET last_seen = ?, expires_at = ? WHERE id = ?")
        .bind(now)
        .bind(now + SESSION_SECS)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(SessionCheck::Refreshed)
}

async fn create_session(pool: &SqlitePool, now: i64) -> Result<String> {
    // Expired rows are useless; clear them out while we are here.
    sqlx::query("DELETE FROM session WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;
    let id = new_session_id();
    sqlx::query("INSERT INTO session (id, created_at, last_seen, expires_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(now)
        .bind(now)
        .bind(now + SESSION_SECS)
        .execute(pool)
        .await?;
    Ok(id)
}

fn session_cookie(state: &AppState, id: String) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, id))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.cookie_secure)
        .max_age(Duration::seconds(SESSION_SECS))
        .build()
}

/// Whether the request carries `Authorization: Bearer <token>` matching the
/// configured `api_token`. False when no token is configured.
fn bearer_matches(headers: &HeaderMap, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return false;
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    // Both sides are hashed first, so the comparison takes the same time
    // whatever the length of the guess or where it first differs.
    given.is_some_and(|given| {
        !given.is_empty() && {
            let a = Sha256::digest(given.as_bytes());
            let b = Sha256::digest(token.as_bytes());
            a.iter()
                .zip(b.iter())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                == 0
        }
    })
}

/// Middleware: lets the request through only with a valid session, or, on
/// `/api/` routes, the configured `api_token` as a Bearer token (no cookie is
/// issued for those). The token only works for GET and HEAD: anything that
/// changes data needs the session cookie.
pub async fn require_session(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    request: Request,
    next: Next,
) -> Response {
    let reads_only = matches!(*request.method(), Method::GET | Method::HEAD);
    if reads_only
        && request.uri().path().starts_with("/api/")
        && bearer_matches(request.headers(), state.api_token.as_deref())
    {
        return next.run(request).await;
    }
    // `get` returns `None` unless the signature checks out.
    let id = jar.get(COOKIE_NAME).map(|c| c.value().to_string());
    if let Some(id) = id {
        match check_session(&state.pool, &id, db::now()).await {
            Ok(SessionCheck::Valid) => return next.run(request).await,
            Ok(SessionCheck::Refreshed) => {
                // Re-send the cookie so the browser's copy slides too.
                let response = next.run(request).await;
                return (jar.add(session_cookie(&state, id)), response).into_response();
            }
            Ok(SessionCheck::Invalid) => {}
            Err(err) => return ApiError::from(err).into_response(),
        }
    }
    if request.uri().path().starts_with("/api/") {
        ApiError::unauthorized().into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

pub async fn login_page() -> Html<String> {
    Html(login_html(None))
}

#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

/// The address lockouts are keyed on: Cloudflare's `CF-Connecting-IP` when
/// present and valid, else the socket peer.
fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> IpAddr {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .or(peer.map(|p| p.ip()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    jar: SignedCookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    let ip = client_ip(&headers, peer.map(|Extension(ConnectInfo(addr))| addr));
    // Guards are temporaries, dropped at the end of the statement, so they
    // are never held across an `.await`.
    let locked = state.lockout.lock().unwrap().is_locked(ip, db::now());
    if locked {
        return too_many_attempts();
    }

    // One password check at a time: each costs ~19 MiB, and parallel guesses
    // would all pass the check above before any failure is recorded.
    let Ok(_permit) = state.verify_gate.acquire().await else {
        return login_error(StatusCode::INTERNAL_SERVER_ERROR, "Login unavailable");
    };
    // Queued requests re-check now that earlier ones have been recorded.
    if state.lockout.lock().unwrap().is_locked(ip, db::now()) {
        return too_many_attempts();
    }

    // Argon2 is deliberately slow, so keep it off the async threads.
    let hash = state.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&form.password, &hash))
        .await
        .unwrap_or(false);

    if !ok {
        state.lockout.lock().unwrap().record_failure(ip, db::now());
        return login_error(StatusCode::UNAUTHORIZED, "Wrong password");
    }
    state.lockout.lock().unwrap().record_success(ip);

    match create_session(&state.pool, db::now()).await {
        Ok(id) => (jar.add(session_cookie(&state, id)), Redirect::to("/")).into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

fn too_many_attempts() -> Response {
    login_error(
        StatusCode::TOO_MANY_REQUESTS,
        "Too many attempts, try again later",
    )
}

pub async fn logout(State(state): State<AppState>, jar: SignedCookieJar) -> Response {
    if let Some(cookie) = jar.get(COOKIE_NAME)
        && let Err(err) = sqlx::query("DELETE FROM session WHERE id = ?")
            .bind(cookie.value())
            .execute(&state.pool)
            .await
    {
        // Still clear the browser cookie, but make the failure visible.
        tracing::error!("could not delete session: {err}");
    }
    (
        jar.remove(Cookie::build(COOKIE_NAME).path("/")),
        Redirect::to("/login"),
    )
        .into_response()
}

fn login_error(status: StatusCode, message: &str) -> Response {
    (status, Html(login_html(Some(message)))).into_response()
}

/// The login form. `error` must be plain text without HTML characters.
fn login_html(error: Option<&str>) -> String {
    let error = error
        .map(|message| format!(r#"<p class="error" role="alert">{message}</p>"#))
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>solar-dash login</title>
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="icon" href="/favicon.ico" sizes="32x32">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<style>
  :root {{ color-scheme: light dark; --bg: #f5f6f8; --fg: #1b1f24; --muted: #5d6671;
          --card: #fff; --line: #d8dce2; --accent: #e8890c; --danger: #c0392b; }}
  @media (prefers-color-scheme: dark) {{
    :root {{ --bg: #111418; --fg: #e6e8eb; --muted: #9aa3ad; --card: #1b2026;
            --line: #343b44; --accent: #f0a030; --danger: #ff7b6b; }}
  }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; padding: 16px;
         box-sizing: border-box; background: var(--bg); color: var(--fg);
         font: 16px/1.5 system-ui, sans-serif; }}
  form {{ width: 100%; max-width: 340px; background: var(--card); border: 1px solid var(--line);
         border-radius: 12px; padding: 28px 24px; box-sizing: border-box; }}
  h1 {{ margin: 0 0 20px; font-size: 1.4rem; text-align: center; }}
  label {{ display: block; margin-bottom: 4px; color: var(--muted); font-size: .9rem; }}
  input, button {{ width: 100%; box-sizing: border-box; font: inherit; padding: 9px 12px;
                  border-radius: 8px; }}
  input {{ border: 1px solid var(--line); background: var(--bg); color: var(--fg); }}
  button {{ margin-top: 16px; border: 0; background: var(--accent); color: #111; font-weight: 600;
           cursor: pointer; }}
  .error {{ margin: 0 0 14px; color: var(--danger); text-align: center; }}
</style>
</head>
<body>
<form method="post" action="/login">
  <h1>solar-dash</h1>
  {error}
  <label for="password">Password</label>
  <input id="password" name="password" type="password" autocomplete="current-password" autofocus required>
  <button type="submit">Log in</button>
</form>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_five_failures() {
        let mut lockout = Lockout::default();
        for i in 0..4 {
            lockout.record_failure(1000 + i);
            assert!(!lockout.is_locked(1000 + i));
        }
        lockout.record_failure(1004);
        assert!(lockout.is_locked(1005));
        // Still locked just before 15 minutes pass, free just after.
        assert!(lockout.is_locked(1004 + LOCKOUT_SECS - 1));
        assert!(!lockout.is_locked(1004 + LOCKOUT_SECS));
    }

    #[test]
    fn old_failures_do_not_count() {
        let mut lockout = Lockout::default();
        for i in 0..4 {
            lockout.record_failure(1000 + i);
        }
        // The next failure comes after the window, so only it counts.
        lockout.record_failure(1000 + FAILURE_WINDOW_SECS + 10);
        assert!(!lockout.is_locked(1000 + FAILURE_WINDOW_SECS + 11));
    }

    #[test]
    fn success_resets_the_count() {
        let mut map = LockoutMap::default();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        for i in 0..4 {
            map.record_failure(ip, 1000 + i);
        }
        map.record_success(ip);
        map.record_failure(ip, 1010);
        assert!(!map.is_locked(ip, 1011));
    }

    #[test]
    fn lockout_is_per_address() {
        let mut map = LockoutMap::default();
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let b: IpAddr = "192.0.2.2".parse().unwrap();
        for i in 0..5 {
            map.record_failure(a, 1000 + i);
        }
        assert!(map.is_locked(a, 1010));
        assert!(!map.is_locked(b, 1010));
    }

    #[test]
    fn lockout_map_is_bounded() {
        let mut map = LockoutMap::default();
        for i in 0..(MAX_TRACKED_IPS as u32 + 500) {
            let ip = IpAddr::V4(Ipv4Addr::from(i));
            map.record_failure(ip, 1000 + i64::from(i));
        }
        assert_eq!(map.entries.len(), MAX_TRACKED_IPS);
        // The oldest were dropped, the newest kept.
        assert!(!map.entries.contains_key(&IpAddr::V4(Ipv4Addr::from(0))));
        assert!(
            map.entries
                .contains_key(&IpAddr::V4(Ipv4Addr::from(MAX_TRACKED_IPS as u32 + 499)))
        );
    }

    #[test]
    fn cf_header_beats_peer_address() {
        let peer: SocketAddr = "10.0.0.9:5555".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(client_ip(&headers, Some(peer)), peer.ip());
        headers.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
        assert_eq!(
            client_ip(&headers, Some(peer)),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
        headers.insert("cf-connecting-ip", "garbage".parse().unwrap());
        assert_eq!(client_ip(&headers, Some(peer)), peer.ip());
    }

    #[test]
    fn password_round_trip() {
        let hash = hash_password("hunter2").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        check_hash(&hash).unwrap();
        assert!(verify_password("hunter2", &hash));
        assert!(!verify_password("hunter3", &hash));
        assert!(check_hash("not a hash").is_err());
    }
}
