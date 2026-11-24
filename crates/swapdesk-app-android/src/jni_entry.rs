//! The JNI entry points: the native methods of `io.github.devbertini.swapdesk.Core` and `Shell` and, in a
//! development build only (cargo feature `dev-harness`), of `DevHarness`.
//!
//! Each one upgrades the caller's environment with `with_env` before anything else, which also keeps a panic from
//! unwinding into the Java VM: an error or a panic becomes a `RuntimeException` there. A refusal the app expects
//! (a bad key, a folder that cannot be made) is not an exception: it comes back as text that starts with `ERROR `.

use jni::EnvUnowned;
use jni::errors::{Error, ThrowRuntimeExAndDefault};
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jfloat, jint};

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
                    crate::phone_screen::show(scenario.prepared_state());
                    let (w, h) = swapdesk_paint::design_size(swapdesk_ui::WindowKind::Main);
                    format!("OK {w} {h}")
                }
                None => format!("ERROR no scenario named {name}"),
            };
            JString::from_str(env, said)
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
