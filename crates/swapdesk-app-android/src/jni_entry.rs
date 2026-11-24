//! The JNI entry points: the native methods of `io.github.devbertini.swapdesk.Core` and, in a development build
//! only (cargo feature `dev-harness`), of `DevHarness`.
//!
//! Each one upgrades the caller's environment with `with_env` before anything else, which also keeps a panic from
//! unwinding into the Java VM: an error or a panic becomes a `RuntimeException` there. A refusal the app expects
//! (a bad key, a folder that cannot be made) is not an exception: it comes back as text that starts with `ERROR `.

use jni::EnvUnowned;
use jni::errors::{Error, ThrowRuntimeExAndDefault};
use jni::objects::{JByteArray, JClass, JString};

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

/// `Core.start(dir, dataKey, name, osVersion, dev)`: see [`crate::app::start`].
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
            crate::app::start(&dir.to_string(), &key, &name.to_string(), &os_version.to_string(), &dev.to_string());
        JString::from_str(env, said(outcome))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Core.networks(facts)`: see [`crate::app::networks_changed`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Core_networks<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    facts: JByteArray<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let facts = env.convert_byte_array(&facts)?;
        JString::from_str(env, said(crate::app::networks_changed(&facts)))
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
