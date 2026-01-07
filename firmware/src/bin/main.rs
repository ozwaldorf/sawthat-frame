//! PhotoPainter - ESP32-S3 E-Paper Photo Frame
//!
//! Environment variables required:
//! - WIFI_SSID: WiFi network name
//! - WIFI_PASS: WiFi password
//! - EDGE_URL: Edge service URL (e.g., http://192.168.1.100:7676)

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources, dns::DnsSocket, tcp::client::{TcpClient, TcpClientState}};
use embassy_time::{Delay, Duration, Timer};
use embedded_hal::delay::DelayNs;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    ram,
    rng::Rng,
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::{
    Controller,
    wifi::{
        ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
    },
};
use photopainter::display::{self, TLS_READ_BUF_SIZE, TLS_WRITE_BUF_SIZE};
use photopainter::epd::{Epd7in3e, RefreshMode};
use photopainter::framebuffer::Framebuffer;
use photopainter::widget::WidgetData;

esp_bootloader_esp_idf::esp_app_desc!();

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASS");
const EDGE_URL: &str = env!("EDGE_URL");

/// Refresh interval between display updates (15 minutes)
const REFRESH_INTERVAL_SECS: u64 = 15 * 60;

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Initialize internal RAM heap (for smaller allocations)
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // Initialize PSRAM for large allocations (framebuffer, PNG buffer)
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);
    println!("PSRAM initialized and added to heap");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT)
            .software_interrupt0,
    );

    // ==================== Power Management (AXP2101) ====================
    // PhotoPainter uses AXP2101 PMIC to control display power
    // I2C: SDA=GPIO47, SCL=GPIO48, Address=0x34
    println!("Initializing AXP2101 PMIC...");

    let mut i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .expect("I2C init failed")
    .with_sda(peripherals.GPIO47)
    .with_scl(peripherals.GPIO48);

    const AXP2101_ADDR: u8 = 0x34;
    const LDO_ONOFF_CTRL0: u8 = 0x90; // ALDO enable bits
    const LDO_VOL2_CTRL: u8 = 0x94; // ALDO3 voltage
    const LDO_VOL3_CTRL: u8 = 0x95; // ALDO4 voltage

    // Try to configure PMIC - may already be set by bootloader
    let pmic_ok = (|| -> Result<(), esp_hal::i2c::master::Error> {
        // Set ALDO3 voltage to 3.3V: (3300-500)/100 = 28 = 0x1C
        i2c.write(AXP2101_ADDR, &[LDO_VOL2_CTRL, 0x1C])?;
        // Set ALDO4 voltage to 3.3V
        i2c.write(AXP2101_ADDR, &[LDO_VOL3_CTRL, 0x1C])?;
        // Enable ALDO3 and ALDO4 (bits 2 and 3) - just set all common LDOs on
        i2c.write(AXP2101_ADDR, &[LDO_ONOFF_CTRL0, 0x0F])?;
        Ok(())
    })();

    match pmic_ok {
        Ok(()) => println!("PMIC configured - ALDO3/ALDO4 enabled at 3.3V"),
        Err(e) => println!("PMIC config skipped (may be pre-configured): {:?}", e),
    }

    // Small delay for power rails to stabilize
    Delay.delay_ms(100);

    // ==================== E-Paper Display Setup ====================
    // PhotoPainter GPIO pins for 7.3" e-paper display
    // DC=GPIO8, CS=GPIO9, SCK=GPIO10, MOSI=GPIO11, RST=GPIO12, BUSY=GPIO13

    // PhotoPainter uses SPI3 (not SPI2)
    let spi = Spi::new(
        peripherals.SPI3,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(10))
            .with_mode(Mode::_0),
    )
    .expect("SPI init failed")
    .with_sck(peripherals.GPIO10)
    .with_mosi(peripherals.GPIO11);

    let cs = Output::new(peripherals.GPIO9, Level::High, OutputConfig::default());
    let spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();

    let busy = Input::new(
        peripherals.GPIO13,
        InputConfig::default().with_pull(Pull::Up),
    );
    let dc = Output::new(peripherals.GPIO8, Level::Low, OutputConfig::default());
    let mut rst = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());

    let mut delay = Delay;

    // Debug: check BUSY pin state
    println!(
        "BUSY pin initial state: {}",
        if busy.is_high() { "HIGH" } else { "LOW" }
    );

    // Manual hardware reset before init (matches C driver timing)
    println!("Performing hardware reset...");
    rst.set_high();
    delay.delay_ms(50);
    rst.set_low();
    delay.delay_ms(20);
    rst.set_high();
    delay.delay_ms(50);

    println!(
        "BUSY pin after reset: {}",
        if busy.is_high() { "HIGH" } else { "LOW" }
    );

    println!("Initializing e-paper display (fast mode)...");
    let mut epd = Epd7in3e::new(spi_device, busy, dc, rst, &mut delay, RefreshMode::Fast)
        .expect("EPD init failed");
    println!("EPD initialized!");

    // Start clearing display (non-blocking - continues during WiFi init)
    println!("Starting display clear...");
    epd.clear_start(photopainter::epd::Color::White, &mut delay)
        .expect("Failed to start display clear");

    // ==================== WiFi Setup (runs while display clears) ====================
    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

    let (controller, interfaces) =
        esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();

    let wifi_interface = interfaces.sta;

    let config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    // Wait for WiFi link
    println!("Waiting for WiFi link...");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    println!("WiFi link up!");

    // Wait for IP address
    println!("Waiting for IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    // ==================== Main Display Loop ====================
    println!("Starting display refresh loop...");
    println!("Edge URL: {}", EDGE_URL);
    println!("Refresh interval: {} seconds", REFRESH_INTERVAL_SECS);

    // Allocate framebuffer (uses PSRAM for the 192KB buffer)
    println!("Allocating framebuffer...");
    let mut framebuffer = Framebuffer::new();
    println!("Framebuffer allocated!");

    // Wait for display clear to finish (if still running)
    println!("Waiting for display clear to complete...");
    epd.refresh_wait(&mut delay)
        .expect("Failed to complete display clear");
    println!("Display cleared!");

    // Use RNG for shuffle seed
    let rng = Rng::new();

    // Allocate TLS buffers for HTTPS support
    let mut tls_read_buf = [0u8; TLS_READ_BUF_SIZE];
    let mut tls_write_buf = [0u8; TLS_WRITE_BUF_SIZE];

    // Create TCP client and DNS socket for HTTP requests
    let tcp_state = mk_static!(TcpClientState<1, 1024, 1024>, TcpClientState::new());
    let tcp_client = TcpClient::new(stack, tcp_state);
    let dns_socket = DnsSocket::new(stack);

    loop {
        // Fetch widget data
        println!("Fetching widget data...");
        let mut items: WidgetData = match display::fetch_widget_data(
            &tcp_client,
            &dns_socket,
            &mut tls_read_buf,
            &mut tls_write_buf,
            EDGE_URL,
            "concerts",
        ).await {
            Ok(data) => data,
            Err(e) => {
                println!("Failed to fetch widget data: {:?}", e);
                // Wait and retry
                Timer::after(Duration::from_secs(60)).await;
                continue;
            }
        };

        // Shuffle items
        let shuffle_seed = (rng.random() as u64) << 32 | rng.random() as u64;
        display::shuffle_items(&mut items, shuffle_seed);

        let total_items = items.len();
        println!("Displaying {} items in shuffled order", total_items);

        // Display all items (2 at a time)
        let mut index: usize = 0;
        while index < total_items {
            // Wake up display if sleeping
            println!("Waking up display...");
            epd.wake_up(&mut delay).expect("Failed to wake display");

            // Display current pair
            println!("Displaying items {} and {} of {}", index, (index + 1).min(total_items - 1), total_items);
            match display::fetch_and_display(
                &tcp_client,
                &dns_socket,
                &mut tls_read_buf,
                &mut tls_write_buf,
                &mut epd,
                &mut delay,
                &mut framebuffer,
                EDGE_URL,
                "concerts",
                &items,
                index,
            )
            .await
            {
                Ok(()) => {
                    println!("Display refresh successful!");
                }
                Err(e) => {
                    println!("Display refresh failed: {:?}", e);
                }
            }

            // Put display to sleep
            println!("Putting display to sleep...");
            epd.sleep(&mut delay).expect("Failed to sleep display");

            // Advance by 2 (showing 2 images at a time)
            index += 2;

            // If more items to show, wait for next refresh
            if index < total_items {
                println!("Sleeping for {} seconds...", REFRESH_INTERVAL_SECS);
                Timer::after(Duration::from_secs(REFRESH_INTERVAL_SECS)).await;
            }
        }

        // All items shown, wait before refetching
        println!("All items displayed. Sleeping {} seconds before refetch...", REFRESH_INTERVAL_SECS);
        Timer::after(Duration::from_secs(REFRESH_INTERVAL_SECS)).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if esp_radio::wifi::sta_state() == WifiStaState::Connected {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");

            let scan_config = ScanConfig::default().with_max(10);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            if let Some(ap) = result.iter().find(|ap| ap.ssid.as_str() == SSID) {
                println!("Found {} (ch{}, {}dBm)", ap.ssid, ap.channel, ap.signal_strength);
            } else {
                println!("SSID '{}' not found in {} APs", SSID, result.len());
            }
        }
        println!("Connecting to {}...", SSID);

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
