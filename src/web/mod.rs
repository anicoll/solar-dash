//! The web server: password login, a JSON API over the readings, and the
//! embedded static files. It only reads from SQLite and never calls iSolarCloud.

mod admin;
mod api;
mod auth;
mod battery;
mod bill;
mod costs;
mod fuel;
mod globird;
mod payback;
mod petrol;

pub(crate) use costs::house_intervals;

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::{FromRef, Path};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum_extra::extract::cookie::Key;
use base64::Engine;
use chrono_tz::Tz;
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_http::compression::CompressionLayer;

use crate::admin_data::{AdminData, SharedAdminData};
use crate::bills::PastBill;
use crate::config::Config;
use crate::cost::TariffSchedule;
use crate::ev_cost::PetrolHistory;
use crate::ev_cost::{CostSetup, EvPrices};
use crate::modbus::{Latest, LatestSettings};
use crate::teslamate::Car;
pub(crate) use api::ApiError;
use auth::LockoutMap;
pub use auth::hash_password;
pub(crate) use payback::{Purchase, purchases_from_config};

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
    /// `api_token` from `[server]`: a Bearer token accepted on `/api/` routes.
    /// Never logged.
    api_token: Option<Arc<str>>,
    /// The newest Modbus sample, kept by the poller.
    latest: Latest,
    /// The inverter's control settings, kept by the poller.
    latest_settings: LatestSettings,
    /// The car's charge state from TeslaMate, kept by the MQTT task.
    car: Car,
    /// The tariffs, petrol prices, purchases, bills and preferences. The
    /// admin page swaps the whole snapshot; handlers call `admin.get()` once.
    admin: SharedAdminData,
    /// Whether the config has a `[globird]` section.
    globird_configured: bool,
    /// Where the nightly backups are, for the admin page's download.
    backup_dir: Option<PathBuf>,
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
            api_token: None,
            latest: Latest::default(),
            latest_settings: LatestSettings::default(),
            car: Car::default(),
            admin: SharedAdminData::new(AdminData::default()),
            globird_configured: false,
            backup_dir: None,
        })
    }

    /// Sets the folder the admin page downloads the latest backup from.
    pub fn with_backup_dir(mut self, dir: PathBuf) -> Self {
        self.backup_dir = Some(dir);
        self
    }

    /// The backup folder, if one was set.
    pub(crate) fn backup_dir(&self) -> Option<&std::path::Path> {
        self.backup_dir.as_deref()
    }

    /// Sets the cookie `Secure` flag (on by default).
    pub fn with_cookie_secure(mut self, secure: bool) -> Self {
        self.cookie_secure = secure;
        self
    }

    /// Accepts `Authorization: Bearer <token>` on `/api/` routes (off by default).
    pub fn with_api_token(mut self, token: Option<&str>) -> Self {
        self.api_token = token.map(Arc::from);
        self
    }

    /// Changes the admin data. The state gets its own slot, so a clone made
    /// before the call is not affected (tests rely on this).
    fn edit_admin(mut self, change: impl FnOnce(&mut AdminData)) -> Self {
        let mut data = (*self.admin.get()).clone();
        change(&mut data);
        self.admin = SharedAdminData::new(data);
        self
    }

    /// Puts in the admin data as a whole, e.g. one built from the config.
    pub fn with_admin_data(self, data: AdminData) -> Self {
        self.edit_admin(|d| *d = data)
    }

    /// Turns on the cost API with these plans.
    pub fn with_tariff(self, tariff: Option<TariffSchedule>) -> Self {
        self.edit_admin(|d| d.tariff = tariff.map(Arc::new))
    }

    /// Sets the prices the car-versus-petrol summary uses.
    pub fn with_ev_prices(self, prices: EvPrices) -> Self {
        self.edit_admin(|d| d.ev_prices = prices)
    }

    /// Sets the petrol prices by day that the per-car history uses.
    pub fn with_petrol_prices(self, petrol: PetrolHistory) -> Self {
        self.edit_admin(|d| d.petrol = petrol)
    }

    /// Sets the petrol car each model is compared with and the petrol grade,
    /// both shown by `/api/ev/cars`.
    pub fn with_petrol_comparison(
        self,
        compare_cars: BTreeMap<String, String>,
        petrol_grade: &str,
    ) -> Self {
        self.edit_admin(|d| {
            d.compare_cars = compare_cars;
            d.petrol_grade = petrol_grade.to_string();
        })
    }

    /// Turns on the payback API with these purchases (oldest first).
    pub fn with_purchases(self, purchases: Vec<Purchase>) -> Self {
        self.edit_admin(|d| d.purchases = purchases.into())
    }

    /// Sets the yearly price rise (percent) the payback projection assumes.
    pub fn with_price_rise_percent(self, percent: f64) -> Self {
        self.edit_admin(|d| d.price_rise_percent = percent)
    }

    /// Records whether a `[globird]` section is configured, which the compare
    /// API reports as `configured`.
    pub fn with_globird_configured(mut self, configured: bool) -> Self {
        self.globird_configured = configured;
        self
    }

    /// Sets the past bills (oldest first) whose totals replace our estimate.
    pub fn with_bills(self, bills: Vec<PastBill>) -> Self {
        self.edit_admin(|d| d.bills = bills.into())
    }

    /// The slot the Modbus poller writes its newest sample to.
    pub fn latest(&self) -> Latest {
        self.latest.clone()
    }

    /// The slot the Modbus poller writes the inverter's settings to.
    pub fn latest_settings(&self) -> LatestSettings {
        self.latest_settings.clone()
    }

    /// The slot the TeslaMate task writes the car's state to.
    pub fn car(&self) -> Car {
        self.car.clone()
    }

    /// What the TeslaMate history sync needs to cost past sessions. It reads
    /// the admin data again on every round, so edits apply without a restart.
    pub fn cost_setup(&self) -> CostSetup {
        CostSetup {
            admin: self.admin.clone(),
            plant_id: self.plant_id,
        }
    }

    /// The database, for the admin writes in `admin_data`.
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The shared admin data, which `admin_data::write` swaps after a change.
    pub(crate) fn admin(&self) -> &SharedAdminData {
        &self.admin
    }

    /// The plant's time zone, which decides where a day ends.
    pub fn tz(&self) -> Tz {
        self.tz
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
        let state = Self::new(pool, plant_id, tz, password_hash, key)?;
        Ok(state
            .with_cookie_secure(config.server.cookie_secure)
            .with_api_token(config.server.api_token.as_deref())
            .with_globird_configured(config.globird.is_some()))
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
        .route("/classic", get(classic_index))
        .route("/next", get(next_redirect))
        .route("/static/{*path}", get(static_file))
        .route("/api/latest", get(api::latest))
        .route("/api/live", get(api::live))
        .route("/api/live/series", get(api::live_series))
        .route("/api/inverter", get(api::inverter))
        .route("/api/inverter/history", get(api::inverter_history))
        .route("/api/ev/sessions", get(api::ev_sessions))
        .route("/api/ev/summary", get(api::ev_summary))
        .route("/api/ev/cars", get(api::ev_cars))
        .route("/api/fuel/cars", get(fuel::cars))
        .route("/api/petrol-prices", get(petrol::list).put(petrol::put))
        .route("/api/petrol-prices/{from}", delete(petrol::remove))
        .route("/api/admin", get(admin::read))
        .route("/api/admin/tariffs", post(admin::tariff_create))
        .route("/api/admin/tariffs/preview", post(admin::tariff_preview))
        .route(
            "/api/admin/tariffs/{id}",
            put(admin::tariff_update).delete(admin::tariff_delete),
        )
        .route("/api/admin/purchases", post(admin::purchase_create))
        .route(
            "/api/admin/purchases/{id}",
            put(admin::purchase_update).delete(admin::purchase_delete),
        )
        .route("/api/admin/cars", post(admin::car_create))
        .route(
            "/api/admin/cars/{id}",
            put(admin::car_update).delete(admin::car_delete),
        )
        .route("/api/admin/cars/{id}/fills", post(admin::fill_create))
        .route(
            "/api/admin/teslamate-cars/{id}/owned-since",
            put(admin::teslamate_car_owned_since),
        )
        .route(
            "/api/admin/fills/{id}",
            put(admin::fill_update).delete(admin::fill_delete),
        )
        .route("/api/admin/bills", put(admin::bills_replace))
        .route("/api/admin/bills/{from}", delete(admin::bill_delete))
        .route("/api/admin/settings", put(admin::settings_update))
        .route("/api/admin/backup", get(admin::backup_info))
        .route("/api/admin/backup/download", get(admin::backup_download))
        .route("/api/series", get(api::series))
        .route("/api/day", get(api::day))
        .route("/api/daily", get(api::daily))
        .route("/api/battery", get(battery::battery))
        .route("/api/costs", get(costs::costs))
        .route("/api/bill/current", get(bill::current))
        .route("/api/tariff", get(costs::tariff))
        .route("/api/globird/compare", get(globird::compare))
        .route("/api/payback", get(payback::payback))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    let public = Router::new()
        .route("/healthz", get(api::healthz))
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/favicon.svg", get(|h: HeaderMap| icon("favicon.svg", h)))
        .route(
            "/favicon.ico",
            get(|h: HeaderMap| icon("favicon-32.png", h)),
        )
        .route(
            "/apple-touch-icon.png",
            get(|h: HeaderMap| icon("apple-touch-icon.png", h)),
        );

    // Compression is outermost so it sees every response; the security
    // headers sit just inside it and cover every route, including 401s.
    protected
        .merge(public)
        .with_state(state)
        .layer(middleware::from_fn(same_origin))
        .layer(middleware::from_fn(security_headers))
        .layer(CompressionLayer::new())
}

/// Refuses a request that changes something (anything but GET or HEAD) when
/// its `Origin` header names a different host than the `Host` header, so a
/// page on another site cannot drive the API with a visitor's login. Requests
/// without an `Origin` (curl, scripts) pass; browsers always send one on
/// cross-site writes.
async fn same_origin(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && let Some(origin) = request.headers().get(header::ORIGIN) {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .or_else(|| request.uri().authority().map(|a| a.as_str()));
        // "https://solar.example.com" -> "solar.example.com"; "null" has no "://".
        let origin_host = origin
            .to_str()
            .ok()
            .and_then(|o| o.split_once("://"))
            .map(|(_, host)| host);
        let same = matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h));
        if !same {
            return api::ApiError::new(StatusCode::FORBIDDEN, "cross-origin request refused")
                .into_response();
        }
    }
    next.run(request).await
}

/// Content-Security-Policy: everything comes from this origin, except ECharts
/// from cdnjs. Inline styles are allowed (the login page has a `<style>`).
const CSP: &str = "default-src 'self'; script-src 'self' https://cdnjs.cloudflare.com; \
                   style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
                   frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Adds the security headers to every response, and `Cache-Control: no-store`
/// to the (private) `/api/` ones.
async fn security_headers(request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Not "no-referrer": with that, browsers send `Origin: null` on our own
    // form posts (login, logout), which `same_origin` would refuse.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    if is_api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
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

async fn index(headers: HeaderMap) -> Response {
    serve_page(&NEXT_PAGE, &headers)
}

/// The previous dashboard, kept at `/classic` until it is removed.
async fn classic_index(headers: HeaderMap) -> Response {
    serve_page(&CLASSIC_PAGE, &headers)
}

/// `/next` was the new dashboard's address while it was being built. Old
/// bookmarks (`/next#bill`) keep working: a browser carries the `#` part over
/// a redirect.
async fn next_redirect() -> Redirect {
    Redirect::to("/")
}

/// Sends a prepared page with its ETag, or a 304 if the browser has it.
fn serve_page(page: &IndexPage, headers: &HeaderMap) -> Response {
    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::ETAG, HeaderValue::from_str(&page.etag).unwrap());
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if etag_matches(headers, &page.etag) {
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    (response_headers, page.html.clone()).into_response()
}

/// An HTML page with `?v=<hash>` added to each stylesheet and script link.
struct IndexPage {
    html: String,
    etag: String,
}

/// Reads the embedded page `file` and versions the links to `assets` (paths
/// under `static/`, such as `app.js`).
fn build_page(file: &str, assets: &[&str]) -> IndexPage {
    let data = Assets::get(file).unwrap_or_else(|| panic!("{file} is embedded"));
    let mut html = String::from_utf8_lossy(&data.data).into_owned();
    for name in assets {
        if let Some(asset) = Assets::get(name) {
            let version = &hex::encode(asset.metadata.sha256_hash())[..12];
            let plain = format!("\"/static/{name}\"");
            html = html.replace(&plain, &format!("\"/static/{name}?v={version}\""));
        }
    }
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(html.as_bytes())));
    IndexPage { html, etag }
}

/// Each build changes the links to app.js and style.css, so a browser or a
/// proxy in front (such as Cloudflare) can never pair a new page with an old
/// cached script, which would leave the page blank. This is the previous
/// dashboard, served at `/classic`.
static CLASSIC_PAGE: LazyLock<IndexPage> =
    LazyLock::new(|| build_page("index.html", &["app.js", "style.css"]));

/// The same for the current dashboard, served at `/`: the shared files, then one stylesheet and one
/// script for each page.
static NEXT_PAGE: LazyLock<IndexPage> = LazyLock::new(|| {
    build_page(
        "next/index.html",
        &[
            "next/next.css",
            "next/today.css",
            "next/bill.css",
            "next/history.css",
            "next/cars.css",
            "next/payback.css",
            "next/admin.css",
            "next/core.js",
            "next/today.js",
            "next/bill.js",
            "next/history.js",
            "next/cars.js",
            "next/payback.js",
            "next/admin.js",
        ],
    )
});

/// Whether an If-None-Match header names `etag` (weak or strong) or `*`.
fn etag_matches(request_headers: &HeaderMap, etag: &str) -> bool {
    request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/"))
                .any(|t| t == "*" || t == etag)
        })
}

async fn static_file(Path(path): Path<String>, headers: HeaderMap) -> Response {
    serve_asset(&path, &headers)
}

/// Serves one of the icons without a session, since the login page and
/// browsers' automatic `/favicon.ico` request carry no cookie. Icons rarely
/// change, so unlike the other static files they may be cached for a day.
async fn icon(name: &'static str, headers: HeaderMap) -> Response {
    let mut response = serve_asset(name, &headers);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    response
}

/// Looks `path` up in the embedded files and sends it with the right content
/// type. The ETag is the file's hash and `no-cache` makes browsers revalidate
/// every time, so a new build shows at once and an unchanged file costs a 304.
fn serve_asset(path: &str, request_headers: &HeaderMap) -> Response {
    let Some(file) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", hex::encode(file.metadata.sha256_hash()));
    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));

    if etag_matches(request_headers, &etag) {
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(file.metadata.mimetype()).unwrap(),
    );
    // Release builds embed the bytes in the binary; borrow them instead of copying.
    let body = match file.data {
        std::borrow::Cow::Borrowed(bytes) => Body::from(bytes),
        std::borrow::Cow::Owned(bytes) => Body::from(bytes),
    };
    (response_headers, body).into_response()
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
    use crate::teslamate::CarState;

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

    /// A router whose petrol prices come from the tables (a fallback price of
    /// 190 is set), with an API token.
    async fn petrol_app(token: Option<&str>) -> (tempfile::TempDir, Router, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("test.db").to_str().unwrap())
            .await
            .unwrap();
        sqlx::query("INSERT INTO setting (key, value) VALUES ('petrol_cents_per_litre', '190.0')")
            .execute(&pool)
            .await
            .unwrap();
        let data = AdminData::load(&mut pool.acquire().await.unwrap())
            .await
            .unwrap();
        let hash = hash_password(PASSWORD).unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let state = AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
            .unwrap()
            .with_api_token(token)
            .with_admin_data(data);
        (dir, router(state), pool)
    }

    fn json_request(method: &str, uri: &str, cookie: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn delete_request(uri: &str, cookie: &str) -> Request<Body> {
        Request::delete(uri)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn petrol_prices_need_a_session() {
        let (_dir, app, pool) = petrol_app(None).await;
        let put = Request::put("/api/petrol-prices")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"from":"2026-03-01","cents_per_litre":199.0}"#,
            ))
            .unwrap();
        for request in [
            get_request("/api/petrol-prices", None),
            put,
            Request::delete("/api/petrol-prices/2026-03-01")
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = send(&app, request).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM petrol_price")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 0);
    }

    #[tokio::test]
    async fn petrol_prices_can_be_put_listed_and_deleted() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;

        let response = send(&app, get_request("/api/petrol-prices", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({"fallback_cents_per_litre": 190.0, "prices": []})
        );

        let put = |body: &str| json_request("PUT", "/api/petrol-prices", &cookie, body);
        let response = send(
            &app,
            put(r#"{"from":"2026-03-01","cents_per_litre":199.0}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = send(
            &app,
            put(r#"{"from":"2025-01-01","cents_per_litre":185.5}"#),
        )
        .await;
        assert_eq!(
            body_json(response).await["prices"],
            serde_json::json!([
                {"from": "2025-01-01", "cents_per_litre": 185.5},
                {"from": "2026-03-01", "cents_per_litre": 199.0},
            ])
        );
        // The list and the cars see the new prices without a restart.
        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        assert_eq!(body_json(response).await["configured"], true);

        // Replacing a date keeps one row for it.
        let response = send(
            &app,
            put(r#"{"from":"2026-03-01","cents_per_litre":201.5}"#),
        )
        .await;
        assert_eq!(
            body_json(response).await["prices"][1]["cents_per_litre"],
            201.5
        );

        let response = send(
            &app,
            delete_request("/api/petrol-prices/2025-01-01", &cookie),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await["prices"],
            serde_json::json!([{"from": "2026-03-01", "cents_per_litre": 201.5}])
        );
        let response = send(
            &app,
            delete_request("/api/petrol-prices/2025-01-01", &cookie),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = send(&app, delete_request("/api/petrol-prices/March", &cookie)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Every successful write is in the change log, with before and after.
        let log: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT action, key, before, after FROM admin_change \
             WHERE what = 'petrol' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let actions: Vec<&str> = log.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(actions, ["create", "create", "update", "delete"]);
        assert!(log[0].2.is_none() && log[0].3.is_some());
        assert!(log[2].2.as_deref().unwrap().contains("199.0"));
        assert!(log[3].3.is_none());
    }

    #[tokio::test]
    async fn petrol_prices_reject_bad_input() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        for body in [
            r#"{"from":"March","cents_per_litre":199.0}"#,
            r#"{"from":"2026-03-01","cents_per_litre":0}"#,
            r#"{"from":"2026-03-01","cents_per_litre":-3.5}"#,
            r#"{"from":"2026-03-01","cents_per_litre":"lots"}"#,
            r#"{"from":"2026-03-01"}"#,
            "not json",
        ] {
            let response = send(
                &app,
                json_request("PUT", "/api/petrol-prices", &cookie, body),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
            assert!(body_json(response).await["error"].is_string(), "{body}");
        }
        // A form cannot write: the body must be JSON.
        let form = Request::put("/api/petrol-prices")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("from=2026-03-01&cents_per_litre=199"))
            .unwrap();
        let response = send(&app, form).await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM petrol_price")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows.0, 0);
    }

    #[tokio::test]
    async fn api_token_is_accepted_as_a_bearer_on_api_routes() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let bearer = |uri: &str, value: &str| {
            Request::get(uri)
                .header(header::AUTHORIZATION, value)
                .body(Body::empty())
                .unwrap()
        };
        let (_dir, app, _path) = petrol_app(Some(token)).await;

        let response = send(
            &app,
            bearer("/api/petrol-prices", &format!("Bearer {token}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // No cookie is issued for token requests.
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let put = Request::put("/api/petrol-prices")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"from":"2026-03-01","cents_per_litre":199.0}"#,
            ))
            .unwrap();
        // The token is for reading: a write needs the session cookie.
        assert_eq!(send(&app, put).await.status(), StatusCode::UNAUTHORIZED);

        for value in [
            "Bearer wrong",
            "Bearer ",
            "Bearer",
            &format!("Bearer {token}x"),
            &format!("Bearer {}", &token[1..]),
            &format!("Basic {token}"),
            token,
        ] {
            let response = send(&app, bearer("/api/petrol-prices", value)).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{value}");
        }

        // Pages outside /api/ still want the login.
        let response = send(&app, bearer("/", &format!("Bearer {token}"))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/login");
    }

    #[tokio::test]
    async fn bearer_header_is_ignored_without_an_api_token() {
        let (_dir, app, _pool) = petrol_app(None).await;
        for value in [
            "Bearer ",
            "Bearer anything",
            "Bearer 0123456789abcdef0123456789abcdef",
        ] {
            let request = Request::get("/api/petrol-prices")
                .header(header::AUTHORIZATION, value)
                .body(Body::empty())
                .unwrap();
            assert_eq!(send(&app, request).await.status(), StatusCode::UNAUTHORIZED);
        }
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
    async fn icons_are_public() {
        let (_dir, app) = test_app().await;

        for (uri, content_type) in [
            ("/favicon.svg", "image/svg+xml"),
            ("/favicon.ico", "image/png"),
            ("/apple-touch-icon.png", "image/png"),
        ] {
            let response = send(&app, get_request(uri, None)).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                content_type,
                "{uri}"
            );
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "public, max-age=86400",
                "{uri}"
            );
        }

        let response = send(&app, get_request("/login", None)).await;
        let body = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains(r#"<link rel="icon" href="/favicon.svg""#));
        assert!(body.contains(r#"<link rel="apple-touch-icon" href="/apple-touch-icon.png">"#));
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
            serde_json::json!({
                "sample": null,
                "car": null,
                "grid_pull": {
                    "now_w": null, "peak_today_w": null, "peak_today_at": null,
                    "warn_w": 12000, "limit_w": 15000, "volts": null,
                }
            })
        );
    }

    #[tokio::test]
    async fn inverter_needs_a_session_and_is_null_without_data() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/inverter", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/inverter", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "settings": null })
        );
    }

    #[tokio::test]
    async fn inverter_history_needs_a_session_and_is_empty_without_data() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/inverter/history", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/inverter/history", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "changes": [], "previous": null })
        );
        let response = send(
            &app,
            get_request("/api/inverter/history?from=200&to=100", Some(&cookie)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn live_includes_the_car_once_teslamate_has_sent_something() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("test.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = hash_password(PASSWORD).unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let state = AppState::new(pool, PLANT, tz, &hash, Key::generate())
            .unwrap()
            .with_cookie_secure(true);
        let app = router(state.clone());
        let cookie = log_in(&app).await;

        // Even a very old car state is shown: TeslaMate's values are retained.
        state.car().set(CarState {
            battery_level_pct: Some(43.0),
            plugged_in: Some(true),
            updated_ts: db::now() - 86_400,
            ..CarState::default()
        });
        let response = send(&app, get_request("/api/live", Some(&cookie))).await;
        let json = body_json(response).await;
        assert!(json["sample"].is_null());
        assert_eq!(json["car"]["battery_level_pct"], 43.0);
        assert_eq!(json["car"]["plugged_in"], true);
        assert!(json["car"]["charger_power_kw"].is_null());
    }

    #[tokio::test]
    async fn ev_sessions_validates_input_and_lists_overlapping_sessions() {
        let (_dir, app, pool) = test_app_with_pool().await;
        let uri = "/api/ev/sessions?from=0&to=1";
        let response = send(&app, get_request(uri, None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let cookie = log_in(&app).await;
        let bad = [
            "/api/ev/sessions",
            "/api/ev/sessions?from=1",
            "/api/ev/sessions?from=x&to=2",
            "/api/ev/sessions?from=2&to=1",
            // 400 days and 1 second.
            "/api/ev/sessions?from=0&to=34560001",
        ];
        for uri in bad {
            let response = send(&app, get_request(uri, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert!(body_json(response).await["error"].is_string(), "{uri}");
        }

        // No sessions yet. An exact 400-day range is allowed.
        let uri = "/api/ev/sessions?from=0&to=34560000";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["sessions"], serde_json::json!([]));

        // One finished session, and one still running that began a minute ago.
        let progress = db::ChargeProgress {
            energy_added_kwh: Some(12.5),
            energy_used_kwh: Some(13.5),
            soc_pct: Some(70.0),
            max_power_kw: Some(7.4),
        };
        let id = db::start_charge_session(&pool, 1, 1_800_000_000, Some(40.0), Some("Home"))
            .await
            .unwrap();
        db::end_charge_session(&pool, id, 1_800_003_600, &progress)
            .await
            .unwrap();
        let running = db::now() - 60;
        db::start_charge_session(&pool, 1, running, None, None)
            .await
            .unwrap();

        let uri = "/api/ev/sessions?from=1800000000&to=1800000100";
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        let json = body_json(response).await;
        let sessions = json["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["started_at"], 1_800_000_000);
        assert_eq!(sessions[0]["ended_at"], 1_800_003_600);
        assert_eq!(sessions[0]["energy_added_kwh"], 12.5);
        assert_eq!(sessions[0]["energy_used_kwh"], 13.5);
        assert_eq!(sessions[0]["start_soc_pct"], 40.0);
        assert_eq!(sessions[0]["end_soc_pct"], 70.0);
        assert_eq!(sessions[0]["max_power_kw"], 7.4);
        assert_eq!(sessions[0]["geofence"], "Home");

        // The open session is found by a range around now.
        let uri = format!("/api/ev/sessions?from={}&to={}", running + 30, running + 40);
        let response = send(&app, get_request(&uri, Some(&cookie))).await;
        let json = body_json(response).await;
        let sessions = json["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0]["ended_at"].is_null());
    }

    #[tokio::test]
    async fn ev_sessions_are_costed_from_the_live_samples_at_home() {
        let (_dir, app, pool) = costs_app().await;
        let cookie = log_in(&app).await;

        // An hour of charging with 7.4 kWh from the wall, all from the grid.
        let start = 1_800_000_000;
        let progress = db::ChargeProgress {
            energy_used_kwh: Some(7.4),
            ..db::ChargeProgress::default()
        };
        let id = db::start_charge_session(&pool, 1, start, None, Some("Home"))
            .await
            .unwrap();
        db::end_charge_session(&pool, id, start + 3600, &progress)
            .await
            .unwrap();
        for ts in (start..=start + 3600).step_by(15) {
            let mut sample = live_sample(ts);
            (sample.pv_w, sample.load_w) = (Some(0.0), Some(8000.0));
            (sample.grid_w, sample.battery_w) = (Some(8000.0), Some(0.0));
            db::insert_live_sample(&pool, &sample).await.unwrap();
        }

        let uri = format!("/api/ev/sessions?from={start}&to={}", start + 3600);
        let response = send(&app, get_request(&uri, Some(&cookie))).await;
        let json = body_json(response).await;
        let session = &json["sessions"][0];
        assert_eq!(session["energy_used_kwh"], 7.4);
        assert_eq!(session["solar_kwh"], 0.0);
        assert_eq!(session["battery_kwh"], 0.0);
        assert_eq!(session["grid_kwh"], 7.4);
        // 7.4 kWh at 25 c.
        assert_eq!(session["cost"], 1.85);
        assert_eq!(session["solar_value"], 0.0);
        assert_eq!(session["coverage"], 1.0);

        // The same session away from home is not costed, and nor is a home
        // session when there is no tariff.
        sqlx::query("UPDATE ev_charge_session SET geofence = 'Work'")
            .execute(&pool)
            .await
            .unwrap();
        let response = send(&app, get_request(&uri, Some(&cookie))).await;
        let json = body_json(response).await;
        for key in ["solar_kwh", "grid_kwh", "cost", "solar_value", "coverage"] {
            assert!(json["sessions"][0][key].is_null(), "{key}");
        }
        sqlx::query("UPDATE ev_charge_session SET geofence = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let response = send(&app, get_request(&uri, Some(&cookie))).await;
        assert_eq!(body_json(response).await["sessions"][0]["cost"], 1.85);
        let no_tariff = {
            let hash = hash_password(PASSWORD).unwrap();
            let tz = chrono_tz::Australia::Adelaide;
            router(AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate()).unwrap())
        };
        let cookie = log_in(&no_tariff).await;
        let response = send(&no_tariff, get_request(&uri, Some(&cookie))).await;
        let json = body_json(response).await;
        assert!(json["sessions"][0]["cost"].is_null());
        assert_eq!(json["sessions"][0]["energy_used_kwh"], 7.4);
    }

    #[tokio::test]
    async fn ev_summary_compares_charging_with_petrol() {
        let (_dir, _app, pool) = costs_app().await;
        let router_with = |prices: EvPrices| {
            let hash = hash_password(PASSWORD).unwrap();
            let tz = chrono_tz::Australia::Adelaide;
            router(
                AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
                    .unwrap()
                    .with_tariff(Some(flat_tariff()))
                    .with_ev_prices(prices),
            )
        };
        let petrol = EvPrices {
            petrol_cents_per_litre: Some(195.0),
            petrol_litres_per_100km: 8.0,
            away_cents_per_kwh: Some(60.0),
        };
        let app = router_with(petrol);
        let uri = "/api/ev/summary?from=2026-09-01&to=2026-09-30";
        let response = send(&app, get_request(uri, None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let cookie = log_in(&app).await;
        let bad = [
            "/api/ev/summary?from=x",
            "/api/ev/summary?from=2026-09-02&to=2026-09-01",
            // 401 days.
            "/api/ev/summary?from=2025-01-01&to=2026-02-05",
        ];
        for uri in bad {
            let response = send(&app, get_request(uri, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert!(body_json(response).await["error"].is_string(), "{uri}");
        }
        // Nothing recorded yet. Without dates it covers the last 12 months.
        let response = send(&app, get_request("/api/ev/summary", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["months"], serde_json::json!([]));
        assert_eq!(json["total"]["km"], 0.0);

        // 100 km in September (the August reading is the starting point), a
        // home session of 7.4 kWh at 25 c, and 10 kWh away at 60 c.
        db::save_odometer(&pool, "2026-08-31", 1000.0, 1)
            .await
            .unwrap();
        db::save_odometer(&pool, "2026-09-20", 1100.0, 2)
            .await
            .unwrap();
        let start = SEP_15_START + 10 * 3600;
        for (offset, geofence, kwh) in [(0, "Home", 7.4), (86_400, "Servo", 10.0)] {
            let progress = db::ChargeProgress {
                energy_used_kwh: Some(kwh),
                ..db::ChargeProgress::default()
            };
            let id = db::start_charge_session(&pool, 1, start + offset, None, Some(geofence))
                .await
                .unwrap();
            db::end_charge_session(&pool, id, start + offset + 3600, &progress)
                .await
                .unwrap();
        }
        for ts in (start..=start + 3600).step_by(15) {
            let mut sample = live_sample(ts);
            (sample.pv_w, sample.load_w) = (Some(0.0), Some(8000.0));
            (sample.grid_w, sample.battery_w) = (Some(8000.0), Some(0.0));
            db::insert_live_sample(&pool, &sample).await.unwrap();
        }

        let response = send(&app, get_request(uri, Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["petrol_cents_per_litre"], 195.0);
        assert_eq!(json["petrol_litres_per_100km"], 8.0);
        let months = json["months"].as_array().unwrap();
        assert_eq!(months.len(), 1);
        assert_eq!(months[0]["month"], "2026-09");
        assert_eq!(months[0]["saved"], 7.75);
        let total = &json["total"];
        assert_eq!(total["km"], 100.0);
        assert_eq!(total["home_kwh"], 7.4);
        assert_eq!(total["home_cost"], 1.85);
        assert_eq!(total["away_kwh"], 10.0);
        assert_eq!(total["away_cost"], 6.0);
        assert_eq!(total["charging_cost"], 7.85);
        // 100 km at 8 L/100km is 8 L, at $1.95.
        assert_eq!(total["petrol_litres"], 8.0);
        assert_eq!(total["petrol_cost"], 15.6);
        assert_eq!(total["saved"], 7.75);
        assert_eq!(total["ev_cents_per_km"], 7.85);
        assert_eq!(total["petrol_cents_per_km"], 15.6);

        // Without an away price the away sessions are listed but not costed.
        let app = router_with(EvPrices {
            away_cents_per_kwh: None,
            ..petrol
        });
        let cookie = log_in(&app).await;
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["total"]["away_kwh"], 10.0);
        assert!(json["total"]["away_cost"].is_null());
        assert_eq!(json["total"]["charging_cost"], 1.85);

        // Without a petrol price the comparison is off.
        let app = router_with(EvPrices::default());
        let cookie = log_in(&app).await;
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["configured"], false);
        assert_eq!(json["total"]["km"], 100.0);
        for key in ["home_cost", "charging_cost", "petrol_cost", "saved"] {
            assert!(json["total"][key].is_null(), "{key}");
        }
    }

    #[tokio::test]
    async fn ev_cars_summarises_each_car_over_its_history() {
        let (_dir, _app, pool) = costs_app().await;
        let router_with = |petrol: PetrolHistory| {
            let hash = hash_password(PASSWORD).unwrap();
            let tz = chrono_tz::Australia::Adelaide;
            let prices = EvPrices {
                petrol_cents_per_litre: None,
                petrol_litres_per_100km: 8.0,
                away_cents_per_kwh: Some(60.0),
            };
            router(
                AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
                    .unwrap()
                    .with_ev_prices(prices)
                    .with_petrol_prices(petrol)
                    .with_petrol_comparison(
                        BTreeMap::from([("Y".to_string(), "Toyota RAV4".to_string())]),
                        "Unleaded 95",
                    ),
            )
        };
        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2026, 9, d).unwrap();
        let app = router_with(PetrolHistory::new(Some(190.0), vec![(day(10), 200.0)]));
        let response = send(&app, get_request("/api/ev/cars", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // The history sync has not run yet.
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["petrol_litres_per_100km"], 8.0);
        assert_eq!(json["cars"], serde_json::json!([]));

        let car = |id: i64, name: &str, model: Option<&str>| db::TmCar {
            id,
            name: Some(name.to_string()),
            model: model.map(str::to_string),
            trim_badging: (id == 1).then(|| "Long Range".to_string()),
            exterior_color: (id == 1).then(|| "PearlWhite".to_string()),
            ..db::TmCar::default()
        };
        db::upsert_tm_cars(
            &pool,
            &[
                car(1, "Daily", Some("3")),
                car(2, "Weekend", Some("Y")),
                car(3, "Unused", None),
                car(4, "Spare", Some("Y")),
            ],
            1,
        )
        .await
        .unwrap();
        let drive = |car_id: i64, date: &str, km: f64| db::TmDriveDay {
            car_id,
            date: date.to_string(),
            km,
            drives: 1,
        };
        db::upsert_tm_drive_days(
            &pool,
            &[
                drive(1, "2026-09-05", 100.0),
                drive(1, "2026-09-12", 50.0),
                drive(2, "2026-09-20", 10.0),
            ],
        )
        .await
        .unwrap();
        // Car 1: a costed session at home (7.4 kWh), one at home not costed
        // yet (5 kWh) and 10 kWh away. Car 4 only charged, with no geofence.
        let start = SEP_15_START + 10 * 3600;
        let charge =
            |id: i64, car_id: i64, at: i64, geofence: Option<&str>, kwh: f64| db::TmCharge {
                id,
                car_id,
                started_at: at,
                ended_at: Some(at + 3600),
                energy_added_kwh: Some(kwh - 0.5),
                energy_used_kwh: Some(kwh),
                start_soc_pct: None,
                end_soc_pct: None,
                geofence: geofence.map(str::to_string),
                fast_charger: false,
            };
        db::save_tm_charges(
            &pool,
            &[
                charge(1, 1, start, Some("Home"), 7.4),
                charge(2, 1, start + 86_400, Some("Home"), 5.0),
                charge(3, 1, start + 2 * 86_400, Some("Servo"), 10.0),
                charge(4, 4, start + 10 * 86_400, None, 4.0),
            ],
        )
        .await
        .unwrap();
        let cost = db::TmChargeCost {
            cost_key: "k".to_string(),
            grid_kwh: 7.4,
            self_kwh: 0.0,
            uncovered_kwh: 0.0,
            cost_cents: 185.0,
            baseline_cents: 370.0,
            feed_in_forgone_cents: 0.0,
        };
        db::save_tm_charge_cost(&pool, 1, &cost).await.unwrap();

        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        let json = body_json(response).await;
        let cars = json["cars"].as_array().unwrap();
        // Newest first; the car with nothing is left out; the two Model Y get
        // their names added.
        let ids: Vec<_> = cars.iter().map(|c| c["car_id"].as_i64().unwrap()).collect();
        assert_eq!(ids, [4, 2, 1]);
        assert!(cars.iter().all(|c| c["source"] == "teslamate"));
        assert_eq!(cars[0]["label"], "Model Y (Spare)");
        assert_eq!(cars[1]["label"], "Model Y (Weekend)");
        assert_eq!(cars[2]["label"], "Model 3");
        assert_eq!(cars[2]["name"], "Daily");
        assert_eq!(cars[2]["first_day"], "2026-09-05");
        assert_eq!(cars[2]["last_day"], "2026-09-17");

        // 100 km at $1.90 and 50 km at $2.00 (the price changed on the 10th).
        let total = &cars[2]["total"];
        assert_eq!(total["km"], 150.0);
        assert_eq!(total["petrol_litres"], 12.0);
        assert_eq!(total["petrol_cost"], 23.2);
        assert_eq!(total["home_kwh"], 12.4);
        assert_eq!(total["away_kwh"], 10.0);
        assert_eq!(total["away_cost"], 6.0);
        assert_eq!(total["pending_sessions"], 1);
        assert_eq!(total["with_system"]["charging_cost"], 7.85);
        assert_eq!(total["with_system"]["saved"], 15.35);
        assert_eq!(total["with_system"]["cents_per_km"], 5.23);
        assert_eq!(total["without_system"]["charging_cost"], 9.7);
        assert_eq!(total["without_system"]["saved"], 13.5);
        assert_eq!(total["system_added"], 1.85);
        assert_eq!(total["grid_kwh"], 7.4);

        // Car details and the comparison car (only the Model Y has one).
        assert_eq!(cars[2]["trim"], "Long Range");
        assert_eq!(cars[2]["paint"], "PearlWhite");
        assert!(cars[2]["wheels"].is_null());
        assert!(cars[2]["marketing_name"].is_null());
        assert!(cars[2]["compare_with"].is_null());
        assert_eq!(cars[1]["compare_with"], "Toyota RAV4");
        assert!(cars[1]["trim"].is_null());
        assert_eq!(json["petrol_grade"], "Unleaded 95");

        // Without a [tariff] there is one plan. Car 1's costed session is in
        // the money, its other home session is only pending; the away session
        // is costed at 60 c/kWh. Nothing was solar, so the share is 0.
        let plans = cars[2]["charging_by_plan"].as_array().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0]["plan"], "Home charging");
        assert_eq!(plans[0]["sessions"], 1);
        assert_eq!(plans[0]["kwh"], 7.4);
        assert_eq!(plans[0]["cost"], 1.85);
        assert_eq!(plans[0]["pending_sessions"], 1);
        assert_eq!(cars[2]["away"]["sessions"], 1);
        assert_eq!(cars[2]["away"]["kwh"], 10.0);
        assert_eq!(cars[2]["away"]["cost"], 6.0);
        assert_eq!(cars[2]["self_share_pct"], 0.0);
        assert!(cars[0]["self_share_pct"].is_null());
        // The combined figures cover every car: car 4's away session too.
        assert_eq!(json["away"]["sessions"], 2);
        assert_eq!(json["away"]["kwh"], 14.0);
        assert_eq!(json["charging_by_plan"][0]["cost"], 1.85);
        assert_eq!(json["self_share_pct"], 0.0);
        let months = cars[2]["months"].as_array().unwrap();
        assert_eq!(months.len(), 1);
        assert_eq!(months[0]["month"], "2026-09");
        assert_eq!(months[0]["system_added"], 1.85);

        // The only km of car 2 are on a day with a price; car 4's session is
        // away from home (no geofence), costed at the away price.
        assert_eq!(cars[1]["total"]["petrol_cost"], 1.6);
        assert_eq!(cars[0]["total"]["away_kwh"], 4.0);
        assert_eq!(cars[0]["total"]["with_system"]["charging_cost"], 2.4);
        assert_eq!(
            cars[0]["total"]["with_system"]["cents_per_km"],
            serde_json::Value::Null
        );

        // With no petrol price at all the savings are null, the charging
        // costs stay.
        let app = router_with(PetrolHistory::default());
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["configured"], false);
        let total = &json["cars"][2]["total"];
        assert!(total["petrol_cost"].is_null());
        assert!(total["with_system"]["saved"].is_null());
        assert_eq!(total["with_system"]["charging_cost"], 7.85);
        assert_eq!(total["system_added"], 1.85);
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

    #[tokio::test]
    async fn security_headers_on_pages_and_api() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;
        for (uri, no_store) in [("/login", false), ("/api/latest", true)] {
            let response = send(&app, get_request(uri, Some(&cookie))).await;
            let h = response.headers();
            assert!(
                h[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("frame-ancestors 'none'")
            );
            assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert_eq!(h[header::REFERRER_POLICY], "same-origin");
            assert_eq!(h["x-frame-options"], "DENY");
            assert_eq!(
                h.get(header::CACHE_CONTROL)
                    .is_some_and(|v| v == "no-store"),
                no_store
            );
        }
        // Errors get them too.
        let response = send(&app, get_request("/api/latest", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn responses_are_gzipped_when_accepted() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;
        let request = Request::get("/static/app.js")
            .header(header::COOKIE, &cookie)
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(Body::empty())
            .unwrap();
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    }

    #[tokio::test]
    async fn classic_page_links_versioned_assets() {
        let (_dir, app) = test_app().await;

        let response = send(&app, get_request("/classic", None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/login");

        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/classic", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        let etag = response.headers()[header::ETAG].clone();
        let html = body_text(response).await;
        for name in ["app.js", "style.css"] {
            let version = &hex::encode(Assets::get(name).unwrap().metadata.sha256_hash())[..12];
            assert!(
                html.contains(&format!("\"/static/{name}?v={version}\"")),
                "{name}"
            );
        }

        // The versioned link still serves the file.
        let response = send(&app, get_request("/static/app.js?v=abc", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);

        let request = Request::get("/classic")
            .header(header::COOKIE, &cookie)
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, request).await.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn index_serves_new_dashboard_with_versioned_assets() {
        let (_dir, app) = test_app().await;

        let response = send(&app, get_request("/", None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/login");

        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        let etag = response.headers()[header::ETAG].clone();
        let html = body_text(response).await;
        for name in [
            "next/next.css",
            "next/core.js",
            "next/today.js",
            "next/admin.js",
        ] {
            let version = &hex::encode(Assets::get(name).unwrap().metadata.sha256_hash())[..12];
            assert!(
                html.contains(&format!("\"/static/{name}?v={version}\"")),
                "{name}"
            );
        }

        let request = Request::get("/")
            .header(header::COOKIE, &cookie)
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, request).await.status(), StatusCode::NOT_MODIFIED);

        // The font is a static file like the rest, under /static.
        let font = "/static/next/fonts/archivo-latin-wdth-wght.woff2";
        let response = send(&app, get_request(font, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn next_redirects_to_the_root() {
        let (_dir, app) = test_app().await;

        // Logged out, the login comes first, like every page.
        let response = send(&app, get_request("/next", None)).await;
        assert_eq!(response.headers()[header::LOCATION], "/login");

        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/next", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/");
    }

    #[tokio::test]
    async fn static_files_use_etags() {
        let (_dir, app) = test_app().await;
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/static/app.js", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        let etag = response.headers()[header::ETAG].clone();

        let request = Request::get("/static/app.js")
            .header(header::COOKIE, &cookie)
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], etag);
        assert!(body_text(response).await.is_empty());
    }

    /// A simple tariff: 100 c supply, `import_cents` import, 5 c export, no windows.
    fn flat_tariff_with(import_cents: f64) -> TariffSchedule {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let text = format!(
            "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = {import_cents}\n\
             default_export_cents = 5.0\n"
        );
        let w: Wrapper = toml::from_str(&text).unwrap();
        TariffSchedule::from_config(&w.tariff, &[]).unwrap()
    }

    /// The flat tariff at 25 c import.
    fn flat_tariff() -> TariffSchedule {
        flat_tariff_with(25.0)
    }

    /// A router over `pool` with this tariff.
    fn costs_router(pool: &SqlitePool, tariff: TariffSchedule) -> Router {
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
        for point_id in [83102, 83072, 83118, 83323] {
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
    async fn current_bill_is_off_without_a_tariff() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/bill/current", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/bill/current", Some(&cookie))).await;
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "configured": false })
        );
    }

    #[tokio::test]
    async fn current_bill_has_a_period_and_a_projection() {
        let (_dir, app, _pool) = costs_app().await;
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/bill/current", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["period_source"], "month");
        assert!(json["days_total"].as_i64().unwrap() >= 28);
        let bands = json["so_far"]["bands"].as_array().unwrap();
        let names: Vec<_> = bands.iter().map(|b| b["band"].as_str().unwrap()).collect();
        assert_eq!(names, ["free", "shoulder", "peak"]);
        assert_eq!(json["uses_globird_figures"], false);
        assert!(json["projected_total_dollars"].is_number());
    }

    #[tokio::test]
    async fn tariff_needs_a_session_and_a_tariff() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/tariff", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/tariff", Some(&cookie))).await;
        assert_eq!(
            body_json(response).await,
            serde_json::json!({ "configured": false })
        );

        let (_dir, app, _pool) = costs_app().await;
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/tariff?date=bad", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let uri = "/api/tariff?date=2026-10-20";
        let json = body_json(send(&app, get_request(uri, Some(&cookie))).await).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["default_import_cents"], 25.0);
        assert_eq!(json["supply_cents_per_day"], 100.0);
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
                "supply": 1.0,
                "import": 0.25,
                "feed_in": 0.1,
            })
        );
        // Not bought: baseline 200 - supply 100 - import 25 = 75 c, and
        // 75 + 10 feed-in + 0 credit = the 85 c saved.
        assert_eq!(
            json["totals"],
            serde_json::json!({
                "days": 1,
                "credit_days": 0,
                "billed_days": 0,
                "supply": 1.0,
                "import": 0.25,
                "import_kwh": 1.0,
                "feed_in": 0.1,
                "export_kwh": 2.0,
                "load_kwh": 4.0,
                "credit": 0.0,
                "actual": 1.15,
                "baseline": 2.0,
                "savings": 0.85,
                "not_bought": 0.75,
            })
        );
        assert_eq!(json["group"], "day");
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

    #[tokio::test]
    async fn house_intervals_cover_the_days_a_time_touches() {
        let (_dir, _app, pool) = costs_app().await;
        let tz = chrono_tz::Australia::Adelaide;
        // The 15th has counters all day; the 16th has none.
        seed_counters(&pool, SEP_15_START, &[3600, LAST_READING], 1000.0).await;
        let now = SEP_15_START + 3 * 86_400;
        let (start, end) = (SEP_15_START + 3 * 3600, SEP_15_START + 5 * 3600);
        let (intervals, covered) = costs::house_intervals(&pool, PLANT, tz, start, end, now)
            .await
            .unwrap();
        assert!(!intervals.is_empty());
        // The whole day is the source, so these are the day's intervals.
        assert!(intervals.iter().any(|i| i.start_ts < start));
        assert!(intervals.windows(2).all(|w| w[0].end_ts <= w[1].start_ts));
        assert!((0.0..=1.0).contains(&covered));

        // A session across midnight loads both days; the empty one adds none.
        let night = (SEP_15_START + 86_400 - 1800, SEP_15_START + 86_400 + 1800);
        let (_, covered) = costs::house_intervals(&pool, PLANT, tz, night.0, night.1, now)
            .await
            .unwrap();
        assert!(covered < 0.6, "the 16th has no data, got {covered}");

        // Nothing stored, or no length: nothing covered.
        let later = SEP_15_START + 10 * 86_400;
        let (intervals, covered) =
            costs::house_intervals(&pool, PLANT, tz, later, later + 3600, now)
                .await
                .unwrap();
        assert!(intervals.is_empty());
        assert_eq!(covered, 0.0);
        let (intervals, covered) = costs::house_intervals(&pool, PLANT, tz, start, start, now)
            .await
            .unwrap();
        assert!(intervals.is_empty());
        assert_eq!(covered, 0.0);
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
    async fn costs_show_the_cars_fuel_saving_per_bucket_and_in_total() {
        let (_dir, plain, pool) = costs_app().await;
        let tz = chrono_tz::Australia::Adelaide;
        for date in ["2026-09-15", "2026-09-16"] {
            let (day_start, _) = api::day_bounds_or_error(date.parse().unwrap(), tz).unwrap();
            seed_counters(&pool, day_start, &[3600, LAST_READING], 1000.0).await;
        }
        let range = "from=2026-09-01&to=2026-09-30";

        // No TeslaMate cars: nothing about cars, and the rest as before.
        let before = costs_json(&plain, range).await;
        assert_eq!(before["cars_available"], false);
        assert!(before["totals"].get("cars").is_none());
        assert!(before["days"][0].get("cars").is_none());
        let before_month = costs_json(&plain, &format!("{range}&group=month")).await;
        assert_eq!(before_month["cars_available"], false);
        assert!(before_month["buckets"][0].get("cars").is_none());

        let hash = hash_password(PASSWORD).unwrap();
        let router_with = |petrol: PetrolHistory| {
            let prices = EvPrices {
                petrol_cents_per_litre: None,
                petrol_litres_per_100km: 8.0,
                away_cents_per_kwh: Some(60.0),
            };
            router(
                AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
                    .unwrap()
                    .with_tariff(Some(flat_tariff()))
                    .with_ev_prices(prices)
                    .with_petrol_prices(petrol),
            )
        };
        // Cars but no petrol price is not enough either.
        let car = db::TmCar {
            id: 1,
            name: Some("Daily".to_string()),
            model: Some("3".to_string()),
            ..db::TmCar::default()
        };
        db::upsert_tm_cars(&pool, &[car], 1).await.unwrap();
        let app = router_with(PetrolHistory::new(None, vec![]));
        let json = costs_json(&app, range).await;
        assert_eq!(json["cars_available"], false);
        assert!(json["totals"].get("cars").is_none());

        // 100 km on the 15th and 50 km on the 17th (no house data that day).
        // At 8 L/100 km and $2.00 that is $16.00 and $8.00 of petrol.
        let drive = |date: &str, km: f64| db::TmDriveDay {
            car_id: 1,
            date: date.to_string(),
            km,
            drives: 1,
        };
        db::upsert_tm_drive_days(
            &pool,
            &[drive("2026-09-15", 100.0), drive("2026-09-17", 50.0)],
        )
        .await
        .unwrap();
        let start = SEP_15_START + 10 * 3600;
        let charge = |id: i64, at: i64, geofence: &str, kwh: f64| db::TmCharge {
            id,
            car_id: 1,
            started_at: at,
            ended_at: Some(at + 3600),
            energy_added_kwh: Some(kwh),
            energy_used_kwh: Some(kwh),
            start_soc_pct: None,
            end_soc_pct: None,
            geofence: Some(geofence.to_string()),
            fast_charger: false,
        };
        // A costed home session (baseline $3.70) on the 15th; on the 16th a
        // home session not costed yet (5 kWh at the plan's 25 c average) and
        // 10 kWh away at 60 c.
        db::save_tm_charges(
            &pool,
            &[
                charge(1, start, "Home", 7.4),
                charge(2, start + 86_400, "Home", 5.0),
                charge(3, start + 86_400 + 7200, "Servo", 10.0),
            ],
        )
        .await
        .unwrap();
        let cost = db::TmChargeCost {
            cost_key: "k".to_string(),
            grid_kwh: 7.4,
            self_kwh: 0.0,
            uncovered_kwh: 0.0,
            cost_cents: 185.0,
            baseline_cents: 370.0,
            feed_in_forgone_cents: 0.0,
        };
        db::save_tm_charge_cost(&pool, 1, &cost).await.unwrap();

        let app = router_with(PetrolHistory::new(Some(200.0), vec![]));
        let json = costs_json(&app, range).await;
        assert_eq!(json["cars_available"], true);
        // Day by day, only the days with house data have rows.
        assert_eq!(json["days"].as_array().unwrap().len(), 2);
        assert_eq!(
            json["days"][0]["cars"],
            serde_json::json!({
                "km": 100.0, "petrol": 16.0, "charging": 3.7, "fuel_saved": 12.3,
                "estimated_sessions": 0
            })
        );
        // No driving on the 16th: the charging is a cost with nothing to offset it.
        assert_eq!(
            json["days"][1]["cars"],
            serde_json::json!({
                "km": 0.0, "petrol": 0.0, "charging": 7.25, "fuel_saved": -7.25,
                "estimated_sessions": 1
            })
        );
        // The total is the two rows added up; the 17th has no house data, so
        // its 50 km are left out.
        assert_eq!(
            json["totals"]["cars"],
            serde_json::json!({
                "km": 100.0, "petrol": 16.0, "charging": 10.95, "fuel_saved": 5.05,
                "estimated_sessions": 1
            })
        );
        // The house's own figures are the same as without cars.
        assert_eq!(json["totals"]["savings"], before["totals"]["savings"]);

        let months = costs_json(&app, &format!("{range}&group=month")).await;
        assert_eq!(months["cars_available"], true);
        assert_eq!(months["buckets"][0]["cars"], months["totals"]["cars"]);
        let years = costs_json(&app, "from=2026-01-01&to=2026-12-31&group=year").await;
        assert_eq!(years["buckets"][0]["cars"]["fuel_saved"], 5.05);

        // A period with house data but no car activity gets zeros.
        let (day_start, _) = api::day_bounds_or_error("2026-08-01".parse().unwrap(), tz).unwrap();
        seed_counters(&pool, day_start, &[3600, LAST_READING], 1000.0).await;
        let august = costs_json(&app, "from=2026-08-01&to=2026-08-31&group=month").await;
        assert_eq!(
            august["buckets"][0]["cars"],
            serde_json::json!({
                "km": 0.0, "petrol": 0.0, "charging": 0.0, "fuel_saved": 0.0,
                "estimated_sessions": 0
            })
        );
    }

    #[tokio::test]
    async fn costs_can_be_grouped_by_month_and_year() {
        let (_dir, app, pool) = costs_app().await;
        // The same usage on 31 Dec 2025, then 15 Sep, 16 Sep and 1 Oct 2026.
        let tz = chrono_tz::Australia::Adelaide;
        for date in ["2025-12-31", "2026-09-15", "2026-09-16", "2026-10-01"] {
            let (day_start, _) = api::day_bounds_or_error(date.parse().unwrap(), tz).unwrap();
            seed_counters(&pool, day_start, &[3600, LAST_READING], 1000.0).await;
        }
        let months = costs_json(&app, "from=2025-01-01&to=2026-12-31&group=month").await;
        assert_eq!(months["group"], "month");
        assert!(months.get("days").is_none());
        let periods: Vec<&str> = months["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["period"].as_str().unwrap())
            .collect();
        assert_eq!(periods, ["2025-12", "2026-09", "2026-10"]);
        let sep = &months["buckets"][1];
        assert_eq!(sep["days"], 2);
        assert_eq!(sep["credit_days"], 0);
        // Two days of the cloud day above: actual 2 * 1.15, saved 2 * 0.85.
        assert_eq!(sep["actual"], 2.3);
        assert_eq!(sep["savings"], 1.7);
        assert_eq!(sep["supply"], 2.0);
        assert_eq!(sep["feed_in"], 0.2);
        assert_eq!(sep["not_bought"], 1.5);
        assert_eq!(months["totals"]["days"], 4);

        let years = costs_json(&app, "from=2025-01-01&to=2026-12-31&group=year").await;
        let summary: Vec<(String, i64)> = years["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                (
                    b["period"].as_str().unwrap().to_string(),
                    b["days"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(summary, [("2025".to_string(), 1), ("2026".to_string(), 3)]);
        // Finished days were stored by the first request and reused.
        assert_eq!(stored_days(&pool).await, 4);
    }

    #[tokio::test]
    async fn grouped_costs_allow_a_longer_range_but_check_the_group() {
        let (_dir, app, _pool) = costs_app().await;
        let cookie = log_in(&app).await;
        let get = |uri: &'static str| send(&app, get_request(uri, Some(&cookie)));
        // 20 years is fine when grouped (and an empty database has no buckets).
        let response = get("/api/costs?from=2007-01-01&to=2026-12-31&group=year").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["buckets"], serde_json::json!([]));
        // Longer is refused, as are an unknown group and a long day range.
        for uri in [
            "/api/costs?from=2006-01-01&to=2026-12-31&group=month",
            "/api/costs?group=week",
            "/api/costs?from=2022-01-01&to=2026-02-09&group=day",
        ] {
            let response = get(uri).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    /// The flat tariff at 35 c import from 2026-09-16, after an earlier plan
    /// at `old_import_cents` that ran until 2026-09-15 (inclusive).
    fn two_plans(old_import_cents: f64) -> TariffSchedule {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
            previous_tariff: Vec<crate::config::PreviousTariff>,
        }
        let text = format!(
            "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 35.0\n\
             default_export_cents = 5.0\n\
             [[previous_tariff]]\nuntil = \"2026-09-15\"\nsupply_cents_per_day = 100.0\n\
             default_import_cents = {old_import_cents}\ndefault_export_cents = 5.0\n"
        );
        let w: Wrapper = toml::from_str(&text).unwrap();
        TariffSchedule::from_config(&w.tariff, &w.previous_tariff).unwrap()
    }

    #[tokio::test]
    async fn days_are_priced_on_the_plan_in_effect_then() {
        let (_dir, _app, pool) = costs_app().await;
        let app = costs_router(&pool, two_plans(25.0));
        // The same usage on two days either side of the plan change.
        seed_counters(&pool, SEP_15_START, &[3600, LAST_READING], 1000.0).await;
        seed_counters(&pool, SEP_15_START + 86_400, &[3600, LAST_READING], 1000.0).await;
        let range = "from=2026-09-15&to=2026-09-16";
        let json = costs_json(&app, range).await;
        // 100 supply + import - 10 export credit: 25 c up to the 15th, 35 c after.
        assert_eq!(json["days"][0]["actual"], 1.15);
        assert_eq!(json["days"][1]["actual"], 1.25);
        // The baseline (load 4 kWh) uses each day's plan too: 100 + 4 * rate.
        assert_eq!(json["days"][0]["baseline"], 2.0);
        assert_eq!(json["days"][1]["baseline"], 2.4);
        assert_eq!(stored_days(&pool).await, 2);

        // Editing the old plan re-prices only its own days.
        let edited = costs_router(&pool, two_plans(30.0));
        let json = costs_json(&edited, range).await;
        assert_eq!(json["days"][0]["actual"], 1.2);
        assert_eq!(json["days"][1]["actual"], 1.25);
        assert_eq!(stored_days(&pool).await, 2);
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
            battery_kwh: None,
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

    /// Saves the two fixture GloBird days (2026-10-09 and 2026-10-17) the way the sync does.
    async fn seed_globird(pool: &SqlitePool) {
        use crate::globird::data::{group_days, parse_cost_rows, parse_usage_rows};
        let data_of = |text: &str| -> Value {
            let mut reply: Value = serde_json::from_str(text).unwrap();
            reply["data"].take()
        };
        let cost = parse_cost_rows(data_of(include_str!(
            "../globird/testdata/cost_detail.json"
        )))
        .unwrap();
        let usage = parse_usage_rows(data_of(include_str!(
            "../globird/testdata/smart_meter_read.json"
        )))
        .unwrap();
        for day in group_days(&cost, &usage) {
            db::save_globird_day(pool, &day, 1000).await.unwrap();
        }
    }

    /// `/api/globird/compare` for a range, logging in on `app` first.
    async fn compare_json(app: &Router, range: &str) -> Value {
        let cookie = log_in(app).await;
        let uri = format!("/api/globird/compare?{range}");
        let response = send(app, get_request(&uri, Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    }

    #[tokio::test]
    async fn globird_compare_needs_a_session() {
        let (_dir, app) = test_app().await;
        let response = send(&app, get_request("/api/globird/compare", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn globird_compare_is_empty_without_data() {
        let (_dir, app) = test_app().await;
        let json = compare_json(&app, "").await;
        assert_eq!(json["configured"], false);
        assert_eq!(json["days"], serde_json::json!([]));
        assert_eq!(json["categories"], serde_json::json!([]));
        assert_eq!(json["totals"]["days_compared"], 0);

        for bad in [
            "from=2026-10-09&to=2026-10-01",
            "from=2025-01-01&to=2026-10-01",
        ] {
            let cookie = log_in(&app).await;
            let uri = format!("/api/globird/compare?{bad}");
            let response = send(&app, get_request(&uri, Some(&cookie))).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[tokio::test]
    async fn globird_compare_lines_up_with_our_estimate() {
        let (_dir, app, pool) = costs_app().await;
        seed_globird(&pool).await;
        // Our counters for the 9th only: 1 kWh in, 2 kWh out.
        let tz = chrono_tz::Australia::Adelaide;
        let oct_9 = chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        let start = api::day_bounds_or_error(oct_9, tz).unwrap().0;
        seed_counters(&pool, start, &[3600, LAST_READING], 1000.0).await;

        let json = compare_json(&app, "from=2026-10-01&to=2026-10-20").await;
        assert_eq!(json["configured"], false);
        let days = json["days"].as_array().unwrap();
        assert_eq!(days.len(), 2);

        let day = &days[0];
        assert_eq!(day["date"], "2026-10-09");
        assert_eq!(day["complete"], true);
        let g = &day["globird"];
        assert_eq!(g["import"], 0.75);
        assert_eq!(g["export"], -1.85);
        assert_eq!(g["supply"], 1.74);
        assert_eq!(g["credit"], -1.0);
        assert_eq!(g["other"], 0.0);
        assert_eq!(g["total"], -0.36);
        assert_eq!(g["lines"].as_array().unwrap().len(), 6);
        assert!(g["import_kwh"].as_f64().unwrap() > 20.0);
        assert!(g["export_kwh"].as_f64().unwrap() > 19.0);
        // 100 c supply + 25 c import - 10 c feed-in.
        let ours = &day["ours"];
        assert_eq!(ours["source"], "cloud");
        assert_eq!(ours["import"], 0.25);
        assert_eq!(ours["export"], -0.1);
        assert_eq!(ours["supply"], 1.0);
        assert_eq!(ours["credit"], 0.0);
        assert_eq!(ours["total"], 1.15);
        assert_eq!(ours["import_kwh"], 1.0);
        assert_eq!(day["diff"]["total"], 1.51);

        // The 17th has GloBird rows but no estimate of ours.
        assert_eq!(days[1]["date"], "2026-10-17");
        assert!(days[1]["ours"].is_null());
        assert!(days[1]["diff"].is_null());

        assert_eq!(json["totals"]["days_compared"], 1);
        assert_eq!(json["totals"]["diff"]["total"], 1.51);
        assert_eq!(json["totals"]["ours"]["total"], 1.15);
        let categories = json["categories"].as_array().unwrap();
        let peak = categories
            .iter()
            .find(|c| c["category"] == "Critical Peak - Export")
            .unwrap();
        assert_eq!(peak["amount"], -0.35);
        assert_eq!(peak["bucket"], "export");

        // A range with neither day is empty.
        let json = compare_json(&app, "from=2026-10-10&to=2026-10-16").await;
        assert_eq!(json["days"], serde_json::json!([]));
    }

    /// A pool with the 9 October cloud day (our estimate: 115 c actual, 200 c
    /// baseline) and GloBird's bill for it (-36 c) seeded.
    async fn billed_pool() -> (tempfile::TempDir, SqlitePool) {
        let (dir, _app, pool) = costs_app().await;
        seed_globird(&pool).await;
        let tz = chrono_tz::Australia::Adelaide;
        let oct_9 = chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        let start = api::day_bounds_or_error(oct_9, tz).unwrap().0;
        seed_counters(&pool, start, &[3600, LAST_READING], 1000.0).await;
        (dir, pool)
    }

    #[tokio::test]
    async fn costs_use_globird_where_it_has_billed_but_the_cache_keeps_our_estimate() {
        let (_dir, pool) = billed_pool().await;
        let app = costs_router(&pool, flat_tariff());
        let json = costs_json(&app, "from=2026-10-08&to=2026-10-10").await;

        // Only the 9th has data of ours. GloBird's figures replace the money.
        let days = json["days"].as_array().unwrap();
        assert_eq!(days.len(), 1);
        let day = &days[0];
        assert_eq!(day["source"], "globird");
        assert_eq!(day["import"], 0.75);
        assert_eq!(day["feed_in"], 1.85);
        assert_eq!(day["supply"], 1.74);
        assert_eq!(day["credit"], 1.0);
        assert_eq!(day["actual"], -0.36);
        // Our baseline (100 + 4 kWh * 25 c) and load stay; savings are baseline - bill.
        assert_eq!(day["baseline"], 2.0);
        assert_eq!(day["load_kwh"], 4.0);
        assert_eq!(day["savings"], 2.36);
        assert!(day["import_kwh"].as_f64().unwrap() > 20.0);
        assert_eq!(json["totals"]["billed_days"], 1);
        assert_eq!(json["totals"]["credit_days"], 1);

        let month = costs_json(&app, "from=2026-10-01&to=2026-10-31&group=month").await;
        assert_eq!(month["buckets"][0]["billed_days"], 1);

        // The stored estimate is still ours.
        let stored: (String, f64) = sqlx::query_as("SELECT source, actual_cents FROM daily_cost")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, ("cloud".to_string(), 115.0));

        // The Bill check still compares GloBird with our raw estimate.
        let compare = compare_json(&app, "from=2026-10-09&to=2026-10-09").await;
        let day = &compare["days"][0];
        assert_eq!(day["ours"]["source"], "cloud");
        assert_eq!(day["ours"]["total"], 1.15);
        assert_eq!(day["diff"]["total"], 1.51);
    }

    #[tokio::test]
    async fn payback_counts_the_billed_savings() {
        let (_dir, pool) = billed_pool().await;
        let app = payback_router(&pool, vec![solar_purchase("2026-10-01", 10.0)]);
        let json = payback_json(&app).await;
        // Baseline 2.00 less GloBird's -0.36, not our 0.85.
        assert_eq!(json["saved"], 2.36);
        assert_eq!(json["net"], -7.64);
    }

    /// A router with the flat tariff, these purchases and the past bills in `csv`.
    fn bills_router(pool: &SqlitePool, csv: &str, purchases: Vec<Purchase>) -> Router {
        let hash = hash_password(PASSWORD).unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let state = AppState::new(pool.clone(), PLANT, tz, &hash, Key::generate())
            .unwrap()
            .with_tariff(Some(flat_tariff()))
            .with_purchases(purchases)
            .with_bills(crate::bills::parse(csv).unwrap());
        router(state)
    }

    const BILLS_HEADER: &str = "from,to,bill_total,excluded,import_kwh,import_dollars,\
        export_kwh,export_dollars,supply_dollars,note\n";

    #[tokio::test]
    async fn payback_counts_a_past_bill_day_with_no_data() {
        let (_dir, _app, pool) = costs_app().await;
        // Two days, no data at all. First pass per day: 100 c supply + 400 c
        // import (10 kWh at 40 c) = 500 c; the bill says 6.00, so 300 c a day.
        // The load falls back to the 10 kWh bought, and with no data it is
        // priced on the flat plan: 100 c + 10 kWh at 25 c = 350 c, so each
        // day saves 50 c.
        let csv = format!("{BILLS_HEADER}2026-10-01,2026-10-02,6.00,0,20,8.00,0,0,2.00,\n");
        let app = bills_router(&pool, &csv, vec![solar_purchase("2026-10-01", 10.0)]);
        let json = payback_json(&app).await;
        assert_eq!(json["saved"], 1.0);
        assert_eq!(json["net"], -9.0);
    }

    #[tokio::test]
    async fn a_range_cutting_a_past_bill_gets_the_whole_periods_smoothing() {
        let (_dir, _app, pool) = costs_app().await;
        // Ten days: first pass 1200 + 4000 = 5200 c against 5000 c billed, so
        // every day is 20 c cheaper: 120 + 400 - 20 = 500 c.
        let csv = format!("{BILLS_HEADER}2026-10-01,2026-10-10,50.00,0,100,40.00,0,0,12.00,\n");
        let app = bills_router(&pool, &csv, Vec::new());

        let half = costs_json(&app, "from=2026-10-01&to=2026-10-05").await;
        let days = half["days"].as_array().unwrap();
        assert_eq!(days.len(), 5);
        assert_eq!(days[0]["source"], "bill");
        assert_eq!(days[0]["actual"], 5.0);
        assert_eq!(half["totals"]["actual"], 25.0);
        assert_eq!(half["totals"]["billed_days"], 5);

        // The whole period adds up to the bill, and days outside it stay out.
        let all = costs_json(&app, "from=2026-09-30&to=2026-10-11").await;
        assert_eq!(all["days"].as_array().unwrap().len(), 10);
        assert_eq!(all["totals"]["actual"], 50.0);
    }

    #[tokio::test]
    async fn globird_compare_without_a_tariff_has_no_ours() {
        let (_dir, app, pool) = test_app_with_pool().await;
        seed_globird(&pool).await;
        let json = compare_json(&app, "from=2026-10-09&to=2026-10-09").await;
        let days = json["days"].as_array().unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0]["globird"]["total"], -0.36);
        assert!(days[0]["ours"].is_null());
        assert!(days[0]["diff"].is_null());
        assert_eq!(json["totals"]["days_compared"], 0);
        assert_eq!(json["totals"]["globird"]["total"], 0.0);
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
        assert_eq!(json["last_12_months_saved"], 0.85);
        // No battery_kwh anywhere: everything stays with the first purchase.
        assert_eq!(json["without"], Value::Null);
        assert_eq!(json["parts"][0]["saved"], 0.85);
        assert_eq!(json["parts"][0]["estimated"], false);
        assert_eq!(json["parts"][0]["net"], -9.15);
        assert_eq!(json["parts"][0]["recovered_pct"], 8.5);
        assert_eq!(json["parts"][1]["saved"], Value::Null);
        assert_eq!(json["parts"][1]["net"], Value::Null);
        // Purchases only: a flat plan with no name adds nothing.
        assert_eq!(json["timeline"].as_array().unwrap().len(), 2);
        assert_eq!(json["timeline"][1]["kind"], "purchase");
        assert_eq!(json["timeline"][1]["label"], "Solar");
        assert_eq!(json["timeline"][1]["cost"], 5.0);
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

    #[tokio::test]
    async fn payback_estimates_what_a_later_battery_added() {
        let (_dir, _app, pool) = costs_app().await;
        // One finished day saving 85 c (see `costs_for_a_cloud_day`), with
        // 11.5 kWh discharged.
        seed_counters(&pool, SEP_15_START, &[3600, LAST_READING], 1000.0).await;
        let discharge = [3600, LAST_READING].map(|offset| Reading {
            point_id: 83323,
            ts: SEP_15_START + offset,
            value: Some(11_500.0),
        });
        db::upsert_readings(&pool, PLANT, &discharge).await.unwrap();
        let with_battery = |date: &str, cost: f64, kwh: f64| Purchase {
            battery_kwh: Some(kwh),
            ..solar_purchase(date, cost)
        };
        let app = payback_router(
            &pool,
            vec![
                with_battery("2026-09-01", 10.0, 10.0),
                with_battery("2026-09-14", 5.0, 5.0),
            ],
        );
        let json = payback_json(&app).await;
        // (11.5 - 0.95 * 10) kWh at the flat 25 c is 50 c for the second battery.
        assert_eq!(json["saved"], 0.85);
        let (first, second) = (&json["parts"][0], &json["parts"][1]);
        assert_eq!(first["saved"], 0.35);
        assert_eq!(first["estimated"], false);
        assert_eq!(second["saved"], 0.5);
        assert_eq!(second["estimated"], true);
        assert_eq!(second["net"], -4.5);
        assert_eq!(second["recovered_pct"], 10.0);
        // Without it: 10 spent, 0.35 saved.
        let without = &json["without"];
        assert_eq!(without["name"], "Solar");
        assert_eq!(without["series"][0]["saved"], 0.35);
        assert_eq!(without["series"][0]["net"], -9.65);
        assert_eq!(without["rate_per_day"], 0.35);
        assert!(without["projected_payback"].is_string());
    }

    /// The ZeroHero plan: free import 11:00-14:00, 10 c export and a $1
    /// credit for (almost) no import 18:00-21:00.
    fn zerohero_tariff() -> TariffSchedule {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let text = r#"
            [tariff]
            supply_cents_per_day = 100.0
            default_import_cents = 40.0
            import = [{ from = "11:00", to = "14:00", cents = 0.0 }]
            default_export_cents = 0.0
            export = [{ from = "18:00", to = "21:00", cents = 10.0 }]
            [tariff.zero_import_credit]
            from = "18:00"
            to = "21:00"
            max_kwh = 0.1
            credit_cents = 100.0
        "#;
        let w: Wrapper = toml::from_str(text).unwrap();
        TariffSchedule::from_config(&w.tariff, &[]).unwrap()
    }

    /// A live sample every minute for the local day `days_ago` days back. The
    /// grid power in the 18:00-21:00 window is `window_grid_w` and the
    /// battery sits at 60%, dropping to 30% in the evening.
    async fn seed_battery_day(pool: &SqlitePool, days_ago: u64, window_grid_w: f64) {
        let tz = chrono_tz::Australia::Adelaide;
        let date = api::today(tz) - chrono::Days::new(days_ago);
        let (start, end) = api::day_bounds_or_error(date, tz).unwrap();
        for ts in (start..end).step_by(60) {
            let hour = (ts - start) / 3600;
            let in_window = (18..21).contains(&hour);
            let sample = Sample {
                grid_w: Some(if in_window { window_grid_w } else { 0.0 }),
                load_w: Some(500.0),
                soc_pct: Some(if hour >= 17 { 30.0 } else { 60.0 }),
                ..live_sample(ts)
            };
            db::insert_live_sample(pool, &sample).await.unwrap();
        }
    }

    #[tokio::test]
    async fn battery_needs_a_session_and_a_credit_in_the_tariff() {
        let (_dir, app, pool) = costs_app().await;
        let response = send(&app, get_request("/api/battery", None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // The flat tariff has no zero-import credit.
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/battery", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["configured"], false);

        // A bad `days` is refused once there is a credit to report on.
        let app = costs_router(&pool, zerohero_tariff());
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/battery?days=lots", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn battery_reports_earned_and_missed_days_with_reasons() {
        let (_dir, _app, pool) = costs_app().await;
        // Yesterday: no import in the window, so the credit is earned.
        seed_battery_day(&pool, 1, 0.0).await;
        // Two days ago: 1 kW import in the window, so it is missed.
        seed_battery_day(&pool, 2, 1000.0).await;
        // Three days ago: the same, but the car was charging then.
        seed_battery_day(&pool, 3, 1000.0).await;
        let tz = chrono_tz::Australia::Adelaide;
        let three_ago = api::today(tz) - chrono::Days::new(3);
        let evening = api::day_bounds_or_error(three_ago, tz).unwrap().0 + 19 * 3600;
        let id = db::start_charge_session(&pool, 1, evening, None, Some("Home"))
            .await
            .unwrap();
        let done = db::ChargeProgress {
            energy_added_kwh: None,
            energy_used_kwh: None,
            soc_pct: None,
            max_power_kw: None,
        };
        db::end_charge_session(&pool, id, evening + 600, &done)
            .await
            .unwrap();

        let app = costs_router(&pool, zerohero_tariff());
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/battery?days=4", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;

        // Newest first: today (window not over or no data), then the three days.
        let days = json["days"].as_array().unwrap();
        assert_eq!(days.len(), 4);
        let credits: Vec<&Value> = days.iter().map(|d| &d["credit"]).collect();
        assert_eq!(credits[1..], ["earned", "missed", "missed"]);
        assert_eq!(days[0]["credit"], "unknown");

        let earned = &days[1];
        assert_eq!(earned["credit_source"], "estimate");
        assert_eq!(earned["window_import_kwh"], 0.0);
        assert_eq!(earned["soc_at_11"], 60.0);
        assert_eq!(earned["soc_at_16"], 60.0);
        assert_eq!(earned["soc_at_21"], 30.0);
        assert_eq!(earned["min_soc_in_window"], 30.0);
        assert!(earned["soc_at_window_start"].is_number());
        assert!(earned["soc_at_window_end"].is_number());
        assert!(earned["window_soc_used_pct"].is_number());
        assert_eq!(earned["reason"], Value::Null);

        let missed = &days[2];
        // 3 hours at 1 kW, less the last minute (a sample covers the minute after it).
        assert_eq!(missed["window_import_kwh"], 3.0);
        assert_eq!(missed["reason"], "battery_low");
        assert_eq!(days[3]["reason"], "car_charging");

        let summary = &json["summary"];
        assert_eq!(summary["days"], 4);
        assert_eq!(summary["earned"], 1);
        assert_eq!(summary["missed"], 2);
        assert_eq!(summary["missed_dollars"], 2.0);
        assert_eq!(summary["battery_floor_pct"], 30.0);
        assert_eq!(summary["avg_soc_at_11"], 60.0);
        assert!(summary["typical_window_soc_used_pct"].is_number());
    }

    #[tokio::test]
    async fn battery_takes_the_credit_from_globirds_bill_when_it_has_the_day() {
        let (_dir, _app, pool) = costs_app().await;
        // Our data says two days ago missed the credit (1 kW import in the window)...
        seed_battery_day(&pool, 2, 1000.0).await;
        // ...but GloBird's bill for that day has the credit line.
        let tz = chrono_tz::Australia::Adelaide;
        let date = (api::today(tz) - chrono::Days::new(2)).to_string();
        for (category, cents) in [("SUPPLY", 100), ("USAGE", 75), ("ZEROHERO Credit", -100)] {
            sqlx::query("INSERT INTO globird_cost VALUES (?, ?, ?, 1.0, 0, 1000)")
                .bind(&date)
                .bind(category)
                .bind(cents)
                .execute(&pool)
                .await
                .unwrap();
        }
        let app = costs_router(&pool, zerohero_tariff());
        let cookie = log_in(&app).await;
        let response = send(&app, get_request("/api/battery?days=3", Some(&cookie))).await;
        let json = body_json(response).await;
        let day = &json["days"][2];
        assert_eq!(day["date"], date.as_str());
        assert_eq!(day["credit"], "earned");
        assert_eq!(day["credit_source"], "globird");
        assert_eq!(day["reason"], Value::Null);
        assert_eq!(json["summary"]["earned"], 1);
        assert_eq!(json["summary"]["missed"], 0);
    }

    // ---- the admin API ----

    /// A request with a JSON body, the login cookie and any extra headers.
    fn admin_request(
        method: &str,
        uri: &str,
        cookie: &str,
        body: &str,
        extra: &[(&str, &str)],
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    /// Sends a JSON write as the logged-in user and gives back status and body.
    async fn admin_send(
        app: &Router,
        method: &str,
        uri: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, Value) {
        let response = send(app, admin_request(method, uri, cookie, body, &[])).await;
        let status = response.status();
        (status, body_json(response).await)
    }

    async fn count(pool: &SqlitePool, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_one(pool)
            .await
            .unwrap();
        n
    }

    const PLAN: &str = r#"{"valid_from":null,"tariff":{"name":"Old plan","supply_cents_per_day":100.0,
        "default_import_cents":30.0,"default_export_cents":5.0,
        "import":[{"from":"11:00","to":"14:00","cents":0.0}]}}"#;

    #[tokio::test]
    async fn admin_routes_need_the_session_cookie_and_the_token_only_reads() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let (_dir, app, pool) = petrol_app(Some(token)).await;
        let bearer = format!("Bearer {token}");

        // Nobody logged in.
        for (method, uri) in [
            ("GET", "/api/admin"),
            ("POST", "/api/admin/tariffs"),
            ("PUT", "/api/admin/tariffs/1"),
            ("DELETE", "/api/admin/tariffs/1"),
            ("POST", "/api/admin/purchases"),
            ("PUT", "/api/admin/purchases/1"),
            ("DELETE", "/api/admin/purchases/1"),
            ("POST", "/api/admin/cars"),
            ("PUT", "/api/admin/cars/1"),
            ("DELETE", "/api/admin/cars/1"),
            ("POST", "/api/admin/cars/1/fills"),
            ("PUT", "/api/admin/teslamate-cars/1/owned-since"),
            ("PUT", "/api/admin/fills/1"),
            ("DELETE", "/api/admin/fills/1"),
            ("PUT", "/api/admin/bills"),
            ("DELETE", "/api/admin/bills/2026-01-01"),
            ("PUT", "/api/admin/settings"),
            ("GET", "/api/admin/backup"),
            ("GET", "/api/admin/backup/download"),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(PLAN))
                .unwrap();
            let response = send(&app, request).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri}"
            );
        }

        // The token reads, but cannot write.
        let read = Request::get("/api/admin")
            .header(header::AUTHORIZATION, &bearer)
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, read).await.status(), StatusCode::OK);
        for (method, uri) in [
            ("POST", "/api/admin/tariffs"),
            ("PUT", "/api/admin/settings"),
            ("PUT", "/api/petrol-prices"),
            ("DELETE", "/api/petrol-prices/2026-03-01"),
        ] {
            let request = admin_request(method, uri, "", PLAN, &[("authorization", &bearer)]);
            let response = send(&app, request).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri}"
            );
        }
        assert_eq!(count(&pool, "tariff_plan").await, 0);
    }

    #[tokio::test]
    async fn backup_download_is_session_only_and_serves_the_newest_file() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("backups");
        let pool = db::open(dir.path().join("test.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = hash_password(PASSWORD).unwrap();
        let state = AppState::new(
            pool,
            PLANT,
            chrono_tz::Australia::Adelaide,
            &hash,
            Key::generate(),
        )
        .unwrap()
        .with_api_token(Some(token))
        .with_backup_dir(backups.clone());
        let app = router(state);
        let cookie = log_in(&app).await;
        let bearer = format!("Bearer {token}");

        // No folder yet: nothing to show or download.
        let info = send(&app, get_request("/api/admin/backup", Some(&cookie))).await;
        assert_eq!(info.status(), StatusCode::OK);
        assert_eq!(body_json(info).await, serde_json::json!({ "latest": null }));
        let download = send(
            &app,
            get_request("/api/admin/backup/download", Some(&cookie)),
        )
        .await;
        assert_eq!(download.status(), StatusCode::NOT_FOUND);

        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(backups.join("solar-2026-10-25.db"), b"old").unwrap();
        std::fs::write(backups.join("solar-2026-10-26.db"), b"SQLite newest").unwrap();
        std::fs::write(backups.join("solar-2026-10-27.db.tmp"), b"partial").unwrap();

        let info = send(&app, get_request("/api/admin/backup", Some(&cookie))).await;
        assert_eq!(
            body_json(info).await,
            serde_json::json!({ "latest": { "date": "2026-10-26", "bytes": 13 } })
        );
        let response = send(
            &app,
            get_request("/api/admin/backup/download", Some(&cookie)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/vnd.sqlite3"
        );
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"solar-2026-10-26.db\""
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"SQLite newest");

        // The Bearer token and anonymous requests cannot download it.
        let by_token = Request::get("/api/admin/backup/download")
            .header(header::AUTHORIZATION, &bearer)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&app, by_token).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let anonymous = get_request("/api/admin/backup/download", None);
        assert_eq!(
            send(&app, anonymous).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn writes_from_another_origin_are_refused() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        let put = |headers: &[(&str, &str)]| {
            admin_request(
                "PUT",
                "/api/admin/settings",
                &cookie,
                r#"{"petrol_grade":"Premium 98"}"#,
                headers,
            )
        };
        // Another site, "null" (sandboxed pages) and a different port are refused.
        for origin in [
            "https://evil.example",
            "null",
            "https://solar.example:8443",
            "http://solar.example.evil.example",
        ] {
            let response = send(&app, put(&[("host", "solar.example"), ("origin", origin)])).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
            assert_eq!(
                body_json(response).await["error"],
                "cross-origin request refused"
            );
        }
        assert_eq!(count(&pool, "setting").await, 1);

        // The same host (any scheme, any case), or no Origin at all, is fine.
        for headers in [
            vec![
                ("host", "solar.example"),
                ("origin", "https://solar.example"),
            ],
            vec![
                ("host", "Solar.Example:8080"),
                ("origin", "http://solar.example:8080"),
            ],
            vec![("host", "solar.example")],
            vec![],
        ] {
            let response = send(&app, put(&headers)).await;
            assert_eq!(response.status(), StatusCode::OK, "{headers:?}");
        }

        // Reads are never refused for their Origin.
        let read = Request::get("/api/admin")
            .header(header::COOKIE, &cookie)
            .header("host", "solar.example")
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, read).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_read_lists_everything_and_can_be_downloaded() {
        let (_dir, app, _pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        admin_send(&app, "POST", "/api/admin/tariffs", &cookie, PLAN).await;
        admin_send(
            &app,
            "POST",
            "/api/admin/purchases",
            &cookie,
            r#"{"name":"Panels","date":"2024-01-02","cost_dollars":9000.0}"#,
        )
        .await;
        admin_send(
            &app,
            "PUT",
            "/api/petrol-prices",
            &cookie,
            r#"{"from":"2026-03-01","cents_per_litre":199.0}"#,
        )
        .await;

        let response = send(&app, get_request("/api/admin", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(header::CONTENT_DISPOSITION)
                .is_none()
        );
        let json = body_json(response).await;
        assert_eq!(json["tariffs"][0]["valid_from"], Value::Null);
        assert_eq!(json["tariffs"][0]["tariff"]["name"], "Old plan");
        assert_eq!(json["tariffs"][0]["tariff"]["import"][0]["to"], "14:00");
        assert_eq!(json["petrol"]["fallback_cents_per_litre"], 190.0);
        assert_eq!(json["petrol"]["prices"][0]["cents_per_litre"], 199.0);
        assert_eq!(json["purchases"][0]["name"], "Panels");
        assert_eq!(json["purchases"][0]["battery_kwh"], Value::Null);
        assert_eq!(json["manual_cars"], serde_json::json!([]));
        assert_eq!(json["bills"], serde_json::json!([]));
        assert_eq!(json["settings"]["petrol_cents_per_litre"], 190.0);
        assert_eq!(json["setting_defaults"]["petrol_litres_per_100km"], 7.5);
        // Newest change first, with the row key and no before/after.
        let changes = json["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0]["what"], "petrol");
        assert_eq!(changes[0]["action"], "create");
        assert_eq!(changes[0]["key"], "2026-03-01");
        assert!(changes[0]["at"].as_i64().unwrap() > 0);
        assert!(changes[0].get("before").is_none());

        let response = send(&app, get_request("/api/admin?download=1", Some(&cookie))).await;
        let disposition = response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            disposition.starts_with("attachment; filename=\"solar-dash-admin-20")
                && disposition.ends_with(".json\""),
            "{disposition}"
        );
        assert_eq!(body_json(response).await["purchases"][0]["name"], "Panels");
    }

    #[tokio::test]
    async fn tariff_plans_can_be_added_changed_and_deleted_and_show_at_once() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;

        // No plan: the cost features are off.
        let response = send(&app, get_request("/api/tariff", Some(&cookie))).await;
        assert_eq!(body_json(response).await["configured"], false);

        let (status, plan) = admin_send(&app, "POST", "/api/admin/tariffs", &cookie, PLAN).await;
        assert_eq!(status, StatusCode::CREATED);
        let first = plan["id"].as_i64().unwrap();
        assert_eq!(plan["tariff"]["name"], "Old plan");
        // No restart needed: the new plan is in force.
        let response = send(&app, get_request("/api/tariff", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["default_import_cents"], 30.0);

        // A newer plan from a date. The older one now ends the day before.
        let newer = r#"{"valid_from":"2026-05-26","tariff":{"supply_cents_per_day":120.0,
            "default_import_cents":40.0}}"#;
        let (status, plan) = admin_send(&app, "POST", "/api/admin/tariffs", &cookie, newer).await;
        assert_eq!(status, StatusCode::CREATED);
        let second = plan["id"].as_i64().unwrap();
        for (date, cents) in [("2026-05-25", 30.0), ("2026-05-26", 40.0)] {
            let uri = format!("/api/tariff?date={date}");
            let response = send(&app, get_request(&uri, Some(&cookie))).await;
            assert_eq!(body_json(response).await["default_import_cents"], cents);
        }

        // Bad plans are refused and nothing changes (not even the change log).
        let changes = count(&pool, "admin_change").await;
        for bad in [
            // The same start day.
            r#"{"valid_from":"2026-05-26","tariff":{"supply_cents_per_day":1.0,"default_import_cents":1.0}}"#,
            // A second plan with no start.
            r#"{"valid_from":null,"tariff":{"supply_cents_per_day":1.0,"default_import_cents":1.0}}"#,
            r#"{"valid_from":"26/05/2026","tariff":{"supply_cents_per_day":1.0,"default_import_cents":1.0}}"#,
            r#"{"valid_from":"2026-06-01","tariff":{"supply_cents_per_day":-1.0,"default_import_cents":1.0}}"#,
            r#"{"valid_from":"2026-06-01","tariff":{"supply_cents_per_day":1.0,"default_import_cents":1.0,
                "import":[{"from":"9:5","to":"10:00","cents":1.0}]}}"#,
            r#"{"valid_from":"2026-06-01"}"#,
            "not json",
        ] {
            let (status, json) = admin_send(&app, "POST", "/api/admin/tariffs", &cookie, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(json["error"].is_string(), "{bad}");
        }
        assert_eq!(count(&pool, "tariff_plan").await, 2);
        assert_eq!(count(&pool, "admin_change").await, changes);
        // The older plan cannot be moved onto the newer one's day either.
        let clash = r#"{"valid_from":"2026-05-26","tariff":{"supply_cents_per_day":1.0,"default_import_cents":1.0}}"#;
        let uri = format!("/api/admin/tariffs/{first}");
        let (status, _) = admin_send(&app, "PUT", &uri, &cookie, clash).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Update the newer plan's rate: it shows at once.
        let changed = r#"{"valid_from":"2026-05-26","tariff":{"supply_cents_per_day":120.0,
            "default_import_cents":45.0}}"#;
        let uri = format!("/api/admin/tariffs/{second}");
        let (status, plan) = admin_send(&app, "PUT", &uri, &cookie, changed).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(plan["tariff"]["default_import_cents"], 45.0);
        let response = send(
            &app,
            get_request("/api/tariff?date=2026-06-01", Some(&cookie)),
        )
        .await;
        assert_eq!(body_json(response).await["default_import_cents"], 45.0);
        let (status, _) = admin_send(&app, "PUT", "/api/admin/tariffs/999", &cookie, changed).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The change log keeps the rows before and after.
        let (before, after): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT before, after FROM admin_change WHERE action = 'update' AND what = 'tariffs'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(before.unwrap().contains("40.0"));
        assert!(after.unwrap().contains("45.0"));

        // Delete both: cost features go off again.
        for id in [second, first] {
            let uri = format!("/api/admin/tariffs/{id}");
            let (status, json) = admin_send(&app, "DELETE", &uri, &cookie, "").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(json["deleted"]["id"], id);
        }
        let (status, _) = admin_send(&app, "DELETE", "/api/admin/tariffs/1", &cookie, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let response = send(&app, get_request("/api/tariff", Some(&cookie))).await;
        assert_eq!(body_json(response).await["configured"], false);
    }

    #[tokio::test]
    async fn tariff_preview_compares_the_current_plans_with_the_change() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        let preview = "/api/admin/tariffs/preview?from=2026-09-15&to=2026-09-15";

        // No plan yet: only the new cost is known. Import 1 kWh and export
        // 2 kWh on 15 September (100 + 30 - 10 = 120 c).
        for point_id in [83102, 83072, 83118] {
            let point = Point {
                point_id,
                code: None,
                name: None,
                unit: Some("Wh".to_string()),
            };
            db::upsert_point(&pool, &point).await.unwrap();
        }
        seed_counters(&pool, SEP_15_START, &[3600, 3900], 1000.0).await;
        let (status, json) = admin_send(&app, "POST", preview, &cookie, PLAN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["days"][0]["date"], "2026-09-15");
        assert_eq!(json["days"][0]["current_cents"], Value::Null);
        assert_eq!(json["days"][0]["new_cents"], 120.0);
        assert_eq!(json["total"]["current_cents"], Value::Null);

        let (_, plan) = admin_send(&app, "POST", "/api/admin/tariffs", &cookie, PLAN).await;
        let id = plan["id"].as_i64().unwrap();

        // Replacing the plan: the import rate goes from 30 to 40 c.
        let dearer = format!(
            r#"{{"id":{id},"valid_from":null,"tariff":{{"supply_cents_per_day":100.0,
            "default_import_cents":40.0,"default_export_cents":5.0}}}}"#
        );
        let (status, json) = admin_send(&app, "POST", preview, &cookie, &dearer).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["days"].as_array().unwrap().len(), 1);
        assert_eq!(json["days"][0]["current_cents"], 120.0);
        assert_eq!(json["days"][0]["new_cents"], 130.0);
        assert_eq!(json["total"]["current_cents"], 120.0);
        assert_eq!(json["total"]["new_cents"], 130.0);

        // Adding a plan that starts on 1 September: a dearer supply charge.
        let added = r#"{"valid_from":"2026-09-01","tariff":{"supply_cents_per_day":200.0,
            "default_import_cents":30.0,"default_export_cents":5.0}}"#;
        let (_, json) = admin_send(&app, "POST", preview, &cookie, added).await;
        assert_eq!(json["days"][0]["current_cents"], 120.0);
        assert_eq!(json["days"][0]["new_cents"], 220.0);

        // Nothing was saved or stored: the plan is as it was and no day is cached.
        assert_eq!(count(&pool, "tariff_plan").await, 1);
        assert_eq!(count(&pool, "daily_cost").await, 0);
        let response = send(&app, get_request("/api/tariff", Some(&cookie))).await;
        assert_eq!(body_json(response).await["default_import_cents"], 30.0);

        // Mistakes are refused like a save would.
        let bad_rate = r#"{"valid_from":null,"tariff":{"supply_cents_per_day":-1.0,
            "default_import_cents":40.0}}"#;
        let (status, _) = admin_send(&app, "POST", preview, &cookie, bad_rate).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let same_day = r#"{"valid_from":null,"tariff":{"supply_cents_per_day":1.0,
            "default_import_cents":40.0}}"#;
        let (status, json) = admin_send(&app, "POST", preview, &cookie, same_day).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        let unknown = dearer.replace(&format!("\"id\":{id}"), "\"id\":999");
        let (status, _) = admin_send(&app, "POST", preview, &cookie, &unknown).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let long = "/api/admin/tariffs/preview?from=2026-01-01&to=2026-05-01";
        let (status, _) = admin_send(&app, "POST", long, &cookie, &dearer).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // The default range is the last 30 days: this data is older, so no days.
        let (status, json) =
            admin_send(&app, "POST", "/api/admin/tariffs/preview", &cookie, &dearer).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["days"].as_array().unwrap().len() <= 1);
    }

    #[tokio::test]
    async fn purchases_can_be_added_changed_and_deleted() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        let (_, _) = admin_send(&app, "POST", "/api/admin/tariffs", &cookie, PLAN).await;
        let response = send(&app, get_request("/api/payback", Some(&cookie))).await;
        assert_eq!(body_json(response).await["configured"], false);

        let panels = r#"{"name":" Panels ","date":"2024-01-02","cost_dollars":9000.0}"#;
        let (status, json) =
            admin_send(&app, "POST", "/api/admin/purchases", &cookie, panels).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json["name"], "Panels");
        let id = json["id"].as_i64().unwrap();
        // The payback API sees it without a restart.
        let response = send(&app, get_request("/api/payback", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["configured"], true);
        assert_eq!(json["total_cost"], 9000.0);

        let battery =
            r#"{"name":"Battery","date":"2025-03-04","cost_dollars":5000.0,"battery_kwh":10.0}"#;
        let (status, second) =
            admin_send(&app, "POST", "/api/admin/purchases", &cookie, battery).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(second["battery_kwh"], 10.0);

        let changes = count(&pool, "admin_change").await;
        for bad in [
            r#"{"name":"  ","date":"2024-01-02","cost_dollars":1.0}"#,
            r#"{"name":"x","date":"yesterday","cost_dollars":1.0}"#,
            r#"{"name":"x","date":"2024-01-02","cost_dollars":-1.0}"#,
            r#"{"name":"x","date":"2024-01-02","cost_dollars":1.0,"battery_kwh":0}"#,
            r#"{"name":"x","date":"2024-01-02"}"#,
        ] {
            let (status, _) = admin_send(&app, "POST", "/api/admin/purchases", &cookie, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(count(&pool, "purchase").await, 2);
        assert_eq!(count(&pool, "admin_change").await, changes);

        let uri = format!("/api/admin/purchases/{id}");
        let cheaper = r#"{"name":"Panels","date":"2024-01-02","cost_dollars":8500.0}"#;
        let (status, json) = admin_send(&app, "PUT", &uri, &cookie, cheaper).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["cost_dollars"], 8500.0);
        let response = send(&app, get_request("/api/payback", Some(&cookie))).await;
        assert_eq!(body_json(response).await["total_cost"], 13500.0);
        let (status, _) =
            admin_send(&app, "PUT", "/api/admin/purchases/999", &cookie, cheaper).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, json) = admin_send(&app, "DELETE", &uri, &cookie, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["deleted"]["cost_dollars"], 8500.0);
        let (status, _) = admin_send(&app, "DELETE", &uri, &cookie, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(count(&pool, "purchase").await, 1);
        let actions: Vec<(String,)> =
            sqlx::query_as("SELECT action FROM admin_change WHERE what = 'purchases' ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let actions: Vec<&str> = actions.iter().map(|a| a.0.as_str()).collect();
        assert_eq!(actions, ["create", "create", "update", "delete"]);
    }

    #[tokio::test]
    async fn manual_cars_and_fills_can_be_added_changed_and_deleted() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;

        let (status, car) = admin_send(
            &app,
            "POST",
            "/api/admin/cars",
            &cookie,
            r#"{"name":" Family PHEV ","model":" Outlander "}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(car["name"], "Family PHEV");
        assert_eq!(car["model"], "Outlander");
        assert_eq!(car["kind"], "phev");
        let id = car["id"].as_i64().unwrap();
        let fills_uri = format!("/api/admin/cars/{id}/fills");

        let changes = count(&pool, "admin_change").await;
        for bad in [
            r#"{"name":"  "}"#,
            r#"{"name":"x","kind":"steam"}"#,
            r#"{"model":"x"}"#,
            "not json",
        ] {
            let (status, json) = admin_send(&app, "POST", "/api/admin/cars", &cookie, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(json["error"].is_string(), "{bad}");
        }
        for (what, bad) in [
            (
                "date",
                r#"{"date":"yesterday","litres":40,"cost_dollars":80}"#,
            ),
            (
                "future",
                r#"{"date":"2999-01-01","litres":40,"cost_dollars":80}"#,
            ),
            (
                "zero litres",
                r#"{"date":"2026-03-01","litres":0,"cost_dollars":80}"#,
            ),
            (
                "200 litres",
                r#"{"date":"2026-03-01","litres":200,"cost_dollars":80}"#,
            ),
            (
                "cost",
                r#"{"date":"2026-03-01","litres":40,"cost_dollars":-1}"#,
            ),
            (
                "odometer",
                r#"{"date":"2026-03-01","litres":40,"cost_dollars":80,"odometer_km":-5}"#,
            ),
            ("no litres", r#"{"date":"2026-03-01","cost_dollars":80}"#),
        ] {
            let (status, json) = admin_send(&app, "POST", &fills_uri, &cookie, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{what}");
            assert!(json["error"].is_string(), "{what}");
        }
        assert_eq!(
            (
                count(&pool, "manual_car").await,
                count(&pool, "fuel_fill").await
            ),
            (1, 0)
        );
        assert_eq!(count(&pool, "admin_change").await, changes);

        // A fill for a car that does not exist.
        let ok = r#"{"date":"2026-03-01","litres":40.0,"cost_dollars":80.0,"odometer_km":1000}"#;
        let (status, _) = admin_send(&app, "POST", "/api/admin/cars/999/fills", &cookie, ok).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, fill) = admin_send(&app, "POST", &fills_uri, &cookie, ok).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(fill["car_id"], id);
        assert_eq!(fill["full_tank"], true);
        assert_eq!(fill["note"], "");
        let fill_id = fill["id"].as_i64().unwrap();
        let second = r#"{"date":"2026-03-10","litres":30.0,"cost_dollars":66.0,
            "odometer_km":1500,"full_tank":true,"note":" servo "}"#;
        let (status, json) = admin_send(&app, "POST", &fills_uri, &cookie, second).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json["note"], "servo");

        // The admin read lists the car with its fill count.
        let response = send(&app, get_request("/api/admin", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["manual_cars"][0]["name"], "Family PHEV");
        assert_eq!(json["manual_cars"][0]["fills"], 2);

        // Newest fill first, with the maths: 30 L over 500 km.
        let response = send(&app, get_request("/api/fuel/cars", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let car = &json["cars"][0];
        assert_eq!(car["source"], "manual");
        assert_eq!(car["label"], "Outlander");
        assert_eq!(car["l_per_100km"], 6.0);
        assert_eq!(car["cost_per_km"], 66.0 / 500.0);
        assert_eq!(car["totals"]["fills"], 2);
        assert_eq!(car["totals"]["litres"], 70.0);
        assert_eq!(car["totals"]["km_tracked"], 500.0);
        assert_eq!(car["months"][0]["month"], "2026-03");
        assert_eq!(car["months"][0]["fills"], 2);
        let fills = car["fills"].as_array().unwrap();
        assert_eq!(fills[0]["date"], "2026-03-10");
        assert_eq!(fills[0]["l_per_100km"], 6.0);
        assert_eq!(fills[0]["km_since_last"], 500.0);
        assert!((fills[0]["cents_per_litre"].as_f64().unwrap() - 220.0).abs() < 1e-9);
        // The base petrol price of the test app.
        assert_eq!(fills[0]["avg_cents_per_litre"], 190.0);
        assert_eq!(fills[1]["date"], "2026-03-01");
        assert_eq!(fills[1]["l_per_100km"], Value::Null);

        // Change a fill (it keeps its car) and a car (no kind: it keeps its own).
        let fill_uri = format!("/api/admin/fills/{fill_id}");
        let edited = r#"{"date":"2026-03-01","litres":40.0,"cost_dollars":80.0,
            "odometer_km":1100,"full_tank":true}"#;
        let (status, json) = admin_send(&app, "PUT", &fill_uri, &cookie, edited).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (json["car_id"].as_i64(), json["odometer_km"].as_f64()),
            (Some(id), Some(1100.0))
        );
        let (status, _) = admin_send(&app, "PUT", "/api/admin/fills/999", &cookie, edited).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = admin_send(
            &app,
            "PUT",
            &fill_uri,
            &cookie,
            r#"{"date":"2999-01-01","litres":1,"cost_dollars":1}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let car_uri = format!("/api/admin/cars/{id}");
        let (status, json) = admin_send(&app, "PUT", &car_uri, &cookie, r#"{"name":"Zoe"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (
                json["name"].as_str(),
                json["model"].clone(),
                json["kind"].as_str()
            ),
            (Some("Zoe"), Value::Null, Some("phev"))
        );
        let (status, _) = admin_send(
            &app,
            "PUT",
            "/api/admin/cars/999",
            &cookie,
            r#"{"name":"x"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let response = send(&app, get_request("/api/fuel/cars", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["cars"][0]["label"], "Zoe");
        assert_eq!(json["cars"][0]["l_per_100km"], 30.0 / 400.0 * 100.0);

        // One fill goes; then the car takes the other with it.
        let (status, json) = admin_send(&app, "DELETE", &fill_uri, &cookie, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["deleted"]["litres"], 40.0);
        let (status, _) = admin_send(&app, "DELETE", &fill_uri, &cookie, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, json) = admin_send(&app, "DELETE", &car_uri, &cookie, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["deleted"]["name"], "Zoe");
        assert_eq!(json["fills_deleted"], 1);
        let (status, _) = admin_send(&app, "DELETE", &car_uri, &cookie, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            (
                count(&pool, "manual_car").await,
                count(&pool, "fuel_fill").await
            ),
            (0, 0)
        );

        let log: Vec<(String, String)> = sqlx::query_as(
            "SELECT what, action FROM admin_change WHERE what IN ('cars', 'fills') ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let log: Vec<(&str, &str)> = log.iter().map(|(w, a)| (w.as_str(), a.as_str())).collect();
        assert_eq!(
            log,
            [
                ("cars", "create"),
                ("fills", "create"),
                ("fills", "create"),
                ("fills", "update"),
                ("cars", "update"),
                ("fills", "delete"),
                ("cars", "delete"),
            ]
        );
        // The deleted car's fill is kept in the log.
        let (before,): (String,) = sqlx::query_as(
            "SELECT before FROM admin_change WHERE what = 'cars' AND action = 'delete'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let before: Value = serde_json::from_str(&before).unwrap();
        assert_eq!(before["fills"][0]["note"], "servo");
    }

    #[tokio::test]
    async fn fuel_cars_are_listed_for_the_token_and_a_car_without_fills_shows() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let (_dir, app, _pool) = petrol_app(Some(token)).await;
        let cookie = log_in(&app).await;
        let anonymous = send(&app, get_request("/api/fuel/cars", None)).await;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        let (_, car) = admin_send(
            &app,
            "POST",
            "/api/admin/cars",
            &cookie,
            r#"{"name":"Hilux","kind":"diesel"}"#,
        )
        .await;
        let request = Request::get("/api/fuel/cars")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response).await;
        let listed = &json["cars"][0];
        assert_eq!(listed["car_id"], car["id"]);
        assert_eq!(
            (&listed["label"], &listed["kind"]),
            (&Value::from("Hilux"), &Value::from("diesel"))
        );
        assert_eq!(listed["fills"], serde_json::json!([]));
        assert_eq!(listed["l_per_100km"], Value::Null);
        assert_eq!(listed["totals"]["fills"], 0);

        // The token cannot write.
        let request = admin_request(
            "POST",
            "/api/admin/cars",
            "",
            r#"{"name":"x"}"#,
            &[("authorization", &format!("Bearer {token}"))],
        );
        assert_eq!(send(&app, request).await.status(), StatusCode::UNAUTHORIZED);

        // TeslaMate cars say where they come from.
        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn csv_request(
        method: &str,
        uri: &str,
        cookie: &str,
        content_type: &str,
        body: String,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn bills_are_replaced_from_csv_and_deleted_one_at_a_time() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        let csv = format!(
            "{BILLS_HEADER}2026-01-01,2026-01-31,152.40,0,610,190.00,280,22.40,48.50,summer, bill\n\
             2026-02-01,2026-02-28,98.00,25.00,540,170.00,310,25.00,44.00,\n"
        );
        let put = |content_type: &str, body: String| {
            csv_request("PUT", "/api/admin/bills", &cookie, content_type, body)
        };

        // Only CSV is accepted.
        let response = send(&app, put("application/json", csv.clone())).await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let response = send(&app, put("text/csv; charset=utf-8", csv.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["rows"], 2);
        let response = send(&app, get_request("/api/admin", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["bills"].as_array().unwrap().len(), 2);
        assert_eq!(json["bills"][0]["note"], "summer, bill");
        assert_eq!(json["bills"][1]["excluded"], 25.0);

        // A bad file (a bad line, or overlapping periods) changes nothing.
        let overlap = format!(
            "{BILLS_HEADER}2026-01-01,2026-01-31,1,0,1,1,1,1,1,\n2026-01-31,2026-02-28,1,0,1,1,1,1,1,\n"
        );
        for bad in [format!("{BILLS_HEADER}oops\n"), overlap, String::new()] {
            let response = send(&app, put("text/csv", bad)).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(body_json(response).await["error"].is_string());
        }
        assert_eq!(count(&pool, "bill").await, 2);

        // A replacement logs the row counts.
        let one = format!("{BILLS_HEADER}2026-03-01,2026-03-31,10,0,1,1,1,1,1,\n");
        let response = send(&app, put("text/csv", one)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (before, after): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT before, after FROM admin_change WHERE what = 'bills' AND action = 'replace' \
             ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(before.as_deref(), Some(r#"{"rows":2}"#));
        assert_eq!(after.as_deref(), Some(r#"{"rows":1}"#));

        let (status, json) =
            admin_send(&app, "DELETE", "/api/admin/bills/2026-03-01", &cookie, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["deleted"]["bill_total"], 10.0);
        let (status, _) =
            admin_send(&app, "DELETE", "/api/admin/bills/2026-03-01", &cookie, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = admin_send(&app, "DELETE", "/api/admin/bills/March", &cookie, "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(count(&pool, "bill").await, 0);
    }

    #[tokio::test]
    async fn manual_cars_keep_an_owned_since_date() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        let cars = "/api/admin/cars";

        let (status, car) = admin_send(
            &app,
            "POST",
            cars,
            &cookie,
            r#"{"name":"PHEV","owned_since":"2024-05-06"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(car["owned_since"], "2024-05-06");
        let id = car["id"].as_i64().unwrap();
        let uri = format!("/api/admin/cars/{id}");

        // A car with no date says null.
        let (_, plain) = admin_send(&app, "POST", cars, &cookie, r#"{"name":"Plain"}"#).await;
        assert_eq!(plain["owned_since"], Value::Null);

        let changes = count(&pool, "admin_change").await;
        for bad in [
            r#"{"name":"PHEV","owned_since":"last year"}"#,
            r#"{"name":"PHEV","owned_since":"2024-02-30"}"#,
            r#"{"name":"PHEV","owned_since":"2999-01-01"}"#,
            r#"{"name":"PHEV","owned_since":5}"#,
        ] {
            let (status, json) = admin_send(&app, "POST", cars, &cookie, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(json["error"].is_string(), "{bad}");
            let (status, _) = admin_send(&app, "PUT", &uri, &cookie, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(count(&pool, "admin_change").await, changes);
        assert_eq!(count(&pool, "manual_car").await, 2);

        // Changed, and listed by the admin and fuel reads.
        let (status, json) = admin_send(
            &app,
            "PUT",
            &uri,
            &cookie,
            r#"{"name":"PHEV","owned_since":"2023-01-02"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["owned_since"], "2023-01-02");
        let json = body_json(send(&app, get_request("/api/admin", Some(&cookie))).await).await;
        assert_eq!(json["manual_cars"][0]["owned_since"], "2023-01-02");
        assert_eq!(json["manual_cars"][1]["owned_since"], Value::Null);
        let json = body_json(send(&app, get_request("/api/fuel/cars", Some(&cookie))).await).await;
        assert_eq!(json["cars"][0]["owned_since"], "2023-01-02");
        assert_eq!(json["cars"][1]["owned_since"], Value::Null);

        // The change log has the before and after.
        let (before, after): (String, String) = sqlx::query_as(
            "SELECT before, after FROM admin_change WHERE what = 'cars' AND action = 'update'
             ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(before.contains("2024-05-06"), "{before}");
        assert!(after.contains("2023-01-02"), "{after}");

        // A PUT without it (or with null) clears it.
        let (status, json) = admin_send(&app, "PUT", &uri, &cookie, r#"{"name":"PHEV"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["owned_since"], Value::Null);
        let (_, json) = admin_send(
            &app,
            "PUT",
            &uri,
            &cookie,
            r#"{"name":"PHEV","owned_since":"2023-01-02"}"#,
        )
        .await;
        assert_eq!(json["owned_since"], "2023-01-02");
        let (_, json) = admin_send(
            &app,
            "PUT",
            &uri,
            &cookie,
            r#"{"name":"PHEV","owned_since":null}"#,
        )
        .await;
        assert_eq!(json["owned_since"], Value::Null);
    }

    #[tokio::test]
    async fn teslamate_cars_keep_an_owned_since_date() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        db::upsert_tm_cars(
            &pool,
            &[db::TmCar {
                id: 7,
                name: Some("Daily".to_string()),
                model: Some("Y".to_string()),
                ..db::TmCar::default()
            }],
            1,
        )
        .await
        .unwrap();
        db::upsert_tm_drive_days(
            &pool,
            &[db::TmDriveDay {
                car_id: 7,
                date: "2026-09-05".to_string(),
                km: 10.0,
                drives: 1,
            }],
        )
        .await
        .unwrap();
        let uri = "/api/admin/teslamate-cars/7/owned-since";
        let owned = || async {
            let json =
                body_json(send(&app, get_request("/api/ev/cars", Some(&cookie))).await).await;
            json["cars"][0]["owned_since"].clone()
        };
        assert_eq!(owned().await, Value::Null);

        let (status, json) =
            admin_send(&app, "PUT", uri, &cookie, r#"{"owned_since":"2025-03-04"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json,
            serde_json::json!({"car_id":7,"owned_since":"2025-03-04"})
        );
        assert_eq!(owned().await, "2025-03-04");

        // A TeslaMate sync does not lose it.
        db::upsert_tm_cars(
            &pool,
            &[db::TmCar {
                id: 7,
                name: Some("Renamed".to_string()),
                ..db::TmCar::default()
            }],
            2,
        )
        .await
        .unwrap();
        assert_eq!(owned().await, "2025-03-04");

        // Changing it replaces the row.
        let (status, _) =
            admin_send(&app, "PUT", uri, &cookie, r#"{"owned_since":"2025-04-05"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(owned().await, "2025-04-05");
        assert_eq!(count(&pool, "tm_car_owned").await, 1);

        let changes = count(&pool, "admin_change").await;
        for bad in [
            r#"{"owned_since":"soon"}"#,
            r#"{"owned_since":"2999-01-01"}"#,
            r#"{"owned_since":7}"#,
            r#"{}"#,
            "not json",
        ] {
            let (status, json) = admin_send(&app, "PUT", uri, &cookie, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(json["error"].is_string(), "{bad}");
        }
        assert_eq!(count(&pool, "admin_change").await, changes);
        assert_eq!(owned().await, "2025-04-05");

        let (status, json) = admin_send(&app, "PUT", uri, &cookie, r#"{"owned_since":null}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json, serde_json::json!({"car_id":7,"owned_since":null}));
        assert_eq!(owned().await, Value::Null);
        assert_eq!(count(&pool, "tm_car_owned").await, 0);
        let actions: Vec<(String,)> = sqlx::query_as(
            "SELECT action FROM admin_change WHERE what = 'teslamate_cars' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(actions.len(), 3);
    }

    #[tokio::test]
    async fn settings_are_set_reset_and_checked() {
        let (_dir, app, pool) = petrol_app(None).await;
        let cookie = log_in(&app).await;
        let put = |body: &'static str| {
            let (app, cookie) = (app.clone(), cookie.clone());
            async move { admin_send(&app, "PUT", "/api/admin/settings", &cookie, body).await }
        };

        let (status, json) = put(
            r#"{"petrol_grade":"Premium 98","compare_cars":{"Y":"Toyota RAV4"},
                "price_rise_percent":5.5,"away_cents_per_kwh":65.0}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["petrol_grade"], "Premium 98");
        assert_eq!(json["petrol_cents_per_litre"], 190.0);
        // The cars API uses it without a restart.
        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        assert_eq!(body_json(response).await["petrol_grade"], "Premium 98");

        // Bad requests change nothing, even the valid keys next to a bad one.
        let changes = count(&pool, "admin_change").await;
        for bad in [
            r#"{"petrol_grade":"Diesel","colour":"red"}"#,
            r#"{"petrol_grade":"  "}"#,
            r#"{"petrol_grade":5}"#,
            r#"{"price_rise_percent":21}"#,
            r#"{"price_rise_percent":-1}"#,
            r#"{"petrol_litres_per_100km":0}"#,
            r#"{"petrol_cents_per_litre":"lots"}"#,
            // Warning at or above the default limit (15), or limit at or below the warning (12).
            r#"{"grid_pull_warn_kw":15}"#,
            r#"{"grid_pull_limit_kw":12}"#,
            r#"{"grid_pull_warn_kw":0}"#,
            r#"{"away_cents_per_kwh":-4}"#,
            r#"{"compare_cars":{"Y":""}}"#,
            r#"{"compare_cars":["Y"]}"#,
        ] {
            let (status, json) = put(bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(json["error"].is_string(), "{bad}");
        }
        assert_eq!(count(&pool, "admin_change").await, changes);
        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        assert_eq!(body_json(response).await["petrol_grade"], "Premium 98");

        // Both thresholds can move together, and /api/live reports them in W.
        let (status, _) = put(r#"{"grid_pull_warn_kw":16,"grid_pull_limit_kw":18.5}"#).await;
        assert_eq!(status, StatusCode::OK);
        let response = send(&app, get_request("/api/live", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["grid_pull"]["warn_w"], 16000);
        assert_eq!(json["grid_pull"]["limit_w"], 18500);
        put(r#"{"grid_pull_warn_kw":null,"grid_pull_limit_kw":null}"#).await;

        // null puts a key back to its default (and removes its row).
        let (status, json) = put(r#"{"petrol_grade":null,"petrol_cents_per_litre":null}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.get("petrol_grade").is_none());
        assert!(json.get("petrol_cents_per_litre").is_none());
        let response = send(&app, get_request("/api/ev/cars", Some(&cookie))).await;
        let json = body_json(response).await;
        assert_eq!(json["petrol_grade"], "Unleaded 91");
        assert_eq!(json["configured"], false);
        // Resetting what is already default, or setting the same value, logs nothing.
        let changes = count(&pool, "admin_change").await;
        put(r#"{"petrol_grade":null,"price_rise_percent":5.5}"#).await;
        assert_eq!(count(&pool, "admin_change").await, changes);
    }
}
