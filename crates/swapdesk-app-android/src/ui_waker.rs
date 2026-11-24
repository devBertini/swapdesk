//! The phone's `UiWaker`: the backend's threads tell the main thread there is news. Android only.
//!
//! A wake calls the static Java method `Shell.wake()`, which posts to the main Looper; there the view calls
//! `Shell.drain()`. This is the one place Rust calls into Java, once per burst of news: wakes that arrive before the
//! main thread drained are folded into the first.
//!
//! The class is kept as a global reference taken on the main thread (`Shell.attach()`): a thread that Rust attached
//! to the Java VM looks classes up through the system class loader, which does not know the app's.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use jni::errors::Error;
use jni::objects::JClass;
use jni::refs::Global;
use jni::vm::JavaVM;
use jni::{Env, jni_sig, jni_str};
use swapdesk_ui::UiWaker;

static SHELL_CLASS: OnceLock<Global<JClass<'static>>> = OnceLock::new();
static WAKER: OnceLock<Arc<MainWaker>> = OnceLock::new();

/// Wakes Android's main thread.
pub struct MainWaker {
    /// A wake was posted and the main thread has not drained since.
    posted: AtomicBool,
}

impl MainWaker {
    /// The main thread is about to drain: the next news posts again.
    pub fn drained(&self) {
        self.posted.store(false, Ordering::Release);
    }
}

impl UiWaker for MainWaker {
    fn wake(&self) {
        if self.posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(class) = SHELL_CLASS.get() else {
            // No view yet: it drains when it is attached (`PaintView.onAttachedToWindow`).
            self.posted.store(false, Ordering::Release);
            return;
        };
        let posted = JavaVM::singleton().and_then(|vm| {
            vm.attach_current_thread(|env| -> Result<(), Error> {
                env.call_static_method(class, jni_str!("wake"), jni_sig!(() -> void), &[])?;
                Ok(())
            })
        });
        if let Err(why) = posted {
            self.posted.store(false, Ordering::Release);
            swapdesk_sys_android::logcat::write(swapdesk_platform::Level::Warn, &format!("WAKE not posted: {why}"));
        }
    }
}

/// The app's waker, one for its life.
pub fn waker() -> Arc<MainWaker> {
    Arc::clone(WAKER.get_or_init(|| Arc::new(MainWaker { posted: AtomicBool::new(false) })))
}

/// Keeps `Shell`'s class for [`MainWaker::wake`]. Called on the main thread, where the app's classes are found.
pub fn attach(env: &mut Env, class: &JClass) -> Result<(), Error> {
    if SHELL_CLASS.get().is_none() {
        let global = env.new_global_ref(class)?;
        let _ = SHELL_CLASS.set(global);
    }
    Ok(())
}
