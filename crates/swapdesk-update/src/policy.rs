//! What is offered (`docs/UPDATES.md` 4.5): a stable version greater than the running one, for this system, and not
//! one that was installed here and rolled back.

use crate::manifest::{Apply, Channel, FileEntry, Manifest};
use crate::{UpdateError, Version};

/// What a verified manifest means for this install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer<'a> {
    /// Nothing new (or nothing this install takes).
    Nothing,
    /// A newer version, installed with one click.
    Install { version: &'a Version, entry: &'a FileEntry },
    /// A newer version that needs the installer or a step by hand: the app links to its release page.
    Manual { version: &'a Version },
}

/// The offer of `manifest` to an install running `running` on `target`. `rolled_back`: the versions installed here
/// that did not commit; they are not offered again by themselves.
pub fn offer<'a>(
    manifest: &'a Manifest,
    running: &Version,
    target: &str,
    rolled_back: &[Version],
) -> Result<Offer<'a>, UpdateError> {
    // Stable only in 1.1 (owner decision 5), whatever the manifest calls itself.
    if manifest.channel != Channel::Stable || manifest.version.is_pre() {
        return Ok(Offer::Nothing);
    }
    if manifest.version <= *running || rolled_back.contains(&manifest.version) {
        return Ok(Offer::Nothing);
    }
    let entry = manifest.entry(target).ok_or(UpdateError::NoEntry)?;
    Ok(match manifest.apply {
        Apply::Auto => Offer::Install { version: &manifest.version, entry },
        Apply::Manual => Offer::Manual { version: &manifest.version },
    })
}
