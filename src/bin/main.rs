// PrintPuck: a round ESP32-S3 desk display for Bambu Lab printers.
//
// Original firmware (bare-metal Rust, no ESP-IDF). Hardware bring-up and the
// wifi/portal/settings/touch/display component layer come from the author's
// esp32s3-ai-assistant (same board); everything user-visible here is designed
// from scratch. Protocol facts (MQTT topics, JSON fields) are Bambu's
// documented local interface; no code was taken from any other project.

#![no_std]
#![no_main]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

#[path = "../pins.rs"]
mod pins;
#[path = "../touch.rs"]
mod touch;
#[path = "../diff.rs"]
mod diff;
#[path = "../settings.rs"]
mod settings;
#[path = "../portal.rs"]
mod portal;
#[path = "../mqtt.rs"]
mod mqtt;
#[path = "../model.rs"]
mod model;
#[path = "../ui.rs"]
mod ui;

use alloc::vec;
use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_net::{
    Runner, StackResources,
};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Delay, Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_println::println;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    rng::Rng,
    spi::{master::{Config as SpiConfig, Spi}, Mode},
    time::Rate,
    timer::timg::TimerGroup,
    Async,
};
use esp_radio::wifi::{
    ap::AccessPointConfig, sta::StationConfig, AuthenticationMethod, Config, ControllerConfig,
    Interface, WifiController,
};
use lcd_async::{
    interface::SpiInterface,
    models::GC9A01,
    options::{ColorInversion, ColorOrder, Orientation},
    Builder,
};
use log::info;

use model::{apply_report, pushall_payload, Status};
use settings::{Field, Settings};
use touch::TouchEvent;
use ui::Screen;

esp_bootloader_esp_idf::esp_app_desc!();

const WIFI_JOIN_TICKS: usize = 25;
const WIFI_JOIN_TICK: Duration = Duration::from_millis(1000);
const TOUCH_INT_FALLBACK_MS: u64 = 500;
/// MQTT keep-alive the printer broker honors.
const MQTT_KEEPALIVE_SECS: u16 = 30;
/// Reconnect backoff after a failed MQTT/TLS attempt.
const MQTT_RETRY_MS: u64 = 5_000;
/// Full-state refresh cadence (the printer also pushes deltas on its own).
const PUSHALL_EVERY_MS: u64 = 120_000;
/// Tap the center third of the dial to toggle the chamber light.
const LIGHT_TAP_RADIUS: i32 = 40;

static TOUCH_EVENTS: Channel<CriticalSectionRawMutex, TouchEvent, 8> = Channel::new();
static PORTAL_REQUEST: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static PORTAL_UP: Signal<CriticalSectionRawMutex, bool> = Signal::new();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 8 * 1024);
    // Framebuffer + MQTT/TLS buffers live in PSRAM.
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // ---------- Settings ----------
    let mut flash = esp_storage::FlashStorage::new(peripherals.FLASH);
    let store = settings::Store::locate(&mut flash);
    let mut settings = store
        .as_ref()
        .and_then(|s| s.load(&mut flash))
        .unwrap_or_else(Settings::unconfigured);

    // ---------- Display ----------
    let mut backlight = Output::new(peripherals.GPIO42, Level::High, OutputConfig::default());
    let dc = Output::new(peripherals.GPIO47, Level::Low, OutputConfig::default());
    let rst = Output::new(peripherals.GPIO38, Level::High, OutputConfig::default());
    let cs = Output::new(peripherals.GPIO5, Level::High, OutputConfig::default());

    let display_spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO4)
    .with_mosi(peripherals.GPIO2)
    .into_async();

    let spi_device = ExclusiveDevice::new_no_delay(display_spi, cs).unwrap();
    let di = SpiInterface::new(spi_device, dc);

    let mut delay = Delay;
    let mut display = Builder::new(GC9A01, di)
        .reset_pin(rst)
        .display_size(pins::DISPLAY_WIDTH, pins::DISPLAY_HEIGHT)
        .orientation(Orientation::new())
        .invert_colors(ColorInversion::Inverted)
        .color_order(ColorOrder::Bgr)
        .init(&mut delay)
        .await
        .expect("display init failed");
    backlight.set_low(); // active-low backlight enable

    let mut ui_frame = vec![0u8; diff::FRAME_BYTES * 2];
    {
        let (frame, prev) = ui_frame.split_at_mut(diff::FRAME_BYTES);
        let boot = Screen::Boot { step: "boot", progress: 5 };
        ui::draw(frame, &boot, &Status::default(), 0);
        diff::flush_changed(&mut display, prev, frame).await;
    }
    info!("Display initialized");

    // ---------- Touch ----------
    let touch_i2c = I2c::new(
        peripherals.I2C1,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(peripherals.GPIO11)
    .with_scl(peripherals.GPIO7)
    .into_async();
    let mut touch_rst = Output::new(peripherals.GPIO6, Level::High, OutputConfig::default());
    touch_rst.set_low();
    Timer::after(Duration::from_millis(10)).await;
    touch_rst.set_high();
    Timer::after(Duration::from_millis(50)).await;
    let touch_int = Input::new(
        peripherals.GPIO12,
        InputConfig::default().with_pull(Pull::Up),
    );
    match touch::Cst816d::new(touch_i2c, pins::TOUCH_CST816D_ADDR).await {
        Ok(touch_dev) => {
            let task = touch_task(touch_dev, touch_int).unwrap();
            spawner.spawn(task);
        }
        Err(e) => println!("CST816D init failed: {e:?}"),
    }

    // ---------- WiFi ----------
    info!("Bringing up wifi");
    let station_config = Config::Station(
        StationConfig::default()
            .with_ssid(settings.wifi_ssid())
            .with_password(settings.wifi_password().into()),
    );
    let (controller, interfaces) = esp_radio::wifi::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .expect("failed to init wifi controller");

    let wifi_interface = interfaces.station;
    let ap_interface = interfaces.access_point;
    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed,
    );
    let (ap_stack, ap_runner) = embassy_net::new(
        ap_interface,
        embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
            address: embassy_net::Ipv4Cidr::new(portal::PORTAL_IP, 24),
            gateway: None,
            dns_servers: Default::default(),
        }),
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        seed ^ 0x5a5a_5a5a,
    );

    spawner.spawn(connection_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(ap_net_task(ap_runner).unwrap());

    // Unconfigured or failed join -> setup portal.
    let setup_reason = if let Some(bad) = settings.first_invalid() {
        info!("Not usable yet (check \"{}\") - starting setup", bad.label());
        Some("nothing is configured yet")
    } else {
        let mut joined = false;
        for tick in 0..WIFI_JOIN_TICKS {
            if embassy_time::with_timeout(WIFI_JOIN_TICK, stack.wait_config_up())
                .await
                .is_ok()
            {
                joined = true;
                break;
            }
            let progress = 20 + (tick as u32 + 1) * 55 / WIFI_JOIN_TICKS as u32;
            show(
                &mut display,
                &mut ui_frame,
                &Screen::Boot { step: "joining wi-fi", progress: progress as u8 },
                &Status::default(),
            )
            .await;
        }
        if joined {
            None
        } else {
            Some("could not join the saved network")
        }
    };

    if let Some(reason) = setup_reason {
        if let Some(store) = store.as_ref() {
            enter_portal(
                &mut display,
                &mut ui_frame,
                ap_stack,
                store,
                &mut flash,
                &settings,
                reason,
            )
            .await;
        }
        println!("no storage partition - cannot run setup, continuing unconfigured");
    }

    info!("Entering monitor loop");

    // ---------- Monitor loop ----------
    let mut status = Status::default();
    let mut anim: u32 = 0;

    'monitor: loop {
        show(&mut display, &mut ui_frame, &Screen::Offline { reason: "connecting" }, &status).await;
        // Connection buffers: owned here, recreated (and freed) per attempt.
        let mut sock_rx = vec![0u8; 4096];
        let mut sock_tx = vec![0u8; 4096];
        let mut tls_rd = vec![0u8; 16384];
        let mut tls_wr = vec![0u8; 16384];
        match mqtt_connect(&stack, &settings, seed, &mut sock_rx, &mut sock_tx, &mut tls_rd, &mut tls_wr).await {
            Ok(mut session) => {
                println!("mqtt session up");
                loop {
                    // Fresh PSRAM receive buffer per wait; a pushall can be
                    // tens of KB.
                    let mut rx_buf = vec![0u8; 32 * 1024];
                    let msg_fut = session.next_message(&mut rx_buf);
                    let tick = Timer::after(Duration::from_millis(500));
                    match select(msg_fut, tick).await {
                        embassy_futures::select::Either::First(msg) => match msg {
                            Ok(Some(m)) => {
                                if m.topic == settings.report_topic() {
                                    apply_report(&mut status, m.payload, 0);
                                }
                            }
                            Ok(None) => { /* oversized packet, dropped */ }
                            Err(e) => {
                                println!("mqtt error: {e:?}");
                                continue 'monitor;
                            }
                        },
                        embassy_futures::select::Either::Second(_) => {
                            anim = anim.wrapping_add(1);
                            // Touch: center tap toggles the chamber light.
                            if let Ok(TouchEvent::Tap { x, y }) =
                                TOUCH_EVENTS.try_receive()
                            {
                                let dx = x as i32 - 120;
                                let dy = y as i32 - 120;
                                if dx * dx + dy * dy < LIGHT_TAP_RADIUS * LIGHT_TAP_RADIUS {
                                    let on = !status.light_on.unwrap_or(false);
                                    let payload = model::light_payload(session.next_seq(), on);
                                    if session
                                        .publish(&settings.request_topic(), payload.as_bytes())
                                        .await
                                        .is_ok()
                                    {
                                        status.light_on = Some(on);
                                        println!("chamber light -> {}", if on { "on" } else { "off" });
                                    }
                                }
                            }
                            show(&mut display, &mut ui_frame, &Screen::Dashboard, &status).await;
                            // Periodic full-state refresh.
                            if session.since_pushall_ms() > PUSHALL_EVERY_MS {
                                let payload = pushall_payload(session.next_seq());
                                if session
                                    .publish(&settings.request_topic(), payload.as_bytes())
                                    .await
                                    .is_ok()
                                {
                                    session.mark_pushall();
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                println!("connect failed: {e}");
                show(&mut display, &mut ui_frame, &Screen::Offline { reason: "printer unreachable" }, &status).await;
                Timer::after(Duration::from_millis(MQTT_RETRY_MS)).await;
            }
        }
    }
}

/// Renders `screen` and flushes the dirty region.
async fn show<DI, RST>(
    display: &mut lcd_async::Display<DI, lcd_async::models::GC9A01, RST>,
    ui_frame: &mut [u8],
    screen: &Screen,
    status: &Status,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    let (frame, prev) = ui_frame.split_at_mut(diff::FRAME_BYTES);
    ui::draw(frame, screen, status, 0);
    diff::flush_changed(display, prev, frame).await;
}

/// TLS stream type: embedded-tls over an embassy-net TCP socket.
type TlsStream<'a> = embedded_tls::TlsConnection<
    'a,
    embassy_net::tcp::TcpSocket<'a>,
    embedded_tls::Aes128GcmSha256,
>;

/// The whole MQTT session, kept alive across loop iterations. Owns the TLS
/// connection; the MQTT client is created per call (it's a zero-cost wrapper
/// over `&mut conn`).
struct Session<'a> {
    conn: TlsStream<'a>,
    seq: u32,
    last_pushall: Option<embassy_time::Instant>,
}

impl<'a> Session<'a> {
    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.seq
    }
    fn since_pushall_ms(&self) -> u64 {
        match self.last_pushall {
            Some(t) => t.elapsed().as_millis() as u64,
            None => u64::MAX,
        }
    }
    fn mark_pushall(&mut self) {
        self.last_pushall = Some(embassy_time::Instant::now());
    }
    async fn publish(
        &mut self,
        topic: &str,
        payload: &[u8],
    ) -> Result<(), mqtt::Error<embedded_tls::TlsError>> {
        mqtt::Client::new(&mut self.conn).publish(topic, payload).await
    }
    async fn next_message<'buf>(
        &mut self,
        buf: &'buf mut [u8],
    ) -> Result<Option<mqtt::Message<'buf>>, mqtt::Error<embedded_tls::TlsError>> {
        mqtt::Client::new(&mut self.conn).next_message(buf).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn mqtt_connect<'a>(
    stack: &embassy_net::Stack<'static>,
    settings: &Settings,
    seed: u64,
    sock_rx: &'a mut [u8],
    sock_tx: &'a mut [u8],
    tls_rd: &'a mut [u8],
    tls_wr: &'a mut [u8],
) -> Result<Session<'a>, &'static str> {
    use embedded_tls::{TlsConfig, TlsContext, UnsecureProvider};
    use rand_chacha::ChaCha8Rng;
    use rand_core::SeedableRng;

    let host = settings.get(Field::PrinterHost);
    let ip: Ipv4Addr = host.parse().map_err(|_| "printer ip is not an ipv4 address")?;
    let endpoint = embassy_net::IpEndpoint::new(embassy_net::IpAddress::Ipv4(ip), 8883);

    let mut socket = embassy_net::tcp::TcpSocket::new(*stack, sock_rx, sock_tx);
    socket.set_timeout(Some(Duration::from_secs(20)));
    socket
        .connect(endpoint)
        .await
        .map_err(|_| "tcp connect failed")?;

    // TLS 1.3 handshake. No cert verify: the printer presents a self-signed
    // device cert; the MQTT access code is the actual auth boundary on this
    // LAN. The "alloc" feature is on, so RSA signature schemes are offered -
    // Bambu's broker (wolfSSL) presents an RSA cert.
    let mut conn: TlsStream<'_> = embedded_tls::TlsConnection::new(socket, tls_rd, tls_wr);
    let rng = ChaCha8Rng::seed_from_u64(seed ^ 0x9e37_79b9);
    let config = TlsConfig::new();
    conn.open(TlsContext::new(&config, UnsecureProvider::new(rng)))
        .await
        .map_err(|_| "tls handshake failed")?;
    println!("tls up");

    let mut client = mqtt::Client::new(&mut conn);
    client
        .connect(
            "printpuck",
            "bblp",
            settings.get(Field::AccessCode),
            MQTT_KEEPALIVE_SECS,
        )
        .await
        .map_err(|_| "mqtt connect rejected (check access code)")?;
    client
        .subscribe(&settings.report_topic())
        .await
        .map_err(|_| "subscribe failed")?;
    println!("mqtt subscribed to {}", settings.report_topic());

    let mut session = Session { conn, seq: 1, last_pushall: None };
    let payload = pushall_payload(session.next_seq());
    session
        .publish(&settings.request_topic(), payload.as_bytes())
        .await
        .map_err(|_| "pushall failed")?;
    session.mark_pushall();
    Ok(session)
}

/// Boots the AP, runs the portal, saves, reboots. Never returns.
async fn enter_portal<DI, RST>(
    display: &mut lcd_async::Display<DI, lcd_async::models::GC9A01, RST>,
    ui_frame: &mut [u8],
    ap_stack: embassy_net::Stack<'static>,
    store: &settings::Store,
    flash: &mut esp_storage::FlashStorage<'_>,
    settings: &Settings,
    reason: &str,
) where
    DI: lcd_async::interface::Interface<Word = u8>,
    RST: embedded_hal::digital::OutputPin,
{
    println!("entering setup portal: {reason}");
    PORTAL_REQUEST.signal(());
    PORTAL_UP.wait().await;

    // Portal runs to completion; the UI ticks alongside it. The portal's own
    // exit is a form POST (Saved) or a screen tap (Cancelled) - both end in
    // a reboot below.
    let (outcome, clients) = portal::run(ap_stack, &TOUCH_EVENTS, settings).await;
    let _ = clients;

    match outcome {
        portal::PortalOutcome::Saved(mut s) => {
            // Keep secrets the form intentionally left blank.
            for f in settings::FIELD_ORDER {
                if s.get(f).is_empty() && f.is_secret() {
                    s.set(f, settings.get(f));
                }
            }
            match store.save(flash, &s) {
                Ok(()) => println!("settings saved; rebooting"),
                Err(e) => println!("save failed: {e}"),
            }
        }
        portal::PortalOutcome::Cancelled => println!("setup cancelled; rebooting"),
    }
    esp_hal::system::software_reset()
}

#[embassy_executor::task]
async fn touch_task(
    mut touch: touch::Cst816d<I2c<'static, Async>>,
    mut int_pin: Input<'static>,
) {
    loop {
        let woke_on_int = embassy_time::with_timeout(
            Duration::from_millis(TOUCH_INT_FALLBACK_MS),
            int_pin.wait_for_falling_edge(),
        )
        .await
        .is_ok();

        let mut first: Option<touch::TouchPoint> = None;
        let mut last: Option<touch::TouchPoint> = None;
        loop {
            match touch.read().await {
                Ok(Some(p)) => {
                    if first.is_none() {
                        if !woke_on_int {
                            println!("touch: INT missed a press, caught by poll fallback");
                        }
                        first = Some(p);
                    }
                    last = Some(p);
                }
                Ok(None) => {
                    if let (Some(a), Some(b)) = (first, last) {
                        let event = touch::classify(a, b);
                        println!("touch ({},{}) -> ({},{}) = {event:?}", a.x, a.y, b.x, b.y);
                        TOUCH_EVENTS.send(event).await;
                    }
                    break;
                }
                Err(e) => {
                    println!("touch read error: {e:?}");
                    break;
                }
            }
            Timer::after(Duration::from_millis(20)).await;
        }
    }
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    {
        let station = async {
            loop {
                println!("Connecting to wifi...");
                match controller.connect_async().await {
                    Ok(info) => {
                        println!("Wifi connected: {info:?}");
                        let info = controller.wait_for_disconnect_async().await.ok();
                        println!("Wifi disconnected: {info:?}");
                    }
                    Err(e) => println!("Wifi connect failed: {e:?}"),
                }
                Timer::after(Duration::from_millis(5000)).await;
            }
        };
        embassy_futures::select::select(station, PORTAL_REQUEST.wait()).await;
    }

    info!("Switching radio to setup AP");
    let ap = Config::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(portal::PORTAL_SSID)
            .with_auth_method(AuthenticationMethod::None)
            .with_max_connections(4),
    );
    match controller.set_config(&ap) {
        Ok(()) => {
            println!("Setup AP up: {}", portal::PORTAL_SSID);
            PORTAL_UP.signal(true);
        }
        Err(e) => {
            println!("Failed to start setup AP: {e:?}");
            PORTAL_UP.signal(false);
        }
    }
    core::future::pending::<()>().await
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// embassy-executor tasks are monomorphised per declaration, so the two
/// stacks need two declarations rather than two spawns of one.
#[embassy_executor::task]
async fn ap_net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}
