mod secrets;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::info;
use smart_leds_trait::{SmartLedsWrite, RGB8};
use ws2812_esp32_rmt_driver::driver::color::LedPixelColorRgb24;
use ws2812_esp32_rmt_driver::LedPixelEsp32Rmt;
type Ws2812 = LedPixelEsp32Rmt<'static, RGB8, LedPixelColorRgb24>;

const TCP_PORT: u16 = 1234;
const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const BLINK_COLORS: &[(u8, u8, u8)] = &[
    (255, 0,   0  ), // red
    (0,   255, 0  ), // green
    (0,   0,   255), // blue
    (255, 255, 255), // white
    (0,   0,   0  ), // off
];

const BRIGHTNESS: u8 = 64; // 25% of 255

fn set_color(led: &mut Ws2812, r: u8, g: u8, b: u8) {
    let scale = |c: u8| ((c as u16 * BRIGHTNESS as u16) / 255) as u8;
    led.write(std::iter::once(RGB8 { r: scale(r), g: scale(g), b: scale(b) })).unwrap();
}

fn handle_command(led: &mut Ws2812, reconnect: &AtomicBool, cmd: &str) {
    let cmd = cmd.trim();
    info!("cmd: {cmd}");
    match cmd {
        "red"       => set_color(led, 255, 0, 0),
        "green"     => set_color(led, 0, 255, 0),
        "blue"      => set_color(led, 0, 0, 255),
        "white"     => set_color(led, 255, 255, 255),
        "off"       => set_color(led, 0, 0, 0),
        "reconnect" => reconnect.store(true, Ordering::Relaxed),
        _ => {
            let parts: Vec<&str> = cmd.splitn(3, ',').collect();
            if parts.len() == 3 {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    parts[0].parse::<u8>(),
                    parts[1].parse::<u8>(),
                    parts[2].parse::<u8>(),
                ) {
                    set_color(led, r, g, b);
                }
            }
        }
    }
}

fn handle_client(mut stream: TcpStream, led: &mut Ws2812, reconnect: &AtomicBool) {
    info!("Client connected: {}", stream.peer_addr().unwrap());
    stream.write_all(b"ESP32-C6 LED controller. Commands: red, green, blue, white, off, r,g,b, reconnect\n").ok();
    stream.set_read_timeout(Some(BLINK_INTERVAL)).ok();

    let mut line = String::new();
    let mut buf = [0u8; 1];
    let mut blink_state = 0usize;
    let mut last_blink = Instant::now();

    loop {
        if last_blink.elapsed() >= BLINK_INTERVAL {
            // no command received recently, keep blinking
        }

        match stream.read(&mut buf) {
            Ok(0) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
            Ok(_) => {
                let ch = buf[0] as char;
                if ch == '\n' {
                    handle_command(led, reconnect, &line);
                    stream.write_all(b"ok\n").ok();
                    line.clear();
                    // Stop blinking — command sets the color
                    last_blink = Instant::now() + Duration::from_secs(3600);
                    continue;
                } else if ch != '\r' {
                    line.push(ch);
                }
                continue;
            }
        }

        // Tick blink
        if last_blink.elapsed() >= BLINK_INTERVAL {
            let (r, g, b) = BLINK_COLORS[blink_state % BLINK_COLORS.len()];
            set_color(led, r, g, b);
            blink_state += 1;
            last_blink = Instant::now();
        }
    }
    info!("Client disconnected");
}

fn wifi_task(modem: esp_idf_svc::hal::modem::Modem, ready: Arc<(Mutex<bool>, Condvar)>, reconnect: Arc<AtomicBool>) {
    let sys_loop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(modem, sys_loop.clone(), Some(nvs)).unwrap(),
        sys_loop,
    ).unwrap();

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: secrets::SSID.try_into().unwrap(),
        password: secrets::PASSWORD.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    })).unwrap();

    wifi.start().unwrap();

    loop {
        if reconnect.swap(false, Ordering::Relaxed) {
            info!("WiFi reconnect requested, disconnecting...");
            wifi.disconnect().ok();
        }
        if !wifi.is_connected().unwrap_or(false) {
            info!("WiFi connecting...");
            match wifi.connect() {
                Ok(_) => {
                    wifi.wait_netif_up().unwrap();
                    let ip = wifi.wifi().sta_netif().get_ip_info().unwrap().ip;
                    info!("WiFi connected, IP: {ip}");
                    let (lock, cvar) = &*ready;
                    *lock.lock().unwrap() = true;
                    cvar.notify_one();
                }
                Err(e) => {
                    info!("WiFi connect failed: {e:?}, retrying...");
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();
    let mut led = Ws2812::new(peripherals.pins.gpio8).unwrap();

    let reconnect = Arc::new(AtomicBool::new(false));
    let ready = Arc::new((Mutex::new(false), Condvar::new()));
    let ready_wifi = Arc::clone(&ready);
    let reconnect_wifi = Arc::clone(&reconnect);
    std::thread::spawn(move || wifi_task(peripherals.modem, ready_wifi, reconnect_wifi));

    // Blink while waiting for WiFi
    let mut blink_state = 0usize;
    let (lock, cvar) = &*ready;
    loop {
        {
            let guard = lock.lock().unwrap();
            let result = cvar.wait_timeout(guard, BLINK_INTERVAL).unwrap();
            if *result.0 { break; }
        }
        let (r, g, b) = BLINK_COLORS[blink_state % BLINK_COLORS.len()];
        set_color(&mut led, r, g, b);
        blink_state += 1;
    }

    let listener = TcpListener::bind(format!("0.0.0.0:{TCP_PORT}")).unwrap();
    info!("TCP server on port {TCP_PORT}");

    // Blink while waiting for a client connection
    listener.set_nonblocking(true).unwrap();
    let mut last_blink = Instant::now();

    loop {
        if last_blink.elapsed() >= BLINK_INTERVAL {
            let (r, g, b) = BLINK_COLORS[blink_state % BLINK_COLORS.len()];
            set_color(&mut led, r, g, b);
            blink_state += 1;
            last_blink = Instant::now();
        }

        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                handle_client(stream, &mut led, &reconnect);
                // Resume blinking after client disconnects
                last_blink = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => info!("Accept error: {e}"),
        }
    }
}
