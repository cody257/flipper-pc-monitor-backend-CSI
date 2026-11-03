//! Tiny local HTTP server for the browser dashboard.
//!
//! Deliberately minimal: no framework, no routing table, two endpoints.
//! `/` serves one self-contained HTML file (no CDN, works offline) and
//! `/api` returns the current snapshot as JSON. Bound to loopback by default
//! so it is not exposed on the local network.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::net_info::{SsidInfo, FLAG_HAS_IP, FLAG_IPV6, FLAG_VPN, FLAG_WIFI};
use crate::telemetry;

const INDEX: &str = include_str!("dashboard.html");

pub async fn serve(addr: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    println!("Dashboard: http://{}", addr);

    loop {
        let (mut socket, _) = listener.accept().await?;

        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let n = match socket.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };

            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request.split_whitespace().nth(1).unwrap_or("/");

            let (status, content_type, body) = match path {
                "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX.to_string()),
                "/api" => ("200 OK", "application/json", api_json()),
                _ => ("404 Not Found", "text/plain; charset=utf-8", "not found".into()),
            };

            let response = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: {content_type}\r\n\
                 Content-Length: {}\r\n\
                 Cache-Control: no-store\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );

            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn unit_str(unit: &[u8; 4]) -> String {
    let end = unit.iter().position(|&b| b == 0).unwrap_or(4);
    String::from_utf8_lossy(&unit[..end]).into_owned()
}

fn ip_string(flags: u8, ip: &[u8; 4]) -> String {
    if flags & FLAG_HAS_IP != 0 {
        format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
    } else if flags & FLAG_IPV6 != 0 {
        format!("{:02x}{:02x}:{:02x}{:02x}", ip[0], ip[1], ip[2], ip[3])
    } else {
        "n/a".to_string()
    }
}

fn ssid_string(ssid: &SsidInfo) -> String {
    let len = (ssid.len as usize).min(ssid.ssid.len());
    String::from_utf8_lossy(&ssid.ssid[..len]).into_owned()
}

fn api_json() -> String {
    let Some(snap) = telemetry::latest() else {
        return serde_json::json!({ "ready": false }).to_string();
    };

    let s = &snap.system;
    let n = &snap.net;

    let ram_total = s.ram_max as f64 * 0.1;
    let ram_used = ram_total * s.ram_usage as f64 / 100.0;
    let vram_total = s.vram_max as f64 * 0.1;
    let vram_used = vram_total * s.vram_usage as f64 / 100.0;

    // 255 is the sentinel the GPU code uses for "no data".
    let has_gpu = s.gpu_usage < u8::MAX;

    let gpu = if has_gpu {
        serde_json::json!(s.gpu_usage)
    } else {
        serde_json::Value::Null
    };

    let vram = if has_gpu {
        serde_json::json!({
            "pct": s.vram_usage,
            "used": round1(vram_used),
            "total": round1(vram_total),
            "unit": unit_str(&s.vram_unit),
        })
    } else {
        serde_json::Value::Null
    };

    let host = &snap.host;

    let battery = match host.battery {
        Some(b) => serde_json::json!({
            "pct": b.pct,
            "charging": b.charging,
            "minutes_left": b.minutes_left,
        }),
        None => serde_json::Value::Null,
    };

    const GIB: f64 = 1_073_741_824.0;
    let disks: Vec<serde_json::Value> = host
        .disks
        .iter()
        .map(|d| {
            let used = d.total.saturating_sub(d.free);
            let pct = if d.total > 0 {
                (used as f64 / d.total as f64 * 100.0).round() as u8
            } else {
                0
            };
            serde_json::json!({
                "name": d.name,
                "used": round1(used as f64 / GIB),
                "total": round1(d.total as f64 / GIB),
                "pct": pct,
            })
        })
        .collect();

    serde_json::json!({
        "ready": true,
        "ts": snap.ts,
        "uptime": host.uptime_secs,
        "battery": battery,
        "disks": disks,
        "throughput": { "rx_bps": host.rx_bps, "tx_bps": host.tx_bps },
        "cpu": s.cpu_usage,
        "ram": {
            "pct": s.ram_usage,
            "used": round1(ram_used),
            "total": round1(ram_total),
            "unit": unit_str(&s.ram_unit),
        },
        "gpu": gpu,
        "vram": vram,
        "net": {
            "wifi": n.flags & FLAG_WIFI != 0,
            "signal": n.wifi_signal,
            "mbps": n.wifi_mbps,
            "band": n.band,
            "channel": n.channel,
            "ssid": ssid_string(&snap.ssid),
            "ip": ip_string(n.flags, &n.ip),
            "vpn": n.flags & FLAG_VPN != 0,
        },
    })
    .to_string()
}
