//! Transport abstraction for talking to an ELM327 adapter.
//!
//! Historically obdium spoke only to serial ports (`Box<dyn serialport::SerialPort>`).
//! BLE adapters (e.g. the Veepeak OBDCheck BLE) are *not* exposed as serial ports on
//! macOS, so we abstract the byte pipe behind [`Transport`]. Serial remains the default;
//! BLE is an optional, additive backend gated behind the `ble` cargo feature.

use std::time::Duration;

use crate::obd::Error;

mod serial;
pub use serial::SerialTransport;

#[cfg(feature = "ble")]
mod ble;
#[cfg(feature = "ble")]
pub use ble::{scan_ble_adapters, BleTransport};

/// A bidirectional byte pipe to an ELM327 adapter.
///
/// This is intentionally the *minimal* surface `OBD` needs. All the higher-level
/// framing (`read_until('>')`, ISO-TP reassembly, AT/PID parsing) stays in `obd.rs`
/// and works identically regardless of which transport is underneath.
pub trait Transport: Send {
    /// Write a full command to the adapter. `bytes` already includes the trailing `\r`.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error>;

    /// Read available bytes into `buf`.
    ///
    /// Return semantics mirror the old serial loop so `read_until` is unchanged:
    /// - `Ok(n > 0)` — bytes were read.
    /// - `Ok(0)`     — nothing available right now; caller should back off and retry.
    /// - `Err(_)`    — timeout / disconnect; caller treats this as end-of-response.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error>;

    /// Drop any buffered input (equivalent to `ClearBuffer::All` on serial).
    fn clear(&mut self);

    /// Set the per-read timeout. No-op for transports that don't have one.
    fn set_timeout(&mut self, timeout: Duration);

    /// Human-readable identifier (serial port path, BLE name/UUID, or "DEMO MODE").
    fn name(&self) -> Option<String>;

    /// Line speed in baud. `None` for transports where it's meaningless (BLE, demo).
    fn baud_rate(&self) -> Option<u32>;
}

/// Fake transport used for DEMO MODE / replay. Never actually reads or writes;
/// `OBD` short-circuits I/O when `replay_requests` is set.
pub struct DummyTransport;

impl Transport for DummyTransport {
    fn write_all(&mut self, _bytes: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Error> {
        Ok(0)
    }
    fn clear(&mut self) {}
    fn set_timeout(&mut self, _timeout: Duration) {}
    fn name(&self) -> Option<String> {
        Some("DEMO MODE".to_string())
    }
    fn baud_rate(&self) -> Option<u32> {
        Some(0)
    }
}
