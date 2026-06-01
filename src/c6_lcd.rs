use esp_hal::{
    dma::{DmaRxBuf, DmaTxBuf},
    gpio::{AnyPin, Level, Output},
    spi::{
        master::{Config, Spi, SpiDmaBus},
        Mode,
    },
    time::Rate,
    Async,
};

use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};

use embedded_graphics::{mono_font::MonoTextStyle, pixelcolor::Rgb565, prelude::*, text::Text};

use lcd_async::{
    interface::SpiInterface,
    models::ST7789,
    options::{ColorInversion, ColorOrder, Orientation, Rotation},
    raw_framebuf::RawFrameBuf,
    Builder, NoResetPin, TestImage,
};

use static_cell::StaticCell;

use defmt::info;

// Display parameters
// const DISPLAY_WIDTH: u16 = 172;
// const DISPLAY_HEIGHT: u16 = 320;
const DISPLAY_WIDTH: u16 = 320;
const DISPLAY_HEIGHT: u16 = 172;
const PIXEL_SIZE: usize = 2; // RGB565 = 2 bytes per pixel
const FRAME_SIZE: usize = (DISPLAY_WIDTH as usize) * (DISPLAY_HEIGHT as usize) * PIXEL_SIZE;

#[derive(Debug, Clone)]
pub enum LcdError {
    DisplayInit,
    TaskInit,
}

pub type LcdSender = Sender<'static, NoopRawMutex, (LcdMessage, bool), 1>;

pub struct LcdPins {
    pub dc: AnyPin<'static>,   // DC (Data/Command)
    pub cs: AnyPin<'static>,   // CS (Chip Select)
    pub sclk: AnyPin<'static>, // CLK
    pub mosi: AnyPin<'static>, // DIN
    pub res: AnyPin<'static>,  // RES (Reset)
    pub bl: AnyPin<'static>,   // Backlight
}

// Init C6 display
// (Note - peripherals are fixed to make ownership easier)
#[allow(clippy::too_many_arguments)]
pub async fn init_lcd(
    pins: LcdPins,
    spi_dev: esp_hal::peripherals::SPI2<'static>, // SPI Device
    dma_ch: esp_hal::peripherals::DMA_CH0<'static>, // DMA device
    spawner: embassy_executor::Spawner,
) -> Result<LcdSender, LcdError> {
    // Destructure pins
    let LcdPins {
        dc,
        cs,
        sclk,
        mosi,
        res,
        bl,
    } = pins;

    // Create DMA buffers for SPI
    #[allow(clippy::manual_div_ceil)]
    let (rx_buffer, rx_descriptors, tx_buffer, tx_descriptors) = esp_hal::dma_buffers!(64, 8000);
    let dma_rx_buf = defmt::unwrap!(DmaRxBuf::new(rx_descriptors, rx_buffer));
    let dma_tx_buf = defmt::unwrap!(DmaTxBuf::new(tx_descriptors, tx_buffer));

    // Create SPI with DMA
    let spi = defmt::unwrap!(Spi::new(
        spi_dev,
        Config::default()
            .with_frequency(Rate::from_mhz(80))
            .with_mode(Mode::_0),
    ))
    .with_sck(sclk)
    .with_mosi(mosi)
    .with_dma(dma_ch)
    .with_buffers(dma_rx_buf, dma_tx_buf)
    .into_async();

    // Create control pins
    let res = Output::new(res, Level::Low, Default::default());
    let dc = Output::new(dc, Level::Low, Default::default());
    let cs = Output::new(cs, Level::High, Default::default());

    // Turn on backlight
    let _bl = Output::new(bl, Level::High, Default::default());

    // Create shared SPI bus
    static SPI_BUS: StaticCell<Mutex<NoopRawMutex, SpiDmaBus<'static, Async>>> = StaticCell::new();
    let spi_bus = Mutex::new(spi);
    let spi_bus = SPI_BUS.init(spi_bus);
    let spi_device = SpiDevice::new(spi_bus, cs);

    // Create display interface
    let di = SpiInterface::new(spi_device, dc);
    let mut delay = embassy_time::Delay;

    // Initialize the display
    let display = Builder::new(ST7789, di)
        .reset_pin(res)
        .orientation(Orientation::default().rotate(Rotation::Deg270))
        .color_order(ColorOrder::Rgb)
        .invert_colors(ColorInversion::Inverted)
        .display_size(DISPLAY_HEIGHT, DISPLAY_WIDTH) // XXX Inverted??
        .display_offset(34, 0)
        .init(&mut delay)
        .await
        .map_err(|_| LcdError::DisplayInit)?;

    info!("Display initialized!");

    static LCD_CHANNEL: StaticCell<Channel<NoopRawMutex, (LcdMessage, bool), 1>> =
        StaticCell::new();
    static LCD_CHANNEL_RX: StaticCell<Receiver<NoopRawMutex, (LcdMessage, bool), 1>> =
        StaticCell::new();

    let lcd_channel = LCD_CHANNEL.init(Channel::new());
    let lcd_rx = LCD_CHANNEL_RX.init(lcd_channel.receiver());
    let lcd_tx = lcd_channel.sender();

    // Create LCD task
    spawner.spawn(lcd_task(display, lcd_rx).map_err(|_| LcdError::TaskInit)?);

    Ok(lcd_tx)
}

#[allow(unused)]
#[derive(Debug, Clone)]
pub enum LcdMessage {
    TestImage,
    Background(Rgb565),
    Draw,
    Text(heapless::String<40>, Point, u8, Rgb565), // Text, (x,y), font_size, colour
    Static(&'static str, Point, u8, Rgb565),       // Text, (x,y), font_size, colour
    Scroll(heapless::String<40>),
}

const FONT_HEIGHT: u16 = 16;
const DISPLAY_LINES: usize = (DISPLAY_HEIGHT / FONT_HEIGHT) as usize;

type LcdSpiDevice = SpiDevice<'static, NoopRawMutex, SpiDmaBus<'static, Async>, Output<'static>>;
type LcdDisplay =
    lcd_async::Display<SpiInterface<LcdSpiDevice, Output<'static>>, ST7789, Output<'static>>;

#[embassy_executor::task]
async fn lcd_task(
    mut display: LcdDisplay,
    lcd_rx: &'static mut Receiver<'static, NoopRawMutex, (LcdMessage, bool), 1>,
) {
    // Initialize frame buffer
    static FRAME_BUFFER: StaticCell<[u8; FRAME_SIZE]> = StaticCell::new();
    let frame_buffer = FRAME_BUFFER.init_with(|| [0; FRAME_SIZE]);
    let mut lines = heapless::Deque::<heapless::String<40>, DISPLAY_LINES>::new();
    loop {
        let mut raw_fb = RawFrameBuf::<Rgb565, _>::new(
            frame_buffer.as_mut_slice(),
            DISPLAY_WIDTH.into(),
            DISPLAY_HEIGHT.into(),
        );
        let (msg, update) = lcd_rx.receive().await;
        match msg {
            LcdMessage::Draw => {} // Empty command to allow update
            LcdMessage::TestImage => {
                defmt::unwrap!(TestImage::new().draw(&mut raw_fb), "draw test-image");
            }
            LcdMessage::Background(c) => {
                defmt::unwrap!(raw_fb.clear(c), "draw background");
            }
            LcdMessage::Text(t, p, s, c) => {
                let style = match s {
                    7 => MonoTextStyle::new(&profont::PROFONT_7_POINT, c),
                    9 => MonoTextStyle::new(&profont::PROFONT_9_POINT, c),
                    12 => MonoTextStyle::new(&profont::PROFONT_12_POINT, c),
                    14 => MonoTextStyle::new(&profont::PROFONT_14_POINT, c),
                    18 => MonoTextStyle::new(&profont::PROFONT_18_POINT, c),
                    24 => MonoTextStyle::new(&profont::PROFONT_24_POINT, c),
                    _ => MonoTextStyle::new(&profont::PROFONT_14_POINT, c), // Default
                };
                defmt::unwrap!(
                    Text::new(t.as_str(), p, style).draw(&mut raw_fb),
                    "draw text"
                );
            }
            LcdMessage::Static(t, p, s, c) => {
                let style = match s {
                    7 => MonoTextStyle::new(&profont::PROFONT_7_POINT, c),
                    9 => MonoTextStyle::new(&profont::PROFONT_9_POINT, c),
                    12 => MonoTextStyle::new(&profont::PROFONT_12_POINT, c),
                    14 => MonoTextStyle::new(&profont::PROFONT_14_POINT, c),
                    18 => MonoTextStyle::new(&profont::PROFONT_18_POINT, c),
                    24 => MonoTextStyle::new(&profont::PROFONT_24_POINT, c),
                    _ => MonoTextStyle::new(&profont::PROFONT_14_POINT, c), // Default
                };
                defmt::unwrap!(Text::new(t, p, style).draw(&mut raw_fb), "draw static");
            }
            LcdMessage::Scroll(t) => {
                raw_fb.clear(Rgb565::BLUE).ok();
                let style = MonoTextStyle::new(&profont::PROFONT_14_POINT, Rgb565::WHITE);
                if lines.is_full() {
                    defmt::unwrap!(lines.pop_front(), "lines.pop_back");
                }
                defmt::unwrap!(lines.push_back(t), "lines.push_front");

                for (i, l) in lines.iter().enumerate() {
                    defmt::unwrap!(
                        Text::new(
                            l.as_str(),
                            Point::new(10, (FONT_HEIGHT * (i + 1) as u16) as i32),
                            style,
                        )
                        .draw(&mut raw_fb),
                        "draw scroll"
                    );
                }
            }
        }
        if update {
            defmt::unwrap!(
                display
                    .show_raw_data(0, 0, DISPLAY_WIDTH, DISPLAY_HEIGHT, frame_buffer)
                    .await,
                "display.show_raw_data"
            )
        }
    }
}
