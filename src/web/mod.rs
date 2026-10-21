//! The web server: password login, a JSON API over the readings, and the
//! embedded static files. It only reads from SQLite and never calls iSolarCloud.

mod api;
mod auth;
mod costs;
mod payback;

use std::future::Future;
use std::net::SocketAddr;
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
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::cost::Tariff;
use crate::modbus::Latest;
use auth::LockoutMap;
pub use auth::hash_password;
use payback::Purchase;

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
    lockout: Arc<Mutex<LockoutMap>>,
    /// Only one password check runs at a time (each costs ~19 MiB of Argon2
    /// memory), so parallel guesses queue up and see the lockout.
    verify_gate: Arc<Semaphore>,
    /// Whether the session cookie gets the `Secure` flag.
    cookie_secure: bool,
    /// The newest Modbus sample, kept by the poller.
    latest: Latest,
    /// The parsed `[tariff]` section; `None` turns the cost API off.
    tariff: Option<Arc<Tariff>>,
    /// The `[[system.purchase]]` entries, oldest first; empty turns the payback API off.
    purchases: Arc<[Purchase]>,
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
            lockout: Arc::new(Mutex::new(LockoutMap::default())),
            verify_gate: Arc::new(Semaphore::new(1)),
            cookie_secure: true,
            latest: Latest::default(),
            tariff: None,
            purchases: Arc::from([]),
        })
    }

    /// Sets the cookie `Secure` flag (on by default).
    pub fn with_cookie_secure(mut self, secure: bool) -> Self {
        self.cookie_secure = secure;
        self
    }

    /// Turns on the cost API with this tariff.
    pub fn with_tariff(mut self, tariff: Option<Tariff>) -> Self {
        self.tariff = tariff.map(Arc::new);
        self
    }

    /// Turns on the payback API with these purchases (oldest first).
    pub fn with_purchases(mut self, purchases: Vec<Purchase>) -> Self {
        self.purchases = purchases.into();
        self
    }

    /// The slot the Modbus poller writes its newest sample to.
    pub fn latest(&self) -> Latest {
        self.latest.clone()
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
        let secret = config.server.session_secret.as_deref().context(
            "serve needs session_secret in the [server] section of config.toml \
             (make one with `openssl rand -base64 64 | tr -d '\\n'; echo`)",
        )?;
        let key = key_from_secret(secret)?;
        let tariff = config
            .tariff
            .as_ref()
            .map(Tariff::from_config)
            .transpose()?;
        let purchases = payback::purchases_from_config(&config.system.purchase)?;
        Ok(Self::new(pool, plant_id, tz, password_hash, key)?
            .with_cookie_secure(config.server.cookie_secure)
            .with_tariff(tariff)
            .with_purchases(purchases))
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
        .route("/api/costs", get(costs::costs))
        .route("/api/payback", get(payback::payback))
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
    // Connect info gives the login handler the peer address for lockouts.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
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
        test_app_with_options(true).await
    }

    async fn test_app_with_options(cookie_secure: bool) -> (tempfile::TempDir, Router, SqlitePool) {
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
        let state = AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
            .unwrap()
            .with_cookie_secure(cookie_secure);
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
        login_request_from(password, None)
    }

    /// A login as seen from `ip` (sent as `CF-Connecting-IP`).
    fn login_request_from(password: &str, ip: Option<&str>) -> Request<Body> {
        let mut builder = Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(ip) = ip {
            builder = builder.header("cf-connecting-ip", ip);
        }
        builder
            .body(Body::from(format!("password={password}")))
            .unwrap()
    }

    fn logout_request(cookie: &str) -> Request<Body> {
        Request::post("/logout")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
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
        assert_eq!(json, serde_json::json!({ "ok": true }));
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
    async fn secure_flag_follows_config() {
        let set_cookie = |app: Router| async move {
            let response = send(&app, login_request("correct%20horse")).await;
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .to_string()
        };

        let (_dir, app, _pool) = test_app_with_options(true).await;
        let secure = set_cookie(app).await;
        assert!(secure.contains("Secure"), "{secure}");
        assert!(secure.contains("HttpOnly") && secure.contains("SameSite=Lax"));

        let (_dir, app, _pool) = test_app_with_options(false).await;
        let plain = set_cookie(app).await;
        assert!(!plain.contains("Secure"), "{plain}");

        // A client-supplied header changes nothing.
        let (_dir, app, _pool) = test_app_with_options(false).await;
        let request = Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("x-forwarded-proto", "https")
            .body(Body::from("password=correct%20horse"))
            .unwrap();
        let response = send(&app, request).await;
        let spoofed = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(!spoofed.contains("Secure"), "{spoofed}");
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
    async fn logout_revokes_the_session() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/latest", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = send(&app, logout_request(&cookie)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/login");
        // A removal cookie is sent: the same name with an empty value.
        let removal = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(removal.starts_with("session=;"), "{removal}");

        // The copied cookie is still correctly signed, but no longer works.
        let response = send(&app, get_request("/api/latest", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn expired_session_is_rejected() {
        let (_dir, app, pool) = test_app_with_pool().await;
        let cookie = log_in(&app).await;
        sqlx::query("UPDATE session SET expires_at = ?")
            .bind(db::now() - 1)
            .execute(&pool)
            .await
            .unwrap();
        let response = send(&app, get_request("/api/latest", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn expiry_slides_at_most_once_an_hour() {
        let (_dir, app, pool) = test_app_with_pool().await;
        let cookie = log_in(&app).await;
        let row = |pool: SqlitePool| async move {
            sqlx::query_as::<_, (i64, i64)>("SELECT last_seen, expires_at FROM session")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let (seen, expires) = row(pool.clone()).await;

        // A request right after login does not write.
        let response = send(&app, get_request("/api/latest", Some(&cookie))).await;
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(row(pool.clone()).await, (seen, expires));

        // Pretend the session was last touched two hours ago.
        sqlx::query(
            "UPDATE session SET last_seen = last_seen - 7200, expires_at = expires_at - 7200",
        )
        .execute(&pool)
        .await
        .unwrap();
        let response = send(&app, get_request("/api/latest", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::SET_COOKIE).is_some());
        let (new_seen, new_expires) = row(pool).await;
        assert!(new_seen >= seen && new_expires >= expires);
    }

    #[tokio::test]
    async fn login_prunes_expired_sessions() {
        let (_dir, app, pool) = test_app_with_pool().await;
        log_in(&app).await;
        sqlx::query("UPDATE session SET expires_at = ?")
            .bind(db::now() - 1)
            .execute(&pool)
            .await
            .unwrap();
        log_in(&app).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn locking_out_one_ip_does_not_affect_another() {
        let (_dir, app) = test_app().await;
        for _ in 0..5 {
            send(&app, login_request_from("nope", Some("203.0.113.1"))).await;
        }
        let response = send(
            &app,
            login_request_from("correct%20horse", Some("203.0.113.1")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        let response = send(
            &app,
            login_request_from("correct%20horse", Some("203.0.113.2")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn parallel_guesses_are_serialised_and_locked_out() {
        let (_dir, app) = test_app().await;
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let app = app.clone();
                tokio::spawn(async move { send(&app, login_request("nope")).await.status() })
            })
            .collect();
        let mut wrong = 0;
        for handle in handles {
            if handle.await.unwrap() == StatusCode::UNAUTHORIZED {
                wrong += 1;
            }
        }
        // Only the first five guesses were actually checked.
        assert_eq!(wrong, 5);
    }

    #[tokio::test]
    async fn missing_session_secret_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = hash_password(PASSWORD).unwrap();
        let toml = format!(
            r#"
            [isolarcloud]
            appkey = "k"
            access_key = "s"
            rsa_public_key = "r"
            username = "u"
            password = "p"
            plant_id = 7
            [server]
            password_hash = "{hash}"
            "#
        );
        let config: Config = toml::from_str(&toml).unwrap();
        assert!(config.server.cookie_secure);
        let err = AppState::from_config(pool.clone(), &config)
            .err()
            .expect("should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("session_secret") && msg.contains("openssl rand"),
            "{msg}"
        );

        let secret = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
        let ok = toml.replace(
            "[server]",
            &format!("[server]\nsession_secret = \"{secret}\""),
        );
        let config: Config = toml::from_str(&ok).unwrap();
        AppState::from_config(pool, &config).unwrap();
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

    /// A simple tariff: 100 c supply, `import_cents` import, 5 c export, no windows.
    fn flat_tariff_with(import_cents: f64) -> Tariff {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let text = format!(
            "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = {import_cents}\n\
             default_export_cents = 5.0\n"
        );
        let w: Wrapper = toml::from_str(&text).unwrap();
        Tariff::from_config(&w.tariff).unwrap()
    }

    /// The flat tariff at 25 c import.
    fn flat_tariff() -> Tariff {
        flat_tariff_with(25.0)
    }

    /// A router over `pool` with this tariff.
    fn costs_router(pool: &SqlitePool, tariff: Tariff) -> Router {
        let hash = hash_password(PASSWORD).unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let state = AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
            .unwrap()
            .with_tariff(Some(tariff));
        router(state)
    }

    /// A router with the flat tariff over an empty database (plus a pool for seeding).
    async fn costs_app() -> (tempfile::TempDir, Router, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("test.db").to_str().unwrap())
            .await
            .unwrap();
        // Readings need their points to exist first.
        for point_id in [83102, 83072, 83118] {
            let point = Point {
                point_id,
                code: None,
                name: None,
                unit: Some("Wh".to_string()),
            };
            db::upsert_point(&pool, &point).await.unwrap();
        }
        let app = costs_router(&pool, flat_tariff());
        (dir, app, pool)
    }

    /// Unix time of local Adelaide midnight on 2026-09-15 (+09:30, no DST change).
    const SEP_15_START: i64 = 1_789_396_200;

    #[tokio::test]
    async fn costs_are_off_without_a_tariff() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/costs", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/costs", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "configured": false })
        );
    }

    #[tokio::test]
    async fn costs_validate_the_date_range() {
        let (_dir, app, _pool) = costs_app().await;
        let cookie = log_in(&app).await;
        let bad = [
            "/api/costs?from=yesterday",
            "/api/costs?to=2026-13-01",
            "/api/costs?from=2026-09-15&to=2026-09-14",
            // 1,501 days.
            "/api/costs?from=2022-01-01&to=2026-02-09",
        ];
        for uri in bad {
            let response = send(&app, get_request(uri, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert!(body_json(response).await["error"].is_string(), "{uri}");
        }
        // Exactly 1,500 days is fine, and an empty database gives no days.
        let uri = "/api/costs?from=2022-01-01&to=2026-02-08";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["days"], serde_json::json!([]));
        assert_eq!(json["totals"]["days"], 0);
    }

    #[tokio::test]
    async fn costs_for_a_cloud_day() {
        let (_dir, app, pool) = costs_app().await;
        let cookie = log_in(&app).await;
        // Import 1 kWh, export 2 kWh and load 4 kWh, each reached by 01:00 and
        // then flat. A neighbouring day without data is left out.
        let mut readings = Vec::new();
        for (point_id, wh) in [(83102, 1000.0), (83072, 2000.0), (83118, 4000.0)] {
            for ts in [SEP_15_START + 3600, SEP_15_START + 3900] {
                readings.push(Reading {
                    point_id,
                    ts,
                    value: Some(wh),
                });
            }
        }
        db::upsert_readings(&pool, PLANT, &readings).await.unwrap();

        let uri = "/api/costs?from=2026-09-14&to=2026-09-16";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["days"].as_array().unwrap().len(), 1);
        // Actual: 100 supply + 1 kWh * 25 - 2 kWh * 5 = 115 c.
        // Baseline: 100 supply + 4 kWh * 25 = 200 c.
        assert_eq!(
            json["days"][0],
            serde_json::json!({
                "date": "2026-09-15",
                "source": "cloud",
                "actual": 1.15,
                "baseline": 2.0,
                "savings": 0.85,
                "import_kwh": 1.0,
                "export_kwh": 2.0,
                "load_kwh": 4.0,
                "credit": 0.0,
            })
        );
        assert_eq!(
            json["totals"],
            serde_json::json!({
                "actual": 1.15,
                "baseline": 2.0,
                "savings": 0.85,
                "credit_days": 0,
                "days": 1,
            })
        );
    }

    #[tokio::test]
    async fn costs_for_a_live_day() {
        let (_dir, app, pool) = costs_app().await;
        let cookie = log_in(&app).await;
        // A sample every minute all day, importing 1 kW to feed a 1 kW load:
        // 1439 held minutes, since the last sample has no next one.
        for ts in (SEP_15_START..SEP_15_START + 86_400).step_by(60) {
            let sample = Sample {
                grid_w: Some(1000.0),
                load_w: Some(1000.0),
                ..live_sample(ts)
            };
            db::insert_live_sample(&pool, &sample).await.unwrap();
        }
        // Cloud numbers for the same day must be ignored.
        let reading = Reading {
            point_id: 83102,
            ts: SEP_15_START + 3600,
            value: Some(99_000.0),
        };
        db::upsert_readings(&pool, PLANT, &[reading]).await.unwrap();

        let uri = "/api/costs?from=2026-09-15&to=2026-09-15";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        let json = body_json(response).await;
        let day = &json["days"][0];
        assert_eq!(day["source"], "inverter");
        // 1439/60 = 23.98 kWh at 25 c, plus 100 c supply: 6.9958 dollars.
        assert_eq!(day["import_kwh"], 23.98);
        assert_eq!(day["load_kwh"], 23.98);
        assert_eq!(day["export_kwh"], 0.0);
        assert_eq!(day["actual"], 7.0);
        assert_eq!(day["baseline"], 7.0);
        assert_eq!(day["savings"], 0.0);
    }

    /// Counter readings for one day: import `import_wh`, export twice that and
    /// load four times that, all at each offset (seconds after `day_start`).
    async fn seed_counters(pool: &SqlitePool, day_start: i64, offsets: &[i64], import_wh: f64) {
        let mut readings = Vec::new();
        for (point_id, factor) in [(83102, 1.0), (83072, 2.0), (83118, 4.0)] {
            for offset in offsets {
                readings.push(Reading {
                    point_id,
                    ts: day_start + offset,
                    value: Some(import_wh * factor),
                });
            }
        }
        db::upsert_readings(pool, PLANT, &readings).await.unwrap();
    }

    /// How many days are stored in the `daily_cost` table.
    async fn stored_days(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM daily_cost")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// `/api/costs` for a range, logging in on `app` first.
    async fn costs_json(app: &Router, range: &str) -> Value {
        let cookie = log_in(app).await;
        let uri = format!("/api/costs?{range}");
        let response = send(app, get_request(&uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    }

    /// Unix time of the start of today's local day.
    fn today_start() -> i64 {
        let tz = chrono_tz::Australia::Adelaide;
        api::day_bounds_or_error(api::today(tz), tz).unwrap().0
    }

    /// Seconds after midnight of the last reading of a complete cloud day (23:55).
    const LAST_READING: i64 = 86_400 - 300;

    #[tokio::test]
    async fn finished_days_come_from_the_cache_but_today_is_recomputed() {
        let (_dir, app, pool) = costs_app().await;
        let today = api::today(chrono_tz::Australia::Adelaide);
        let range = format!("from=2026-09-15&to={today}");
        seed_counters(&pool, SEP_15_START, &[3600, LAST_READING], 1000.0).await;
        seed_counters(&pool, today_start(), &[3600], 1000.0).await;

        let first = costs_json(&app, &range).await;
        assert_eq!(first["totals"]["days"], 2);
        assert_eq!(first["days"][0]["savings"], 0.85);
        // Only the finished day is stored.
        assert_eq!(stored_days(&pool).await, 1);

        // Wipe the source data and give today new numbers. The finished day
        // can only be answered from the cache; today must be worked out again.
        sqlx::query("DELETE FROM reading")
            .execute(&pool)
            .await
            .unwrap();
        seed_counters(&pool, today_start(), &[3600], 2000.0).await;
        let second = costs_json(&app, &range).await;
        assert_eq!(second["days"][0], first["days"][0]);
        assert_eq!(second["days"][1]["date"], today.to_string());
        assert_eq!(second["days"][1]["savings"], 1.7);
        assert_eq!(stored_days(&pool).await, 1);
    }

    #[tokio::test]
    async fn a_changed_tariff_recomputes_cached_days() {
        let (_dir, app, pool) = costs_app().await;
        seed_counters(&pool, SEP_15_START, &[3600, LAST_READING], 1000.0).await;
        let range = "from=2026-09-15&to=2026-09-15";
        let before = costs_json(&app, range).await;
        assert_eq!(before["days"][0]["actual"], 1.15);
        assert_eq!(stored_days(&pool).await, 1);

        // Same database, import now 35 c: 100 + 35 - 10 = 125 c.
        let dearer = costs_router(&pool, flat_tariff_with(35.0));
        let after = costs_json(&dearer, range).await;
        assert_eq!(after["days"][0]["actual"], 1.25);
        // The row was replaced, not added.
        assert_eq!(stored_days(&pool).await, 1);
        // And the original tariff does not see the replaced row as its own.
        sqlx::query("DELETE FROM reading")
            .execute(&pool)
            .await
            .unwrap();
        let gone = costs_json(&app, range).await;
        assert_eq!(gone["days"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn an_incomplete_day_is_not_cached_until_it_is_complete() {
        let (_dir, app, pool) = costs_app().await;
        let range = "from=2026-09-15&to=2026-09-15";
        // Readings stop at 01:05: the day shows up but is not stored.
        seed_counters(&pool, SEP_15_START, &[3600, 3900], 1000.0).await;
        let partial = costs_json(&app, range).await;
        assert_eq!(partial["days"][0]["import_kwh"], 1.0);
        assert_eq!(stored_days(&pool).await, 0);

        // A backfill adds the rest of the day, so the next request sees it
        // and stores it.
        seed_counters(&pool, SEP_15_START, &[LAST_READING], 3000.0).await;
        let complete = costs_json(&app, range).await;
        assert_eq!(complete["days"][0]["import_kwh"], 3.0);
        assert_eq!(stored_days(&pool).await, 1);
    }

    fn solar_purchase(date: &str, cost_dollars: f64) -> Purchase {
        Purchase {
            name: "Solar".to_string(),
            date: date.parse().unwrap(),
            cost_dollars,
        }
    }

    /// Like `costs_router`, with these purchases as well.
    fn payback_router(pool: &SqlitePool, purchases: Vec<Purchase>) -> Router {
        let hash = hash_password(PASSWORD).unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let state = AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
            .unwrap()
            .with_tariff(Some(flat_tariff()))
            .with_purchases(purchases);
        router(state)
    }

    async fn payback_json(app: &Router) -> Value {
        let cookie = log_in(app).await;
        let response = send(app, get_request("/api/payback", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    }

    #[tokio::test]
    async fn payback_needs_a_tariff_and_purchases() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/payback", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let off = serde_json::json!({ "configured": false });
        assert_eq!(payback_json(&app).await, off);

        // A tariff but no purchases.
        let (_dir, app, pool) = costs_app().await;
        assert_eq!(payback_json(&app).await, off);
        // Purchases but no tariff.
        let hash = hash_password(PASSWORD).unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let state = AppState::new(pool, PLANT, tz, &hash, Key::generate())
            .unwrap()
            .with_purchases(vec![solar_purchase("2026-09-14", 10.0)]);
        assert_eq!(payback_json(&router(state)).await, off);
    }

    #[tokio::test]
    async fn payback_adds_up_savings_against_purchases() {
        let (_dir, _app, pool) = costs_app().await;
        // One finished day saving 85 c (see `costs_for_a_cloud_day`).
        seed_counters(&pool, SEP_15_START, &[3600, LAST_READING], 1000.0).await;
        let app = payback_router(
            &pool,
            vec![
                solar_purchase("2026-09-14", 10.0),
                solar_purchase("2026-10-02", 5.0),
            ],
        );
        let json = payback_json(&app).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["total_cost"], 15.0);
        assert_eq!(json["saved"], 0.85);
        assert_eq!(json["net"], -14.15);
        assert_eq!(json["recovered_pct"], 5.67);
        assert_eq!(json["rate_per_day"], 0.85);
        // 14.15 / 0.85 = 17 days from today: some "YYYY-MM" in the future.
        let projected = json["projected_payback"].as_str().unwrap();
        assert!(projected.len() == 7 && projected.as_bytes()[4] == b'-');
        assert_eq!(json["purchases"][1]["date"], "2026-10-02");
        assert_eq!(json["purchases"][1]["cost"], 5.0);
        // September: the saving and the first purchase. October: the second purchase too.
        assert_eq!(
            json["series"][0],
            serde_json::json!({ "month": "2026-09", "saved": 0.85, "net": -9.15 })
        );
        assert_eq!(
            json["series"][1],
            serde_json::json!({ "month": "2026-10", "saved": 0.85, "net": -14.15 })
        );
        assert_eq!(
            json["by_year"],
            serde_json::json!([{ "year": 2026, "saved": 0.85, "days": 1, "partial": true }])
        );

        // Already paid back: no projection date.
        let cheap = payback_router(&pool, vec![solar_purchase("2026-09-14", 0.5)]);
        let json = payback_json(&cheap).await;
        assert_eq!(json["net"], 0.35);
        assert_eq!(json["projected_payback"], Value::Null);
    }
}
