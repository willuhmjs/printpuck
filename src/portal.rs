// SoftAP configuration portal: DHCP + captive DNS + HTTP settings form on
// 192.168.4.1. Same proven plumbing as the assistant's portal (edge-dhcp /
// edge-captive codecs driven over embassy-net sockets); the form, fields and
// styling are PrintPuck's own.

use alloc::{format, string::String, vec, vec::Vec};
use core::cell::Cell;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use edge_dhcp::{
    server::{Server as DhcpServer, ServerOptions},
    Options as DhcpOptions, Packet as DhcpPacket,
};
use embassy_futures::select::{select4, Either4};
use embassy_net::{
    udp::{PacketMetadata, UdpMetadata, UdpSocket},
    IpAddress, IpEndpoint, IpListenEndpoint, Stack,
};
use embassy_time::{Duration, Timer};
use esp_println::println;

use crate::settings::{Settings, FIELD_ORDER};

pub const PORTAL_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
pub const PORTAL_SSID: &str = "PrintPuck-Setup";
const PORTAL_URL: &str = "http://192.168.4.1";

/// Outcome of a portal session. Both variants end in a reboot.
pub enum PortalOutcome {
    Saved(Settings),
    Cancelled,
}

/// Message shown on the puck while the portal runs, drawn by the main UI loop.
pub struct PortalStatus {
    pub clients: u8,
}

/// Runs the portal until the user either submits the form or taps to cancel.
///
/// `stack` must be the access-point interface's stack, already configured with
/// a static address of `PORTAL_IP`, and the radio must already be in AP mode.
///
/// `ui_tick` is called every ~500ms to render the portal screen; it returns
/// `true` if the user tapped to cancel.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    stack: Stack<'static>,
    touch_events: &embassy_sync::channel::Channel<
        embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
        crate::touch::TouchEvent,
        8,
    >,
    current: &Settings,
) -> (PortalOutcome, Cell<u8>) {
    // Socket buffers on the PSRAM heap (safe here: CPU-only access, and the
    // flash-write cache-off window is inside a critical section).
    let mut dhcp_rx_meta = [PacketMetadata::EMPTY; 4];
    let mut dhcp_tx_meta = [PacketMetadata::EMPTY; 4];
    let mut dhcp_rx = vec![0u8; 1024];
    let mut dhcp_tx = vec![0u8; 1024];
    let mut dhcp_scratch = vec![0u8; 1024];

    let mut dns_rx_meta = [PacketMetadata::EMPTY; 4];
    let mut dns_tx_meta = [PacketMetadata::EMPTY; 4];
    let mut dns_rx = vec![0u8; 768];
    let mut dns_tx = vec![0u8; 768];
    let mut dns_scratch = vec![0u8; 768];

    let mut http_rx = vec![0u8; 2048];
    let mut http_tx = vec![0u8; 2048];

    let clients = Cell::new(0u8);

    let dhcp = dhcp_task(
        stack,
        &mut dhcp_rx_meta,
        &mut dhcp_rx,
        &mut dhcp_tx_meta,
        &mut dhcp_tx,
        &mut dhcp_scratch,
        &clients,
    );
    let dns = dns_task(
        stack,
        &mut dns_rx_meta,
        &mut dns_rx,
        &mut dns_tx_meta,
        &mut dns_tx,
        &mut dns_scratch,
    );
    let http = http_task(stack, &mut http_rx, &mut http_tx, current);
    let ui_loop = async {
        loop {
            if let Ok(crate::touch::TouchEvent::Tap { .. }) = embassy_time::with_timeout(
                Duration::from_millis(500),
                touch_events.receive(),
            )
            .await
            {
                return;
            }
        }
    };

    match select4(dhcp, dns, http, ui_loop).await {
        Either4::Third(settings) => (PortalOutcome::Saved(settings), clients),
        _ => (PortalOutcome::Cancelled, clients),
    }
}

// ---------------------------------------------------------------- DHCP -----

#[allow(clippy::too_many_arguments)]
async fn dhcp_task(
    stack: Stack<'static>,
    rx_meta: &mut [PacketMetadata],
    rx_buf: &mut [u8],
    tx_meta: &mut [PacketMetadata],
    tx_buf: &mut [u8],
    scratch: &mut [u8],
    clients: &Cell<u8>,
) -> Settings {
    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    if let Err(e) = socket.bind(IpListenEndpoint { addr: None, port: 67 }) {
        println!("portal: dhcp bind failed: {e:?}");
        return core::future::pending().await;
    }

    let mut server = DhcpServer::<_, 4>::new(|| embassy_time::Instant::now().as_secs(), PORTAL_IP);
    let mut gw = [PORTAL_IP];
    let dns_servers = [PORTAL_IP];

    let mut packet = vec![0u8; 1024];
    loop {
        let (len, meta) = match socket.recv_from(&mut packet).await {
            Ok(v) => v,
            Err(e) => {
                println!("portal: dhcp recv error: {e:?}");
                continue;
            }
        };
        let request = match DhcpPacket::decode(&packet[..len]) {
            Ok(r) => r,
            Err(e) => {
                println!("portal: bad dhcp packet: {e:?}");
                continue;
            }
        };

        let mut options = ServerOptions::new(PORTAL_IP, Some(&mut gw));
        options.dns = &dns_servers;
        // RFC 8910 captive-portal URL: iOS/Android open the page directly.
        options.captive_url = Some(PORTAL_URL);

        let mut opt_buf = DhcpOptions::buf();
        let Some(reply) = server.handle_request(&mut opt_buf, &options, &request) else {
            continue;
        };
        let handed_out_address = !reply.yiaddr.is_unspecified();

        // RFC 2131 4.1 destination selection.
        let dest = if !request.giaddr.is_unspecified() {
            SocketAddr::V4(SocketAddrV4::new(request.giaddr, 67))
        } else if !request.ciaddr.is_unspecified() && !request.broadcast {
            SocketAddr::V4(SocketAddrV4::new(request.ciaddr, 68))
        } else {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, 68))
        };

        let encoded = match reply.encode(scratch) {
            Ok(b) => b,
            Err(e) => {
                println!("portal: dhcp encode failed: {e:?}");
                continue;
            }
        };
        let dest: UdpMetadata = match dest {
            SocketAddr::V4(v4) => IpEndpoint::new(IpAddress::Ipv4(*v4.ip()), v4.port()).into(),
            _ => meta,
        };
        if let Err(e) = socket.send_to(encoded, dest).await {
            println!("portal: dhcp send error: {e:?}");
        } else if handed_out_address {
            clients.set(clients.get().saturating_add(1).min(9));
        }
    }
}

// ----------------------------------------------------------------- DNS -----

async fn dns_task(
    stack: Stack<'static>,
    rx_meta: &mut [PacketMetadata],
    rx_buf: &mut [u8],
    tx_meta: &mut [PacketMetadata],
    tx_buf: &mut [u8],
    scratch: &mut [u8],
) -> Settings {
    let mut socket = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    if let Err(e) = socket.bind(IpListenEndpoint { addr: None, port: 53 }) {
        println!("portal: dns bind failed: {e:?}");
        return core::future::pending().await;
    }

    let mut query = vec![0u8; 768];
    loop {
        let (len, meta) = match socket.recv_from(&mut query).await {
            Ok(v) => v,
            Err(e) => {
                println!("portal: dns recv error: {e:?}");
                continue;
            }
        };
        match edge_captive::reply(
            &query[..len],
            &PORTAL_IP.octets(),
            Duration::from_secs(60).into(),
            scratch,
        ) {
            Ok(n) => {
                if let Err(e) = socket.send_to(&scratch[..n], meta).await {
                    println!("portal: dns send error: {e:?}");
                }
            }
            Err(e) => println!("portal: dns reply failed: {e:?}"),
        }
    }
}

// ---------------------------------------------------------------- HTTP -----

async fn http_task(
    stack: Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    current: &Settings,
) -> Settings {
    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, rx_buf, tx_buf);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if let Err(e) = socket.accept(IpListenEndpoint { addr: None, port: 80 }).await {
            println!("portal: accept failed: {e:?}");
            Timer::after(Duration::from_millis(200)).await;
            continue;
        }

        match serve_one(&mut socket, current).await {
            Some(new_settings) => {
                socket.close();
                Timer::after(Duration::from_millis(500)).await;
                socket.abort();
                return new_settings;
            }
            None => {
                socket.close();
                Timer::after(Duration::from_millis(50)).await;
                socket.abort();
            }
        }
    }
}

/// Handles a single request. Returns `Some` only for a successful form POST.
async fn serve_one(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    current: &Settings,
) -> Option<Settings> {
    let mut buf = vec![0u8; 4096];
    let mut n = 0usize;
    let head_end = loop {
        if n == buf.len() {
            return None;
        }
        let read = socket.read(&mut buf[n..]).await.ok()?;
        if read == 0 {
            return None;
        }
        n += read;
        if let Some(p) = find(&buf[..n], b"\r\n\r\n") {
            break p + 4;
        }
    };

    let head = core::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?;
    let path = parts.next()?;
    println!("portal: {method} {path}");

    if method == "POST" && path.starts_with("/save") {
        let content_length = head
            .split("\r\n")
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if content_length > 8192 {
            respond(socket, "413 Payload Too Large", "text/plain", b"too big").await;
            return None;
        }

        let mut body = Vec::with_capacity(content_length);
        body.extend_from_slice(&buf[head_end..n]);
        while body.len() < content_length {
            let mut chunk = [0u8; 512];
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(content_length);

        let updated = apply_form(current, &body);
        let page = saved_page();
        respond(socket, "200 OK", "text/html; charset=utf-8", page.as_bytes()).await;
        socket.flush().await.ok();
        return Some(updated);
    }

    // Every GET - including captive-portal probes - gets the form.
    let page = form_page(current);
    respond(socket, "200 OK", "text/html; charset=utf-8", page.as_bytes()).await;
    socket.flush().await.ok();
    None
}

async fn respond(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: &[u8],
) {
    use embedded_io_async::Write;
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    socket.write_all(body).await.ok();
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------- form -----

fn form_page(current: &Settings) -> String {
    let mut html = String::with_capacity(3072);
    html.push_str(
        "<!DOCTYPE html><html><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>PrintPuck Setup</title><style>\
         body{font-family:-apple-system,system-ui,sans-serif;background:#0d1117;color:#e6edf3;\
         margin:0;padding:20px;max-width:520px}\
         h1{font-size:20px;margin:0 0 4px}p.sub{color:#8b949e;font-size:13px;margin:0 0 20px}\
         label{display:block;margin:14px 0 4px;font-size:13px;color:#c9d1d9}\
         input{width:100%;box-sizing:border-box;padding:10px;font-size:16px;\
         border:1px solid #30363d;border-radius:8px;background:#161b22;color:#e6edf3}\
         button{width:100%;margin-top:24px;padding:14px;font-size:16px;font-weight:600;\
         border:0;border-radius:8px;background:#1f6feb;color:#fff}\
         </style></head><body><h1>PrintPuck Setup</h1>\
         <p class=sub>Point this puck at a Bambu Lab printer on your network. \
         Saving restarts the device.</p>\
         <form method=POST action=/save>",
    );
    for field in FIELD_ORDER {
        let value = current.get(field);
        html.push_str("<label for=");
        html.push_str(field.key());
        html.push('>');
        push_escaped(&mut html, field.label());
        html.push_str("</label><input id=");
        html.push_str(field.key());
        html.push_str(" name=");
        html.push_str(field.key());
        if field.is_secret() {
            html.push_str(" type=password autocomplete=off placeholder=\"");
            push_escaped(&mut html, if value.is_empty() { field.hint() } else { "unchanged" });
            html.push('"');
        } else {
            html.push_str(" type=text autocapitalize=off autocorrect=off placeholder=\"");
            push_escaped(&mut html, field.hint());
            html.push_str("\" value=\"");
            push_escaped(&mut html, value);
            html.push('"');
        }
        html.push('>');
    }
    html.push_str("<button type=submit>Save &amp; Restart</button></form></body></html>");
    html
}

fn saved_page() -> String {
    String::from(
        "<!DOCTYPE html><html><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Saved</title><style>body{font-family:-apple-system,system-ui,sans-serif;\
         background:#0d1117;color:#e6edf3;margin:0;padding:40px 20px;text-align:center}\
         h1{font-size:22px}p{color:#8b949e}</style></head><body>\
         <h1>Saved</h1><p>The puck is restarting and will join your network. \
         This page will stop responding.</p></body></html>",
    )
}

fn push_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Folds a urlencoded form body onto a copy of the current settings. A field
/// that is absent, or present but empty *and* secret, keeps its old value.
fn apply_form(current: &Settings, body: &[u8]) -> Settings {
    let mut out = current.clone();
    for pair in body.split(|&b| b == b'&') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = urldecode(&pair[..eq]);
        let value = urldecode(&pair[eq + 1..]);
        let Some(field) = FIELD_ORDER.into_iter().find(|f| f.key() == key) else {
            continue;
        };
        if value.is_empty() && field.is_secret() {
            continue;
        }
        out.set(field, value);
    }
    out
}

fn urldecode(raw: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < raw.len() => {
                match (hex(raw[i + 1]), hex(raw[i + 2])) {
                    (Some(h), Some(l)) => {
                        bytes.push(h << 4 | l);
                        i += 3;
                    }
                    _ => {
                        bytes.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(bytes).unwrap_or_default()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
