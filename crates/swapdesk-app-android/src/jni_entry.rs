//! The JNI entry points: the native methods of `io.github.devbertini.swapdesk.Core` and `Shell` and, in a
//! development build only (cargo feature `dev-harness`), of `DevHarness`.
//!
//! Each one upgrades the caller's environment with `with_env` before anything else, which also keeps a panic from
//! unwinding into the Java VM: an error or a panic becomes a `RuntimeException` there. A refusal the app expects
//! (a bad key, a folder that cannot be made) is not an exception: it comes back as text that starts with `ERROR `.

use jni::EnvUnowned;
use jni::errors::{Error, ThrowRuntimeExAndDefault};
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jfloat, jint, jlong};

fn said(result: Result<String, String>) -> String {
    result.unwrap_or_else(|why| format!("ERROR {why}"))
}

/// `Core.version()`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Core_version<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> { JString::from_str(env, crate::version()) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Core.start(dir, dataKey, name, osVersion, dev)`: see [`crate::startup::start`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Core_start<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    dir: JString<'caller>,
    key: JByteArray<'caller>,
    name: JString<'caller>,
    os_version: JString<'caller>,
    dev: JString<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let key = env.convert_byte_array(&key)?;
        let outcome =
            crate::startup::start(&dir.to_string(), &key, &name.to_string(), &os_version.to_string(), &dev.to_string());
        JString::from_str(env, said(outcome))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Core.networks(facts)`: see [`crate::startup::networks_changed`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Core_networks<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    facts: JByteArray<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let facts = env.convert_byte_array(&facts)?;
        JString::from_str(env, said(crate::startup::networks_changed(&facts)))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.font()`: the interface's face, Instrument Sans, which the library carries for every system.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_font<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JByteArray<'caller> {
    env.with_env(|env| -> Result<_, Error> { env.byte_array_from_slice(swapdesk_paint::INSTRUMENT_SANS) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.monoMetrics(ascent, descent)`: see [`crate::phone_screen::set_mono_metrics`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_monoMetrics<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ascent: jfloat,
    descent: jfloat,
) {
    env.with_env(|_| -> Result<_, Error> {
        crate::phone_screen::set_mono_metrics(ascent, descent);
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.paint(widthPx, heightPx, density)`: see [`crate::phone_screen::paint`]. Main thread only.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_paint<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    width_px: jint,
    height_px: jint,
    density: jfloat,
) -> JByteArray<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        env.byte_array_from_slice(&crate::phone_screen::paint(width_px, height_px, density))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.measured(answer)`: see [`crate::phone_screen::measured`]. Empty when taken. Main thread only.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_measured<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    answer: JByteArray<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let answer = env.convert_byte_array(&answer)?;
        let said = match crate::phone_screen::measured(&answer) {
            Ok(_) => String::new(),
            Err(why) => format!("ERROR {why}"),
        };
        JString::from_str(env, said)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.attach()`: keeps the class for the waker's call into `Shell.wake()` (see `crate::ui_waker`). Main thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_attach<'caller>(
    mut env: EnvUnowned<'caller>,
    class: JClass<'caller>,
) {
    env.with_env(|env| -> Result<_, Error> { crate::ui_waker::attach(env, &class) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// A native method of `Shell` that only calls the phone's shell (`swapdesk_shell_android::PhoneShell`) and hands a
/// plain value back: `$default` when there is no shell yet. Main thread only, like every method of `Shell`.
macro_rules! shell_call {
    ($(#[$doc:meta])* $java:ident($($arg:ident: $ty:ty),*) -> $ret:ty, $default:expr, |$shell:ident| $body:expr) => {
        $(#[$doc])*
        #[unsafe(no_mangle)]
        pub extern "system" fn $java<'caller>(
            mut env: EnvUnowned<'caller>,
            _class: JClass<'caller>,
            $($arg: $ty),*
        ) -> $ret {
            env.with_env(|_| -> Result<_, Error> {
                Ok(crate::phone_screen::with_shell(|$shell| $body).unwrap_or($default))
            })
            .resolve::<ThrowRuntimeExAndDefault>()
        }
    };
}

shell_call!(
    /// `Shell.press(x, y)`: a finger is down and stays, at this point of the view in device pixels.
    Java_io_github_devbertini_swapdesk_Shell_press(x: jfloat, y: jfloat) -> (), (), |shell| shell.press(x, y)
);
shell_call!(
    /// `Shell.release(x, y)`: the finger came up where it went down: a tap.
    Java_io_github_devbertini_swapdesk_Shell_release(x: jfloat, y: jfloat) -> (), (), |shell| shell.release(x, y)
);
shell_call!(
    /// `Shell.cancel()`: the touch became a drag or was taken away.
    Java_io_github_devbertini_swapdesk_Shell_cancel() -> (), (), |shell| shell.cancel()
);
shell_call!(
    /// `Shell.scrollsAt(x, y)`: whether a drag that starts here moves the pane.
    Java_io_github_devbertini_swapdesk_Shell_scrollsAt(x: jfloat, y: jfloat) -> jboolean, false, |shell| shell
        .scrolls_at(x, y)
);
shell_call!(
    /// `Shell.scrollBy(dy)`: moves the pane by `dy` device pixels; whether it moved.
    Java_io_github_devbertini_swapdesk_Shell_scrollBy(dy: jfloat) -> jboolean, false, |shell| shell.scroll_by(dy)
);
shell_call!(
    /// `Shell.scrollTo(y)`: puts the pane at `y` device pixels from its top; whether it moved.
    Java_io_github_devbertini_swapdesk_Shell_scrollTo(y: jfloat) -> jboolean, false, |shell| shell.scroll_to(y)
);
shell_call!(
    /// `Shell.scrollOffset()`: the pane's offset, in device pixels.
    Java_io_github_devbertini_swapdesk_Shell_scrollOffset() -> jfloat, 0.0, |shell| shell.scroll_range().0
);
shell_call!(
    /// `Shell.scrollMax()`: the pane's largest offset, in device pixels.
    Java_io_github_devbertini_swapdesk_Shell_scrollMax() -> jfloat, 0.0, |shell| shell.scroll_range().1
);
shell_call!(
    /// `Shell.reveal(field)`: scrolls the pane so that a text field is in sight; whether it moved.
    Java_io_github_devbertini_swapdesk_Shell_reveal(field: jint) -> jboolean, false, |shell| u8::try_from(field)
        .ok()
        .and_then(swapdesk_shell_android::phone_shell::edit_of)
        .is_some_and(|edit| shell.reveal(edit))
);
shell_call!(
    /// `Shell.takesBack()`: whether Android's back gesture is the app's to answer now.
    Java_io_github_devbertini_swapdesk_Shell_takesBack() -> jboolean, false, |shell| shell.takes_back()
);
shell_call!(
    /// `Shell.back()`: Android's back gesture; false when it is the system's.
    Java_io_github_devbertini_swapdesk_Shell_back() -> jboolean, false, |shell| shell.back()
);
shell_call!(
    /// `Shell.waitsForData()`: whether something the screens show is still on its way from the service.
    Java_io_github_devbertini_swapdesk_Shell_waitsForData() -> jboolean, false, |shell| shell.waits_for_data()
);
shell_call!(
    /// `Shell.armPlaceholders()`: what still waits shows as placeholders (100 ms after it was asked for).
    Java_io_github_devbertini_swapdesk_Shell_armPlaceholders() -> (), (), |shell| shell.arm_placeholders()
);
shell_call!(
    /// `Shell.tick()`: a second went by.
    Java_io_github_devbertini_swapdesk_Shell_tick() -> (), (), |shell| shell.tick()
);
shell_call!(
    /// `Shell.wantsFrame()`: whether the screen is behind the state.
    Java_io_github_devbertini_swapdesk_Shell_wantsFrame() -> jboolean, false, |shell| shell.wants_frame()
);
shell_call!(
    /// `Shell.chose(index)`: the answer to a list of choices; a negative index when it was closed without one.
    Java_io_github_devbertini_swapdesk_Shell_chose(index: jint) -> (), (), |shell| shell
        .chose(usize::try_from(index).ok())
);
shell_call!(
    /// `Shell.setClock(nowMs, utcOffsetMin)`: the phone's time and its offset from UTC.
    Java_io_github_devbertini_swapdesk_Shell_setClock(now_ms: jlong, utc_offset_min: jint) -> (), (), |shell| shell
        .set_clock(u64::try_from(now_ms).unwrap_or(0), utc_offset_min)
);
shell_call!(
    /// `Shell.setDark(dark)`: the system's appearance.
    Java_io_github_devbertini_swapdesk_Shell_setDark(dark: jboolean) -> (), (), |shell| shell.set_dark(dark)
);

/// `Shell.showLive(language, dark)`: see [`crate::startup::show_live`]. Main thread only.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_showLive<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    language: JString<'caller>,
    dark: jboolean,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let language: String = language.to_string();
        JString::from_str(env, said(crate::startup::show_live(&language, dark)))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.localNetwork(state)`: see [`crate::startup::local_network`]. Main thread only.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_localNetwork<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    state: jint,
) {
    env.with_env(|_| -> Result<_, Error> {
        crate::startup::local_network(state);
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.drain()`: the backend has news (the waker posted). Main thread only.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_drain<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) {
    env.with_env(|_| -> Result<_, Error> {
        // First: news that arrives while this drains posts again.
        crate::ui_waker::waker().drained();
        let _ = crate::phone_screen::with_shell(|shell| shell.drain());
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.takeAsks()`: what only Android can do, queued since the last call (`PhoneShell::take_asks`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_takeAsks<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JByteArray<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let asks = crate::phone_screen::with_shell(|shell| shell.take_asks()).unwrap_or_else(|| vec![0]);
        env.byte_array_from_slice(&asks)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.edits()`: the native text fields of the last frame (`PhoneShell::edits`); empty with no screen.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_edits<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JByteArray<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let edits = crate::phone_screen::with_shell(|shell| shell.edits()).unwrap_or_default();
        env.byte_array_from_slice(&edits)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.submitEdit(field, text)`: the text of a native field, when it acts (`PhoneShell::submit_edit`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_submitEdit<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    field: jint,
    text: JString<'caller>,
) {
    env.with_env(|_| -> Result<_, Error> {
        let text: String = text.to_string();
        let edit = u8::try_from(field).ok().and_then(swapdesk_shell_android::phone_shell::edit_of);
        if let Some(edit) = edit {
            let _ = crate::phone_screen::with_shell(|shell| shell.submit_edit(edit, text));
        }
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Shell.cost()`: what the last frame cost, for the trace: `lists=N labels=N forced=0|1`. Main thread only.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Shell_cost<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let cost = crate::phone_screen::cost();
        JString::from_str(env, format!("lists={} labels={} forced={}", cost.lists, cost.labels, u8::from(cost.forced)))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// The development harness's native methods (`DevHarness`, in the app's debug sources only).
#[cfg(feature = "dev-harness")]
mod dev {
    use super::*;

    /// `DevHarness.selfTest()`. It opens sockets and waits for a handshake, so Java calls it off the main thread.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_devbertini_swapdesk_DevHarness_selfTest<'caller>(
        mut env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> JString<'caller> {
        env.with_env(|env| -> Result<_, Error> { JString::from_str(env, crate::selftest::run()) })
            .resolve::<ThrowRuntimeExAndDefault>()
    }

    /// `DevHarness.probe(spec)`: the reachability check of a pairing endpoint. Off the main thread.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_devbertini_swapdesk_DevHarness_probe<'caller>(
        mut env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        spec: JString<'caller>,
    ) -> JString<'caller> {
        env.with_env(|env| -> Result<_, Error> {
            let spec: String = spec.to_string();
            JString::from_str(env, crate::selftest::probe(&spec))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
    }

    /// `DevHarness.mock(scenario)`: shows a state of the design's mock (`swapdesk_ui::mock`), as the desktop apps'
    /// `--mock` does. Returns `OK <width> <height>`, the design size of its window in DIPs. Main thread only.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_devbertini_swapdesk_DevHarness_mock<'caller>(
        mut env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        scenario: JString<'caller>,
    ) -> JString<'caller> {
        env.with_env(|env| -> Result<_, Error> {
            let name: String = scenario.to_string();
            let said = match swapdesk_ui::mock::Scenario::from_name(&name) {
                Some(scenario) => {
                    // The mock's own backend answers from its script, as behind the desktops' `--mock`.
                    let backend = swapdesk_ui::mock::MockBackend::new(crate::ui_waker::waker());
                    crate::phone_screen::show(scenario.prepared_state(), std::sync::Arc::new(backend), false);
                    let (w, h) = swapdesk_paint::design_size(swapdesk_ui::WindowKind::Main);
                    format!("OK {w} {h}")
                }
                None => format!("ERROR no scenario named {name}"),
            };
            JString::from_str(env, said)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
    }

    /// `DevHarness.state()`: where the screen is, for a script that drives it by real touches: the pane, the
    /// computer whose page it is, the pane's offset and range, what stands over it, how many computers are listed,
    /// how many commands went to the backend, and every control of the last frame with the middle of its box in
    /// device pixels of the view. Main thread.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_devbertini_swapdesk_DevHarness_state<'caller>(
        mut env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
    ) -> JString<'caller> {
        env.with_env(|env| -> Result<_, Error> {
            use std::fmt::Write as _;
            let said = crate::phone_screen::with_shell(|shell| {
                let st = shell.state();
                let (offset, max) = shell.scroll_range();
                let over = if st.incoming_session_open() {
                    "request"
                } else if st.dialog(swapdesk_ui::WindowKind::Main).is_some() {
                    "dialog"
                } else if st.pairing().is_some() {
                    "pairing"
                } else {
                    "-"
                };
                let mut s = format!(
                    "nav={:?} device={} scroll={offset:.0}/{max:.0} over={over} dark={} computers={} renaming={} sent={}",
                    st.nav(),
                    st.selected_device().map_or("-", |d| d.name.as_str()),
                    st.dark(),
                    st.devices().len(),
                    st.renaming().is_some(),
                    shell.commands_sent(),
                );
                // The live state: whether the computer on the page is paired, the session with it, the pairing
                // code on the screen (which a person compares with the other screen's; the script does the same).
                if let Some(d) = st.selected_device() {
                    let session = st.open_session_with(&d.id).is_some();
                    let _ = write!(s, " paired={} session={session} tab={:?}", d.paired, st.tab());
                }
                if let Some(swapdesk_ui::PairingState::ShowCode { code, .. }) = st.pairing().map(|p| &p.state) {
                    let _ = write!(s, " code={}", code.replace(' ', ""));
                }
                if let Some(this) = st.this_device() {
                    let _ = write!(s, " ready={:?}", this.ready);
                }
                let _ = write!(s, " local={:?}", st.local_network());
                // The first notice as the screens word it: its title, its text and its button.
                if let Some(kind) = st.notices().first() {
                    let (title, body, button) = st.notice_texts(*kind);
                    let _ = write!(s, " notice=[{title};;{body};;{button}]");
                }
                if let Some(out) = shell.output() {
                    for w in &out.widgets {
                        let (x, y) = w.rect.center();
                        let _ = write!(s, " | {:?}@{:.0},{:.0}", w.id, x * shell.density(), y * shell.density());
                    }
                }
                s
            })
            .unwrap_or_else(|| "no screen".to_owned());
            JString::from_str(env, said)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
    }

    /// `DevHarness.add(address)`: adds a computer at `address`, which may carry a port (`10.0.2.2:47850`: the
    /// development instance of the service on the PC that runs the emulator; the address field of the screens takes
    /// an address alone). Main thread.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_devbertini_swapdesk_DevHarness_add<'caller>(
        mut env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        address: JString<'caller>,
    ) {
        env.with_env(|_| -> Result<_, Error> {
            let ipv4: String = address.to_string();
            let _ = crate::phone_screen::with_shell(|shell| {
                shell.send(vec![swapdesk_ui::Command::AddByAddress { ipv4 }]);
            });
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>()
    }

    /// `DevHarness.run(spec)`: starts a scripted run ([`crate::harness::run`]) and returns at once.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_devbertini_swapdesk_DevHarness_run<'caller>(
        mut env: EnvUnowned<'caller>,
        _class: JClass<'caller>,
        spec: JString<'caller>,
    ) -> JString<'caller> {
        env.with_env(|env| -> Result<_, Error> {
            let spec: String = spec.to_string();
            JString::from_str(env, crate::harness::run(&spec))
        })
        .resolve::<ThrowRuntimeExAndDefault>()
    }
}
