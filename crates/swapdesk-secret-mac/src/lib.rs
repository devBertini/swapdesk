//! `SecretStore` for macOS (contracts C3 and C5): every secret is a generic-password item of a keychain, one item per
//! name, and the C5 folder (audit and trace logs) is a directory with a checked owner and mode.
//!
//! Stage 23 (D27) runs `ServiceCore` inside the app process as the logged-in user, so the first variant keeps the
//! secrets in the **user's login keychain** and the folder under `~/Library/Application Support/Swapdesk`. Stage 24 moves
//! the core into the root daemon, whose secrets live in the System keychain and `/Library/Application Support/Swapdesk`
//! (C5.2). That change is only a different [`SecretLocation`]: the store, its names and its item layout stay the same.
//! [`KeychainLocation`] is `#[non_exhaustive]` for the System variant, and [`FolderOwner`] already names root.
//!
//! * [`location`] is pure and builds on every host (the location type and the name rule).
//! * `keychain`, `folder` and `store` exist on macOS only and hold the `unsafe` (Security and CoreFoundation calls,
//!   `geteuid`).
//!
//! Sources (read 2026-11-08): Apple, "Using the keychain to manage user secrets" and "Item attribute keys and values" for the
//! generic-password layout (class, service, account, value data); `SecItemAdd`, `SecItemCopyMatching`, `SecItemUpdate` and
//! `SecItemDelete` in the Security framework reference, <https://developer.apple.com/documentation/security/keychain-services>;
//! the keychain is the file-based login keychain because no data-protection keychain key is set (TN3137,
//! <https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains>); the Rust bindings are `objc2-security`
//! 0.3.2, <https://docs.rs/objc2-security/0.3.2/objc2_security/>.

pub mod location;

#[cfg(target_os = "macos")]
mod folder;
#[cfg(target_os = "macos")]
mod keychain;
#[cfg(target_os = "macos")]
mod store;

pub use location::{FOLDER_NAME, FolderOwner, KeychainLocation, SERVICE_NAME, SecretLocation, check_name};

#[cfg(target_os = "macos")]
pub use folder::ensure_folder;
#[cfg(target_os = "macos")]
pub use store::{MacSecretStore, create_keychain_file, delete_keychain_file, lock_keychain_file};
