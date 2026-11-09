//! SawThat Frame Firmware - ESP32-S3 E-Paper Photo Frame
//!
//! Configuration is read at boot from `CONFIG.TXT` on the SD card root (see
//! `sawthat_frame_firmware::config`). Without a usable config the frame shows a setup screen
//! and sleeps until KEY is pressed.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use core::time::Duration as CoreDuration;
use log::info;

use embassy_executor::Spawner;
use embassy_net::{
    Runner, Stack, StackResources,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Delay, Duration, Timer, with_timeout};
use embedded_hal::delay::DelayNs;
use embedded_hal::spi::SpiDevice;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::system::Stack as CoreStack;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull, RtcPinWithResistors},
    i2c::master::{Config as I2cConfig, I2c},
    otg_fs::Usb,
    ram,
    rng::Rng,
    rtc_cntl::{
        Rtc,
        sleep::{Ext0WakeupSource, Ext1WakeupSource, TimerWakeupSource, WakeupLevel},
    },
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::event::{EventExt, StaDisconnected};
use esp_radio::{
    Controller,
    wifi::{
        AuthMethod, ClientConfig, Config as WifiConfig, ModeConfig, WifiController, WifiDevice,
    },
};
use esp_rtos::embassy::Executor;
use sawthat_frame_firmware::TimestampLogger;
use sawthat_frame_firmware::battery;
use sawthat_frame_firmware::cache::{CacheError, SdCache};
use sawthat_frame_firmware::config::{self, Config, ConfigError, WifiCause};
use sawthat_frame_firmware::display;
use sawthat_frame_firmware::epd::{Epd7in3e, Rect, RefreshMode, WIDTH};
use sawthat_frame_firmware::framebuffer::Framebuffer;
use sawthat_frame_firmware::progress;
use sawthat_frame_firmware::sync::{self, TLS_READ_BUF_SIZE, TLS_WRITE_BUF_SIZE};
use sawthat_frame_firmware::usb_drive;
use sawthat_frame_firmware::widget::{Orientation, WidgetData};
use sawthat_render::Renderer;
use sawthat_render::concerts::Concert;

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

/// Maximum time to connect to WiFi and obtain an IP
const WIFI_TIMEOUT: Duration = Duration::from_secs(60);
/// Delay between WiFi connection attempts
const WIFI_RETRY_SECS: u64 = 5;
/// TCP socket buffer sizes (allocated in PSRAM, see `alloc_tcp_state`)
const TCP_RX_SIZE: usize = 8192;
const TCP_TX_SIZE: usize = 1024;
/// Largest stored image (vertical 480x800 at 4bpp plus header)
const IMAGE_BUF_SIZE: usize = 480 * 800 / 2 + sawthat_render::IMAGE_HEADER;
/// Half-screen partial update buffer (400x480 at 4bpp)
const HALF_BUFFER_SIZE: usize = 400 * 480 / 2;

/// Concert text font files on the SD root, in order of preference
const FONT_FILES: [&str; 2] = ["FONT.TTF", "FONT.OTF"];
/// Largest accepted font file on the SD card
const MAX_FONT: usize = 4 * 1024 * 1024;
/// Button hold threshold in milliseconds
const HOLD_THRESHOLD_MS: u32 = 500;
/// Button polling interval in milliseconds
const BUTTON_POLL_MS: u64 = 50;
/// How long the completed sync dialog stays on screen before it is erased
const DIALOG_HOLD_MS: u64 = 1000;
/// AXP2101 PMIC I2C address and fuel gauge percentage register
const AXP2101_ADDR: u8 = 0x34;
const BAT_PERCENT_REG: u8 = 0xA4;
/// Display busy polling interval in milliseconds (display refresh takes seconds)
const DISPLAY_BUSY_POLL_MS: u64 = 200;
/// Magic number to validate RTC memory state
const SLEEP_STATE_MAGIC: u32 = 0xCAFE_F00D;

/// RTC fast memory state - persists across deep sleep
#[esp_hal::ram(unstable(rtc_fast))]
static mut SLEEP_STATE: SleepState = SleepState::new();

/// RTC time (us) of the next timer wake, so a charger wake does not postpone it
#[esp_hal::ram(unstable(rtc_fast))]
static mut NEXT_WAKE_US: u64 = 0;

/// Set while sleeping on the setup screen, so the wake after it syncs like a power-on
#[esp_hal::ram(unstable(rtc_fast))]
static mut SETUP_PENDING: bool = false;

/// Hash of the setup screen on the panel (see `message_hash`), so an unchanged one is not redrawn
#[esp_hal::ram(unstable(rtc_fast))]
static mut SETUP_SHOWN: u32 = 0;

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

/// Last ESP-IDF station disconnect reason, and whether the station joined the network
static WIFI_REASON: AtomicU8 = AtomicU8::new(0);
static WIFI_LINKED: AtomicBool = AtomicBool::new(false);
/// Whether an attempt in the current connection failed authentication
static WIFI_AUTH_FAILED: AtomicBool = AtomicBool::new(false);
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

/// Stack for the second core, which renders artwork during sync
const RENDER_CORE_STACK_SIZE: usize = 96 * 1024;
static mut RENDER_CORE_STACK: CoreStack<RENDER_CORE_STACK_SIZE> = CoreStack::new();

/// Cover to render on the second core
struct RenderJob {
    index: usize,
    concert: Concert,
    jpeg: Vec<u8>,
    size: (u32, u32),
}

/// Rendered artwork in the stored image format
struct RenderResult {
    index: usize,
    image: Result<Vec<u8>, sync::SyncError>,
}

static RENDER_JOBS: Channel<CriticalSectionRawMutex, RenderJob, 1> = Channel::new();
static RENDER_RESULTS: Channel<CriticalSectionRawMutex, RenderResult, 4> = Channel::new();
/// Concert text font from the SD card, or `None` for the embedded font; sent before the first job
static RENDER_FONT: Signal<CriticalSectionRawMutex, Option<&'static [u8]>> = Signal::new();

/// Renders covers on the second core so downloads continue on the first
#[embassy_executor::task]
async fn render_task() {
    let font = RENDER_FONT.wait().await.and_then(|data| {
        let font = sawthat_render::text::parse_font(data);
        if font.is_none() {
            info!("SD card font is invalid, using the embedded font");
        }
        font
    });
    let renderer = font.map_or_else(Renderer::new, Renderer::with_font);
    loop {
        let job = RENDER_JOBS.receive().await;
        let image = sync::render_cover(&renderer, &job.jpeg, &job.concert, job.size);
        RENDER_RESULTS
            .send(RenderResult {
                index: job.index,
                image,
            })
            .await;
    }
}

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

/// Start acting on button presses
fn start_button_monitor() {
    BUTTON_STATE.store(BUTTON_POLLING, Ordering::Relaxed);
}

/// Whether a tap or hold is waiting to be handled
fn button_pending() -> bool {
    matches!(
        BUTTON_STATE.load(Ordering::Relaxed),
        BUTTON_NEXT | BUTTON_FLIP
    )
}

/// Button monitor task: classifies each press as a tap or hold while monitoring is active.
/// One tap is queued at a time; a hold replaces a queued tap.
///
/// Presses are detected from pin edges (interrupt driven), so a tap is not missed while the
/// main core is busy with a sync.
#[embassy_executor::task]
async fn button_monitor_task(key_input: &'static mut Input<'static>) {
    loop {
        key_input.wait_for_falling_edge().await;
        // A hold may replace a queued tap; a tap needs nothing queued
        if !matches!(
            BUTTON_STATE.load(Ordering::Relaxed),
            BUTTON_POLLING | BUTTON_NEXT
        ) {
            continue;
        }

        let held = with_timeout(
            Duration::from_millis(HOLD_THRESHOLD_MS as u64),
            key_input.wait_for_high(),
        )
        .await
        .is_err();
        let accepted = if held {
            BUTTON_STATE
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
                    matches!(state, BUTTON_POLLING | BUTTON_NEXT).then_some(BUTTON_FLIP)
                })
                .is_ok()
        } else {
            BUTTON_STATE
                .compare_exchange(
                    BUTTON_POLLING,
                    BUTTON_NEXT,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        };
        if accepted {
            flash_green(if held { 3 } else { 1 });
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

    // The second core renders artwork while this core handles the network during sync
    let sw_ints = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start_second_core(
        peripherals.CPU_CTRL,
        sw_ints.software_interrupt0,
        sw_ints.software_interrupt1,
        // Taken once at boot; `&mut STATIC` is rejected for `static mut` in edition 2024
        #[allow(clippy::deref_addrof)]
        unsafe {
            &mut *(&raw mut RENDER_CORE_STACK)
        },
        || {
            let executor = mk_static!(Executor, Executor::new());
            executor.run(|spawner| {
                spawner.spawn(render_task()).ok();
            });
        },
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

    // A KEY press on the setup screen only retries, so it is not read as a tap or hold
    let setup_pending = unsafe { core::ptr::replace(&raw mut SETUP_PENDING, false) };

    if button_wake && !setup_pending {
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

    // Persistent button monitor; acts only while monitoring is started
    spawner
        .spawn(button_monitor_task(mk_static!(Input<'static>, key_input)))
        .ok();

    // ==================== Normal Boot Sequence ====================
    // Now do the heavier initialization
    info!(
        "Boot! Wake reason: {:?}, reset reason: {:?}",
        wake_reason,
        esp_hal::system::reset_reason()
    );

    let mut delay = Delay;
    let cold_boot = setup_pending
        || !matches!(
            wake_reason,
            esp_hal::system::SleepSource::Timer
                | esp_hal::system::SleepSource::Ext0
                | esp_hal::system::SleepSource::Ext1
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

    let config = match sd_cache.as_mut() {
        None => Err(ConfigError::NoSdCard),
        Some(cache) => load_config(cache),
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

    // Framebuffer (192KB, PSRAM)
    let mut framebuffer = Framebuffer::new();

    // Show the setup screen for `$error`, expose the card to a connected computer (rebooting
    // afterwards to read the new config), then sleep until KEY, USB, or the `$retry` timer
    macro_rules! enter_setup {
        ($error:expr, $retry:expr) => {{
            let error: ConfigError = $error;
            info!("Setup needed: {:?}", error);
            let message = error.message();
            // Skip the refresh when the same setup screen is already shown
            let shown = message_hash(&message, orientation);
            if !(setup_pending && unsafe { SETUP_SHOWN } == shown) {
                framebuffer.clear(sawthat_frame_firmware::epd::Color::White);
                progress::draw_setup(&mut framebuffer, orientation, &message);
                match epd.display(framebuffer.as_slice(), &mut delay) {
                    Ok(()) => unsafe { SETUP_SHOWN = shown },
                    Err(e) => info!("Setup screen refresh failed: {:?}", e),
                }
            }
            epd.sleep(&mut delay).ok();

            if let Some(cache) = sd_cache.take()
                && usb_host_connected().await
            {
                start_button_monitor();
                let usb = Usb::new(peripherals.USB0, peripherals.GPIO20, peripherals.GPIO19);
                let exit = usb_drive::serve(cache.into_card(), usb, button_pending).await;
                info!("USB drive mode ended: {:?}", exit);
                usb_drive::restore_phy();
                if exit != usb_drive::Exit::NoHost {
                    delay.delay_ms(100);
                    esp_hal::system::software_reset();
                }
            }
            sleep_on_setup(&mut rtc, &mut i2c, &mut delay, $retry)
        }};
    }

    let config = match config {
        Ok(config) => config,
        Err(e) => enter_setup!(e, None),
    };
    info!("Refresh interval: {} minutes", config.refresh_minutes);

    // Charger plugged, unplugged, started or finished during sleep: redraw only the battery
    // body, then sleep until the timer wake that was already scheduled
    if matches!(wake_reason, esp_hal::system::SleepSource::Ext1) && resuming {
        info!("Charger changed");
        let power = read_power(&mut i2c).await;
        draw_battery_icon(&mut framebuffer, orientation, power);
        let rect = battery_body_rect(orientation);
        let region = framebuffer.extract_rect(&rect);
        if let Err(e) = epd.partial_update(&rect, &region, &mut delay) {
            info!("Battery refresh failed: {:?}", e);
        }
        epd.sleep(&mut delay).ok();

        let now = rtc.time_since_boot().as_micros() as u64;
        let remaining = unsafe { NEXT_WAKE_US }.saturating_sub(now) / 1_000_000;
        sleep_until_wake(&mut rtc, &mut i2c, &mut delay, remaining.max(1));
    }

    // Buffers for stored artwork and partial updates (PSRAM)
    let mut image_buf: Box<[u8; IMAGE_BUF_SIZE]> = Box::new([0u8; IMAGE_BUF_SIZE]);
    let mut half_buffer: Box<[u8; HALF_BUFFER_SIZE]> = Box::new([0u8; HALF_BUFFER_SIZE]);

    let rng = Rng::new();

    // TLS buffers for HTTPS
    let mut tls_read_buf: Box<[u8; TLS_READ_BUF_SIZE]> = Box::new([0u8; TLS_READ_BUF_SIZE]);
    let mut tls_write_buf: Box<[u8; TLS_WRITE_BUF_SIZE]> = Box::new([0u8; TLS_WRITE_BUF_SIZE]);
    // Second pair keeps a cover connection open alongside the Deezer API one
    let mut cover_tls_read_buf: Box<[u8; TLS_READ_BUF_SIZE]> = Box::new([0u8; TLS_READ_BUF_SIZE]);
    let mut cover_tls_write_buf: Box<[u8; TLS_WRITE_BUF_SIZE]> =
        Box::new([0u8; TLS_WRITE_BUF_SIZE]);

    // Network resources - created on first WiFi init
    let mut tcp_client: Option<TcpClient<'static, 2, TCP_RX_SIZE, TCP_TX_SIZE>> = None;
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
                    let (wifi_ctrl, ifaces) =
                        esp_radio::wifi::new(ctrl, wifi, WifiConfig::default()).unwrap();
                    StaDisconnected::update_handler(|event| {
                        WIFI_REASON.store(event.reason(), Ordering::Relaxed)
                    });

                    let (stk, runner) = embassy_net::new(
                        ifaces.sta,
                        embassy_net::Config::dhcpv4(Default::default()),
                        mk_static!(StackResources<4>, StackResources::<4>::new()),
                        rng.random() as u64,
                    );
                    spawner.spawn(net_task(runner)).ok();

                    let tcp_state = alloc_tcp_state();
                    tcp_client = Some(TcpClient::new(stk, tcp_state));
                    dns_socket = Some(DnsSocket::new(stk));
                    net_stack = Some(stk);
                    _esp_radio_ctrl = Some(ctrl);
                    wifi_controller = Some(wifi_ctrl);
                }

                let stack = net_stack.unwrap();
                let ctrl = wifi_controller.as_mut().unwrap();
                WIFI_REASON.store(0, Ordering::Relaxed);
                WIFI_LINKED.store(false, Ordering::Relaxed);
                WIFI_AUTH_FAILED.store(false, Ordering::Relaxed);
                wifi_connected = with_timeout(WIFI_TIMEOUT, async {
                    let joined = wifi_connect(ctrl, &config).await;
                    if joined {
                        wait_for_ip(stack).await;
                    }
                    joined
                })
                .await
                .unwrap_or(false);

                if wifi_connected {
                    info!("WiFi ready!");
                } else {
                    info!("WiFi connection failed");
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

    // Test builds: delete some artwork so the power-on sync has work to do
    if let Some(n) = option_env!("SAWTHAT_INVALIDATE").and_then(|n| n.parse::<usize>().ok())
        && cold_boot
        && let Some(cache) = sd_cache.as_mut()
    {
        let removed = items
            .iter()
            .take(n)
            .filter(|path| cache.remove_image(path, orientation).is_ok())
            .count();
        info!("Invalidated {} {:?} images", removed, orientation);
    }

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
        // Whether the render task has been sent its font
        let mut font_sent = false;

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
        // work), or a welcome screen with instructions when there is no artwork for the
        // current orientation. Render missing artwork while the panel refreshes; the first
        // rendered screen replaces the welcome screen's background, keeping the dialog. A tap
        // shows the next ready screen behind the dialog, and a hold stops the sync to rotate.
        // Then erase the dialog. Evaluates to whether a refresh was started or a press waits.
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

                if sd_cache.is_some() && !ensure_wifi!() {
                    enter_setup!(
                        ConfigError::WifiFailed(
                            config.wifi_ssid.clone(),
                            WifiCause::classify(
                                WIFI_REASON.load(Ordering::Relaxed),
                                WIFI_LINKED.load(Ordering::Relaxed),
                                WIFI_AUTH_FAILED.load(Ordering::Relaxed)
                            )
                        ),
                        Some(config.refresh_secs())
                    );
                }
                if let Some(cache) = sd_cache.as_mut()
                    && wifi_connected
                {
                    start_blink();
                    let mut net = sync::Net {
                        tcp: tcp_client.as_ref().unwrap(),
                        dns: dns_socket.as_ref().unwrap(),
                        tls_read: &mut *tls_read_buf,
                        tls_write: &mut *tls_write_buf,
                        cover_tls_read: &mut *cover_tls_read_buf,
                        cover_tls_write: &mut *cover_tls_write_buf,
                    };

                    let concerts = match sync::fetch_bands(&mut net, &config.user_id)
                        .await
                        .ok()
                        .and_then(|json| Some((sync::parse_concerts(&json, &config.user_id)?, json)))
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
                                .and_then(|json| sync::parse_concerts(&json, &config.user_id))
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
                        let rssi = wifi_controller.as_ref().and_then(|c| c.rssi().ok());
                        let mut progress = (total > 0)
                            .then(|| progress::Progress::new(orientation, total, rssi));

                        // Screens shown during the sync come from the cached list when it was
                        // shown, otherwise from `pool`: artwork on the card, then each new
                        // render, in order
                        let mut from_items = false;
                        let mut pool: Vec<usize> =
                            (0..concerts.len()).filter(|&i| ready_flags[i]).collect();
                        let mut pool_shown = 0usize;
                        let mut completed = 0usize;
                        let mut in_flight = 0usize;

                        // Full refresh of the next screen behind the dialog (at the current
                        // progress), evaluating to whether it started. Re-arms the button
                        // monitor unless a hold is waiting to be handled.
                        macro_rules! show_next {
                            () => {{
                                let paths: Option<Vec<&str>> = if from_items {
                                    Some(
                                        (0..per_screen)
                                            .map(|s| items[(index + s) % items.len()].as_str())
                                            .collect(),
                                    )
                                } else {
                                    pool.get(pool_shown..pool_shown + per_screen).map(|next| {
                                        next.iter().map(|&i| concerts[i].path.as_str()).collect()
                                    })
                                };
                                let started = paths.is_some_and(|paths| {
                                    compose_screen(
                                        cache,
                                        &mut framebuffer,
                                        &mut *image_buf,
                                        &paths,
                                        orientation,
                                        $battery,
                                    )
                                }) && {
                                    if let Some(p) = progress.as_mut() {
                                        p.draw(&mut framebuffer, completed);
                                    }
                                    epd.display_start(framebuffer.as_slice(), &mut delay).is_ok()
                                };
                                if from_items && !started {
                                    // Skip a cached screen that could not be shown, so a
                                    // waiting tap tries the next one
                                    advance_full!();
                                }
                                if started {
                                    if from_items {
                                        advance_full!();
                                    } else {
                                        pool_shown += per_screen;
                                    }
                                    if let Some(p) = progress.as_mut() {
                                        p.confirm();
                                    }
                                    shown = true;
                                    if BUTTON_STATE.load(Ordering::Relaxed) != BUTTON_FLIP {
                                        start_button_monitor();
                                    }
                                }
                                started
                            }};
                        }

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
                                from_items = true;
                                advance_full!();
                                start_button_monitor();
                                if let Some(p) = progress.as_mut() {
                                    p.confirm();
                                }
                            }
                        } else if !show_next!() && progress.is_some() {
                            // Nothing to show yet: welcome screen until the first render
                            let p = progress.insert(progress::Progress::welcome(
                                orientation,
                                total,
                                rssi,
                                config.refresh_minutes,
                            ));
                            framebuffer.clear(sawthat_frame_firmware::epd::Color::White);
                            draw_battery_icon(&mut framebuffer, orientation, $battery);
                            p.draw(&mut framebuffer, 0);
                            if epd
                                .display_start(framebuffer.as_slice(), &mut delay)
                                .is_ok()
                            {
                                p.confirm();
                                start_button_monitor();
                            }
                        }

                        if total > 0 && !font_sent {
                            RENDER_FONT.signal(load_font(cache).map(|data| &*Vec::leak(data)));
                            font_sent = true;
                        }

                        let mut albums = sync::AlbumCache::new();

                        // After progress: show the first screen once one is available, the
                        // next screen for a waiting tap, or otherwise the new bar slice
                        macro_rules! update_screen {
                            () => {{
                                let state = BUTTON_STATE.load(Ordering::Relaxed);
                                if !shown && state != BUTTON_FLIP && pool.len() >= per_screen {
                                    while epd.is_busy() {
                                        Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS))
                                            .await;
                                    }
                                    show_next!();
                                } else if state == BUTTON_NEXT && !epd.is_busy() && show_next!() {
                                    info!("Tap during sync: showing the next screen");
                                } else if let Some(p) = progress.as_mut()
                                    && !epd.is_busy()
                                    && let Some((rect, region)) =
                                        p.update(&mut framebuffer, completed)
                                    && epd.partial_update_start(&rect, &region, &mut delay).is_ok()
                                {
                                    p.confirm();
                                }
                            }};
                        }

                        // Store a finished render, then update the screen
                        macro_rules! finish_render {
                            ($result:expr) => {{
                                let RenderResult { index: i, image } = $result;
                                let concert = &concerts[i];
                                completed += 1;
                                in_flight -= 1;
                                match image {
                                    Ok(image) => {
                                        match cache.write_image(&concert.path, orientation, &image) {
                                            Ok(()) => {
                                                ready_flags[i] = true;
                                                pool.push(i);
                                            }
                                            Err(e) => {
                                                info!("Failed to store {}: {:?}", concert.path, e)
                                            }
                                        }
                                    }
                                    Err(e) => info!("Skipping {}: {:?}", concert.path, e),
                                }
                                update_screen!();
                            }};
                        }

                        // Download covers on this core while the second core renders; a
                        // hold stops the sync so the screen can rotate
                        sync::fetch_covers(
                            &mut net,
                            &mut albums,
                            &concerts,
                            &missing,
                            layout_size(orientation),
                            async |i, cover| {
                                match cover {
                                    Ok(jpeg) => {
                                        RENDER_JOBS
                                            .send(RenderJob {
                                                index: i,
                                                concert: concerts[i].clone(),
                                                jpeg,
                                                size: layout_size(orientation),
                                            })
                                            .await;
                                        in_flight += 1;
                                    }
                                    Err(e) => {
                                        info!("Skipping {}: {:?}", concerts[i].path, e);
                                        completed += 1;
                                        update_screen!();
                                    }
                                }
                                while let Ok(result) = RENDER_RESULTS.try_receive() {
                                    finish_render!(result);
                                }
                                BUTTON_STATE.load(Ordering::Relaxed) != BUTTON_FLIP
                            },
                        )
                        .await;
                        while in_flight > 0 {
                            finish_render!(RENDER_RESULTS.receive().await);
                        }
                        if BUTTON_STATE.load(Ordering::Relaxed) == BUTTON_FLIP {
                            info!("Sync stopped to rotate");
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
                        // The cleanup may have removed artwork the other orientation's list
                        // still names (a concert or the user changed); keep only current items
                        let other = orientation.toggle();
                        if let Some(mut list) = cache.load_widget_data(other) {
                            let len = list.len();
                            list.retain(|item| valid.contains(item));
                            if list.len() != len
                                && let Err(e) = cache.store_widget_data(other, &list)
                            {
                                info!("Failed to prune {:?} item list: {:?}", other, e);
                            }
                        }

                        // Show the full bar, hold it briefly, then erase the dialog. Skipped
                        // when the screen is replaced next anyway: a welcome screen with no
                        // artwork behind it, or a button press waiting to be handled.
                        if let Some(p) = progress.as_mut()
                            && p.is_shown()
                            && !button_pending()
                        {
                            while epd.is_busy() {
                                Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
                            }
                            if let Some((rect, region)) = p.update(&mut framebuffer, total)
                                && epd.partial_update_start(&rect, &region, &mut delay).is_ok()
                            {
                                p.confirm();
                                while epd.is_busy() {
                                    Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS))
                                        .await;
                                }
                            }
                            Timer::after(Duration::from_millis(DIALOG_HOLD_MS)).await;
                            if shown && let Some((rect, region)) = p.restore(&mut framebuffer) {
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
                // A waiting press is handled after the refresh, like any other screen
                shown || button_pending()
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

            let power = read_power(&mut i2c).await;

            let display_started = if need_sync {
                need_sync = false;
                info!("Syncing {:?} artwork", orientation);
                let shown = sync_cycle!(power);
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
                draw_battery_icon(&mut framebuffer, orientation, power);

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
                        power,
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

    unsafe {
        NEXT_WAKE_US = rtc.time_since_boot().as_micros() as u64 + config.refresh_secs() * 1_000_000;
    }
    sleep_until_wake(&mut rtc, &mut i2c, &mut delay, config.refresh_secs());
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
        match i2c.write_read(AXP2101_ADDR, &[reg], &mut buf) {
            Ok(()) => return Some(buf[0]),
            Err(e) => info!("PMIC read {:#04x} attempt {} failed: {:?}", reg, attempt, e),
        }
    }
    None
}

/// Battery level and power symbol, as shown by the battery icon
#[derive(Clone, Copy)]
struct Power {
    percent: u8,
    symbol: battery::PowerSymbol,
}

/// Read battery level (from cell voltage, falling back to the fuel gauge) and power source:
/// a check when plugged in and full, a USB icon when connected to a computer, and a bolt
/// when charging from anything else
async fn read_power(i2c: &mut I2c<'_, esp_hal::Blocking>) -> Power {
    let (mv, vbus, charging, full) = log_charger_status(i2c);
    let percent = match mv {
        Some(mv) => battery_percent_from_mv(mv),
        None => pmic_read(i2c, BAT_PERCENT_REG).unwrap_or(50),
    };
    let symbol = if vbus && full {
        battery::PowerSymbol::Full
    } else if vbus && usb_host_connected().await {
        battery::PowerSymbol::Usb
    } else if charging {
        battery::PowerSymbol::Bolt
    } else {
        battery::PowerSymbol::None
    };
    info!("Power: {}%, {:?}", percent, symbol);
    Power { percent, symbol }
}

/// Whether a computer is on the USB port: a host sends a start-of-frame packet every
/// millisecond, which advances the USB serial frame counter; a charger does not. Waits up to
/// a second for the host to enumerate after boot.
async fn usb_host_connected() -> bool {
    // USB_SERIAL_JTAG FRAM_NUM register (SOF frame index in bits 10:0)
    const FRAM_NUM: *const u32 = 0x6003_8024 as *const u32;
    let frame = || unsafe { FRAM_NUM.read_volatile() } & 0x7FF;
    let first = frame();
    for _ in 0..100 {
        Timer::after(Duration::from_millis(10)).await;
        if frame() != first {
            return true;
        }
    }
    false
}

/// Log AXP2101 charger state, returning battery voltage in mV, whether USB power is present,
/// whether the battery is charging, and whether charging is done
fn log_charger_status(i2c: &mut I2c<'_, esp_hal::Blocking>) -> (Option<u16>, bool, bool, bool) {
    const STATUS1: u8 = 0x00;
    const STATUS2: u8 = 0x01;
    const ADC_ENABLE: u8 = 0x30;
    const VBAT_H: u8 = 0x34;
    const VBAT_L: u8 = 0x35;

    let mut vbus = false;
    if let Some(s1) = pmic_read(i2c, STATUS1) {
        vbus = s1 & 0x20 != 0;
        info!(
            "PMIC status1={:#04x}: vbus_good={} battery_present={}",
            s1,
            s1 & 0x20 != 0,
            s1 & 0x08 != 0
        );
    }

    let (mut charging, mut full) = (false, false);
    if let Some(s2) = pmic_read(i2c, STATUS2) {
        charging = (s2 >> 5) & 0x03 == 1;
        full = s2 & 0x07 == 4;
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
        let _ = i2c.write(AXP2101_ADDR, &[ADC_ENABLE, adc | 0x01]);
    }
    let mv = pmic_read(i2c, VBAT_H)
        .zip(pmic_read(i2c, VBAT_L))
        .map(|(h, l)| ((h as u16 & 0x3F) << 8) | l as u16);
    info!("PMIC vbat={:?}mV", mv);
    // A zero reading means the ADC has not sampled yet
    (mv.filter(|&mv| mv > 0), vbus, charging, full)
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

/// Read `CONFIG.TXT` from the SD root
fn load_config<S: SpiDevice, D: DelayNs>(cache: &mut SdCache<S, D>) -> Result<Config, ConfigError> {
    let read =
        |cache: &mut SdCache<S, D>| cache.read_root_file(config::CONFIG_FILE, config::MAX_CONFIG);
    let data = match read(cache) {
        // Write a template to fill in, so the setup screen names the first missing key
        Err(CacheError::NotFound) => {
            match cache.write_root_file(config::CONFIG_FILE, config::TEMPLATE.as_bytes()) {
                Ok(()) => {
                    info!("Wrote a {} template", config::CONFIG_FILE);
                    read(cache)
                }
                Err(_) => Err(CacheError::NotFound),
            }
        }
        result => result,
    };
    let data = data.map_err(|e| match e {
        CacheError::NotFound => ConfigError::Missing,
        CacheError::Filesystem => ConfigError::BadFormat,
        _ => ConfigError::Unreadable,
    })?;
    config::parse(&data)
}

/// Read the first of `FONT_FILES` on the SD root, if any
fn load_font<S: SpiDevice, D: DelayNs>(cache: &mut SdCache<S, D>) -> Option<Vec<u8>> {
    FONT_FILES
        .iter()
        .find_map(|&name| match cache.read_root_file(name, MAX_FONT) {
            Ok(data) => {
                info!("Using font {} ({} bytes)", name, data.len());
                Some(data)
            }
            Err(CacheError::NotFound) => None,
            Err(e) => {
                info!("Failed to read font {}: {:?}", name, e);
                None
            }
        })
}

/// KEY button (GPIO4, active low) as a deep sleep wake source
fn key_wakeup() -> Ext0WakeupSource<esp_hal::peripherals::GPIO4<'static>> {
    // Reclaim the pin for the RTC domain; the pull-up keeps it from floating
    let key_pin = unsafe { esp_hal::peripherals::GPIO4::steal() };
    key_pin.rtcio_pullup(true);
    key_pin.rtcio_pulldown(false);
    Ext0WakeupSource::new(key_pin, WakeupLevel::Low)
}

/// Deep sleep on the setup screen until KEY, a USB plug, or `retry_secs` (if any), then boot
/// like a power-on
fn sleep_on_setup(
    rtc: &mut Rtc,
    i2c: &mut I2c<'_, esp_hal::Blocking>,
    delay: &mut Delay,
    retry_secs: Option<u64>,
) -> ! {
    unsafe {
        let state = &raw mut SLEEP_STATE;
        (*state).invalidate();
        SETUP_PENDING = true;
    }
    arm_charger_irq(i2c);
    let ext0 = key_wakeup();
    let mut irq_pin = charger_irq_pin();
    let mut irq_pins: [&mut dyn esp_hal::gpio::RtcPin; 1] = [&mut irq_pin];
    let ext1 = Ext1WakeupSource::new(&mut irq_pins, WakeupLevel::Low);
    info!(
        "Entering deep sleep until KEY, USB, or retry in {:?} s...",
        retry_secs
    );
    // Let serial output flush
    delay.delay_ms(100);
    match retry_secs {
        Some(secs) => {
            let timer = TimerWakeupSource::new(CoreDuration::from_secs(secs));
            rtc.sleep_deep(&[&ext0, &ext1, &timer])
        }
        None => rtc.sleep_deep(&[&ext0, &ext1]),
    }
}

/// Identifies a setup screen by its message and orientation
fn message_hash(message: &str, orientation: Orientation) -> u32 {
    message
        .bytes()
        .chain([orientation as u8])
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(b as u32))
}

/// AXP2101 IRQ line (GPIO21, active low), reclaimed for the RTC domain with a pull-up
fn charger_irq_pin() -> esp_hal::peripherals::GPIO21<'static> {
    let irq_pin = unsafe { esp_hal::peripherals::GPIO21::steal() };
    irq_pin.rtcio_pullup(true);
    irq_pin.rtcio_pulldown(false);
    irq_pin
}

/// Arm the charger wake and deep sleep for `seconds`, or until KEY or a charger change.
///
/// Wake sources: the timer, the KEY button (GPIO4, active low), and the AXP2101 IRQ line
/// (GPIO21, active low) for USB plug/unplug and charge start/done.
fn sleep_until_wake(
    rtc: &mut Rtc,
    i2c: &mut I2c<'_, esp_hal::Blocking>,
    delay: &mut Delay,
    seconds: u64,
) -> ! {
    arm_charger_irq(i2c);

    let timer = TimerWakeupSource::new(CoreDuration::from_secs(seconds));

    let ext0 = key_wakeup();
    let mut irq_pin = charger_irq_pin();
    let mut irq_pins: [&mut dyn esp_hal::gpio::RtcPin; 1] = [&mut irq_pin];
    let ext1 = Ext1WakeupSource::new(&mut irq_pins, WakeupLevel::Low);

    info!(
        "Entering deep sleep for {} seconds (KEY or a charger change wakes early)...",
        seconds
    );
    // Let serial output flush
    delay.delay_ms(100);
    rtc.sleep_deep(&[&timer, &ext0, &ext1])
}

/// Enable only the AXP2101 charger IRQs (VBUS insert/remove, charge start/done) and clear
/// pending ones, so the IRQ line is high until the next charger change
fn arm_charger_irq(i2c: &mut I2c<'_, esp_hal::Blocking>) {
    const INTEN: [(u8, u8); 3] = [(0x40, 0x00), (0x41, 0xC0), (0x42, 0x18)];
    const INTSTS: [u8; 3] = [0x48, 0x49, 0x4A];
    for (reg, value) in INTEN {
        let _ = i2c.write(AXP2101_ADDR, &[reg, value]);
    }
    for reg in INTSTS {
        let _ = i2c.write(AXP2101_ADDR, &[reg, 0xFF]);
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

/// Connect to WiFi network
/// Connect to the configured network, retrying until it joins (true) or fails the same
/// definite way twice (false)
async fn wifi_connect(controller: &mut WifiController<'static>, config: &Config) -> bool {
    start_fast_blink();
    info!("Device capabilities: {:?}", controller.capabilities());

    if !matches!(controller.is_started(), Ok(true)) {
        let client = ClientConfig::default()
            .with_ssid(config.wifi_ssid.clone())
            .with_password(config.wifi_pass.clone());
        // An empty password means an open network
        let client_config = ModeConfig::Client(if config.wifi_pass.is_empty() {
            client.with_auth_method(AuthMethod::None)
        } else {
            client
        });
        controller.set_config(&client_config).unwrap();
        info!("Starting WiFi...");
        controller.start_async().await.unwrap();
        info!("WiFi started!");
    }

    info!("Connecting to {}...", config.wifi_ssid);
    let mut last: Option<WifiCause> = None;
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                match controller.rssi() {
                    Ok(rssi) => info!("WiFi connected (RSSI {} dBm)", rssi),
                    Err(_) => info!("WiFi connected!"),
                }
                WIFI_LINKED.store(true, Ordering::Relaxed);
                stop_blink();
                return true;
            }
            Err(e) => {
                let reason = WIFI_REASON.load(Ordering::Relaxed);
                let cause =
                    WifiCause::classify(reason, false, WIFI_AUTH_FAILED.load(Ordering::Relaxed));
                if cause == WifiCause::Auth {
                    WIFI_AUTH_FAILED.store(true, Ordering::Relaxed);
                }
                info!("Failed to connect: {e:?} (reason {})", reason);
                // A missing network or rejected password twice in a row will not fix itself
                if matches!(cause, WifiCause::NotFound | WifiCause::Auth) && last == Some(cause) {
                    info!("Giving up: {:?}", cause);
                    return false;
                }
                last = Some(cause);
                Timer::after(Duration::from_secs(WIFI_RETRY_SECS)).await;
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
    power: Power,
) -> bool {
    framebuffer.clear(sawthat_frame_firmware::epd::Color::White);
    let drawn = paths
        .iter()
        .enumerate()
        .all(|(slot, path)| draw_stored(cache, framebuffer, buf, path, orientation, slot as u8));
    if drawn {
        draw_battery_icon(framebuffer, orientation, power);
    }
    drawn
}

/// Battery icon position: centered at the top in horizontal, top right in vertical
fn battery_origin(orientation: Orientation) -> (u16, u16) {
    let vertical = orientation == Orientation::Vertical;
    let (bat_w, _) = battery::battery_dimensions(vertical);
    let x = if vertical {
        WIDTH as u16 - bat_w - 8
    } else {
        (WIDTH as u16 - bat_w) / 2
    };
    (x, 8)
}

/// Draw the battery icon with its power symbol
fn draw_battery_icon(framebuffer: &mut Framebuffer, orientation: Orientation, power: Power) {
    let (x, y) = battery_origin(orientation);
    battery::draw_battery(
        framebuffer.as_mut_slice(),
        x,
        y,
        power.percent,
        orientation == Orientation::Vertical,
        power.symbol,
    );
}

/// Panel-aligned rect inside the battery body, covering the fill and bolt but no background
fn battery_body_rect(orientation: Orientation) -> Rect {
    let (x, y) = battery_origin(orientation);
    match orientation {
        // Body is 42x24 below the tip; x is cut to the 8-pixel window alignment
        Orientation::Horizontal => Rect::new(x, y, 40, 24),
        // Body is 24x42 below the 6px tip
        Orientation::Vertical => Rect::new(x, y + 6, 24, 42),
    }
}

/// TCP socket buffers for both sync connections, allocated in PSRAM and kept for the
/// program's lifetime so they don't take internal RAM from the main stack
fn alloc_tcp_state() -> &'static TcpClientState<2, TCP_RX_SIZE, TCP_TX_SIZE> {
    let layout = core::alloc::Layout::new::<TcpClientState<2, TCP_RX_SIZE, TCP_TX_SIZE>>();
    // SAFETY: zeroed memory is identical to `TcpClientState::new()` in embassy-net 0.8 (all
    // pool slots unused, buffers uninitialized); allocating it directly avoids building the
    // struct on the stack. The allocation is never freed.
    unsafe {
        let ptr =
            alloc::alloc::alloc_zeroed(layout) as *mut TcpClientState<2, TCP_RX_SIZE, TCP_TX_SIZE>;
        assert!(!ptr.is_null(), "TCP buffer allocation failed");
        &*ptr
    }
}
