//! How the app was launched (D32): by the person, or by the system as a login item.
//!
//! The launch Apple event (`kAEOpenApplication`) carries `keyAEPropData` = `keyAELaunchedAsLogInItem` when the app was
//! started as a login item (Apple: CoreServices `keyAELaunchedAsLogInItem`, "the receiving application was launched as
//! a login item and should only perform actions suitable to that environment"). While the event is handled,
//! `NSAppleEventManager.sharedAppleEventManager.currentAppleEvent` returns it
//! (developer.apple.com/documentation/foundation/nsappleeventmanager/currentappleevent). The two four-character codes
//! were checked against the macOS SDK header on the MacBook (09/11).
//!
//! The decision is pure ([`is_login_launch`]); only reading the event is macOS.

/// A four-character code as the big-endian `u32` the Apple Event Manager uses.
pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// `keyAEPropData`: the parameter of the launch event that says why the app was launched.
pub const KEY_AE_PROP_DATA: u32 = fourcc(b"prdt");

/// `keyAELaunchedAsLogInItem`: the value of `keyAEPropData` for a login-item launch.
pub const LAUNCHED_AS_LOGIN_ITEM: u32 = fourcc(b"lgit");

/// True when the launch event's `keyAEPropData` (if it has one) says the app was launched as a login item.
pub fn is_login_launch(prop_data: Option<u32>) -> bool {
    prop_data == Some(LAUNCHED_AS_LOGIN_ITEM)
}

/// The enum code in `keyAEPropData` of the Apple event being handled, `None` without an event or without that parameter.
#[cfg(target_os = "macos")]
pub fn current_prop_data() -> Option<u32> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};

    let manager_class = AnyClass::get(c"NSAppleEventManager")?;
    // SAFETY: `NSAppleEventManager` is Foundation's; `sharedAppleEventManager` takes nothing and returns the shared
    // instance (autoreleased, retained here).
    let manager: Retained<AnyObject> = unsafe { msg_send![manager_class, sharedAppleEventManager] };
    // SAFETY: `currentAppleEvent` takes nothing and returns an `NSAppleEventDescriptor` or nil.
    let event: Option<Retained<AnyObject>> = unsafe { msg_send![&*manager, currentAppleEvent] };
    let event = event?;
    // SAFETY: `paramDescriptorForKeyword:` takes an `AEKeyword` (`FourCharCode`, `u32`) and returns a descriptor or nil.
    let param: Option<Retained<AnyObject>> = unsafe { msg_send![&*event, paramDescriptorForKeyword: KEY_AE_PROP_DATA] };
    let param = param?;
    // SAFETY: `enumCodeValue` takes nothing and returns an `OSType` (`u32`); it is 0 when the descriptor is no enum.
    let code: u32 = unsafe { msg_send![&*param, enumCodeValue] };
    Some(code)
}

/// True when the launch event being handled says the app was started as a login item.
#[cfg(target_os = "macos")]
pub fn launched_as_login_item() -> bool {
    is_login_launch(current_prop_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_the_four_characters() {
        assert_eq!(KEY_AE_PROP_DATA, 0x7072_6474);
        assert_eq!(LAUNCHED_AS_LOGIN_ITEM, 0x6c67_6974);
    }

    #[test]
    fn only_the_login_item_code_hides_the_window() {
        assert!(is_login_launch(Some(fourcc(b"lgit"))));
        assert!(!is_login_launch(None));
        assert!(!is_login_launch(Some(0)));
        assert!(!is_login_launch(Some(fourcc(b"svit"))));
    }
}
