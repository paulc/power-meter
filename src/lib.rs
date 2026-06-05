#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

pub mod ble;
pub mod encoder;
pub mod encoder_pcnt;
pub mod ina219;
pub mod lcd;
pub mod wdt;
