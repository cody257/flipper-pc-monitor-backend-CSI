use btleplug::api::{Central, Manager as _, Peripheral as _};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use uuid::Uuid;

pub const FLIPPER_CHARACTERISTIC_UUID: Uuid =
    Uuid::from_u128(0x19ed82ae_ed21_4c9d_4145_228e62fe0000);

/// TX characteristic (Flipper → PC). This is the reverse channel the app
/// writes to with `ble_profile_serial_tx`. Reversed from the firmware's
/// `char_tx_uuid` byte array (`{00 00 fe 61 ...}`) into UUID order.
pub const FLIPPER_TX_CHARACTERISTIC_UUID: Uuid =
    Uuid::from_u128(0x19ed82ae_ed21_4c9d_4145_228e61fe0000);

pub async fn get_central(manager: &Manager) -> Adapter {
    manager
        .adapters()
        .await
        .unwrap()
        .into_iter()
        .nth(0)
        .unwrap()
}

pub async fn get_flipper(central: &Adapter, id: &PeripheralId) -> Option<Peripheral> {
    for p in central
        .peripherals()
        .await
        .unwrap()
        .iter()
        .filter(|p| p.id() == *id)
    {
        if p.properties()
            .await
            .unwrap()
            .unwrap()
            .local_name
            .iter()
            .any(|name| name.contains("PC Mon"))
        {
            return Some(p.clone());
        }
    }
    None
}
