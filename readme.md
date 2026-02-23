# trmnl-display

A Rust/Axum HTTP server that renders a **Google Calendar + Weather** dashboard
for the [TRMNL](https://usetrmnl.com) e-ink display, and also serves a PNG
preview for regular browsers.

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Monday, June 9  ·  08:42                                  [header bar] │
├──────────────────────┬──────────────────────────────────────────────────┤
│  22°C                │  UPCOMING EVENTS                                 │
│  Partly Cloudy       │  Today                                           │
│  Feels 20°C          │  09:00  Team standup                             │
│                      │  11:30  Dentist                                   │
│  💧 65% humidity     │  Tomorrow                                        │
│  💨 18 km/h wind     │  08:00  Gym                                       │
│  ☔ 30% precip       │  14:00  Product review                           │
│                      │  ...                                             │
│  [bar chart]         │                                                  │
│  09 10 11 12 13 ...  │                                                  │
└──────────────────────┴──────────────────────────────────────────────────┘
```

## Endpoints

| Endpoint | Format | Purpose |
|---|---|---|
| `GET /display` | `image/bmp` 1-bit | TRMNL firmware |
| `GET /preview` | `image/png` gray | Browser preview |
| `GET /health`  | `text/plain`      | Health check |

Both image endpoints accept `?refresh=true` to force a data re-fetch.

## Quick start

### 1. Prerequisites

```bash
rustup update stable
```

### 2. Clone & configure

```bash
cp .env.example .env
# Edit .env with your credentials
```

### 3. Add fonts

Create an `assets/` directory next to the binary and drop in two TTF files:

```bash
mkdir assets
# DejaVu Sans (freely licensed)
curl -L "https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/dejavu-fonts-ttf-2.37.tar.bz2" \
  | tar xj --strip-components=2 -C assets/ --wildcards '*/DejaVuSans.ttf' '*/DejaVuSans-Bold.ttf'
mv assets/DejaVuSans.ttf      assets/font-regular.ttf
mv assets/DejaVuSans-Bold.ttf assets/font-bold.ttf
```

### 4. Build & run

```bash
cargo run --release
```

Open <http://localhost:3000/preview> in your browser to see the PNG output.

## Google Calendar setup

### API Key (simplest, public/own calendars)

1. Go to <https://console.cloud.google.com>
2. Create a project → Enable **Google Calendar API**
3. Credentials → Create **API key**
4. In Calendar settings, share the calendar publicly (or keep it private and use OAuth)
5. Set `GOOGLE_API_KEY` and `GOOGLE_CALENDAR_ID` in `.env`

### OAuth2 token (private calendars)

Use `gcloud` or the OAuth playground to obtain a token:

```bash
gcloud auth print-access-token
```

Set `GOOGLE_OAUTH_TOKEN` in `.env` (refresh it periodically or wire up a token-refresh loop).

## TRMNL firmware config

In the TRMNL web dashboard, add a **Custom Plugin** with:

- **Strategy**: Polling
- **URL**: `http://your-server:3000/display`
- **Refresh interval**: 15 min (matches the server's background refresh)
- **Headers**: `X-TRMNL-Api-Key: your_secret_key` (if `TRMNL_API_KEY` is set)

## Configuration reference

| Variable | Required | Default | Description |
|---|---|---|---|
| `GOOGLE_CALENDAR_ID` | ✓ | `primary` | Calendar ID or email |
| `GOOGLE_API_KEY` | one of | — | Simple API key |
| `GOOGLE_OAUTH_TOKEN` | one of | — | OAuth Bearer token |
| `WEATHER_LAT` | ✓ | `48.8566` | Latitude |
| `WEATHER_LON` | ✓ | `2.3522` | Longitude |
| `BIND` | — | `0.0.0.0:3000` | Listen address |
| `TRMNL_API_KEY` | — | — | Protects `/display` |
| `RUST_LOG` | — | — | Log verbosity |

## Architecture

```
main.rs
├── fetch_calendar()      Google Calendar API v3
├── fetch_weather()       Open-Meteo (free, no API key)
├── render_display()      image + imageproc + rusttype
├── encode_bmp_1bit()     hand-rolled 1-bit BMP encoder
├── handle_display()      → BMP response
└── handle_preview()      → PNG response
```

Data is cached in memory and refreshed every 15 minutes by a background
Tokio task. Both endpoints accept `?refresh=true` to force immediate re-fetch.