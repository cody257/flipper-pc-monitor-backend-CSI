//! Network page data: Wi-Fi signal/link rate and public IP / VPN state.
//!
//! This is sampled on its own schedule (Wi-Fi every few seconds, public IP
//! every 10 minutes) and cached, so the 1 Hz send loop in `main.rs` only ever
//! reads a snapshot. Blocking there would delay the resource frame and make
//! the Flipper think the link died.

use serde::Serialize;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::process::Command;

/// First byte of the network frame. Lets the Flipper reject a frame that
/// happens to have the right length but is not this packet.
pub const NET_MAGIC: u8 = 0xA2;

pub const FLAG_HAS_IP: u8 = 1 << 0;
pub const FLAG_VPN: u8 = 1 << 1;
pub const FLAG_WIFI: u8 = 1 << 2;
pub const FLAG_IPV6: u8 = 1 << 3;

const WIFI_REFRESH: Duration = Duration::from_secs(3);
const IP_REFRESH: Duration = Duration::from_secs(600);
const IP_RETRY: Duration = Duration::from_secs(15);

/// Wire format. Must stay in sync with `NetStruct` in `pc_monitor.h`.
/// 11 bytes: fixed-width fields only, no strings, no `usize`.
#[derive(Serialize, Debug, Clone)]
pub struct NetInfo {
    pub magic: u8,
    pub wifi_signal: u8,
    pub wifi_mbps: u16,
    pub band: u8, // 0 = unknown, 1 = 2.4 GHz, 2 = 5 GHz, 3 = 6 GHz
    pub channel: u8,
    pub ip: [u8; 4],
    pub flags: u8,
}

/// Wire format for the SSID frame. Kept separate from `NetInfo` so neither
/// frame grows past the 20-byte comfort limit.
#[derive(Serialize, Debug, Clone)]
pub struct SsidInfo {
    pub magic: u8,
    pub len: u8,
    pub ssid: [u8; 12],
}

pub const SSID_MAGIC: u8 = 0xA3;

#[derive(Default, Clone)]
struct WifiInfo {
    connected: bool,
    signal: u8,
    mbps: u16,
    band: u8,
    channel: u8,
    ssid: [u8; 12],
    ssid_len: u8,
}

struct IpInfo {
    ip: [u8; 4],
    ipv6: bool,
    vpn: bool,
}

#[derive(Default)]
struct NetState {
    wifi: Option<WifiInfo>,
    ip: Option<IpInfo>,
    next_ip_check: Option<Instant>,
}

static STATE: OnceLock<Mutex<NetState>> = OnceLock::new();

fn state() -> &'static Mutex<NetState> {
    STATE.get_or_init(|| Mutex::new(NetState::default()))
}

fn lock() -> std::sync::MutexGuard<'static, NetState> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// Instant snapshot for the send loop. Never touches the network.
pub fn current() -> NetInfo {
    let s = lock();
    let mut info = NetInfo {
        magic: NET_MAGIC,
        wifi_signal: 0,
        wifi_mbps: 0,
        band: 0,
        channel: 0,
        ip: [0; 4],
        flags: 0,
    };

    if let Some(w) = &s.wifi {
        if w.connected {
            info.flags |= FLAG_WIFI;
            info.wifi_signal = w.signal;
            info.wifi_mbps = w.mbps;
            info.band = w.band;
            info.channel = w.channel;
        }
    }

    if let Some(i) = &s.ip {
        info.ip = i.ip;
        if i.ipv6 {
            info.flags |= FLAG_IPV6;
        } else {
            info.flags |= FLAG_HAS_IP;
        }
        if i.vpn {
            info.flags |= FLAG_VPN;
        }
    }

    info
}

/// SSID snapshot for the third frame.
pub fn current_ssid() -> SsidInfo {
    let s = lock();
    let mut out = SsidInfo {
        magic: SSID_MAGIC,
        len: 0,
        ssid: [0; 12],
    };

    if let Some(w) = &s.wifi {
        if w.connected {
            out.len = w.ssid_len;
            out.ssid = w.ssid;
        }
    }

    out
}

/// Background sampler. Started once from `main`.
pub async fn refresh_loop() {
    loop {
        if let Some(wifi) = read_wifi().await {
            lock().wifi = Some(wifi);
        }

        let due = lock()
            .next_ip_check
            .map_or(true, |t| Instant::now() >= t);

        if due {
            let trace = fetch_trace().await;
            let adapter_vpn = vpn_adapter_up().await;

            let mut s = lock();
            match trace {
                Some(t) => {
                    s.ip = Some(IpInfo {
                        ip: t.ip,
                        ipv6: t.ipv6,
                        vpn: t.warp || adapter_vpn,
                    });
                    s.next_ip_check = Some(Instant::now() + IP_REFRESH);
                }
                // Keep the last known address, but try again soon rather than
                // leaving the page blank for ten minutes.
                None => s.next_ip_check = Some(Instant::now() + IP_RETRY),
            }
        }

        tokio::time::sleep(WIFI_REFRESH).await;
    }
}

/// `key: value` lookup that tolerates the leading indentation in `netsh` output.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.trim().eq_ignore_ascii_case(key) {
            Some(v.trim())
        } else {
            None
        }
    })
}

/// `key=value` lookup for the Cloudflare trace response.
fn trace_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        if k.trim().eq_ignore_ascii_case(key) {
            Some(v.trim())
        } else {
            None
        }
    })
}

fn band_code(value: &str) -> u8 {
    if value.starts_with("2.4") {
        1
    } else if value.starts_with('5') {
        2
    } else if value.starts_with('6') {
        3
    } else {
        0
    }
}

#[cfg(target_os = "windows")]
async fn read_wifi() -> Option<WifiInfo> {
    let out = Command::new("netsh")
        .args(["wlan", "show", "interfaces"])
        .output()
        .await
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&out.stdout);

    // No wireless interface at all, or the radio is off.
    let Some(state) = field(&text, "State") else {
        return None;
    };
    if !state.eq_ignore_ascii_case("connected") {
        return Some(WifiInfo::default());
    }

    let raw_ssid = field(&text, "SSID").unwrap_or("");
    let bytes = raw_ssid.as_bytes();
    let mut ssid_bytes = [0u8; 12];
    let ssid_len = bytes.len().min(ssid_bytes.len());
    ssid_bytes[..ssid_len].copy_from_slice(&bytes[..ssid_len]);

    Some(WifiInfo {
        connected: true,
        ssid: ssid_bytes,
        ssid_len: ssid_len as u8,
        signal: field(&text, "Signal")
            .and_then(|v| v.trim_end_matches('%').trim().parse().ok())
            .unwrap_or(0),
        mbps: field(&text, "Receive rate (Mbps)")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v.round() as u16)
            .unwrap_or(0),
        band: field(&text, "Band").map(band_code).unwrap_or(0),
        channel: field(&text, "Channel")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    })
}

#[cfg(not(target_os = "windows"))]
async fn read_wifi() -> Option<WifiInfo> {
    // TODO: `iw dev <if> link` on Linux, `airport -I` on macOS.
    None
}

struct Trace {
    ip: [u8; 4],
    ipv6: bool,
    warp: bool,
}

async fn fetch_trace() -> Option<Trace> {
    // Prefer the IPv4 path so the Flipper can print a dotted quad.
    if let Some(t) = curl_trace(true).await {
        return Some(t);
    }
    curl_trace(false).await
}

async fn curl_trace(force_v4: bool) -> Option<Trace> {
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "--max-time", "6"]);
    if force_v4 {
        cmd.arg("-4");
    }
    // Returns plain text: ip=, loc=, warp=, colo=...
    cmd.arg("https://one.one.one.one/cdn-cgi/trace");

    let out = tokio::time::timeout(Duration::from_secs(8), cmd.output())
        .await
        .ok()?
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let ip_raw = trace_field(&text, "ip")?;
    let warp = trace_field(&text, "warp")
        .map(|v| v == "on")
        .unwrap_or(false);

    match Ipv4Addr::from_str(ip_raw) {
        Ok(v4) => Some(Trace {
            ip: v4.octets(),
            ipv6: false,
            warp,
        }),
        Err(_) => ipv6_head(ip_raw).map(|head| Trace {
            ip: head,
            ipv6: true,
            warp,
        }),
    }
}

/// First two hextets of an IPv6 address, as raw bytes, for a shortened
/// `2600:8800` style display. Full IPv6 does not fit the screen or the frame.
fn ipv6_head(addr: &str) -> Option<[u8; 4]> {
    let mut parts = addr.split(':');
    let a = u16::from_str_radix(parts.next()?.trim(), 16).ok()?.to_be_bytes();
    let b = u16::from_str_radix(parts.next()?.trim(), 16).ok()?.to_be_bytes();
    Some([a[0], a[1], b[0], b[1]])
}

#[cfg(target_os = "windows")]
async fn vpn_adapter_up() -> bool {
    const KEYS: [&str; 11] = [
        "vpn",
        "wireguard",
        "tailscale",
        "openvpn",
        "nordlynx",
        "zerotier",
        "wintun",
        "tap-",
        "proton",
        "mullvad",
        "globalprotect",
    ];

    let script = "Get-NetAdapter | Where-Object Status -eq 'Up' \
                  | Select-Object -ExpandProperty InterfaceDescription";

    let Ok(Ok(out)) = tokio::time::timeout(
        Duration::from_secs(8),
        Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output(),
    )
    .await
    else {
        return false;
    };

    if !out.status.success() {
        return false;
    }

    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    KEYS.iter().any(|k| text.contains(*k))
}

#[cfg(not(target_os = "windows"))]
async fn vpn_adapter_up() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "fl=1\nh=one.one.one.one\nip=203.0.113.42\nts=1\nwarp=off\n";

    #[test]
    fn parses_trace_key_value() {
        assert_eq!(trace_field(TRACE, "ip"), Some("203.0.113.42"));
        assert_eq!(trace_field(TRACE, "warp"), Some("off"));
        assert_eq!(trace_field(TRACE, "missing"), None);
    }

    #[test]
    fn parses_netsh_key_colon() {
        let text = "    State                  : connected\n    Signal                 : 77%\n";
        assert_eq!(field(text, "State"), Some("connected"));
        assert_eq!(field(text, "Signal"), Some("77%"));
    }

    #[test]
    fn shortens_ipv6() {
        assert_eq!(
            ipv6_head("2600:8800:1aa2:e800::1"),
            Some([0x26, 0x00, 0x88, 0x00])
        );
        assert_eq!(ipv6_head("not-an-ip"), None);
    }

    #[test]
    fn maps_band_names() {
        assert_eq!(band_code("2.4 GHz"), 1);
        assert_eq!(band_code("5 GHz"), 2);
        assert_eq!(band_code("6 GHz"), 3);
        assert_eq!(band_code("?"), 0);
    }

    /// Hits the network on purpose. Run with:
    /// `cargo test -- --ignored live_trace_fetch`
    #[tokio::test]
    #[ignore]
    async fn live_trace_fetch() {
        let trace = fetch_trace().await.expect("trace fetch returned None");
        println!(
            "ip={} ipv6={} warp={}",
            Ipv4Addr::from(trace.ip),
            trace.ipv6,
            trace.warp
        );
    }
}
