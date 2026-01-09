#![no_std]

extern crate alloc;

pub mod battery;
pub mod cache;
pub mod display;
pub mod epd;
pub mod framebuffer;
pub mod widget;

/// Timestamped logger for the `log` crate - adds timestamps to all log messages
pub struct TimestampLogger;

impl TimestampLogger {
    /// Initialize the timestamped logger at the specified level
    pub fn init(level: log::LevelFilter) {
        unsafe {
            log::set_logger_racy(&LOGGER).unwrap();
            log::set_max_level_racy(level);
        }
    }
}

static LOGGER: TimestampLogger = TimestampLogger;

impl log::Log for TimestampLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let now = embassy_time::Instant::now();
            let ms = now.as_millis();
            let secs = ms / 1000;
            let millis = ms % 1000;

            let level = match record.level() {
                log::Level::Error => "ERROR",
                log::Level::Warn => "WARN",
                log::Level::Info => "INFO",
                log::Level::Debug => "DEBUG",
                log::Level::Trace => "TRACE",
            };

            esp_println::println!(
                "[{:>4}.{:03}] {:>5} - {}",
                secs,
                millis,
                level,
                record.args()
            );
        }
    }

    fn flush(&self) {}
}
