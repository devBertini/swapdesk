//! The viewer session's transfers as the explorer's transfers list shows them: which ones are this window's, and what
//! "Retomar" and "Cancelar" do.

use crate::state::{TransferView, UiState};
use crate::strings::MsgKey;

impl UiState {
    /// The transfers of the viewer session shown (the other sessions' belong to the host role).
    pub fn viewer_transfers(&self) -> Vec<&TransferView> {
        let session = self.viewer_session().map(|s| s.id);
        self.transfers.iter().filter(|t| Some(t.session_id) == session).collect()
    }

    /// The line of a failed transfer's row: "no free space" when the other computer's volume was too full
    /// (F-FILE-03), "did not finish" for every other reason.
    pub fn transfer_failure_text(&self, reason: crate::api::ApiCode) -> String {
        self.name_of(if reason == crate::api::ApiCode::NoSpace {
            MsgKey::TransferNoSpace
        } else {
            MsgKey::TransferFailed
        })
    }
}

#[cfg(test)]
mod tests;
