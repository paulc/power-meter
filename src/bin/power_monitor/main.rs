#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use esp_hal::{clock::CpuClock, gpio::Pin, i2c, time::Rate, timer::timg::TimerGroup, Async};
use esp_sync::RawMutex;

use embassy_executor::Spawner;
use embassy_futures::select::{select3, Either3};
use embassy_sync::{
    channel::{Channel, Sender},
    mutex::Mutex,
};
use embassy_time::{Duration, Ticker, Timer};

use panic_rtt_target as _;

use portable_atomic::Ordering;
use static_cell::StaticCell;

use power_monitor::ble::{ble_task, INA219_CONFIG, POWER_AVG, POWER_INSTANT};
use power_monitor::encoder::{encoder_init, EncoderMsg};
use power_monitor::ina219::{ina219_init, Ina219Config, Ina219Error, Ina219Reading};
use power_monitor::lcd::{lcd_init, LcdPins};
use power_monitor::wdt::wdt_task;

mod lcd_update;
use lcd_update::{update, ENCODER};

extern crate alloc;

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

    // I2C Bus
    let i2c_config = i2c::master::Config::default().with_frequency(Rate::from_khz(100));
    let scl = peripherals.GPIO3;
    let sda = peripherals.GPIO4;
    let mut i2c = i2c::master::I2c::new(peripherals.I2C0, i2c_config)
        .expect("Error initailising I2C")
        .with_scl(scl)
        .with_sda(sda)
        .into_async();

    defmt::info!("Scan I2C bus: START");
    for addr in 0..=127 {
        if i2c.write_async(addr, &[0]).await.is_ok() {
            defmt::info!("Found I2C device at address: 0x{:02x}", addr);
        }
        Timer::after_millis(5).await;
    }
    defmt::info!("Scan I2C bus: DONE");

    // Create shared I2C bus
    static I2C_BUS: StaticCell<Mutex<RawMutex, i2c::master::I2c<Async>>> = StaticCell::new();
    let i2c_bus = I2C_BUS.init(Mutex::new(i2c));

    // INA219
    let mut ina219_device = defmt::unwrap!(ina219_init(i2c_bus).await);
    INA219_CONFIG.store(ina219_device.config.0, Ordering::Relaxed);
    defmt::info!("INA219 INIT: {}", ina219_device.config.as_str());

    // LCD
    let pins = LcdPins {
        dc: peripherals.GPIO9.degrade(),   // DC (Data/Command)
        cs: peripherals.GPIO14.degrade(),  // CS (Chip Select)
        sclk: peripherals.GPIO6.degrade(), // CLK
        mosi: peripherals.GPIO7.degrade(), // DIN
        res: peripherals.GPIO8.degrade(),  // RES (Reset)
        bl: peripherals.GPIO15.degrade(),  // Backlight
    };

    let mut lcd_tx = lcd_init(
        pins,
        peripherals.SPI2,    // SPI Device
        peripherals.DMA_CH0, // DMA device
        spawner,
    )
    .await
    .expect("init_lcd");
    defmt::info!("LCD INIT");

    // Rotary encoder
    let (clk, dt, sw) = (
        peripherals.GPIO20.degrade(),
        peripherals.GPIO19.degrade(),
        peripherals.GPIO18.degrade(),
    );
    let encoder_rx = encoder_init(spawner, clk, dt, sw);
    defmt::info!("ENCODER INIT");

    // Create channel for BLE config write
    static CONFIG_CHANNEL: StaticCell<Channel<RawMutex, u16, 1>> = StaticCell::new();
    static CONFIG_TX: StaticCell<Sender<RawMutex, u16, 1>> = StaticCell::new();
    let config_channel = CONFIG_CHANNEL.init(Channel::new());
    let config_rx = config_channel.receiver();
    let config_tx = CONFIG_TX.init(config_channel.sender());

    // BLE Init
    spawner.spawn(defmt::unwrap!(ble_task(spawner, peripherals.BT, config_tx)));
    defmt::info!("BLE INIT");

    // Main loop
    let mut ticker = Ticker::every(Duration::from_millis(100));
    let mut reading = Ina219Reading::default();
    let mut avg = heapless::Deque::<Ina219Reading, 10>::new();

    loop {
        match select3(ticker.next(), encoder_rx.receive(), config_rx.receive()).await {
            Either3::First(_) => {
                // Update reading/display
                reading = update(
                    &mut lcd_tx,
                    reading.clone(),
                    ina219_device.read().await,
                    &ina219_device.config.as_str(),
                )
                .await;
                POWER_INSTANT.store(u64::from_le_bytes(reading.to_bytes()), Ordering::Relaxed);
                if avg.is_full() {
                    let (v, i) = avg
                        .iter()
                        .fold((0.0, 0.0), |a, e| (a.0 + e.bus_v, a.1 + e.shunt_ma));
                    let reading_avg = Ina219Reading {
                        bus_v: v / avg.len() as f32,
                        shunt_ma: i / avg.len() as f32,
                    };
                    POWER_AVG.store(
                        u64::from_le_bytes(reading_avg.to_bytes()),
                        Ordering::Relaxed,
                    );
                    defmt::info!(
                        "AVG: {{ \"bus_v\": {}V, \"shunt_ma\": {}mA }}",
                        reading_avg.bus_v,
                        reading_avg.shunt_ma
                    );
                    avg.clear();
                }
                defmt::unwrap!(avg.push_back(reading.clone()), "push_back"); // SAFE
            }
            // Encoder input
            Either3::Second(msg) => match msg {
                // Get encoder input
                // - turn to increment/decrement selection
                // - press to cycle setting
                EncoderMsg::Button => {
                    // Get encoder_index: -1 not selected
                    let encoder_index = ENCODER.load(Ordering::Relaxed).rem_euclid(5) - 1;
                    let config = ina219_device.config.clone();
                    defmt::unwrap!(
                        async {
                            match encoder_index {
                                0 => {
                                    // BRNG
                                    ina219_device
                                        .write_config(config.with_brng(config.get_brng().cycle()))
                                        .await?
                                }
                                1 => {
                                    // PGA
                                    ina219_device
                                        .write_config(config.with_pga(config.get_pga().cycle()))
                                        .await?
                                }
                                2 => {
                                    // BADC
                                    ina219_device
                                        .write_config(config.with_badc(config.get_badc().cycle()))
                                        .await?
                                }
                                3 => {
                                    // SADC
                                    ina219_device
                                        .write_config(config.with_sadc(config.get_sadc().cycle()))
                                        .await?
                                }
                                _ => {}
                            }
                            Ok::<(), Ina219Error>(())
                        }
                        .await
                    );
                    // Write to static
                    INA219_CONFIG.store(ina219_device.config.0, Ordering::Relaxed);
                }
                EncoderMsg::Increment => {
                    ENCODER.fetch_add(1, Ordering::Relaxed);
                }
                EncoderMsg::Decrement => {
                    ENCODER.fetch_add(-1, Ordering::Relaxed);
                }
            },
            // config_rx
            Either3::Third(config) => {
                defmt::info!("Update INA219 Config: {}", config);
                let config = Ina219Config(config);
                defmt::unwrap!(ina219_device.write_config(config).await);
                // Write to static
                INA219_CONFIG.store(ina219_device.config.0, Ordering::Relaxed);
            }
        }
    }
}
