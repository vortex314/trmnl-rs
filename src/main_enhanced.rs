// ─────────────────────────────────────────────────────────────────────────────
// trmnl-display  –  Axum HTTP server for TRMNL e-ink + browser PNG preview
//
// Environment variables required:
//   GOOGLE_CALENDAR_ID       – e.g. "primary" or full calendar ID
//   GOOGLE_API_KEY           – server-key with Calendar API enabled
//                              (or use GOOGLE_OAUTH_TOKEN for OAuth)
//   WEATHER_LAT / WEATHER_LON – decimal coordinates for weather
//   TRMNL_API_KEY            – optional: protect the /display route
//
// Endpoints:
//   GET /display          → BMP (1-bit) for TRMNL firmware
//   GET /preview          → PNG (grayscale) for browser preview
//   GET /health           → 200 OK
// ─────────────────────────────────────────────────────────────────────────────

use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone, Utc};
use image::{GrayImage, Luma, RgbaImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_line_segment_mut, draw_text_mut};
use imageproc::rect::Rect;
use ab_glyph::{FontVec, PxScale};
use serde::Deserialize;
use std::{collections::HashMap, env, sync::Arc};
use tokio::sync::RwLock;
use tracing::{error, info};

// ── Display constants ────────────────────────────────────────────────────────

/// TRMNL 7.5" display resolution
const WIDTH: u32 = 800;
const HEIGHT: u32 = 480;

const WHITE: Luma<u8> = Luma([255]);
const BLACK: Luma<u8> = Luma([0]);
const LIGHT_GRAY: Luma<u8> = Luma([200]);
const DARK_GRAY: Luma<u8> = Luma([80]);

// ── Data models ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CalendarEvent {
    summary: String,
    start: DateTime<Local>,
    end: DateTime<Local>,
    all_day: bool,
}

#[derive(Debug, Clone)]
struct WeatherData {
    temperature_c: f32,
    apparent_temp_c: f32,
    condition: String,
    condition_icon: &'static str, // ASCII art for e-ink
    humidity: u8,
    wind_kph: f32,
    precip_chance: u8,
    hourly: Vec<HourlyForecast>,
}

#[derive(Debug, Clone)]
struct HourlyForecast {
    hour: u8,
    temp_c: f32,
    precip_chance: u8,
}

// ── App state with simple cache ───────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    oauth: Option<Arc<OAuthTokenManager>>,
    cache: Arc<RwLock<DisplayCache>>,
    font_regular: Arc<FontVec>,
    font_bold: Arc<FontVec>,
}

struct Config {
    calendar_id: String,
    google_api_key: Option<String>,
    /// Static OAuth token (no auto-refresh, expires ~1h)
    google_oauth_token: Option<String>,
    /// OAuth2 refresh-token credentials (auto-refresh, never expires)
    google_refresh_token: Option<String>,
    google_client_id: Option<String>,
    google_client_secret: Option<String>,
    weather_lat: f64,
    weather_lon: f64,
    trmnl_api_key: Option<String>,
}

// ── OAuth2 token manager (auto-refresh) ──────────────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

/// Holds a cached access token and refreshes it automatically when it expires.
struct OAuthTokenManager {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    http: reqwest::Client,
    cached: tokio::sync::Mutex<Option<(String, std::time::Instant)>>,
}

impl OAuthTokenManager {
    fn new(client_id: String, client_secret: String, refresh_token: String) -> Self {
        Self {
            client_id,
            client_secret,
            refresh_token,
            http: reqwest::Client::new(),
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// Returns a valid access token, refreshing from Google if needed.
    async fn access_token(&self) -> Result<String> {
        let mut lock = self.cached.lock().await;

        // Return cached token if it has more than 60s remaining
        if let Some((token, expires_at)) = lock.as_ref() {
            if expires_at.saturating_duration_since(std::time::Instant::now()).as_secs() > 60 {
                return Ok(token.clone());
            }
        }

        info!("OAuth2: refreshing access token");

        // Build application/x-www-form-urlencoded body manually (no reqwest feature needed)
        let body = format!(
            "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
            urlencoding::encode(&self.client_id),
            urlencoding::encode(&self.client_secret),
            urlencoding::encode(&self.refresh_token),
        );
        let resp = self.http
            .post("https://oauth2.googleapis.com/token")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("OAuth2 token request failed")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OAuth2 token refresh failed: {body}");
        }

        let token_resp: TokenResponse = resp.json().await
            .context("Parsing OAuth2 token response")?;

        let expires_at = std::time::Instant::now()
            + std::time::Duration::from_secs(token_resp.expires_in);

        *lock = Some((token_resp.access_token.clone(), expires_at));
        info!("OAuth2: new access token valid for {}s", token_resp.expires_in);

        Ok(token_resp.access_token)
    }
}

#[derive(Default)]
struct DisplayCache {
    events: Vec<CalendarEvent>,
    weather: Option<WeatherData>,
    last_updated: Option<chrono::DateTime<Utc>>,
}

// ── Google Calendar API ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GCalResponse {
    items: Vec<GCalItem>,
}

#[derive(Deserialize)]
struct GCalItem {
    summary: Option<String>,
    start: GCalDateTime,
    end: GCalDateTime,
}

#[derive(Deserialize)]
struct GCalDateTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date: Option<String>,
}

async fn fetch_calendar(config: &Config, oauth: Option<&OAuthTokenManager>) -> Result<Vec<CalendarEvent>> {
    let client = reqwest::Client::new();
    let now = Utc::now();
    // Fetch 4 weeks starting from today
    let end = now + chrono::Duration::weeks(4);

    let time_min = now.to_rfc3339();
    let time_max = end.to_rfc3339();

    let url = format!(
        "https://www.googleapis.com/calendar/v3/calendars/{}/events",
        urlencoding::encode(&config.calendar_id)
    );

    let mut req = client
        .get(&url)
        .query(&[
            ("timeMin", time_min.as_str()),
            ("timeMax", time_max.as_str()),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", "100"),
        ]);

    if let Some(key) = &config.google_api_key {
        req = req.query(&[("key", key.as_str())]);
    } else if let Some(mgr) = oauth {
        // Auto-refreshing OAuth2 token
        let token = mgr.access_token().await.context("Getting OAuth2 access token")?;
        req = req.bearer_auth(token);
    } else if let Some(token) = &config.google_oauth_token {
        // Static token fallback (expires in ~1h)
        req = req.bearer_auth(token);
    }

    let resp: GCalResponse = req
        .send()
        .await
        .context("Google Calendar request failed")?
        .json()
        .await
        .context("Parsing Calendar response")?;

    let events = resp
        .items
        .into_iter()
        .filter_map(|item| {
            let summary = item.summary.unwrap_or_else(|| "(No title)".into());

            if let Some(dt_str) = &item.start.date_time {
                // Timed event
                let start = DateTime::parse_from_rfc3339(dt_str).ok()?.with_timezone(&Local);
                let end_str = item.end.date_time.as_deref()?;
                let end = DateTime::parse_from_rfc3339(end_str).ok()?.with_timezone(&Local);
                Some(CalendarEvent { summary, start, end, all_day: false })
            } else if let Some(date_str) = &item.start.date {
                // All-day event
                let naive = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
                let start = Local.from_local_datetime(&naive.and_hms_opt(0, 0, 0)?).single()?;
                let end = start + chrono::Duration::days(1);
                Some(CalendarEvent { summary, start, end, all_day: true })
            } else {
                None
            }
        })
        .collect();

    Ok(events)
}

// ── Open-Meteo weather (free, no key) ────────────────────────────────────────

#[derive(Deserialize)]
struct MeteoResponse {
    current: MeteoCurrentValues,
    #[serde(rename = "hourly")]
    hourly: MeteoHourly,
}

#[derive(Deserialize)]
struct MeteoCurrentValues {
    temperature_2m: f32,
    apparent_temperature: f32,
    relative_humidity_2m: u8,
    wind_speed_10m: f32,
    weather_code: u8,
}

#[derive(Deserialize)]
struct MeteoHourly {
    time: Vec<String>,
    temperature_2m: Vec<f32>,
    precipitation_probability: Vec<u8>,
}

fn wmo_to_condition(code: u8) -> (&'static str, &'static str) {
    match code {
        0 => ("Clear", "☀"),
        1..=3 => ("Partly Cloudy", "⛅"),
        45 | 48 => ("Fog", "🌫"),
        51..=55 => ("Drizzle", "🌦"),
        61..=65 => ("Rain", "🌧"),
        71..=75 => ("Snow", "❄"),
        80..=82 => ("Showers", "🌦"),
        95 => ("Thunderstorm", "⛈"),
        _ => ("Unknown", "?"),
    }
}

async fn fetch_weather(config: &Config) -> Result<WeatherData> {
    let client = reqwest::Client::new();
    let url = "https://api.open-meteo.com/v1/forecast";

    let resp: MeteoResponse = client
        .get(url)
        .query(&[
            ("latitude", config.weather_lat.to_string()),
            ("longitude", config.weather_lon.to_string()),
            ("current", "temperature_2m,apparent_temperature,relative_humidity_2m,wind_speed_10m,weather_code".to_string()),
            ("hourly", "temperature_2m,precipitation_probability".to_string()),
            ("forecast_days", "1".to_string()),
            ("wind_speed_unit", "kmh".to_string()),
        ])
        .send()
        .await?
        .json()
        .await?;

    let (condition, icon) = wmo_to_condition(resp.current.weather_code);

    // Pick next 8 hourly slots from now
    let now_h = Local::now().hour() as usize;
    let hourly: Vec<HourlyForecast> = resp
        .hourly
        .time
        .iter()
        .zip(
            resp.hourly.temperature_2m.iter()
                .zip(resp.hourly.precipitation_probability.iter()),
        )
        .enumerate()
        .filter(|(i, _)| *i >= now_h && *i < now_h + 8)
        .map(|(i, (_, (temp, precip)))| HourlyForecast {
            hour: (i % 24) as u8,
            temp_c: *temp,
            precip_chance: *precip,
        })
        .collect();

    // Rough precip chance from hourly median
    let precip_chance = if hourly.is_empty() {
        0
    } else {
        let sum: u16 = hourly.iter().map(|h| h.precip_chance as u16).sum();
        (sum / hourly.len() as u16) as u8
    };

    Ok(WeatherData {
        temperature_c: resp.current.temperature_2m,
        apparent_temp_c: resp.current.apparent_temperature,
        condition: condition.to_string(),
        condition_icon: icon,
        humidity: resp.current.relative_humidity_2m,
        wind_kph: resp.current.wind_speed_10m,
        precip_chance,
        hourly,
    })
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn render_display(
    events: &[CalendarEvent],
    font_regular: &FontVec,
    font_bold: &FontVec,
) -> GrayImage {
    let mut img = GrayImage::from_pixel(WIDTH, HEIGHT, WHITE);
    let now = Local::now();

    // ── Header bar ──────────────────────────────────────────────────────────
    // week_start: Monday of the current week (needed for header + grid)
    let today_pre   = now.date_naive();
    let week_start  = today_pre - chrono::Duration::days(today_pre.weekday().num_days_from_monday() as i64);
    draw_filled_rect_mut(&mut img, Rect::at(0, 0).of_size(WIDTH, 48), BLACK);
    let range_end   = week_start + chrono::Duration::days(27);
    let header_text = format!(
        "{}  –  {}   ·   {}",
        week_start.format("%d %b"),
        range_end.format("%d %b %Y"),
        now.format("%H:%M"),
    );
    draw_text_mut(&mut img, WHITE, 16, 12, PxScale::from(26.0), font_bold, &header_text);

    // ── Day-of-week column headers ───────────────────────────────────────────
    let dow_labels = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let cell_w = WIDTH as i32 / 7;
    let header_y = 52i32;
    for (i, label) in dow_labels.iter().enumerate() {
        let is_weekend = i >= 5;
        let col_x = i as i32 * cell_w + cell_w / 2 - 12;
        let col  = if is_weekend { DARK_GRAY } else { BLACK };
        draw_text_mut(&mut img, col, col_x, header_y, PxScale::from(15.0), font_bold, label);
    }

    // Separator
    draw_line_segment_mut(&mut img, (0.0, 70.0), (WIDTH as f32, 70.0), DARK_GRAY);

    // ── Build events-by-date map ─────────────────────────────────────────────
    use std::collections::HashMap as HMap;
    let mut events_by_date: HMap<chrono::NaiveDate, Vec<&CalendarEvent>> = HMap::new();
    for ev in events {
        events_by_date.entry(ev.start.date_naive()).or_default().push(ev);
    }

    // ── 4-week grid geometry (28 days, always 4 rows) ──────────────────────
    let today      = now.date_naive();
    let num_rows   = 4i32;
    let total_days = 28u32;

    let grid_y0  = 72i32;
    let grid_bot = HEIGHT as i32 - 20; // leave room for footer
    let row_h    = (grid_bot - grid_y0) / num_rows;

    // Font sizes
    let num_size  = PxScale::from(13.0);
    let ev_size   = PxScale::from(10.0);
    let ev_line_h = 12i32;
    let max_ev_lines = ((row_h - 18) / ev_line_h).max(0) as usize;

    // ── Draw cells ───────────────────────────────────────────────────────────
    for day_idx in 0..total_days {
        let date   = week_start + chrono::Duration::days(day_idx as i64);
        let col    = (day_idx % 7) as i32;
        let row    = (day_idx / 7) as i32;

        let cell_x = col * cell_w;
        let cell_y = grid_y0 + row * row_h;

        let is_today   = date == today;
        let is_weekend = col >= 5;

        // Background
        if is_today {
            draw_filled_rect_mut(&mut img,
                Rect::at(cell_x, cell_y).of_size(cell_w as u32, row_h as u32 - 1),
                BLACK);
        } else if is_weekend {
            draw_filled_rect_mut(&mut img,
                Rect::at(cell_x, cell_y).of_size(cell_w as u32, row_h as u32 - 1),
                Luma([242u8]));
        }

        // Grid lines
        draw_line_segment_mut(&mut img,
            (cell_x as f32,            (cell_y + row_h - 1) as f32),
            ((cell_x + cell_w) as f32, (cell_y + row_h - 1) as f32),
            Luma([195u8]));
        if col < 6 {
            draw_line_segment_mut(&mut img,
                ((cell_x + cell_w - 1) as f32, cell_y as f32),
                ((cell_x + cell_w - 1) as f32, (cell_y + row_h - 1) as f32),
                Luma([195u8]));
        }

        // Day number
        let day_str = format!("{}", date.day());
        let num_col = if is_today { WHITE } else if is_weekend { DARK_GRAY } else { BLACK };
        draw_text_mut(&mut img, num_col, cell_x + 3, cell_y + 2, num_size, font_bold, &day_str);

        // Events
        if let Some(evs) = events_by_date.get(&date) {
            let ev_col   = if is_today { WHITE       } else { Luma([20u8])  };
            let more_col = if is_today { Luma([180u8])} else { DARK_GRAY    };

            for (i, ev) in evs.iter().enumerate() {
                if i >= max_ev_lines {
                    let remaining = evs.len() - max_ev_lines + 1;
                    let more  = format!("+{}", remaining);
                    let line_y = cell_y + 17 + (max_ev_lines as i32 - 1) * ev_line_h;
                    draw_text_mut(&mut img, more_col, cell_x + 3, line_y, ev_size, font_regular, &more);
                    break;
                }

                let line_y    = cell_y + 17 + i as i32 * ev_line_h;
                let max_chars = ((cell_w - 5) / 6).max(3) as usize;

                let label = if ev.all_day {
                    let t: String = ev.summary.chars().take(max_chars).collect();
                    if ev.summary.chars().count() > max_chars {
                        format!("{}…", t.chars().take(t.len().saturating_sub(1)).collect::<String>())
                    } else { t }
                } else {
                    let prefix = ev.start.format("%H:%M ").to_string();
                    let rem    = max_chars.saturating_sub(prefix.len());
                    format!("{}{}", prefix, ev.summary.chars().take(rem).collect::<String>())
                };

                draw_text_mut(&mut img, ev_col, cell_x + 3, line_y, ev_size, font_regular, &label);
            }
        }
    }

    // ── Footer ───────────────────────────────────────────────────────────────
    draw_line_segment_mut(&mut img, (0.0, (HEIGHT - 18) as f32), (WIDTH as f32, (HEIGHT - 18) as f32), LIGHT_GRAY);
    let footer = format!("Updated {}", now.format("%H:%M:%S"));
    draw_text_mut(&mut img, DARK_GRAY, 8, (HEIGHT - 15) as i32, PxScale::from(12.0), font_regular, &footer);

    img
}
// ── BMP 1-bit encoder (TRMNL expects raw BMP) ────────────────────────────────

fn encode_bmp_1bit(img: &GrayImage) -> Vec<u8> {
    let w = img.width() as usize;
    let h = img.height() as usize;

    // ── Floyd-Steinberg dithering ────────────────────────────────────────────
    // Work in i32 to handle error accumulation without overflow.
    let mut pixels: Vec<i32> = img.pixels().map(|p| p[0] as i32).collect();

    for y in 0..h {
        for x in 0..w {
            let old_val = pixels[y * w + x].clamp(0, 255);
            let new_val = if old_val >= 128 { 255 } else { 0 };
            let err     = old_val - new_val;
            pixels[y * w + x] = new_val;

            // Distribute error to neighbours (standard FS coefficients)
            if x + 1 < w {
                pixels[y * w + x + 1]         += err * 7 / 16;
            }
            if y + 1 < h {
                if x > 0 {
                    pixels[(y+1) * w + x - 1] += err * 3 / 16;
                }
                pixels[(y+1) * w + x]         += err * 5 / 16;
                if x + 1 < w {
                    pixels[(y+1) * w + x + 1] += err * 1 / 16;
                }
            }
        }
    }

    // ── BMP file layout ──────────────────────────────────────────────────────
    let row_bytes       = ((w + 31) / 32) * 4; // 4-byte aligned
    let pixel_data_size = row_bytes * h;
    let file_size       = 62 + pixel_data_size;

    let mut bmp = Vec::with_capacity(file_size);

    // File header (14 bytes)
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());          // reserved
    bmp.extend_from_slice(&62u32.to_le_bytes());         // pixel data offset

    // BITMAPINFOHEADER (40 bytes)
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(w as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down scan
    bmp.extend_from_slice(&1u16.to_le_bytes());          // color planes
    bmp.extend_from_slice(&1u16.to_le_bytes());          // bits per pixel
    bmp.extend_from_slice(&0u32.to_le_bytes());          // no compression
    bmp.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
    bmp.extend_from_slice(&2835u32.to_le_bytes());       // ~72 dpi x
    bmp.extend_from_slice(&2835u32.to_le_bytes());       // ~72 dpi y
    bmp.extend_from_slice(&2u32.to_le_bytes());          // colors in table
    bmp.extend_from_slice(&0u32.to_le_bytes());          // important colors

    // Color table: index 0 = black, index 1 = white
    bmp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    bmp.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);

    // Pixel data
    for y in 0..h {
        let mut col = 0usize;
        let mut bit = 7i32;
        let mut byte = 0u8;

        for x in 0..w {
            if pixels[y * w + x] >= 128 {
                byte |= 1 << bit; // bright → palette index 1 (white)
            }
            bit -= 1;
            if bit < 0 {
                bmp.push(byte);
                byte = 0;
                bit  = 7;
                col += 1;
            }
        }
        if bit < 7 { bmp.push(byte); col += 1; }      // flush partial byte
        while col < row_bytes { bmp.push(0); col += 1; } // row padding
    }

    bmp
}

// ── Data refresh helper ───────────────────────────────────────────────────────

async fn refresh_data(state: &AppState) -> Result<()> {
    let events = fetch_calendar(&state.config, state.oauth.as_deref()).await.unwrap_or_else(|e| {
        error!("Calendar fetch error: {e}");
        vec![]
    });

    let mut cache = state.cache.write().await;
    cache.events = events;
    cache.last_updated = Some(Utc::now());
    Ok(())
}

// ── Request query params ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DisplayQuery {
    refresh: Option<bool>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn auth_check(headers: &HeaderMap, config: &Config) -> bool {
    if let Some(expected) = &config.trmnl_api_key {
        let ok_bearer = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_start_matches("Bearer ") == expected)
            .unwrap_or(false);

        let ok_key = headers
            .get("X-TRMNL-Api-Key")
            .and_then(|v| v.to_str().ok())
            .map(|s| s == expected)
            .unwrap_or(false);

        ok_bearer || ok_key
    } else {
        true // no key configured → open
    }
}

/// BMP endpoint for TRMNL firmware
async fn handle_display(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DisplayQuery>,
) -> Response {
    if !auth_check(&headers, &state.config).await {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let needs_refresh = q.refresh.unwrap_or(false)
        || state.cache.read().await.last_updated.is_none();

    if needs_refresh {
        if let Err(e) = refresh_data(&state).await {
            error!("Refresh failed: {e}");
        }
    }

    let cache = state.cache.read().await;
    let img = render_display(
        &cache.events,
        &state.font_regular,
        &state.font_bold,
    );
    drop(cache);

    let bmp = encode_bmp_1bit(&img);

    (
        StatusCode::OK,
        [
            ("Content-Type", "image/bmp"),
            ("Cache-Control", "no-store"),
        ],
        bmp,
    )
        .into_response()
}

/// PNG endpoint for browser preview
async fn handle_preview(
    State(state): State<AppState>,
    Query(q): Query<DisplayQuery>,
) -> Response {
    let needs_refresh = q.refresh.unwrap_or(false)
        || state.cache.read().await.last_updated.is_none();

    if needs_refresh {
        if let Err(e) = refresh_data(&state).await {
            error!("Refresh failed: {e}");
        }
    }

    let cache = state.cache.read().await;
    let img = render_display(
        &cache.events,
        &state.font_regular,
        &state.font_bold,
    );
    drop(cache);

    let mut png_bytes: Vec<u8> = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut png_bytes);
        img.write_to(&mut cursor, image::ImageFormat::Png)
            .expect("PNG encode failed");
    }

    (
        StatusCode::OK,
        [
            ("Content-Type", "image/png"),
            ("Cache-Control", "no-store"),
        ],
        png_bytes,
    )
        .into_response()
}

async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

// ── Font loading ──────────────────────────────────────────────────────────────

fn load_fonts() -> (FontVec, FontVec) {
    macro_rules! maybe_load {
        ($path:expr) => {
            std::fs::read($path)
                .ok()
                .and_then(|b| FontVec::try_from_vec(b).ok())
        };
    }

    let regular = maybe_load!("assets/font-regular.ttf")
        .expect("Missing assets/font-regular.ttf – copy a TTF there");

    let bold = maybe_load!("assets/font-bold.ttf")
        .expect("Missing assets/font-bold.ttf – copy a TTF there");

    (regular, bold)
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config {
        calendar_id: env::var("GOOGLE_CALENDAR_ID").unwrap_or_else(|_| "primary".into()),
        google_api_key: env::var("GOOGLE_API_KEY").ok(),
        google_oauth_token: env::var("GOOGLE_OAUTH_TOKEN").ok(),
        google_refresh_token: env::var("GOOGLE_REFRESH_TOKEN").ok(),
        google_client_id: env::var("GOOGLE_CLIENT_ID").ok(),
        google_client_secret: env::var("GOOGLE_CLIENT_SECRET").ok(),
        weather_lat: env::var("WEATHER_LAT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(48.8566), // Paris as default
        weather_lon: env::var("WEATHER_LON")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2.3522),
        trmnl_api_key: env::var("TRMNL_API_KEY").ok(),
    };

    let (font_regular, font_bold) = load_fonts();

    // Build OAuth token manager if refresh-token credentials are present
    let oauth: Option<Arc<OAuthTokenManager>> = match (
        env::var("GOOGLE_REFRESH_TOKEN").ok(),
        env::var("GOOGLE_CLIENT_ID").ok(),
        env::var("GOOGLE_CLIENT_SECRET").ok(),
    ) {
        (Some(rt), Some(cid), Some(cs)) => {
            info!("OAuth2: using refresh-token flow");
            Some(Arc::new(OAuthTokenManager::new(cid, cs, rt)))
        }
        _ => {
            if env::var("GOOGLE_API_KEY").is_err() && env::var("GOOGLE_OAUTH_TOKEN").is_err() {
                tracing::warn!("No Google credentials found – calendar will be empty");
            }
            None
        }
    };

    let state = AppState {
        config: Arc::new(config),
        oauth,
        cache: Arc::new(RwLock::new(DisplayCache::default())),
        font_regular: Arc::new(font_regular),
        font_bold: Arc::new(font_bold),
    };

    // Background refresh every 15 minutes
    {
        let s = state.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = refresh_data(&s).await {
                    error!("Background refresh error: {e}");
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(900)).await;
            }
        });
    }

    let app = Router::new()
        .route("/display", get(handle_display))
        .route("/preview", get(handle_preview))
        .route("/health", get(handle_health))
        .with_state(state);

    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:3000".into());
    info!("Listening on http://{bind}");
    info!("  BMP  → http://{bind}/display   (TRMNL firmware)");
    info!("  PNG  → http://{bind}/preview   (browser preview)");

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── urlencoding helper (avoid extra dep) ─────────────────────────────────────
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
}

use chrono::Timelike;