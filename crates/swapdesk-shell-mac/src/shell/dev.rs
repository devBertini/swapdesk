//! Development only: what a script needs to drive the windows the way a person does, for the app's `--ui-script`
//! mode (a dev-harness build; see `swapdesk-app-mac`'s `ui_script.rs`). Every action goes through the window as
//! AppKit sends input (`sendEvent:`): a click lands on the painted control, keys go to whatever has the keyboard, and
//! over the video rectangle they must reach the video view below the canvas.
//!
//! Apple documentation: `NSWindow.sendEvent(_:)`, `NSEvent.mouseEvent(with:location:...)`,
//! `NSWindow.firstResponder`, `NSView.convert(_:to:)`; `screencapture(1)` (`-l <window id>`, `-o` no shadow, `-x` no
//! sound).

use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::Path;

use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::NSPoint;

use super::*;

/// The window a script names: `main`, `viewer` or `banner`.
fn kind_of(window: &str) -> Result<WindowKind, String> {
    match window {
        "main" => Ok(WindowKind::Main),
        "viewer" => Ok(WindowKind::Viewer),
        "banner" => Ok(WindowKind::Banner),
        other => Err(format!("unknown window {other:?} (main, viewer, banner)")),
    }
}

fn canvas_of(shell: &Shell, window: &str) -> Result<(WindowKind, Retained<CanvasView>), String> {
    let kind = kind_of(window)?;
    shell.canvas(kind).map(|c| (kind, c)).ok_or_else(|| format!("there is no {window} window now"))
}

/// The controls of a window as a script names them: the `Debug` text of each id, `(disabled)` after a disabled one.
fn control_names(state: &UiState, kind: WindowKind) -> String {
    let names: Vec<String> = state
        .controls(kind)
        .into_iter()
        .filter(|c| c.id.focusable())
        .map(|c| if c.disabled { format!("{:?}(disabled)", c.id) } else { format!("{:?}", c.id) })
        .collect();
    names.join(" ")
}

fn with<R>(f: impl FnOnce(&Rc<Shell>) -> Result<R, String>) -> Result<R, String> {
    with_shell(f).unwrap_or_else(|| Err("the shell is not running".into()))
}

/// A mouse click on the painted control `control` (its `Debug` text, as [`dev_state`] lists it) of `window`.
pub fn dev_click(window: &str, control: &str) -> Result<(), String> {
    with(|s| {
        let (kind, canvas) = canvas_of(s, window)?;
        let id = {
            let state = s.state.borrow();
            let found = state.controls(kind).into_iter().find(|c| format!("{:?}", c.id) == control);
            match found {
                Some(c) if c.disabled => return Err(format!("{control} is disabled")),
                Some(c) => c.id,
                None => return Err(format!("no control {control}; there are: {}", control_names(&state, kind))),
            }
        };
        if !canvas.paint_now() {
            return Err(format!("the {window} window did not paint"));
        }
        if canvas.send_click(id) { Ok(()) } else { Err(format!("{control} is not painted in the {window} window")) }
    })
}

/// Key presses to whatever has the keyboard in `window` (a text field after a click on it). `\r` is Return, `\t`
/// is Tab.
pub fn dev_keys(window: &str, text: &str) -> Result<(), String> {
    with(|s| {
        let (_, canvas) = canvas_of(s, window)?;
        if canvas.send_keys(text) { Ok(()) } else { Err(format!("the keys could not be sent to the {window} window")) }
    })
}

/// Input on the viewer's video rectangle, at the fraction (`fx`, `fy`) of it from its top-left corner: `click` (the
/// press and the release must reach the video view through the canvas, and it must then have the keyboard), `move`,
/// or `keys` with `text` (to the window's first responder, which a click made the video view).
pub fn dev_video(action: &str, fx: f64, fy: f64, text: &str) -> Result<(), String> {
    with(|s| {
        let viewer = s.viewer.borrow();
        let v = viewer.as_ref().ok_or("there is no viewer window now")?;
        let is_video_first = || {
            v.window
                .firstResponder()
                .is_some_and(|r| Retained::as_ptr(&r).cast::<c_void>() == Retained::as_ptr(&v.video).cast::<c_void>())
        };
        let bounds = v.video.bounds();
        // The video view is flipped: its own coordinates grow down from its top-left corner.
        let local = NSPoint::new(bounds.size.width * fx, bounds.size.height * fy);
        let at = v.video.convertPoint_toView(local, None);
        let mouse = |kind: NSEventType| -> Result<(), String> {
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind,
                at,
                NSEventModifierFlags::empty(),
                0.0,
                v.window.windowNumber(),
                None,
                0,
                1,
                1.0,
            )
            .ok_or("the mouse event could not be made")?;
            if kind == NSEventType::MouseMoved {
                // AppKit gives a real move to the owner of the tracking area under the pointer: the canvas, which
                // covers the window (`sendEvent:` would reach only the first responder of a key window).
                v.canvas.mouseMoved(&event);
            } else {
                v.window.sendEvent(&event);
            }
            Ok(())
        };
        match action {
            "click" => {
                mouse(NSEventType::LeftMouseDown)?;
                mouse(NSEventType::LeftMouseUp)?;
                if is_video_first() {
                    Ok(())
                } else {
                    Err("the click did not reach the video view: it does not have the keyboard".into())
                }
            }
            "move" => mouse(NSEventType::MouseMoved),
            "keys" => {
                if !is_video_first() {
                    return Err("the video view does not have the keyboard: click it first".into());
                }
                if v.canvas.send_keys(text) { Ok(()) } else { Err("the keys could not be sent".into()) }
            }
            other => Err(format!("unknown video action {other:?} (click, move, keys)")),
        }
    })
}

/// A picture of `window` as it is on screen (title bar and video included) at `path`, taken by `screencapture`; it
/// needs the Mac unlocked.
pub fn dev_shot(window: &str, path: &Path) -> Result<(), String> {
    let number = with(|s| match kind_of(window)? {
        WindowKind::Main => Ok(s.main.window.windowNumber()),
        WindowKind::Viewer => {
            s.viewer.borrow().as_ref().map(|v| v.window.windowNumber()).ok_or("there is no viewer window now".into())
        }
        WindowKind::Banner => {
            s.banner.borrow().as_ref().map(|b| b.panel.windowNumber()).ok_or("there is no banner now".into())
        }
    })?;
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-x", "-o", &format!("-l{number}"), &path.to_string_lossy()])
        .status()
        .map_err(|e| format!("screencapture: {e}"))?;
    if status.success() && path.exists() { Ok(()) } else { Err(format!("screencapture ended with {status}")) }
}

/// What the windows show now, one `key=value` per line: the screen, the devices, the pairing, the sessions, each
/// window's controls by name, the text fields and, for a viewer, where its video view sits against the rectangle the
/// frame left for it.
pub fn dev_state() -> String {
    with_shell(|s| {
        let state = s.state.borrow();
        let mut out = String::new();
        let _ = writeln!(out, "screen={:?}", state.screen());
        for (i, d) in state.devices().iter().enumerate() {
            let _ = writeln!(out, "device.{i}={}|{}|paired={}|{:?}", d.id, d.name, d.paired, d.status);
        }
        let _ = writeln!(out, "selected={}", state.selected_device().map_or("none", |d| d.id.as_str()));
        match state.pairing() {
            Some(p) => {
                let _ = writeln!(out, "pairing={:?}", p.state);
            }
            None => out.push_str("pairing=none\n"),
        }
        let _ = writeln!(out, "incoming={}", state.incoming().is_some());
        let _ = writeln!(out, "dialog.main={:?}", state.dialog(WindowKind::Main));
        match state.viewer_session() {
            Some(v) => {
                let _ = writeln!(out, "viewer_session={}|{:?}", v.id, v.state);
            }
            None => out.push_str("viewer_session=none\n"),
        }
        let _ = writeln!(out, "host_session={}", state.host_session().is_some());
        for (name, kind) in [("main", WindowKind::Main), ("viewer", WindowKind::Viewer), ("banner", WindowKind::Banner)]
        {
            if s.canvas(kind).is_some() {
                let _ = writeln!(out, "window.{name}.controls={}", control_names(&state, kind));
            }
        }
        drop(state);
        for line in s.main.canvas.fields_dump().lines() {
            let text = line
                .split_whitespace()
                .nth(1)
                .and_then(|id| {
                    [EditId::Address, EditId::Name, EditId::Rename].into_iter().find(|e| format!("{e:?}") == id)
                })
                .and_then(|e| s.main.canvas.edit_text(e));
            let _ = writeln!(out, "main.{line} text={text:?}");
        }
        let _ = writeln!(out, "main.focus={:?}", s.main.canvas.focus());
        if let Some(v) = s.viewer.borrow().as_ref() {
            let f = v.video.frame();
            let _ = writeln!(
                out,
                "viewer.video.frame={:.2} {:.2} {:.2} {:.2}",
                f.origin.x, f.origin.y, f.size.width, f.size.height
            );
            if let Some(r) = v.canvas.video_rect() {
                let _ = writeln!(out, "viewer.video.rect={:.2} {:.2} {:.2} {:.2}", r.x, r.y, r.w, r.h);
            }
            let c = v.content.frame();
            let _ = writeln!(out, "viewer.content.size={:.2} {:.2}", c.size.width, c.size.height);
            let first = v.window.firstResponder().map(|r| r.class().name().to_string_lossy().into_owned());
            let _ = writeln!(out, "viewer.first_responder={}", first.as_deref().unwrap_or("none"));
            let _ = writeln!(out, "viewer.key_window={}", v.window.isKeyWindow());
            let (seen, taken) = v.video.mouse_seen();
            let _ = writeln!(out, "viewer.video.mouse_seen={seen} taken={taken}");
        }
        out
    })
    .unwrap_or_else(|| "shell=none\n".to_owned())
}
