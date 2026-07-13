//! Minimal DBC (CAN database) parser + decoder, for using
//! [opendbc](https://github.com/commaai/opendbc) signal definitions.
//!
//! opendbc ships `.dbc` files describing how to decode a platform's raw CAN
//! messages — far more than the generic OBD-II PIDs. Generic OBD is
//! request/response (which the rest of this server speaks); raw CAN is a passive
//! broadcast stream. This module does the data-format half: parse the `BO_`
//! (message) / `SG_` (signal) definitions and decode a CAN frame you supply
//! (e.g. captured via an ELM327 monitor mode or another CAN tool) into physical
//! values. It does not itself sniff the bus.
//!
//! Bit layout follows the DBC convention: `@1` = little-endian (Intel), `@0` =
//! big-endian (Motorola); `+`/`-` = unsigned/signed; physical = raw*factor +
//! offset.

#[derive(Clone, Debug)]
pub struct Signal {
    pub name: String,
    pub start_bit: u32,
    pub length: u32,
    pub little_endian: bool,
    pub signed: bool,
    pub factor: f64,
    pub offset: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: String,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: u32,
    pub name: String,
    pub dlc: u32,
    pub signals: Vec<Signal>,
}

impl Message {
    /// The 11/29-bit arbitration id (DBC sets bit 31 to flag extended frames).
    pub fn arbitration_id(&self) -> u32 {
        self.id & 0x1FFF_FFFF
    }
    pub fn extended(&self) -> bool {
        self.id & 0x8000_0000 != 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct Dbc {
    pub messages: Vec<Message>,
}

impl Dbc {
    pub fn parse(text: &str) -> Dbc {
        let mut messages: Vec<Message> = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if let Some(rest) = line.strip_prefix("BO_ ") {
                if let Some(msg) = parse_message_header(rest) {
                    messages.push(msg);
                }
            } else if line.starts_with("SG_ ") {
                if let Some(sig) = parse_signal(line) {
                    if let Some(last) = messages.last_mut() {
                        last.signals.push(sig);
                    }
                }
            }
        }
        Dbc { messages }
    }

    pub fn message_by_id(&self, can_id: u32) -> Option<&Message> {
        let id = can_id & 0x1FFF_FFFF;
        self.messages.iter().find(|m| m.arbitration_id() == id)
    }

    /// Decode a CAN frame into (signal name, raw, physical value).
    pub fn decode(&self, can_id: u32, data: &[u8]) -> Option<Vec<DecodedSignal>> {
        let msg = self.message_by_id(can_id)?;
        Some(
            msg.signals
                .iter()
                .map(|s| {
                    let raw = extract_bits(data, s.start_bit, s.length, s.little_endian);
                    let raw_signed = if s.signed {
                        sign_extend(raw, s.length)
                    } else {
                        raw as i64
                    };
                    DecodedSignal {
                        name: s.name.clone(),
                        raw: raw_signed,
                        value: raw_signed as f64 * s.factor + s.offset,
                        unit: s.unit.clone(),
                    }
                })
                .collect(),
        )
    }
}

#[derive(Clone, Debug)]
pub struct DecodedSignal {
    pub name: String,
    pub raw: i64,
    pub value: f64,
    pub unit: String,
}

/// `BO_ <id> <name>: <dlc> <transmitter>`
fn parse_message_header(rest: &str) -> Option<Message> {
    let mut it = rest.split_whitespace();
    let id: u32 = it.next()?.parse().ok()?;
    let name = it.next()?.trim_end_matches(':').to_string();
    let dlc: u32 = it.next()?.parse().ok()?;
    Some(Message {
        id,
        name,
        dlc,
        signals: Vec::new(),
    })
}

/// ` SG_ <name> [mux] : <start>|<len>@<order><sign> (<factor>,<offset>) [<min>|<max>] "<unit>" <recv>`
fn parse_signal(line: &str) -> Option<Signal> {
    let after = line.strip_prefix("SG_ ")?;
    let colon = after.find(':')?;
    let name = after[..colon].split_whitespace().next()?.to_string();
    let rest = after[colon + 1..].trim();

    let mut it = rest.split_whitespace();
    let bitdef = it.next()?; // e.g. "15|11@0-"
    let factor_offset = it.next()?; // e.g. "(1,0)"
    let minmax = it.next()?; // e.g. "[-32768|32767]"

    // bitdef: start|len@order sign
    let (start_s, tail) = bitdef.split_once('|')?;
    let (len_s, ordersign) = tail.split_once('@')?;
    let start_bit: u32 = start_s.parse().ok()?;
    let length: u32 = len_s.parse().ok()?;
    let order_char = ordersign.chars().next()?;
    let sign_char = ordersign.chars().nth(1)?;
    let little_endian = order_char == '1';
    let signed = sign_char == '-';

    // (factor,offset)
    let fo = factor_offset.trim_start_matches('(').trim_end_matches(')');
    let (factor_s, offset_s) = fo.split_once(',')?;
    let factor: f64 = factor_s.parse().ok()?;
    let offset: f64 = offset_s.parse().ok()?;

    // [min|max]
    let mm = minmax.trim_start_matches('[').trim_end_matches(']');
    let (min, max) = match mm.split_once('|') {
        Some((a, b)) => (a.parse().ok(), b.parse().ok()),
        None => (None, None),
    };

    // "unit" — take the text between the first pair of quotes.
    let unit = match (rest.find('"'), rest.rfind('"')) {
        (Some(a), Some(b)) if b > a => rest[a + 1..b].to_string(),
        _ => String::new(),
    };

    Some(Signal {
        name,
        start_bit,
        length,
        little_endian,
        signed,
        factor,
        offset,
        min,
        max,
        unit,
    })
}

/// Extract `length` bits from `data`, honoring DBC bit ordering.
fn extract_bits(data: &[u8], start: u32, length: u32, little_endian: bool) -> u64 {
    if length == 0 || length > 64 {
        return 0;
    }
    let mask = if length == 64 {
        u64::MAX
    } else {
        (1u64 << length) - 1
    };

    if little_endian {
        // Intel: assemble a little-endian 64-bit view, shift, mask.
        let mut acc: u64 = 0;
        for (i, b) in data.iter().enumerate().take(8) {
            acc |= (*b as u64) << (8 * i);
        }
        (acc >> start) & mask
    } else {
        // Motorola: `start` is the MSB in DBC sawtooth numbering. Read bits
        // MSB-first across an MSB-ordered bit stream (bit 0 = MSB of byte 0).
        let start_msb = (start / 8) * 8 + (7 - (start % 8));
        let mut value: u64 = 0;
        for i in 0..length {
            value = (value << 1) | get_bit_msb(data, start_msb + i) as u64;
        }
        value
    }
}

/// Bit at MSB-stream index (0 = MSB of byte 0, 7 = LSB of byte 0, 8 = MSB of byte 1…).
fn get_bit_msb(data: &[u8], msb_index: u32) -> u8 {
    let byte = (msb_index / 8) as usize;
    let bit = 7 - (msb_index % 8);
    match data.get(byte) {
        Some(b) => (b >> bit) & 1,
        None => 0,
    }
}

fn sign_extend(raw: u64, length: u32) -> i64 {
    if length == 0 || length >= 64 {
        return raw as i64;
    }
    let sign_bit = 1u64 << (length - 1);
    if raw & sign_bit != 0 {
        (raw as i64) - (1i64 << length)
    } else {
        raw as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
BO_ 100 SPEED: 8 ECU
 SG_ VEHICLE_SPEED : 0|8@1+ (1,0) [0|255] "km/h" DRIVER
 SG_ ACCEL : 8|8@1- (1,0) [-128|127] "" DRIVER

BO_ 200 ENGINE: 8 ECU
 SG_ RPM : 7|16@0+ (0.25,0) [0|16383] "rpm" DRIVER
"#;

    #[test]
    fn parses_messages_and_signals() {
        let dbc = Dbc::parse(SAMPLE);
        assert_eq!(dbc.messages.len(), 2);
        let speed = &dbc.messages[0];
        assert_eq!(speed.id, 100);
        assert_eq!(speed.name, "SPEED");
        assert_eq!(speed.signals.len(), 2);
        assert_eq!(speed.signals[0].name, "VEHICLE_SPEED");
        assert_eq!(speed.signals[0].unit, "km/h");
        assert!(speed.signals[0].little_endian);
        assert!(!speed.signals[0].signed);
        assert!(speed.signals[1].signed);
    }

    #[test]
    fn decodes_little_endian_unsigned() {
        let dbc = Dbc::parse(SAMPLE);
        // byte0 = 0x50 = 80 km/h
        let decoded = dbc.decode(100, &[0x50, 0x00, 0, 0, 0, 0, 0, 0]).unwrap();
        let speed = decoded.iter().find(|s| s.name == "VEHICLE_SPEED").unwrap();
        assert_eq!(speed.raw, 80);
        assert_eq!(speed.value, 80.0);
    }

    #[test]
    fn decodes_little_endian_signed() {
        let dbc = Dbc::parse(SAMPLE);
        // byte1 = 0x80 -> signed 8-bit = -128
        let decoded = dbc.decode(100, &[0x00, 0x80, 0, 0, 0, 0, 0, 0]).unwrap();
        let accel = decoded.iter().find(|s| s.name == "ACCEL").unwrap();
        assert_eq!(accel.raw, -128);
        assert_eq!(accel.value, -128.0);
    }

    #[test]
    fn decodes_big_endian_with_scale() {
        let dbc = Dbc::parse(SAMPLE);
        // RPM big-endian 16-bit at start 7 = bytes 0..1 MSB-first. 0x0FA0 = 4000
        // raw * 0.25 = 1000 rpm.
        let decoded = dbc.decode(200, &[0x0F, 0xA0, 0, 0, 0, 0, 0, 0]).unwrap();
        let rpm = decoded.iter().find(|s| s.name == "RPM").unwrap();
        assert_eq!(rpm.raw, 4000);
        assert_eq!(rpm.value, 1000.0);
    }

    #[test]
    fn extended_flag_and_id_masking() {
        let dbc = Dbc::parse("BO_ 2364540158 X: 8 XXX\n SG_ A : 0|8@1+ (1,0) [0|0] \"\" X\n");
        let m = &dbc.messages[0];
        assert!(m.extended());
        // Same signal is reachable by the masked arbitration id.
        assert!(dbc.message_by_id(m.arbitration_id()).is_some());
    }

    #[test]
    fn unknown_message_id_returns_none() {
        let dbc = Dbc::parse(SAMPLE);
        assert!(dbc.decode(999, &[0; 8]).is_none());
    }
}
