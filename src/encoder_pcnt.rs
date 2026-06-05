use esp_hal::gpio::{AnyPin, Input, InputConfig, Pull};
use esp_hal::pcnt::{
    channel::{CtrlMode, EdgeMode},
    unit::{Counter, Unit},
    Pcnt,
};

use core::cell::RefCell;
use critical_section::Mutex;
use static_cell::StaticCell;

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_sync::signal::Signal;
use embassy_time::Timer;

use panic_rtt_target as _;

static ENCODER_SIGNAL: Signal<CriticalSectionRawMutex, EncoderSignal> = Signal::new();
static PCNT_UNIT: Mutex<RefCell<Option<Unit<'static, 1>>>> = Mutex::new(RefCell::new(None));

const CHANNEL_LENGTH: usize = 4;
const DEBOUNCE_MS: u64 = 2;

#[derive(Clone, Debug)]
enum EncoderSignal {
    Increment,
    Decrement,
}

#[derive(Clone, Debug)]
pub enum EncoderMsg {
    Button,
    Increment,
    Decrement,
}

pub async fn encoder_init(
    spawner: Spawner,
    pcnt: esp_hal::peripherals::PCNT<'static>,
    clk: AnyPin<'static>,
    dt: AnyPin<'static>,
    sw: AnyPin<'static>,
) -> Receiver<'static, NoopRawMutex, EncoderMsg, CHANNEL_LENGTH> {
    static ENCODER_CHANNEL: StaticCell<Channel<NoopRawMutex, EncoderMsg, CHANNEL_LENGTH>> =
        StaticCell::new();
    let encoder_chan = ENCODER_CHANNEL.init(Channel::new());

    // Configure PCNT device (fixed to use UNIT1 in X4 mode)
    let mut pcnt = Pcnt::new(pcnt);
    pcnt.set_interrupt_handler(pcnt_interrupt);

    let pcnt_unit = pcnt.unit1;
    // Max glitch filter
    pcnt_unit.set_filter(Some(1023u16)).unwrap();
    // X4 encoder - interrupt at +/-4 for each click
    pcnt_unit.set_low_limit(Some(-4)).unwrap();
    pcnt_unit.set_high_limit(Some(4)).unwrap();
    pcnt_unit.clear();

    // Configure clk/dt as inputs
    let config = InputConfig::default().with_pull(Pull::Up);
    let input_clk = Input::new(clk, config).peripheral_input();
    let input_dt = Input::new(dt, config).peripheral_input();

    let ch0 = &pcnt_unit.channel0;
    ch0.set_edge_signal(input_clk.clone());
    ch0.set_ctrl_signal(input_dt.clone());
    ch0.set_input_mode(EdgeMode::Increment, EdgeMode::Decrement);
    ch0.set_ctrl_mode(CtrlMode::Reverse, CtrlMode::Keep);

    let ch1 = &pcnt_unit.channel1;
    ch1.set_edge_signal(input_dt);
    ch1.set_ctrl_signal(input_clk);
    ch1.set_input_mode(EdgeMode::Decrement, EdgeMode::Increment);
    ch1.set_ctrl_mode(CtrlMode::Reverse, CtrlMode::Keep);

    pcnt_unit.listen();
    pcnt_unit.resume();

    static COUNTER: StaticCell<Counter<'static, 1>> = StaticCell::new();
    let counter = COUNTER.init(pcnt_unit.counter.clone());

    Timer::after_millis(100).await;
    pcnt_unit.clear();

    critical_section::with(|cs| PCNT_UNIT.borrow_ref_mut(cs).replace(pcnt_unit));

    // Start encoder task
    spawner.spawn(encoder_task(counter, sw, encoder_chan.sender()).expect("encoder_task"));

    // Return channel receiver
    encoder_chan.receiver()
}

#[embassy_executor::task]
async fn encoder_task(
    counter: &'static mut Counter<'static, 1>,
    sw: AnyPin<'static>,
    tx: Sender<'static, NoopRawMutex, EncoderMsg, CHANNEL_LENGTH>,
) {
    let mut enc_sw = Input::new(sw, InputConfig::default().with_pull(Pull::Up));
    loop {
        match select(ENCODER_SIGNAL.wait(), enc_sw.wait_for_falling_edge()).await {
            Either::First(signal) => match signal {
                EncoderSignal::Increment => tx.send(EncoderMsg::Increment).await,
                EncoderSignal::Decrement => tx.send(EncoderMsg::Decrement).await,
            },
            Either::Second(_) => {
                // Debounce input
                Timer::after_millis(DEBOUNCE_MS).await;
                if enc_sw.is_low() {
                    tx.send(EncoderMsg::Button).await;
                }
            }
        }
        /*
        // Counter should always be 0 when waiting for input
        defmt::info!(">> PCNT: {}", counter.get());
        if counter.get() != 0_i16 {
            defmt::info!(">> PCNT RESET: {}", counter.get());
            critical_section::with(|cs| {
                PCNT_UNIT.borrow_ref_mut(cs).as_mut().map(|u| u.clear());
            });
        }
        */
    }
}

#[esp_hal::handler]
fn pcnt_interrupt() {
    critical_section::with(|cs| {
        let mut u = PCNT_UNIT.borrow_ref_mut(cs);
        if let Some(u) = u.as_mut() {
            if u.interrupt_is_set() {
                let events = u.events();
                if events.high_limit {
                    ENCODER_SIGNAL.signal(EncoderSignal::Increment);
                    // defmt::info!(">> PCNT_INTERRUPT: Increment");
                } else if events.low_limit {
                    ENCODER_SIGNAL.signal(EncoderSignal::Decrement);
                    // defmt::info!(">> PCNT_INTERRUPT: Decrement");
                }
                u.reset_interrupt();
            }
        }
    });
}
