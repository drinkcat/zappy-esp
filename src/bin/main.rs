#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
// TODO: This causes clippy issues, hard to disable in embassy tasks.
//#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_net::{Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pubsub::PubSubChannel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{ClientConfig, ModeConfig, WifiController, WifiDevice};
use log::info;
use reqwless::client::HttpClient;
use reqwless::request::{Method, RequestBuilder as _};
use static_cell::StaticCell;

#[cfg(feature = "esp32c6_devkit")]
use esp_hal::rmt::{PulseCode, Rmt};
#[cfg(feature = "esp32c6_devkit")]
use esp_hal::time::Rate;
#[cfg(feature = "esp32c6_devkit")]
use esp_hal_smartled::{SmartLedsAdapter, smart_led_buffer};
#[cfg(feature = "esp32c6_devkit")]
use rgb::RGB;
#[cfg(feature = "esp32c6_devkit")]
use smart_leds_trait::SmartLedsWrite as _;

#[cfg(feature = "xiao_esp32c6")]
use esp_hal::gpio::{Level, Output, OutputConfig};

extern crate alloc;
use alloc::format;
use alloc::string::ToString as _;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const THINGSBOARD_TOKEN: &str = env!("THINGSBOARD_TOKEN");
const THINGSBOARD_HOST: &str = "thingsboard.cloud";
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5 * 60);

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(feature = "esp32c6_devkit")]
type LedAdapter = SmartLedsAdapter<'static, { esp_hal_smartled::buffer_size(1) }, RGB<u8>>;
#[cfg(feature = "xiao_esp32c6")]
type LedAdapter = Output<'static>;

static WIFI_CONNECTED: Signal<CriticalSectionRawMutex, bool> = Signal::new();
// PubSubChannel: capacity 4, 2 subscribers (led_task + thingsboard_task), 1 publisher
// Each subscriber has an independent read pointer, so a slow LED task won't block HTTP.
static ZAP_PUBSUB: PubSubChannel<CriticalSectionRawMutex, (), 4, 2, 1> = PubSubChannel::new();

const ZAP_BLINK_DURATION: Duration = Duration::from_secs(5);
const ZAP_BLINK_INTERVAL: Duration = Duration::from_millis(100);

fn led_set(led: &mut LedAdapter, r: u8, g: u8, b: u8) {
    #[cfg(feature = "esp32c6_devkit")]
    led.write(core::iter::once(RGB { r, g, b })).unwrap();

    // Xiao GPIO15 LED is active-low; treat any non-zero as "on"
    #[cfg(feature = "xiao_esp32c6")]
    led.set_level(if r > 0 || g > 0 || b > 0 {
        Level::Low
    } else {
        Level::High
    });
}

#[embassy_executor::task]
async fn led_task(mut led: LedAdapter) {
    // Blink blue (devkit) / on (xiao) until WiFi connects
    let mut on = false;
    loop {
        if WIFI_CONNECTED.signaled() {
            break;
        }
        on = !on;
        if on {
            led_set(&mut led, 0, 0, 64); // blue / on
        } else {
            led_set(&mut led, 0, 0, 0);
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    led_set(&mut led, 0, 0, 0);
    info!("LED off (WiFi ready)");

    let mut sub = ZAP_PUBSUB.subscriber().unwrap();
    loop {
        sub.next_message_pure().await;
        // Blink yellow (devkit) / on (xiao) for ZAP_BLINK_DURATION
        let mut deadline = embassy_time::Instant::now() + ZAP_BLINK_DURATION;
        let mut on = false;
        while embassy_time::Instant::now() < deadline {
            if sub.try_next_message_pure().is_some() {
                deadline = embassy_time::Instant::now() + ZAP_BLINK_DURATION;
            }
            on = !on;
            if on {
                led_set(&mut led, 64, 50, 0); // yellow / on
            } else {
                led_set(&mut led, 0, 0, 0);
            }
            Timer::after(ZAP_BLINK_INTERVAL).await;
        }
        led_set(&mut led, 0, 0, 0);
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

#[embassy_executor::task]
async fn thingsboard_task(stack: Stack<'static>) {
    static TCP_STATE: StaticCell<TcpClientState<1, 1024, 1024>> = StaticCell::new();
    let tcp_client = TcpClient::new(stack, TCP_STATE.init(TcpClientState::new()));
    let dns = DnsSocket::new(stack);

    let url = format!("http://{THINGSBOARD_HOST}/api/v1/{THINGSBOARD_TOKEN}/telemetry");

    // Send boot telemetry
    send_telemetry(&tcp_client, &dns, &url, Some("boot")).await;

    let mut sub = ZAP_PUBSUB.subscriber().unwrap();
    loop {
        // Wait for either a zap or the keepalive timer, whichever comes first
        let next_keepalive = Timer::after(KEEPALIVE_INTERVAL);
        let key = match select(sub.next_message_pure(), next_keepalive).await {
            Either::First(_) => Some("zap"),
            Either::Second(_) => None,
        };
        send_telemetry(&tcp_client, &dns, &url, key).await;
    }
}

async fn send_telemetry(
    client: &TcpClient<'_, 1>,
    dns: &DnsSocket<'_>,
    url: &str,
    key: Option<&str>,
) {
    let body = key.map_or(b"{}".as_slice(), |k| match k {
        "boot" => b"{\"boot\":1}",
        "zap" => b"{\"zap\":1}",
        _ => b"{}",
    });

    let mut rx_buf = [0u8; 1024];
    let mut http = HttpClient::new(client, dns);
    match http.request(Method::POST, url).await {
        Ok(req) => {
            let headers = [("Content-Type", "application/json")];
            let mut req = req.headers(&headers).body(body);
            let result = req.send(&mut rx_buf).await;
            match result {
                Ok(resp) => info!(
                    "ThingsBoard telemetry sent (key={key:?}), status={:?}",
                    resp.status
                ),
                Err(e) => info!("ThingsBoard send failed: {e:?}"),
            }
        }
        Err(e) => info!("ThingsBoard connect failed: {e:?}"),
    }
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

    // LED init — devkit: WS2812 on GPIO8, xiao: active-low GPIO15
    #[cfg(feature = "esp32c6_devkit")]
    let led: LedAdapter = {
        let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).expect("Failed to initialize RMT");
        static LED_BUFFER: StaticCell<[PulseCode; esp_hal_smartled::buffer_size(1)]> =
            StaticCell::new();
        let led_buffer = LED_BUFFER.init(smart_led_buffer!(1));
        SmartLedsAdapter::new_with_color(rmt.channel0, peripherals.GPIO8, led_buffer)
    };
    #[cfg(feature = "xiao_esp32c6")]
    let led: LedAdapter = Output::new(peripherals.GPIO15, Level::High, OutputConfig::default());

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    #[cfg(feature = "esp32c6_devkit")]
    let board_name = "ESP32C6 DevKit";
    #[cfg(feature = "xiao_esp32c6")]
    let board_name = "Xiao ESP32C6";
    info!("Zappy initialized for {}! (token: {}...)", board_name, &THINGSBOARD_TOKEN[..2]);

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

    // Zap pin — devkit: GPIO4, xiao: GPIO2
    #[cfg(feature = "esp32c6_devkit")]
    let mut zap_pin = Input::new(
        peripherals.GPIO4,
        InputConfig::default().with_pull(Pull::Down),
    );
    #[cfg(feature = "xiao_esp32c6")]
    let mut zap_pin = Input::new(
        peripherals.GPIO2,
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

    spawner.spawn(thingsboard_task(stack)).unwrap();

    let mut zap_count: u32 = 0;
    let zap_pub = ZAP_PUBSUB.publisher().unwrap();
    loop {
        zap_pin.wait_for_rising_edge().await;
        zap_count += 1;
        info!("Zap! count={zap_count}");
        zap_pub.publish_immediate(());
        Timer::after(Duration::from_millis(100)).await; // debounce
    }
}
