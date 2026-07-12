//! Optional BLE (Bluetooth Low Energy / GATT) transport, gated behind the `ble` feature.
//!
//! Adapters like the **Veepeak OBDCheck BLE** are GATT peripherals, not serial ports —
//! macOS never gives them a `/dev/cu.*` node, which is why the serial path can't see them.
//! `btleplug` wraps CoreBluetooth (macOS), BlueZ (Linux) and WinRT (Windows).
//!
//! ## Async → sync bridge
//! btleplug is async and notification-based; the rest of obdium is synchronous and
//! `read()`-based. We own a dedicated Tokio runtime here and:
//!   - `write_all` writes to the *write* characteristic via `block_on`.
//!   - a background task subscribes to the *notify* characteristic and pushes every
//!     received byte into an `mpsc` channel that `read()` drains synchronously.
//!
//! ELM327 replies arrive across several ~20-byte notifications; because the byte channel
//! is a flat stream, `OBD::read_until('>')` reassembles them for free.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use futures::StreamExt;
use tokio::runtime::Runtime;
use uuid::Uuid;

use super::Transport;
use crate::obd::Error;

/// Well-known GATT UUIDs used by many ELM327 BLE clones (Veepeak/vLinker/etc.).
/// These are only *hints*: [`BleTransport::connect`] autodetects the real write/notify
/// characteristics from the peripheral's advertised properties and falls back to these.
mod known_uuids {
    use uuid::{uuid, Uuid};
    // 0000fff0-0000-1000-8000-00805f9b34fb service, fff1 notify, fff2 write.
    pub const SERVICE_FFF0: Uuid = uuid!("0000fff0-0000-1000-8000-00805f9b34fb");
    pub const NOTIFY_FFF1: Uuid = uuid!("0000fff1-0000-1000-8000-00805f9b34fb");
    pub const WRITE_FFF2: Uuid = uuid!("0000fff2-0000-1000-8000-00805f9b34fb");
    // Some units instead expose the ffe0 service with ffe1 as a combined read/write/notify.
    pub const SERVICE_FFE0: Uuid = uuid!("0000ffe0-0000-1000-8000-00805f9b34fb");
    pub const CHAR_FFE1: Uuid = uuid!("0000ffe1-0000-1000-8000-00805f9b34fb");

    pub const SERVICES: [Uuid; 2] = [SERVICE_FFF0, SERVICE_FFE0];
}

const SCAN_TIME: Duration = Duration::from_secs(3);

pub struct BleTransport {
    rt: Runtime,
    peripheral: Peripheral,
    write_char: Characteristic,
    /// Notifications arrive here as a flat byte stream from the background listener.
    rx: Receiver<u8>,
    /// How long `read()` waits for the next byte before reporting end-of-response.
    timeout: Duration,
    name: String,
}

impl BleTransport {
    /// Discover and connect to a BLE adapter identified by advertised name substring
    /// (case-insensitive) or peripheral id string.
    pub fn connect(identifier: &str) -> Result<Self, Error> {
        let rt = Runtime::new().map_err(|_| Error::ConnectionFailed)?;
        let (tx, rx) = mpsc::channel::<u8>();

        const STEP_TIMEOUT: Duration = Duration::from_secs(15);

        let (peripheral, write_char, notify_char, name) = rt.block_on(async {
            match tokio::time::timeout(STEP_TIMEOUT, connect_inner(identifier)).await {
                Ok(res) => res,
                Err(_) => {
                    println!("BLE connect_inner timed out after {STEP_TIMEOUT:?}");
                    Err(Error::ConnectionFailed)
                }
            }
        })?;

        // Subscribe and pump notifications into the channel for the connection lifetime.
        rt.block_on(async {
            match tokio::time::timeout(STEP_TIMEOUT, peripheral.subscribe(&notify_char)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => {
                    println!("BLE subscribe failed: {e:?}");
                    Err(Error::ConnectionFailed)
                }
                Err(_) => {
                    println!("BLE subscribe timed out after {STEP_TIMEOUT:?}");
                    Err(Error::ConnectionFailed)
                }
            }
        })?;
        spawn_notification_pump(&rt, peripheral.clone(), notify_char.uuid, tx);

        Ok(Self {
            rt,
            peripheral,
            write_char,
            rx,
            timeout: Duration::from_secs(1),
            name,
        })
    }
}

impl Transport for BleTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        // ELM327 BLE bridges accept "write without response"; some require "with response".
        let write_type = if self
            .write_char
            .properties
            .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
        {
            WriteType::WithoutResponse
        } else {
            WriteType::WithResponse
        };

        let peripheral = &self.peripheral;
        let write_char = &self.write_char;
        self.rt.block_on(async {
            peripheral
                .write(write_char, bytes, write_type)
                .await
                .map_err(|_| Error::ELM327WriteError)
        })
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        // Block up to `timeout` for the next byte; on timeout report Err so
        // `read_until` ends the response (same contract as the serial transport).
        match self.rx.recv_timeout(self.timeout) {
            Ok(byte) => {
                buf[0] = byte;
                let mut n = 1;
                // Drain whatever else is already queued to reduce per-byte overhead.
                while n < buf.len() {
                    match self.rx.try_recv() {
                        Ok(b) => {
                            buf[n] = b;
                            n += 1;
                        }
                        Err(_) => break,
                    }
                }
                Ok(n)
            }
            Err(RecvTimeoutError::Timeout) => Err(Error::ELM327ReadError),
            Err(RecvTimeoutError::Disconnected) => Err(Error::NoConnection),
        }
    }

    fn clear(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    fn name(&self) -> Option<String> {
        Some(self.name.clone())
    }

    fn baud_rate(&self) -> Option<u32> {
        None // Meaningless for BLE.
    }
}

impl Drop for BleTransport {
    fn drop(&mut self) {
        let peripheral = self.peripheral.clone();
        let _ = self.rt.block_on(async { peripheral.disconnect().await });
    }
}

/// List nearby BLE adapters as `(name, id)` pairs for a discovery UI.
///
/// This runs a short scan and returns peripherals advertising a known ELM327 service.
/// Pass `all = true` to return every discovered peripheral instead (useful for adapters
/// with nonstandard advertisements).
pub fn scan_ble_adapters(all: bool) -> Vec<(String, String)> {
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return Vec::new(),
    };

    rt.block_on(async move {
        let mut out = Vec::new();
        let Ok(manager) = Manager::new().await else {
            return out;
        };
        let Ok(adapters) = manager.adapters().await else {
            return out;
        };
        let Some(central) = adapters.into_iter().next() else {
            return out;
        };

        if central.start_scan(ScanFilter::default()).await.is_err() {
            return out;
        }
        tokio::time::sleep(SCAN_TIME).await;

        let Ok(peripherals) = central.peripherals().await else {
            return out;
        };
        for p in peripherals {
            let Ok(Some(props)) = p.properties().await else {
                continue;
            };
            let advertises_obd = props
                .services
                .iter()
                .any(|s| known_uuids::SERVICES.contains(s));
            if all || advertises_obd {
                let name = props.local_name.unwrap_or_else(|| p.id().to_string());
                out.push((name, p.id().to_string()));
            }
        }
        out
    })
}

/// Scan, match `identifier`, connect, discover services, and resolve the
/// write + notify characteristics (autodetected, with the known UUIDs as a fallback).
async fn connect_inner(
    identifier: &str,
) -> Result<(Peripheral, Characteristic, Characteristic, String), Error> {
    println!("BLE: creating manager");
    let manager = Manager::new().await.map_err(|_| Error::ConnectionFailed)?;
    let central = manager
        .adapters()
        .await
        .map_err(|_| Error::ConnectionFailed)?
        .into_iter()
        .next()
        .ok_or(Error::ConnectionFailed)?;

    println!("BLE: starting scan");
    central
        .start_scan(ScanFilter::default())
        .await
        .map_err(|_| Error::ConnectionFailed)?;
    tokio::time::sleep(SCAN_TIME).await;
    println!("BLE: scan sleep done, stopping scan");
    let _ = central.stop_scan().await;

    let peripherals = central
        .peripherals()
        .await
        .map_err(|_| Error::ConnectionFailed)?;
    println!("BLE: found {} peripherals", peripherals.len());

    let needle = identifier.to_lowercase();
    let mut chosen: Option<(Peripheral, String)> = None;
    for p in peripherals {
        let props = p.properties().await.ok().flatten();
        let name = props
            .as_ref()
            .and_then(|pr| pr.local_name.clone())
            .unwrap_or_default();
        let id = p.id().to_string();
        if name.to_lowercase().contains(&needle) || id.to_lowercase().contains(&needle) {
            let display = if name.is_empty() { id } else { name };
            chosen = Some((p, display));
            break;
        }
    }
    let (peripheral, name) = chosen.ok_or(Error::ConnectionFailed)?;
    println!("BLE: chose peripheral {name}, connecting");

    peripheral
        .connect()
        .await
        .map_err(|e| {
            println!("BLE: peripheral.connect() failed: {e:?}");
            Error::ConnectionFailed
        })?;
    println!("BLE: connected, discovering services");
    peripheral
        .discover_services()
        .await
        .map_err(|e| {
            println!("BLE: discover_services() failed: {e:?}");
            Error::ConnectionFailed
        })?;
    println!("BLE: services discovered");

    let chars = peripheral.characteristics();
    println!("BLE: {} characteristics found: {:?}", chars.len(), chars.iter().map(|c| (c.uuid, c.properties)).collect::<Vec<_>>());
    let write_char = pick_characteristic(&chars, true).ok_or(Error::ConnectionFailed)?;
    let notify_char = pick_characteristic(&chars, false).ok_or(Error::ConnectionFailed)?;
    println!("BLE: write_char={:?} notify_char={:?}", write_char.uuid, notify_char.uuid);

    Ok((peripheral, write_char, notify_char, name))
}

/// Pick the write (`want_write = true`) or notify characteristic. Prefers the known
/// Veepeak/vLinker UUIDs, then falls back to any characteristic with the right property.
fn pick_characteristic(
    chars: &std::collections::BTreeSet<Characteristic>,
    want_write: bool,
) -> Option<Characteristic> {
    let preferred: &[Uuid] = if want_write {
        &[known_uuids::WRITE_FFF2, known_uuids::CHAR_FFE1]
    } else {
        &[known_uuids::NOTIFY_FFF1, known_uuids::CHAR_FFE1]
    };
    let has_required_prop = |c: &Characteristic| {
        if want_write {
            c.properties
                .intersects(CharPropFlags::WRITE | CharPropFlags::WRITE_WITHOUT_RESPONSE)
        } else {
            c.properties
                .intersects(CharPropFlags::NOTIFY | CharPropFlags::INDICATE)
        }
    };

    // A UUID match only counts if that characteristic actually has the
    // needed property — the same UUID can appear multiple times across
    // services/instances with different capabilities.
    for &uuid in preferred {
        if let Some(c) = chars
            .iter()
            .find(|c| c.uuid == uuid && has_required_prop(c))
        {
            return Some(c.clone());
        }
    }

    chars.iter().find(|c| has_required_prop(c)).cloned()
}

/// Spawn the long-lived task that forwards notification bytes into the channel.
fn spawn_notification_pump(rt: &Runtime, peripheral: Peripheral, notify_uuid: Uuid, tx: Sender<u8>) {
    rt.spawn(async move {
        let Ok(mut stream) = peripheral.notifications().await else {
            return;
        };
        while let Some(data) = stream.next().await {
            if data.uuid != notify_uuid {
                continue;
            }
            for byte in data.value {
                // Receiver dropped => connection closed; stop pumping.
                if tx.send(byte).is_err() {
                    return;
                }
            }
        }
    });
}
