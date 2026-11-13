//! The transfers list's view model: the session's own transfers, resume, cancel and the saved-as outcome.

use crate::api::{ApiCode, Command, TransferState, UiEvent};
use crate::interact::ControlId;
use crate::mock::Scenario;
use crate::state::UiState;

fn transfer(s: &mut UiState, id: u32, session_id: u64, name: &str, outgoing: bool, state: TransferState) {
    s.apply(UiEvent::Transfer { transfer_id: id, session_id, name: name.into(), outgoing, state });
}

#[test]
fn transfers_show_progress_resume_cancel_and_the_saved_as_outcome() {
    let mut s = Scenario::Visualizador.prepared_state();
    transfer(&mut s, 5, 1, "relatorio.pdf", true, TransferState::Queued);
    transfer(&mut s, 5, 1, "relatorio.pdf", true, TransferState::Running { done: 10, total: 100, bytes_per_s: 50 });
    transfer(&mut s, 6, 1, "praia.jpg", false, TransferState::Paused { done: 40, total: 100, why: ApiCode::Timeout });
    transfer(&mut s, 7, 1, "foto.jpg", false, TransferState::Done);
    // A transfer of another session (this machine hosting) is not this window's.
    transfer(&mut s, 9, 777, "alheio.bin", false, TransferState::Done);
    assert_eq!(s.viewer_transfers().iter().map(|t| t.id).collect::<Vec<_>>(), [5, 6, 7]);

    assert_eq!(s.activate(ControlId::TransferResume(6)).commands, vec![Command::TransferResume { transfer_id: 6 }]);
    assert_eq!(s.activate(ControlId::TransferCancel(5)).commands, vec![Command::TransferCancel { transfer_id: 5 }]);
    assert_eq!(
        s.viewer_transfers().iter().map(|t| t.id).collect::<Vec<_>>(),
        [6, 7],
        "a cancelled transfer leaves the list at once"
    );

    // Files never overwrite: the name the file got is shown beside "Recebido".
    s.apply(UiEvent::TransferSavedAs { transfer_id: 7, session_id: 1, name: "foto (1).jpg".into() });
    let t = s.viewer_transfers()[1].clone();
    assert_eq!(
        (t.name.as_str(), t.saved_as.as_deref(), t.state.clone()),
        ("foto.jpg", Some("foto (1).jpg"), TransferState::Done)
    );
    s.apply(UiEvent::TransferSavedAs { transfer_id: 404, session_id: 1, name: "x".into() });
    assert_eq!(s.viewer_transfers().len(), 2, "a note about no transfer changes nothing");
    transfer(&mut s, 6, 1, "praia.jpg", false, TransferState::Failed { reason: ApiCode::NotPermitted });
    assert!(matches!(s.viewer_transfers()[0].state, TransferState::Failed { reason: ApiCode::NotPermitted }));
}

#[test]
fn a_transfer_that_failed_for_lack_of_space_says_so_and_any_other_failure_says_it_did_not_finish() {
    let mut s = Scenario::Visualizador.prepared_state();
    transfer(&mut s, 1, 1, "filme.mkv", true, TransferState::Failed { reason: ApiCode::NoSpace });
    transfer(&mut s, 2, 1, "foto.jpg", true, TransferState::Failed { reason: ApiCode::Internal });
    let text = |s: &UiState, i: usize| match s.viewer_transfers()[i].state {
        TransferState::Failed { reason } => s.transfer_failure_text(reason),
        _ => unreachable!(),
    };
    assert_eq!(text(&s, 0), "Sem espaço livre no outro computador");
    assert_eq!(text(&s, 1), "Não terminou");
}
