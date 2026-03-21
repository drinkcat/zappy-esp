mod ws2812;

use esp_idf_svc::hal::peripherals::Peripherals;
use smart_leds_trait::{SmartLedsWrite, RGB8};
use std::thread;
use std::time::Duration;
use ws2812::Ws2812;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();
    let mut led = Ws2812::new(peripherals.pins.gpio8).unwrap();

    let colors = [RGB8 { r: 25, g: 0, b: 0 }, RGB8::default()];

    loop {
        for &color in &colors {
            led.write(std::iter::once(color)).unwrap();
            thread::sleep(Duration::from_millis(500));
        }
    }
}
