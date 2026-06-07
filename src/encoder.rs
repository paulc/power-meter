/*
 * Software Rotary Encoder Driver
 *
 * Unless you have a good reason use encoder_pcnt (uses HW PCNT peripheral)
 *
 *
 * CLK  ------+            +-----------
 *            |            |
 *            +------------+
 *
 * DT   ------------+             +-----
 *                  |             |
 *                  +-------------+
 *           ^      ^      ^      ^
 *       11     01     00     10     11
 *              +1     +1     +1     +1
 *
 *
 * CLK  ------------+             +-----
 *                  |             |
 *                  +-------------+
 *
 * DT   ------+            +-----------
 *            |            |
 *            +------------+
 *            ^      ^      ^      ^
 *        11    10     00     01     11
 *              -1     -1     -1     -1
 *
 *  prev    next
 *  11  ->  10    -1
 *  10  ->  00    -1
 *  01  ->  11    -1
 *  00  ->  01    -1
 *  11  ->  01    +1
 *  10  ->  11    +1
 *  01  ->  00    +1
 *  00  ->  10    +1
 *
 */

use embassy_executor::Spawner;
use embassy_futures::select::{select3, Either3};
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_time::Timer;
use esp_hal::gpio::{AnyPin, Input, InputConfig, Pull};
use esp_sync::RawMutex;
use portable_atomic::{AtomicI16, Ordering};
use static_cell::StaticCell;

const CHANNEL_LENGTH: usize = 4;
const SW_DEBOUNCE_MS: u64 = 10;
const ENCODER_DEBOUNCE_MS: u64 = 1;

pub static COUNTER: AtomicI16 = AtomicI16::new(0);

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
    spawner.spawn(defmt::unwrap!(encoder_task(
        clk,
        dt,
        sw,
        encoder_chan.sender()
    )));
    encoder_chan.receiver()
}

#[inline]
fn state(a: &Input, b: &Input) -> u8 {
    ((a.is_high() as u8) << 1) | (b.is_high() as u8)
}

#[embassy_executor::task]
async fn encoder_task(
    clk: AnyPin<'static>,
    dt: AnyPin<'static>,
    sw: AnyPin<'static>,
    tx: Sender<'static, RawMutex, EncoderMsg, CHANNEL_LENGTH>,
) {
    let mut enc_clk = Input::new(clk, InputConfig::default().with_pull(Pull::Up));
    let mut enc_dt = Input::new(dt, InputConfig::default().with_pull(Pull::Up));
    let mut enc_sw = Input::new(sw, InputConfig::default().with_pull(Pull::Up));
    let mut prev: i16 = 0;
    let mut counter: i16 = 0;

    let mut prev_state = state(&enc_clk, &enc_dt);

    loop {
        match select3(
            enc_clk.wait_for_any_edge(),
            enc_dt.wait_for_any_edge(),
            enc_sw.wait_for_falling_edge(),
        )
        .await
        {
            Either3::First(_) | Either3::Second(_) => {
                Timer::after_millis(ENCODER_DEBOUNCE_MS).await;
                let current_state = state(&enc_clk, &enc_dt);
                match (prev_state, current_state) {
                    (0b11, 0b10) | (0b10, 0b00) | (0b01, 0b11) | (0b00, 0b01) => counter -= 1,
                    (0b11, 0b01) | (0b10, 0b11) | (0b01, 0b00) | (0b00, 0b10) => counter += 1,
                    _ => {} // Invalid
                };
                prev_state = current_state;
            }
            Either3::Third(_) => {
                // Debounce input
                Timer::after_millis(SW_DEBOUNCE_MS).await;
                if enc_sw.is_low() {
                    let _ = tx.try_send(EncoderMsg::Button);
                }
                // Dont check counter
                continue;
            }
        }
        // Update static counter
        COUNTER.store(counter, Ordering::Relaxed);
        // X4 encoder - signal event every 4 steps
        let delta = counter - prev;
        if delta.abs() >= 4 {
            if delta > 0 {
                let _ = tx.try_send(EncoderMsg::Increment);
            } else {
                let _ = tx.try_send(EncoderMsg::Decrement);
            }
            prev = counter;
        }
    }
}
