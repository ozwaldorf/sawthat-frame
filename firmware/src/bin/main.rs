//! SawThat Frame Firmware - ESP32-S3 E-Paper Photo Frame
//!
//! Environment variables required:
//! - WIFI_SSID: WiFi network name
//! - WIFI_PASS: WiFi password
//!
//! Optional:
//! - SAWTHAT_USER_ID: SawThat user whose concerts are shown
//! - SAWTHAT_FONT (render crate): font file for artwork text

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use core::time::Duration as CoreDuration;
use log::info;

use embassy_executor::Spawner;
use embassy_net::{
    Runner, Stack, StackResources,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Delay, Duration, Timer, with_timeout};
use embedded_hal::delay::DelayNs;
use embedded_hal::spi::SpiDevice;
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
use sawthat_frame_firmware::display;
use sawthat_frame_firmware::epd::{Epd7in3e, Rect, RefreshMode, WIDTH};
use sawthat_frame_firmware::framebuffer::Framebuffer;
use sawthat_frame_firmware::progress;
use sawthat_frame_firmware::sync::{self, TLS_READ_BUF_SIZE, TLS_WRITE_BUF_SIZE};
use sawthat_frame_firmware::widget::{Orientation, WidgetData};
use sawthat_render::Renderer;

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
/// SawThat user whose concerts are shown
const SAWTHAT_USER_ID: &str = match option_env!("SAWTHAT_USER_ID") {
    Some(id) => id,
    None => "a320940a-b493-4515-9f25-d393ebb540e6",
};

/// Maximum time to connect to WiFi and obtain an IP
const WIFI_TIMEOUT: Duration = Duration::from_secs(60);
/// TCP socket buffer sizes
const TCP_RX_SIZE: usize = 8192;
const TCP_TX_SIZE: usize = 1024;
/// Largest stored image (vertical 480x800 at 4bpp plus header)
const IMAGE_BUF_SIZE: usize = 480 * 800 / 2 + sawthat_render::IMAGE_HEADER;
/// Half-screen partial update buffer (400x480 at 4bpp)
const HALF_BUFFER_SIZE: usize = 400 * 480 / 2;

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

/// LED command sent via signal
#[derive(Clone, Copy)]
enum LedCommand {
    /// Flash green LED N times
    GreenFlash(u8),
    /// Start red LED blinking at interval (ms)
    RedBlink(u16),
    /// Stop red LED blinking (keep LED on)
    RedSolid,
}

/// Signal to wake LED task when commands are sent
static LED_SIGNAL: Signal<CriticalSectionRawMutex, LedCommand> = Signal::new();

/// Combined LED task - handles red LED blinking and green LED flash requests
/// Uses signal-based waking to avoid continuous polling
#[embassy_executor::task]
async fn led_task(led_red: &'static mut Output<'static>, led_green: &'static mut Output<'static>) {
    let mut blink_enabled = false;
    let mut blink_interval_ms: u64 = 500;

    loop {
        if blink_enabled {
            // When blinking, use select to handle either signal or blink timer
            use embassy_futures::select::{Either, select};

            match select(
                LED_SIGNAL.wait(),
                Timer::after(Duration::from_millis(blink_interval_ms)),
            )
            .await
            {
                Either::First(cmd) => {
                    // Process command
                    match cmd {
                        LedCommand::GreenFlash(count) => {
                            for _ in 0..count {
                                led_green.set_low(); // ON
                                Timer::after(Duration::from_millis(100)).await;
                                led_green.set_high(); // OFF
                                Timer::after(Duration::from_millis(100)).await;
                            }
                        }
                        LedCommand::RedBlink(interval) => {
                            blink_enabled = true;
                            blink_interval_ms = interval as u64;
                        }
                        LedCommand::RedSolid => {
                            blink_enabled = false;
                            led_red.set_low(); // ON (active low)
                        }
                    }
                }
                Either::Second(()) => {
                    // Blink timer fired - toggle LED
                    led_red.toggle();
                }
            }
        } else {
            // Not blinking - just wait for signal (no polling!)
            let cmd = LED_SIGNAL.wait().await;
            match cmd {
                LedCommand::GreenFlash(count) => {
                    for _ in 0..count {
                        led_green.set_low(); // ON
                        Timer::after(Duration::from_millis(100)).await;
                        led_green.set_high(); // OFF
                        Timer::after(Duration::from_millis(100)).await;
                    }
                }
                LedCommand::RedBlink(interval) => {
                    blink_enabled = true;
                    blink_interval_ms = interval as u64;
                }
                LedCommand::RedSolid => {
                    led_red.set_low(); // ON (active low)
                }
            }
        }
    }
}

/// Start blinking the red LED (normal speed - 500ms)
fn start_blink() {
    LED_SIGNAL.signal(LedCommand::RedBlink(500));
}

/// Start fast blinking the red LED (100ms)
fn start_fast_blink() {
    LED_SIGNAL.signal(LedCommand::RedBlink(100));
}

/// Stop blinking and keep red LED solid on
fn stop_blink() {
    LED_SIGNAL.signal(LedCommand::RedSolid);
}

/// Flash green LED the specified number of times
fn flash_green(count: u8) {
    LED_SIGNAL.signal(LedCommand::GreenFlash(count));
}

/// Signal to wake button monitor task
static BUTTON_MONITOR_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Start button monitoring (signals the persistent task)
fn start_button_monitor() {
    BUTTON_STATE.store(BUTTON_POLLING, Ordering::Relaxed);
    BUTTON_MONITOR_SIGNAL.signal(());
}

/// Button monitor task - persistent task that monitors button when signaled
/// Waits for signal when inactive (no polling), monitors button when active
#[embassy_executor::task]
async fn button_monitor_task(key_input: &'static Input<'static>) {
    loop {
        // Wait for signal to start monitoring (no polling when inactive)
        BUTTON_MONITOR_SIGNAL.wait().await;

        // Monitor button until state changes from BUTTON_POLLING
        while BUTTON_STATE.load(Ordering::Relaxed) == BUTTON_POLLING {
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
                            flash_green(3);
                        }
                        break;
                    }

                    hold_time += BUTTON_POLL_MS as u32;
                    Timer::after(Duration::from_millis(BUTTON_POLL_MS)).await;
                }

                // If we detected a hold, go back to waiting
                if BUTTON_STATE.load(Ordering::Relaxed) != BUTTON_POLLING {
                    break;
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
                    flash_green(1);
                }
                break;
            }

            Timer::after(Duration::from_millis(BUTTON_POLL_MS)).await;
        }
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

    // PSRAM is registered first so general allocations land there, leaving internal RAM
    // for allocations that request it explicitly (WiFi buffers, RTOS task stacks)
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // RTOS provides the embassy time driver, so it must start before any Timer is awaited
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT)
            .software_interrupt0,
    );

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

    // Make key_input static for use in button monitor task
    let key_input: &'static Input<'static> = mk_static!(Input<'static>, key_input);

    // Spawn persistent button monitor task (waits on signal when inactive)
    spawner.spawn(button_monitor_task(key_input)).ok();

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
            flash_green(3);
        } else {
            // Button released before 500ms - advance to next item
            BUTTON_STATE.store(BUTTON_NEXT, Ordering::Relaxed);
            // Request 1 flash for next item
            flash_green(1);
        }
    }

    // ==================== Normal Boot Sequence ====================
    // Now do the heavier initialization
    info!("Boot! Wake reason: {:?}", wake_reason);

    let mut delay = Delay;
    let cold_boot = !matches!(
        wake_reason,
        esp_hal::system::SleepSource::Timer | esp_hal::system::SleepSource::Ext0
    );

    // ==================== SD Card Initialization ====================
    // SD card SPI pins: CS=GPIO38, CLK=GPIO39, MISO=GPIO40, MOSI=GPIO41
    info!("Initializing SD card...");

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
            info!("SD card init failed: {:?} (artwork storage unavailable)", e);
            None
        }
    };

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
    // WiFi is only initialized when a sync is needed
    let mut wifi_peripheral: Option<esp_hal::peripherals::WIFI<'static>> = Some(peripherals.WIFI);

    // Kept alive so the radio stays initialized
    let mut _esp_radio_ctrl: Option<&'static Controller<'static>> = None;
    let mut wifi_controller: Option<WifiController<'static>> = None;
    let mut wifi_connected = false;

    // ==================== RTC for Deep Sleep ====================
    let mut rtc = Rtc::new(peripherals.LPWR);

    info!("Refresh interval: {} seconds", REFRESH_INTERVAL_SECS);

    // Framebuffer (192KB, PSRAM)
    let mut framebuffer = Framebuffer::new();

    // Buffers for stored artwork and partial updates (PSRAM)
    let mut image_buf: Box<[u8; IMAGE_BUF_SIZE]> = Box::new([0u8; IMAGE_BUF_SIZE]);
    let mut half_buffer: Box<[u8; HALF_BUFFER_SIZE]> = Box::new([0u8; HALF_BUFFER_SIZE]);

    let rng = Rng::new();

    // TLS buffers for HTTPS
    let mut tls_read_buf: Box<[u8; TLS_READ_BUF_SIZE]> = Box::new([0u8; TLS_READ_BUF_SIZE]);
    let mut tls_write_buf: Box<[u8; TLS_WRITE_BUF_SIZE]> = Box::new([0u8; TLS_WRITE_BUF_SIZE]);

    // Network resources - created on first WiFi init
    let mut tcp_client: Option<TcpClient<'static, 1, TCP_RX_SIZE, TCP_TX_SIZE>> = None;
    let mut dns_socket: Option<DnsSocket<'static>> = None;
    let mut net_stack: Option<Stack<'static>> = None;

    // Initialize WiFi (once) and connect, evaluating to whether it is connected
    macro_rules! ensure_wifi {
        () => {{
            if !wifi_connected {
                if wifi_controller.is_none() {
                    info!("Initializing WiFi...");
                    start_fast_blink();

                    let ctrl = esp_radio::init().unwrap();
                    let ctrl = mk_static!(Controller<'static>, ctrl);

                    let wifi = wifi_peripheral.take().unwrap();
                    // AMPDU RX bursts were dropped by the driver, stalling large downloads
                    let wifi_config = WifiConfig::default().with_ampdu_rx_enable(false);
                    let (wifi_ctrl, ifaces) =
                        esp_radio::wifi::new(ctrl, wifi, wifi_config).unwrap();

                    let (stk, runner) = embassy_net::new(
                        ifaces.sta,
                        embassy_net::Config::dhcpv4(Default::default()),
                        mk_static!(StackResources<3>, StackResources::<3>::new()),
                        rng.random() as u64,
                    );
                    spawner.spawn(net_task(runner)).ok();

                    let tcp_state = mk_static!(
                        TcpClientState<1, TCP_RX_SIZE, TCP_TX_SIZE>,
                        TcpClientState::new()
                    );
                    tcp_client = Some(TcpClient::new(stk, tcp_state));
                    dns_socket = Some(DnsSocket::new(stk));
                    net_stack = Some(stk);
                    _esp_radio_ctrl = Some(ctrl);
                    wifi_controller = Some(wifi_ctrl);
                }

                let stack = net_stack.unwrap();
                let ctrl = wifi_controller.as_mut().unwrap();
                wifi_connected = with_timeout(WIFI_TIMEOUT, async {
                    wifi_connect(ctrl).await;
                    wait_for_ip(stack).await;
                })
                .await
                .is_ok();

                if wifi_connected {
                    info!("WiFi ready!");
                } else {
                    info!("WiFi connection timed out");
                    wifi_disconnect(ctrl).await;
                }
            }
            wifi_connected
        }};
    }

    // ==================== Item List ====================
    let mut items: Box<WidgetData> = Box::new(
        sd_cache
            .as_mut()
            .and_then(|c| c.load_widget_data(orientation))
            .unwrap_or_default(),
    );
    info!("{} stored items for {:?}", items.len(), orientation);

    {
        // Get saved state if resuming
        let (mut shuffle_seed, saved_index, saved_next_slot, saved_slot_items) = if resuming {
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
            let seed = (rng.random() as u64) << 32 | rng.random() as u64;
            (seed, 0, 0u8, [0usize, 0usize])
        };

        // Shuffle items (same seed = same order)
        display::shuffle_items(&mut items, shuffle_seed);

        // Check if data matches (after shuffling, so items are in the same order)
        let (data_matches, saved_orientation) = if resuming {
            unsafe {
                let state = &raw const SLEEP_STATE;
                ((*state).matches_data(&items), (*state).get_orientation())
            }
        } else {
            (false, Orientation::Horizontal)
        };

        // Sync on power-on, and whenever there is nothing to show
        let mut need_sync = cold_boot || items.is_empty();

        let can_partial = data_matches
            && !need_sync
            && orientation == Orientation::Horizontal
            && saved_orientation == Orientation::Horizontal
            && saved_index >= 2;

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

        let mut failures = 0usize;

        // Track the items shown by a full refresh starting at `index`
        macro_rules! advance_full {
            () => {{
                if orientation == Orientation::Horizontal {
                    slot_items = [index % items.len(), (index + 1) % items.len()];
                    next_slot = 0;
                    index += 2;
                    use_partial = true;
                } else {
                    index += 1;
                }
            }};
        }

        // Sync cycle: show the next cached screen (with the progress dialog when there is
        // work), render missing artwork for the current orientation while the panel
        // refreshes, show the first rendered screen if nothing was cached, then erase the
        // dialog. Evaluates to whether a refresh was started.
        macro_rules! sync_cycle {
            ($battery:expr) => {{
                let per_screen = screen_items(orientation);
                let mut shown = false;
                let mut synced: Option<Box<WidgetData>> = None;

                // Compose the next cached screen now; it is shown once the work is known
                let cached: Vec<&str> = (0..per_screen.min(items.len()))
                    .map(|slot| items[(index + slot) % items.len()].as_str())
                    .collect();
                let cached_ready = !cached.is_empty()
                    && sd_cache.as_mut().is_some_and(|c| {
                        compose_screen(
                            c,
                            &mut framebuffer,
                            &mut *image_buf,
                            &cached,
                            orientation,
                            $battery,
                        )
                    });

                if let Some(cache) = sd_cache.as_mut()
                    && ensure_wifi!()
                {
                    start_blink();
                    let mut net = sync::Net {
                        tcp: tcp_client.as_ref().unwrap(),
                        dns: dns_socket.as_ref().unwrap(),
                        tls_read: &mut *tls_read_buf,
                        tls_write: &mut *tls_write_buf,
                    };

                    let concerts = match sync::fetch_bands(&mut net, SAWTHAT_USER_ID)
                        .await
                        .ok()
                        .and_then(|json| Some((sync::parse_concerts(&json)?, json)))
                    {
                        Some((concerts, json)) => {
                            if let Err(e) = cache.store_bands(&json) {
                                info!("Failed to store band data: {:?}", e);
                            }
                            Some(concerts)
                        }
                        None => {
                            info!("Concert fetch failed, using stored band data");
                            cache
                                .load_bands()
                                .and_then(|json| sync::parse_concerts(&json))
                        }
                    };

                    if let Some(concerts) = concerts {
                        let missing: Vec<usize> = (0..concerts.len())
                            .filter(|&i| !cache.has_image(&concerts[i].path, orientation))
                            .collect();
                        let total = missing.len();
                        info!(
                            "{} of {} concerts need {:?} artwork",
                            total,
                            concerts.len(),
                            orientation
                        );

                        let mut ready_flags = alloc::vec![true; concerts.len()];
                        for &i in &missing {
                            ready_flags[i] = false;
                        }
                        let mut progress =
                            (total > 0).then(|| progress::Progress::new(orientation, total));

                        // Show the cached screen, with the dialog when there is work
                        if cached_ready {
                            if let Some(p) = progress.as_mut() {
                                p.draw(&mut framebuffer, 0);
                            }
                            if epd
                                .display_start(framebuffer.as_slice(), &mut delay)
                                .is_ok()
                            {
                                shown = true;
                                advance_full!();
                                start_button_monitor();
                                if let Some(p) = progress.as_mut() {
                                    p.confirm();
                                }
                            }
                        } else {
                            // No cached list: use artwork already on the card, if any
                            let on_card: Vec<&str> = concerts
                                .iter()
                                .zip(&ready_flags)
                                .filter(|&(_, &ready)| ready)
                                .map(|(c, _)| c.path.as_str())
                                .take(per_screen)
                                .collect();
                            if on_card.len() == per_screen
                                && compose_screen(
                                    cache,
                                    &mut framebuffer,
                                    &mut *image_buf,
                                    &on_card,
                                    orientation,
                                    $battery,
                                )
                            {
                                if let Some(p) = progress.as_mut() {
                                    p.draw(&mut framebuffer, 0);
                                }
                                if epd
                                    .display_start(framebuffer.as_slice(), &mut delay)
                                    .is_ok()
                                {
                                    shown = true;
                                    start_button_monitor();
                                    if let Some(p) = progress.as_mut() {
                                        p.confirm();
                                    }
                                }
                            } else if let Some(p) = progress.as_mut() {
                                let (rect, region) = p.draw(&mut framebuffer, 0);
                                if epd.partial_update_start(&rect, &region, &mut delay).is_ok() {
                                    p.confirm();
                                }
                            }
                        }

                        let renderer = Renderer::new();
                        let mut albums = sync::AlbumCache::new();
                        let mut rendered: Vec<usize> = Vec::new();
                        for (done, &i) in missing.iter().enumerate() {
                            // Refresh the new bar slice once the panel is free
                            if let Some(p) = progress.as_mut()
                                && !epd.is_busy()
                                && let Some((rect, region)) = p.update(&mut framebuffer, done)
                                && epd.partial_update_start(&rect, &region, &mut delay).is_ok()
                            {
                                p.confirm();
                            }

                            let concert = &concerts[i];
                            match sync::render_concert(
                                &mut net,
                                &mut albums,
                                &renderer,
                                concert,
                                layout_size(orientation),
                            )
                            .await
                            {
                                Ok(image) => {
                                    match cache.write_image(&concert.path, orientation, &image) {
                                        Ok(()) => {
                                            ready_flags[i] = true;
                                            rendered.push(i);
                                        }
                                        Err(e) => {
                                            info!("Failed to store {}: {:?}", concert.path, e)
                                        }
                                    }
                                }
                                Err(e) => info!("Skipping {}: {:?}", concert.path, e),
                            }

                            // With nothing cached, show the first screen as soon as it renders
                            if !shown && rendered.len() >= per_screen {
                                while epd.is_busy() {
                                    Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
                                }
                                let paths: Vec<&str> = rendered[..per_screen]
                                    .iter()
                                    .map(|&i| concerts[i].path.as_str())
                                    .collect();
                                if compose_screen(
                                    cache,
                                    &mut framebuffer,
                                    &mut *image_buf,
                                    &paths,
                                    orientation,
                                    $battery,
                                ) {
                                    if let Some(p) = progress.as_mut() {
                                        p.draw(&mut framebuffer, done + 1);
                                    }
                                    if epd
                                        .display_start(framebuffer.as_slice(), &mut delay)
                                        .is_ok()
                                    {
                                        shown = true;
                                        start_button_monitor();
                                        if let Some(p) = progress.as_mut() {
                                            p.confirm();
                                        }
                                    }
                                }
                            }
                        }

                        let mut ready: Box<WidgetData> = Box::new(heapless::Vec::new());
                        let mut valid: Box<WidgetData> = Box::new(heapless::Vec::new());
                        for (concert, &is_ready) in concerts.iter().zip(&ready_flags) {
                            let mut path = heapless::String::new();
                            if path.push_str(&concert.path).is_ok() {
                                if is_ready {
                                    let _ = ready.push(path.clone());
                                }
                                let _ = valid.push(path);
                            }
                        }
                        info!("{} {:?} items ready", ready.len(), orientation);

                        // Keep artwork for every current concert in both orientations
                        match cache.cleanup_stale(&valid) {
                            Ok(n) if n > 0 => info!("Removed {} stale files", n),
                            Ok(_) => {}
                            Err(e) => info!("Cleanup failed: {:?}", e),
                        }
                        if let Err(e) = cache.store_widget_data(orientation, &ready) {
                            info!("Failed to store item list: {:?}", e);
                        }

                        // Erase the dialog
                        if let Some(p) = progress.as_mut() {
                            while epd.is_busy() {
                                Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
                            }
                            if let Some((rect, region)) = p.restore(&mut framebuffer) {
                                epd.partial_update_start(&rect, &region, &mut delay).ok();
                            }
                        }
                        synced = Some(ready);
                    }
                    stop_blink();
                }

                if wifi_connected {
                    if let Some(ctrl) = wifi_controller.as_mut() {
                        wifi_disconnect(ctrl).await;
                    }
                    wifi_connected = false;
                }

                // Without a sync, still show the cached screen
                if cached_ready
                    && !shown
                    && epd
                        .display_start(framebuffer.as_slice(), &mut delay)
                        .is_ok()
                {
                    shown = true;
                    advance_full!();
                    start_button_monitor();
                }

                if let Some(mut fresh) = synced {
                    display::shuffle_items(&mut fresh, shuffle_seed);
                    if *fresh != *items {
                        info!("Item list changed ({} items), reshuffling", fresh.len());
                        shuffle_seed = (rng.random() as u64) << 32 | rng.random() as u64;
                        display::shuffle_items(&mut fresh, shuffle_seed);
                        items = fresh;
                        index = 0;
                        slot_items = [0, 0];
                        next_slot = 0;
                    }
                }
                if shown {
                    use_partial = orientation == Orientation::Horizontal;
                }
                shown
            }};
        }

        loop {
            // After showing every item, sync before starting over
            if !items.is_empty() && index >= items.len() {
                info!("All items shown");
                index = 0;
                need_sync = true;
            }

            info!("Waking up display...");
            epd.wake_up(&mut delay).expect("Failed to wake display");

            let battery_percent = match log_charger_status(&mut i2c) {
                Some(mv) => battery_percent_from_mv(mv),
                None => {
                    let mut buf = [0u8; 1];
                    i2c.write_read(AXP2101_ADDR, &[BAT_PERCENT_REG], &mut buf)
                        .map(|_| buf[0])
                        .unwrap_or(50)
                }
            };
            info!("Battery: {}%", battery_percent);

            let display_started = if need_sync {
                need_sync = false;
                info!("Syncing {:?} artwork", orientation);
                let shown = sync_cycle!(battery_percent);
                if !shown && !items.is_empty() {
                    // Synced without showing a screen: display normally
                    continue;
                }
                shown
            } else if items.is_empty() {
                false
            } else if use_partial && orientation == Orientation::Horizontal {
                // ==================== Partial Refresh ====================
                // Update one half of the display with a single new item
                let item_idx = index % items.len();
                let item_path = items[item_idx].as_str();
                info!(
                    "Partial update: slot={}, item={} of {}",
                    next_slot,
                    item_idx,
                    items.len()
                );

                let drawn = sd_cache.as_mut().is_some_and(|c| {
                    draw_stored(
                        c,
                        &mut framebuffer,
                        &mut *image_buf,
                        item_path,
                        Orientation::Horizontal,
                        next_slot,
                    )
                });
                if !drawn {
                    failures += 1;
                    index += 1;
                    if failures >= items.len() {
                        info!("No displayable items");
                        break;
                    }
                    continue;
                }
                draw_battery_icon(&mut framebuffer, orientation, battery_percent);

                framebuffer.extract_half(next_slot, &mut *half_buffer);
                let x_offset = if next_slot == 0 { 0 } else { 400 };
                let rect = Rect::new(x_offset, 0, 400, 480);
                info!("Partial refresh: x={}, w={}, h={}", x_offset, 400, 480);

                let started = epd
                    .partial_update_start(&rect, &*half_buffer, &mut delay)
                    .is_ok();
                if started {
                    slot_items[next_slot as usize] = item_idx;
                    next_slot = (next_slot + 1) % 2;
                    index += 1;
                    start_button_monitor();
                }
                started
            } else {
                // ==================== Full Refresh ====================
                // Update entire display with 2 items (horizontal) or 1 item (vertical)
                let paths: Vec<&str> = (0..screen_items(orientation))
                    .map(|slot| items[(index + slot) % items.len()].as_str())
                    .collect();
                info!(
                    "Full refresh: {} item(s) from index {} of {}",
                    paths.len(),
                    index,
                    items.len()
                );

                let drawn = sd_cache.as_mut().is_some_and(|c| {
                    compose_screen(
                        c,
                        &mut framebuffer,
                        &mut *image_buf,
                        &paths,
                        orientation,
                        battery_percent,
                    )
                });
                if !drawn {
                    failures += 1;
                    index += 1;
                    if failures >= items.len() {
                        info!("No displayable items");
                        break;
                    }
                    continue;
                }

                info!("Updating display (full refresh)...");
                let started = epd
                    .display_start(framebuffer.as_slice(), &mut delay)
                    .is_ok();
                if started {
                    advance_full!();
                    start_button_monitor();
                }
                started
            };

            if !display_started {
                info!("No artwork available, will retry after sleep");
                epd.sleep(&mut delay).ok();
                break;
            }
            failures = 0;

            // Wait for the panel while the button monitor runs
            while epd.is_busy() {
                Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
            }
            match epd.finish_display(&mut delay) {
                Ok(()) => info!("Display refresh successful!"),
                Err(e) => info!("Display refresh failed: {:?}", e),
            }

            info!("Putting display to sleep...");
            epd.sleep(&mut delay).expect("Failed to sleep display");

            // Check button state and stop monitoring
            let button_state = BUTTON_STATE.swap(BUTTON_CANCELLED, Ordering::Relaxed);

            // LED feedback was already provided by the button monitor task
            match button_state {
                BUTTON_FLIP => {
                    info!("Button held during update! Toggling orientation...");
                    orientation = orientation.toggle();
                    if let Some(cache) = sd_cache.as_mut()
                        && let Err(e) = cache.store_orientation(orientation)
                    {
                        info!("Failed to store orientation: {:?}", e);
                    }

                    // Each orientation has its own artwork; sync if none is cached
                    *items = sd_cache
                        .as_mut()
                        .and_then(|c| c.load_widget_data(orientation))
                        .unwrap_or_default();
                    display::shuffle_items(&mut items, shuffle_seed);
                    need_sync = items.is_empty();
                    index = 0;
                    use_partial = false;
                    slot_items = [0, 0];
                    next_slot = 0;
                    info!(
                        "Re-displaying with orientation: {:?} ({} items)",
                        orientation,
                        items.len()
                    );
                }
                BUTTON_NEXT => {
                    info!("Button tap during update, next item (index={})", index);
                }
                _ => {
                    info!("No button press, entering deep sleep");
                    break;
                }
            }
        }

        // Save state for next wake (index already advanced in the loop)
        unsafe {
            let state = &raw mut SLEEP_STATE;
            (*state).save(
                index,
                items.len(),
                shuffle_seed,
                orientation,
                next_slot,
                slot_items,
                &items,
            );
        }
        info!(
            "Saved state: index={}, total={}, orientation={:?}, next_slot={}, slot_items=[{}, {}]",
            index,
            items.len(),
            orientation,
            next_slot,
            slot_items[0],
            slot_items[1]
        );
    }

    if wifi_connected && let Some(ctrl) = wifi_controller.as_mut() {
        info!("Disconnecting WiFi for deep sleep...");
        wifi_disconnect(ctrl).await;
    }

    // Reclaim GPIO4 for deep sleep wake source
    let key_pin = unsafe { esp_hal::peripherals::GPIO4::steal() };

    info!(
        "Entering deep sleep for {} seconds (press button to wake early)...",
        REFRESH_INTERVAL_SECS
    );
    enter_deep_sleep(&mut rtc, key_pin, &mut delay, REFRESH_INTERVAL_SECS);
}

/// Approximate Li-ion state of charge from resting cell voltage
fn battery_percent_from_mv(mv: u16) -> u8 {
    const CURVE: [(u16, u8); 8] = [
        (3300, 0),
        (3600, 10),
        (3700, 30),
        (3800, 55),
        (3900, 70),
        (4000, 82),
        (4100, 93),
        (4200, 100),
    ];
    if mv <= CURVE[0].0 {
        return 0;
    }
    for pair in CURVE.windows(2) {
        let ((v0, p0), (v1, p1)) = (pair[0], pair[1]);
        if mv <= v1 {
            return p0 + ((mv - v0) as u32 * (p1 - p0) as u32 / (v1 - v0) as u32) as u8;
        }
    }
    100
}

/// Read one AXP2101 register, retrying on bus errors
fn pmic_read(i2c: &mut I2c<'_, esp_hal::Blocking>, reg: u8) -> Option<u8> {
    let mut buf = [0u8; 1];
    for attempt in 0..3 {
        match i2c.write_read(0x34, &[reg], &mut buf) {
            Ok(()) => return Some(buf[0]),
            Err(e) => info!("PMIC read {:#04x} attempt {} failed: {:?}", reg, attempt, e),
        }
    }
    None
}

/// Log AXP2101 charger state and VBUS presence, returning battery voltage in mV
fn log_charger_status(i2c: &mut I2c<'_, esp_hal::Blocking>) -> Option<u16> {
    const STATUS1: u8 = 0x00;
    const STATUS2: u8 = 0x01;
    const ADC_ENABLE: u8 = 0x30;
    const VBAT_H: u8 = 0x34;
    const VBAT_L: u8 = 0x35;

    if let Some(s1) = pmic_read(i2c, STATUS1) {
        info!(
            "PMIC status1={:#04x}: vbus_good={} battery_present={}",
            s1,
            s1 & 0x20 != 0,
            s1 & 0x08 != 0
        );
    }

    if let Some(s2) = pmic_read(i2c, STATUS2) {
        let direction = match (s2 >> 5) & 0x03 {
            0 => "standby",
            1 => "charging",
            2 => "discharging",
            _ => "unknown",
        };
        let stage = match s2 & 0x07 {
            0 => "trickle",
            1 => "pre-charge",
            2 => "constant-current",
            3 => "constant-voltage",
            4 => "done",
            5 => "not charging",
            _ => "unknown",
        };
        info!("PMIC status2={:#04x}: {} ({})", s2, direction, stage);
    }

    // Battery voltage ADC channel must be enabled to read VBAT
    if let Some(adc) = pmic_read(i2c, ADC_ENABLE)
        && adc & 0x01 == 0
    {
        let _ = i2c.write(0x34, &[ADC_ENABLE, adc | 0x01]);
    }
    let mv = ((pmic_read(i2c, VBAT_H)? as u16 & 0x3F) << 8) | pmic_read(i2c, VBAT_L)? as u16;
    info!("PMIC vbat={}mV", mv);
    // A zero reading means the ADC has not sampled yet
    (mv > 0).then_some(mv)
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

/// Items shown per screen: two halves in horizontal, one full screen in vertical
fn screen_items(orientation: Orientation) -> usize {
    match orientation {
        Orientation::Horizontal => 2,
        Orientation::Vertical => 1,
    }
}

/// Artwork dimensions for an orientation
fn layout_size(orientation: Orientation) -> (u32, u32) {
    match orientation {
        Orientation::Horizontal => sawthat_render::HORIZ_SIZE,
        Orientation::Vertical => sawthat_render::VERT_SIZE,
    }
}

/// Read a stored image and draw it into a framebuffer slot
fn draw_stored<S: SpiDevice, D: DelayNs>(
    cache: &mut SdCache<S, D>,
    framebuffer: &mut Framebuffer,
    buf: &mut [u8],
    path: &str,
    orientation: Orientation,
    slot: u8,
) -> bool {
    match cache.read_image(path, orientation, buf) {
        Ok(len) => match display::render_stored(&buf[..len], framebuffer, slot, orientation) {
            Ok(()) => true,
            Err(e) => {
                info!("Bad stored image {}: {:?}", path, e);
                false
            }
        },
        Err(e) => {
            info!("Failed to read {}: {:?}", path, e);
            false
        }
    }
}

/// Compose a full screen: stored artwork for each slot plus the battery icon
fn compose_screen<S: SpiDevice, D: DelayNs>(
    cache: &mut SdCache<S, D>,
    framebuffer: &mut Framebuffer,
    buf: &mut [u8],
    paths: &[&str],
    orientation: Orientation,
    battery_percent: u8,
) -> bool {
    framebuffer.clear(sawthat_frame_firmware::epd::Color::White);
    let drawn = paths
        .iter()
        .enumerate()
        .all(|(slot, path)| draw_stored(cache, framebuffer, buf, path, orientation, slot as u8));
    if drawn {
        draw_battery_icon(framebuffer, orientation, battery_percent);
    }
    drawn
}

/// Draw the battery icon: centered at the top in horizontal, top right in vertical
fn draw_battery_icon(framebuffer: &mut Framebuffer, orientation: Orientation, percent: u8) {
    let vertical = orientation == Orientation::Vertical;
    let (bat_w, _) = battery::battery_dimensions(vertical);
    let x = if vertical {
        WIDTH as u16 - bat_w - 8
    } else {
        (WIDTH as u16 - bat_w) / 2
    };
    battery::draw_battery(framebuffer.as_mut_slice(), x, 8, percent, vertical);
}
