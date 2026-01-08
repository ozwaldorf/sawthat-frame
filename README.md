# SawThat Frame

E-paper display frame for concert/album art, powered by an ESP32-S3 with a 7.3" Spectra 6 full-color display.

## Hardware

Built on the [Waveshare ESP32-S3-PhotoPainter](https://www.amazon.com/dp/B0FWRJD8HZ) - a prebuilt 7.3" E6 full-color e-paper display with a solid wood frame.

### Display

[E Ink Spectra 6](https://www.eink.com/brand/detail/Spectra6) technology:

| Spec | Value |
|------|-------|
| Resolution | 800 x 480 (127.8 PPI) |
| Colors | 6 (black, white, red, yellow, blue, green) |
| Active area | 160mm x 96mm |
| Refresh time | ~12 seconds |
| Power | Zero power to maintain image |

### Memory

| Type | Size | Notes |
|------|------|-------|
| Internal SRAM | 512 KB | Always available |
| PSRAM | 8 MB | Lost during deep sleep |
| RTC SRAM | 8 KB | Preserved during deep sleep |

### Power

Optional 1800 mAh Li-Po battery with USB-C charging.

| Mode | Current | Notes |
|------|---------|-------|
| Active | ~100-200 mA | WiFi + display refresh |
| Light sleep | 0.7-2 mA | PSRAM preserved |
| Deep sleep | ~10 uA | PSRAM lost |

**Example: 15-minute refresh interval and deep sleep**

```
Per cycle (15 min):
  Active:  150 mA × 20 sec   = 0.83 mAh  (WiFi + fetch + refresh)
  Sleep:   10 uA × 900 sec   = 0.002 mAh (deep sleep, negligible)
  Total:                       ~0.83 mAh

Per day (96 cycles):
  96 × 0.83 mAh = ~80 mAh/day

Battery life:
  1800 mAh / 80 mAh/day ≈ 22 days
```

## Usage

Enter the dev shell with all required build tools:

```bash
nix develop
```

### Server

The server provides the widget API and image processing.

```bash
cd server
cargo run
```

| Variable | Default | Description |
|----------|---------|-------------|
| `PORT` | `3000` | HTTP port |
| `RUST_LOG` | `info` | Log filter |

#### NixOS

Add to your flake inputs and NixOS configuration:

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
          };
        }
      ];
    };
  };
}
```

| Option | Default | Description |
|--------|---------|-------------|
| `enable` | `false` | Enable the service |
| `port` | `3000` | HTTP port |
| `openFirewall` | `false` | Open port in firewall |
| `logLevel` | `info` | RUST_LOG filter |

### Firmware

The firmware runs on the ESP32-S3 and drives the e-paper display.

**Prerequisites:** Install the ESP Rust toolchain using [espup](https://github.com/esp-rs/espup):

```bash
cargo install espup # or, `nix develop` for a nixos compatible version
espup install
source ~/export-esp.sh
```

**Configuration:** Set WiFi credentials and server address via environment variables:

```bash
export WIFI_SSID="your-ssid"
export WIFI_PASS="your-password"
export SERVER_URL="http://192.168.1.42:3000"
```

**Build and flash:**

```bash
cd firmware
cargo run --release
```


