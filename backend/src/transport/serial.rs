//! Default serial (UART/USB/Bluetooth-Classic-SPP) transport.
//!
//! This is a thin wrapper over `serialport` that preserves obdium's original
//! behavior exactly — it is the default backend.

use std::io::Write;
use std::time::Duration;

use serialport::SerialPort;

use super::Transport;
use crate::obd::Error;

pub struct SerialTransport {
    port: Box<dyn SerialPort>,
}

impl SerialTransport {
    /// Open a serial port at the given baud rate. Returns `None` on failure so the
    /// caller can keep the original `.ok()` semantics.
    pub fn open(port: &str, baud_rate: u32) -> Option<Self> {
        serialport::new(port, baud_rate)
            .timeout(Duration::from_secs(1))
            .open()
            .ok()
            .map(|port| Self { port })
    }
}

impl Transport for SerialTransport {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.port
            .write_all(bytes)
            .map_err(|_| Error::ELM327WriteError)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        // Timeouts surface as Err here; `read_until` treats that as end-of-response,
        // exactly as the old inline serial loop did.
        self.port.read(buf).map_err(|_| Error::ELM327ReadError)
    }

    fn clear(&mut self) {
        let _ = self.port.clear(serialport::ClearBuffer::All);
    }

    fn set_timeout(&mut self, timeout: Duration) {
        let _ = self.port.set_timeout(timeout);
    }

    fn name(&self) -> Option<String> {
        self.port.name()
    }

    fn baud_rate(&self) -> Option<u32> {
        self.port.baud_rate().ok()
    }
}
