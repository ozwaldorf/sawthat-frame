# Concert and album art display frame

## Hardware details

Waveshare ESP32-S3-PhotoPainter 7.3inch E6 Full Color E-Paper Display with Solid Wood Photo Frame, Ultra-Long Standby, 800 × 480 Resolution, with BAT

https://www.amazon.com/dp/B0FWRJD8HZ

### ESP32-S3 specifications

Module: [ESP32-S3-WROOM-1-N16R8](https://www.espressif.com/sites/default/files/documentation/esp32-s3-wroom-1_wroom-1u_datasheet_en.pdf)

- **CPU**: Dual-core Xtensa LX7 @ 240 MHz
- **Internal SRAM**: 512 KB (data/instructions)
- **RTC Memory**: 8 KB fast + 8 KB slow (retained in deep sleep)
- **PSRAM**: 8 MB (octal SPI, 3 GPIO pins reserved)
- **Flash**: 16 MB (quad SPI)
- **WiFi**: 802.11 b/g/n 2.4 GHz
- **Bluetooth**: v5.0 BLE
- **Battery**: Lithium battery charging via Type-C, sleep current ≤ 1mA

### Onboard peripherals

- **Audio codec**: ES8311 (playback) + ES7210 (microphone) for voice interaction
- **RTC**: PCF85063 real-time clock
- **Sensor**: SHTC3 temperature/humidity
- **Storage**: MicroSD card slot (TF card included)

### E-Paper display (Spectra 6 / E6)

[E Ink Spectra 6](https://www.eink.com/brand/detail/Spectra6) technology:

- **Resolution**: 800 × 480 pixels (127.8 PPI)
- **Active area**: 160.0mm × 96.0mm
- **Pixel pitch**: 0.2mm × 0.2mm
- **Colors**: 6 colors (black, white, red, yellow, blue, green)
- **Color gamut**: 64% wider than previous ACeP generation
- **Refresh time**: ~12 seconds full update
- **Power**: Zero power to maintain image; power only needed during refresh

### Memory constraints

| Resource | Size | Notes |
|----------|------|-------|
| Internal SRAM | 512 KB | Fast access, stack and critical buffers |
| PSRAM | 8 MB | Slower octal SPI, image buffers (7.5 MB if ECC enabled) |
| Flash | 16 MB | Program code + static assets + OTA partition |
| RTC SRAM | 16 KB | Survives deep sleep, useful for state persistence |

**Key limitations:**

- Program binary should stay under ~4 MB to leave room for OTA partition and NVS storage
- WiFi stack consumes ~50-100 KB of RAM when active
- PSRAM access is slower than internal SRAM; avoid tight loops accessing external RAM
- If PSRAM ECC is enabled for higher temperature tolerance (up to 85°C), usable size reduced by 1/16
- Sleep current ≤ 1mA (board-level); ESP32-S3 deep sleep ~10 µA (chip-level)

## Power management

### Battery

Optional 1800 mAh 3.7V Li-Po battery with onboard charging via Type-C.

### Sleep modes

| Mode | Current | PSRAM | Wake sources |
|------|---------|-------|--------------|
| Active | ~100-200 mA | Preserved | n/a |
| Light sleep | 0.7-2 mA | Preserved | Timer, GPIO, UART |
| Deep sleep | ~10 µA | **Lost** | Timer, GPIO, ULP |

Light sleep preserves PSRAM (image cache intact), but real-world current is higher than datasheet due to flash/PSRAM leakage. Enable `CONFIG_ESP_SLEEP_PSRAM_LEAKAGE_WORKAROUND` to reduce.

### Battery life estimates

**Light sleep with periodic refresh:**

| Refresh interval | Avg current | Battery life |
|------------------|-------------|--------------|
| Every 15 min | ~2.5 mA | ~30 days |
| Every hour | ~1.7 mA | ~45 days |
| Every 6 hours | ~1.5 mA | ~50 days |

Assumes per refresh cycle:
- WiFi fetch: ~5 sec @ 150 mA
- Display refresh: ~12 sec @ 30 mA
- Light sleep: remainder @ 1.5 mA

**Deep sleep (cache loss on wake):**

| Refresh interval | Avg current | Battery life |
|------------------|-------------|--------------|
| Every hour | ~50 µA | ~4 years |
| Every 6 hours | ~15 µA | ~13 years |

Deep sleep requires full cache rebuild on wake (WiFi + fetch all images).

### Recommended strategy

Use **light sleep** for this application:
- Image cache preserved across sleep cycles
- 1-2 month battery life with hourly refresh
- No re-download of cached images on wake

For USB-powered (always-on) deployments, sleep is optional.

## Rust stack

### Display driver

epd-waveshare pull request adding support for Spectra E6:
https://github.com/caemor/epd-waveshare/pull/235

### esp-rs ecosystem

This project uses the [esp-rs](https://docs.esp-rs.org/) bare-metal Rust ecosystem:

- **[esp-hal](https://github.com/esp-rs/esp-hal)**: no_std hardware abstraction layer for ESP32-S3 peripherals (SPI, GPIO, timers, PSRAM)
- **[esp-wifi](https://github.com/esp-rs/esp-wifi)**: WiFi driver stack for network connectivity
- **[esp-alloc](https://github.com/esp-rs/esp-alloc)**: Heap allocator for dynamic memory when needed
- **[embedded-graphics](https://github.com/embedded-graphics/embedded-graphics)**: 2D graphics primitives and image rendering
- **[epd-waveshare](https://github.com/caemor/epd-waveshare)**: E-paper display driver (Spectra 6 support in PR)

### Why no_std (bare-metal)?

The esp-rs ecosystem offers two approaches:
- **esp-hal (no_std)**: Direct hardware access, minimal overhead, smaller binaries
- **esp-idf-hal (std)**: Full standard library via ESP-IDF, larger footprint

We use no_std for:
- Smaller memory footprint (critical with 512 KB internal SRAM)
- Predictable timing for display refresh operations
- Lower power consumption
- Faster boot times

### Embedded Rust constraints

| Constraint | Impact |
|------------|--------|
| `#![no_std]` | No filesystem, threads, or dynamic linking; only `core` and `alloc` |
| No unwinding | Panics abort; must use `Result` for error handling |
| Static allocation | Heap available via `esp-alloc` but prefer static buffers |
| Release builds required | Debug builds can be 10-100x slower; timing-sensitive code fails |

**Build considerations:**

- Release builds with LTO typically produce 1-3 MB binaries
- Must use `opt-level = "s"` or `"z"` for size optimization
- Full rebuilds are slow (~2-5 min); incremental builds help during development
- PSRAM must be explicitly initialized and configured in startup code

**Async runtime options:**

- **[embassy](https://embassy.dev/)**: Cooperative async executor, timer/interrupt driven
- **esp-hal async**: Native async support for peripherals (SPI, I2C, etc.)

No OS scheduler overhead; tasks yield at `.await` points.

## Implementation plan

- WiFi connection used for pulling data and images
- Display in 2×1 horizontal layout (landscape 800×480)
- Timer cycles through widget items
- Standby mode during idle periods

## Widget system

The display is driven by **widgets** - generic data sources produced by edge functions. This allows multiple content types (concerts, album art, weather, etc.) to share the same display and caching infrastructure.

### Widget endpoint response

```
GET /api/widget/{widget_name}

Response headers:
  X-Cache-Policy: <max|ttl_seconds|0>
  X-Cache-Key: <u32>

Response body (JSON array):
[
  {
    "width": 1,              // 1 = 400×480 (half), 2 = 800×480 (full)
    "cache_policy": "max",   // "max" | seconds | "0"
    "cache_key": 12345678,   // u32 key for image cache
    "path": "/concerts/abc123"
  },
  ...
]
```

### Cache policies

| Policy | Meaning | Use case |
|--------|---------|----------|
| `max` | Infinite, never expires | Static album art, logos |
| `<seconds>` | TTL in seconds | Dynamic content, weather |
| `0` | No cache, always fetch | Real-time data |

### Image endpoint

```
GET /api/widget/{widget_name}/{path}

Response: PNG with 6-color indexed palette
```

Image dimensions are determined by the `width` field in widget item metadata.

### Cache key lifecycle

Cache keys are **reference counted** by active widget data:

1. On widget data refresh, collect all `cache_key` values from items
2. Any cached image not referenced by any active widget is **deleted**
3. Cache keys that remain valid are reused (no re-fetch needed)

```
Example: Album art widget refresh

Before refresh:
  Widget data: [A, B, C] → cache keys: {101, 102, 103}

After refresh (C removed, D added):
  Widget data: [A, B, D] → cache keys: {101, 102, 104}

  - Key 103 unreferenced → deleted immediately
  - Keys 101, 102 still valid → no fetch
  - Key 104 new → fetch and cache
```

This ensures:
- Removed items don't consume cache space
- Unchanged items don't require re-download
- Cache policy (max/ttl) only applies while item is active

### Widget examples

**Concert widget:**
```json
// Widget header: X-Cache-Policy: 604800 (1 week TTL for the list)
// Album art with concert info, cached indefinitely
[
  {"width": 1, "cache_policy": "max", "cache_key": 1001, "path": "/concert/radiohead-2024-msg"},
  {"width": 1, "cache_policy": "max", "cache_key": 1002, "path": "/concert/beatles-reunion-2024"}
]
```

**Calendar widget:**
```json
// Widget header: X-Cache-Policy: max (list never expires)
// /today refreshes every 6 hours, /next-two-weeks every 24 hours
[
  {"width": 1, "cache_policy": "21600", "cache_key": 3001, "path": "/today"},
  {"width": 2, "cache_policy": "86400", "cache_key": 3002, "path": "/next-two-weeks"}
]
```

## Image processing (edge function)

Widget images are pre-processed server-side:

1. Resize to target dimensions (400×480 or 800×480)
2. Primary color extraction
3. Floyd-Steinberg dithering to 6-color palette
4. Encode as indexed PNG

**6-color palette:**

| Index | Color |
|-------|-------|
| 0 | Black |
| 1 | White |
| 2 | Red |
| 3 | Yellow |
| 4 | Blue |
| 5 | Green |

### Image cache format (indexed PNG)

Images are stored as **PNG with 6-color indexed palette**:

- Standard format with excellent compression (DEFLATE)
- 6-color palette embedded in PNG header
- Dithered patterns compress well due to repetition
- Decoding via `png` or `minipng` crate (no_std compatible)

**Expected sizes for 400×480 dithered images:**

| Content type | Compressed size |
|--------------|-----------------|
| Simple/flat artwork | 6-12 KB |
| Typical album art | 18-30 KB |
| Complex/noisy images | 35-50 KB |
| Worst case (random) | ~72 KB (approaches raw) |

Average expected: **~25 KB per image**

### Image cache capacity

With PNG compression, the device can cache **200+ images** across all widgets.

| PSRAM budget | Reserved | Available | Images @ 25 KB avg |
|--------------|----------|-----------|---------------------|
| 8 MB | 2 MB (WiFi, framebuffer, heap) | 6 MB | **~240 images** |
| Conservative | 3 MB | 5 MB | **~200 images** |

### PNG decoding → framebuffer → refresh

The Spectra 6 display uses a **4-bit packed framebuffer** (2 pixels per byte):

```
Framebuffer layout (800×480):
  - 4 bits per pixel, 2 pixels per byte
  - Row-major order, left to right
  - Size: 800 × 480 / 2 = 192,000 bytes (~188 KB)

Byte layout:
  [pixel_0:3-0][pixel_1:3-0]  (high nibble, low nibble)
```

**Decoding pipeline:**

- PNG (compressed, PSRAM)
- DEFLATE decompress (streaming, row by row)
- Indexed pixels (1 byte per pixel, 0-5 values)
- Pack to 4-bit nibbles (2 pixels → 1 byte)
- Write directly to framebuffer region

```
**Memory-efficient streaming decode:**

1. Allocate single row buffer: `400 bytes` (width=1) or `800 bytes` (width=2)
2. Decode PNG row-by-row using `minipng` streaming API
3. Pack each row to nibbles and write to framebuffer offset
4. No intermediate full-image buffer needed

```rust
// Pseudocode for streaming decode
fn decode_to_framebuffer(png: &[u8], fb: &mut [u8], x_offset: u16, width: u16) {
    let mut decoder = PngDecoder::new(png);
    let mut row_buf = [0u8; 800];  // max row width

    for y in 0..480 {
        decoder.decode_row(&mut row_buf[..width as usize]);

        let fb_row = &mut fb[y * 400..][..width as usize / 2];
        for (i, chunk) in row_buf[..width as usize].chunks(2).enumerate() {
            fb_row[(x_offset as usize / 2) + i] = (chunk[0] << 4) | chunk[1];
        }
    }
}
```

Decode time is mostly negligible compared to the 12-second e-paper refresh.
