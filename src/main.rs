mod secrets;

use esp_idf_svc::hal::peripherals::Peripherals;
use smart_leds_trait::{SmartLedsWrite, RGB8};
use ws2812_esp32_rmt_driver::driver::color::LedPixelColorRgb24;
use ws2812_esp32_rmt_driver::LedPixelEsp32Rmt;
type Ws2812 = LedPixelEsp32Rmt<'static, RGB8, LedPixelColorRgb24>;

fn set_color(led: &mut Ws2812, r: u8, g: u8, b: u8) {
    led.write(std::iter::once(RGB8 { r, g, b })).unwrap();
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();
    let mut led = Ws2812::new(peripherals.pins.gpio8).unwrap();

    let colors: &[(u8, u8, u8)] = &[
        (255, 255, 255), // white
        (255, 0,   0  ), // red
        (0,   255, 0  ), // green
        (0,   0,   255), // blue
        (0,   0,   0  ), // off
    ];

    loop {
        for &(r, g, b) in colors {
            set_color(&mut led, r, g, b);
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
}
