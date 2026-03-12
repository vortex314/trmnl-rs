# trmnl-display

A Rust/Axum HTTP server that renders a **4-week Google Calendar grid** for the
[TRMNL](https://usetrmnl.com) e-ink display. Implements the full **BYOS**
(Bring Your Own Server) firmware protocol so the device talks directly to your
server without going through TRMNL's cloud.

```
Mon     Tue     Wed     Thu     Fri     Sat     Sun
────────────────────────────────────────────────────
 17      18      19      20      21      22      23
         09:00   Dentist          Standup
 24      25      26     [27]      28       1       2
                        09:00                          ← today = filled black
                        Standup
                        14:00 Re…
                        +1
  3       4       5       6       7       8       9
 10      11      12      13      14      15      16
```

The display always shows the **Monday of the current week through 4 full
weeks**, fitting as many event lines per cell as the row height allows.

---

## Endpoints

### BYOS firmware protocol

| Route | Method | Headers required | Purpose |
|---|---|---|---|
| `/api/setup` | `GET` | `ID: <MAC>` | Device first boot — register MAC, get API key |
| `/api/display` | `GET` | `ID: <MAC>`, `Access-Token: <key>` | Every refresh — returns JSON with `image_url` |
| `/api/log` | `POST` | `ID: <MAC>` | Device sends diagnostic logs |
| `/api/image/:filename` | `GET` | — | Serves the actual 1-bit BMP |

### Browser / debug

| Route | Method | Purpose |
|---|---|---|
| `/preview` | `GET` | Grayscale PNG for browser preview |
| `/health` | `GET` | `{"status":"ok"}` |

Both `/preview` and `/api/display` accept `?refresh=true` to force an
immediate Calendar re-fetch.

---

## How the BYOS flow works

```
Device boots
  → GET /api/setup   (header: ID=<MAC>)
  ← { "api_key": "abc123", "image_url": "http://your-server/api/image/…", … }

Every 15 min
  → GET /api/display (headers: ID=<MAC>, Access-Token=<api_key>)
  ← { "image_url": "http://your-server/api/image/calendar_1700000000.bmp",
      "refresh_rate": 900, "update_firmware": false, … }

Device fetches image
  → GET /api/image/calendar_1700000000.bmp
  ← [raw 1-bit BMP bytes, 800×480]

Device logs
  → POST /api/log   (JSON body)
  ← { "status": "ok" }
```

Devices auto-register on first contact — no manual provisioning needed.

---

## Quick start

### 1. Prerequisites

```bash
rustup update stable
```

### 2. Clone & configure

```bash
cp .env.example .env
# Edit .env — at minimum set BASE_URL, GOOGLE_CALENDAR_ID, and credentials
```

### 3. Add fonts

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

Open <http://localhost:3000/preview> in your browser to verify the output.

---

## Google Calendar setup

### Recommended: OAuth2 refresh token (private calendars, auto-renews)

**Step 1 — Create OAuth2 credentials in Google Cloud Console**

1. Go to <https://console.cloud.google.com> → create or select a project
2. **APIs & Services → Library** → enable **Google Calendar API**
3. **APIs & Services → OAuth consent screen**
   - User type: **External**
   - Fill in app name, support email
   - Add scope: `https://www.googleapis.com/auth/calendar.readonly`
   - **Test users** → add your Gmail address
4. **APIs & Services → Credentials → + Create Credentials → OAuth 2.0 Client ID**
   - Application type: **Desktop app**
   - Download the JSON → save as `client_secret.json`

**Step 2 — Obtain a refresh token (once)**

```bash
python3 -m venv /tmp/oauth-venv
/tmp/oauth-venv/bin/pip install google-auth-oauthlib

/tmp/oauth-venv/bin/python3 - <<'EOF'
from google_auth_oauthlib.flow import InstalledAppFlow
flow = InstalledAppFlow.from_client_secrets_file(
    'client_secret.json',
    scopes=['https://www.googleapis.com/auth/calendar.readonly']
)
creds = flow.run_local_server(port=0)
print("GOOGLE_REFRESH_TOKEN =", creds.refresh_token)
print("GOOGLE_CLIENT_ID     =", creds.client_id)
print("GOOGLE_CLIENT_SECRET =", creds.client_secret)
EOF
```

A browser window opens → log in → grant access → the three values are printed.
Paste them into `.env`. The server exchanges the refresh token for a fresh
access token automatically before each Calendar API call — it never expires.

### Alternative: API key (public calendars only)

1. **Credentials → + Create Credentials → API key**
2. Restrict it to the **Google Calendar API**
3. In Google Calendar → calendar settings → **Make available to public**
4. Set `GOOGLE_API_KEY` and `GOOGLE_CALENDAR_ID` in `.env`

> ⚠️ Making your calendar public means anyone with the calendar ID can read it.
> Use the OAuth2 refresh token flow for personal calendars.

---

## Pointing the TRMNL device at your server

During the TRMNL device's WiFi setup (captive portal), there is a field for a
**custom server URL**. Set it to your server's LAN IP or tunnel URL:

```
http://192.168.1.10:3000
```

The firmware automatically appends `/api/setup`, `/api/display`, and
`/api/log` to this base URL. Make sure `BASE_URL` in your `.env` matches
exactly the same address the device uses, because the device will fetch the
BMP from the `image_url` your server returns.

### Exposing your server outside the LAN

If the device needs to reach your server over the internet, use a tunnel:

```bash
# Tailscale (recommended for home use)
tailscale up
# use your Tailscale IP as BASE_URL

# Or cloudflared
cloudflared tunnel --url http://localhost:3000
# use the *.trycloudflare.com URL as BASE_URL
```

---

## Configuration reference

| Variable | Required | Default | Description |
|---|---|---|---|
| `BASE_URL` | ✓ | `http://localhost:3000` | Public URL the device uses to fetch the BMP |
| `GOOGLE_CALENDAR_ID` | ✓ | `primary` | Calendar ID or Gmail address |
| `GOOGLE_REFRESH_TOKEN` | one of | — | OAuth2 refresh token (recommended) |
| `GOOGLE_CLIENT_ID` | with refresh token | — | OAuth2 client ID |
| `GOOGLE_CLIENT_SECRET` | with refresh token | — | OAuth2 client secret |
| `GOOGLE_API_KEY` | one of | — | Simple API key (public calendars only) |
| `GOOGLE_OAUTH_TOKEN` | one of | — | Static Bearer token (expires in ~1h) |
| `REFRESH_RATE_SECS` | — | `900` | Refresh interval sent to device (seconds) |
| `BIND` | — | `0.0.0.0:3000` | Server listen address |
| `TRMNL_API_KEY` | — | — | Optional secret to protect `/api/display` |
| `RUST_LOG` | — | — | Log verbosity, e.g. `trmnl_display=info` |

---

## Architecture

```
main.rs
├── fetch_calendar()         Google Calendar API v3 (4-week window)
├── OAuthTokenManager        Auto-refreshing OAuth2 access token cache
├── render_display()         4-week grid → GrayImage (image + imageproc + ab_glyph)
├── encode_bmp_1bit()        Floyd-Steinberg dithering → 1-bit BMP
│
├── handle_setup()           GET /api/setup      — device registration
├── handle_api_display()     GET /api/display    — JSON with image_url
├── handle_log()             POST /api/log       — device diagnostics
├── handle_image()           GET /api/image/:f   — raw BMP bytes
├── handle_preview()         GET /preview        — PNG for browser
└── handle_health()          GET /health         — liveness check
```

Calendar data is fetched once on startup and then refreshed every 15 minutes
by a background Tokio task. Devices that call `/api/display` with
`?refresh=true` trigger an immediate re-fetch.


# BUILD
```sh
docker build -t trmnl .
docker save trmnl > trmnl-image.tar
```

Deploy via portainer 
- upload image via 'import file'
- create container based on image 
- set network : host
- restart policy : always
