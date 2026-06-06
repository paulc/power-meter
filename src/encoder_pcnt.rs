use esp_hal::gpio::{AnyPin, Input, InputConfig, Pull};
use esp_hal::pcnt::{
    channel::{CtrlMode, EdgeMode},
    unit::Unit,
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

// Per-unit statics
static UNIT0: Mutex<RefCell<Option<Unit<'static, 0>>>> = Mutex::new(RefCell::new(None));
static SIGNAL0: Signal<CriticalSectionRawMutex, EncoderMsg> = Signal::new();
static CHANNEL0: StaticCell<Channel<NoopRawMutex, EncoderMsg, CHANNEL_LENGTH>> = StaticCell::new();

static UNIT1: Mutex<RefCell<Option<Unit<'static, 1>>>> = Mutex::new(RefCell::new(None));
static SIGNAL1: Signal<CriticalSectionRawMutex, EncoderMsg> = Signal::new();
static CHANNEL1: StaticCell<Channel<NoopRawMutex, EncoderMsg, CHANNEL_LENGTH>> = StaticCell::new();

const CHANNEL_LENGTH: usize = 4;
const DEBOUNCE_MS: u64 = 2;

#[derive(Clone, Debug)]
pub enum EncoderMsg {
    Button,
    Increment,
    Decrement,
}

pub struct EncoderUnit<const N: usize> {
    pub unit: Unit<'static, N>,
    pub unit_cell: &'static Mutex<RefCell<Option<Unit<'static, N>>>>,
    pub signal: &'static Signal<CriticalSectionRawMutex, EncoderMsg>,
    pub channel: &'static StaticCell<Channel<NoopRawMutex, EncoderMsg, CHANNEL_LENGTH>>,
}

pub fn encoder_module_init(
    pcnt: esp_hal::peripherals::PCNT<'static>,
) -> (EncoderUnit<0>, EncoderUnit<1>) {
    let mut pcnt = Pcnt::new(pcnt);
    pcnt.set_interrupt_handler(pcnt_interrupt);
    (
        EncoderUnit {
            unit: pcnt.unit0,
            unit_cell: &UNIT0,
            signal: &SIGNAL0,
            channel: &CHANNEL0,
        },
        EncoderUnit {
            unit: pcnt.unit1,
            unit_cell: &UNIT1,
            signal: &SIGNAL1,
            channel: &CHANNEL1,
        },
    )
}

impl<const N: usize> EncoderUnit<N> {
    pub async fn init(
        self,
        spawner: Spawner,
        clk: AnyPin<'static>,
        dt: AnyPin<'static>,
        sw: AnyPin<'static>,
    ) -> Receiver<'static, NoopRawMutex, EncoderMsg, CHANNEL_LENGTH> {
        // Create channel
        let encoder_chan = self.channel.init(Channel::new());

        // Configure Unit

        // Glich filter
        self.unit.set_filter(Some(1023u16)).unwrap();

        // X4 encoder - interrupt at +/-4 for each click
        self.unit.set_low_limit(Some(-4)).unwrap();
        self.unit.set_high_limit(Some(4)).unwrap();
        // Need to clear after setting limits to avoid invalid interrupts
        self.unit.clear();

        // Configure clk/dt as inputs
        let config = InputConfig::default().with_pull(Pull::Up);
        let input_clk = Input::new(clk, config).peripheral_input();
        let input_dt = Input::new(dt, config).peripheral_input();

        // Set clk edge trigger
        let ch0 = &self.unit.channel0;
        ch0.set_edge_signal(input_clk.clone());
        ch0.set_ctrl_signal(input_dt.clone());
        ch0.set_input_mode(EdgeMode::Increment, EdgeMode::Decrement);
        ch0.set_ctrl_mode(CtrlMode::Reverse, CtrlMode::Keep);

        // Set dt edge trigger
        let ch1 = &self.unit.channel1;
        ch1.set_edge_signal(input_dt);
        ch1.set_ctrl_signal(input_clk);
        ch1.set_input_mode(EdgeMode::Decrement, EdgeMode::Increment);
        ch1.set_ctrl_mode(CtrlMode::Reverse, CtrlMode::Keep);

        // Enable interrupts & restart
        self.unit.listen();
        self.unit.resume();

        Timer::after_millis(100).await;
        self.unit.clear();

        critical_section::with(|cs| self.unit_cell.borrow_ref_mut(cs).replace(self.unit));

        // Start encoder task
        spawner.spawn(encoder_task(sw, &self.signal, encoder_chan.sender()).expect("encoder_task"));

        // Return channel receiver
        encoder_chan.receiver()
    }
}

#[embassy_executor::task]
async fn encoder_task(
    sw: AnyPin<'static>,
    signal: &'static Signal<CriticalSectionRawMutex, EncoderMsg>,
    tx: Sender<'static, NoopRawMutex, EncoderMsg, CHANNEL_LENGTH>,
) {
    let mut enc_sw = Input::new(sw, InputConfig::default().with_pull(Pull::Up));
    loop {
        match select(signal.wait(), enc_sw.wait_for_falling_edge()).await {
            Either::First(msg) => tx.send(msg).await,
            Either::Second(_) => {
                // Debounce input
                Timer::after_millis(DEBOUNCE_MS).await;
                if enc_sw.is_low() {
                    tx.send(EncoderMsg::Button).await;
                }
            }
        }
    }
}

#[esp_hal::handler]
fn pcnt_interrupt() {
    critical_section::with(|cs| {
        check_unit(UNIT0.borrow_ref_mut(cs).as_mut(), &SIGNAL0);
        check_unit(UNIT1.borrow_ref_mut(cs).as_mut(), &SIGNAL1);
    });
}

fn check_unit<const N: usize>(
    unit: Option<&mut Unit<'static, N>>,
    signal: &Signal<CriticalSectionRawMutex, EncoderMsg>,
) {
    if let Some(u) = unit {
        if u.interrupt_is_set() {
            let events = u.events();
            if events.high_limit {
                signal.signal(EncoderMsg::Increment);
            } else if events.low_limit {
                signal.signal(EncoderMsg::Decrement);
            }
            u.reset_interrupt();
            u.clear();
        }
    }
}
