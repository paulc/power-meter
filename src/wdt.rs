use esp_hal::{
    peripherals::TIMG0,
    timer::timg::{MwdtStage, Wdt},
};

use embassy_time::{Duration, Timer};

#[embassy_executor::task]
pub async fn wdt_task(wdt: &'static mut Wdt<TIMG0<'static>>) {
    wdt.set_timeout(
        MwdtStage::Stage0,
        esp_hal::time::Duration::from_millis(5_000),
    );
    wdt.enable();
    let mut counter = 0_u32;
    loop {
        wdt.feed();
        if counter.is_multiple_of(10) {
            let (used, free) = {
                let used = esp_alloc::HEAP.used();
                let free = esp_alloc::HEAP.free();
                (used, free)
            };
            defmt::info!("Heap: used={} free={} total={}", used, free, used + free);
        }
        Timer::after(Duration::from_secs(1)).await;
        counter += 1;
    }
}
