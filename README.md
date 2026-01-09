# SawThat Frame

E-paper display frame for concert/album art data pulled from [sawthat.band](https://sawthat.band).

Built on the [Waveshare ESP32-S3-PhotoPainter](https://www.amazon.com/dp/B0FWRJD8HZ):
- 7.3" Spectra 6 color e-paper display
- ESP32-S3 with wifi and ble
- SDMMC reader with included 16GB sdcard
- GPIO Buttons and LEDs
- (unused) Speaker, microphones

## Examples

### Physical Device

<img height="300" alt="image" src="https://github.com/user-attachments/assets/1339362c-3e16-41e8-9678-fdbbde66b622" />
<img height="300" alt="image" src="https://github.com/user-attachments/assets/48a749e5-eb84-4dd3-bde5-b50005cf1192" />

### API Outputs

<img width="200" alt="image" src="https://github.com/user-attachments/assets/fec53b02-4f2d-4364-ad80-8443322e50a5" />
<img width="200" alt="image" src="https://github.com/user-attachments/assets/3dbf4be8-1f17-4b98-9662-7f4bce0b97fc" />
<img width="200" alt="image" src="https://github.com/user-attachments/assets/e2c4833c-266f-46f2-b716-da079fed40f7" />
<img width="200" alt="image" src="https://github.com/user-attachments/assets/b8f79071-ebb0-491a-8b84-938f8ac5e284" />
<img width="200" alt="image" src="https://github.com/user-attachments/assets/9e0b9d28-fed3-40e7-9b57-fdd8d0cd8f3e" />
<img width="200" alt="image" src="https://github.com/user-attachments/assets/48ae1261-95dd-44bf-af56-65e42f56e4da" />
<img width="200" alt="image" src="https://github.com/user-attachments/assets/4260845a-4886-47f9-b77b-dbe3d43cd809" />
<img width="200" alt="image" src="https://github.com/user-attachments/assets/1ebe5b9b-870c-4f64-808d-cf5b9c026a80" />

## Usage

### Requirements

- Rustup
- Espup
- Espflash

A nix dev shell can be used for all required build tools:

```bash
nix develop
```

### Running the server

The server provides the widget API for data fetching and image processing

#### From source

```bash
cd server
PORT=3000 cargo run -r
```

#### Using nix

```bash
nix run .
```

#### NixOS Module

For nixos systems, a module is provided to run the server as a systemd service.
Add the following flake input and NixOS configuration:

```nix
{
  inputs.sawthat-frame.url = "github:ozwaldorf/sawthat-frame";

  outputs = { nixpkgs, sawthat-frame, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        sawthat-frame.nixosModules.default
        {
          services.sawthat-frame-server = {
            enable = true;
            port = 3000;
            openFirewall = true;
            logLevel = "info";
          };
        }
      ];
    };
  };
}
```

### Firmware

The firmware runs on the ESP32-S3 and drives the e-paper display.

#### Prerequisites

Install the ESP Rust toolchain using [espup](https://github.com/esp-rs/espup):

```bash
espup install
source ~/export-esp.sh
```

#### Configuration

Set WiFi credentials and server address via environment variables:

```bash
export WIFI_SSID="your-ssid"
export WIFI_PASS="your-password"
export SERVER_URL="http://192.168.1.42:3000"
```

#### Build and flash

Flash the firmware to the device and connect to the serial console:

```bash
cd firmware
cargo run --release
```

#### Button Controls

The KEY button controls navigation and orientation:

| Action | Duration | Effect |
|--------|----------|--------|
| Tap | >= 50ms | Next item |
| Hold | >= 500ms | Toggle orientation (horizontal/vertical) |

Button input is detected in two places:
- **On wake**: Immediately after waking from deep sleep (button or timer)
- **Post-display**: 10-second window after each display refresh

LED feedback:
- **Green LED**: 1 flash = next item, 3 flashes = orientation changed
- **Red LED**: Solid = idle, blinking = network activity, fast blink = WiFi connecting

### SD Card Cache

The firmware uses an optional SD card for caching. If no SD card is present, the firmware falls back to fetching everything from the network on each boot.

#### Directory Structure

```
/concerts/
  WIDGET.JSN          # JSON array of item paths
  ORIENT.DAT          # Orientation state (1 byte: 0=horizontal, 1=vertical)
  horiz/
    {hash}.PNG        # Horizontal orientation images (400x480 each)
  vert/
    {hash}.PNG        # Vertical orientation images (480x800)
```

Image filenames are 8-character hex hashes of the item path (FAT 8.3 compatible).

#### What Gets Cached

| Data | File | Purpose |
|------|------|---------|
| Widget items | `WIDGET.JSN` | List of concert IDs to display |
| Orientation | `ORIENT.DAT` | Persists orientation across power cycles |
| Images | `horiz/*.PNG`, `vert/*.PNG` | Pre-rendered e-paper images |

#### Cache Behavior

- **Boot**: Load widget data and orientation from SD card if available
- **Cache hit**: Read PNG directly from SD card (skips WiFi entirely)
- **Cache miss**: Fetch from server, store to SD card for next time
- **Background sync**: While display refreshes, fetch fresh widget data and prefetch next image
- **Cleanup**: When widget data changes, stale images are automatically deleted

## Specifications

### Firmware Lifecycle

```mermaid
stateDiagram-v2
    PowerOn --> Boot

    state Boot {
        direction LR
        InitSD: Init SD card
        InitSD --> LoadJson: Load widget.json
    }

    Boot --> CacheCheck: Check image cache

    state CacheCheck <<choice>>
    CacheCheck --> ReadSD: Cached
    CacheCheck --> Init: Not cached

    state Init {
        direction LR
        [*] --> Connect
        Connect --> FetchJson: Fetch widget.json
        FetchJson --> FetchPNG: Fetch PNG
        FetchPNG --> StorePNG: Store to SD
    }

    ReadSD --> Display
    Init --> Display

    state Display {
        [*] --> Refresh: E-paper refresh (~15s)
        [*] --> Sync

        state "Background Sync" as Sync {
            direction LR
            [*] --> WiFi: Connect
            WiFi --> FetchFresh: Fetch fresh data
            FetchFresh --> Update: Update widget.json
            Update --> Cleanup: Remove stale PNGs
            Cleanup --> Prefetch: Prefetch next PNG
        }

        Refresh --> [*]
        Sync --> [*]
    }

    Display --> Input: 10s button wait

    state Input <<choice>>
    Input --> CacheCheck: Button tap
    Input --> Orientation: Button hold
    Input --> Sleep: Timeout

    Orientation --> CacheCheck: Toggle horiz/vert

    Sleep --> Boot: 15min timer or button
```

### Network Interactions

```mermaid
sequenceDiagram
    participant FW as Firmware
    participant API as Server API
    participant ST as SawThat.band
    participant DZ as Deezer

    Note over FW,DZ: Fetch Widget Data
    FW->>API: GET /concerts
    API->>ST: GET /api/bands/{user_id}
    ST-->>API: Band list + concerts JSON
    Note over API: Compute concert list
    API-->>FW: ["2024-06-15-band-id", ...]

    Note over FW,DZ: Fetch Image (cache miss)
    FW->>API: GET /concerts/horiz/2024-06-15-band-id
    API->>DZ: GET /search/artist?q={band}
    DZ-->>API: Artist ID
    API->>DZ: GET /artist/{id}/albums
    DZ-->>API: Album list with dates
    API->>DZ: GET CDN image (closest album)
    DZ-->>API: Album art JPEG
    Note over API: Render widget PNG<br/>(dither, palette, text)
    API-->>FW: PNG image

    Note over FW: Cache and display
```

### Image Processing Pipeline

The server transforms source images into 6-color indexed PNGs for the e-paper display:

1. **Resize**: Cover-fit with center crop (400×360 horizontal, 480×680 vertical)
2. **Tone adjustments**: Exposure (×0.8), saturation boost (×2.0), and S-curve for mid-tones
3. **Canvas composition**: Image area with gradient blend into solid background for text
4. **Dithering**: Floyd-Steinberg error diffusion in OKLab color space to 6-color palette
5. **Text rendering**: Concert info (band, date, venue) with adaptive font sizing
6. **PNG encode**: Indexed color output with embedded palette
