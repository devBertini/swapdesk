//! The frame barcode of `SyntheticScreen` (C7) and the reader the B1/B2 measurements use.
//!
//! Layout: cells of 16x16 pixels (macroblock-aligned) from the top-left corner, row by row, left to right,
//! `min(width / 16, 32)` cells per row. Each cell is flat luma 16 (bit 0) or 235 (bit 1), written as gray BGRA.
//! The bits, most significant first: the 32-bit frame counter, the 64-bit `captured_at` and a CRC-16/CCITT-FALSE
//! of those twelve bytes (an addition to C7: it lets a measurement reject a misread frame instead of trusting it).

use swapdesk_platform::{DecodedFrame, Frame, MonoMicros, Pixels};

/// Cell size in pixels: one H.264 macroblock.
pub const BARCODE_CELL: u32 = 16;
const LUMA_ZERO: u8 = 16;
const LUMA_ONE: u8 = 235;
const PAYLOAD_BITS: usize = 32 + 64;
const TOTAL_BITS: usize = PAYLOAD_BITS + 16;
const MAX_CELLS_PER_ROW: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarcodeReading {
    pub counter: u32,
    pub captured_at: MonoMicros,
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

fn layout(width: u32, height: u32) -> Option<(usize, usize)> {
    let per_row = (width / BARCODE_CELL).min(MAX_CELLS_PER_ROW) as usize;
    if per_row == 0 {
        return None;
    }
    let rows = TOTAL_BITS.div_ceil(per_row);
    if rows as u32 * BARCODE_CELL > height {
        return None;
    }
    Some((per_row, rows))
}

/// Height in pixels the barcode takes for a frame of this width, `None` when the frame is too small.
pub fn barcode_height(width: u32, height: u32) -> Option<u32> {
    layout(width, height).map(|(_, rows)| rows as u32 * BARCODE_CELL)
}

fn payload_bits(counter: u32, captured_at: MonoMicros) -> [bool; TOTAL_BITS] {
    let mut bytes = [0u8; 14];
    bytes[..4].copy_from_slice(&counter.to_be_bytes());
    bytes[4..12].copy_from_slice(&captured_at.0.to_be_bytes());
    let crc = crc16(&bytes[..12]);
    bytes[12..].copy_from_slice(&crc.to_be_bytes());
    let mut bits = [false; TOTAL_BITS];
    for (i, bit) in bits.iter_mut().enumerate() {
        *bit = bytes[i / 8] >> (7 - i % 8) & 1 == 1;
    }
    bits
}

/// Draws the barcode into a BGRA buffer. Returns false (and draws nothing) when the frame is too small.
pub fn draw_barcode(
    data: &mut [u8],
    stride: u32,
    width: u32,
    height: u32,
    counter: u32,
    captured_at: MonoMicros,
) -> bool {
    let Some((per_row, _)) = layout(width, height) else { return false };
    let bits = payload_bits(counter, captured_at);
    for (i, &bit) in bits.iter().enumerate() {
        let luma = if bit { LUMA_ONE } else { LUMA_ZERO };
        let cx = (i % per_row) as u32 * BARCODE_CELL;
        let cy = (i / per_row) as u32 * BARCODE_CELL;
        for y in cy..cy + BARCODE_CELL {
            let row = y as usize * stride as usize + cx as usize * 4;
            for px in data[row..row + BARCODE_CELL as usize * 4].as_chunks_mut::<4>().0.iter_mut() {
                px.copy_from_slice(&[luma, luma, luma, 255]);
            }
        }
    }
    true
}

/// Reads counter and timestamp back from a decoded frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct BarcodeReader;

impl BarcodeReader {
    pub fn new() -> Self {
        Self
    }

    /// `None` when the frame is too small, the pixels are not CPU memory or the checksum fails.
    pub fn read_bgra(&self, data: &[u8], stride: u32, width: u32, height: u32) -> Option<BarcodeReading> {
        let (per_row, _) = layout(width, height)?;
        let mut bytes = [0u8; 14];
        for i in 0..TOTAL_BITS {
            let cx = (i % per_row) as u32 * BARCODE_CELL;
            let cy = (i / per_row) as u32 * BARCODE_CELL;
            let mut sum: u64 = 0;
            for y in cy..cy + BARCODE_CELL {
                let row = y as usize * stride as usize + cx as usize * 4;
                let line = data.get(row..row + BARCODE_CELL as usize * 4)?;
                for px in line.as_chunks::<4>().0.iter() {
                    // BT.601 luma of B, G, R (weights in thousandths).
                    sum += (114 * u64::from(px[0]) + 587 * u64::from(px[1]) + 299 * u64::from(px[2])) / 1000;
                }
            }
            let mean = sum / u64::from(BARCODE_CELL * BARCODE_CELL);
            if mean >= u64::from(LUMA_ZERO + LUMA_ONE) / 2 {
                bytes[i / 8] |= 1 << (7 - i % 8);
            }
        }
        let crc = u16::from_be_bytes([bytes[12], bytes[13]]);
        if crc != crc16(&bytes[..12]) {
            return None;
        }
        Some(BarcodeReading {
            counter: u32::from_be_bytes(bytes[..4].try_into().ok()?),
            captured_at: MonoMicros(u64::from_be_bytes(bytes[4..12].try_into().ok()?)),
        })
    }

    pub fn read_pixels(&self, pixels: &Pixels, width: u32, height: u32) -> Option<BarcodeReading> {
        match pixels {
            Pixels::Cpu { stride, data } => self.read_bgra(data, *stride, width, height),
            Pixels::Gpu(_) => None,
        }
    }

    pub fn read_frame(&self, frame: &Frame) -> Option<BarcodeReading> {
        self.read_pixels(&frame.pixels, frame.width, frame.height)
    }

    pub fn read_decoded(&self, frame: &DecodedFrame) -> Option<BarcodeReading> {
        self.read_pixels(&frame.pixels, frame.width, frame.height)
    }
}
