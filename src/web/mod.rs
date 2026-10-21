//! The web server: password login, a JSON API over the readings, and the
//! embedded static files. It only reads from SQLite and never calls iSolarCloud.

mod api;
mod auth;

use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{FromRef, Path};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::Key;
use base64::Engine;
use chrono_tz::Tz;
use rust_embed::RustEmbed;
use sqlx::SqlitePool;
use tokio::net::TcpListener;
use tracing::warn;

use crate::config::Config;
use auth::Lockout;
pub use auth::hash_password;

/// The files in `static/`, compiled into the binary.
// (In debug builds rust-embed reads them from disk instead, so edits show up
// without recompiling.)
#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

/// Everything the handlers share. Cloning is cheap: the pool and the `Arc`s
/// are handles to the same underlying data.
#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    plant_id: i64,
    tz: Tz,
    /// The Argon2 PHC string the login password is checked against.
    password_hash: Arc<str>,
    /// Signs the session cookie.
    key: Key,
    /// Failed-login bookkeeping. A plain `Mutex` is fine: it is only held for
    /// a moment and never across an `.await`.
    lockout: Arc<Mutex<Lockout>>,
}

// Lets `SignedCookieJar` find the key inside our state.
impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.key.clone()
    }
}

impl AppState {
    pub fn new(
        pool: SqlitePool,
        plant_id: i64,
        tz: Tz,
        password_hash: &str,
        key: Key,
    ) -> Result<Self> {
        auth::check_hash(password_hash)?;
        Ok(Self {
            pool,
            plant_id,
            tz,
            password_hash: password_hash.into(),
            key,
            lockout: Arc::new(Mutex::new(Lockout::default())),
        })
    }

    /// Builds the state from the config, with clear errors for missing settings.
    pub fn from_config(pool: SqlitePool, config: &Config) -> Result<Self> {
        let password_hash = config.server.password_hash.as_deref().context(
            "serve needs password_hash in the [server] section of config.toml \
             (run `cargo run -- hash-password` to make one)",
        )?;
        let plant_id = config
            .isolarcloud
            .plant_id
            .context("serve needs plant_id in config.toml (run login-test to see your plants)")?;
        let tz: Tz = config.isolarcloud.timezone.parse().map_err(|_| {
            anyhow::anyhow!(
                "unknown timezone {:?} in config.toml",
                config.isolarcloud.timezone
            )
        })?;
        let key = match &config.server.session_secret {
            Some(secret) => key_from_secret(secret)?,
            None => {
                warn!("no session_secret in config.toml; logins will reset on every restart");
                Key::generate()
            }
        };
        Self::new(pool, plant_id, tz, password_hash, key)
    }
}

/// Decodes `session_secret` (base64) into a signing key.
fn key_from_secret(secret: &str) -> Result<Key> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(secret.trim())
        .context("session_secret is not valid base64 (make one with `openssl rand -base64 64`)")?;
    if bytes.len() < 64 {
        bail!(
            "session_secret is too short: need at least 64 bytes, got {} \
             (make one with `openssl rand -base64 64`)",
            bytes.len()
        );
    }
    Ok(Key::from(&bytes))
}

/// All the routes of the web server.
pub fn router(state: AppState) -> Router {
    // These need a valid session. `route_layer` only wraps the routes added
    // before it, so the public routes below are untouched.
    let protected = Router::new()
        .route("/", get(index))
        .route("/static/{*path}", get(static_file))
        .route("/api/latest", get(api::latest))
        .route("/api/live", get(api::live))
        .route("/api/live/series", get(api::live_series))
        .route("/api/series", get(api::series))
        .route("/api/day", get(api::day))
        .route("/api/daily", get(api::daily))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    let public = Router::new()
        .route("/healthz", get(api::healthz))
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/logout", post(auth::logout));

    protected.merge(public).with_state(state)
}

/// Serves requests on `listener` until `shutdown` completes.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("web server failed")
}

async fn index() -> Response {
    serve_asset("index.html")
}

async fn static_file(Path(path): Path<String>) -> Response {
    serve_asset(&path)
}

/// Looks `path` up in the embedded files and sends it with the right content type.
fn serve_asset(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => (
            [(header::CONTENT_TYPE, file.metadata.mimetype().to_string())],
            file.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::db::{self, Point, Reading};
    use crate::modbus::Sample;

    const PLANT: i64 = 7;
    const PASSWORD: &str = "correct horse";

    /// A router over a temp database with a few seeded rows.
    async fn test_app() -> (tempfile::TempDir, Router) {
        let (dir, app, _pool) = test_app_with_pool().await;
        (dir, app)
    }

    /// Like `test_app`, but also gives back the pool, for seeding more rows.
    async fn test_app_with_pool() -> (tempfile::TempDir, Router, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("test.db").to_str().unwrap())
            .await
            .unwrap();
        let point = Point {
            point_id: 83033,
            code: None,
            name: Some("Plant power".to_string()),
            unit: Some("W".to_string()),
        };
        db::upsert_point(&pool, &point).await.unwrap();
        let readings: Vec<Reading> = (0..3)
            .map(|i| Reading {
                point_id: 83033,
                ts: 1_800_000_000 + i * 300,
                value: Some(1000.0 + i as f64),
            })
            .collect();
        db::upsert_readings(&pool, PLANT, &readings).await.unwrap();

        let hash = hash_password(PASSWORD).unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let state = AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate()).unwrap();
        (dir, router(state), pool)
    }

    async fn send(app: &Router, request: Request<Body>) -> Response {
        app.clone().oneshot(request).await.unwrap()
    }

    fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut builder = Request::get(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        builder.body(Body::empty()).unwrap()
    }

    fn login_request(password: &str) -> Request<Body> {
        Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("password={password}")))
            .unwrap()
    }

    async fn body_text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn body_json(response: Response) -> Value {
        serde_json::from_str(&body_text(response).await).unwrap()
    }

    /// Logs in and returns the `session=...` part of the Set-Cookie header.
    async fn log_in(app: &Router) -> String {
        let response = send(app, login_request("correct%20horse")).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        set_cookie.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn healthz_is_public() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/healthz", None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["ok"], true);
        assert_eq!(json["last_reading_ts"], 1_800_000_600);
    }

    #[tokio::test]
    async fn api_needs_a_session() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/latest", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_json(response).await["error"], "unauthorized");

        // A browser page redirects to the login form instead.
        let response = send(&app, get_request("/", None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/login");

        // A cookie that was not signed by us is rejected.
        let response = send(
            &app,
            get_request("/api/latest", Some("session=99999999999")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_then_api_works() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;

        let response = send(&app, get_request("/api/latest", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let point = &json["points"][0];
        assert_eq!(point["point_id"], 83033);
        assert_eq!(point["name"], "Plant power");
        assert_eq!(point["unit"], "W");
        assert_eq!(point["ts"], 1_800_000_600);
        assert_eq!(point["value"], 1002.0);

        // The dashboard page loads with the cookie.
        let response = send(&app, get_request("/", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }

    #[tokio::test]
    async fn static_files_are_served_after_login() {
        let (_dir, app) = test_app().await;

        // Static files need a session like everything else.
        let response = send(&app, get_request("/static/app.js", None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let cookie = log_in(&app).await;
        for (uri, content_type) in [
            ("/static/app.js", "javascript"),
            ("/static/style.css", "text/css"),
        ] {
            let response = send(&app, get_request(uri, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let actual = response.headers()[header::CONTENT_TYPE].to_str().unwrap();
            assert!(actual.contains(content_type), "{uri}: {actual}");
        }
        let response = send(&app, get_request("/static/missing.js", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cookie_flags_follow_the_request_scheme() {
        let (_dir, app) = test_app().await;
        let response = send(&app, login_request("correct%20horse")).await;
        let plain = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Lax"));
        assert!(!plain.contains("Secure"));

        let request = Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("x-forwarded-proto", "https")
            .body(Body::from("password=correct%20horse"))
            .unwrap();
        let response = send(&app, request).await;
        let secure = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(secure.contains("Secure"));
    }

    #[tokio::test]
    async fn wrong_password_shows_error() {
        let (_dir, app) = test_app().await;
        let response = send(&app, login_request("nope")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert!(body_text(response).await.contains("Wrong password"));
    }

    #[tokio::test]
    async fn too_many_wrong_passwords_lock_logins() {
        let (_dir, app) = test_app().await;
        for _ in 0..5 {
            send(&app, login_request("nope")).await;
        }
        // Even the right password is refused while locked.
        let response = send(&app, login_request("correct%20horse")).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            body_text(response)
                .await
                .contains("Too many attempts, try again later")
        );
    }

    #[tokio::test]
    async fn logout_clears_cookie() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;
        let request = Request::post("/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/login");
        // A removal cookie is sent: the same name with an empty value.
        let removal = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(removal.starts_with("session=;"), "{removal}");
    }

    #[tokio::test]
    async fn series_validates_input() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;
        let bad = [
            "/api/series",
            "/api/series?points=abc&from=1&to=2",
            "/api/series?points=83033&from=2&to=1",
            "/api/series?points=83033&from=0&to=2678401",
            "/api/series?points=1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21&from=1&to=2",
            "/api/daily?from=2026-01-01&to=2027-12-31",
            "/api/day?date=yesterday",
        ];
        for uri in bad {
            let response = send(&app, get_request(uri, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert!(body_json(response).await["error"].is_string(), "{uri}");
        }

        let uri = "/api/series?points=83033&from=1800000000&to=1800000300";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let series = &json["series"][0];
        assert_eq!(series["point_id"], 83033);
        assert_eq!(series["data"].as_array().unwrap().len(), 2);
        assert_eq!(series["data"][1][1], 1001.0);
    }

    #[tokio::test]
    async fn day_and_daily_respond() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;

        // 1_800_000_000 is 2027-01-15 07:20 UTC, i.e. 17:50 in Adelaide.
        let response = send(&app, get_request("/api/day?date=2027-01-15", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["date"], "2027-01-15");
        // Adelaide is UTC+10:30 in January, so the local day starts at 13:30 UTC the day before.
        assert_eq!(json["start"], 1_799_933_400);
        assert_eq!(json["end"], 1_799_933_400 + 24 * 3600);
        let pv = &json["series"][0];
        assert_eq!(pv["key"], "pv_w");
        assert_eq!(pv["data"].as_array().unwrap().len(), 3);

        let uri = "/api/daily?from=2027-01-15&to=2027-01-15";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["days"][0]["date"], "2027-01-15");
        assert!(json["days"][0]["pv_kwh"].is_null());
    }

    fn live_sample(ts: i64) -> Sample {
        Sample {
            ts,
            pv_w: Some(4895.0),
            load_w: Some(708.0),
            grid_w: Some(0.0),
            battery_w: Some(4228.0),
            soc_pct: None,
        }
    }

    #[tokio::test]
    async fn live_needs_a_session_and_is_null_without_data() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/live", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/live", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "sample": null })
        );
    }

    #[tokio::test]
    async fn live_returns_a_fresh_sample_but_not_a_stale_one() {
        let (_dir, app, pool) = test_app_with_pool().await;
        let cookie = log_in(&app).await;

        // 10 minutes old: too stale to be "live".
        let stale = live_sample(db::now() - 600);
        db::insert_live_sample(&pool, &stale).await.unwrap();
        let response = send(&app, get_request("/api/live", Some(&cookie))).await;
        assert!(body_json(response).await["sample"].is_null());

        let fresh = live_sample(db::now() - 10);
        db::insert_live_sample(&pool, &fresh).await.unwrap();
        let response = send(&app, get_request("/api/live", Some(&cookie))).await;
        let json = body_json(response).await;
        let sample = &json["sample"];
        assert_eq!(sample["ts"], fresh.ts);
        assert_eq!(sample["pv_w"], 4895.0);
        assert_eq!(sample["load_w"], 708.0);
        assert_eq!(sample["grid_w"], 0.0);
        assert_eq!(sample["battery_w"], 4228.0);
        assert!(sample["soc_pct"].is_null());
    }

    #[tokio::test]
    async fn live_series_validates_input_and_returns_minute_averages() {
        let (_dir, app, pool) = test_app_with_pool().await;
        let cookie = log_in(&app).await;
        let bad = [
            "/api/live/series",
            "/api/live/series?from=1",
            "/api/live/series?from=x&to=2",
            "/api/live/series?from=2&to=1",
            // 2 days and 1 second.
            "/api/live/series?from=0&to=172801",
        ];
        for uri in bad {
            let response = send(&app, get_request(uri, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert!(body_json(response).await["error"].is_string(), "{uri}");
        }

        // Two samples in the minute starting at 1_800_000_000 (a multiple of 60).
        for (ts, pv) in [(1_800_000_005, 100.0), (1_800_000_035, 300.0)] {
            let row = Sample {
                pv_w: Some(pv),
                ..live_sample(ts)
            };
            db::insert_live_sample(&pool, &row).await.unwrap();
        }
        let uri = "/api/live/series?from=1800000000&to=1800000059";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let rows = json["data"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            serde_json::json!([1_800_000_000, 200.0, 708.0, 0.0, 4228.0, null])
        );

        // An exact 2-day range is allowed and may be empty.
        let uri = "/api/live/series?from=0&to=172800";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["data"], serde_json::json!([]));
    }
}
