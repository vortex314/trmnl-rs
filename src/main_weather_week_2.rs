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
    // Fetch the entire current month
    let month_start = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
    let next_month = if now.month() == 12 {
        chrono::NaiveDate::from_ymd_opt(now.year() + 1, 1, 1).unwrap()
    } else {
        chrono::NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1).unwrap()
    };
    let month_start_utc: chrono::DateTime<Utc> = Utc.from_utc_datetime(&month_start.and_hms_opt(0, 0, 0).unwrap());
    let end: chrono::DateTime<Utc> = Utc.from_utc_datetime(&next_month.and_hms_opt(0, 0, 0).unwrap());

    let time_min = month_start_utc.to_rfc3339();
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
    weather: Option<&WeatherData>,
    font_regular: &FontVec,
    font_bold: &FontVec,
) -> GrayImage {
    let mut img = GrayImage::from_pixel(WIDTH, HEIGHT, WHITE);

    // ── Header bar ──────────────────────────────────────────────────────────
    draw_filled_rect_mut(&mut img, Rect::at(0, 0).of_size(WIDTH, 56), BLACK);

    let now = Local::now();
    let header_text = now.format("%A, %B %-d  ·  %H:%M").to_string();
    draw_text_mut(
        &mut img,
        WHITE,
        20,
        12,
        PxScale::from(30.0),
        font_bold,
        &header_text,
    );

    // ── Divider ──────────────────────────────────────────────────────────────
    draw_line_segment_mut(&mut img, (0.0, 56.0), (WIDTH as f32, 56.0), DARK_GRAY);

    // ── Weather panel (left column, y=60..480) ───────────────────────────────
    let weather_col_w = 280u32;

    if let Some(w) = weather {
        // Temperature
        let temp_str = format!("{:.0}°C", w.temperature_c);
        draw_text_mut(&mut img, BLACK, 20, 70, PxScale::from(72.0), font_bold, &temp_str);

        // Condition
        draw_text_mut(&mut img, BLACK, 20, 148, PxScale::from(24.0), font_regular, &w.condition);

        // "Feels like"
        let feels = format!("Feels {:.0}°C", w.apparent_temp_c);
        draw_text_mut(&mut img, DARK_GRAY, 20, 178, PxScale::from(20.0), font_regular, &feels);

        // Humidity / Wind / Precip details
        let details = [
            format!("💧 {}% humidity", w.humidity),
            format!("💨 {:.0} km/h wind", w.wind_kph),
            format!("☔ {}% precip", w.precip_chance),
        ];
        for (i, line) in details.iter().enumerate() {
            draw_text_mut(
                &mut img,
                BLACK,
                20,
                210 + i as i32 * 30,
                PxScale::from(20.0),
                font_regular,
                line,
            );
        }

        // ── Hourly mini bar chart ────────────────────────────────────────────
        let chart_y_base = 330i32;
        let bar_max_h = 60i32;
        let bar_w = 26u32;
        let gap = 6u32;

        let temps: Vec<f32> = w.hourly.iter().map(|h| h.temp_c).collect();
        let t_min = temps.iter().cloned().fold(f32::INFINITY, f32::min);
        let t_max = temps.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let t_range = (t_max - t_min).max(1.0);

        for (i, h) in w.hourly.iter().enumerate().take(8) {
            let x = (20 + i as u32 * (bar_w + gap)) as i32;
            let bar_h = ((h.temp_c - t_min) / t_range * bar_max_h as f32) as u32 + 6;
            let y = chart_y_base - bar_h as i32;

            let shade = if h.precip_chance > 50 { DARK_GRAY } else { Luma([160u8]) };
            draw_filled_rect_mut(&mut img, Rect::at(x, y).of_size(bar_w, bar_h), shade);

            // Hour label
            let label = format!("{:02}", h.hour);
            draw_text_mut(&mut img, BLACK, x + 2, chart_y_base + 4, PxScale::from(16.0), font_regular, &label);

            // Temp label above bar
            let t_label = format!("{:.0}", h.temp_c);
            draw_text_mut(&mut img, BLACK, x, y - 18, PxScale::from(15.0), font_regular, &t_label);
        }

        // Hourly section label
        draw_text_mut(&mut img, DARK_GRAY, 20, 358, PxScale::from(16.0), font_regular, "Next 8 hours");
    } else {
        draw_text_mut(&mut img, DARK_GRAY, 20, 90, PxScale::from(22.0), font_regular, "Weather unavailable");
    }

    // ── Vertical separator ───────────────────────────────────────────────────
    draw_line_segment_mut(
        &mut img,
        (weather_col_w as f32, 60.0),
        (weather_col_w as f32, HEIGHT as f32),
        LIGHT_GRAY,
    );

    // ── Month calendar grid (right column) ──────────────────────────────────
    let cal_x = weather_col_w as i32 + 16;
    let cal_w  = WIDTH as i32 - weather_col_w as i32 - 16;

    let today        = Local::now().date_naive();
    let month_start  = chrono::NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
    let days_in_month = {
        let next = if today.month() == 12 {
            chrono::NaiveDate::from_ymd_opt(today.year() + 1, 1, 1).unwrap()
        } else {
            chrono::NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1).unwrap()
        };
        (next - month_start).num_days() as u32
    };

    // Month + year title
    let month_title = today.format("%B %Y").to_string();
    draw_text_mut(&mut img, BLACK, cal_x, 62, PxScale::from(22.0), font_bold, &month_title);

    // Day-of-week headers: Mon … Sun
    let dow_labels = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
    let cell_w = cal_w / 7;
    let header_y = 90i32;
    for (i, label) in dow_labels.iter().enumerate() {
        let x = cal_x + i as i32 * cell_w + cell_w / 2 - 8;
        draw_text_mut(&mut img, DARK_GRAY, x, header_y, PxScale::from(15.0), font_bold, label);
    }

    // Separator below headers
    draw_line_segment_mut(
        &mut img,
        (cal_x as f32, 108.0),
        ((cal_x + cal_w) as f32, 108.0),
        LIGHT_GRAY,
    );

    // Build a set of dates that have events
    use std::collections::HashMap as HMap;
    let mut events_by_date: HMap<chrono::NaiveDate, Vec<&CalendarEvent>> = HMap::new();
    for ev in events {
        events_by_date.entry(ev.start.date_naive()).or_default().push(ev);
    }

    // How many week rows does this month need?
    let first_dow   = month_start.weekday().num_days_from_monday() as i32;
    let total_slots = first_dow + days_in_month as i32;
    let num_rows    = (total_slots + 6) / 7; // ceil

    // Divide remaining vertical space evenly across rows
    // Reserve 24px footer + 4px breathing room
    let grid_y0  = 112i32;
    let grid_bot = HEIGHT as i32 - 28;
    let row_h    = (grid_bot - grid_y0) / num_rows;

    // Font sizes that fit inside a cell
    let num_size   = PxScale::from(13.0); // day number
    let ev_size    = PxScale::from(10.0); // event lines
    let ev_line_h  = 11i32;              // pixels per event line
    // How many event lines fit below the day number (2px padding top + 13px num + 2px gap)
    let ev_area_h  = row_h - 17;
    let max_ev_lines = (ev_area_h / ev_line_h).max(0) as usize;

    for day_idx in 0..days_in_month {
        let date   = month_start + chrono::Duration::days(day_idx as i64);
        let col    = (first_dow + day_idx as i32) % 7;
        let row    = (first_dow + day_idx as i32) / 7;

        let cell_x = cal_x + col * cell_w;
        let cell_y = grid_y0 + row * row_h;

        let is_today   = date == today;
        let is_weekend = col >= 5;

        // ── Cell background ──────────────────────────────────────────────────
        if is_today {
            // Filled black for today
            draw_filled_rect_mut(
                &mut img,
                Rect::at(cell_x, cell_y).of_size(cell_w as u32, row_h as u32 - 1),
                BLACK,
            );
        } else if is_weekend {
            // Very subtle gray tint for weekends
            draw_filled_rect_mut(
                &mut img,
                Rect::at(cell_x, cell_y).of_size(cell_w as u32, row_h as u32 - 1),
                Luma([245u8]),
            );
        }

        // ── Cell border ──────────────────────────────────────────────────────
        // Bottom
        draw_line_segment_mut(
            &mut img,
            (cell_x as f32,              (cell_y + row_h - 1) as f32),
            ((cell_x + cell_w) as f32,   (cell_y + row_h - 1) as f32),
            Luma([200u8]),
        );
        // Right (skip last column)
        if col < 6 {
            draw_line_segment_mut(
                &mut img,
                ((cell_x + cell_w - 1) as f32, cell_y as f32),
                ((cell_x + cell_w - 1) as f32, (cell_y + row_h - 1) as f32),
                Luma([200u8]),
            );
        }

        // ── Day number ───────────────────────────────────────────────────────
        let day_str = format!("{}", date.day());
        let num_col = if is_today { WHITE } else if is_weekend { DARK_GRAY } else { BLACK };
        draw_text_mut(&mut img, num_col, cell_x + 2, cell_y + 2, num_size, font_bold, &day_str);

        // ── Event lines ──────────────────────────────────────────────────────
        if let Some(evs) = events_by_date.get(&date) {
            let ev_col   = if is_today { WHITE } else { Luma([30u8]) };
            let more_col = if is_today { Luma([200u8]) } else { DARK_GRAY };

            for (i, ev) in evs.iter().enumerate() {
                if i >= max_ev_lines {
                    // Show "+N more" on the last line
                    let remaining = evs.len() - max_ev_lines + 1;
                    let more = format!("+{} more", remaining);
                    let line_y = cell_y + 17 + (max_ev_lines as i32 - 1) * ev_line_h;
                    draw_text_mut(&mut img, more_col, cell_x + 2, line_y, ev_size, font_regular, &more);
                    break;
                }

                let line_y = cell_y + 17 + i as i32 * ev_line_h;

                // Build label: time prefix + truncated title
                // Available chars ≈ cell_w / ~6px per char at size 10
                let max_chars = ((cell_w - 4) / 6).max(3) as usize;
                let label = if ev.all_day {
                    let t = ev.summary.chars().take(max_chars).collect::<String>();
                    if ev.summary.chars().count() > max_chars { format!("{}…", &t[..t.len().saturating_sub(1)]) } else { t }
                } else {
                    let prefix = ev.start.format("%H:%M ").to_string();
                    let rem = max_chars.saturating_sub(prefix.len());
                    let title: String = ev.summary.chars().take(rem).collect();
                    format!("{}{}", prefix, title)
                };

                draw_text_mut(&mut img, ev_col, cell_x + 2, line_y, ev_size, font_regular, &label);
            }
        }
    }

    // ── Footer ───────────────────────────────────────────────────────────────
    draw_line_segment_mut(&mut img, (0.0, (HEIGHT - 24) as f32), (WIDTH as f32, (HEIGHT - 24) as f32), LIGHT_GRAY);
    let footer = format!("Updated {}", now.format("%H:%M:%S"));
    draw_text_mut(&mut img, DARK_GRAY, 20, (HEIGHT - 20) as i32, PxScale::from(14.0), font_regular, &footer);
    draw_text_mut(&mut img, DARK_GRAY, (WIDTH - 160) as i32, (HEIGHT - 20) as i32, PxScale::from(14.0), font_regular, "trmnl-display v0.1");

    img
}

// ── BMP 1-bit encoder (TRMNL expects raw BMP) ────────────────────────────────

fn encode_bmp_1bit(img: &GrayImage) -> Vec<u8> {
    let w = img.width();
    let h = img.height();

    // Each row padded to 4-byte boundary; 1 bit per pixel
    let row_bytes = ((w + 31) / 32) * 4;
    let pixel_data_size = row_bytes * h;

    let file_size = 62 + pixel_data_size; // 14-byte file header + 40-byte DIB + 8-byte palette

    let mut bmp = Vec::with_capacity(file_size as usize);

    // ── File header ──────────────────────────────────────────────────────────
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
    bmp.extend_from_slice(&62u32.to_le_bytes()); // pixel data offset

    // ── BITMAPINFOHEADER (40 bytes) ──────────────────────────────────────────
    bmp.extend_from_slice(&40u32.to_le_bytes()); // header size
    bmp.extend_from_slice(&(w as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down
    bmp.extend_from_slice(&1u16.to_le_bytes()); // color planes
    bmp.extend_from_slice(&1u16.to_le_bytes()); // bits per pixel
    bmp.extend_from_slice(&0u32.to_le_bytes()); // no compression
    bmp.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
    bmp.extend_from_slice(&2835u32.to_le_bytes()); // x pixels/meter
    bmp.extend_from_slice(&2835u32.to_le_bytes()); // y pixels/meter
    bmp.extend_from_slice(&2u32.to_le_bytes()); // colors in table
    bmp.extend_from_slice(&0u32.to_le_bytes()); // important colors

    // ── Color table: black then white ────────────────────────────────────────
    bmp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // black
    bmp.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]); // white

    // ── Pixel data ───────────────────────────────────────────────────────────
    for y in 0..h {
        let mut col: u32 = 0;
        let mut bit = 7i32;
        let mut byte = 0u8;

        for x in 0..w {
            let luma = img.get_pixel(x, y)[0];
            // threshold at 128: bright → 1 (white in 1-bit BMP palette index 1)
            if luma >= 128 {
                byte |= 1 << bit;
            }
            bit -= 1;
            if bit < 0 {
                bmp.push(byte);
                byte = 0;
                bit = 7;
                col += 1;
            }
        }
        // flush remaining bits in the row
        if bit < 7 {
            bmp.push(byte);
            col += 1;
        }
        // padding to 4-byte row boundary
        while col < row_bytes {
            bmp.push(0);
            col += 1;
        }
    }

    bmp
}

// ── Data refresh helper ───────────────────────────────────────────────────────

async fn refresh_data(state: &AppState) -> Result<()> {
    let events = fetch_calendar(&state.config, state.oauth.as_deref()).await.unwrap_or_else(|e| {
        error!("Calendar fetch error: {e}");
        vec![]
    });

    let weather = fetch_weather(&state.config).await.ok();

    let mut cache = state.cache.write().await;
    cache.events = events;
    cache.weather = weather;
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
        cache.weather.as_ref(),
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
        cache.weather.as_ref(),
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

