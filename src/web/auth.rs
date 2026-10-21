//! Password login: Argon2 password check, a signed session cookie, a lockout
//! after repeated failures, and the middleware that guards the protected routes.

use std::collections::VecDeque;

use anyhow::{Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::Form;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, SameSite, SignedCookieJar};
use serde::Deserialize;
use time::Duration;

use super::AppState;
use super::api::ApiError;
use crate::db;

const COOKIE_NAME: &str = "session";
const SESSION_SECS: i64 = 30 * 24 * 3600;

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

    pub fn record_success(&mut self) {
        self.failures.clear();
    }
}

/// Whether the request reached us over https. Cloudflare Tunnel terminates
/// TLS and talks plain http to us, so it tells us with this header.
fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

/// True if the jar holds a validly signed cookie whose expiry is in the future.
fn has_valid_session(jar: &SignedCookieJar) -> bool {
    // `get` returns `None` unless the signature checks out.
    jar.get(COOKIE_NAME)
        .and_then(|cookie| cookie.value().parse::<i64>().ok())
        .is_some_and(|expiry| expiry > db::now())
}

/// Middleware: lets the request through only with a valid session.
pub async fn require_session(jar: SignedCookieJar, request: Request, next: Next) -> Response {
    if has_valid_session(&jar) {
        return next.run(request).await;
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

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: SignedCookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    let now = db::now();
    // The guard is a temporary, dropped at the end of the statement, so it is
    // never held across an `.await`.
    let locked = state.lockout.lock().unwrap().is_locked(now);
    if locked {
        return login_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts, try again later",
        );
    }

    // Argon2 is deliberately slow, so keep it off the async threads.
    let hash = state.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&form.password, &hash))
        .await
        .unwrap_or(false);

    if !ok {
        state.lockout.lock().unwrap().record_failure(now);
        return login_error(StatusCode::UNAUTHORIZED, "Wrong password");
    }
    state.lockout.lock().unwrap().record_success();

    let expiry = now + SESSION_SECS;
    let cookie = Cookie::build((COOKIE_NAME, expiry.to_string()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(is_https(&headers))
        .max_age(Duration::seconds(SESSION_SECS));
    (jar.add(cookie), Redirect::to("/")).into_response()
}

pub async fn logout(jar: SignedCookieJar) -> Response {
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
        let mut lockout = Lockout::default();
        for i in 0..4 {
            lockout.record_failure(1000 + i);
        }
        lockout.record_success();
        lockout.record_failure(1010);
        assert!(!lockout.is_locked(1011));
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
