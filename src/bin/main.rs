#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::rmt::{PulseCode, Rmt};
use esp_hal::rng::Rng;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal_smartled::{SmartLedsAdapter, smart_led_buffer};
use esp_radio::wifi::{ClientConfig, ModeConfig, WifiController, WifiDevice};
use log::info;
use rgb::RGB;
use smart_leds_trait::SmartLedsWrite as _;
use static_cell::StaticCell;

extern crate alloc;
use alloc::string::ToString as _;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

type LedAdapter = SmartLedsAdapter<'static, { esp_hal_smartled::buffer_size(1) }, RGB<u8>>;
static WIFI_CONNECTED: Signal<CriticalSectionRawMutex, bool> = Signal::new();
static ZAP_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

const ZAP_BLINK_DURATION: Duration = Duration::from_secs(5);
const ZAP_BLINK_INTERVAL: Duration = Duration::from_millis(100);

#[embassy_executor::task]
async fn led_task(mut led: LedAdapter) {
    // Blink blue until WiFi connects
    let mut on = false;
    loop {
        if WIFI_CONNECTED.signaled() {
            break;
        }
        on = !on;
        let color = if on {
            RGB { r: 0, g: 0, b: 64 } // blue
        } else {
            RGB { r: 0, g: 0, b: 0 }
        };
        led.write(core::iter::once(color)).unwrap();
        Timer::after(Duration::from_millis(500)).await;
    }

    led.write(core::iter::once(RGB { r: 0, g: 0, b: 0 }))
        .unwrap();
    info!("LED off (WiFi ready)");

    loop {
        ZAP_SIGNAL.wait().await;
        // Blink yellow for ZAP_BLINK_DURATION
        let deadline = embassy_time::Instant::now() + ZAP_BLINK_DURATION;
        let mut on = false;
        while embassy_time::Instant::now() < deadline {
            on = !on;
            let color = if on {
                RGB { r: 64, g: 50, b: 0 } // yellow
            } else {
                RGB { r: 0, g: 0, b: 0 }
            };
            led.write(core::iter::once(color)).unwrap();
            Timer::after(ZAP_BLINK_INTERVAL).await;
        }
        led.write(core::iter::once(RGB { r: 0, g: 0, b: 0 }))
            .unwrap();
    }
}

#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    loop {
        info!("WiFi connecting...");
        match controller.connect_async().await {
            Ok(()) => {
                info!("WiFi connected!");
                controller
                    .wait_for_event(esp_radio::wifi::WifiEvent::StaDisconnected)
                    .await;
                info!("WiFi disconnected, reconnecting...");
            }
            Err(e) => {
                info!("WiFi connect failed: {e:?}, retrying in 5s");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.2.0

    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);

    // WS2812 LED on GPIO8 (ESP32-C6 DevKit)
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).expect("Failed to initialize RMT");
    static LED_BUFFER: StaticCell<[PulseCode; esp_hal_smartled::buffer_size(1)]> =
        StaticCell::new();
    let led_buffer = LED_BUFFER.init(smart_led_buffer!(1));
    let led: LedAdapter =
        SmartLedsAdapter::new_with_color(rmt.channel0, peripherals.GPIO8, led_buffer);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("Embassy initialized!");

    static RADIO_INIT: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio_init =
        RADIO_INIT.init(esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller"));
    let (mut wifi_controller, interfaces) =
        esp_radio::wifi::new(&*radio_init, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    wifi_controller
        .set_config(&ModeConfig::Client(
            ClientConfig::default()
                .with_ssid(WIFI_SSID.to_string())
                .with_password(WIFI_PASSWORD.to_string()),
        ))
        .expect("Failed to configure WiFi");
    wifi_controller.start().expect("Failed to start WiFi");

    let seed = {
        let rng = Rng::new();
        (rng.random() as u64) << 32 | rng.random() as u64
    };

    static STACK_RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        interfaces.sta,
        embassy_net::Config::dhcpv4(Default::default()),
        STACK_RESOURCES.init(StackResources::new()),
        seed,
    );

    // Zap detection on GPIO4 (ESP32-C6 DevKit)
    let mut zap_pin = Input::new(
        peripherals.GPIO4,
        InputConfig::default().with_pull(Pull::Down),
    );

    spawner.spawn(led_task(led)).unwrap();
    spawner.spawn(wifi_task(wifi_controller)).unwrap();
    spawner.spawn(net_task(runner)).unwrap();

    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        info!("WiFi ready, IP: {}", cfg.address);
    }
    WIFI_CONNECTED.signal(true);

    let mut zap_count: u32 = 0;
    loop {
        zap_pin.wait_for_rising_edge().await;
        zap_count += 1;
        info!("Zap! count={zap_count}");
        ZAP_SIGNAL.signal(());
        Timer::after(Duration::from_millis(100)).await; // debounce
    }
}
