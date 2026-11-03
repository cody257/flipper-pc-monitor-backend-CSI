//! Host-level extras for the dashboard: disks, uptime, battery, throughput.
//!
//! None of this is sent to the Flipper yet — it is dashboard-only for now.
//! Sampling is deliberately split by cost: throughput every tick (cheap),
//! disks and battery every 30 seconds (they barely change).

use std::path::Path;
use std::time::{Duration, Instant};

use sysinfo::{Disks, Networks};

#[derive(Clone, Default)]
pub struct HostInfo {
    pub uptime_secs: u64,
    pub rx_bps: u64,
    pub tx_bps: u64,
    pub battery: Option<BatteryInfo>,
    pub disks: Vec<DiskInfo>,
}

#[derive(Clone, Copy)]
pub struct BatteryInfo {
    pub pct: u8,
    pub charging: bool,
    pub minutes_left: u32,
}

#[derive(Clone)]
pub struct DiskInfo {
    pub name: String,
    pub total: u64,
    pub free: u64,
}

pub struct HostSampler {
    networks: Networks,
    disks: Disks,
    battery: Option<BatteryInfo>,
    last_battery: Option<Instant>,
    last_disks: Option<Instant>,
}

impl HostSampler {
    pub fn new() -> Self {
        let mut networks = Networks::new_with_refreshed_list();
        // First refresh only establishes a baseline; the counters are reset.
        networks.refresh();

        Self {
            networks,
            disks: Disks::new_with_refreshed_list(),
            battery: None,
            last_battery: None,
            last_disks: None,
        }
    }

    pub fn sample(&mut self) -> HostInfo {
        self.networks.refresh();
        let mut rx = 0u64;
        let mut tx = 0u64;
        for (_, data) in self.networks.iter() {
            rx += data.received();
            tx += data.transmitted();
        }

        if self
            .last_disks
            .map_or(true, |t| t.elapsed() >= Duration::from_secs(30))
        {
            self.disks.refresh();
            self.last_disks = Some(Instant::now());
        }

        if self
            .last_battery
            .map_or(true, |t| t.elapsed() >= Duration::from_secs(30))
        {
            self.battery = read_battery();
            self.last_battery = Some(Instant::now());
        }

        let disks = self
            .disks
            .iter()
            .filter(|d| d.total_space() > 1_000_000_000)
            .map(|d| DiskInfo {
                name: disk_name(d.mount_point()),
                total: d.total_space(),
                free: d.available_space(),
            })
            .collect();

        HostInfo {
            uptime_secs: sysinfo::System::uptime(),
            rx_bps: rx,
            tx_bps: tx,
            battery: self.battery,
            disks,
        }
    }
}

/// `C:\` becomes `C:`, `/` stays `/`.
fn disk_name(mount_point: &Path) -> String {
    let raw = mount_point.to_string_lossy();
    let trimmed = raw.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        raw.into_owned()
    } else {
        trimmed.to_string()
    }
}

fn read_battery() -> Option<BatteryInfo> {
    let manager = battery::Manager::new().ok()?;
    let mut batteries = manager.batteries().ok()?;
    let bat = batteries.next()?.ok()?;

    let pct = (bat.state_of_charge().value * 100.0).round().clamp(0.0, 100.0) as u8;
    let charging = matches!(
        bat.state(),
        battery::State::Charging | battery::State::Full
    );

    let minutes_left = bat
        .time_to_empty()
        .map(|t| (t.value / 60.0) as u32)
        .unwrap_or(0);

    Some(BatteryInfo {
        pct,
        charging,
        minutes_left,
    })
}
