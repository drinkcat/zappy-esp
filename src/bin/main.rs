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
use embassy_futures::select::{Either3, select3};
#[cfg(feature = "thingsboard")]
use embassy_futures::select::{Either, select};
use embassy_net::{Runner, Stack, StackResources};
#[cfg(feature = "thingsboard")]
use embassy_net::dns::DnsSocket;
#[cfg(feature = "thingsboard")]
use embassy_net::tcp::client::{TcpClient, TcpClientState};
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
#[cfg(feature = "thingsboard")]
use reqwless::client::HttpClient;
#[cfg(feature = "thingsboard")]
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
use alloc::string::ToString as _;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
// MQTT keepalive advertised to the broker, and how often we ping. The ping
// interval must be well under the keepalive or the broker drops us (it allows
// up to 1.5x keepalive without a packet).
const MQTT_KEEPALIVE_SECS: u16 = 60;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(feature = "thingsboard")]
const THINGSBOARD_TOKEN: &str = env!("THINGSBOARD_TOKEN");
#[cfg(feature = "thingsboard")]
const THINGSBOARD_HOST: &str = "thingsboard.cloud";

#[cfg(feature = "homeassistant")]
const HA_HOST: &str = env!("HA_HOST");
#[cfg(feature = "homeassistant")]
const HA_USER: &str = env!("HA_USER");
#[cfg(feature = "homeassistant")]
const HA_TOKEN: &str = env!("HA_TOKEN");

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

// Timestamp source for esp-println's `timestamp` feature: milliseconds since
// boot from embassy-time's monotonic clock. Logs become "INFO (12345) - msg".
#[unsafe(no_mangle)]
extern "Rust" fn _esp_println_timestamp() -> u64 {
    embassy_time::Instant::now().as_millis()
}

#[cfg(feature = "esp32c6_devkit")]
type LedAdapter = SmartLedsAdapter<'static, { esp_hal_smartled::buffer_size(1) }, RGB<u8>>;
#[cfg(feature = "xiao_esp32c6")]
type LedAdapter = Output<'static>;

static WIFI_CONNECTED: Signal<CriticalSectionRawMutex, bool> = Signal::new();
// PubSubChannel: capacity 4, 2 subscribers (led_task + mqtt_task), 1 publisher
// Each subscriber has an independent read pointer, so a slow LED task won't block MQTT.
static ZAP_PUBSUB: PubSubChannel<CriticalSectionRawMutex, u32, 4, 2, 1> = PubSubChannel::new();

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
        sub.next_message_pure().await; // value unused by LED task
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

#[cfg(feature = "homeassistant")]
#[embassy_executor::task]
async fn mqtt_task(stack: Stack<'static>) {
    use core::num::NonZero;
    use rust_mqtt::{
        Bytes,
        buffer::AllocBuffer,
        client::{
            Client,
            options::{ConnectOptions, PublicationOptions, TopicReference},
        },
        config::KeepAlive,
        types::{MqttBinary, MqttString, TopicName},
    };

    let mut sub = ZAP_PUBSUB.subscriber().unwrap();

    // Unique, stable MQTT client id derived from the STA MAC. A shared id like
    // "zappy" gets evicted (Disconnect: SessionTakenOver) whenever anything else
    // connects with the same id, causing an endless reconnect storm.
    let mac = esp_radio::wifi::sta_mac();
    let client_id = alloc::format!(
        "zappy-{:02x}{:02x}{:02x}",
        mac[3],
        mac[4],
        mac[5]
    );
    info!("MQTT client id: {client_id}");

    // Socket buffers must be initialized exactly once — a StaticCell panics if
    // init twice, so these live outside the reconnect loop. The TcpSocket
    // borrows them fresh on each iteration.
    static RX: StaticCell<[u8; 1024]> = StaticCell::new();
    static TX: StaticCell<[u8; 1024]> = StaticCell::new();
    let rx_buf = RX.init([0; 1024]);
    let tx_buf = TX.init([0; 1024]);

    loop {
        info!("MQTT connecting to {}...", HA_HOST);
        let mut sock = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buf[..], &mut tx_buf[..]);

        let remote = match stack.dns_query(HA_HOST, embassy_net::dns::DnsQueryType::A).await {
            Ok(addrs) if !addrs.is_empty() => embassy_net::IpEndpoint::new(addrs[0], 1883),
            _ => {
                info!("MQTT DNS failed, retrying in 10s");
                Timer::after(Duration::from_secs(10)).await;
                continue;
            }
        };

        // The TCP idle timeout must exceed the MQTT ping interval, or the
        // socket closes itself between pings (broker logs "connection closed by
        // client") and we reconnect forever. Give it generous room past the
        // ping interval so a single slow round-trip doesn't tear down the socket.
        sock.set_timeout(Some(embassy_time::Duration::from_secs(
            MQTT_KEEPALIVE_SECS as u64 * 2,
        )));
        if let Err(e) = sock.connect(remote).await {
            info!("MQTT TCP connect failed: {e:?}, retrying in 10s");
            Timer::after(Duration::from_secs(10)).await;
            continue;
        }

        let mut buffer = AllocBuffer;
        let mut client = Client::<'_, _, _, 0, 1, 1, 0>::new(&mut buffer);

        let connect_opts = ConnectOptions::new()
            .clean_start()
            .keep_alive(KeepAlive::Seconds(NonZero::new(MQTT_KEEPALIVE_SECS).unwrap()))
            .user_name(MqttString::try_from(HA_USER).unwrap())
            .password(MqttBinary::try_from(HA_TOKEN.as_bytes()).unwrap());

        match client
            .connect(
                sock,
                &connect_opts,
                Some(MqttString::try_from(client_id.as_str()).unwrap()),
            )
            .await
        {
            Ok(_) => info!("MQTT connected"),
            Err(e) => {
                info!("MQTT connect failed: {e:?}, retrying in 10s");
                Timer::after(Duration::from_secs(10)).await;
                continue;
            }
        }

        let boot_topic = TopicName::new(MqttString::try_from("zappy/boot").unwrap()).unwrap();
        let zap_topic = TopicName::new(MqttString::try_from("zappy/zap").unwrap()).unwrap();

        // MQTT discovery — HA auto-creates entities from these config messages
        let disc_zap_topic =
            TopicName::new(MqttString::try_from("homeassistant/sensor/zappy/zap/config").unwrap())
                .unwrap();
        let disc_boot_topic = TopicName::new(
            MqttString::try_from("homeassistant/sensor/zappy/boot/config").unwrap(),
        )
        .unwrap();
        let disc_zap_payload = br#"{"name":"Zappy Zap Count","state_topic":"zappy/zap","unique_id":"zappy_zap_count","state_class":"total_increasing","device":{"identifiers":["zappy"],"name":"Zappy"}}"#;
        // force_update lets HA register each boot=1 even though the value never
        // changes; omit it from the zap sensor, which is total_increasing.
        let disc_boot_payload = br#"{"name":"Zappy Boot","state_topic":"zappy/boot","unique_id":"zappy_boot","force_update":true,"device":{"identifiers":["zappy"],"name":"Zappy"}}"#;
        for (topic, payload) in [
            (disc_zap_topic, disc_zap_payload.as_slice()),
            (disc_boot_topic, disc_boot_payload.as_slice()),
        ] {
            // Retain discovery configs so HA keeps the entities across restarts
            // instead of losing them until the device next reconnects.
            let opts = PublicationOptions::new(TopicReference::Name(topic)).retain();
            if let Err(e) = client.publish(&opts, Bytes::from(payload)).await {
                info!("MQTT discovery publish failed: {e:?}");
            }
        }
        info!("MQTT discovery published");

        // Retain state too, so HA shows the last value between the device's
        // brief connection windows instead of going blank.
        let boot_opts = PublicationOptions::new(TopicReference::Name(boot_topic)).retain();
        let zap_opts = PublicationOptions::new(TopicReference::Name(zap_topic.clone())).retain();
        let boot_ok = client.publish(&boot_opts, Bytes::from(b"1".as_slice())).await;
        let zap_ok = client.publish(&zap_opts, Bytes::from(b"0".as_slice())).await;
        match (boot_ok, zap_ok) {
            (Ok(_), Ok(_)) => info!("MQTT boot published"),
            (Err(e), _) | (_, Err(e)) => {
                info!("MQTT boot publish failed: {e:?}");
            }
        }

        'connected: loop {
            let next_keepalive = Timer::after(KEEPALIVE_INTERVAL);
            // rust-mqtt is poll-driven: client.poll() is what reads incoming
            // packets (PINGRESP, broker control traffic). Without it the socket
            // RX backs up and the broker resets the connection within a
            // keepalive period. poll() races here so we drain it continuously;
            // poll_header (where it idles) is cancel-safe, so dropping it when
            // another branch wins is fine.
            match select3(sub.next_message_pure(), next_keepalive, client.poll()).await {
                Either3::First(count) => {
                    let payload = alloc::format!("{count}");
                    let pub_opts =
                        PublicationOptions::new(TopicReference::Name(zap_topic.clone())).retain();
                    match client.publish(&pub_opts, Bytes::from(payload.as_bytes())).await {
                        Ok(_) => info!("MQTT zap published (count={count})"),
                        Err(e) => {
                            info!("MQTT zap publish failed: {e:?}, reconnecting");
                            break 'connected;
                        }
                    }
                }
                Either3::Second(_) => match client.ping().await {
                    Ok(()) => info!("MQTT ping ok"),
                    Err(e) => {
                        info!("MQTT ping failed: {e:?}, reconnecting");
                        break 'connected;
                    }
                },
                Either3::Third(result) => match result {
                    Ok(_event) => {} // drained an incoming packet (e.g. PINGRESP)
                    Err(e) => {
                        info!("MQTT poll failed: {e:?}, reconnecting");
                        break 'connected;
                    }
                },
            }
        }
    }
}

#[cfg(feature = "thingsboard")]
#[embassy_executor::task]
async fn thingsboard_task(stack: Stack<'static>) {
    static TCP_STATE: StaticCell<TcpClientState<1, 1024, 1024>> = StaticCell::new();
    let tcp_client = TcpClient::new(stack, TCP_STATE.init(TcpClientState::new()));
    let dns = DnsSocket::new(stack);

    let url = format!("http://{THINGSBOARD_HOST}/api/v1/{THINGSBOARD_TOKEN}/telemetry");

    send_telemetry(&tcp_client, &dns, &url, Some("boot")).await;

    let mut sub = ZAP_PUBSUB.subscriber().unwrap();
    loop {
        let next_keepalive = Timer::after(KEEPALIVE_INTERVAL);
        let key = match select(sub.next_message_pure(), next_keepalive).await {
            Either::First(_) => Some("zap"),
            Either::Second(_) => None,
        };
        send_telemetry(&tcp_client, &dns, &url, key).await;
    }
}

#[cfg(feature = "thingsboard")]
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

    // 80 MHz is plenty for blinking an LED and publishing the occasional MQTT
    // message; running below max keeps the core cooler.
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz);
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
    info!("Zappy initialized for {}!", board_name);

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
    // WiFi power-save left at the default (None): modem-sleep added too much
    // latency to MQTT publishes/keepalives for a responsive device.

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

    #[cfg(feature = "homeassistant")]
    spawner.spawn(mqtt_task(stack)).unwrap();
    #[cfg(feature = "thingsboard")]
    spawner.spawn(thingsboard_task(stack)).unwrap();

    let mut zap_count: u32 = 0;
    let zap_pub = ZAP_PUBSUB.publisher().unwrap();
    loop {
        zap_pin.wait_for_rising_edge().await;
        zap_count += 1;
        info!("Zap! count={zap_count}");
        zap_pub.publish_immediate(zap_count);
        Timer::after(Duration::from_millis(100)).await; // debounce
    }
}
