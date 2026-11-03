//! One sampler, many readers.
//!
//! Before this module the PC was only sampled while a Flipper was connected,
//! which meant a dashboard would have had to duplicate the work (two NVML /
//! sysinfo passes per second). Instead everything is collected once into a
//! snapshot, and both the BLE sender and the web server read that snapshot.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::host_info::{self, HostInfo};
use crate::net_info::{self, NetInfo, SsidInfo};
use crate::system_info::SystemInfo;

#[derive(Clone)]
pub struct Snapshot {
    pub system: SystemInfo,
    pub net: NetInfo,
    pub ssid: SsidInfo,
    pub host: HostInfo,
    pub ts: u64,
}

static LATEST: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Cheap copy of the latest sample. Returns `None` until the first tick.
pub fn latest() -> Option<Snapshot> {
    LATEST.lock().expect("sampler mutex").clone()
}

pub async fn sampler() {
    let mut system_info = sysinfo::System::new_all();
    let mut host = host_info::HostSampler::new();

    loop {
        let system = SystemInfo::get_system_info(&mut system_info).await;
        let net = net_info::current();
        let ssid = net_info::current_ssid();
        let host = host.sample();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        {
            let mut slot = LATEST.lock().expect("sampler mutex");
            *slot = Some(Snapshot {
                system,
                net,
                ssid,
                host,
                ts,
            });
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
