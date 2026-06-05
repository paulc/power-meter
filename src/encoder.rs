/*
 * This doesnt work - use encoder_pcnt (uses HW PCNT peripheral)
 */

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_time::Timer;
use esp_hal::gpio::{AnyPin, Input, InputConfig, Level, Pull};
use esp_sync::RawMutex;
use static_cell::StaticCell;

const CHANNEL_LENGTH: usize = 4;

#[derive(Clone, Debug)]
pub enum EncoderMsg {
    Button,
    Increment,
    Decrement,
}

pub fn encoder_init(
    spawner: Spawner,
    clk: AnyPin<'static>,
    dt: AnyPin<'static>,
    sw: AnyPin<'static>,
) -> Receiver<'static, RawMutex, EncoderMsg, CHANNEL_LENGTH> {
    static ENCODER_CHANNEL: StaticCell<Channel<RawMutex, EncoderMsg, CHANNEL_LENGTH>> =
        StaticCell::new();
    let encoder_chan = ENCODER_CHANNEL.init(Channel::new());
    spawner.spawn(encoder_task(clk, dt, sw, encoder_chan.sender()).expect("encoder_task"));
    encoder_chan.receiver()
}

const DEBOUNCE_MS: u64 = 2;

#[embassy_executor::task]
async fn encoder_task(
    clk: AnyPin<'static>,
    dt: AnyPin<'static>,
    sw: AnyPin<'static>,
    encoder_tx: Sender<'static, RawMutex, EncoderMsg, CHANNEL_LENGTH>,
) {
    let mut enc_sw = Input::new(sw, InputConfig::default().with_pull(Pull::Up));
    let mut enc_clk = Input::new(clk, InputConfig::default().with_pull(Pull::Up));
    let enc_dt = Input::new(dt, InputConfig::default().with_pull(Pull::Up));

    loop {
        match select(
            enc_clk.wait_for_falling_edge(),
            enc_sw.wait_for_falling_edge(),
        )
        .await
        {
            Either::First(_) => {
                // Wait for input to stabilise
                defmt::info!(">> CLK EDGE");
                let mut state = [Level::Low; 20];
                let mut i = 0_usize;
                // Fill state table
                for s in state.iter_mut() {
                    *s = enc_clk.level();
                    Timer::after_micros(50).await;
                }
                // Wait for input to stabilise
                while !state.iter().all(|&s| s == state[0]) {
                    defmt::info!(">> State: {:?}", state);
                    state[i.rem_euclid(state.len())] = enc_clk.level();
                    i += 1;
                    Timer::after_micros(50).await;
                }
                match (enc_clk.is_high(), enc_dt.is_high()) {
                    (true, false) | (false, true) => encoder_tx.send(EncoderMsg::Increment).await,
                    (true, true) | (false, false) => encoder_tx.send(EncoderMsg::Decrement).await,
                };
            }
            Either::Second(_) => {
                // Debounce input
                Timer::after_millis(DEBOUNCE_MS).await;
                if enc_sw.is_low() {
                    encoder_tx.send(EncoderMsg::Button).await;
                }
            }
        }
    }
}
