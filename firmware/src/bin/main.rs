//! PhotoPainter - ESP32-S3 E-Paper Photo Frame
//!
//! Environment variables required:
//! - WIFI_SSID: WiFi network name
//! - WIFI_PASS: WiFi password
//! - EDGE_URL: Edge service URL (e.g., http://192.168.1.100:7676)

#![no_std]
#![no_main]

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
    wifi::{ClientConfig, ModeConfig, WifiController, WifiDevice},
};
use photopainter::battery;
use photopainter::display::{self, TLS_READ_BUF_SIZE, TLS_WRITE_BUF_SIZE};
use photopainter::epd::{Epd7in3e, Rect, RefreshMode, WIDTH};
use photopainter::framebuffer::Framebuffer;
use photopainter::widget::{Orientation, WidgetData, MAX_ITEMS};

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

/// Magic number to validate RTC memory state
const SLEEP_STATE_MAGIC: u32 = 0xCAFE_F00D;

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
    /// Cache keys of items (to detect data changes)
    cache_keys: [u32; MAX_ITEMS],
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
            cache_keys: [0; MAX_ITEMS],
        }
    }

    fn is_valid(&self) -> bool {
        self.magic == SLEEP_STATE_MAGIC
    }

    #[allow(dead_code)]
    fn invalidate(&mut self) {
        self.magic = 0;
    }

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
        for (i, item) in items.iter().enumerate() {
            self.cache_keys[i] = item.cache_key;
        }
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
        if items.len() != self.total_items {
            return false;
        }
        for (i, item) in items.iter().enumerate() {
            if self.cache_keys[i] != item.cache_key {
                return false;
            }
        }
        true
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
    let resuming = unsafe { (*(&raw const SLEEP_STATE)).is_valid() };
    let mut orientation = if resuming {
        unsafe { (*(&raw const SLEEP_STATE)).get_orientation() }
    } else {
        Orientation::default()
    };

    // Track if we should advance to next item (button tap without hold)
    let mut advance_item = false;

    if button_wake {
        // Button caused wake - check if held for 500ms total (boot takes ~200ms)
        delay.delay_ms(300);

        if key_input.is_low() {
            // Button held - toggle orientation
            orientation = orientation.toggle();

            // Flash LED 3 times for rotation
            for _ in 0..3 {
                led_green.set_low();  // ON
                delay.delay_ms(100);
                led_green.set_high(); // OFF
                delay.delay_ms(100);
            }

            // Wait for button release
            while key_input.is_low() {
                delay.delay_ms(50);
            }
        } else {
            // Button tap - advance to next item
            advance_item = true;

            // Flash LED 1 time for next item
            led_green.set_low();  // ON
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

    // ==================== WiFi Setup ====================
    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

    let (mut controller, interfaces) =
        esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();

    let wifi_interface = interfaces.sta;

    let net_config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    // Start network task (runs continuously)
    spawner.spawn(net_task(runner)).ok();

    // Initial WiFi connection
    wifi_connect(&mut controller).await;
    wait_for_ip(stack).await;

    // ==================== RTC for Deep Sleep ====================
    let mut rtc = Rtc::new(peripherals.LPWR);

    // ==================== Main Display Logic ====================
    println!("Starting display update...");
    println!("Edge URL: {}", EDGE_URL);
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
        println!("Resuming from deep sleep: index={}, total={}, orientation={:?}", index, total, orientation);
    } else {
        println!("Fresh boot (no valid sleep state)");
    }

    // Allocate framebuffer (uses PSRAM for the 192KB buffer)
    println!("Allocating framebuffer...");
    let mut framebuffer = Framebuffer::new();
    println!("Framebuffer allocated!");

    // Use RNG for shuffle seed
    let rng = Rng::new();

    // Allocate TLS buffers for HTTPS support
    let mut tls_read_buf = [0u8; TLS_READ_BUF_SIZE];
    let mut tls_write_buf = [0u8; TLS_WRITE_BUF_SIZE];

    // Create TCP client and DNS socket for HTTP requests
    let tcp_state = mk_static!(TcpClientState<1, 1024, 1024>, TcpClientState::new());
    let tcp_client = TcpClient::new(stack, tcp_state);
    let dns_socket = DnsSocket::new(stack);

    // Fetch widget data (with retries)
    println!("Fetching widget data...");
    let mut items: WidgetData = loop {
        start_blink();
        let result = display::fetch_widget_data(
            &tcp_client,
            &dns_socket,
            &mut tls_read_buf,
            &mut tls_write_buf,
            EDGE_URL,
            "concerts",
        )
        .await;
        stop_blink();

        match result {
            Ok(data) => break data,
            Err(e) => {
                println!("Failed to fetch widget data: {:?}, retrying in 30s...", e);
                Timer::after(Duration::from_secs(30)).await;
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
    let data_matches = resuming && unsafe { (*(&raw const SLEEP_STATE)).matches_data(&items) };

    // Determine if we can use partial refresh
    let saved_orientation = if resuming {
        unsafe { (*(&raw const SLEEP_STATE)).get_orientation() }
    } else {
        Orientation::Horizontal
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
            // ==================== Partial Refresh Mode ====================
            // Only update one half of the display with a single new item
            let item_idx = index % total_items;
            println!(
                "Partial update: slot={}, item={} of {}",
                next_slot, item_idx, total_items
            );

            start_blink();

            // Fetch single image to the target slot
            let fetch_result = display::fetch_single_to_framebuffer(
                &tcp_client,
                &dns_socket,
                &mut tls_read_buf,
                &mut tls_write_buf,
                &mut framebuffer,
                EDGE_URL,
                "concerts",
                &items,
                item_idx,
                next_slot,
            )
            .await;

            // Draw battery indicator in the slot being updated (if right side)
            if fetch_result.is_ok() && next_slot == 1 {
                let (bat_w, _bat_h) = battery::battery_dimensions(false);
                let battery_x = WIDTH as u16 - bat_w - 8;
                let battery_y = 8;
                battery::draw_battery(
                    framebuffer.as_mut_slice(),
                    battery_x,
                    battery_y,
                    battery_percent,
                    false,
                );
            }

            let result = match fetch_result {
                Ok(()) => {
                    // Extract the half we need to update
                    let mut half_buffer = [0u8; HALF_BUFFER_SIZE];
                    framebuffer.extract_half(next_slot, &mut half_buffer);

                    // Create rect for the half (left: x=0, right: x=400)
                    let x_offset = if next_slot == 0 { 0 } else { 400 };
                    let rect = Rect::new(x_offset, 0, 400, 480);

                    println!("Partial refresh: x={}, w={}, h={}", x_offset, 400, 480);

                    // Do partial update
                    match epd.partial_update_start(&rect, &half_buffer, &mut delay) {
                        Ok(()) => {
                            while epd.is_busy() {
                                Timer::after(Duration::from_millis(50)).await;
                            }
                            epd.refresh_wait(&mut delay).map_err(|_| display::DisplayError::Network)
                        }
                        Err(_) => Err(display::DisplayError::Network),
                    }
                }
                Err(e) => Err(e),
            };
            stop_blink();
            embassy_futures::yield_now().await;

            // Update slot tracking
            if result.is_ok() {
                slot_items[next_slot as usize] = item_idx;
                next_slot = (next_slot + 1) % 2;
                index += 1; // Advance by 1 for partial updates
            }

            result
        } else {
            // ==================== Full Refresh Mode ====================
            // Update entire display with 2 items (horizontal) or 1 item (vertical)
            println!(
                "Full refresh: items {} and {} of {}",
                index,
                (index + 1).min(total_items - 1),
                total_items
            );

            start_blink();
            let fetch_result = display::fetch_to_framebuffer(
                &tcp_client,
                &dns_socket,
                &mut tls_read_buf,
                &mut tls_write_buf,
                &mut framebuffer,
                EDGE_URL,
                "concerts",
                orientation,
                &items,
                index,
            )
            .await;

            // Draw battery indicator into framebuffer
            if fetch_result.is_ok() {
                let vertical = orientation == Orientation::Vertical;
                let (bat_w, _bat_h) = battery::battery_dimensions(vertical);
                let battery_x = WIDTH as u16 - bat_w - 8;
                let battery_y = 8;
                battery::draw_battery(
                    framebuffer.as_mut_slice(),
                    battery_x,
                    battery_y,
                    battery_percent,
                    vertical,
                );
            }

            let result = match fetch_result {
                Ok(()) => {
                    println!("Updating display (full refresh)...");
                    match epd.display_start(framebuffer.as_slice(), &mut delay) {
                        Ok(()) => {
                            while epd.is_busy() {
                                Timer::after(Duration::from_millis(50)).await;
                            }
                            epd.finish_display(&mut delay).map_err(|_| display::DisplayError::Network)
                        }
                        Err(_) => Err(display::DisplayError::Network),
                    }
                }
                Err(e) => Err(e),
            };
            stop_blink();
            embassy_futures::yield_now().await;

            // Update slot tracking for horizontal mode (enables partial updates next time)
            if result.is_ok() && orientation == Orientation::Horizontal {
                slot_items[0] = index % total_items;
                slot_items[1] = (index + 1) % total_items;
                next_slot = 0;
                index += 2;
                use_partial = true; // Enable partial updates for subsequent refreshes
            } else if result.is_ok() {
                index += 1; // Vertical mode: advance by 1
            }

            result
        };

        match display_result {
            Ok(()) => println!("Display refresh successful!"),
            Err(e) => println!("Display refresh failed: {:?}", e),
        }

        // Put display to sleep
        println!("Putting display to sleep...");
        epd.sleep(&mut delay).expect("Failed to sleep display");

        // Wait 30s for button input before deep sleep
        println!("Press KEY within 30s (tap=next item, hold=rotate)...");
        let mut should_redisplay = false;
        for _ in 0..300 {
            if key_input.is_low() {
                println!("KEY pressed, checking for 500ms hold...");
                // Wait 500ms and check if button is still held
                delay.delay_ms(500);

                if key_input.is_low() {
                    // Button held - toggle orientation
                    println!("Button held! Toggling orientation...");
                    orientation = orientation.toggle();
                    // Reset partial mode on orientation change
                    use_partial = false;
                    slot_items = [0, 0];
                    next_slot = 0;

                    // Flash LED2 3 times to confirm rotation
                    for _ in 0..3 {
                        led_green.set_low();  // ON
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
                    // Button released - show next item
                    // Index already incremented by previous display, just trigger redisplay
                    println!("Button tap, next item (index={})", index);

                    // Flash LED2 1 time to confirm next item
                    led_green.set_low();  // ON
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
        (*state).save(index, total_items, shuffle_seed, orientation, next_slot, slot_items, &items);
    }
    println!(
        "Saved state: index={}, total={}, orientation={:?}, next_slot={}, slot_items=[{}, {}]",
        index, total_items, orientation, next_slot, slot_items[0], slot_items[1]
    );

    // Disconnect WiFi before deep sleep
    println!("Disconnecting WiFi for deep sleep...");
    wifi_disconnect(&mut controller).await;

    // Drop the Input and reclaim GPIO4 for deep sleep wake source
    drop(key_input);
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
