/*
 * Software Rotary Encoder Driver
 *
 * Unless you have a good reason use encoder_pcnt (uses HW PCNT peripheral)
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
const DEBOUNCE_MS: u64 = 2;

// State: clk_high << 1 | dt_high
// Index: (prev << 2) | current
// Value: 0 = no change, 1 = CW, -1 = CCW, 2 = invalid
#[rustfmt::skip]
const TRANSITION_TABLE: [i8; 16] = [
    /*    curr    00  01  10  11 */
    /* prev 00 */  0, -1,  1,  2, 
    /* prev 01 */  1,  0,  2, -1,
    /* prev 10 */ -1,  2,  0,  1, 
    /* prev 11 */  2,  1, -1,  0,
];

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

    let mut prev_state: u8 = (enc_clk.is_high() as u8) << 1 | enc_dt.is_high() as u8;

    loop {
        match select3(
            enc_clk.wait_for_any_edge(),
            enc_dt.wait_for_any_edge(),
            enc_sw.wait_for_falling_edge(),
        )
        .await
        {
            Either3::First(_) | Either3::Second(_) => {
                let curr_state = (enc_clk.is_high() as u8) << 1 | enc_dt.is_high() as u8;
                let delta = TRANSITION_TABLE[((prev_state << 2) | curr_state) as usize];
                if delta != 2 {
                    counter += delta as i16;
                }
                prev_state = curr_state;
            }
            Either3::Third(_) => {
                // Debounce input
                Timer::after_millis(DEBOUNCE_MS).await;
                if enc_sw.is_low() {
                    tx.send(EncoderMsg::Button).await;
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
                tx.send(EncoderMsg::Increment).await;
            } else {
                tx.send(EncoderMsg::Decrement).await;
            }
            prev = counter;
        }
    }
}
