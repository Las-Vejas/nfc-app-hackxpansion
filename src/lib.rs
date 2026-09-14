#![no_std]

//! NFC Writer — an app for the Hackxpansion console's `nfcxpansion` module.
//!
//! Writes an NDEF record onto the ST25DV04K so a phone tapped against the
//! module opens a link, and reads back whatever is on the tag so you can see
//! what a phone (e.g. NFC Tools) put there.

extern crate alloc;

use alloc::{boxed::Box, string::String};
use core::{fmt::Write as _, future::Future, pin::Pin};

use xpanse_api::{
    app::App,
    interfaces::{
        buttons::{A, B, Button, X, Y},
        nfc::{Nfc, NfcError},
    },
    reexports::{
        defmt,
        embassy_futures::select::{Either5, select5},
        embassy_time::{Duration, Ticker, Timer},
        slint::{self, ComponentHandle, SharedString},
    },
    registry::{Registry, ResourceLease},
};

// Pulls in the components compiled from `ui/nfc_writer.slint` by build.rs.
slint::include_modules!();

// ---------------------------------------------------------------------------
// Tag layout
// ---------------------------------------------------------------------------

/// Capability Container for the ST25DV04K: 4-byte CC, version 1.0,
/// read/write always granted, MLEN 0x40 (64 * 8 = 512 bytes).
const CC_FILE: [u8; 4] = [0xE1, 0x40, 0x40, 0x05];

const TLV_NDEF: u8 = 0x03;
const TLV_TERMINATOR: u8 = 0xFE;

/// User EEPROM on the ST25DV04K. The CC file lives at the very start.
const USER_MEMORY_LEN: usize = 512;

/// Conservative write chunk. The ST25DV's I2C page is larger than this, but
/// staying inside an aligned 16-byte window is safe whatever the page size,
/// and the messages we write are tiny anyway.
const WRITE_CHUNK: usize = 16;

/// An EEPROM write cycle. The chip NACKs its own address until it finishes,
/// and the driver does no acknowledge-polling, so we have to wait it out.
const WRITE_CYCLE_MS: u64 = 6;

/// NFC Forum URI abbreviation codes. Index = the first payload byte.
const URI_PREFIX: [&str; 0x24] = [
    "",
    "http://www.",
    "https://www.",
    "http://",
    "https://",
    "tel:",
    "mailto:",
    "ftp://anonymous:anonymous@",
    "ftp://ftp.",
    "ftps://",
    "sftp://",
    "smb://",
    "nfs://",
    "ftp://",
    "dav://",
    "news:",
    "telnet://",
    "imap:",
    "rtsp://",
    "urn:",
    "pop:",
    "sip:",
    "sips:",
    "tftp:",
    "btspp://",
    "btl2cap://",
    "btgoep://",
    "tcpobex://",
    "irdaobex://",
    "file://",
    "urn:epc:id:",
    "urn:epc:tag:",
    "urn:epc:pat:",
    "urn:epc:raw:",
    "urn:epc:",
    "urn:nfc:",
];

/// What kind of NDEF record a preset produces.
enum RecordKind {
    /// A URI record. `prefix` is an index into [`URI_PREFIX`], so the scheme
    /// costs one byte instead of eight.
    Uri { prefix: u8, rest: &'static str },
    /// A UTF-8 text record, tagged as English.
    Text(&'static str),
}

struct Preset {
    label: &'static str,
    kind: RecordKind,
}

/// Cycle through these with B. Add your own here — it's a one-line change.
const PRESETS: &[Preset] = &[
    Preset {
        label: "https://hackclub.com",
        kind: RecordKind::Uri {
            prefix: 0x04,
            rest: "hackclub.com",
        },
    },
    Preset {
        label: "github.com/Las-Vejas/nfcxpansion",
        kind: RecordKind::Uri {
            prefix: 0x04,
            rest: "github.com/Las-Vejas/nfcxpansion",
        },
    },
    Preset {
        label: "Text: hello from the console",
        kind: RecordKind::Text("hello from the console"),
    },
];

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Builds CC file + NDEF TLV + record + terminator into `out`.
///
/// Returns the number of bytes written, or `None` if the message doesn't fit.
/// This is pure — no hardware — so it is the one part of the app you can unit
/// test on your laptop.
fn build_message(kind: &RecordKind, out: &mut [u8]) -> Option<usize> {
    let mut record = [0u8; 256];

    let record_len = match kind {
        RecordKind::Uri { prefix, rest } => {
            let payload_len = 1 + rest.len();
            if payload_len > u8::MAX as usize {
                return None;
            }
            // MB | ME | SR | TNF=1 (NFC Forum well-known type)
            record[0] = 0xD1;
            record[1] = 0x01; // type length
            record[2] = payload_len as u8; // short record: one length byte
            record[3] = b'U';
            record[4] = *prefix;
            record
                .get_mut(5..5 + rest.len())?
                .copy_from_slice(rest.as_bytes());
            5 + rest.len()
        }
        RecordKind::Text(text) => {
            // status byte + "en" + the text itself
            let payload_len = 3 + text.len();
            if payload_len > u8::MAX as usize {
                return None;
            }
            record[0] = 0xD1;
            record[1] = 0x01;
            record[2] = payload_len as u8;
            record[3] = b'T';
            record[4] = 0x02; // UTF-8, language code is 2 bytes long
            record[5] = b'e';
            record[6] = b'n';
            record
                .get_mut(7..7 + text.len())?
                .copy_from_slice(text.as_bytes());
            7 + text.len()
        }
    };

    // Keeping the record under 255 bytes lets the TLV use a single length
    // byte. Anything longer would need the 0xFF + 16-bit form.
    if record_len >= 0xFF {
        return None;
    }

    let total = CC_FILE.len() + 2 + record_len + 1;
    if total > out.len() || total > USER_MEMORY_LEN {
        return None;
    }

    out[..4].copy_from_slice(&CC_FILE);
    out[4] = TLV_NDEF;
    out[5] = record_len as u8;
    out[6..6 + record_len].copy_from_slice(&record[..record_len]);
    out[6 + record_len] = TLV_TERMINATOR;

    Some(total)
}

// ---------------------------------------------------------------------------
// Decoding — what a phone left on the tag
// ---------------------------------------------------------------------------

/// Renders whatever is on the tag as something readable on a 320x240 screen.
fn describe_tag(buf: &[u8]) -> String {
    let mut out = String::new();

    match buf.first() {
        Some(0xE1) | Some(0xE2) => {}
        Some(0xFF) | None => {
            out.push_str("blank / unreadable");
            return out;
        }
        Some(_) => {
            out.push_str("no CC file - tag not NDEF formatted");
            return out;
        }
    }

    // Walk the TLV chain that starts right after the CC file.
    let mut i = CC_FILE.len();
    loop {
        let tag = match buf.get(i) {
            Some(tag) => *tag,
            None => {
                out.push_str("ran off the end of memory");
                return out;
            }
        };
        i += 1;

        // A NULL TLV is a single padding byte with no length field.
        if tag == 0x00 {
            continue;
        }
        if tag == TLV_TERMINATOR {
            out.push_str("formatted, but empty");
            return out;
        }

        let len = match buf.get(i) {
            Some(0xFF) => {
                let hi = *buf.get(i + 1).unwrap_or(&0) as usize;
                let lo = *buf.get(i + 2).unwrap_or(&0) as usize;
                i += 3;
                (hi << 8) | lo
            }
            Some(len) => {
                let len = *len as usize;
                i += 1;
                len
            }
            None => {
                out.push_str("TLV with no length");
                return out;
            }
        };

        if tag == TLV_NDEF {
            return match buf.get(i..i + len) {
                Some(value) => describe_record(value),
                None => {
                    out.push_str("NDEF message truncated");
                    out
                }
            };
        }

        // Some other TLV (lock control, memory control). Step over it.
        i += len;
    }
}

/// Decodes the first record of an NDEF message.
fn describe_record(value: &[u8]) -> String {
    let mut out = String::new();

    if value.len() < 3 {
        out.push_str("record too short");
        return out;
    }

    let header = value[0];
    let tnf = header & 0x07;
    let short_record = header & 0x10 != 0;
    let has_id = header & 0x08 != 0;
    let type_len = value[1] as usize;

    let mut i = 2;
    let payload_len = if short_record {
        let len = value[i] as usize;
        i += 1;
        len
    } else {
        let bytes = match value.get(i..i + 4) {
            Some(bytes) => bytes,
            None => {
                out.push_str("bad payload length");
                return out;
            }
        };
        i += 4;
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
    };

    let id_len = if has_id {
        let len = *value.get(i).unwrap_or(&0) as usize;
        i += 1;
        len
    } else {
        0
    };

    let record_type = match value.get(i..i + type_len) {
        Some(record_type) => record_type,
        None => {
            out.push_str("bad type field");
            return out;
        }
    };
    i += type_len + id_len;

    let payload = match value.get(i..i + payload_len) {
        Some(payload) => payload,
        None => {
            out.push_str("payload truncated");
            return out;
        }
    };

    // TNF 1 is "NFC Forum well-known type" — the only one we decode.
    if tnf == 0x01 && record_type == b"U" {
        let prefix = *payload.first().unwrap_or(&0) as usize;
        out.push_str(URI_PREFIX.get(prefix).copied().unwrap_or(""));
        push_text(&mut out, payload.get(1..).unwrap_or(&[]));
    } else if tnf == 0x01 && record_type == b"T" {
        let status = *payload.first().unwrap_or(&0);
        let lang_len = (status & 0x3F) as usize;
        push_text(&mut out, payload.get(1 + lang_len..).unwrap_or(&[]));
    } else {
        let _ = write!(out, "TNF {tnf}, {payload_len} byte payload");
    }

    out
}

fn push_text(out: &mut String, bytes: &[u8]) {
    match core::str::from_utf8(bytes) {
        Ok(text) => out.push_str(text),
        Err(_) => out.push_str("<not valid UTF-8>"),
    }
}

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(8).enumerate() {
        let _ = write!(out, "{:04X} ", row * 8);
        for byte in chunk {
            let _ = write!(out, "{byte:02X} ");
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Talking to the chip
// ---------------------------------------------------------------------------

enum WriteError {
    /// The message is bigger than the tag.
    TooBig,
    /// A reader field is up; the RF side may be holding the memory array.
    FieldPresent,
    /// I2C failed.
    Bus,
    /// The write succeeded but the read-back didn't match.
    Verify,
}

impl WriteError {
    fn message(&self) -> &'static str {
        match self {
            WriteError::TooBig => "Too big for this tag",
            WriteError::FieldPresent => "Lift the phone, then press A",
            WriteError::Bus => "I2C failed - check driver address",
            WriteError::Verify => "Read-back did not match",
        }
    }
}

/// Writes `data` at `address`, chunked and paced so the EEPROM keeps up.
async fn write_all(nfc: &mut Box<dyn Nfc>, address: u16, data: &[u8]) -> Result<(), NfcError> {
    let mut address = address;
    let mut rest = data;

    while !rest.is_empty() {
        // Never let one transaction straddle a chunk boundary.
        let room = WRITE_CHUNK - (address as usize % WRITE_CHUNK);
        let take = room.min(rest.len());

        nfc.write(address, &rest[..take]).await?;
        Timer::after_millis(WRITE_CYCLE_MS).await;

        address += take as u16;
        rest = &rest[take..];
    }

    Ok(())
}

/// Encodes a preset, writes it to the tag, and proves it landed.
async fn write_preset(nfc: &mut Box<dyn Nfc>, preset: &Preset) -> Result<usize, WriteError> {
    let mut message = [0u8; USER_MEMORY_LEN];
    let len = build_message(&preset.kind, &mut message).ok_or(WriteError::TooBig)?;

    // Both interfaces reach the same memory array. Writing under an active
    // reader field is how you get a half-written tag.
    if nfc.detect_field().await.map_err(|_| WriteError::Bus)? {
        return Err(WriteError::FieldPresent);
    }

    write_all(nfc, 0, &message[..len])
        .await
        .map_err(|_| WriteError::Bus)?;

    let mut readback = [0u8; USER_MEMORY_LEN];
    nfc.read(0, &mut readback[..len])
        .await
        .map_err(|_| WriteError::Bus)?;

    if readback[..len] != message[..len] {
        return Err(WriteError::Verify);
    }

    Ok(len)
}

// ---------------------------------------------------------------------------
// The app
// ---------------------------------------------------------------------------

/// The four physical buttons. Asking for A, B, X and Y (rather than mixing in
/// Up/Down/Left/Right) matters: the four-button module aliases each pin to one
/// of each pair, so A *is* Down and B *is* Right. Requesting both halves of a
/// pair asks for the same button twice, and the registry refuses.
type Controls = (
    Box<dyn Button<A>>,
    Box<dyn Button<B>>,
    Box<dyn Button<X>>,
    Box<dyn Button<Y>>,
);

pub struct NfcWriterApp {
    nfc: ResourceLease<Box<dyn Nfc>>,
    a: ResourceLease<Box<dyn Button<A>>>,
    b: ResourceLease<Box<dyn Button<B>>>,
    x: ResourceLease<Box<dyn Button<X>>>,
    y: ResourceLease<Box<dyn Button<Y>>>,
}

impl App for NfcWriterApp {
    const NAME: &'static str = "NFC Writer";

    fn can_run(registry: &Registry) -> bool {
        // Must mirror `new` exactly, or the app shows up in the picker and
        // then fails to start.
        registry.has::<Box<dyn Nfc>>() && registry.has_resource_set::<Controls>()
    }

    fn new(registry: &mut Registry) -> Option<Self> {
        let nfc = registry.take_resource::<Box<dyn Nfc>>()?;

        // A bare `?` here would drop the NFC lease, which removes the module
        // from the registry until the console reboots. Hand it back instead.
        match registry.take_resource_set::<Controls>() {
            Some((a, b, x, y)) => Some(Self { nfc, a, b, x, y }),
            None => {
                registry.return_resource(nfc);
                None
            }
        }
    }

    fn run<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(async move {
            // Destructuring up front gives us an independent `&mut` to each
            // field, so we can wait on four buttons and poke the NFC module
            // without the borrow checker objecting.
            let Self { nfc, a, b, x, y } = self;

            let ui = match NfcWriterUI::new() {
                Ok(ui) => ui,
                Err(_) => {
                    defmt::error!("NfcWriterApp: failed to create UI");
                    return;
                }
            };
            if ui.show().is_err() {
                defmt::error!("NfcWriterApp: failed to show UI");
                return;
            }

            let mut selected = 0usize;
            let mut show_hex = false;

            ui.set_preset(PRESETS[selected].label.into());
            ui.set_status("Ready".into());
            ui.set_tag_text("press X to read".into());

            let mut ticker = Ticker::every(Duration::from_millis(250));

            loop {
                // Resolve the race in its own statement. Written as
                // `match select5(..).await { .. }` the temporary borrows of
                // a/b/x/y would stay alive for the whole match body, and the
                // arms could not touch those buttons again.
                let event = select5(
                    a.resource_mut().wait_for_pressed(),
                    b.resource_mut().wait_for_pressed(),
                    x.resource_mut().wait_for_pressed(),
                    y.resource_mut().wait_for_pressed(),
                    ticker.next(),
                )
                .await;

                match event {
                    // A — short press writes, long press leaves.
                    Either5::First(()) => {
                        let mut held_ms = 0u32;
                        while a.resource().is_pressed() && held_ms < 1_000 {
                            Timer::after_millis(20).await;
                            held_ms += 20;
                        }

                        if held_ms >= 1_000 {
                            while a.resource().is_pressed() {
                                Timer::after_millis(20).await;
                            }
                            break;
                        }

                        ui.set_status("Writing...".into());
                        let preset = &PRESETS[selected];

                        match write_preset(nfc.resource_mut(), preset).await {
                            Ok(len) => {
                                let mut status = String::new();
                                let _ = write!(status, "Written, {len} bytes verified");
                                ui.set_status(SharedString::from(status.as_str()));
                                ui.set_tag_text(preset.label.into());
                                defmt::info!("wrote {} bytes to tag", len);
                            }
                            Err(error) => {
                                ui.set_status(error.message().into());
                                defmt::warn!("write failed: {}", error.message());
                            }
                        }
                    }

                    // B — next preset.
                    Either5::Second(()) => {
                        selected = (selected + 1) % PRESETS.len();
                        ui.set_preset(PRESETS[selected].label.into());
                        ui.set_status("Ready".into());
                    }

                    // X — read the tag back and decode it.
                    Either5::Third(()) => {
                        ui.set_status("Reading...".into());

                        let mut buf = [0u8; 256];
                        match nfc.resource_mut().read(0, &mut buf).await {
                            Ok(()) => {
                                let decoded = describe_tag(&buf);
                                ui.set_tag_text(SharedString::from(decoded.as_str()));
                                ui.set_hex(SharedString::from(hex_dump(&buf[..32]).as_str()));
                                ui.set_status("Read OK".into());
                                defmt::info!("tag says: {}", decoded.as_str());
                            }
                            Err(_) => {
                                ui.set_status("Read failed - check driver address".into());
                            }
                        }
                    }

                    // Y — flip between the decoded view and a raw hex dump.
                    Either5::Fourth(()) => {
                        show_hex = !show_hex;
                        ui.set_show_hex(show_hex);
                    }

                    // Tick — refresh the field indicator.
                    Either5::Fifth(()) => {
                        let field_on = nfc.resource_mut().detect_field().await.unwrap_or(false);
                        ui.set_field_on(field_on);
                    }
                }
            }

            if ui.hide().is_err() {
                defmt::error!("NfcWriterApp: failed to hide UI");
            }
        })
    }

    fn release(self, registry: &mut Registry) {
        // Every lease has to go back. A dropped lease takes its whole physical
        // group out of the registry until the console reboots.
        registry.return_resource(self.nfc);
        registry.return_resource(self.a);
        registry.return_resource(self.b);
        registry.return_resource(self.x);
        registry.return_resource(self.y);
    }
}
