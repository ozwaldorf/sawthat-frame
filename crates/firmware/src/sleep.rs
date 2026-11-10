//! Deep sleep with pin and timer wake sources.

use esp_hal::delay::Delay;
use esp_hal::gpio::{AnyPin, Event, Input, InputConfig, Pin, Pull, WakeupConfig};
use esp_hal::peripherals::{GPIO0, GPIO4, GPIO21, LPWR};
use esp_hal::rtc_cntl::sleep::{LowPower, RtcSleepConfig};
use esp_hal::time::{Duration, Instant};

/// KEY button (GPIO4, active low)
pub fn key_pin() -> AnyPin<'static> {
    // Reclaimed from its driver, which is not used again before the sleep
    unsafe { GPIO4::steal() }.degrade()
}

/// BOOT button (GPIO0, active low)
pub fn boot_pin() -> AnyPin<'static> {
    unsafe { GPIO0::steal() }.degrade()
}

/// AXP2101 IRQ line (GPIO21, active low)
pub fn charger_irq_pin() -> AnyPin<'static> {
    unsafe { GPIO21::steal() }.degrade()
}

/// Deep sleep until one of `pins` goes low, or `seconds` pass if given. The pins are held high by
/// pull-ups during the sleep.
pub fn deep_sleep(pins: impl IntoIterator<Item = AnyPin<'static>>, seconds: Option<u64>) -> ! {
    let config = WakeupConfig::default().with_low_power_path(true);
    for pin in pins {
        let mut input = Input::new(pin, InputConfig::default().with_pull(Pull::Up));
        input.listen(Event::LowLevel);
        input
            .apply_wakeup_config(&config)
            .expect("wake pin is not an RTC pin");
    }
    // Owned by this sleep, which does not return
    let mut low_power = LowPower::new(unsafe { LPWR::steal() });
    match seconds {
        Some(secs) => low_power.set_wakeup_deadline(Instant::now() + Duration::from_secs(secs)),
        None => low_power.clear_wakeup_deadline(),
    }
    // Let serial output flush
    Delay::new().delay_millis(100);
    low_power.sleep_deep(RtcSleepConfig::deep())
}

/// Deep sleep until KEY is pressed, with no other wake source
pub fn shutdown() -> ! {
    deep_sleep([key_pin()], None)
}
