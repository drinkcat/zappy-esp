// Copyright 2026 Nicolas Boichat <nicolas@boichat.ch>
// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "secrets")]
mod secrets;
#[cfg(not(feature = "secrets"))]
#[path = "secrets_placeholder.rs"]
mod secrets;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use embedded_svc::http::client::Client as HttpClient;
use embedded_svc::io::Write as SvcWrite;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};

const ZAP_BLINK_DURATION: Duration = Duration::from_secs(5);
const ZAP_BLINK_INTERVAL: Duration = Duration::from_millis(100);

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{InterruptType, Output, PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::info;

#[cfg(feature = "esp32c6_devkit")]
use smart_leds_trait::{SmartLedsWrite, RGB8};
#[cfg(feature = "esp32c6_devkit")]
use ws2812_esp32_rmt_driver::driver::color::LedPixelColorRgb24;
#[cfg(feature = "esp32c6_devkit")]
use ws2812_esp32_rmt_driver::LedPixelEsp32Rmt;

#[cfg(feature = "esp32c6_devkit")]
type Led = LedPixelEsp32Rmt<'static, RGB8, LedPixelColorRgb24>;
#[cfg(feature = "xiao_esp32c6")]
type Led = PinDriver<'static, Output>;

const BLINK_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(feature = "esp32c6_devkit")]
const BRIGHTNESS: u8 = 64; // 25% of 255

struct State {
    wifi_ready: (Mutex<bool>, Condvar),
    zap_flag: AtomicBool, // set by ISR, cleared by zap_task
    zap_times: Mutex<Vec<SystemTime>>,
    zap_blink_until: Mutex<Option<Instant>>,
}

impl State {
    fn new() -> Self {
        Self {
            wifi_ready: (Mutex::new(false), Condvar::new()),
            zap_flag: AtomicBool::new(false),
            zap_times: Mutex::new(Vec::new()),
            zap_blink_until: Mutex::new(None),
        }
    }
}

// Returns true while zap-blinking. Resets zap_blink_state to false when done.
fn tick_zap_blink(led: &mut Led, state: &State, zap_blink_state: &mut bool) -> bool {
    let until = *state.zap_blink_until.lock().unwrap();
    match until {
        Some(t) if Instant::now() < t => {
            *zap_blink_state = !*zap_blink_state;
            set_led(led, *zap_blink_state);
            true
        }
        _ => {
            *zap_blink_state = false;
            false
        }
    }
}

#[cfg(feature = "esp32c6_devkit")]
fn set_led_color(led: &mut Led, r: u8, g: u8, b: u8) {
    let scale = |c: u8| ((c as u16 * BRIGHTNESS as u16) / 255) as u8;
    led.write(std::iter::once(RGB8 {
        r: scale(r),
        g: scale(g),
        b: scale(b),
    }))
    .unwrap();
}

#[cfg(feature = "esp32c6_devkit")]
fn set_led(led: &mut Led, on: bool) {
    // zap blink color: yellow
    let (r, g, b) = if on { (255, 200, 0) } else { (0, 0, 0) };
    set_led_color(led, r, g, b);
}

#[cfg(feature = "esp32c6_devkit")]
fn set_led_wifi(led: &mut Led, on: bool) {
    // wifi blink color: blue
    let (r, g, b) = if on { (0, 0, 255) } else { (0, 0, 0) };
    set_led_color(led, r, g, b);
}

#[cfg(feature = "xiao_esp32c6")]
fn set_led(led: &mut Led, on: bool) {
    // GPIO15 LED is active-low
    if on { led.set_low() } else { led.set_high() }.unwrap();
}

#[cfg(feature = "xiao_esp32c6")]
fn set_led_wifi(led: &mut Led, on: bool) {
    set_led(led, on);
}

fn thingsboard_send_telemetry(key: Option<&str>) {
    let url = format!(
        "http://thingsboard.cloud/api/v1/{}/telemetry",
        secrets::THINGSBOARD_TOKEN
    );
    let body = key.map_or("{}".to_string(), |k| format!("{{\"{k}\":1}}"));

    let conn = EspHttpConnection::new(&HttpConfig::default());
    match conn {
        Ok(conn) => {
            let mut client = HttpClient::wrap(conn);
            let headers = [("Content-Type", "application/json")];
            match client.post(&url, &headers).and_then(|mut req| {
                req.write_all(body.as_bytes())?;
                req.submit()
            }) {
                Ok(resp) => info!("ThingsBoard telemetry sent, status={}", resp.status()),
                Err(e) => info!("ThingsBoard telemetry failed: {e:?}"),
            }
        }
        Err(e) => info!("ThingsBoard HTTP init failed: {e:?}"),
    }
}

fn zap_task(mut zap_pin: PinDriver<'static, esp_idf_svc::hal::gpio::Input>, state: Arc<State>) {
    // Wait for WiFi before attempting HTTP
    let (lock, cvar) = &state.wifi_ready;
    let guard = lock.lock().unwrap();
    drop(cvar.wait_while(guard, |ready| !*ready).unwrap());

    loop {
        if state.zap_flag.swap(false, Ordering::Relaxed) {
            let t = SystemTime::now();
            let mut times = state.zap_times.lock().unwrap();
            let count = times.len() + 1;
            times.push(t);
            info!("Zap! count={count}");
            drop(times);
            *state.zap_blink_until.lock().unwrap() = Some(Instant::now() + ZAP_BLINK_DURATION);
            // Debounce then re-arm before the HTTP call
            std::thread::sleep(Duration::from_millis(100));
            zap_pin.enable_interrupt().unwrap();
            std::thread::spawn(|| thingsboard_send_telemetry(Some("zap")));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wifi_task(modem: esp_idf_svc::hal::modem::Modem, state: Arc<State>) {
    let sys_loop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(modem, sys_loop.clone(), Some(nvs)).unwrap(),
        sys_loop,
    )
    .unwrap();

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: secrets::SSID.try_into().unwrap(),
        password: secrets::PASSWORD.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))
    .unwrap();

    wifi.start().unwrap();

    loop {
        if !wifi.is_connected().unwrap_or(false) {
            info!("WiFi connecting...");
            match wifi.connect() {
                Ok(_) => {
                    wifi.wait_netif_up().unwrap();
                    let ip = wifi.wifi().sta_netif().get_ip_info().unwrap().ip;
                    info!("WiFi connected, IP: {ip}");
                    let (lock, cvar) = &state.wifi_ready;
                    *lock.lock().unwrap() = true;
                    cvar.notify_one();
                }
                Err(e) => info!("WiFi connect failed: {e:?}, retrying..."),
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();
    #[cfg(feature = "esp32c6_devkit")]
    let mut led = LedPixelEsp32Rmt::new(peripherals.pins.gpio8).unwrap();
    #[cfg(feature = "xiao_esp32c6")]
    let mut led = PinDriver::output(peripherals.pins.gpio15.degrade_output()).unwrap();

    let state = Arc::new(State::new());

    // Zap detection — ISR sets flag, zap_task records timestamps
    let state_isr = Arc::clone(&state);
    #[cfg(feature = "xiao_esp32c6")]
    let mut zap_pin = PinDriver::input(peripherals.pins.gpio2, Pull::Floating).unwrap();
    #[cfg(feature = "esp32c6_devkit")]
    let mut zap_pin = PinDriver::input(peripherals.pins.gpio4, Pull::Floating).unwrap();
    zap_pin.set_interrupt_type(InterruptType::PosEdge).unwrap();
    unsafe {
        zap_pin
            .subscribe(move || {
                state_isr.zap_flag.store(true, Ordering::Relaxed);
            })
            .unwrap();
    }
    zap_pin.enable_interrupt().unwrap();

    std::thread::spawn({
        let s = Arc::clone(&state);
        move || zap_task(zap_pin, s)
    });
    std::thread::spawn({
        let s = Arc::clone(&state);
        move || wifi_task(peripherals.modem, s)
    });

    // Slow blink (blue/on) while waiting for WiFi
    let mut wifi_blink_state = false;
    let (lock, cvar) = &state.wifi_ready;
    loop {
        let guard = lock.lock().unwrap();
        let result = cvar.wait_timeout(guard, BLINK_INTERVAL).unwrap();
        if *result.0 {
            break;
        }
        drop(result);
        wifi_blink_state = !wifi_blink_state;
        set_led_wifi(&mut led, wifi_blink_state);
    }

    set_led(&mut led, false); // off when WiFi ready (idle)

    std::thread::spawn(|| {
        thingsboard_send_telemetry(Some("boot"));
        loop {
            std::thread::sleep(Duration::from_secs(5 * 60));
            thingsboard_send_telemetry(None);
        }
    });

    let mut zap_blink_state = false;
    loop {
        let was_blinking = zap_blink_state;
        if tick_zap_blink(&mut led, &state, &mut zap_blink_state) {
            std::thread::sleep(ZAP_BLINK_INTERVAL);
        } else if was_blinking {
            set_led(&mut led, false); // off after zap
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
