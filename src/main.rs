use std::{env, net::Ipv4Addr, str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};

use chrono::{DateTime, Local, NaiveDate, SecondsFormat, TimeZone, Utc};

use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::RwLock, time::sleep};

const DEFAULT_ADDRESS: &str = "200 E Wells St, Milwaukee, WI 53202";
const REFRESH_HOUR: u32 = 4;
const SCHEDULE_URL: &str = "https://itmdapps.milwaukee.gov/DpwServletsPublicAll/garbageDayService";
const TAX_URL: &str = "https://itmdapps.milwaukee.gov/ItmdServices/gis/Tax";
const CENSUS_URL: &str = "https://geocoding.geo.census.gov/geocoder/locations/onelineaddress";
const USER_AGENT: &str = "Mozilla/5.0 (MagicMirror)";
const REFERER: &str =
    "https://itmdapps.milwaukee.gov/DpwServletsPublicAll/getCollectionScheduleByGPS.jsp";
const RECYCLE_BLUE: &str = "#21864f3";
const GARBAGE_GREEN: &str = "#4caf50";

struct Config {
    port: u16,
    address: Option<String>,
    lat: f64,
    lng: f64,
}

impl Config {
    fn from_env() -> Result<Self> {
        let port: u16 = env_number("PORT", 8577)?;
        let lat: f64 = env_number("LAT", 43.0417)?;
        let lng: f64 = env_number("LNG", -87.9098)?;
        if port == 0 {
            bail!("PORT must be set between 1 and 65535!");
        }

        if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
            bail!("LAT must be a finite number between -90 and 90!");
        }

        if !lng.is_finite() || !(-180.0..=180.0).contains(&lng) {
            bail!("LNG must be a finite number between -180 and 180!");
        }

        let address = match env::var("ADDRESS") {
            Ok(value) => {
                let value = value.trim().to_owned();

                if value.is_empty() { None } else { Some(value) }
            }
            Err(env::VarError::NotPresent) => Some(DEFAULT_ADDRESS.to_owned()),
            Err(error) => return Err(error.into()),
        };

        Ok(Self {
            port,
            address,
            lat,
            lng,
        })
    }
}

fn env_number<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr,
{
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| anyhow!("Invalid {name}: {value:?}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    date: Option<NaiveDate>,
    raw: Option<String>,
    route: Value,
    alt_date: Option<NaiveDate>,
    alt_raw: Option<String>,
    is_guaranteed: bool,
    is_winter: bool,
}

#[derive(Clone, Serialize)]
struct Schedule {
    module: &'static str,
    updated: String,
    address: Option<String>,
    garbage: Option<Service>,
    recycling: Option<Service>,
}

#[derive(Default)]
struct Cache {
    schedule: Option<Schedule>,
    last_refresh: Option<String>,
    last_error: Option<String>,
}

struct AppState {
    config: Config,
    client: Client,
    cache: RwLock<Cache>,
}

type SharedState = Arc<AppState>;

fn utc_now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn city_headers(request: RequestBuilder) -> RequestBuilder {
    request
        .header("Referer", REFERER)
        .header("Accept", "application/json, text/plain, */*")
}

async fn geocode_address(client: &Client, address: &str) -> Result<(f64, f64)> {
    let body: Value = client
        .get(CENSUS_URL)
        .query(&[
            ("address", address),
            ("benchmark", "Public_AR_Current"),
            ("format", "json"),
        ])
        .send()
        .await
        .context("Census request failed")?
        .error_for_status()
        .context("Census returned an HTTP error")?
        .json()
        .await
        .context("Census returned invalid JSON")?;
    let coordinates = body
        .pointer("/result/addressMatches/0/coordinates")
        .with_context(|| format!("Could not geocode: {address}"))?;

    let lat = coordinates["y"]
        .as_f64()
        .context("Census response is missing latitude")?;

    let lng = coordinates["x"]
        .as_f64()
        .context("Census response is missing longitude")?;

    Ok((lat, lng))
}

async fn nearest_address(client: &Client, lat: f64, lng: f64) -> Result<Value> {
    let request = client
        .post(TAX_URL)
        .query(&[("method", "getNearestTaxesByPoint")])
        .json(&json!({
            "x": lng,
            "y": lat,
            "spatialReference": 4326,
            "method": "getNearestTaxesByPoint",
        }));

    let body: Value = city_headers(request)
        .send()
        .await
        .context("City geocoder request failed")?
        .error_for_status()
        .context("City geocoder returned an HTTP error")?
        .json()
        .await
        .context("City geocoder returned invalid JSON")?;

    body.pointer("/result/0")
        .filter(|hit| hit.is_object())
        .cloned()
        .context("No address found; are you inside Milwaukee city limits?")
}

fn query_field(hit: &Value, name: &str) -> String {
    match hit.get(name) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.trim().to_owned(),
        Some(value) => value.to_string().trim().to_owned(),
    }
}

async fn fetch_schedule(client: &Client, hit: &Value) -> Result<Value> {
    let params = [
        ("redir", "y".to_owned()),
        ("embed", "y".to_owned()),
        ("method", "na".to_owned()),
        ("laddr", query_field(hit, "HouseNumber")),
        ("sdir", query_field(hit, "Direction")),
        ("sname", query_field(hit, "StreetName")),
        ("stype", query_field(hit, "StreetType")),
        ("faddr", query_field(hit, "Address")),
    ];

    let request = client.get(SCHEDULE_URL).query(&params);

    let body = city_headers(request)
        .send()
        .await
        .context("Schedule request failed")?
        .error_for_status()
        .context("Schedule endpoint returned an HTTP error")?
        .json::<Value>()
        .await
        .context("Schedule endpoint returned invalid JSON")?;

    Ok(body)
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn optional_text(value: &Value) -> Option<String> {
    if !js_truthy(value) {
        return None;
    }

    Some(match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    })
}

fn parse_city_date(input: &str) -> Option<NaiveDate> {
    let input = input.trim();

    let weekdays = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sun",
        "Mon",
        "Tue",
        "Wed",
        "Thu",
        "Fri",
        "Sat",
    ];

    let date_text = match input.split_once(char::is_whitespace) {
        Some((first, rest))
            if weekdays
                .iter()
                .any(|weekday| first.trim_end_matches(',').eq_ignore_ascii_case(weekday)) =>
        {
            rest.trim()
        }
        _ => input,
    };

    let formats = [
        "%B %d, %Y",
        "%B %d %Y",
        "%b %d, %Y",
        "%b %d %Y",
        "%m/%d/%Y",
        "%Y-%m-%d",
    ];

    formats
        .iter()
        .find_map(|format| NaiveDate::parse_from_str(date_text, format).ok())
}

fn normalized_date(text: Option<&str>) -> Option<NaiveDate> {
    text.and_then(|text| {
        let parsed = parse_city_date(text);

        if parsed.is_none() {
            eprintln!("Unrecognized collection date: {text:?}");
        }

        parsed
    })
}
fn normalize_service(value: &Value) -> Option<Service> {
    if !value.is_object() {
        return None;
    }

    let raw = optional_text(&value["date"]);
    let alt_raw = optional_text(&value["alt_date"]);

    let route = if js_truthy(&value["route"]) {
        value["route"].clone()
    } else {
        Value::Null
    };

    Some(Service {
        date: normalized_date(raw.as_deref()),
        raw,
        route,
        alt_date: normalized_date(alt_raw.as_deref()),
        alt_raw,
        is_guaranteed: js_truthy(&value["is_guaranteeed"]),
        is_winter: js_truthy(&value["is_winter"]),
    })
}

fn normalize(raw: &Value, address: Option<String>) -> Schedule {
    Schedule {
        module: "MMM-MilwaukeeTrash",
        updated: utc_now_string(),
        address,
        garbage: normalize_service(&raw["garbage"]),
        recycling: normalize_service(&raw["recycling"]),
    }
}

fn relative_label(days: i64) -> String {
    match days {
        0 => "Today".to_owned(),
        1 => "Tomorrow".to_owned(),
        value if value < 0 => "passed".to_owned(),
        value => format!("in {value} days"),
    }
}

#[derive(Serialize)]
struct Cell {
    color: &'static str,
    value: Value,
}

fn cell(value: impl Into<Value>, color: &'static str) -> Cell {
    Cell {
        color,
        value: value.into(),
    }
}

#[derive(Serialize)]
struct Row {
    icon: Cell,

    #[serde(rename = "type")]
    service_type: Cell,

    day: Cell,
    date: Cell,
    when: Cell,
    route: Cell,

    #[serde(rename = "zf-note", skip_serializing_if = "Option::is_none")]
    note: Option<Cell>,
}

fn to_rows(schedule: Option<&Schedule>, today: NaiveDate) -> Vec<Row> {
    let Some(schedule) = schedule else {
        return Vec::new();
    };

    let mut items = Vec::new();

    for (is_recycling, service) in [
        (false, schedule.garbage.as_ref()),
        (true, schedule.recycling.as_ref()),
    ] {
        if let Some(service) = service {
            if let Some(date) = service.date {
                items.push((date, is_recycling, service));
            }
        }
    }

    items.sort_by_key(|(date, _, _)| *date);

    let has_notes = items.iter().any(|(_, _, service)| {
        service.is_winter
            && service
                .alt_raw
                .as_ref()
                .is_some_and(|text| !text.is_empty())
    });

    items
        .into_iter()
        .map(|(date, is_recycling, service)| {
            let (name, icon, color) = if is_recycling {
                ("Recycling", "fa-recycling", RECYCLE_BLUE)
            } else {
                ("Garbage", "fa-trash", GARBAGE_GREEN)
            };

            let note_text = if service.is_winter {
                service
                    .alt_raw
                    .as_ref()
                    .map(|text| format!("winter alt {text}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };

            let route = if js_truthy(&service.route) {
                service.route.clone()
            } else {
                json!("")
            };

            Row {
                icon: cell(icon, color),
                service_type: cell(name, color),
                day: cell(date.format("%A").to_string(), color),
                date: cell(date.to_string(), color),
                when: cell(relative_label(days_until(date, today)), color),
                route: cell(route, color),
                note: if has_notes {
                    Some(cell(note_text, color))
                } else {
                    None
                },
            }
        })
        .collect()
}

async fn load_schedule(state: &AppState) -> Result<Schedule> {
    let (lat, lng) = match state.config.address.as_deref() {
        Some(address) => geocode_address(&state.client, address).await?,
        None => (state.config.lat, state.config.lng),
    };

    let hit = nearest_address(&state.client, lat, lng).await?;

    let address = optional_text(&hit["Address"]);

    let raw = fetch_schedule(&state.client, &hit).await?;

    Ok(normalize(&raw, address))
}

async fn refresh(state: &AppState) {
    match load_schedule(state).await {
        Ok(schedule) => {
            let refreshed_at = utc_now_string();

            println!(
                "{} refreshed -> {}",
                refreshed_at,
                schedule.address.as_deref().unwrap_or("(unknown)")
            );

            let mut cache = state.cache.write().await;

            cache.schedule = Some(schedule);
            cache.last_refresh = Some(refreshed_at);
            cache.last_error = None;
        }

        Err(error) => {
            let message = format!("{error:#}");

            eprintln!("refresh failed (keeping last good data): {message}");

            let mut cache = state.cache.write().await;

            cache.last_error = Some(message);
            // Don't modify schedule or last_refresh
        }
    }
}

fn next_refresh_at(now: DateTime<Local>) -> Result<DateTime<Local>> {
    let mut date = now.date_naive();

    loop {
        let local_time = date
            .and_hms_opt(REFRESH_HOUR, 0, 0)
            .context("Invalid refresh hour")?;

        let candidate = Local
            .from_local_datetime(&local_time)
            .earliest()
            .context("Refresh time does not exist in the system timezone")?;

        if candidate > now {
            return Ok(candidate);
        }

        date = date
            .succ_opt()
            .context("Could not calculate the next calendar day")?;
    }
}

async fn run_refresh_loop(state: SharedState) {
    refresh(&state).await;

    loop {
        let target = match next_refresh_at(Local::now()) {
            Ok(target) => target,

            Err(error) => {
                eprintln!("could not schedule refresh: {error:#}");
                sleep(Duration::from_secs(60)).await;
                continue;
            }
        };

        println!("next city refresh at {}", target.to_rfc3339());

        loop {
            let remaining = target.signed_duration_since(Local::now());

            if remaining < chrono::TimeDelta::zero() {
                break;
            }

            let delay = remaining
                .to_std()
                .unwrap_or(Duration::ZERO)
                .min(Duration::from_secs(60));

            sleep(delay).await;
        }

        refresh(&state).await;
    }
}

async fn schedule_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let today = Local::now().date_naive();

    let rows = {
        let cache = state.cache.read().await;
        to_rows(cache.schedule.as_ref(), today)
    };

    (
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(rows),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    address: Option<String>,
    last_refresh: Option<String>,
    last_error: Option<String>,
    next_refresh_at: Option<String>,
    refresh_hour: u32,
    rows: Vec<Row>,
}

async fn status_handler(State(state): State<SharedState>) -> Json<StatusResponse> {
    let now = Local::now();

    let next_refresh = next_refresh_at(now.clone()).ok().map(|date| {
        date.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    });

    let cache = state.cache.read().await;

    let address = cache
        .schedule
        .as_ref()
        .and_then(|schedule| schedule.address.clone());

    Json(StatusResponse {
        address,
        last_refresh: cache.last_refresh.clone(),
        last_error: cache.last_error.clone(),
        next_refresh_at: next_refresh,
        refresh_hour: REFRESH_HOUR,
        rows: to_rows(cache.schedule.as_ref(), now.date_naive()),
    })
}

fn days_until(date: NaiveDate, today: NaiveDate) -> i64 {
    (date - today).num_days()
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found")
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env()?;
    let port = config.port;

    let client = Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .context("Could not create HTTP client")?;

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                anyhow!(
                    "Port {port} is already in use; \
                    stop the existing server or choose another PORT"
                )
            } else {
                anyhow!("Could not bind port {port}: {error}")
            }
        })?;
    let state = Arc::new(AppState {
        config,
        client,
        cache: RwLock::new(Cache::default()),
    });

    let app = Router::new()
        .route("/schedule", get(schedule_handler))
        .route("/status", get(status_handler))
        .fallback(not_found)
        .with_state(Arc::clone(&state));

    let refresh_task = tokio::spawn(run_refresh_loop(Arc::clone(&state)));

    println!("Serving http://localhost:{port}/schedule");

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;

    refresh_task.abort();

    result.context("HTTP server failed")?;

    Ok(())
}
