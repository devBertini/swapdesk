//! The recorder's tests: geometry reaches the canvas in device pixels by the rules the desktop painters follow, and
//! laying text out from lists of measures gives what a painter that measures on the spot would draw.

use std::cell::RefCell;

use swapdesk_paint::svg::Seg;
use swapdesk_paint::{
    Align, Frame, Interaction, Label, Painter, Rect, design_size, label_height, layout_dump, line_box,
};
use swapdesk_ui::mock::Scenario;
use swapdesk_ui::tokens::{Color, TextStyle};
use swapdesk_ui::{UiState, WindowKind};

use crate::canvas_ops::{FRAME, NEEDS};
use crate::canvas_ops::{Op, argb, decode, flag};
use crate::text_measure::{Measured, Run, TextCache, TextKey, decode_needs, encode_measured};
use crate::{FrameCost, Metrics, PASSES_MAX, PhoneWindow, Recorder, Surface};

const METRICS: Metrics = Metrics { ascent: 0.97, descent: 0.25, mono_ascent: 0.9, mono_descent: 0.3 };

/// Android in the tests: every character is `advance` of the size wide, an ellipsized line is cut to fit, the
/// semi-bold start of a label is a run of its own, and a line breaks where it is full, or at the last space before
/// that when `words` is set.
#[derive(Clone, Copy)]
struct Fake {
    advance: f32,
    words: bool,
}

/// Close to the size a label gets before it is measured.
const NEAR: Fake = Fake { advance: 0.55, words: false };

impl Fake {
    fn measure(self, key: &TextKey) -> Measured {
        let advance = f32::from_bits(key.size_px) * self.advance;
        let chars: Vec<(usize, char)> = key.text.chars().enumerate().collect();
        let fit = ((key.max_w_px as f32 / advance).floor() as usize).max(1);
        let mut lines: Vec<Vec<(usize, char)>> = Vec::new();
        if key.ellipsis {
            let mut line = chars.clone();
            if line.len() > fit {
                line.truncate(fit.saturating_sub(1));
                line.push((usize::MAX, '…'));
            }
            lines.push(line);
        } else {
            let mut rest = &chars[..];
            loop {
                let mut end = fit.min(rest.len());
                if self.words && end < rest.len() {
                    // Back to the last space that fits; a word longer than the line breaks where the line is full.
                    if let Some(space) = rest[..end].iter().rposition(|(_, c)| *c == ' ').filter(|at| *at > 0) {
                        end = space;
                    }
                }
                lines.push(rest[..end].to_vec());
                rest = &rest[end..];
                while self.words && rest.first().is_some_and(|(_, c)| *c == ' ') {
                    rest = &rest[1..];
                }
                if rest.is_empty() {
                    break;
                }
            }
        }
        let bold_chars = usize::from(key.bold_chars);
        let run = |bold: bool, part: &[(usize, char)]| Run {
            bold,
            width: part.len() as f32 * advance,
            text: part.iter().map(|(_, c)| *c).collect(),
        };
        Measured {
            lines: lines
                .into_iter()
                .map(|line| {
                    let bold = line.iter().take_while(|(i, _)| *i < bold_chars).count();
                    let mut runs = Vec::new();
                    if bold > 0 {
                        runs.push(run(true, &line[..bold]));
                    }
                    if bold < line.len() || runs.is_empty() {
                        runs.push(run(false, &line[bold..]));
                    }
                    runs
                })
                .collect(),
        }
    }
}

/// Paints until a frame comes, answering each list of needs as Java would. Returns the frame, the layout it
/// registered and how many times the window was painted.
fn frame(window: &mut PhoneWindow, state: &UiState, on: Surface, android: Fake) -> (Vec<u8>, String, usize) {
    let ix = Interaction::default();
    for pass in 1..=PASSES_MAX + 1 {
        let bytes = window.paint(METRICS, state, &ix, on);
        if bytes[0] == FRAME {
            let layout = layout_dump(window.output().expect("a frame leaves what it registered"));
            return (bytes, layout, pass);
        }
        assert_eq!(bytes[0], NEEDS);
        let asked = decode_needs(&bytes).expect("the list of needs reads back");
        let answer: Vec<Measured> = asked.iter().map(|k| android.measure(k)).collect();
        assert_eq!(window.measured(&encode_measured(&answer)), Ok(asked.len()), "the measures are taken");
    }
    panic!("no frame after {} paints", PASSES_MAX + 1);
}

/// The painter the text is checked against: it measures every label on the spot with the same fake Android and logs
/// each run it draws as the operation it should become. Geometry is not its business.
struct Direct {
    scale: f32,
    android: Fake,
    texts: RefCell<Vec<Op>>,
}

struct DirectLabel {
    lines: Vec<(f32, Vec<Run>)>,
    line_h: f32,
    st: TextStyle,
}

impl Painter for Direct {
    fn scale(&self) -> f32 {
        self.scale
    }
    fn clear(&self, _: Color) {}
    fn fill_rect(&self, _: Rect, _: Color) {}
    fn fill_rrect(&self, _: Rect, _: f32, _: Color) {}
    fn stroke_rrect(&self, _: Rect, _: f32, _: f32, _: Color) {}
    fn stroke_rrect_dashed(&self, _: Rect, _: f32, _: f32, _: Color) {}
    fn line(&self, _: (f32, f32), _: (f32, f32), _: f32, _: Color, _: bool) {}
    fn fill_circle(&self, _: (f32, f32), _: f32, _: Color) {}
    fn stroke_circle(&self, _: (f32, f32), _: f32, _: f32, _: Color) {}
    fn arc_stroke(&self, _: (f32, f32), _: f32, _: f32, _: f32, _: f32, _: Color) {}
    fn fill_triangle(&self, _: [(f32, f32); 3], _: Color) {}
    fn svg(&self, _: &str, _: f32, _: f32, _: f32, _: f32, _: Color) {}
    fn shadow(&self, _: Rect, _: f32, _: f32, _: f32, _: Color) {}
    fn push_clip(&self, _: Rect) {}
    fn pop_clip(&self) {}
    fn content_height(&self, st: TextStyle) -> f32 {
        let (a, d) =
            if st.mono { (METRICS.mono_ascent, METRICS.mono_descent) } else { (METRICS.ascent, METRICS.descent) };
        (a + d) * st.size
    }
    fn layout(&self, text: &str, st: TextStyle, max_w: f32, align: Align, ellipsis: bool, bold_chars: usize) -> Label {
        let measured = self.android.measure(&TextKey {
            text: text.to_owned(),
            size_px: (st.size * self.scale).to_bits(),
            weight: st.weight,
            tracking: st.tracking.to_bits(),
            mono: st.mono,
            italic: st.italic,
            max_w_px: (max_w * self.scale).round().clamp(0.0, 16_000_000.0) as i32,
            ellipsis,
            bold_chars: bold_chars as u16,
        });
        let (a, d) =
            if st.mono { (METRICS.mono_ascent, METRICS.mono_descent) } else { (METRICS.ascent, METRICS.descent) };
        let (line_h, baseline) = line_box(st, self.scale, a, d);
        let widths: Vec<f32> =
            measured.lines.iter().map(|l| l.iter().map(|r| r.width).sum::<f32>() / self.scale).collect();
        let lines = measured
            .lines
            .into_iter()
            .zip(&widths)
            .map(|(runs, w)| (if align == Align::Center { (max_w - w) / 2.0 } else { 0.0 }, runs))
            .collect::<Vec<_>>();
        let h = label_height(lines.len().max(1) as f32 * line_h, self.scale);
        Label::new(widths.iter().copied().fold(0.0, f32::max), h, baseline, DirectLabel { lines, line_h, st })
    }
    fn draw(&self, l: &Label, x: f32, y: f32, c: Color) {
        let label = l.handle::<DirectLabel>().expect("a label is drawn by the painter that made it");
        let first = ((y + l.baseline) * self.scale).round();
        for (i, (dx, runs)) in label.lines.iter().enumerate() {
            let mut pen = (x + dx) * self.scale;
            for run in runs {
                if !run.text.is_empty() {
                    self.texts.borrow_mut().push(Op::Text {
                        at: [pen, first + i as f32 * label.line_h * self.scale],
                        color: argb(c),
                        size: label.st.size * self.scale,
                        weight: if run.bold { label.st.weight.max(600) } else { label.st.weight },
                        tracking: label.st.tracking,
                        flags: flag::of(label.st.mono, label.st.italic, false),
                        text: run.text.clone(),
                    });
                }
                pen += run.width;
            }
        }
    }
}

/// For every scenario of every window, at the density of a desktop and of a phone, and with fake Androids near and
/// far from the size a label gets before it is measured: a frame comes, a second paint gives the same frame at once,
/// the layout it registers and every run of text it draws are those of a painter that measures on the spot, and its
/// clips are balanced. With the near Android a new screen costs two or three passes.
#[test]
fn text_from_lists_of_measures_is_what_measuring_on_the_spot_draws_for_every_scenario() {
    let androids = [NEAR, Fake { advance: 0.3, words: true }, Fake { advance: 0.9, words: true }];
    let mut most = 0;
    for (which, android) in androids.into_iter().enumerate() {
        for scale in [1.0f32, 2.625] {
            for sc in Scenario::ALL {
                let state = sc.prepared_state();
                for kind in state.windows() {
                    let what = format!("{} {kind:?} at {scale}, Android {which}", sc.name());
                    let on = Surface { kind, size: design_size(kind), scale, offscreen: true };
                    let mut window = PhoneWindow::new();
                    let (bytes, layout, passes) = frame(&mut window, &state, on, android);
                    assert!(!window.cost().forced && window.cost().lists == passes - 1, "{what}: {:?}", window.cost());
                    most = most.max(passes);
                    if which == 0 {
                        assert!((2..=3).contains(&passes), "{what}: {passes} passes for a new screen");
                    }
                    let (again, _, second) = frame(&mut window, &state, on, android);
                    assert_eq!((second, &again), (1, &bytes), "{what}: the same frame at once");
                    assert_eq!(window.cost(), FrameCost::default(), "{what}: nothing measured for it");

                    let direct = Direct { scale, android, texts: RefCell::new(Vec::new()) };
                    let ix = Interaction::default();
                    let expected = swapdesk_paint::paint(Frame::new(&direct, &state, &ix, on.size, true), kind);
                    assert_eq!(layout, layout_dump(&expected), "{what}");

                    let recorded = decode(&bytes).expect("the frame reads back");
                    let texts: Vec<&Op> = recorded.iter().filter(|o| matches!(o, Op::Text { .. })).collect();
                    let drawn = direct.texts.into_inner();
                    assert_eq!(texts.len(), drawn.len(), "{what}: runs of text");
                    for (i, (got, want)) in texts.iter().zip(&drawn).enumerate() {
                        assert_eq!(*got, want, "{what}: run {i}");
                    }
                    let clips = recorded.iter().fold(0i32, |n, o| match o {
                        Op::PushClip(_) => n + 1,
                        Op::PopClip => n - 1,
                        _ => n,
                    });
                    assert_eq!(clips, 0, "{what}: every clip pushed is popped");
                }
            }
        }
    }
    assert!(most <= 3, "the slowest screen took {most} passes");
}

fn recorded(scale: f32, paint: impl FnOnce(&Recorder)) -> Vec<Op> {
    let cache = TextCache::new();
    let recorder = Recorder::new(scale, METRICS, &cache);
    paint(&recorder);
    decode(&recorder.finish()).expect("the frame reads back")
}

const INK: Color = Color { r: 0x17, g: 0x1A, b: 0x21, a: 0xFF };

/// At 150%: a filled rectangle and a clip have their edges on whole device pixels (each edge rounded by itself, so
/// two boxes that share an edge still share it); a stroke stays where it is, because it is centred on its path.
#[test]
fn fills_and_clips_snap_to_device_pixels_and_strokes_do_not() {
    let ops = recorded(1.5, |p| {
        p.fill_rect(Rect::new(10.3, 20.3, 100.0, 1.0), INK);
        p.fill_rrect(Rect::new(0.5, 0.5, 9.0, 9.0), 4.0, INK);
        p.push_clip(Rect::new(1.0, 1.0, 33.4, 33.4));
        p.pop_clip();
        p.stroke_rrect(Rect::new(10.3, 20.3, 100.0, 50.0), 8.0, 2.0, INK);
        p.stroke_rrect_dashed(Rect::new(10.3, 20.3, 100.0, 50.0), 8.0, 1.0, INK);
        p.line((1.0, 1.5), (3.0, 3.5), 1.0, INK, true);
        p.fill_circle((5.0, 5.0), 3.0, INK);
        p.stroke_circle((5.0, 5.0), 3.0, 1.0, INK);
        p.fill_triangle([(0.0, 0.0), (1.0, 0.5), (0.0, 1.0)], INK);
        p.clear(INK);
        // A rectangle turned inside out by the rounding is empty, never negative.
        p.fill_rect(Rect::new(0.0, 0.0, -3.0, 0.2), INK);
    });
    let ink = 0xFF17_1A21;
    assert_eq!(
        ops,
        vec![
            // 10.3 → 15.45 → 15; 110.3 → 165.45 → 165; 20.3 → 30.45 → 30; 21.3 → 31.95 → 32.
            Op::FillRect([15.0, 30.0, 150.0, 2.0], ink),
            // 0.5 → 0.75 → 1; 9.5 → 14.25 → 14.
            Op::FillRrect([1.0, 1.0, 13.0, 13.0], 6.0, ink),
            // 1 → 1.5 → 2; 34.4 → 51.6 → 52.
            Op::PushClip([2.0, 2.0, 50.0, 50.0]),
            Op::PopClip,
            Op::StrokeRrect([10.3 * 1.5, 20.3 * 1.5, 150.0, 75.0], 12.0, 3.0, ink),
            Op::StrokeRrectDashed([10.3 * 1.5, 20.3 * 1.5, 150.0, 75.0], 12.0, 1.5, ink),
            Op::Line([1.5, 2.25], [4.5, 5.25], 1.5, ink, true),
            Op::FillCircle([7.5, 7.5], 4.5, ink),
            Op::StrokeCircle([7.5, 7.5], 4.5, 1.5, ink),
            Op::Triangle([0.0, 0.0, 1.5, 0.75, 0.0, 1.5], ink),
            Op::Clear(ink),
            Op::FillRect([0.0, 0.0, 0.0, 0.0], ink),
        ]
    );
}

/// An icon is its path from the 24-unit viewBox, scaled to its size and to the display, with the stroke's width in
/// viewBox units; an arc is cubics around its centre; a path with nothing in it draws nothing.
#[test]
fn icons_and_arcs_are_paths_in_device_pixels() {
    let ops = recorded(2.0, |p| {
        // A 12 DIP icon at (10, 20): one viewBox unit is half a DIP, one device pixel.
        p.svg("M4 4h16v8", 10.0, 20.0, 12.0, 1.75, INK);
        p.svg("", 0.0, 0.0, 24.0, 1.75, INK);
        p.arc_stroke((50.0, 50.0), 10.0, 0.0, 90.0, 3.0, INK);
    });
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!(
        ops[0],
        Op::StrokePath(1.75, 0xFF17_1A21, vec![Seg::Move(24.0, 44.0), Seg::Line(40.0, 44.0), Seg::Line(40.0, 52.0)])
    );
    let Op::StrokePath(width, _, arc) = &ops[1] else { panic!("{:?}", ops[1]) };
    assert_eq!(*width, 6.0);
    assert_eq!(arc[0], Seg::Move(120.0, 100.0), "it starts on the positive x axis");
    let Some(Seg::Cubic(.., x, y)) = arc.last().copied() else { panic!("{arc:?}") };
    assert!((x - 100.0).abs() < 0.01 && (y - 120.0).abs() < 0.01, "and turns clockwise on screen, to below: {x} {y}");
}

/// A shadow is twelve translucent rounded rectangles under the box, the widest the faintest, which together carry
/// the colour's alpha once and a third (the desktop painters' sum).
#[test]
fn a_shadow_is_twelve_layers_that_shrink_and_darken() {
    let shade = Color { r: 0x17, g: 0x1A, b: 0x21, a: 0x29 };
    let ops = recorded(2.0, |p| p.shadow(Rect::new(100.0, 100.0, 200.0, 80.0), 12.0, 8.0, 24.0, shade));
    assert_eq!(ops.len(), 12);
    let layers: Vec<([f32; 4], f32, u32, f32)> = ops
        .iter()
        .map(|o| match o {
            Op::ShadowLayer(r, radius, rgb, alpha) => (*r, *radius, *rgb, *alpha),
            other => panic!("{other:?}"),
        })
        .collect();
    // The first layer grows by half the blur on every side, 8 DIPs below the box.
    assert_eq!(layers[0].0, [176.0, 192.0, 448.0, 208.0]);
    assert_eq!((layers[0].1, layers[0].2), (48.0, 0x0017_1A21));
    for pair in layers.windows(2) {
        assert!(pair[1].0[2] < pair[0].0[2] && pair[1].3 > pair[0].3, "{pair:?}");
    }
    let sum: f32 = layers.iter().map(|l| l.3).sum();
    let expected = f32::from(shade.a) / 255.0 * 0.9 * (1.0 + 11.0 / 24.0);
    assert!((sum - expected).abs() < 1e-5, "{sum} against {expected}");
}

/// A label's first baseline is a whole device pixel wherever the label is put, later lines step by the line height,
/// a centred line starts where its measured width puts it, and the semi-bold start of a label is drawn at 600.
#[test]
fn text_is_drawn_run_by_run_from_a_whole_pixel_baseline() {
    let st = TextStyle::new(14.0, 400);
    let scale = 2.625;
    let cache = TextCache::new();
    let paint = |cache: &TextCache| {
        let recorder = Recorder::new(scale, METRICS, cache);
        let wrapped = recorder.layout("Sala controla este PC", st, 60.0, Align::Start, false, 4);
        recorder.draw(&wrapped, 10.3, 20.7, INK);
        let centred = recorder.layout("ok", st.mono().italic(), 100.0, Align::Center, true, 0);
        recorder.draw(&centred, 0.0, 0.0, INK);
        (wrapped, centred, recorder.finish())
    };
    let _ = paint(&cache);
    let keys = cache.take_needs();
    assert_eq!(keys.len(), 2);
    let answer: Vec<Measured> = keys.iter().map(|k| NEAR.measure(k)).collect();
    cache.measured(&keys, &encode_measured(&answer)).unwrap();
    let (wrapped, centred, bytes) = paint(&cache);
    assert!(!cache.has_needs());

    let (line_h, baseline) = line_box(st, scale, METRICS.ascent, METRICS.descent);
    let advance = 14.0 * scale * 0.55;
    // 60 DIPs are 157.5 device pixels, laid out in 158: seven characters a line, so three lines.
    assert_eq!((wrapped.h, wrapped.baseline), (label_height(3.0 * line_h, scale), baseline));
    assert_eq!(wrapped.w, 7.0 * advance / scale);
    assert_eq!(centred.w, 2.0 * advance / scale);

    let ops = decode(&bytes).unwrap();
    let texts: Vec<(&str, [f32; 2], u16, u8)> = ops
        .iter()
        .map(|o| match o {
            Op::Text { text, at, weight, flags, size, .. } => {
                assert_eq!(*size, 14.0 * scale);
                (text.as_str(), *at, *weight, *flags)
            }
            other => panic!("{other:?}"),
        })
        .collect();
    let first = ((20.7 + baseline) * scale).round();
    let left = 10.3 * scale;
    assert_eq!(first.fract(), 0.0);
    assert_eq!(
        texts[..4],
        [
            ("Sala", [left, first], 600, 0),
            (" co", [left + 4.0 * advance, first], 400, 0),
            ("ntrola ", [left, first + line_h * scale], 400, 0),
            ("este PC", [left, first + 2.0 * line_h * scale], 400, 0),
        ]
    );
    let (mono_line_h, mono_baseline) = line_box(st.mono(), scale, METRICS.mono_ascent, METRICS.mono_descent);
    let _ = mono_line_h;
    let middle = (100.0 - 2.0 * advance / scale) / 2.0 * scale;
    assert_eq!(texts[4], ("ok", [middle, (mono_baseline * scale).round()], 400, flag::MONO | flag::ITALIC));
    assert_eq!(texts.len(), 5);
}

/// A real screen at a phone's density: every fill and clip is on whole pixels and it starts by clearing its canvas.
#[test]
fn a_whole_screen_keeps_its_fills_on_whole_pixels() {
    let state = Scenario::Dispositivos.prepared_state();
    let on = Surface { kind: WindowKind::Main, size: design_size(WindowKind::Main), scale: 2.625, offscreen: true };
    let (bytes, ..) = frame(&mut PhoneWindow::new(), &state, on, NEAR);
    let ops = decode(&bytes).unwrap();
    let whole = |v: &f32| v.fract() == 0.0;
    let mut texts = 0;
    for op in &ops {
        match op {
            Op::FillRect(r, _) | Op::FillRrect(r, ..) | Op::PushClip(r) => assert!(r.iter().all(whole), "{op:?}"),
            Op::Text { size, .. } => {
                texts += 1;
                assert!(*size > 20.0, "sizes are device pixels: {op:?}");
            }
            _ => {}
        }
    }
    assert!(texts > 10, "the screen has text: {texts}");
    assert!(matches!(ops[0], Op::Clear(_)), "a main window starts by clearing its canvas");
}

/// A frame always comes. When Java's answer is refused the next paint gives the frame as it is, and when the answers
/// never arrive the window stops asking after its limit; either way the labels nobody measured are still drawn, on
/// one line, and the window measures again at the next frame.
#[test]
fn a_frame_comes_even_when_the_measures_do_not() {
    let state = Scenario::Dispositivos.prepared_state();
    let on = Surface { kind: WindowKind::Main, size: design_size(WindowKind::Main), scale: 2.0, offscreen: true };
    let ix = Interaction::default();
    let texts_of = |bytes: &[u8]| {
        let ops = decode(bytes).expect("the frame reads back");
        ops.iter().filter(|o| matches!(o, Op::Text { text, .. } if !text.is_empty())).count()
    };

    // An answer that does not read back.
    let mut window = PhoneWindow::new();
    assert_eq!(window.paint(METRICS, &state, &ix, on)[0], NEEDS);
    assert!(window.measured(&[1, 2, 3]).is_err());
    let forced = window.paint(METRICS, &state, &ix, on);
    assert_eq!(forced[0], FRAME);
    assert!(window.cost().forced && window.known_labels() == 0, "{:?}", window.cost());
    assert!(texts_of(&forced) > 10, "the text is drawn at its rough size");
    // The refusal is not held against the next frame, which measures as usual.
    let (measured, _, passes) = frame(&mut window, &state, on, NEAR);
    assert!(passes > 1 && !window.cost().forced && window.known_labels() > 10);
    assert!(texts_of(&measured) >= texts_of(&forced), "every label is still drawn, now measured");

    // Lists that nobody answers: the window stops asking.
    let mut window = PhoneWindow::new();
    let mut paints = 0;
    let stuck = loop {
        let bytes = window.paint(METRICS, &state, &ix, on);
        paints += 1;
        if bytes[0] == FRAME {
            break bytes;
        }
        assert!(paints <= PASSES_MAX, "the window stops asking");
    };
    assert_eq!(paints, PASSES_MAX + 1);
    assert_eq!((window.cost().lists, window.cost().forced), (PASSES_MAX, true));
    assert_eq!(texts_of(&stuck), texts_of(&forced));
}
