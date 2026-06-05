#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use esp_hal::{clock::CpuClock, gpio::Pin, timer::timg::TimerGroup};

use embassy_executor::Spawner;

use panic_rtt_target as _;

use portable_atomic::{AtomicI32, Ordering};
use static_cell::StaticCell;

use power_monitor::encoder::{encoder_init, EncoderMsg};
use power_monitor::wdt::wdt_task;

extern crate alloc;

pub static ENCODER: AtomicI32 = AtomicI32::new(0);

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.3.0
    // generator parameters: --chip esp32c6 -o unstable-hal -o alloc -o embassy -o ble-trouble -o probe-rs -o defmt -o panic-rtt-target -o stable-aarch64-apple-darwin

    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    defmt::info!("Embassy Init");

    // Watchdog
    static WDT: StaticCell<esp_hal::timer::timg::Wdt<esp_hal::peripherals::TIMG0>> =
        StaticCell::new();
    let wdt = WDT.init(timg0.wdt);

    spawner.spawn(wdt_task(wdt).expect("wdt_task"));
    defmt::info!("WDT Init");

    // Rotary encoder
    let (clk, dt, sw) = (
        peripherals.GPIO20.degrade(),
        peripherals.GPIO19.degrade(),
        peripherals.GPIO18.degrade(),
    );
    let encoder_rx = encoder_init(spawner, clk, dt, sw);
    defmt::info!("ENCODER INIT");

    loop {
        let msg = encoder_rx.receive().await;
        match msg {
            // Get encoder input
            // - turn to increment/decrement selection
            // - press to cycle setting
            EncoderMsg::Button => {
                // Get encoder_index: -1 not selected
                let encoder_index = ENCODER.load(Ordering::Relaxed);
                defmt::info!(">> BUTTON: {}", encoder_index);
            }
            EncoderMsg::Increment => {
                ENCODER.fetch_add(1, Ordering::Relaxed);
                defmt::info!(">> INCREMENT: {}", ENCODER.load(Ordering::Relaxed));
            }
            EncoderMsg::Decrement => {
                ENCODER.fetch_add(-1, Ordering::Relaxed);
                defmt::info!(">> DECREMENT: {}", ENCODER.load(Ordering::Relaxed));
            }
        }
    }
}
