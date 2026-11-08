use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use swapdesk_platform::{
    AdapterId, CaptureConfig, Clock, CursorUpdate, DisplayId, DisplayInfo, ErrorKind, Frame, Pixels, PlatformError,
    Rect, Result, ScreenSource,
};

use crate::barcode::{barcode_height, draw_barcode};
use crate::clock::MonotonicClock;

/// How `next_frame` spends time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    /// Every call returns the next frame at once: deterministic tests.
    Free,
    /// Frames come at the configured fps on the wall clock: measurements.
    Realtime,
}

const DISPLAY: DisplayId = DisplayId(0);

fn err(kind: ErrorKind, op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.to_string() }
}

/// CPU BGRA frames with a moving gradient, the frame number as text and the C7 barcode in the top-left corner.
/// `captured_at` of a frame is the injected clock's reading when the frame is produced.
pub struct SyntheticScreen {
    width: u32,
    height: u32,
    fps: u16,
    clock: Arc<dyn Clock>,
    pacing: Pacing,
    open: bool,
    /// Frames produced since `open`; also the next frame's counter.
    produced: u32,
    started: Instant,
    lost_at: Vec<u32>,
    must_reacquire: bool,
    resize_at: Vec<(u32, u32, u32)>,
    cursor: VecDeque<CursorUpdate>,
}

impl SyntheticScreen {
    pub fn new(width: u32, height: u32, fps: u16) -> Self {
        Self {
            width,
            height,
            fps: fps.max(1),
            clock: Arc::new(MonotonicClock::new()),
            pacing: Pacing::Free,
            open: false,
            produced: 0,
            started: Instant::now(),
            lost_at: Vec::new(),
            must_reacquire: false,
            resize_at: Vec::new(),
            cursor: VecDeque::new(),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_pacing(mut self, pacing: Pacing) -> Self {
        self.pacing = pacing;
        self
    }

    /// `next_frame` fails with `AccessLost` instead of producing frame `frame_index` (counted from `open`), and keeps
    /// failing until `reacquire`.
    pub fn script_access_lost_at(&mut self, frame_index: u32) {
        self.lost_at.push(frame_index);
    }

    /// From frame `frame_index` on, the display has the new size (`displays()` follows).
    pub fn script_display_change_at(&mut self, frame_index: u32, width: u32, height: u32) {
        self.resize_at.push((frame_index, width, height));
    }

    pub fn queue_cursor(&mut self, update: CursorUpdate) {
        self.cursor.push_back(update);
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn render(&self, counter: u32, captured_at: swapdesk_platform::MonoMicros) -> Frame {
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = w * 4;
        let mut data = vec![0u8; stride * h];
        let phase = counter.wrapping_mul(3);
        for (y, row) in data.chunks_exact_mut(stride).enumerate() {
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                px[0] = (x as u32).wrapping_add(phase) as u8;
                px[1] = ((y as u32) / 2).wrapping_add(phase / 2) as u8;
                px[2] = ((x + y) as u32 / 3).wrapping_sub(phase) as u8;
                px[3] = 255;
            }
        }
        let top = barcode_height(self.width, self.height).unwrap_or(0);
        draw_digits(&mut data, stride, self.width, self.height, 16, top + 16, counter);
        draw_barcode(&mut data, stride as u32, self.width, self.height, counter, captured_at);
        Frame {
            display: DISPLAY,
            captured_at,
            width: self.width,
            height: self.height,
            pixels: Pixels::Cpu { stride: stride as u32, data },
            dirty: Vec::new(),
            content_changed: true,
        }
    }
}

// 3x5 digit font, one row per u8 (bits 2..0).
const DIGITS: [[u8; 5]; 10] = [
    [7, 5, 5, 5, 7],
    [2, 6, 2, 2, 7],
    [7, 1, 7, 4, 7],
    [7, 1, 7, 1, 7],
    [5, 5, 7, 1, 1],
    [7, 4, 7, 1, 7],
    [7, 4, 7, 5, 7],
    [7, 1, 1, 1, 1],
    [7, 5, 7, 5, 7],
    [7, 5, 7, 1, 7],
];

fn draw_digits(data: &mut [u8], stride: usize, width: u32, height: u32, x0: u32, y0: u32, value: u32) {
    const SCALE: u32 = 6;
    let text = value.to_string();
    let box_w = text.len() as u32 * 4 * SCALE + SCALE;
    let box_h = 7 * SCALE;
    if x0 + box_w > width || y0 + box_h > height {
        return;
    }
    for y in y0..y0 + box_h {
        for x in x0..x0 + box_w {
            let i = y as usize * stride + x as usize * 4;
            data[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
        }
    }
    for (n, ch) in text.bytes().enumerate() {
        let glyph = DIGITS[(ch - b'0') as usize];
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..3u32 {
                if bits >> (2 - gx) & 1 == 0 {
                    continue;
                }
                for sy in 0..SCALE {
                    for sx in 0..SCALE {
                        let x = x0 + SCALE + (n as u32 * 4 + gx) * SCALE + sx;
                        let y = y0 + SCALE + gy as u32 * SCALE + sy;
                        let i = y as usize * stride + x as usize * 4;
                        data[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                    }
                }
            }
        }
    }
}

impl ScreenSource for SyntheticScreen {
    fn displays(&mut self) -> Result<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo {
            id: DISPLAY,
            name: "Synthetic".to_string(),
            bounds: Rect { x: 0, y: 0, width: self.width, height: self.height },
            scale_milli: 1000,
            refresh_mhz: u32::from(self.fps) * 1000,
            rotation: 0,
            primary: true,
            adapter: AdapterId(0),
        }])
    }

    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> Result<()> {
        if display != DISPLAY {
            return Err(err(ErrorKind::InvalidInput, "SyntheticScreen.open", "unknown display"));
        }
        self.fps = self.fps.min(config.max_fps).max(1);
        self.open = true;
        self.produced = 0;
        self.must_reacquire = false;
        self.started = Instant::now();
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if !self.open {
            return Err(err(ErrorKind::Closed, "SyntheticScreen.next_frame", "not open"));
        }
        if self.must_reacquire || self.lost_at.contains(&self.produced) {
            self.lost_at.retain(|&n| n != self.produced);
            self.must_reacquire = true;
            return Err(err(ErrorKind::AccessLost, "SyntheticScreen.next_frame", "scripted"));
        }
        if self.pacing == Pacing::Realtime {
            let period = Duration::from_secs_f64(1.0 / f64::from(self.fps));
            let due = self.started + period * self.produced;
            let wait = due.saturating_duration_since(Instant::now());
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        if let Some(&(_, w, h)) = self.resize_at.iter().find(|r| r.0 == self.produced) {
            self.width = w;
            self.height = h;
        }
        let counter = self.produced;
        self.produced += 1;
        Ok(Some(self.render(counter, self.clock.now())))
    }

    fn reacquire(&mut self) -> Result<()> {
        self.must_reacquire = false;
        Ok(())
    }

    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        self.cursor.pop_front()
    }

    fn close(&mut self) {
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BarcodeReader, FakeClock};
    use swapdesk_platform::MonoMicros;

    fn screen(w: u32, h: u32) -> (SyntheticScreen, FakeClock) {
        let clock = FakeClock::new(MonoMicros(1_000_000_000_007));
        let mut s = SyntheticScreen::new(w, h, 60).with_clock(Arc::new(clock.clone()));
        s.open(DISPLAY, CaptureConfig { max_fps: 60 }).unwrap();
        (s, clock)
    }

    /// JPEG-like stand-in for H.264 (GUESS): 8x8 DCT of the luma plane, uniform quantization, inverse DCT.
    fn lossy(data: &mut [u8], stride: usize, width: usize, height: usize, step: f32) {
        use std::f32::consts::PI;
        let mut basis = [[0f32; 8]; 8];
        for (k, row) in basis.iter_mut().enumerate() {
            for (n, v) in row.iter_mut().enumerate() {
                let c = if k == 0 { (1.0f32 / 8.0).sqrt() } else { (2.0f32 / 8.0).sqrt() };
                *v = c * ((2 * n + 1) as f32 * k as f32 * PI / 16.0).cos();
            }
        }
        for by in (0..height).step_by(8) {
            for bx in (0..width).step_by(8) {
                let mut blk = [[0f32; 8]; 8];
                for (y, line) in blk.iter_mut().enumerate() {
                    for (x, v) in line.iter_mut().enumerate() {
                        let i = (by + y) * stride + (bx + x) * 4;
                        let l = 0.114 * f32::from(data[i])
                            + 0.587 * f32::from(data[i + 1])
                            + 0.299 * f32::from(data[i + 2]);
                        *v = l - 128.0;
                    }
                }
                let mut coef = [[0f32; 8]; 8];
                for u in 0..8 {
                    for v in 0..8 {
                        let mut s = 0.0;
                        for y in 0..8 {
                            for x in 0..8 {
                                s += blk[y][x] * basis[u][y] * basis[v][x];
                            }
                        }
                        coef[u][v] = (s / step).round() * step;
                    }
                }
                for y in 0..8 {
                    for x in 0..8 {
                        let mut s = 0.0;
                        for u in 0..8 {
                            for v in 0..8 {
                                s += coef[u][v] * basis[u][y] * basis[v][x];
                            }
                        }
                        let l = (s + 128.0).clamp(0.0, 255.0) as u8;
                        let i = (by + y) * stride + (bx + x) * 4;
                        data[i..i + 4].copy_from_slice(&[l, l, l, 255]);
                    }
                }
            }
        }
    }

    #[test]
    fn barcode_reads_back_exactly() {
        let (mut s, clock) = screen(640, 360);
        let reader = BarcodeReader::new();
        for n in 0..5u32 {
            clock.advance(Duration::from_micros(16_667));
            let f = s.next_frame(Duration::ZERO).unwrap().unwrap();
            let r = reader.read_frame(&f).expect("barcode");
            assert_eq!(r.counter, n);
            assert_eq!(r.captured_at, f.captured_at);
            assert_eq!(r.captured_at, clock.now());
        }
    }

    #[test]
    fn barcode_survives_lossy_quantizer_and_checksum_rejects_garbage() {
        let (mut s, _clock) = screen(640, 360);
        let reader = BarcodeReader::new();
        let f = s.next_frame(Duration::ZERO).unwrap().unwrap();
        let Pixels::Cpu { stride, mut data } = f.pixels else { panic!("cpu frame") };
        lossy(&mut data, stride as usize, 640, 360, 60.0);
        let r = reader.read_bgra(&data, stride, 640, 360).expect("barcode after quantizer");
        assert_eq!(r.counter, 0);
        assert_eq!(r.captured_at, MonoMicros(1_000_000_000_007));
        // Flatten the barcode area to mid gray: the checksum must refuse it.
        for y in 0..48usize {
            for px in data[y * stride as usize..y * stride as usize + 512 * 4].as_chunks_mut::<4>().0.iter_mut() {
                px.copy_from_slice(&[128, 128, 128, 255]);
            }
        }
        assert_eq!(reader.read_bgra(&data, stride, 640, 360), None);
    }

    #[test]
    fn small_frame_has_no_barcode_and_narrow_frame_wraps() {
        let (mut s, _c) = screen(64, 64);
        let f = s.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!(BarcodeReader::new().read_frame(&f), None);
        let (mut s, _c) = screen(160, 200);
        let f = s.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!(BarcodeReader::new().read_frame(&f).map(|r| r.counter), Some(0));
    }

    #[test]
    fn scripted_access_lost_and_display_change() {
        let (mut s, _c) = screen(640, 360);
        s.script_access_lost_at(2);
        s.script_display_change_at(3, 800, 450);
        assert!(s.next_frame(Duration::ZERO).unwrap().is_some());
        assert!(s.next_frame(Duration::ZERO).unwrap().is_some());
        for _ in 0..2 {
            let e = s.next_frame(Duration::ZERO).err().expect("access lost");
            assert_eq!(e.kind, ErrorKind::AccessLost);
        }
        s.reacquire().unwrap();
        let f = s.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!((f.width, f.height), (640, 360));
        assert_eq!(BarcodeReader::new().read_frame(&f).map(|r| r.counter), Some(2));
        let f = s.next_frame(Duration::ZERO).unwrap().unwrap();
        assert_eq!((f.width, f.height), (800, 450));
        assert_eq!(BarcodeReader::new().read_frame(&f).map(|r| r.counter), Some(3));
        assert_eq!(s.displays().unwrap()[0].bounds.width, 800);
    }

    #[test]
    fn realtime_pacing_keeps_the_rate() {
        let mut s = SyntheticScreen::new(160, 200, 100).with_pacing(Pacing::Realtime);
        s.open(DISPLAY, CaptureConfig { max_fps: 100 }).unwrap();
        let t = Instant::now();
        for _ in 0..11 {
            assert!(s.next_frame(Duration::from_secs(1)).unwrap().is_some());
        }
        assert!(t.elapsed() >= Duration::from_millis(95));
    }

    #[test]
    fn not_open_is_closed() {
        let mut s = SyntheticScreen::new(64, 64, 30);
        assert_eq!(s.next_frame(Duration::ZERO).err().unwrap().kind, ErrorKind::Closed);
        assert_eq!(s.open(DisplayId(9), CaptureConfig { max_fps: 30 }).err().unwrap().kind, ErrorKind::InvalidInput);
    }
}
