// Runtime-editable configuration, persisted to flash as a single
// magic+version+CRC record in the ESP-IDF-style `nvs` partition. Same
// storage scheme as the assistant (proven on this board); the fields are
// PrintPuck's own.

extern crate alloc;
use alloc::{string::String, string::ToString};

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

const MAGIC: [u8; 4] = *b"PUCK";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 12;
const MAX_RECORD: usize = 4096;

/// The editable string fields, in serialization order. Adding a field means
/// bumping `VERSION` (older records then read as unconfigured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    WifiSsid,
    WifiPassword,
    PrinterHost,
    PrinterSerial,
    AccessCode,
}

pub const FIELD_ORDER: [Field; 5] = [
    Field::WifiSsid,
    Field::WifiPassword,
    Field::PrinterHost,
    Field::PrinterSerial,
    Field::AccessCode,
];

impl Field {
    /// Stable identifier used as the HTML form field name.
    pub const fn key(self) -> &'static str {
        match self {
            Field::WifiSsid => "wifi_ssid",
            Field::WifiPassword => "wifi_password",
            Field::PrinterHost => "printer_host",
            Field::PrinterSerial => "printer_serial",
            Field::AccessCode => "access_code",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Field::WifiSsid => "Wi-Fi network (SSID)",
            Field::WifiPassword => "Wi-Fi password",
            Field::PrinterHost => "Printer IP address",
            Field::PrinterSerial => "Printer serial number",
            Field::AccessCode => "Printer access code",
        }
    }

    pub const fn hint(self) -> &'static str {
        match self {
            Field::WifiSsid => "network name",
            Field::WifiPassword => "blank for an open network",
            Field::PrinterHost => "192.168.1.42",
            Field::PrinterSerial => "on the printer's settings page",
            Field::AccessCode => "8 digits, printer settings > network",
        }
    }

    /// Secrets are never echoed back into the setup form.
    pub const fn is_secret(self) -> bool {
        matches!(self, Field::WifiPassword | Field::AccessCode)
    }

    pub fn accepts(self, value: &str) -> bool {
        if value.is_empty() {
            return !self.is_required();
        }
        true
    }

    /// Whether the firmware can do anything at all without this field. The
    /// Wi-Fi password is optional (open networks are legitimate); everything
    /// else is required for local MQTT.
    pub const fn is_required(self) -> bool {
        !matches!(self, Field::WifiPassword)
    }
}

#[derive(Clone)]
pub struct Settings {
    fields: [String; FIELD_ORDER.len()],
}

impl Settings {
    /// A blank device: no network, no printer. Deliberately unusable -
    /// `first_invalid()` reports a blank required field, so `main` sends it
    /// to the setup portal.
    pub fn unconfigured() -> Self {
        Self {
            fields: [const { String::new() }; FIELD_ORDER.len()],
        }
    }

    pub fn first_invalid(&self) -> Option<Field> {
        FIELD_ORDER.into_iter().find(|f| !f.accepts(self.get(*f)))
    }

    fn index(field: Field) -> usize {
        FIELD_ORDER.iter().position(|f| *f == field).expect("field is in FIELD_ORDER")
    }

    pub fn get(&self, field: Field) -> &str {
        &self.fields[Self::index(field)]
    }

    pub fn set(&mut self, field: Field, value: impl Into<String>) {
        let value: String = value.into();
        self.fields[Self::index(field)] = value.trim().to_string();
    }

    pub fn wifi_ssid(&self) -> &str {
        self.get(Field::WifiSsid)
    }
    pub fn wifi_password(&self) -> &str {
        self.get(Field::WifiPassword)
    }

    /// MQTT topic the printer publishes status reports to.
    pub fn report_topic(&self) -> String {
        alloc::format!("device/{}/report", self.get(Field::PrinterSerial))
    }

    /// MQTT topic commands are published to.
    pub fn request_topic(&self) -> String {
        alloc::format!("device/{}/request", self.get(Field::PrinterSerial))
    }

    /// Serializes into `out` (which must be internal RAM - see `SCRATCH`) and
    /// returns the used length, already padded to a 4-byte flash word.
    fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        let mut cursor = HEADER_LEN;
        for field in FIELD_ORDER {
            let bytes = self.get(field).as_bytes();
            let len = u16::try_from(bytes.len()).ok()?;
            out.get_mut(cursor..cursor + 2)?.copy_from_slice(&len.to_le_bytes());
            cursor += 2;
            out.get_mut(cursor..cursor + bytes.len())?.copy_from_slice(bytes);
            cursor += bytes.len();
        }
        let payload_len = u16::try_from(cursor - HEADER_LEN).ok()?;
        let crc = crc32(&out[HEADER_LEN..cursor]);

        out[..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        out[5] = 0; // reserved
        out[6..8].copy_from_slice(&payload_len.to_le_bytes());
        out[8..12].copy_from_slice(&crc.to_le_bytes());

        let padded = cursor.next_multiple_of(4);
        out.get_mut(cursor..padded)?.fill(0);
        Some(padded)
    }

    fn decode(record: &[u8]) -> Option<Self> {
        if record.len() < HEADER_LEN || record[..4] != MAGIC || record[4] != VERSION {
            return None;
        }
        let payload_len = u16::from_le_bytes([record[6], record[7]]) as usize;
        let expected_crc = u32::from_le_bytes([record[8], record[9], record[10], record[11]]);
        let payload = record.get(HEADER_LEN..HEADER_LEN + payload_len)?;
        if crc32(payload) != expected_crc {
            return None;
        }

        let mut settings = Self::unconfigured();
        let mut cursor = 0usize;
        for field in FIELD_ORDER {
            let len_bytes = payload.get(cursor..cursor + 2)?;
            let len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
            cursor += 2;
            let bytes = payload.get(cursor..cursor + len)?;
            cursor += len;
            settings.fields[Self::index(field)] = core::str::from_utf8(bytes).ok()?.to_string();
        }
        Some(settings)
    }
}

/// Bitwise CRC-32 (IEEE).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// Flash scratch buffers must live in internal RAM: esp-storage disables the
// instruction cache during flash ops and PSRAM is reached through that cache.
// See the assistant's settings.rs for the full story; identical constraint.
static mut SCRATCH: [u8; MAX_RECORD] = [0; MAX_RECORD];
static mut TABLE_SCRATCH: [u8; 1024] = [0; 1024];

/// Where in flash the settings record lives, resolved once from the partition
/// table.
#[derive(Clone, Copy)]
pub struct Store {
    offset: u32,
    size: u32,
}

impl Store {
    /// Locates the `nvs` data partition. Returns `None` if the partition
    /// table has no NVS entry (persistence disabled).
    pub fn locate<F: embedded_storage::Storage>(flash: &mut F) -> Option<Self> {
        use esp_bootloader_esp_idf::partitions;

        let table_buf = unsafe { &mut *core::ptr::addr_of_mut!(TABLE_SCRATCH) };
        let table = partitions::read_partition_table(flash, table_buf).ok()?;
        let entry = table
            .find_partition(partitions::PartitionType::Data(
                partitions::DataPartitionSubType::Nvs,
            ))
            .ok()??;
        Some(Self { offset: entry.offset(), size: entry.len() })
    }

    pub fn load<F: ReadNorFlash>(&self, flash: &mut F) -> Option<Settings> {
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let len = MAX_RECORD.min(self.size as usize);
        flash.read(self.offset, &mut buf[..len]).ok()?;
        Settings::decode(&buf[..len])
    }

    pub fn save<F: NorFlash>(&self, flash: &mut F, settings: &Settings) -> Result<(), &'static str> {
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let len = settings.encode_into(buf).ok_or("settings too large to store")?;
        let sector = F::ERASE_SIZE;
        let erase_len = len.next_multiple_of(sector) as u32;
        if erase_len > self.size {
            return Err("settings record exceeds nvs partition");
        }
        flash
            .erase(self.offset, self.offset + erase_len)
            .map_err(|_| "flash erase failed")?;
        flash.write(self.offset, &buf[..len]).map_err(|_| "flash write failed")?;
        Ok(())
    }

    /// Wipes the stored record; the next boot reads as unconfigured and goes
    /// to the setup portal.
    pub fn clear<F: NorFlash>(&self, flash: &mut F) -> Result<(), &'static str> {
        let sector = F::ERASE_SIZE as u32;
        flash
            .erase(self.offset, self.offset + sector)
            .map_err(|_| "flash erase failed")
    }
}
