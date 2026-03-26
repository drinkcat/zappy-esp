mod secrets;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{InterruptType, PinDriver, Pull};
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

struct State {
    wifi_reconnect: AtomicBool,
    wifi_ready: (Mutex<bool>, Condvar),
    zap_flag: AtomicBool,       // set by ISR, cleared by zap_task
    zap_times: Mutex<Vec<SystemTime>>,
}

impl State {
    fn new() -> Self {
        Self {
            wifi_reconnect: AtomicBool::new(false),
            wifi_ready: (Mutex::new(false), Condvar::new()),
            zap_flag: AtomicBool::new(false),
            zap_times: Mutex::new(Vec::new()),
        }
    }
}

enum Response {
    Ok,
    ColorSet,
    Text(String),
}

fn set_color(led: &mut Ws2812, r: u8, g: u8, b: u8) {
    let scale = |c: u8| ((c as u16 * BRIGHTNESS as u16) / 255) as u8;
    led.write(std::iter::once(RGB8 { r: scale(r), g: scale(g), b: scale(b) })).unwrap();
}

fn handle_command(led: &mut Ws2812, state: &State, cmd: &str) -> Response {
    let cmd = cmd.trim();
    info!("cmd: {cmd}");
    match cmd {
        "red"       => { set_color(led, 255, 0, 0);        Response::ColorSet }
        "green"     => { set_color(led, 0, 255, 0);        Response::ColorSet }
        "blue"      => { set_color(led, 0, 0, 255);        Response::ColorSet }
        "white"     => { set_color(led, 255, 255, 255);    Response::ColorSet }
        "off"       => { set_color(led, 0, 0, 0);          Response::ColorSet }
        "reconnect" => { state.wifi_reconnect.store(true, Ordering::Relaxed); Response::Ok }
        "zaps"      => {
            let times = state.zap_times.lock().unwrap();
            let n = times.len();
            let list: String = times.iter().map(|t| {
                let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                format!("{secs}\n")
            }).collect();
            Response::Text(format!("count={n}\n{list}"))
        }
        _ => {
            let parts: Vec<&str> = cmd.splitn(3, ',').collect();
            if parts.len() == 3 {
                if let (Ok(r), Ok(g), Ok(b)) = (
                    parts[0].parse::<u8>(),
                    parts[1].parse::<u8>(),
                    parts[2].parse::<u8>(),
                ) {
                    set_color(led, r, g, b);
                    return Response::ColorSet;
                }
            }
            Response::Ok
        }
    }
}

fn handle_client(mut stream: TcpStream, led: &mut Ws2812, state: &State) {
    info!("Client connected: {}", stream.peer_addr().unwrap());
    stream.write_all(b"Commands: red, green, blue, white, off, r,g,b, reconnect, zaps\n").ok();
    stream.set_read_timeout(Some(BLINK_INTERVAL)).ok();

    let mut line = String::new();
    let mut buf = [0u8; 1];
    let mut blink_state = 0usize;
    let mut last_blink = Instant::now();

    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
            Ok(_) => {
                let ch = buf[0] as char;
                if ch == '\n' {
                    match handle_command(led, state, &line) {
                        Response::ColorSet => {
                            stream.write_all(b"ok\n").ok();
                            last_blink = Instant::now() + Duration::from_secs(3600);
                        }
                        Response::Ok => { stream.write_all(b"ok\n").ok(); }
                        Response::Text(t) => { stream.write_all(t.as_bytes()).ok(); }
                    }
                    line.clear();
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

fn zap_task(state: Arc<State>) {
    loop {
        if state.zap_flag.swap(false, Ordering::Relaxed) {
            let t = SystemTime::now();
            let mut times = state.zap_times.lock().unwrap();
            let count = times.len() + 1;
            times.push(t);
            info!("Zap! count={count}");
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
    ).unwrap();

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: secrets::SSID.try_into().unwrap(),
        password: secrets::PASSWORD.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    })).unwrap();

    wifi.start().unwrap();

    loop {
        if state.wifi_reconnect.swap(false, Ordering::Relaxed) {
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
    let mut led = Ws2812::new(peripherals.pins.gpio8).unwrap();

    let state = Arc::new(State::new());

    // Zap detection — ISR sets flag, zap_task records timestamps
    let state_isr = Arc::clone(&state);
    let mut zap_pin = PinDriver::input(peripherals.pins.gpio4, Pull::Down).unwrap();
    zap_pin.set_interrupt_type(InterruptType::PosEdge).unwrap();
    unsafe {
        zap_pin.subscribe(move || {
            state_isr.zap_flag.store(true, Ordering::Relaxed);
        }).unwrap();
    }
    zap_pin.enable_interrupt().unwrap();

    std::thread::spawn({ let s = Arc::clone(&state); move || zap_task(s) });
    std::thread::spawn({ let s = Arc::clone(&state); move || wifi_task(peripherals.modem, s) });

    // Blink while waiting for WiFi
    let mut blink_state = 0usize;
    let (lock, cvar) = &state.wifi_ready;
    loop {
        let guard = lock.lock().unwrap();
        let result = cvar.wait_timeout(guard, BLINK_INTERVAL).unwrap();
        if *result.0 { break; }
        drop(result);
        let (r, g, b) = BLINK_COLORS[blink_state % BLINK_COLORS.len()];
        set_color(&mut led, r, g, b);
        blink_state += 1;
    }

    let listener = TcpListener::bind(format!("0.0.0.0:{TCP_PORT}")).unwrap();
    info!("TCP server on port {TCP_PORT}");
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
                handle_client(stream, &mut led, &state);
                last_blink = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => info!("Accept error: {e}"),
        }
    }
}
