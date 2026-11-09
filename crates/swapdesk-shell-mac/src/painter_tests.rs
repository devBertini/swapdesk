//! The shared screens painted by the Mac painter: every mock scenario renders, places every control the state lists,
//! and writes its picture and layout dump to `test-results/mac-shots/` for the comparison with the Windows renderer
//! (`test-results/stage11-shots/*.layout.txt`).

use std::path::PathBuf;

use swapdesk_paint::{Interaction, design_size, layout_dump};
use swapdesk_ui::WindowKind;
use swapdesk_ui::mock::Scenario;

use crate::painter::{Fonts, render};

fn shots_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-results/mac-shots");
    std::fs::create_dir_all(&dir).expect("shots dir");
    dir
}

fn suffix(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::Main => "",
        WindowKind::Viewer => "-viewer",
        WindowKind::Banner => "-banner",
    }
}

#[test]
fn every_scenario_paints_with_core_graphics_and_every_control_has_a_place() {
    let dir = shots_dir();
    let fonts = Fonts::new();
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        for kind in state.windows() {
            let r = render(&fonts, &state, kind, design_size(kind), 1.0, &Interaction::default())
                .unwrap_or_else(|e| panic!("{} {kind:?}: {e}", sc.name()));
            assert!(r.distinct_colors() > 3, "{} {kind:?} looks blank", sc.name());
            let name = format!("{}{}", sc.name(), suffix(kind));
            r.save_png(&dir.join(format!("{name}.png"))).unwrap_or_else(|e| panic!("{name}: {e}"));
            std::fs::write(dir.join(format!("{name}.layout.txt")), layout_dump(&r.output)).expect("layout dump");
            let missing: Vec<_> = state
                .controls(kind)
                .into_iter()
                .filter(|c| c.id.focusable() && !r.output.widgets.iter().any(|w| w.id == c.id))
                .map(|c| format!("{:?}", c.id))
                .collect();
            assert!(missing.is_empty(), "{name}: not drawn: {missing:?}");
        }
    }
}

#[test]
fn a_second_identical_paint_lays_no_text_out_and_draws_the_same_pixels() {
    let fonts = Fonts::new();
    let state = Scenario::Dispositivos.prepared_state();
    let ix = Interaction::default();
    let size = design_size(WindowKind::Main);
    let first = render(&fonts, &state, WindowKind::Main, size, 1.0, &ix).expect("first");
    let created = fonts.layouts_created.get();
    assert!(created > 10, "the first paint lays text out ({created})");
    let second = render(&fonts, &state, WindowKind::Main, size, 1.0, &ix).expect("second");
    assert_eq!(fonts.layouts_created.get(), created);
    assert_eq!(first.pixels, second.pixels);
}

#[test]
fn a_retina_paint_has_twice_the_pixels_and_the_same_layout() {
    let fonts = Fonts::new();
    let state = Scenario::Inicio.prepared_state();
    let ix = Interaction::default();
    let size = design_size(WindowKind::Main);
    let one = render(&fonts, &state, WindowKind::Main, size, 1.0, &ix).expect("1x");
    let two = render(&fonts, &state, WindowKind::Main, size, 2.0, &ix).expect("2x");
    assert_eq!((two.width, two.height), (one.width * 2, one.height * 2));
    two.save_png(&shots_dir().join("inicio@2x.png")).expect("png");
    assert_eq!(one.output.widgets.len(), two.output.widgets.len());
}
