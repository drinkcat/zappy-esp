mod secrets;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use embedded_svc::http::client::Client as HttpClient;
use embedded_svc::io::Write as SvcWrite;

const ZAP_BLINK_DURATION: Duration = Duration::from_secs(5);
const ZAP_BLINK_INTERVAL: Duration = Duration::from_millis(100);

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
const BRIGHTNESS: u8 = 64; // 25% of 255

struct State {
    wifi_reconnect: AtomicBool,
    wifi_ready: (Mutex<bool>, Condvar),
    zap_flag: AtomicBool,       // set by ISR, cleared by zap_task
    zap_times: Mutex<Vec<SystemTime>>,
    zap_blink_until: Mutex<Option<Instant>>,
}

impl State {
    fn new() -> Self {
        Self {
            wifi_reconnect: AtomicBool::new(false),
            wifi_ready: (Mutex::new(false), Condvar::new()),
            zap_flag: AtomicBool::new(false),
            zap_times: Mutex::new(Vec::new()),
            zap_blink_until: Mutex::new(None),
        }
    }
}

enum Response {
    Ok,
    ColorSet,
    Text(String),
}

// Returns true if currently in zap-blink mode and ticked the LED.
// Returns true while zap-blinking. Resets zap_blink_state to false when done.
fn tick_zap_blink(led: &mut Ws2812, state: &State, zap_blink_state: &mut bool) -> bool {
    let until = *state.zap_blink_until.lock().unwrap();
    match until {
        Some(t) if Instant::now() < t => {
            *zap_blink_state = !*zap_blink_state;
            if *zap_blink_state {
                set_color_raw(led, 255, 165, 0); // orange on
            } else {
                set_color_raw(led, 0, 0, 0);     // off
            }
            true
        }
        _ => {
            *zap_blink_state = false;
            false
        }
    }
}

fn set_color_raw(led: &mut Ws2812, r: u8, g: u8, b: u8) {
    let scale = |c: u8| ((c as u16 * BRIGHTNESS as u16) / 255) as u8;
    led.write(std::iter::once(RGB8 { r: scale(r), g: scale(g), b: scale(b) })).unwrap();
}

fn set_color(led: &mut Ws2812, r: u8, g: u8, b: u8) {
    set_color_raw(led, r, g, b);
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
                let ms = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis();
                format!("{ms}\n")
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
    stream.set_read_timeout(Some(ZAP_BLINK_INTERVAL)).ok();

    let mut line = String::new();
    let mut buf = [0u8; 1];
    let mut zap_blink_state = false;

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
                        Response::ColorSet => { stream.write_all(b"ok\n").ok(); }
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

        let was_blinking = zap_blink_state;
        if !tick_zap_blink(led, state, &mut zap_blink_state) && was_blinking {
            set_color(led, 0, 0, 255); // restore blue after zap
        }
    }
    info!("Client disconnected");
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
            *state.zap_blink_until.lock().unwrap() =
                Some(Instant::now() + ZAP_BLINK_DURATION);
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

    std::thread::spawn({ let s = Arc::clone(&state); move || zap_task(zap_pin, s) });
    std::thread::spawn({ let s = Arc::clone(&state); move || wifi_task(peripherals.modem, s) });

    // Blink red slowly while waiting for WiFi
    let mut red_state = false;
    let (lock, cvar) = &state.wifi_ready;
    loop {
        let guard = lock.lock().unwrap();
        let result = cvar.wait_timeout(guard, BLINK_INTERVAL).unwrap();
        if *result.0 { break; }
        drop(result);
        red_state = !red_state;
        if red_state { set_color(&mut led, 255, 0, 0); } else { set_color(&mut led, 0, 0, 0); }
    }

    set_color(&mut led, 0, 0, 255); // solid blue when WiFi ready

    std::thread::spawn(|| {
        thingsboard_send_telemetry(Some("boot"));
        loop {
            std::thread::sleep(Duration::from_secs(5 * 60));
            thingsboard_send_telemetry(None);
        }
    });

    let listener = TcpListener::bind(format!("0.0.0.0:{TCP_PORT}")).unwrap();
    info!("TCP server on port {TCP_PORT}");
    listener.set_nonblocking(true).unwrap();
    let mut zap_blink_state = false;

    loop {
        let was_blinking = zap_blink_state;
        if tick_zap_blink(&mut led, &state, &mut zap_blink_state) {
            std::thread::sleep(ZAP_BLINK_INTERVAL);
        } else if was_blinking {
            set_color(&mut led, 0, 0, 255); // restore blue after zap
        }

        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                handle_client(stream, &mut led, &state);
                set_color(&mut led, 0, 0, 255); // restore blue after client disconnects
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => info!("Accept error: {e}"),
        }
    }
}
