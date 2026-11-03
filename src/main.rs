use btleplug::api::{Central, CentralEvent, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::stream::StreamExt;
use std::collections::HashMap;
use std::error::Error;

mod commands;
mod flipper_manager;
mod helpers;
mod host_info;
mod net_info;
mod system_info;
mod telemetry;
mod web;

async fn data_sender(flipper: Peripheral) {
    let id = flipper.id();
    let chars = flipper.characteristics();
    let cmd_char = match chars
        .iter()
        .find(|c| c.uuid == flipper_manager::FLIPPER_CHARACTERISTIC_UUID)
    {
        Some(c) => c,
        None => {
            return println!("[{}] Failed to find characteristic", id.to_string());
        }
    };
    println!("[{}] Sending data...", id.to_string());

    // Everything is sampled once per second by telemetry::sampler; this task
    // only forwards the latest snapshot. Sampling used to live here, which
    // meant it stopped whenever no Flipper was connected.
    loop {
        if let Some(snapshot) = telemetry::latest() {
            if let Ok(bytes) = bincode::serialize(&snapshot.system) {
                if let Err(e) = flipper
                    .write(
                        cmd_char,
                        &bytes,
                        btleplug::api::WriteType::WithoutResponse,
                    )
                    .await
                {
                    println!("[{}] Failed to write: {}", id.to_string(), e);
                }
            }

            // Second frame: the network page. Sent as its own message so an
            // app that does not know about it simply ignores it. Small gap
            // keeps the two notifications from arriving as one callback.
            if let Ok(bytes) = bincode::serialize(&snapshot.net) {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                if let Err(e) = flipper
                    .write(
                        cmd_char,
                        &bytes,
                        btleplug::api::WriteType::WithoutResponse,
                    )
                    .await
                {
                    println!("[{}] Failed to write net frame: {}", id.to_string(), e);
                }
            }

            // Third frame: SSID, which does not fit in the 11-byte net frame.
            if let Ok(bytes) = bincode::serialize(&snapshot.ssid) {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                if let Err(e) = flipper
                    .write(
                        cmd_char,
                        &bytes,
                        btleplug::api::WriteType::WithoutResponse,
                    )
                    .await
                {
                    println!("[{}] Failed to write ssid frame: {}", id.to_string(), e);
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(880)).await;
    }
}

async fn reconnect_thread(central: Adapter, id: PeripheralId) {
    loop {
        if let Some(flipper) = flipper_manager::get_flipper(&central, &id).await {
            let _ = flipper.connect().await;
        };

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn command_receiver(flipper: Peripheral) {
    let id = flipper.id();

    let mut notifications = match flipper.notifications().await {
        Ok(stream) => stream,
        Err(e) => {
            println!("[{}] Failed to read notifications: {}", id.to_string(), e);
            return;
        }
    };

    println!("[{}] Listening for commands", id.to_string());

    while let Some(notification) = notifications.next().await {
        commands::handle(&notification.value);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init();
    // std::env::set_var("RUST_BACKTRACE", "full");

    let manager = Manager::new().await?;

    // Sample Wi-Fi / public IP on their own schedule, independent of sending.
    tokio::spawn(net_info::refresh_loop());

    // One sampler shared by the BLE sender and the web dashboard.
    tokio::spawn(telemetry::sampler());

    let port = std::env::var("PCM_WEB_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8099);
    let web_addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    tokio::spawn(async move {
        if let Err(e) = web::serve(web_addr).await {
            println!("Dashboard unavailable on {}: {}", web_addr, e);
        }
    });

    let central = flipper_manager::get_central(&manager).await;
    println!("Found {:?} adapter", central.adapter_info().await.unwrap());

    let mut events = central.events().await?;

    println!("Scanning... Launch PC Monitor app on Flipper");
    central.start_scan(ScanFilter::default()).await?;

    let mut data_workers: HashMap<PeripheralId, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut reconnect_workers: HashMap<PeripheralId, tokio::task::JoinHandle<()>> = HashMap::new();

    while let Some(event) = events.next().await {
        match event {
            CentralEvent::DeviceDiscovered(id) => {
                if let Some(flp) = flipper_manager::get_flipper(&central, &id).await {
                    println!("[{}] Connecting to Flipper", &id.to_string());
                    if let Err(e) = flp.connect().await {
                        println!(
                            "[{}] Failed to connect to Flipper: {}",
                            id.to_string(),
                            e.to_string()
                        );
                    }
                }
            }
            CentralEvent::DeviceConnected(id) => {
                if let Some(flp) = flipper_manager::get_flipper(&central, &id).await {
                    flp.discover_services().await?;
                    println!("[{}] Connected to Flipper", &id.to_string());

                    // Reverse channel: subscribe to the Flipper's TX so button
                    // presses arrive here.
                    let tx = flp
                        .characteristics()
                        .iter()
                        .find(|c| c.uuid == flipper_manager::FLIPPER_TX_CHARACTERISTIC_UUID)
                        .cloned();

                    match tx {
                        Some(tx) => match flp.subscribe(&tx).await {
                            Ok(()) => {
                                tokio::spawn(command_receiver(flp.clone()));
                            }
                            Err(e) => {
                                println!("[{}] Failed to subscribe to TX: {}", id.to_string(), e)
                            }
                        },
                        None => println!("[{}] TX characteristic not found", id.to_string()),
                    }

                    data_workers.insert(id.clone(), tokio::spawn(data_sender(flp)));
                };

                match reconnect_workers.get(&id) {
                    Some(worker) => {
                        worker.abort();
                        reconnect_workers.remove(&id);
                    }
                    None => {}
                }
            }
            CentralEvent::DeviceDisconnected(id) => {
                match data_workers.get(&id) {
                    Some(worker) => {
                        worker.abort();
                        println!(
                            "[{}] Disconnected from Flipper. Waiting for reconnection",
                            &id.to_string()
                        );

                        data_workers.remove(&id);
                    }
                    None => {}
                };

                reconnect_workers.insert(
                    id.clone(),
                    tokio::spawn(reconnect_thread(central.clone(), id)),
                );
            }
            _ => {}
        }
    }
    Ok(())
}
