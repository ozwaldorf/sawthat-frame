# SawThat Frame

E-paper display frame for concert/album art data pulled from [sawthat.band](https://sawthat.band).

Built on the [Waveshare ESP32-S3-PhotoPainter](https://www.amazon.com/dp/B0FWRJD8HZ):
- 7.3" Spectra 6 color e-paper display
- ESP32S3 with wifi and ble
- 16MB Flash
- GPIO Buttons and LEDs
- (unused) Speaker, microphones

## API Examples

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

### Server

The server provides the widget API for data fetching and image processing

#### Source

```bash
cd server
PORT=3000 cargo run -r
```

#### Nix

```bash
nix run
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


