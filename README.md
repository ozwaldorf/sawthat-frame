# SawThat Frame

E-paper display frame for concert/album art data pulled from [sawthat.band](https://sawthat.band).

Built on the [Waveshare ESP32-S3-PhotoPainter](https://www.amazon.com/dp/B0FWRJD8HZ):
- 7.3" Spectra 6 color e-paper display
- ESP32-S3 with wifi and ble
- SDMMC reader with included 16GB sdcard
- GPIO Buttons and LEDs
- (unused) Speaker, microphones

Everything runs on the device: it fetches concerts from SawThat, finds matching album art
on Deezer, renders it for the 6-color display, and caches the result on the SD card.

## Examples

### Physical Device

<img height="300" alt="image" src="https://github.com/user-attachments/assets/1339362c-3e16-41e8-9678-fdbbde66b622" />
<img height="300" alt="image" src="https://github.com/user-attachments/assets/48a749e5-eb84-4dd3-bde5-b50005cf1192" />

### Rendered Artwork

<img width="150" alt="Santana" src="docs/examples/santana_2012_horiz.png" />
<img width="150" alt="Primus" src="docs/examples/primus_2014_horiz.png" />
<img width="150" alt="Billy Strings" src="docs/examples/billy_strings_2017_horiz.png" />
<img width="150" alt="Korn" src="docs/examples/korn_2022_horiz.png" />
<img width="150" alt="GRiZ" src="docs/examples/griz_2022_horiz.png" />
<img width="150" alt="Yonder Mountain" src="docs/examples/yonder_mountain_2024_horiz.png" />
<img width="150" alt="Atmosphere" src="docs/examples/atmosphere_2025_horiz.png" />
<img width="150" alt="Phish" src="docs/examples/phish_2025_horiz.png" />

## Usage

### Requirements

- Rustup
- Espup
- Espflash

A nix dev shell can be used for all required build tools:

```bash
nix develop
```

Install the ESP Rust toolchain using [espup](https://github.com/esp-rs/espup):

```bash
espup install
source ~/export-esp.sh
```

### Configuration

Set WiFi credentials via environment variables:

```bash
export WIFI_SSID="your-ssid"
export WIFI_PASS="your-password"
```

Optional:

| Variable | Default | Purpose |
|----------|---------|---------|
| `SAWTHAT_USER_ID` | built-in user | SawThat user whose concerts are shown |
| `SAWTHAT_FONT` | IBM Plex Mono Bold | Path to a TTF/OTF used for the concert text |

### Build and flash

Flash the firmware to the device and connect to the serial console:

```bash
cd firmware
cargo run --release
```

### Button Controls

The KEY button controls navigation and orientation:

| Action | Duration | Effect |
|--------|----------|--------|
| Tap | < 500ms | Next item |
| Hold | >= 500ms | Toggle orientation (horizontal/vertical) |

Button input is detected in two places:
- **On wake**: Immediately after waking from deep sleep
- **During refresh**: While the e-paper display is refreshing

LED feedback:
- **Green LED**: 1 flash = next item, 3 flashes = orientation changed
- **Red LED**: Solid = idle, blinking = syncing, fast blink = WiFi connecting

### Battery Indicator

The battery level is derived from the measured cell voltage (the PMIC's fuel gauge is
unreliable until it has seen a full charge cycle):

| Level | Fill |
|-------|------|
| 0-15% | Red |
| 16-32% | Dark orange, dithered like the artwork |
| 33-49% | Orange, dithered like the artwork |
| 50-100% | Green |

## Sync

Artwork is rendered on the device and stored on the SD card. Only the current orientation is
rendered; the other orientation is synced the first time it is shown.

A sync runs:
- **On power-on**
- **After every item has been shown** (once per full cycle through the shuffled list)
- **When the orientation is toggled** and no artwork exists for it yet

All other wakes read artwork from the SD card without using WiFi.

While syncing, the frame shows artwork immediately: the next cached screen, or, with an empty
SD card, the first screen as soon as it has been rendered. A "Syncing N concerts..." dialog
with a progress bar is drawn in the corner; only the newly filled slice of the bar is
refreshed as concerts render, and the dialog is erased with a small refresh when done.

If SawThat is unreachable, the last successful response stored on the SD card is used.

### SD Card Layout

```
/concerts/
  BANDS.JSN           # Last SawThat bands response
  HORIZ.JSN           # Items with horizontal artwork
  VERT.JSN            # Items with vertical artwork
  ORIENT.DAT          # Orientation (1 byte: 0=horizontal, 1=vertical)
  horiz/
    {hash}.IMG        # Horizontal artwork (400x480)
  vert/
    {hash}.IMG        # Vertical artwork (480x800)
```

Item paths are `YYYY-MM-DD-{band-id}`. Image filenames are 8-character hex hashes of the item
path (FAT 8.3 compatible). Images are stored as a 4-byte header (width, height) followed by
4-bit palette indices. Files for concerts that are no longer listed are removed after each
sync.

## Specifications

### Firmware Lifecycle

```mermaid
stateDiagram-v2
    PowerOn --> Boot
    Wake --> Boot: 15min timer or button

    state Boot {
        direction LR
        InitSD: Init SD card
        InitSD --> LoadList: Load item list for orientation
    }

    Boot --> NeedSync

    state NeedSync <<choice>>
    NeedSync --> Sync: Power-on, cycle complete,\nor no artwork
    NeedSync --> Display: Otherwise

    state Sync {
        direction LR
        [*] --> Fetch: Fetch concerts
        Fetch --> Show: Show screen + dialog
        Show --> Render: Render missing artwork
        Render --> Erase: Erase dialog
    }

    state Display {
        direction LR
        [*] --> Read: Read artwork from SD
        Read --> Refresh: E-paper refresh
    }

    Sync --> Input
    Display --> Input

    state Input <<choice>>
    Input --> NeedSync: Button tap
    Input --> Orientation: Button hold
    Input --> Sleep: No input

    Orientation --> NeedSync: Toggle horiz/vert
```

### Network Interactions

```mermaid
sequenceDiagram
    participant FW as Firmware
    participant ST as SawThat.band
    participant DZ as Deezer

    FW->>ST: GET /api/bands?id={user_id}
    ST-->>FW: Bands and concerts JSON
    Note over FW: Store response, compute concert list

    loop Each concert without artwork
        FW->>DZ: GET /search/artist?q={band}
        DZ-->>FW: Artist ID
        FW->>DZ: GET /artist/{id}/albums
        DZ-->>FW: Albums with release dates
        FW->>DZ: GET cover (closest album before the concert, sized for the layout)
        DZ-->>FW: Album art JPEG
        Note over FW: Render and store to SD
    end
```

Albums are looked up once per band per sync. If no album precedes the concert, the band's
Spotify picture from SawThat is used instead.

### Image Processing Pipeline

The `render` crate (`no_std`) transforms source images for the 6-color display:

1. **Resize**: Cover-fit with center crop (400x360 horizontal, 480x680 vertical). Deezer covers
   are requested at the covering size so only a crop is needed
2. **Tone adjustments**: Exposure (x0.8), saturation boost (x2.0), and S-curve for mid-tones
3. **Background color**: Dominant color of the bottom 10% of the adjusted image
4. **Canvas composition**: Image area with gradient blend into solid background for text
5. **Dithering**: Floyd-Steinberg error diffusion in OKLab color space to the 6-color palette
6. **Text rendering**: Concert info (band, date, venue) with adaptive font sizing

The same pipeline can be run on a host to preview output:

```bash
cd render
cargo run --release --example render -- cover.jpg "Band" 27-07-2012 "Venue" out
```

## Maintenance

- `embassy-net` is pinned to `=0.8.0` in `firmware/Cargo.toml`. `alloc_tcp_state` in
  `firmware/src/bin/main.rs` allocates the TCP socket buffers in PSRAM as zeroed memory, which
  relies on an all-zero `TcpClientState` matching `TcpClientState::new()`. Check that against the
  new version's source before upgrading.
- The main core needs more than 75KB of stack. Keep large buffers out of internal RAM (`.bss`), or
  the boot will overflow the stack guard.
