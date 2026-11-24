//! The JNI entry points: the native methods of `io.github.devbertini.swapdesk.Core`.
//!
//! Each one upgrades the caller's environment with `with_env` before anything else, which also keeps a panic from
//! unwinding into the Java VM: an error or a panic becomes a `RuntimeException` there.

use jni::EnvUnowned;
use jni::errors::{Error, ThrowRuntimeExAndDefault};
use jni::objects::{JClass, JString};

/// `Core.version()`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Core_version<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> { JString::from_str(env, crate::version()) })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Core.probe(address)`: the development harness's reachability check of a pairing endpoint. Off the main thread,
/// as `selfTest`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Core_probe<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    address: JString<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> {
        let address: String = address.to_string();
        JString::from_str(env, crate::selftest::probe(&address))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// `Core.selfTest()`. It opens sockets and waits for a handshake, so Java calls it off the main thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_devbertini_swapdesk_Core_selfTest<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| -> Result<_, Error> { JString::from_str(env, crate::selftest::run()) })
        .resolve::<ThrowRuntimeExAndDefault>()
}
