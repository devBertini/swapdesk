//! What a key press types on a Mac, against the real keyboard layouts of the system (`UCKeyTranslate` over the
//! layout's `uchr` table), and the viewer's input typing accents toward a host whose layout differs. Nothing is posted
//! to the session and the selected input source is not changed: the layouts are looked up by id.
//!
//! "Brazilian - Pro" is the owner's layout (US-based: ' ` ~ ^ and " are dead keys, and ç is ' then c); "U.S." has
//! the Option dead keys every US-based Mac layout has (Option+E acute, Option+N tilde) and Option+C for ç.
#![cfg(target_os = "macos")]

use std::sync::mpsc::sync_channel;

use swapdesk_input_mac::{InputConfig, Layout, MacInput, MacLayout, NoKeyboardState, Typed};
use swapdesk_keys::mode::KeyboardMode;
use swapdesk_proto::InputEvent;
use swapdesk_shell_mac::{KeyMsg, Modifiers, ViewerInput};

const BRAZILIAN_PRO: &str = "com.apple.keylayout.Brazilian-Pro";
const US: &str = "com.apple.keylayout.US";

const KVK_A: u16 = 0x00;
const KVK_C: u16 = 0x08;
const KVK_E: u16 = 0x0E;
const KVK_N: u16 = 0x2D;
const KVK_T: u16 = 0x11;
const KVK_6: u16 = 0x16;
const KVK_QUOTE: u16 = 0x27;
const KVK_GRAVE: u16 = 0x32;
const KVK_SPACE: u16 = 0x31;

fn press(kvk: u16, modifiers: Modifiers) -> KeyMsg {
    KeyMsg { key_code: kvk, down: true, repeat: false, text: None, modifiers }
}

fn layout(id: &str) -> MacLayout {
    MacLayout::named(id).unwrap_or_else(|| panic!("the layout {id} is installed with every macOS"))
}

/// Types the presses in order and returns what each one gave.
fn type_all(l: &mut MacLayout, keys: &[(u16, Modifiers)]) -> Vec<Typed> {
    keys.iter().map(|&(kvk, m)| l.type_key(&press(kvk, m))).collect()
}

fn text(t: &str) -> Typed {
    Typed { text: Some(t.into()), dead: false }
}

fn dead() -> Typed {
    Typed { text: None, dead: true }
}

#[test]
fn brazilian_pro_composes_the_accents_of_portuguese_from_its_dead_keys() {
    let plain = Modifiers::default();
    let shift = Modifiers::SHIFT;
    let mut l = layout(BRAZILIAN_PRO);
    for (keys, want) in [
        (vec![(KVK_QUOTE, plain), (KVK_A, plain)], "á"),
        (vec![(KVK_QUOTE, plain), (KVK_E, plain)], "é"),
        (vec![(KVK_QUOTE, plain), (KVK_C, plain)], "ç"),
        (vec![(KVK_GRAVE, shift), (KVK_A, plain)], "ã"),
        (vec![(KVK_6, shift), (KVK_E, plain)], "ê"),
        (vec![(KVK_QUOTE, plain), (KVK_A, shift)], "Á"),
        (vec![(KVK_QUOTE, plain), (KVK_SPACE, plain)], "'"),
    ] {
        let got = type_all(&mut l, &keys);
        assert_eq!(got, vec![dead(), text(want)], "{want}");
    }
    // A letter the accent does not compose with: both come out, nothing is left pending.
    let got = type_all(&mut l, &[(KVK_QUOTE, plain), (KVK_T, plain), (KVK_A, plain)]);
    assert_eq!(got[0], dead());
    assert_eq!(got[2], text("a"), "the accent was used up: {got:?}");
    assert!(got[1].text.as_deref().is_some_and(|t| t.ends_with('t')), "{got:?}");
    // A plain letter is itself.
    assert_eq!(type_all(&mut l, &[(KVK_A, plain)]), vec![text("a")]);
}

#[test]
fn a_cleared_dead_key_does_not_reach_the_next_letter() {
    let mut l = layout(BRAZILIAN_PRO);
    assert_eq!(l.type_key(&press(KVK_QUOTE, Modifiers::default())), dead());
    l.clear_dead_key();
    assert_eq!(l.type_key(&press(KVK_A, Modifiers::default())), text("a"));
}

#[test]
fn us_option_combinations_type_their_characters_and_dead_keys() {
    let opt = Modifiers::OPTION;
    let plain = Modifiers::default();
    let mut l = layout(US);
    assert_eq!(type_all(&mut l, &[(KVK_C, opt)]), vec![text("ç")]);
    assert_eq!(type_all(&mut l, &[(KVK_E, opt), (KVK_E, plain)]), vec![dead(), text("é")]);
    assert_eq!(type_all(&mut l, &[(KVK_N, opt), (KVK_A, plain)]), vec![dead(), text("ã")]);
}

/// The owner's case end to end in the viewer's input: "Automático" toward a Windows host (layouts differ), the Mac on
/// "Brazilian - Pro". Each accent arrives as one text record; no key record reaches the host, and nothing is left
/// held.
#[test]
fn the_viewer_types_accents_to_a_windows_host_as_text() {
    let config = InputConfig { mode: KeyboardMode::Auto, layouts_match: false, ..InputConfig::default() };
    let input = MacInput::with_parts(config, Box::new(NoKeyboardState), Box::new(layout(BRAZILIAN_PRO)));
    let (tx, rx) = sync_channel(64);
    input.attach(tx);
    input.focus(true);
    let plain = Modifiers::default();
    let stroke = |kvk: u16, m: Modifiers| {
        assert!(input.key(&press(kvk, m)));
        assert!(input.key(&KeyMsg { down: false, ..press(kvk, m) }));
    };
    for (accent, letter) in [(KVK_QUOTE, KVK_C), (KVK_QUOTE, KVK_A), (KVK_QUOTE, KVK_E)] {
        stroke(accent, plain);
        stroke(letter, plain);
    }
    // The tilde is Shift+`: Shift is a key on the host, let go before the letter.
    assert!(input.modifiers_changed(Modifiers::SHIFT));
    stroke(KVK_GRAVE, Modifiers::SHIFT);
    assert!(input.modifiers_changed(plain));
    stroke(KVK_A, plain);
    let got: Vec<InputEvent> = rx.try_iter().collect();
    let texts: Vec<&str> = got
        .iter()
        .filter_map(|e| match e {
            InputEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["ç", "á", "é", "ã"]);
    let keys: Vec<u16> = got
        .iter()
        .filter_map(|e| match e {
            InputEvent::Key { usage, .. } => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec![0xE1, 0xE1], "only Shift went as a key (down, up): {got:?}");
    // Physical mode is unchanged: the dead key goes as the ' key (HID 0x34) and the layout is not asked.
    input.set_config(InputConfig { mode: KeyboardMode::Physical, ..config });
    stroke(KVK_QUOTE, plain);
    let got: Vec<InputEvent> = rx.try_iter().collect();
    assert!(
        matches!(
            got.as_slice(),
            [InputEvent::Key { usage: 0x34, down: true, .. }, InputEvent::Key { usage: 0x34, down: false, .. }]
        ),
        "{got:?}"
    );
}
