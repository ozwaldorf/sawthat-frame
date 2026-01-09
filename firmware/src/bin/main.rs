//! SawThat Frame Firmware - ESP32-S3 E-Paper Photo Frame
//!
//! Environment variables required:
//! - WIFI_SSID: WiFi network name
//! - WIFI_PASS: WiFi password
//! - SERVER_URL: Edge service URL (e.g., http://192.168.1.100:7676)

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use core::time::Duration as CoreDuration;

use embassy_executor::Spawner;
use embassy_net::{
    Runner, Stack, StackResources,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
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
    rtc_cntl::{
        Rtc,
        sleep::{Ext0WakeupSource, TimerWakeupSource, WakeupLevel},
    },
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
    wifi::{ClientConfig, Config as WifiConfig, ModeConfig, WifiController, WifiDevice},
};
use sawthat_frame_firmware::battery;
use sawthat_frame_firmware::cache::SdCache;
use sawthat_frame_firmware::display::{self, TLS_READ_BUF_SIZE, TLS_WRITE_BUF_SIZE};
use sawthat_frame_firmware::epd::{Epd7in3e, Rect, RefreshMode, WIDTH};
use sawthat_frame_firmware::framebuffer::Framebuffer;
use sawthat_frame_firmware::widget::{Orientation, WidgetData};

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
const SERVER_URL: &str = env!("SERVER_URL");

/// Refresh interval between display updates (15 minutes)
const REFRESH_INTERVAL_SECS: u64 = 15 * 60;

/// Magic number to validate RTC memory state
const SLEEP_STATE_MAGIC: u32 = 0xCAFE_F00D;

/// Compute a single hash for all widget data
fn hash_data(items: &WidgetData) -> u32 {
    let mut hash: u32 = 5381;
    for item in items.iter() {
        for byte in item.as_bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(*byte as u32);
        }
        hash = hash.wrapping_mul(33).wrapping_add(0); // separator
    }
    hash
}

/// State persisted in RTC memory across deep sleep
#[repr(C)]
struct SleepState {
    /// Magic number to validate state
    magic: u32,
    /// Current index into widget items (next item to fetch)
    index: usize,
    /// Total number of items
    total_items: usize,
    /// Shuffle seed used (to reproduce ordering)
    shuffle_seed: u64,
    /// Display orientation (0 = horizontal, 1 = vertical)
    orientation: u8,
    /// Next slot to update in horizontal mode (0 = left, 1 = right)
    next_slot: u8,
    /// Item indices currently displayed in each slot [left, right]
    slot_items: [usize; 2],
    /// Hash of all items (to detect data changes)
    data_hash: u32,
}

impl SleepState {
    const fn new() -> Self {
        Self {
            magic: 0,
            index: 0,
            total_items: 0,
            shuffle_seed: 0,
            orientation: 0,
            next_slot: 0,
            slot_items: [0, 0],
            data_hash: 0,
        }
    }

    fn is_valid(&self) -> bool {
        self.magic == SLEEP_STATE_MAGIC
    }

    #[allow(dead_code)]
    fn invalidate(&mut self) {
        self.magic = 0;
    }

    #[allow(clippy::too_many_arguments)]
    fn save(
        &mut self,
        index: usize,
        total_items: usize,
        shuffle_seed: u64,
        orientation: Orientation,
        next_slot: u8,
        slot_items: [usize; 2],
        items: &WidgetData,
    ) {
        self.magic = SLEEP_STATE_MAGIC;
        self.index = index;
        self.total_items = total_items;
        self.shuffle_seed = shuffle_seed;
        self.orientation = orientation as u8;
        self.next_slot = next_slot;
        self.slot_items = slot_items;
        self.data_hash = hash_data(items);
    }

    fn get_orientation(&self) -> Orientation {
        Orientation::from_u8(self.orientation)
    }

    fn get_next_slot(&self) -> u8 {
        self.next_slot
    }

    fn get_slot_items(&self) -> [usize; 2] {
        self.slot_items
    }

    fn matches_data(&self, items: &WidgetData) -> bool {
        items.len() == self.total_items && self.data_hash == hash_data(items)
    }
}

/// RTC fast memory state - persists across deep sleep
#[esp_hal::ram(unstable(rtc_fast))]
static mut SLEEP_STATE: SleepState = SleepState::new();

/// Flag to control red LED blinking from blink task
static BLINK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Blink interval in milliseconds (100 = fast, 500 = normal)
static BLINK_INTERVAL_MS: AtomicU16 = AtomicU16::new(500);

/// Red LED blink task - blinks when BLINK_ACTIVE is true, solid on otherwise
#[embassy_executor::task]
async fn blink_task(led: &'static mut Output<'static>) {
    loop {
        if BLINK_ACTIVE.load(Ordering::Relaxed) {
            led.toggle();
        } else {
            led.set_low(); // ON (active low)
        }
        let interval = BLINK_INTERVAL_MS.load(Ordering::Relaxed) as u64;
        Timer::after(Duration::from_millis(interval)).await;
    }
}

/// Start blinking the red LED (normal speed - 500ms)
fn start_blink() {
    BLINK_INTERVAL_MS.store(500, Ordering::Relaxed);
    BLINK_ACTIVE.store(true, Ordering::Relaxed);
}

/// Start fast blinking the red LED (100ms)
fn start_fast_blink() {
    BLINK_INTERVAL_MS.store(100, Ordering::Relaxed);
    BLINK_ACTIVE.store(true, Ordering::Relaxed);
}

/// Stop blinking and keep red LED solid on
fn stop_blink() {
    BLINK_ACTIVE.store(false, Ordering::Relaxed);
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // Init logger first so we can see any early crashes
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Check wake reason immediately
    let wake_reason = esp_hal::rtc_cntl::wakeup_cause();
    let button_wake = matches!(wake_reason, esp_hal::system::SleepSource::Ext0);

    // ==================== Early Button Check (before heavy init) ====================
    // Set up button and LED GPIOs first for fast response to button wake
    let key_input = Input::new(
        peripherals.GPIO4,
        InputConfig::default().with_pull(Pull::Up),
    );
    let mut led_green = Output::new(peripherals.GPIO42, Level::High, OutputConfig::default());
    let led_red = Output::new(peripherals.GPIO45, Level::Low, OutputConfig::default()); // ON by default

    // Spawn red LED blink task (needs 'static lifetime)
    let led_red_static: &'static mut Output<'static> = mk_static!(Output<'static>, led_red);
    spawner.spawn(blink_task(led_red_static)).ok();
    let mut delay = Delay;

    // Check sleep state to get current orientation
    let (resuming, mut orientation) = unsafe {
        let state = &raw const SLEEP_STATE;
        let valid = (*state).is_valid();
        let orient = if valid {
            (*state).get_orientation()
        } else {
            Orientation::default()
        };
        (valid, orient)
    };

    // Track if we should advance to next item (button tap without hold)
    let mut advance_item = false;
    // Track if orientation was changed during boot (to save to SD card later)
    let mut orientation_changed = false;

    if button_wake {
        // Button caused wake - poll every 50ms to detect hold vs tap
        let mut hold_time_ms: u32 = 0;
        const HOLD_THRESHOLD_MS: u32 = 500;

        // Poll button state every 50ms
        while key_input.is_low() {
            delay.delay_ms(50);
            hold_time_ms += 50;
            if hold_time_ms >= HOLD_THRESHOLD_MS {
                break;
            }
        }

        if hold_time_ms >= HOLD_THRESHOLD_MS {
            // Button held >= 500ms - toggle orientation
            orientation = orientation.toggle();
            orientation_changed = true;

            // Flash LED 3 times for rotation
            for _ in 0..3 {
                led_green.set_low(); // ON
                delay.delay_ms(100);
                led_green.set_high(); // OFF
                delay.delay_ms(100);
            }

            // Wait for button release
            while key_input.is_low() {
                delay.delay_ms(50);
            }
        } else {
            // Button released before 500ms - advance to next item
            advance_item = true;

            // Flash LED 1 time for next item
            led_green.set_low(); // ON
            delay.delay_ms(100);
            led_green.set_high(); // OFF
        }
    }

    // ==================== Normal Boot Sequence ====================
    // Now do the heavier initialization
    println!("Boot! Wake reason: {:?}", wake_reason);

    // Wait for USB serial to reconnect after deep sleep wake
    esp_hal::delay::Delay::new().delay_millis(2000);

    // Initialize internal RAM heap (for smaller allocations)
    println!("Initializing heap...");
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // Initialize PSRAM for large allocations (framebuffer, PNG buffer)
    println!("Initializing PSRAM...");
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);
    println!("PSRAM initialized");

    println!("Starting RTOS...");
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT)
            .software_interrupt0,
    );
    println!("RTOS started");

    // ==================== SD Card Cache Initialization ====================
    // SD card SPI pins: CS=GPIO38, CLK=GPIO39, MISO=GPIO40, MOSI=GPIO41
    println!("Initializing SD card cache...");

    let sd_spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(20))
            .with_mode(Mode::_0),
    )
    .expect("SD SPI init failed")
    .with_sck(peripherals.GPIO39)
    .with_mosi(peripherals.GPIO41)
    .with_miso(peripherals.GPIO40);

    let sd_cs = Output::new(peripherals.GPIO38, Level::High, OutputConfig::default());
    let sd_spi_device = ExclusiveDevice::new_no_delay(sd_spi, sd_cs).unwrap();

    let mut sd_cache = match SdCache::new(sd_spi_device, delay.clone()) {
        Ok(cache) => cache,
        Err(e) => {
            println!("SD card init failed: {:?}", e);
            panic!("SD card required for caching");
        }
    };

    if let Err(e) = sd_cache.init() {
        println!("SD cache init error: {:?}", e);
    }

    // Try to load widget data from cache (for cache-first boot)
    let cached_items = sd_cache.load_widget_data();
    let has_cached_data = cached_items.is_some();
    println!(
        "Cached widget data: {}",
        if has_cached_data { "found" } else { "not found" }
    );

    // Handle orientation persistence
    if orientation_changed {
        // Orientation was changed during boot button hold - save to SD card
        if let Err(e) = sd_cache.store_orientation(orientation) {
            println!("Failed to store orientation: {:?}", e);
        }
    } else if let Some(cached_orient) = sd_cache.load_orientation() {
        // Load orientation from SD card (persistent across power cycles)
        orientation = cached_orient;
        println!("Using cached orientation: {:?}", orientation);
    }

    // ==================== Power Management (AXP2101) ====================
    // SawThat Frame uses AXP2101 PMIC to control display power
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
    const BAT_PERCENT_REG: u8 = 0xA4; // Battery percentage (0-100)

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
    delay.delay_ms(100);

    // ==================== E-Paper Display Setup ====================
    // PhotoPainter GPIO pins for 7.3" e-paper display
    // DC=GPIO8, CS=GPIO9, SCK=GPIO10, MOSI=GPIO11, RST=GPIO12, BUSY=GPIO13

    // SawThat Frame uses SPI3 (not SPI2)
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

    // ==================== WiFi Setup (Deferred) ====================
    // Keep WiFi peripheral for lazy initialization - saves ~500-1000ms on cached boots
    let mut wifi_peripheral: Option<esp_hal::peripherals::WIFI<'static>> = Some(peripherals.WIFI);

    // WiFi state - will be initialized on first network access
    // Note: _esp_radio_ctrl is a reference because it's stored in static memory
    // It's kept alive to ensure the radio stays initialized
    let mut _esp_radio_ctrl: Option<&'static Controller<'static>> = None;
    let mut wifi_controller: Option<WifiController<'static>> = None;
    let mut wifi_connected = false;

    // ==================== RTC for Deep Sleep ====================
    let mut rtc = Rtc::new(peripherals.LPWR);

    // ==================== Main Display Logic ====================
    println!("Starting display update...");
    println!("Edge URL: {}", SERVER_URL);
    println!("Refresh interval: {} seconds", REFRESH_INTERVAL_SECS);

    if button_wake {
        if advance_item {
            println!("Button tap: will advance to next item");
        } else {
            println!("Button hold: toggled orientation to {:?}", orientation);
        }
    } else if resuming {
        let (index, total) = unsafe {
            let state = &raw const SLEEP_STATE;
            ((*state).index, (*state).total_items)
        };
        println!(
            "Resuming from deep sleep: index={}, total={}, orientation={:?}",
            index, total, orientation
        );
    } else {
        println!("Fresh boot (no valid sleep state)");
    }

    // Allocate framebuffer (uses PSRAM for the 192KB buffer)
    println!("Allocating framebuffer...");
    let mut framebuffer = Framebuffer::new();
    println!("Framebuffer allocated!");

    // Use RNG for shuffle seed
    let rng = Rng::new();

    // Allocate TLS buffers for HTTPS support (on heap to save stack)
    let mut tls_read_buf: Box<[u8; TLS_READ_BUF_SIZE]> = Box::new([0u8; TLS_READ_BUF_SIZE]);
    let mut tls_write_buf: Box<[u8; TLS_WRITE_BUF_SIZE]> = Box::new([0u8; TLS_WRITE_BUF_SIZE]);

    // TCP client and DNS socket - created lazily after WiFi init
    let mut tcp_client: Option<TcpClient<'static, 1, 1024, 1024>> = None;
    let mut dns_socket: Option<DnsSocket<'static>> = None;

    // Helper macro to ensure WiFi is initialized and connected
    macro_rules! ensure_wifi {
        () => {{
            if !wifi_connected {
                println!("Initializing WiFi (deferred)...");
                start_fast_blink(); // Visual feedback during slow init

                // Initialize esp-radio (this is the slow part ~500-1000ms)
                let ctrl = esp_radio::init().unwrap();
                let ctrl = mk_static!(Controller<'static>, ctrl);

                // Create WiFi controller and interfaces
                let wifi = wifi_peripheral.take().unwrap();
                let (wifi_ctrl, ifaces) = esp_radio::wifi::new(
                    ctrl,
                    wifi,
                    WifiConfig::default(),
                )
                .unwrap();

                let net_config = embassy_net::Config::dhcpv4(Default::default());
                let (stk, runner) = embassy_net::new(
                    ifaces.sta,
                    net_config,
                    mk_static!(StackResources<3>, StackResources::<3>::new()),
                    rng.random() as u64,
                );
                let stk = mk_static!(Stack<'static>, stk);
                spawner.spawn(net_task(runner)).ok();

                let tcp_state = mk_static!(TcpClientState<1, 1024, 1024>, TcpClientState::new());
                tcp_client = Some(TcpClient::new(*stk, tcp_state));
                dns_socket = Some(DnsSocket::new(*stk));
                _esp_radio_ctrl = Some(ctrl);
                wifi_controller = Some(wifi_ctrl);

                // Connect to WiFi
                wifi_connect(wifi_controller.as_mut().unwrap()).await;
                wait_for_ip(*stk).await;
                wifi_connected = true;
                println!("WiFi ready!");
            }
        }};
    }

    // Fetch widget data (use cache if available, then refresh from network)
    // Keep boxed to avoid 6KB on stack
    println!("Fetching widget data...");
    let mut items: Box<WidgetData> = if let Some(cached) = cached_items {
        println!("Using cached widget data ({} items)", cached.len());
        Box::new(cached)
    } else {
        // No cache - must fetch from network
        ensure_wifi!();

        loop {
            start_blink();
            let result = display::fetch_widget_data(
                tcp_client.as_ref().unwrap(),
                dns_socket.as_ref().unwrap(),
                &mut *tls_read_buf,
                &mut *tls_write_buf,
                SERVER_URL,
                "concerts",
            )
            .await;
            stop_blink();

            match result {
                Ok(data) => {
                    // Store in cache for next boot
                    if let Err(e) = sd_cache.store_widget_data(&data) {
                        println!("Failed to cache widget data: {:?}", e);
                    }
                    break data;
                }
                Err(e) => {
                    println!("Failed to fetch widget data: {:?}, retrying in 30s...", e);
                    Timer::after(Duration::from_secs(30)).await;
                }
            }
        }
    };

    // Get saved state if resuming
    let (shuffle_seed, saved_index, saved_next_slot, saved_slot_items) = if resuming {
        unsafe {
            let state = &raw const SLEEP_STATE;
            (
                (*state).shuffle_seed,
                (*state).index,
                (*state).get_next_slot(),
                (*state).get_slot_items(),
            )
        }
    } else {
        // Fresh start with new shuffle seed
        let seed = (rng.random() as u64) << 32 | rng.random() as u64;
        (seed, 0, 0u8, [0usize, 0usize])
    };

    // Shuffle items (same seed = same order)
    display::shuffle_items(&mut items, shuffle_seed);

    // Now check if data matches (after shuffling, so cache_keys are in same order)
    // Also get saved orientation for partial refresh check
    let (data_matches, saved_orientation) = if resuming {
        unsafe {
            let state = &raw const SLEEP_STATE;
            ((*state).matches_data(&items), (*state).get_orientation())
        }
    } else {
        (false, Orientation::Horizontal)
    };

    let can_partial = data_matches
        && orientation == Orientation::Horizontal
        && saved_orientation == Orientation::Horizontal
        && saved_index >= 2; // At least one full refresh has happened

    let (mut index, mut next_slot, mut slot_items, mut use_partial) = if can_partial {
        println!(
            "Resuming with partial update: slot={}, slot_items=[{}, {}], index={}",
            saved_next_slot, saved_slot_items[0], saved_slot_items[1], saved_index
        );
        (saved_index, saved_next_slot, saved_slot_items, true)
    } else if data_matches {
        println!("Resuming from index {} (full refresh)", saved_index);
        (saved_index, 0u8, [0usize, 0usize], false)
    } else {
        println!("Fresh start or data changed");
        (0, 0u8, [0usize, 0usize], false)
    };

    let total_items = items.len();
    println!("Displaying {} items in shuffled order", total_items);

    // Buffer for partial updates (400x480 = 96000 bytes)
    const HALF_BUFFER_SIZE: usize = 400 * 480 / 2;

    // Display loop - allows re-display on orientation change
    loop {
        // If we've shown all items, start over
        if index >= total_items {
            println!("All items shown, starting over");
            index = 0;
        }

        // Wake up display
        println!("Waking up display...");
        epd.wake_up(&mut delay).expect("Failed to wake display");

        // Read battery percentage
        let battery_percent = {
            let mut buf = [0u8; 1];
            match i2c.write_read(AXP2101_ADDR, &[BAT_PERCENT_REG], &mut buf) {
                Ok(()) => {
                    println!("Battery: {}%", buf[0]);
                    buf[0]
                }
                Err(e) => {
                    println!("Failed to read battery: {:?}", e);
                    50 // Default to 50% on error
                }
            }
        };

        let display_result = if use_partial && orientation == Orientation::Horizontal {
            // ==================== Partial Refresh Mode (Cache-Aware) ====================
            // Only update one half of the display with a single new item
            let item_idx = index % total_items;
            let item_path = items[item_idx].as_str();
            println!(
                "Partial update: slot={}, item={} of {}",
                next_slot, item_idx, total_items
            );

            // PNG buffer for fetching/reading (256KB)
            let mut png_buf: alloc::boxed::Box<[u8; 256 * 1024]> =
                alloc::boxed::Box::new([0u8; 256 * 1024]);

            start_blink();

            // Check cache first
            let png_len = if sd_cache.has_image(item_path, Orientation::Horizontal) {
                println!("Cache HIT: {}", item_path);
                sd_cache
                    .read_image(item_path, Orientation::Horizontal, &mut *png_buf)
                    .unwrap_or_default()
            } else {
                println!("Cache MISS: {}", item_path);
                // Initialize and connect WiFi if not already connected
                ensure_wifi!();
                match display::fetch_png(
                    tcp_client.as_ref().unwrap(),
                    dns_socket.as_ref().unwrap(),
                    &mut *tls_read_buf,
                    &mut *tls_write_buf,
                    &mut *png_buf,
                    SERVER_URL,
                    "concerts",
                    item_path,
                    Orientation::Horizontal,
                )
                .await
                {
                    Ok(len) => {
                        if let Err(e) =
                            sd_cache.write_image(item_path, Orientation::Horizontal, &png_buf[..len])
                        {
                            println!("Cache store failed: {:?}", e);
                        }
                        len
                    }
                    Err(e) => {
                        println!("Fetch failed: {:?}", e);
                        0
                    }
                }
            };

            // Render to framebuffer
            let fetch_result = if png_len > 0 {
                display::render_png_to_framebuffer(
                    &png_buf[..png_len],
                    &mut framebuffer,
                    next_slot,
                    Orientation::Horizontal,
                )
            } else {
                Err(display::DisplayError::Network)
            };

            // Draw battery indicator centered horizontally
            if fetch_result.is_ok() {
                let (bat_w, _bat_h) = battery::battery_dimensions(false);
                let battery_x = (WIDTH as u16 - bat_w) / 2;
                let battery_y = 8;
                battery::draw_battery(
                    framebuffer.as_mut_slice(),
                    battery_x,
                    battery_y,
                    battery_percent,
                    false,
                );
            }

            // Start partial update
            let display_started = match fetch_result {
                Ok(()) => {
                    // Extract the half we need to update
                    let mut half_buffer = [0u8; HALF_BUFFER_SIZE];
                    framebuffer.extract_half(next_slot, &mut half_buffer);

                    // Create rect for the half (left: x=0, right: x=400)
                    let x_offset = if next_slot == 0 { 0 } else { 400 };
                    let rect = Rect::new(x_offset, 0, 400, 480);

                    println!("Partial refresh: x={}, w={}, h={}", x_offset, 400, 480);

                    epd.partial_update_start(&rect, &half_buffer, &mut delay).is_ok()
                }
                Err(_) => false,
            };

            // Update slot tracking early so prefetch uses correct next index
            if display_started {
                slot_items[next_slot as usize] = item_idx;
                next_slot = (next_slot + 1) % 2;
                index += 1; // Advance by 1 for partial updates
            }

            // Prefetch next image and refresh widget data while display is refreshing
            if display_started {
                // Initialize and connect WiFi now if we deferred it
                ensure_wifi!();

                // Prefetch next image
                let prefetch_idx = index % total_items;
                let prefetch_path = items[prefetch_idx].as_str();
                if !sd_cache.has_image(prefetch_path, Orientation::Horizontal) {
                    println!("Prefetching next image: {}", prefetch_path);
                    let mut prefetch_buf: Box<[u8; 256 * 1024]> = Box::new([0u8; 256 * 1024]);
                    if let Ok(len) = display::fetch_png(
                        tcp_client.as_ref().unwrap(),
                        dns_socket.as_ref().unwrap(),
                        &mut *tls_read_buf,
                        &mut *tls_write_buf,
                        &mut *prefetch_buf,
                        SERVER_URL,
                        "concerts",
                        prefetch_path,
                        Orientation::Horizontal,
                    )
                    .await
                    {
                        if let Err(e) =
                            sd_cache.write_image(prefetch_path, Orientation::Horizontal, &prefetch_buf[..len])
                        {
                            println!("Prefetch cache store failed: {:?}", e);
                        } else {
                            println!("Prefetched and cached: {}", prefetch_path);
                        }
                    }
                }

                // Refresh widget data from server if we used cached data
                if has_cached_data {
                    println!("Refreshing widget data from server...");
                    if let Ok(fresh_items) = display::fetch_widget_data(
                        tcp_client.as_ref().unwrap(),
                        dns_socket.as_ref().unwrap(),
                        &mut *tls_read_buf,
                        &mut *tls_write_buf,
                        SERVER_URL,
                        "concerts",
                    )
                    .await
                    {
                        if fresh_items.len() != items.len()
                            || fresh_items
                                .iter()
                                .zip(items.iter())
                                .any(|(a, b)| a.as_str() != b.as_str())
                        {
                            println!("Widget data changed, updating cache");
                            if let Err(e) = sd_cache.store_widget_data(&fresh_items) {
                                println!("Failed to update widget data cache: {:?}", e);
                            }
                            if let Ok(count) = sd_cache.cleanup_stale(&fresh_items)
                                && count > 0
                            {
                                println!("Invalidated {} stale cache entries", count);
                            }
                        }
                    }
                }
            }

            // Now wait for display to finish
            let result = if display_started {
                while epd.is_busy() {
                    Timer::after(Duration::from_millis(50)).await;
                }
                epd.refresh_wait(&mut delay)
                    .map_err(|_| display::DisplayError::Network)
            } else {
                Err(display::DisplayError::Network)
            };
            stop_blink();
            embassy_futures::yield_now().await;

            result
        } else {
            // ==================== Full Refresh Mode (Cache-Aware) ====================
            // Update entire display with 2 items (horizontal) or 1 item (vertical)
            println!(
                "Full refresh: items {} and {} of {}",
                index,
                (index + 1).min(total_items - 1),
                total_items
            );

            // Clear framebuffer
            framebuffer.clear(sawthat_frame_firmware::epd::Color::White);

            // PNG buffer for fetching/reading (256KB)
            let mut png_buf: alloc::boxed::Box<[u8; 256 * 1024]> =
                alloc::boxed::Box::new([0u8; 256 * 1024]);

            start_blink();

            // Number of items to display
            let items_per_screen = match orientation {
                Orientation::Horizontal => 2,
                Orientation::Vertical => 1,
            };

            let mut fetch_ok = true;
            for slot in 0..items_per_screen {
                let item_idx = (index + slot) % total_items;
                let item_path = items[item_idx].as_str();

                // Check cache first
                let png_len = if sd_cache.has_image(item_path, orientation) {
                    println!("Cache HIT: {}", item_path);
                    sd_cache
                        .read_image(item_path, orientation, &mut *png_buf)
                        .unwrap_or_default()
                } else {
                    println!("Cache MISS: {}", item_path);
                    // Initialize and connect WiFi if not already connected
                    ensure_wifi!();
                    // Fetch from network
                    match display::fetch_png(
                        tcp_client.as_ref().unwrap(),
                        dns_socket.as_ref().unwrap(),
                        &mut *tls_read_buf,
                        &mut *tls_write_buf,
                        &mut *png_buf,
                        SERVER_URL,
                        "concerts",
                        item_path,
                        orientation,
                    )
                    .await
                    {
                        Ok(len) => {
                            // Store in cache
                            if let Err(e) =
                                sd_cache.write_image(item_path, orientation, &png_buf[..len])
                            {
                                println!("Cache store failed: {:?}", e);
                            }
                            len
                        }
                        Err(e) => {
                            println!("Fetch failed: {:?}", e);
                            0
                        }
                    }
                };

                // Decode and render to framebuffer
                if png_len > 0 {
                    if let Err(e) = display::render_png_to_framebuffer(
                        &png_buf[..png_len],
                        &mut framebuffer,
                        slot as u8,
                        orientation,
                    ) {
                        println!("Render failed: {:?}", e);
                        fetch_ok = false;
                    }
                } else {
                    fetch_ok = false;
                }
            }

            let fetch_result: Result<(), display::DisplayError> = if fetch_ok {
                Ok(())
            } else {
                Err(display::DisplayError::Network)
            };

            // Draw battery indicator into framebuffer
            if fetch_result.is_ok() {
                let vertical = orientation == Orientation::Vertical;
                let (bat_w, _bat_h) = battery::battery_dimensions(vertical);
                // Centered horizontally in horizontal mode, right-aligned in vertical
                let battery_x = if vertical {
                    WIDTH as u16 - bat_w - 8
                } else {
                    (WIDTH as u16 - bat_w) / 2
                };
                let battery_y = 8;
                battery::draw_battery(
                    framebuffer.as_mut_slice(),
                    battery_x,
                    battery_y,
                    battery_percent,
                    vertical,
                );
            }

            // Start display update
            let display_started = match fetch_result {
                Ok(()) => {
                    println!("Updating display (full refresh)...");
                    epd.display_start(framebuffer.as_slice(), &mut delay).is_ok()
                }
                Err(_) => false,
            };

            // Update slot tracking for horizontal mode (enables partial updates next time)
            if display_started && orientation == Orientation::Horizontal {
                slot_items[0] = index % total_items;
                slot_items[1] = (index + 1) % total_items;
                next_slot = 0;
                index += 2;
                use_partial = true; // Enable partial updates for subsequent refreshes
            } else if display_started {
                index += 1; // Vertical mode: advance by 1
            }

            // Prefetch next image and refresh widget data while display is refreshing
            if display_started {
                // Initialize and connect WiFi now if we deferred it (using cached data path)
                ensure_wifi!();

                // Prefetch next image
                let prefetch_idx = index % total_items;
                let prefetch_path = items[prefetch_idx].as_str();
                if !sd_cache.has_image(prefetch_path, orientation) {
                    println!("Prefetching next image: {}", prefetch_path);
                    let mut prefetch_buf: Box<[u8; 256 * 1024]> = Box::new([0u8; 256 * 1024]);
                    if let Ok(len) = display::fetch_png(
                        tcp_client.as_ref().unwrap(),
                        dns_socket.as_ref().unwrap(),
                        &mut *tls_read_buf,
                        &mut *tls_write_buf,
                        &mut *prefetch_buf,
                        SERVER_URL,
                        "concerts",
                        prefetch_path,
                        orientation,
                    )
                    .await
                    {
                        if let Err(e) =
                            sd_cache.write_image(prefetch_path, orientation, &prefetch_buf[..len])
                        {
                            println!("Prefetch cache store failed: {:?}", e);
                        } else {
                            println!("Prefetched and cached: {}", prefetch_path);
                        }
                    }
                }

                // Refresh widget data from server if we used cached data
                if has_cached_data {
                    println!("Refreshing widget data from server...");
                    if let Ok(fresh_items) = display::fetch_widget_data(
                        tcp_client.as_ref().unwrap(),
                        dns_socket.as_ref().unwrap(),
                        &mut *tls_read_buf,
                        &mut *tls_write_buf,
                        SERVER_URL,
                        "concerts",
                    )
                    .await
                    {
                        // Check if data changed
                        if fresh_items.len() != items.len()
                            || fresh_items
                                .iter()
                                .zip(items.iter())
                                .any(|(a, b)| a.as_str() != b.as_str())
                        {
                            println!("Widget data changed, updating cache");
                            if let Err(e) = sd_cache.store_widget_data(&fresh_items) {
                                println!("Failed to update widget data cache: {:?}", e);
                            }
                            // Invalidate stale image cache entries
                            if let Ok(count) = sd_cache.cleanup_stale(&fresh_items)
                                && count > 0
                            {
                                println!("Invalidated {} stale cache entries", count);
                            }
                        }
                    }
                }
            }

            // Now wait for display to finish
            let result = if display_started {
                while epd.is_busy() {
                    Timer::after(Duration::from_millis(50)).await;
                }
                epd.finish_display(&mut delay)
                    .map_err(|_| display::DisplayError::Network)
            } else {
                Err(display::DisplayError::Network)
            };
            stop_blink();
            embassy_futures::yield_now().await;

            result
        };

        match display_result {
            Ok(()) => println!("Display refresh successful!"),
            Err(e) => println!("Display refresh failed: {:?}", e),
        }

        // Put display to sleep
        println!("Putting display to sleep...");
        epd.sleep(&mut delay).expect("Failed to sleep display");

        // Wait 10s for button input before deep sleep
        println!("Press KEY within 10s (tap=next item, hold=rotate)...");
        let mut should_redisplay = false;
        const HOLD_THRESHOLD_MS: u32 = 500;
        for _ in 0..100 {
            if key_input.is_low() {
                println!("KEY pressed, polling for hold...");

                // Poll every 50ms to detect hold vs tap
                let mut hold_time_ms: u32 = 0;
                while key_input.is_low() {
                    Timer::after(Duration::from_millis(50)).await;
                    hold_time_ms += 50;
                    if hold_time_ms >= HOLD_THRESHOLD_MS {
                        break;
                    }
                }

                if hold_time_ms >= HOLD_THRESHOLD_MS {
                    // Button held >= 500ms - toggle orientation
                    println!("Button held! Toggling orientation...");
                    orientation = orientation.toggle();
                    // Save to SD card
                    if let Err(e) = sd_cache.store_orientation(orientation) {
                        println!("Failed to store orientation: {:?}", e);
                    }
                    // Reset partial mode on orientation change
                    use_partial = false;
                    slot_items = [0, 0];
                    next_slot = 0;

                    // Flash LED2 3 times to confirm rotation
                    for _ in 0..3 {
                        led_green.set_low(); // ON
                        delay.delay_ms(100);
                        led_green.set_high(); // OFF
                        delay.delay_ms(100);
                    }

                    // Wait for button release
                    while key_input.is_low() {
                        delay.delay_ms(50);
                    }

                    println!("Re-displaying with orientation: {:?}", orientation);
                } else {
                    // Button released before 500ms - show next item
                    println!("Button tap, next item (index={})", index);

                    // Flash LED2 1 time to confirm next item
                    led_green.set_low(); // ON
                    delay.delay_ms(100);
                    led_green.set_high(); // OFF
                }

                should_redisplay = true;
                break;
            }
            Timer::after(Duration::from_millis(100)).await; // Async yield lets blink task run
        }

        if !should_redisplay {
            // No button press, exit loop and go to sleep
            break;
        }
        // Loop back to re-display
    }

    // Save state for next wake (index already advanced in the loop)
    unsafe {
        let state = &raw mut SLEEP_STATE;
        (*state).save(
            index,
            total_items,
            shuffle_seed,
            orientation,
            next_slot,
            slot_items,
            &items,
        );
    }
    println!(
        "Saved state: index={}, total={}, orientation={:?}, next_slot={}, slot_items=[{}, {}]",
        index, total_items, orientation, next_slot, slot_items[0], slot_items[1]
    );

    // Disconnect WiFi before deep sleep (only if it was initialized)
    if let Some(ctrl) = wifi_controller.as_mut() {
        println!("Disconnecting WiFi for deep sleep...");
        wifi_disconnect(ctrl).await;
    } else {
        println!("WiFi was never initialized, skipping disconnect");
    }

    // Reclaim GPIO4 for deep sleep wake source
    let key_pin = unsafe { esp_hal::peripherals::GPIO4::steal() };

    // Enter deep sleep
    println!(
        "Entering deep sleep for {} seconds (press button to wake early)...",
        REFRESH_INTERVAL_SECS
    );
    enter_deep_sleep(&mut rtc, key_pin, &mut delay, REFRESH_INTERVAL_SECS);
}

/// Enter deep sleep with timer and KEY button (GPIO4) wake sources
fn enter_deep_sleep<P: esp_hal::gpio::RtcPinWithResistors>(
    rtc: &mut Rtc,
    key_pin: P,
    delay: &mut Delay,
    seconds: u64,
) -> ! {
    // Configure wake sources
    let timer = TimerWakeupSource::new(CoreDuration::from_secs(seconds));

    // Enable internal pull-up on GPIO4 so it doesn't float and trigger spurious wakes
    key_pin.rtcio_pullup(true);
    key_pin.rtcio_pulldown(false);

    // GPIO4 KEY button is active low (button pulls to ground when pressed)
    let ext0 = Ext0WakeupSource::new(key_pin, WakeupLevel::Low);

    // Small delay to let serial output flush
    delay.delay_ms(100);

    // Enter deep sleep (never returns - device reboots on wake)
    rtc.sleep_deep(&[&timer, &ext0])
}

/// Connect to WiFi network
async fn wifi_connect(controller: &mut WifiController<'static>) {
    start_fast_blink();
    println!("Device capabilities: {:?}", controller.capabilities());

    if !matches!(controller.is_started(), Ok(true)) {
        let client_config = ModeConfig::Client(
            ClientConfig::default()
                .with_ssid(SSID.into())
                .with_password(PASSWORD.into()),
        );
        controller.set_config(&client_config).unwrap();
        println!("Starting WiFi...");
        controller.start_async().await.unwrap();
        println!("WiFi started!");
    }

    println!("Connecting to {}...", SSID);
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                println!("WiFi connected!");
                stop_blink();
                break;
            }
            Err(e) => {
                println!("Failed to connect: {e:?}, retrying...");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Disconnect and stop WiFi to save power
async fn wifi_disconnect(controller: &mut WifiController<'static>) {
    if let Err(e) = controller.disconnect_async().await {
        println!("Disconnect error (may already be disconnected): {:?}", e);
    }
    if let Err(e) = controller.stop_async().await {
        println!("Stop error: {:?}", e);
    }
    println!("WiFi stopped");
}

/// Wait for network stack to get an IP address
async fn wait_for_ip(stack: Stack<'static>) {
    println!("Waiting for link...");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    println!("Link up!");

    println!("Waiting for IP...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
