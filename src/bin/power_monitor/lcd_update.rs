use embedded_graphics::{
    pixelcolor::Rgb565,
    prelude::{Point, RgbColor},
};

use core::fmt::Write;
use portable_atomic::{AtomicI32, Ordering};

use power_monitor::ina219::*;
use power_monitor::lcd::{LcdMessage, LcdSender};

pub static ENCODER: AtomicI32 = AtomicI32::new(0);

/// Update INA219 reading and display
pub async fn update(
    lcd_tx: &mut LcdSender,
    previous: Ina219Reading,
    reading: Result<Ina219Reading, Ina219Error>,
    status: &(&str, &str, &str, &str),
) -> Ina219Reading {
    let mut next = previous;

    // Clear screen
    lcd_tx
        .send((LcdMessage::Background(Rgb565::BLUE), false))
        .await;

    // Update reading (checking for errors)
    match reading {
        Ok(r) => {
            next = r;
        }
        Err(Ina219Error::NotReady) => {
            lcd_tx
                .send((
                    LcdMessage::Static("Not Ready", Point::new(10, 105), 14, Rgb565::RED),
                    false,
                ))
                .await;
        }
        Err(Ina219Error::Overflow) => {
            lcd_tx
                .send((
                    LcdMessage::Static("Overflow", Point::new(10, 105), 14, Rgb565::RED),
                    false,
                ))
                .await;
        }
        Err(Ina219Error::I2cError) => {
            lcd_tx
                .send((
                    LcdMessage::Static("I2C Error", Point::new(10, 105), 14, Rgb565::RED),
                    false,
                ))
                .await;
        }
    }

    // Display reading
    let mut v_txt = heapless::String::<40>::new();
    let _ = write!(&mut v_txt, "{:>8.3}V", next.bus_v);
    lcd_tx
        .send((
            LcdMessage::Text(v_txt, Point::new(140, 25), 24, Rgb565::GREEN),
            false,
        ))
        .await;
    let mut i_txt = heapless::String::<40>::new();
    let _ = write!(&mut i_txt, "{:>8.3}mA", next.shunt_ma);
    lcd_tx
        .send((
            LcdMessage::Text(i_txt, Point::new(140, 55), 24, Rgb565::YELLOW),
            false,
        ))
        .await;
    let mut p_txt = heapless::String::<40>::new();
    let _ = write!(&mut p_txt, "{:>8.3}mW", next.power_mw());
    lcd_tx
        .send((
            LcdMessage::Text(p_txt, Point::new(140, 85), 24, Rgb565::WHITE),
            false,
        ))
        .await;

    // Write Labels
    for (i, label) in ["V(bus)", "I(shunt)", "Power"].iter().enumerate() {
        lcd_tx
            .send((
                LcdMessage::Static(
                    label,
                    Point::new(10, 23 + (i as i32 * 30)),
                    18,
                    Rgb565::GREEN,
                ),
                false,
            ))
            .await;
    }

    // Write Settings
    // Get encoder position to highlight selected (-1 is not selected)
    let encoder_index = ENCODER.load(Ordering::Relaxed).rem_euclid(5) - 1;
    let (brng, pga, badc, sadc) = status;
    for (i, (label, value)) in [
        ("Range", brng),
        ("PGA", pga),
        ("BADC", badc),
        ("SADC", sadc),
    ]
    .iter()
    .enumerate()
    {
        let mut status_txt = heapless::String::<40>::new();
        let _ = write!(&mut status_txt, "{:<8}: {}", label, value);
        lcd_tx
            .send((
                LcdMessage::Text(
                    status_txt,
                    Point::new(10, 124 + (i as i32 * 14)),
                    12,
                    if encoder_index == i as i32 {
                        Rgb565::RED
                    } else {
                        Rgb565::GREEN
                    },
                ),
                false,
            ))
            .await;
    }

    // Update display
    lcd_tx.send((LcdMessage::Draw, true)).await;

    // Return new reading
    next
}
