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
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};
use core::time::Duration as CoreDuration;
use log::info;

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
use esp_radio::{
    Controller,
    wifi::{ClientConfig, Config as WifiConfig, ModeConfig, WifiController, WifiDevice},
};
use sawthat_frame_firmware::TimestampLogger;
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

// Environment configuration
const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASS");
const SERVER_URL: &str = env!("SERVER_URL");

/// Refresh interval between display updates (15 minutes)
const REFRESH_INTERVAL_SECS: u64 = 15 * 60;
/// Button hold threshold in milliseconds
const HOLD_THRESHOLD_MS: u32 = 500;
/// Button polling interval in milliseconds
const BUTTON_POLL_MS: u64 = 50;
/// Display busy polling interval in milliseconds (display refresh takes seconds)
const DISPLAY_BUSY_POLL_MS: u64 = 200;
/// Magic number to validate RTC memory state
const SLEEP_STATE_MAGIC: u32 = 0xCAFE_F00D;

/// RTC fast memory state - persists across deep sleep
#[esp_hal::ram(unstable(rtc_fast))]
static mut SLEEP_STATE: SleepState = SleepState::new();

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

/// Button monitor state
static BUTTON_STATE: AtomicU8 = AtomicU8::new(BUTTON_CANCELLED);
const BUTTON_CANCELLED: u8 = 0;
const BUTTON_POLLING: u8 = 1;
const BUTTON_NEXT: u8 = 2;
const BUTTON_FLIP: u8 = 3;

/// Green LED flash request (0 = none, 1 = one flash, 3 = three flashes)
static LED_GREEN_FLASH: AtomicU8 = AtomicU8::new(0);
/// Flag to control red LED blinking
static LED_RED_BLINK: AtomicBool = AtomicBool::new(false);
/// Red LED blink interval in milliseconds (100 = fast, 500 = normal)
static LED_RED_INTERVAL: AtomicU16 = AtomicU16::new(500);

/// Combined LED task - handles red LED blinking and green LED flash requests
#[embassy_executor::task]
async fn led_task(led_red: &'static mut Output<'static>, led_green: &'static mut Output<'static>) {
    let mut last_blink = embassy_time::Instant::now();

    loop {
        // Handle green LED flash requests (priority)
        let flash_count = LED_GREEN_FLASH.swap(0, Ordering::Relaxed);
        if flash_count > 0 {
            for _ in 0..flash_count {
                led_green.set_low(); // ON
                Timer::after(Duration::from_millis(100)).await;
                led_green.set_high(); // OFF
                Timer::after(Duration::from_millis(100)).await;
            }
        }

        // Handle red LED blinking
        let interval = LED_RED_INTERVAL.load(Ordering::Relaxed) as u64;
        if last_blink.elapsed() >= Duration::from_millis(interval) {
            if LED_RED_BLINK.load(Ordering::Relaxed) {
                led_red.toggle();
            } else {
                led_red.set_low(); // ON (active low)
            }
            last_blink = embassy_time::Instant::now();
        }

        Timer::after(Duration::from_millis(50)).await;
    }
}

/// Start blinking the red LED (normal speed - 500ms)
fn start_blink() {
    LED_RED_INTERVAL.store(500, Ordering::Relaxed);
    LED_RED_BLINK.store(true, Ordering::Relaxed);
}

/// Start fast blinking the red LED (100ms)
fn start_fast_blink() {
    LED_RED_INTERVAL.store(100, Ordering::Relaxed);
    LED_RED_BLINK.store(true, Ordering::Relaxed);
}

/// Stop blinking and keep red LED solid on
fn stop_blink() {
    LED_RED_BLINK.store(false, Ordering::Relaxed);
}

/// Button monitor task - polls button every 50ms and sets state when action detected
/// Signals LED flash via atomic when action is detected
#[embassy_executor::task(pool_size = 4)]
async fn button_monitor_task(key_input: &'static Input<'static>) {
    loop {
        // Check if we should stop
        if BUTTON_STATE.load(Ordering::Relaxed) != BUTTON_POLLING {
            return;
        }

        // Check if button is pressed
        if key_input.is_low() {
            let mut hold_time: u32 = 0;

            // Button hold check
            while key_input.is_low() {
                if hold_time >= HOLD_THRESHOLD_MS {
                    // Button was held past the threshold, set the action state
                    if BUTTON_STATE
                        .compare_exchange(
                            BUTTON_POLLING,
                            BUTTON_FLIP,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        // Request 3 flashes for flip
                        LED_GREEN_FLASH.store(3, Ordering::Relaxed);
                    }
                    return;
                }

                hold_time += BUTTON_POLL_MS as u32;
                Timer::after(Duration::from_millis(BUTTON_POLL_MS)).await;
            }

            // Otherwise, tap detected, set the action state
            if BUTTON_STATE
                .compare_exchange(
                    BUTTON_POLLING,
                    BUTTON_NEXT,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                // Request 1 flash for next
                LED_GREEN_FLASH.store(1, Ordering::Relaxed);
            }
            return;
        }

        Timer::after(Duration::from_millis(BUTTON_POLL_MS)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // Init timestamped logger for all log crate output (including ESP libs)
    TimestampLogger::init(log::LevelFilter::Info);

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
    let led_green = Output::new(peripherals.GPIO42, Level::High, OutputConfig::default());
    let led_red = Output::new(peripherals.GPIO45, Level::Low, OutputConfig::default()); // ON by default

    // Make LEDs static and spawn combined LED task
    let led_red: &'static mut Output<'static> = mk_static!(Output<'static>, led_red);
    let led_green: &'static mut Output<'static> = mk_static!(Output<'static>, led_green);
    spawner.spawn(led_task(led_red, led_green)).ok();

    // Make key_input static for use in spawned button monitor task
    let key_input: &'static Input<'static> = mk_static!(Input<'static>, key_input);

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

    if button_wake {
        // Button caused wake - poll every 50ms to detect hold vs tap
        let mut hold_time_ms: u32 = 0;

        // Poll button state every 50ms (async to let LED task run)
        while key_input.is_low() {
            Timer::after(Duration::from_millis(BUTTON_POLL_MS)).await;
            hold_time_ms += BUTTON_POLL_MS as u32;
            if hold_time_ms >= HOLD_THRESHOLD_MS {
                break;
            }
        }

        if hold_time_ms >= HOLD_THRESHOLD_MS {
            // Button held >= 500ms - toggle orientation
            orientation = orientation.toggle();
            BUTTON_STATE.store(BUTTON_FLIP, Ordering::Relaxed);
            // Request 3 flashes for rotation
            LED_GREEN_FLASH.store(3, Ordering::Relaxed);
        } else {
            // Button released before 500ms - advance to next item
            BUTTON_STATE.store(BUTTON_NEXT, Ordering::Relaxed);
            // Request 1 flash for next item
            LED_GREEN_FLASH.store(1, Ordering::Relaxed);
        }
    }

    // ==================== Normal Boot Sequence ====================
    // Now do the heavier initialization
    info!("Boot! Wake reason: {:?}", wake_reason);

    // Initialize internal RAM heap (for smaller allocations)
    info!("Initializing heap...");
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // Initialize PSRAM for large allocations (framebuffer, PNG buffer)
    info!("Initializing PSRAM...");
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);
    info!("PSRAM initialized");

    info!("Starting RTOS...");
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT)
            .software_interrupt0,
    );
    info!("RTOS started");

    let mut delay = Delay;

    // ==================== SD Card Cache Initialization ====================
    // SD card SPI pins: CS=GPIO38, CLK=GPIO39, MISO=GPIO40, MOSI=GPIO41
    info!("Initializing SD card cache...");

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
        Ok(mut cache) => {
            if let Err(e) = cache.init() {
                info!("SD cache init error: {:?}", e);
            }
            Some(cache)
        }
        Err(e) => {
            info!("SD card init failed: {:?} (cache disabled)", e);
            None
        }
    };

    // Try to load widget data from cache (for cache-first boot)
    let cached_items = sd_cache.as_mut().and_then(|c| c.load_widget_data());
    let has_cached_data = cached_items.is_some();
    info!(
        "Cached widget data: {}",
        if has_cached_data {
            "found"
        } else {
            "not found"
        }
    );

    // Handle orientation persistence
    if BUTTON_STATE.load(Ordering::Relaxed) == BUTTON_FLIP {
        // Orientation was changed during boot button hold - save to SD card
        if let Some(cache) = sd_cache.as_mut()
            && let Err(e) = cache.store_orientation(orientation)
        {
            info!("Failed to store orientation: {:?}", e);
        }
        // Reset button state after handling so display loop starts fresh
        BUTTON_STATE.store(BUTTON_CANCELLED, Ordering::Relaxed);
    } else if BUTTON_STATE.load(Ordering::Relaxed) == BUTTON_NEXT {
        // Button tap detected during boot - reset state, display loop will show next item
        BUTTON_STATE.store(BUTTON_CANCELLED, Ordering::Relaxed);
    } else if let Some(cached_orient) = sd_cache.as_mut().and_then(|c| c.load_orientation()) {
        // Load orientation from SD card (persistent across power cycles)
        orientation = cached_orient;
        info!("Using cached orientation: {:?}", orientation);
    }

    // ==================== Power Management (AXP2101) ====================
    // SawThat Frame uses AXP2101 PMIC to control display power
    // I2C: SDA=GPIO47, SCL=GPIO48, Address=0x34
    info!("Initializing AXP2101 PMIC...");

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
        Ok(()) => info!("PMIC configured - ALDO3/ALDO4 enabled at 3.3V"),
        Err(e) => info!("PMIC config skipped (may be pre-configured): {:?}", e),
    }

    // Small delay for power rails to stabilize
    delay.delay_ms(100);

    // ==================== E-Paper Display Setup ====================
    // PhotoPainter GPIO pins for 7.3" e-paper display (SPI3)
    // DC=GPIO8, CS=GPIO9, SCK=GPIO10, MOSI=GPIO11, RST=GPIO12, BUSY=GPIO13

    info!("Initializing e-paper display (fast mode)...");

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

    // Manual hardware reset before init (matches C driver timing)
    rst.set_high();
    delay.delay_ms(50);
    rst.set_low();
    delay.delay_ms(20);
    rst.set_high();
    delay.delay_ms(50);

    let mut epd = Epd7in3e::new(spi_device, busy, dc, rst, &mut delay, RefreshMode::Fast)
        .expect("EPD init failed");
    info!("EPD initialized!");

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
    info!("Starting display update...");
    info!("Server URL: {}", SERVER_URL);
    info!("Refresh interval: {} seconds", REFRESH_INTERVAL_SECS);

    // Allocate framebuffer (uses PSRAM for the 192KB buffer)
    info!("Allocating framebuffer...");
    let mut framebuffer = Framebuffer::new();
    info!("Framebuffer allocated!");

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
                info!("Initializing WiFi (deferred)...");
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
                info!("WiFi ready!");
            }
        }};
    }

    // Fetch widget data (use cache if available, then refresh from network)
    // Keep boxed to avoid 6KB on stack
    info!("Fetching widget data...");
    let mut items: Box<WidgetData> = if let Some(cached) = cached_items {
        info!("Using cached widget data ({} items)", cached.len());
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
                    if let Some(cache) = sd_cache.as_mut()
                        && let Err(e) = cache.store_widget_data(&data)
                    {
                        info!("Failed to cache widget data: {:?}", e);
                    }
                    break data;
                }
                Err(e) => {
                    info!("Failed to fetch widget data: {:?}, retrying in 30s...", e);
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
        info!(
            "Resuming with partial update: slot={}, slot_items=[{}, {}], index={}",
            saved_next_slot, saved_slot_items[0], saved_slot_items[1], saved_index
        );
        (saved_index, saved_next_slot, saved_slot_items, true)
    } else if data_matches {
        info!("Resuming from index {} (full refresh)", saved_index);
        (saved_index, 0u8, [0usize, 0usize], false)
    } else {
        info!("Fresh start or data changed");
        (0, 0u8, [0usize, 0usize], false)
    };

    let total_items = items.len();
    info!("Displaying {} items in shuffled order", total_items);

    // Buffer for partial updates (400x480 = 96000 bytes)
    const HALF_BUFFER_SIZE: usize = 400 * 480 / 2;

    // Display loop - allows re-display on orientation change
    loop {
        // If we've shown all items, start over
        if index >= total_items {
            info!("All items shown, starting over");
            index = 0;
        }

        // Wake up display
        info!("Waking up display...");
        epd.wake_up(&mut delay).expect("Failed to wake display");

        // Read battery percentage
        let battery_percent = {
            let mut buf = [0u8; 1];
            match i2c.write_read(AXP2101_ADDR, &[BAT_PERCENT_REG], &mut buf) {
                Ok(()) => {
                    info!("Battery: {}%", buf[0]);
                    buf[0]
                }
                Err(e) => {
                    info!("Failed to read battery: {:?}", e);
                    50 // Default to 50% on error
                }
            }
        };

        let display_result = if use_partial && orientation == Orientation::Horizontal {
            // ==================== Partial Refresh Mode (Cache-Aware) ====================
            // Only update one half of the display with a single new item
            let item_idx = index % total_items;
            let item_path = items[item_idx].as_str();
            info!(
                "Partial update: slot={}, item={} of {}",
                next_slot, item_idx, total_items
            );

            // PNG buffer for fetching/reading (256KB)
            let mut png_buf: alloc::boxed::Box<[u8; 256 * 1024]> =
                alloc::boxed::Box::new([0u8; 256 * 1024]);

            start_blink();

            // Check cache first
            let cache_hit = sd_cache
                .as_mut()
                .is_some_and(|c| c.has_image(item_path, Orientation::Horizontal));
            let png_len = if cache_hit {
                info!("Cache HIT: {}", item_path);
                sd_cache
                    .as_mut()
                    .and_then(|c| {
                        c.read_image(item_path, Orientation::Horizontal, &mut *png_buf)
                            .ok()
                    })
                    .unwrap_or_default()
            } else {
                info!("Cache MISS: {}", item_path);
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
                        if let Some(cache) = sd_cache.as_mut()
                            && let Err(e) = cache.write_image(
                                item_path,
                                Orientation::Horizontal,
                                &png_buf[..len],
                            )
                        {
                            info!("Cache store failed: {:?}", e);
                        }
                        len
                    }
                    Err(e) => {
                        info!("Fetch failed: {:?}", e);
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

                    info!("Partial refresh: x={}, w={}, h={}", x_offset, 400, 480);

                    epd.partial_update_start(&rect, &half_buffer, &mut delay)
                        .is_ok()
                }
                Err(_) => false,
            };

            // Update slot tracking early so prefetch uses correct next index
            if display_started {
                slot_items[next_slot as usize] = item_idx;
                next_slot = (next_slot + 1) % 2;
                index += 1; // Advance by 1 for partial updates
            }

            // Spawn button monitor task and do work while it runs
            if display_started {
                // Start button monitoring
                BUTTON_STATE.store(BUTTON_POLLING, Ordering::Relaxed);
                spawner.spawn(button_monitor_task(key_input)).ok();

                // Initialize and connect WiFi now if we deferred it
                ensure_wifi!();

                // Prefetch next image (only if cache is available)
                if let Some(cache) = sd_cache.as_mut() {
                    let prefetch_idx = index % total_items;
                    let prefetch_path = items[prefetch_idx].as_str();
                    if !cache.has_image(prefetch_path, Orientation::Horizontal) {
                        info!("Prefetching next image: {}", prefetch_path);
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
                            if let Err(e) = cache.write_image(
                                prefetch_path,
                                Orientation::Horizontal,
                                &prefetch_buf[..len],
                            ) {
                                info!("Prefetch cache store failed: {:?}", e);
                            } else {
                                info!("Prefetched and cached: {}", prefetch_path);
                            }
                        }
                    }
                }

                // Refresh widget data from server if we used cached data
                if has_cached_data {
                    info!("Refreshing widget data from server...");
                    if let Ok(fresh_items) = display::fetch_widget_data(
                        tcp_client.as_ref().unwrap(),
                        dns_socket.as_ref().unwrap(),
                        &mut *tls_read_buf,
                        &mut *tls_write_buf,
                        SERVER_URL,
                        "concerts",
                    )
                    .await
                        && (fresh_items.len() != items.len()
                            || fresh_items
                                .iter()
                                .zip(items.iter())
                                .any(|(a, b)| a.as_str() != b.as_str()))
                    {
                        info!("Widget data changed, updating cache");
                        if let Some(cache) = sd_cache.as_mut() {
                            if let Err(e) = cache.store_widget_data(&fresh_items) {
                                info!("Failed to update widget data cache: {:?}", e);
                            }
                            if let Ok(count) = cache.cleanup_stale(&fresh_items)
                                && count > 0
                            {
                                info!("Invalidated {} stale cache entries", count);
                            }
                        }
                    }
                }

                // Wait for display busy (button task handles button detection separately)
                while epd.is_busy() {
                    Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
                }
            }

            // Finish display
            let result = if display_started {
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
            info!(
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
                let cache_hit = sd_cache
                    .as_mut()
                    .is_some_and(|c| c.has_image(item_path, orientation));
                let png_len = if cache_hit {
                    info!("Cache HIT: {}", item_path);
                    sd_cache
                        .as_mut()
                        .and_then(|c| c.read_image(item_path, orientation, &mut *png_buf).ok())
                        .unwrap_or_default()
                } else {
                    info!("Cache MISS: {}", item_path);
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
                            if let Some(cache) = sd_cache.as_mut()
                                && let Err(e) =
                                    cache.write_image(item_path, orientation, &png_buf[..len])
                            {
                                info!("Cache store failed: {:?}", e);
                            }
                            len
                        }
                        Err(e) => {
                            info!("Fetch failed: {:?}", e);
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
                        info!("Render failed: {:?}", e);
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
                    info!("Updating display (full refresh)...");
                    epd.display_start(framebuffer.as_slice(), &mut delay)
                        .is_ok()
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

            // Spawn button monitor task and do work while it runs
            if display_started {
                // Start button monitoring
                BUTTON_STATE.store(BUTTON_POLLING, Ordering::Relaxed);
                spawner.spawn(button_monitor_task(key_input)).ok();

                // Initialize and connect WiFi now if we deferred it (using cached data path)
                ensure_wifi!();

                // Prefetch next image (only if cache is available)
                if let Some(cache) = sd_cache.as_mut() {
                    let prefetch_idx = index % total_items;
                    let prefetch_path = items[prefetch_idx].as_str();
                    if !cache.has_image(prefetch_path, orientation) {
                        info!("Prefetching next image: {}", prefetch_path);
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
                                cache.write_image(prefetch_path, orientation, &prefetch_buf[..len])
                            {
                                info!("Prefetch cache store failed: {:?}", e);
                            } else {
                                info!("Prefetched and cached: {}", prefetch_path);
                            }
                        }
                    }
                }
                embassy_futures::yield_now().await;

                // Refresh widget data from server if we used cached data
                if has_cached_data {
                    info!("Refreshing widget data from server...");
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
                            info!("Widget data changed, updating cache");
                            if let Some(cache) = sd_cache.as_mut() {
                                if let Err(e) = cache.store_widget_data(&fresh_items) {
                                    info!("Failed to update widget data cache: {:?}", e);
                                }
                                // Invalidate stale image cache entries
                                if let Ok(count) = cache.cleanup_stale(&fresh_items)
                                    && count > 0
                                {
                                    info!("Invalidated {} stale cache entries", count);
                                }
                            }
                        }
                    }
                }
                stop_blink();

                // Wait for display busy (button task handles button detection separately)
                while epd.is_busy() {
                    Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
                }
            }

            // Finish display
            let result = if display_started {
                epd.finish_display(&mut delay)
                    .map_err(|_| display::DisplayError::Network)
            } else {
                Err(display::DisplayError::Network)
            };

            embassy_futures::yield_now().await;

            result
        };

        match display_result {
            Ok(()) => info!("Display refresh successful!"),
            Err(e) => info!("Display refresh failed: {:?}", e),
        }

        // Put display to sleep
        info!("Putting display to sleep...");
        epd.sleep(&mut delay).expect("Failed to sleep display");

        // Check button state and cancel task if still polling
        let button_state = BUTTON_STATE.swap(BUTTON_CANCELLED, Ordering::Relaxed);

        // Handle button action detected during display update
        // (LED feedback already provided by button monitor task)
        match button_state {
            BUTTON_FLIP => {
                info!("Button held during update! Toggling orientation...");
                orientation = orientation.toggle();
                // Save to SD card
                if let Some(cache) = sd_cache.as_mut()
                    && let Err(e) = cache.store_orientation(orientation)
                {
                    info!("Failed to store orientation: {:?}", e);
                }
                // Reset partial mode on orientation change
                use_partial = false;
                slot_items = [0, 0];
                next_slot = 0;

                info!("Re-displaying with orientation: {:?}", orientation);
                // Continue loop to re-display
            }
            BUTTON_NEXT => {
                info!("Button tap during update, next item (index={})", index);
                // Continue loop to show next item
            }
            _ => {
                // No button press (POLLING or CANCELLED), exit loop and go to deep sleep
                info!("No button press, entering deep sleep");
                break;
            }
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
    info!(
        "Saved state: index={}, total={}, orientation={:?}, next_slot={}, slot_items=[{}, {}]",
        index, total_items, orientation, next_slot, slot_items[0], slot_items[1]
    );

    // Disconnect WiFi before deep sleep (only if it was initialized)
    if let Some(ctrl) = wifi_controller.as_mut() {
        info!("Disconnecting WiFi for deep sleep...");
        wifi_disconnect(ctrl).await;
    } else {
        info!("WiFi was never initialized, skipping disconnect");
    }

    // Reclaim GPIO4 for deep sleep wake source
    let key_pin = unsafe { esp_hal::peripherals::GPIO4::steal() };

    // Enter deep sleep
    info!(
        "Entering deep sleep for {} seconds (press button to wake early)...",
        REFRESH_INTERVAL_SECS
    );
    enter_deep_sleep(&mut rtc, key_pin, &mut delay, REFRESH_INTERVAL_SECS);
}

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

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

/// Connect to WiFi network
async fn wifi_connect(controller: &mut WifiController<'static>) {
    start_fast_blink();
    info!("Device capabilities: {:?}", controller.capabilities());

    if !matches!(controller.is_started(), Ok(true)) {
        let client_config = ModeConfig::Client(
            ClientConfig::default()
                .with_ssid(SSID.into())
                .with_password(PASSWORD.into()),
        );
        controller.set_config(&client_config).unwrap();
        info!("Starting WiFi...");
        controller.start_async().await.unwrap();
        info!("WiFi started!");
    }

    info!("Connecting to {}...", SSID);
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                info!("WiFi connected!");
                stop_blink();
                break;
            }
            Err(e) => {
                info!("Failed to connect: {e:?}, retrying...");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Disconnect and stop WiFi to save power
async fn wifi_disconnect(controller: &mut WifiController<'static>) {
    if let Err(e) = controller.disconnect_async().await {
        info!("Disconnect error (may already be disconnected): {:?}", e);
    }
    if let Err(e) = controller.stop_async().await {
        info!("Stop error: {:?}", e);
    }
    info!("WiFi stopped");
}

/// Wait for network stack to get an IP address
async fn wait_for_ip(stack: Stack<'static>) {
    info!("Waiting for link...");
    loop {
        if stack.is_link_up() {
            break;
        }
        // 1500ms polling is sufficient - link up is not time-critical
        Timer::after(Duration::from_millis(1500)).await;
    }
    info!("Link up!");

    info!("Waiting for IP...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        // 1500ms polling is sufficient - DHCP takes seconds anyway
        Timer::after(Duration::from_millis(1500)).await;
    }
}
