mod secrets;
mod ws2812;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::info;
use smart_leds_trait::{SmartLedsWrite, RGB8};
use ws2812::Ws2812;

const TCP_PORT: u16 = 1234;

fn set_color(led: &mut Ws2812, r: u8, g: u8, b: u8) {
    led.write(std::iter::once(RGB8 { r, g, b })).unwrap();
}

fn handle_command(led: &mut Ws2812, cmd: &str) {
    let cmd = cmd.trim();
    info!("cmd: {cmd}");
    match cmd {
        "red"   => set_color(led, 255, 0, 0),
        "green" => set_color(led, 0, 255, 0),
        "blue"  => set_color(led, 0, 0, 255),
        "white" => set_color(led, 255, 255, 255),
        "off"   => set_color(led, 0, 0, 0),
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

fn handle_client(mut stream: TcpStream, led: &mut Ws2812) {
    info!("Client connected: {}", stream.peer_addr().unwrap());
    stream.write_all(b"ESP32-C6 LED controller. Commands: red, green, blue, white, off, r,g,b\n").ok();

    let mut line = String::new();
    let mut buf = [0u8; 1];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let ch = buf[0] as char;
                if ch == '\n' {
                    handle_command(led, &line);
                    stream.write_all(b"ok\n").ok();
                    line.clear();
                } else if ch != '\r' {
                    line.push(ch);
                }
            }
        }
    }
    info!("Client disconnected");
}

fn wifi_task(modem: esp_idf_svc::hal::modem::Modem, ready: Arc<(Mutex<bool>, Condvar)>) {
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
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();
    let mut led = Ws2812::new(peripherals.pins.gpio8).unwrap();

    let ready = Arc::new((Mutex::new(false), Condvar::new()));
    let ready_wifi = Arc::clone(&ready);
    std::thread::spawn(move || wifi_task(peripherals.modem, ready_wifi));

    // Wait for first WiFi connection before binding
    let (lock, cvar) = &*ready;
    drop(cvar.wait_while(lock.lock().unwrap(), |connected| !*connected).unwrap());

    let listener = TcpListener::bind(format!("0.0.0.0:{TCP_PORT}")).unwrap();
    info!("TCP server on port {TCP_PORT}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => handle_client(stream, &mut led),
            Err(e) => info!("Accept error: {e}"),
        }
    }
}
