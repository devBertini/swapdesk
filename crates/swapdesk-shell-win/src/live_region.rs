//! Live regions for UI Automation. AccessKit raises `LiveRegionChanged` for a node with a `live` property only when
//! the node has a name (`accesskit_windows` 0.35.2, `adapter.rs` lines 256 and 314-324), and the shared tree's
//! status region has no label of its own: its text is in its children. So the shell names each live region after
//! its children's text. The name changes when the card's heading or body changes and not when a node outside the
//! region (the countdown) does, so a screen reader hears the card once per change.

use accesskit::{Live, TreeUpdate};

/// Gives every unnamed live region the text of its children, in order, one per line.
pub(crate) fn name_live_regions(tree: &mut TreeUpdate) {
    let texts: Vec<(usize, String)> = tree
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, (_, n))| n.live().is_some_and(|l| l != Live::Off) && n.label().is_none_or(str::is_empty))
        .map(|(i, (_, n))| {
            let lines: Vec<&str> = n
                .children()
                .iter()
                .filter_map(|c| tree.nodes.iter().find(|(id, _)| id == c))
                .filter_map(|(_, child)| child.label().or_else(|| child.value()))
                .filter(|t| !t.is_empty())
                .collect();
            (i, lines.join("\n"))
        })
        .collect();
    for (i, text) in texts {
        if !text.is_empty() {
            tree.nodes[i].1.set_label(text);
        }
    }
}

/// The text of the live region of a tree, if one has any (what a screen reader reads when it changes).
#[cfg(test)]
pub(crate) fn live_text(tree: &TreeUpdate) -> Option<String> {
    tree.nodes
        .iter()
        .find(|(_, n)| n.live().is_some_and(|l| l != Live::Off))
        .and_then(|(_, n)| n.label().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_ui::mock::{ENTRY, MAC, Scenario};
    use swapdesk_ui::{ConnectWhy, SessionState, UiEvent, UiState, WindowKind, a11y};

    fn tree_of(state: &UiState) -> TreeUpdate {
        let mut t = a11y::build_tree(state, WindowKind::Main, &|_| None, None);
        name_live_regions(&mut t);
        t
    }

    #[test]
    fn the_live_region_is_named_after_the_card_so_uia_raises_live_region_changed() {
        let mut s = Scenario::EstadoAguardandoAceite.prepared_state();
        let before = live_text(&tree_of(&s)).expect("the waiting card has a named live region");
        assert!(before.contains("Esperando"), "{before}");
        // The countdown is outside the region: a tick leaves the name as it was, so nothing is announced.
        s.tick();
        s.tick();
        assert_eq!(live_text(&tree_of(&s)), Some(before.clone()), "a tick of the countdown renames nothing");
        // A new state of the entry changes the card text, so the name changes once.
        s.apply(UiEvent::Session {
            session_id: ENTRY,
            device_id: MAC.into(),
            state: SessionState::NotOpened { why: ConnectWhy::Declined },
        });
        let after = live_text(&tree_of(&s)).expect("the end card has a named live region");
        assert_ne!(after, before);
        assert!(after.contains("recusou"), "{after}");
    }

    #[test]
    fn no_card_means_no_live_region_text() {
        let s = Scenario::Dispositivos.prepared_state();
        assert_eq!(live_text(&tree_of(&s)), None);
    }
}
