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
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
use esp_hal::system::Stack as CoreStack;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, InputPin, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    ram,
    rng::Rng,
    rtc_cntl::{Rtc, WakeupSource},
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
    usb::otg::Usb,
};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiConfig, ConnectionError, DisconnectReason, Interface,
    WifiController, sta::StationConfig,
};
use esp_rtos::embassy::Executor;
use sawthat_frame_firmware::TimestampLogger;
use sawthat_frame_firmware::battery;
use sawthat_frame_firmware::cache::ConcertList;
use sawthat_frame_firmware::cache::{CacheError, SdCache};
use sawthat_frame_firmware::config::{self, Config, ConfigError, WifiCause};
use sawthat_frame_firmware::display;
use sawthat_frame_firmware::epd::{Epd7in3e, Rect, RefreshMode, WIDTH};
use sawthat_frame_firmware::framebuffer::Framebuffer;
use sawthat_frame_firmware::installer::Installer;
use sawthat_frame_firmware::orientation::Orientation;
use sawthat_frame_firmware::progress;
use sawthat_frame_firmware::sync::{self, TLS_READ_BUF_SIZE, TLS_WRITE_BUF_SIZE};
use sawthat_frame_firmware::usb_drive;
use sawthat_frame_firmware::{crash, sleep};
use sawthat_render::Renderer;
use sawthat_render::concerts::Concert;

esp_bootloader_esp_idf::esp_app_desc!();

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
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
const FONT_FILES: [&str; 2] = sawthat_frame_firmware::installer::FONT_NAMES;
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
/// AXP2101 rail enables: DC1-DC5 in bits 0-4, ALDO1-ALDO4 in bits 0-3, DLDO2 in bit 0
const DC_ONOFF_CTRL: u8 = 0x80;
const LDO_ONOFF_CTRL0: u8 = 0x90;
const LDO_ONOFF_CTRL1: u8 = 0x91;
/// ALDO3 feeds the audio codecs, ALDO4 the panel. DC1 is VCC3V3 (ESP32, SD card, LEDs) and
/// the other rails are unconnected.
const ALDO3: u8 = 1 << 2;
const ALDO4: u8 = 1 << 3;
/// Battery log on the SD root, one CSV row per power reading
const BATTERY_LOG: &str = "BATTERY.CSV";
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
    /// Current index into the concert list (next concert to show)
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
        items: &ConcertList,
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

    fn matches_data(&self, items: &ConcertList) -> bool {
        items.len() == self.total_items && self.data_hash == hash_data(items)
    }
}

/// Button monitor state
static BUTTON_STATE: AtomicU8 = AtomicU8::new(BUTTON_CANCELLED);

/// Set by a BOOT press while awake; enters or leaves USB drive mode when a computer is connected
static BOOT_PRESSED: AtomicBool = AtomicBool::new(false);

/// Last station disconnect reason, and whether the station joined the network
static WIFI_REASON: critical_section::Mutex<Cell<Option<DisconnectReason>>> =
    critical_section::Mutex::new(Cell::new(None));
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
    /// Alternate red and green at interval (ms)
    Alternate(u16),
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

/// Combined LED task: red blinking, red and green alternating, and green flash requests
/// Uses signal-based waking to avoid continuous polling
#[embassy_executor::task]
async fn led_task(led_red: &'static mut Output<'static>, led_green: &'static mut Output<'static>) {
    use embassy_futures::select::{Either, select};

    /// What the LEDs do between commands
    #[derive(Clone, Copy)]
    enum Mode {
        /// Red solid, green off
        Solid,
        /// Red blinking at the interval (ms)
        RedBlink(u64),
        /// Red and green alternating at the interval (ms)
        Alternate(u64),
    }

    // LEDs are active low
    let mut mode = Mode::Solid;
    loop {
        let cmd = match mode {
            Mode::Solid => Some(LED_SIGNAL.wait().await),
            Mode::RedBlink(ms) | Mode::Alternate(ms) => {
                match select(LED_SIGNAL.wait(), Timer::after(Duration::from_millis(ms))).await {
                    Either::First(cmd) => Some(cmd),
                    Either::Second(()) => None,
                }
            }
        };
        match cmd {
            None => {
                led_red.toggle();
                if matches!(mode, Mode::Alternate(_)) {
                    led_green.toggle();
                }
            }
            Some(LedCommand::GreenFlash(count)) => {
                for _ in 0..count {
                    led_green.set_low();
                    Timer::after(Duration::from_millis(100)).await;
                    led_green.set_high();
                    Timer::after(Duration::from_millis(100)).await;
                }
                // Keep alternating out of phase with red
                if matches!(mode, Mode::Alternate(_)) && led_red.is_set_high() {
                    led_green.set_low();
                }
            }
            Some(LedCommand::RedBlink(ms)) => mode = Mode::RedBlink(ms as u64),
            Some(LedCommand::RedSolid) => {
                mode = Mode::Solid;
                led_red.set_low();
            }
            Some(LedCommand::Alternate(ms)) => {
                mode = Mode::Alternate(ms as u64);
                led_red.set_low();
                led_green.set_high();
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

/// Alternate red and green LEDs (USB drive mode)
fn start_alternate() {
    LED_SIGNAL.signal(LedCommand::Alternate(250));
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

/// Records BOOT presses from pin edges in `BOOT_PRESSED`
#[embassy_executor::task]
async fn boot_monitor_task(boot: &'static mut Input<'static>) {
    loop {
        boot.wait_for_falling_edge().await;
        BOOT_PRESSED.store(true, Ordering::Relaxed);
        flash_green(1);
        boot.wait_for_high().await;
        Timer::after(Duration::from_millis(BUTTON_POLL_MS)).await;
    }
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
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
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
    // KEY, BOOT, and the charger IRQ share a wake path, so the pins tell which one fired
    let button_wake = caused_wakeup(sleep::key_pin());
    let boot_wake = caused_wakeup(sleep::boot_pin());
    let charger_wake = caused_wakeup(sleep::charger_irq_pin());
    let wake_label = if wake_reason.contains(WakeupSource::Timer) {
        "timer"
    } else if button_wake {
        "button"
    } else if boot_wake {
        "boot"
    } else if charger_wake {
        "charger"
    } else {
        "reset"
    };

    // PSRAM is registered first so general allocations land there, leaving internal RAM
    // for allocations that request it explicitly (WiFi buffers, RTOS task stacks)
    let psram = esp_hal::psram::Psram::new(
        peripherals.PSRAM,
        esp_hal::psram::PsramConfig {
            mode: esp_hal::psram::PsramMode::OctalSpi,
            ..Default::default()
        },
    );
    info!("PSRAM: {} MB", psram.raw_parts().1 / 1024 / 1024);
    esp_alloc::psram_allocator!(&psram);
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // RTOS provides the embassy time driver, so it must start before any Timer is awaited
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // The second core renders artwork while this core handles the network during sync
    esp_rtos::start_second_core(
        peripherals.CPU_CTRL,
        peripherals.FROM_CPU_INTR1,
        // Taken once at boot; `&mut STATIC` is rejected for `static mut` in edition 2024
        #[allow(clippy::deref_addrof)]
        unsafe {
            &mut *(&raw mut RENDER_CORE_STACK)
        },
        || {
            let executor = mk_static!(Executor, Executor::new());
            executor.run(|spawner| {
                spawner.spawn(render_task().unwrap());
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
    spawner.spawn(led_task(led_red, led_green).unwrap());

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
    spawner.spawn(button_monitor_task(mk_static!(Input<'static>, key_input)).unwrap());
    let boot_input = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );
    spawner.spawn(boot_monitor_task(mk_static!(Input<'static>, boot_input)).unwrap());

    // ==================== Normal Boot Sequence ====================
    // Now do the heavier initialization
    info!(
        "Firmware v{} ({})",
        crash::FIRMWARE_VERSION,
        crash::GIT_REVISION
    );
    info!(
        "Boot! Wake reason: {:?}, reset reason: {:?}",
        wake_reason,
        esp_hal::system::reset_reason()
    );

    let mut delay = Delay;
    let cold_boot = setup_pending || wake_reason.is_empty();

    // ==================== SD Card Initialization ====================
    // SD card SPI pins: CS=GPIO38, CLK=GPIO39, MISO=GPIO40, MOSI=GPIO41

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

    let mut installer = Installer::new(peripherals.FLASH);
    let mut sd_cache = match SdCache::new(sd_spi_device, delay.clone()) {
        Ok(mut cache) => {
            if let Err(e) = cache.init() {
                info!("SD cache init error: {:?}", e);
            }
            // Before identifying the font, so a new one renders the artwork again
            apply_installer_font(&mut cache, &mut installer);
            cache.detect_font(&FONT_FILES);
            Some(cache)
        }
        Err(e) => {
            info!("SD card init failed: {:?} (artwork storage unavailable)", e);
            None
        }
    };

    let config = match sd_cache.as_mut() {
        None => Err(ConfigError::NoSdCard),
        Some(cache) => {
            apply_installer_settings(cache, &mut installer);
            load_config(cache)
        }
    };

    // Orientation: a hold during boot toggled it; otherwise CONFIG.TXT sets it, taking over the
    // orientation file of older firmware when it has no entry
    if BUTTON_STATE.load(Ordering::Relaxed) == BUTTON_FLIP {
        if let Some(cache) = sd_cache.as_mut() {
            save_orientation(cache, orientation);
        }
        // Reset button state after handling so display loop starts fresh
        BUTTON_STATE.store(BUTTON_CANCELLED, Ordering::Relaxed);
    } else if BUTTON_STATE.load(Ordering::Relaxed) == BUTTON_NEXT {
        // Button tap detected during boot - reset state, display loop will show next item
        BUTTON_STATE.store(BUTTON_CANCELLED, Ordering::Relaxed);
    } else if let Ok(config) = &config {
        if let Some(configured) = config.orientation {
            orientation = configured;
        } else if let Some(cache) = sd_cache.as_mut()
            && let Some(legacy) = cache.load_legacy_orientation()
        {
            orientation = legacy;
            save_orientation(cache, orientation);
        }
        info!("Orientation: {:?}", orientation);
    }

    // ==================== Power Management (AXP2101) ====================
    // SawThat Frame uses AXP2101 PMIC to control display power
    // I2C: SDA=GPIO47, SCL=GPIO48, Address=0x34

    let mut i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .expect("I2C init failed")
    .with_sda(peripherals.GPIO47)
    .with_scl(peripherals.GPIO48);

    const LDO_VOL2_CTRL: u8 = 0x94; // ALDO3 voltage
    const LDO_VOL3_CTRL: u8 = 0x95; // ALDO4 voltage

    // ALDO3/ALDO4 at 3.3V, (3300-500)/100 = 0x1C, and only those LDOs on. The panel rail is
    // cut during deep sleep, so this also powers it back up.
    let mut enable_rails = || -> Result<(), esp_hal::i2c::master::Error> {
        i2c.write(AXP2101_ADDR, &[LDO_VOL2_CTRL, 0x1C])?;
        i2c.write(AXP2101_ADDR, &[LDO_VOL3_CTRL, 0x1C])?;
        i2c.write(AXP2101_ADDR, &[LDO_ONOFF_CTRL0, ALDO3 | ALDO4])?;
        Ok(())
    };
    match enable_rails()
        .or_else(|_| enable_rails())
        .or_else(|_| enable_rails())
    {
        Ok(()) => info!("PMIC configured - ALDO3/ALDO4 enabled at 3.3V"),
        Err(e) => info!("PMIC rail config failed, panel may be unpowered: {:?}", e),
    }

    // Unconnected rails off: DC2-DC5 and DLDO2
    pmic_update(&mut i2c, DC_ONOFF_CTRL, 0x1E, 0);
    pmic_update(&mut i2c, LDO_ONOFF_CTRL1, 0x01, 0);

    // Charger: 2 A input limit, 50 mA precharge, 500 mA constant current, 25 mA termination
    const CHARGER: [(u8, u8, u8); 4] = [
        (0x16, 0x07, 5),
        (0x61, 0x0F, 2),
        (0x62, 0x1F, 0x0B),
        (0x63, 0x0F, 1),
    ];
    for (reg, mask, value) in CHARGER {
        if !pmic_update(&mut i2c, reg, mask, value) {
            info!("PMIC charger register {:#04x} not set", reg);
        }
    }

    // Small delay for power rails to stabilize
    delay.delay_ms(100);

    // ==================== E-Paper Display Setup ====================
    // PhotoPainter GPIO pins for 7.3" e-paper display (SPI3)
    // DC=GPIO8, CS=GPIO9, SCK=GPIO10, MOSI=GPIO11, RST=GPIO12, BUSY=GPIO13

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
    info!("E-paper display ready (fast mode)");

    // ==================== WiFi Setup (Deferred) ====================
    // WiFi is only initialized when a sync is needed, and the controller is dropped after it to
    // power the radio down
    let wifi_peripheral = peripherals.WIFI;
    let mut wifi_controller: Option<WifiController<'static>> = None;
    let mut wifi_connected = false;

    // ==================== RTC for Deep Sleep ====================
    let mut rtc = Rtc::new(peripherals.RTC_TIMER);

    // Framebuffer (192KB, PSRAM)
    let mut framebuffer = Framebuffer::new();

    // USB OTG peripheral and pins (D+, D-), taken when drive mode starts
    let mut usb_parts = Some((peripherals.USB_FS, peripherals.GPIO20, peripherals.GPIO19));

    // Expose the SD card to a connected computer until it is ejected, unplugged, or KEY or
    // BOOT is pressed, then reboot to read the new config. Continues only if no computer
    // enumerated the drive.
    macro_rules! drive_mode {
        ($tick:expr) => {{
            if usb_parts.is_some()
                && let Some(cache) = sd_cache.take()
                && let Some((usb_fs, dp, dm)) = usb_parts.take()
            {
                start_button_monitor();
                BOOT_PRESSED.store(false, Ordering::Relaxed);
                start_alternate();
                let usb = Usb::new_fs(usb_fs, dp, dm);
                let exit = usb_drive::serve(
                    cache.into_card(),
                    usb,
                    || button_pending() || BOOT_PRESSED.load(Ordering::Relaxed),
                    $tick,
                )
                .await;
                info!("USB drive mode ended: {:?}", exit);
                usb_drive::restore_phy();
                if exit != usb_drive::Exit::NoHost {
                    delay.delay_ms(100);
                    esp_hal::system::software_reset();
                }
            }
        }};
    }

    // Before deep sleep: let any refresh finish, put the panel to sleep and cut its rail
    macro_rules! power_down_panel {
        () => {{
            epd.sleep(&mut delay).ok();
            panel_power_off(&mut i2c);
        }};
    }

    // Drive mode tick that puts the panel to sleep once the refresh started before it ends
    macro_rules! sleep_panel_when_idle {
        ($busy:ident) => {
            || {
                if $busy && !epd.is_busy() {
                    epd.sleep(&mut delay).ok();
                    $busy = false;
                }
            }
        };
    }

    // Show the setup screen for `$error`, expose the card to a connected computer (rebooting
    // afterwards to read the new config), then sleep until KEY, BOOT, USB, or the `$retry`
    // timer. The panel refreshes while the drive starts.
    macro_rules! enter_setup {
        ($error:expr, $retry:expr) => {{
            let error: ConfigError = $error;
            info!("Setup needed: {:?}", error);
            let message = error.message();
            // Skip the refresh when the same setup screen is already shown
            let shown = message_hash(&message, orientation);
            let mut panel_busy = false;
            if !(setup_pending && unsafe { SETUP_SHOWN } == shown) {
                framebuffer.clear(sawthat_frame_firmware::epd::Color::White);
                progress::draw_setup(&mut framebuffer, orientation, &message);
                match epd.display_start(framebuffer.as_slice(), &mut delay) {
                    Ok(()) => {
                        unsafe { SETUP_SHOWN = shown };
                        panel_busy = true;
                    }
                    Err(e) => info!("Setup screen refresh failed: {:?}", e),
                }
            }
            if usb_host_connected().await {
                drive_mode!(sleep_panel_when_idle!(panel_busy));
            }
            while epd.is_busy() {
                Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
            }
            power_down_panel!();
            sleep_on_setup(&mut i2c, $retry)
        }};
    }

    // Draw the drive mode box over the current screen and expose the card to a connected
    // computer while the panel refreshes
    macro_rules! drive_mode_with_notice {
        () => {{
            if sd_cache.is_some() {
                let rect = progress::draw_drive_mode(&mut framebuffer, orientation);
                let region = framebuffer.extract_rect(&rect);
                let mut panel_busy = match epd.partial_update_start(&rect, &region, &mut delay) {
                    Ok(()) => true,
                    Err(e) => {
                        info!("Drive mode box refresh failed: {:?}", e);
                        false
                    }
                };
                drive_mode!(sleep_panel_when_idle!(panel_busy));
                while epd.is_busy() {
                    Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
                }
            }
        }};
    }

    // Show the report of a crash in the previous boot, then stay off until KEY. The report is
    // cleared only once shown, so a crash while showing it shuts down instead of looping.
    if let Some(report) = crash::pending() {
        let (version, trace, urls) = (report.version(), report.trace(), report.issue_urls());
        info!(
            "Showing crash report from {}: {}",
            version,
            report.message()
        );
        for line in &trace {
            info!("  {}", line);
        }
        info!("Report link: {}", urls[0]);
        progress::draw_crash(
            &mut framebuffer,
            orientation,
            report.message(),
            &version,
            &trace,
            &urls,
        );
        match epd.display_start(framebuffer.as_slice(), &mut delay) {
            Ok(()) => {
                while epd.is_busy() {
                    Timer::after(Duration::from_millis(DISPLAY_BUSY_POLL_MS)).await;
                }
            }
            Err(e) => info!("Crash screen refresh failed: {:?}", e),
        }
        power_down_panel!();
        crash::clear();
        boot_fresh_next();
        unsafe { SETUP_SHOWN = 0 };
        info!("Shutting down until KEY");
        sleep::shutdown();
    }

    let mut config = match config {
        Ok(config) => config,
        Err(e) => enter_setup!(e, None),
    };
    info!("Refresh interval: {} minutes", config.refresh_minutes);

    // BOOT pressed during sleep: enter drive mode if a computer is connected, otherwise sleep
    // until the timer wake that was already scheduled
    if boot_wake {
        info!("Woken by BOOT");
        let host = usb_host_connected().await;
        if host {
            // Returns only if no computer enumerated the drive, with the card handed over
            drive_mode_with_notice!();
        }
        if resuming || host {
            power_down_panel!();
            if resuming {
                sleep_until_scheduled(&mut rtc, &mut i2c);
            }
            sleep_until_wake(&mut i2c, config.refresh_secs());
        }
    }

    // Charger plugged, unplugged, started or finished during sleep: redraw only the battery
    // body, then sleep until the timer wake that was already scheduled
    if charger_wake && resuming {
        info!("Charger changed");
        let power = read_power(&mut i2c).await;
        log_power(sd_cache.as_mut(), &rtc, wake_label, &power);
        draw_battery_icon(&mut framebuffer, orientation, power);
        let rect = battery_body_rect(orientation);
        let region = framebuffer.extract_rect(&rect);
        if let Err(e) = epd.partial_update(&rect, &region, &mut delay) {
            info!("Battery refresh failed: {:?}", e);
        }
        power_down_panel!();
        sleep_until_scheduled(&mut rtc, &mut i2c);
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
    let mut tcp_client: Option<TcpClient<'static, 2, TCP_TX_SIZE, TCP_RX_SIZE>> = None;
    let mut dns_socket: Option<DnsSocket<'static>> = None;
    let mut net_stack: Option<Stack<'static>> = None;

    // Initialize WiFi and connect, evaluating to whether it is connected
    macro_rules! ensure_wifi {
        () => {{
            if !wifi_connected {
                start_fast_blink();

                if net_stack.is_none() {
                    let (stk, runner) = embassy_net::new(
                        Interface::station(),
                        embassy_net::Config::dhcpv4(Default::default()),
                        mk_static!(StackResources<4>, StackResources::<4>::new()),
                        rng.random() as u64,
                    );
                    spawner.spawn(net_task(runner).unwrap());

                    let tcp_state = alloc_tcp_state();
                    tcp_client = Some(TcpClient::new(stk, tcp_state));
                    dns_socket = Some(DnsSocket::new(stk));
                    net_stack = Some(stk);
                }
                if wifi_controller.is_none() {
                    // SAFETY: the controller of an earlier connection was dropped, so this is
                    // the only user of the peripheral
                    let wifi = unsafe { wifi_peripheral.clone_unchecked() };
                    wifi_controller = Some(WifiController::new(wifi, Default::default()).unwrap());
                }

                let stack = net_stack.unwrap();
                let ctrl = wifi_controller.as_mut().unwrap();
                set_wifi_reason(None);
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

                if !wifi_connected {
                    info!("WiFi connection failed");
                    wifi_stop(&mut wifi_controller).await;
                }
            }
            wifi_connected
        }};
    }

    // ==================== Concert List ====================
    let mut items: ConcertList = sd_cache
        .as_mut()
        .and_then(|c| c.load_concert_list(orientation))
        .unwrap_or_default();
    info!("{} concerts in the {:?} list", items.len(), orientation);

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
        display::shuffle(&mut items, shuffle_seed);

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
                let mut synced: Option<ConcertList> = None;

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
                                wifi_reason(),
                                WIFI_LINKED.load(Ordering::Relaxed),
                                WIFI_AUTH_FAILED.load(Ordering::Relaxed)
                            )
                        ),
                        Some(config.refresh_secs())
                    );
                }
                // A username in place of the user ID: look the ID up once and store it
                if sd_cache.is_some() && wifi_connected && !config::is_uuid(&config.user_id) {
                    let mut net = sync::Net {
                        tcp: tcp_client.as_ref().unwrap(),
                        dns: dns_socket.as_ref().unwrap(),
                        tls_read: &mut *tls_read_buf,
                        tls_write: &mut *tls_write_buf,
                        cover_tls_read: &mut *cover_tls_read_buf,
                        cover_tls_write: &mut *cover_tls_write_buf,
                    };
                    match sync::resolve_user_id(&mut net, &config.user_id).await {
                        Ok(id) => {
                            info!("SawThat user {} has ID {}", config.user_id, id);
                            if let Some(cache) = sd_cache.as_mut() {
                                store_config_value(cache, config::USER_ID_KEY, &id);
                            }
                            config.user_id = id;
                        }
                        // The page loaded but names no user
                        Err(e @ (sync::SyncError::NoUserId | sync::SyncError::Http(_))) => {
                            info!("SawThat user lookup failed: {:?}", e);
                            enter_setup!(
                                ConfigError::UserNotFound(config.user_id.clone()),
                                Some(config.refresh_secs())
                            );
                        }
                        Err(e) => info!("SawThat user lookup failed, retrying next sync: {:?}", e),
                    }
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

                        let mut ready = ConcertList::new();
                        let mut valid = ConcertList::new();
                        for (concert, &is_ready) in concerts.iter().zip(&ready_flags) {
                            if is_ready {
                                ready.push(concert.path.clone());
                            }
                            valid.push(concert.path.clone());
                        }

                        // Keep artwork for every current concert in both orientations
                        match cache.cleanup_stale(&valid) {
                            Ok(n) if n > 0 => info!("Removed {} stale files", n),
                            Ok(_) => {}
                            Err(e) => info!("Cleanup failed: {:?}", e),
                        }
                        if let Err(e) = cache.store_concert_list(orientation, &ready) {
                            info!("Failed to store the concert list: {:?}", e);
                        }
                        // The cleanup may have removed artwork the other orientation's list
                        // still names (a concert or the user changed); keep only current items
                        let other = orientation.toggle();
                        if let Some(mut list) = cache.load_concert_list(other) {
                            let len = list.len();
                            list.retain(|item| valid.contains(item));
                            if list.len() != len
                                && let Err(e) = cache.store_concert_list(other, &list)
                            {
                                info!("Failed to prune the {:?} concert list: {:?}", other, e);
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
                    wifi_stop(&mut wifi_controller).await;
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
                    display::shuffle(&mut fresh, shuffle_seed);
                    if fresh != items {
                        info!("Concert list changed ({} concerts), reshuffling", fresh.len());
                        shuffle_seed = (rng.random() as u64) << 32 | rng.random() as u64;
                        display::shuffle(&mut fresh, shuffle_seed);
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
                info!("All concerts shown");
                index = 0;
                need_sync = true;
            }

            info!("Waking up display...");
            epd.wake_up(&mut delay).expect("Failed to wake display");

            let power = read_power(&mut i2c).await;
            log_power(sd_cache.as_mut(), &rtc, wake_label, &power);

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
                    "Partial refresh: concert {} of {}",
                    item_idx + 1,
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
                        info!("No concerts with artwork");
                        break;
                    }
                    continue;
                }
                draw_battery_icon(&mut framebuffer, orientation, power);

                framebuffer.extract_half(next_slot, &mut *half_buffer);
                let x_offset = if next_slot == 0 { 0 } else { 400 };
                let rect = Rect::new(x_offset, 0, 400, 480);

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
                        info!("No concerts with artwork");
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
                    if let Some(cache) = sd_cache.as_mut() {
                        save_orientation(cache, orientation);
                    }

                    // Each orientation has its own artwork; sync if none is cached
                    items = sd_cache
                        .as_mut()
                        .and_then(|c| c.load_concert_list(orientation))
                        .unwrap_or_default();
                    display::shuffle(&mut items, shuffle_seed);
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
                    info!("Button tap during update, next concert (index={})", index);
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

    if wifi_connected {
        info!("Disconnecting WiFi for deep sleep...");
        wifi_stop(&mut wifi_controller).await;
    }

    // BOOT pressed while awake: enter drive mode if a computer is connected
    if BOOT_PRESSED.swap(false, Ordering::Relaxed) && usb_host_connected().await {
        info!("BOOT pressed with a computer connected");
        epd.wake_up(&mut delay).ok();
        drive_mode_with_notice!();
    }

    unsafe {
        NEXT_WAKE_US =
            rtc.time_since_power_up().as_micros() as u64 + config.refresh_secs() * 1_000_000;
    }
    power_down_panel!();
    sleep_until_wake(&mut i2c, config.refresh_secs());
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

/// Set the `mask` bits of an AXP2101 register to `value`, keeping the others
fn pmic_update(i2c: &mut I2c<'_, esp_hal::Blocking>, reg: u8, mask: u8, value: u8) -> bool {
    pmic_read(i2c, reg).is_some_and(|old| {
        i2c.write(AXP2101_ADDR, &[reg, (old & !mask) | (value & mask)])
            .is_ok()
    })
}

/// Cut the panel rail for deep sleep; the next boot turns it back on. Call only after the
/// panel is put to sleep, which waits for any refresh to finish.
fn panel_power_off(i2c: &mut I2c<'_, esp_hal::Blocking>) {
    if !pmic_update(i2c, LDO_ONOFF_CTRL0, ALDO4, 0) {
        info!("Panel rail not cut");
    }
}

/// Battery level and power symbol, as shown by the battery icon, with the raw PMIC readings
#[derive(Clone, Copy)]
struct Power {
    percent: u8,
    symbol: battery::PowerSymbol,
    mv: Option<u16>,
    gauge: Option<u8>,
    vbus: bool,
    charging: bool,
    full: bool,
}

/// Read battery level (from cell voltage, falling back to the fuel gauge) and power source:
/// a check when plugged in and full, a USB icon when connected to a computer, and a bolt
/// when charging from anything else
async fn read_power(i2c: &mut I2c<'_, esp_hal::Blocking>) -> Power {
    let (mv, vbus, charging, full, stage) = read_charger(i2c);
    let gauge = pmic_read(i2c, BAT_PERCENT_REG);
    let percent = mv.map(battery_percent_from_mv).or(gauge).unwrap_or(50);
    let symbol = if vbus && full {
        battery::PowerSymbol::Full
    } else if vbus && usb_host_connected().await {
        battery::PowerSymbol::Usb
    } else if charging {
        battery::PowerSymbol::Bolt
    } else {
        battery::PowerSymbol::None
    };
    info!(
        "Power: {}% ({} mV), {}, {:?}",
        percent,
        mv.unwrap_or(0),
        stage,
        symbol
    );
    Power {
        percent,
        symbol,
        mv,
        gauge,
        vbus,
        charging,
        full,
    }
}

/// Append a power reading to `BATTERY_LOG`, stamped with the RTC time, which keeps counting
/// through deep sleep and restarts on power loss or reset
fn log_power<S: SpiDevice, D: DelayNs>(
    cache: Option<&mut SdCache<S, D>>,
    rtc: &Rtc,
    wake: &str,
    power: &Power,
) {
    let Some(cache) = cache else { return };
    let opt = |v: Option<u16>| v.map(|v| alloc::format!("{}", v)).unwrap_or_default();
    let row = alloc::format!(
        "{},{},{},{},{},{},{},{}\n",
        rtc.time_since_power_up().as_secs(),
        wake,
        opt(power.mv),
        power.percent,
        opt(power.gauge.map(u16::from)),
        power.vbus as u8,
        power.charging as u8,
        power.full as u8,
    );
    let header = b"uptime_s,wake,mv,percent,gauge,vbus,charging,full\n";
    if let Err(e) = cache.append_root_file(BATTERY_LOG, header, row.as_bytes()) {
        info!("Battery log write failed: {:?}", e);
    }
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

/// AXP2101 charger state: battery voltage in mV, whether USB power is present, whether the
/// battery is charging, whether charging is done, and the charge stage
fn read_charger(
    i2c: &mut I2c<'_, esp_hal::Blocking>,
) -> (Option<u16>, bool, bool, bool, &'static str) {
    const STATUS1: u8 = 0x00;
    const STATUS2: u8 = 0x01;
    const ADC_ENABLE: u8 = 0x30;
    const VBAT_H: u8 = 0x34;
    const VBAT_L: u8 = 0x35;

    let vbus = pmic_read(i2c, STATUS1).is_some_and(|s1| s1 & 0x20 != 0);

    let (mut charging, mut full, mut stage) = (false, false, "charger unknown");
    if let Some(s2) = pmic_read(i2c, STATUS2) {
        charging = (s2 >> 5) & 0x03 == 1;
        full = s2 & 0x07 == 4;
        stage = match s2 & 0x07 {
            0 => "trickle charging",
            1 => "pre-charging",
            2 => "charging (constant current)",
            3 => "charging (constant voltage)",
            4 => "charged",
            _ => "not charging",
        };
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
    // A zero reading means the ADC has not sampled yet
    (mv.filter(|&mv| mv > 0), vbus, charging, full, stage)
}

/// Hash of a concert list, to notice when it changes
fn hash_data(items: &ConcertList) -> u32 {
    let mut hash: u32 = 5381;
    for item in items.iter() {
        for byte in item.as_bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(*byte as u32);
        }
        hash = hash.wrapping_mul(33).wrapping_add(0); // separator
    }
    hash
}

/// Set `key` to `value` in `CONFIG.TXT`, keeping the rest of the file
fn write_config_value<S: SpiDevice, D: DelayNs>(
    cache: &mut SdCache<S, D>,
    key: &str,
    value: &str,
) -> Result<(), CacheError> {
    let data = cache.read_root_file(config::CONFIG_FILE, config::MAX_CONFIG)?;
    let text = core::str::from_utf8(&data).map_err(|_| CacheError::Read)?;
    let text = config::set_value(text, key, value);
    cache.write_root_file(config::CONFIG_FILE, text.as_bytes())
}

/// Set `key` to `value` in `CONFIG.TXT`, logging a failure
fn store_config_value<S: SpiDevice, D: DelayNs>(cache: &mut SdCache<S, D>, key: &str, value: &str) {
    match write_config_value(cache, key, value) {
        Ok(()) => info!("Stored {} in {}", key, config::CONFIG_FILE),
        Err(e) => info!("Failed to store {}: {:?}", key, e),
    }
}

/// Set the orientation in `CONFIG.TXT`, keeping the rest of the file, and drop the orientation
/// file of older firmware once it is stored there
fn save_orientation<S: SpiDevice, D: DelayNs>(cache: &mut SdCache<S, D>, orientation: Orientation) {
    let value = config::orientation_value(orientation);
    match write_config_value(cache, config::ORIENTATION_KEY, value) {
        Ok(()) => {
            info!(
                "Stored orientation {:?} in {}",
                orientation,
                config::CONFIG_FILE
            );
            cache.remove_legacy_orientation();
        }
        Err(e) => info!("Failed to store orientation: {:?}", e),
    }
}

/// Apply the web installer's settings to `CONFIG.TXT`, starting from the template when the card has
/// none, then erase them. They stay in flash for the next boot if the card cannot be written.
fn apply_installer_settings<S: SpiDevice, D: DelayNs>(
    cache: &mut SdCache<S, D>,
    installer: &mut Installer,
) {
    let Some(settings) = installer.settings() else {
        return;
    };
    let base = match cache.read_root_file(config::CONFIG_FILE, config::MAX_CONFIG) {
        Ok(data) => alloc::string::String::from_utf8(data)
            .unwrap_or_else(|_| alloc::string::String::from(config::TEMPLATE)),
        Err(CacheError::NotFound) => alloc::string::String::from(config::TEMPLATE),
        Err(e) => {
            info!(
                "Installer settings not applied, {} unreadable: {:?}",
                config::CONFIG_FILE,
                e
            );
            return;
        }
    };
    let text = settings.iter().fold(base, |text, (key, value)| {
        config::set_value(&text, key, value)
    });
    match cache.write_root_file(config::CONFIG_FILE, text.as_bytes()) {
        Ok(()) => {
            info!(
                "Applied {} installer settings to {}",
                settings.len(),
                config::CONFIG_FILE
            );
            installer.clear_settings();
        }
        Err(e) => info!("Installer settings not applied: {:?}", e),
    }
}

/// Copy the web installer's font to the SD root, replacing any other font file, then erase it.
/// It stays in flash for the next boot if the card cannot be written.
fn apply_installer_font<S: SpiDevice, D: DelayNs>(
    cache: &mut SdCache<S, D>,
    installer: &mut Installer,
) {
    let Some((name, font)) = installer.font() else {
        return;
    };
    let write = |cache: &mut SdCache<S, D>| -> Result<(), CacheError> {
        for other in FONT_FILES.iter().filter(|&&other| other != name) {
            cache.remove_root_file(other)?;
        }
        cache.write_root_file(name, &font)
    };
    match write(cache) {
        Ok(()) => {
            info!(
                "Stored the installer font as {} ({} bytes)",
                name,
                font.len()
            );
            installer.clear_font(font.len());
        }
        Err(e) => info!("Installer font not stored: {:?}", e),
    }
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

/// Whether `pin` ended the deep sleep that this boot woke from
fn caused_wakeup(pin: impl InputPin + 'static) -> bool {
    Input::new(pin, InputConfig::default().with_pull(Pull::Up)).caused_wakeup()
}

/// Deep sleep until KEY, BOOT, the AXP2101 IRQ line, or `seconds` if given
fn deep_sleep(seconds: Option<u64>) -> ! {
    sleep::deep_sleep(
        [
            sleep::key_pin(),
            sleep::boot_pin(),
            sleep::charger_irq_pin(),
        ],
        seconds,
    )
}

/// Make the next wake sync like a power-on
fn boot_fresh_next() {
    unsafe {
        let state = &raw mut SLEEP_STATE;
        (*state).invalidate();
        SETUP_PENDING = true;
    }
}

/// Deep sleep on the setup screen until KEY, a USB plug, or `retry_secs` (if any), then boot
/// like a power-on
fn sleep_on_setup(i2c: &mut I2c<'_, esp_hal::Blocking>, retry_secs: Option<u64>) -> ! {
    boot_fresh_next();
    arm_charger_irq(i2c);
    info!(
        "Entering deep sleep until KEY, USB, or retry in {:?} s...",
        retry_secs
    );
    deep_sleep(retry_secs)
}

/// Identifies a setup screen by its message and orientation
fn message_hash(message: &str, orientation: Orientation) -> u32 {
    message
        .bytes()
        .chain([orientation as u8])
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(b as u32))
}

/// Sleep until the timer wake that was already scheduled (or KEY, BOOT, or a charger change)
fn sleep_until_scheduled(rtc: &mut Rtc, i2c: &mut I2c<'_, esp_hal::Blocking>) -> ! {
    let now = rtc.time_since_power_up().as_micros();
    let remaining = unsafe { NEXT_WAKE_US }.saturating_sub(now) / 1_000_000;
    sleep_until_wake(i2c, remaining.max(1))
}

/// Arm the charger wake and deep sleep for `seconds`, or until KEY, BOOT, or a charger change.
///
/// The AXP2101 IRQ line signals USB plug/unplug and charge start/done.
fn sleep_until_wake(i2c: &mut I2c<'_, esp_hal::Blocking>, seconds: u64) -> ! {
    arm_charger_irq(i2c);
    info!(
        "Entering deep sleep for {} seconds (KEY or a charger change wakes early)...",
        seconds
    );
    deep_sleep(Some(seconds))
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
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

fn wifi_reason() -> Option<DisconnectReason> {
    critical_section::with(|cs| WIFI_REASON.borrow(cs).get())
}

fn set_wifi_reason(reason: Option<DisconnectReason>) {
    critical_section::with(|cs| WIFI_REASON.borrow(cs).set(reason));
}

/// Station settings for the configured network; an empty password means an open network
fn station_config(config: &Config) -> Option<StationConfig> {
    let ssid = config.wifi_ssid.as_str().try_into().ok()?;
    let authentication = if config.wifi_pass.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(config.wifi_pass.as_str().try_into().ok()?)
    };
    Some(
        StationConfig::default()
            .with_ssid(ssid)
            .with_authentication(authentication),
    )
}

/// Connect to the configured network, retrying until it joins (true) or fails the same
/// definite way twice (false)
async fn wifi_connect(controller: &mut WifiController<'static>, config: &Config) -> bool {
    start_fast_blink();

    let Some(station) = station_config(config) else {
        info!("WiFi SSID or password is too long");
        return false;
    };
    if let Err(e) = controller.set_config(&WifiConfig::Station(station)) {
        info!("WiFi config failed: {:?}", e);
        return false;
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
                let reason = match e {
                    ConnectionError::Failed(info) => Some(info.reason),
                    _ => None,
                };
                set_wifi_reason(reason);
                let cause =
                    WifiCause::classify(reason, false, WIFI_AUTH_FAILED.load(Ordering::Relaxed));
                if cause == WifiCause::Auth {
                    WIFI_AUTH_FAILED.store(true, Ordering::Relaxed);
                }
                info!("Failed to connect: {e:?}");
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

/// Disconnect, and drop the controller to power the radio down
async fn wifi_stop(controller: &mut Option<WifiController<'static>>) {
    if let Some(mut controller) = controller.take() {
        if let Err(e) = controller.disconnect_async().await {
            info!("Disconnect error (may already be disconnected): {:?}", e);
        }
        info!("WiFi stopped");
    }
}

/// Wait for network stack to get an IP address
async fn wait_for_ip(stack: Stack<'static>) {
    loop {
        if stack.is_link_up() {
            break;
        }
        // 1500ms polling is sufficient - link up is not time-critical
        Timer::after(Duration::from_millis(1500)).await;
    }

    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP {}", config.address);
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
            Ok(()) => {
                match orientation {
                    Orientation::Horizontal => info!("Showing {} in slot {}", path, slot),
                    Orientation::Vertical => info!("Showing {}", path),
                }
                true
            }
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
        // Body is 42x24 right of the 6px tip in the framebuffer (mirrored, see
        // `battery::draw_battery`); x is cut to the 8-pixel window alignment
        Orientation::Horizontal => Rect::new(x + 8, y, 40, 24),
        // Body is 24x42 below the 6px tip
        Orientation::Vertical => Rect::new(x, y + 6, 24, 42),
    }
}

/// TCP socket buffers for both sync connections, allocated in PSRAM and kept for the
/// program's lifetime so they don't take internal RAM from the main stack
fn alloc_tcp_state() -> &'static TcpClientState<2, TCP_TX_SIZE, TCP_RX_SIZE> {
    let layout = core::alloc::Layout::new::<TcpClientState<2, TCP_TX_SIZE, TCP_RX_SIZE>>();
    // SAFETY: zeroed memory is identical to `TcpClientState::new()` in embassy-net 0.9 (all
    // pool slots unused, buffers uninitialized); allocating it directly avoids building the
    // struct on the stack. The allocation is never freed.
    unsafe {
        let ptr =
            alloc::alloc::alloc_zeroed(layout) as *mut TcpClientState<2, TCP_TX_SIZE, TCP_RX_SIZE>;
        assert!(!ptr.is_null(), "TCP buffer allocation failed");
        &*ptr
    }
}
