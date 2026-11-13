//! The viewer session's transfers as the explorer's transfers list shows them: which ones are this window's, and what
//! "Retomar" and "Cancelar" do.

use crate::state::{TransferView, UiState};

impl UiState {
    /// The transfers of the viewer session shown (the other sessions' belong to the host role).
    pub fn viewer_transfers(&self) -> Vec<&TransferView> {
        let session = self.viewer_session().map(|s| s.id);
        self.transfers.iter().filter(|t| Some(t.session_id) == session).collect()
    }
}

#[cfg(test)]
mod tests;
