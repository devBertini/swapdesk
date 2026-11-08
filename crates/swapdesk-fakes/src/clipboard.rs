use std::sync::{Arc, Mutex};

use swapdesk_platform::{ClipContent, ClipRead, ClipboardProvider, Result};

use crate::lock;

#[derive(Default)]
struct Inner {
    count: u64,
    content: Option<ClipContent>,
    concealed: bool,
    unsupported: bool,
    writes: Vec<ClipContent>,
}

/// In-memory clipboard with a change counter and a concealed flag (a password manager's copy). Clones share it.
#[derive(Clone, Default)]
pub struct FakeClipboard(Arc<Mutex<Inner>>);

impl FakeClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Something other than Swapdesk copied `content`; `concealed` marks it as hidden from history (password manager).
    pub fn copy_externally(&self, content: ClipContent, concealed: bool) {
        let mut g = lock(&self.0);
        g.count += 1;
        g.content = Some(content);
        g.concealed = concealed;
    }

    /// The clipboard holds a format Swapdesk does not carry.
    pub fn copy_unsupported(&self) {
        let mut g = lock(&self.0);
        g.count += 1;
        g.content = None;
        g.concealed = false;
        g.unsupported = true;
    }

    /// What Swapdesk wrote, in order.
    pub fn writes(&self) -> Vec<ClipContent> {
        lock(&self.0).writes.clone()
    }
}

impl ClipboardProvider for FakeClipboard {
    fn change_count(&mut self) -> Result<u64> {
        Ok(lock(&self.0).count)
    }

    fn read(&mut self) -> Result<ClipRead> {
        let g = lock(&self.0);
        Ok(if g.unsupported {
            ClipRead::Unsupported
        } else if g.concealed && g.content.is_some() {
            ClipRead::Concealed
        } else {
            g.content.clone().map_or(ClipRead::Empty, ClipRead::Content)
        })
    }

    fn write(&mut self, content: &ClipContent) -> Result<()> {
        let mut g = lock(&self.0);
        g.count += 1;
        g.content = Some(content.clone());
        g.concealed = false;
        g.unsupported = false;
        g.writes.push(content.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_counter_concealed_and_unsupported() {
        let mut c = FakeClipboard::new();
        let ext = c.clone();
        assert_eq!(c.read().unwrap(), ClipRead::Empty);
        assert_eq!(c.change_count().unwrap(), 0);
        ext.copy_externally(ClipContent::Text("abc".into()), false);
        assert_eq!(c.change_count().unwrap(), 1);
        assert_eq!(c.read().unwrap(), ClipRead::Content(ClipContent::Text("abc".into())));
        ext.copy_externally(ClipContent::Text("senha".into()), true);
        assert_eq!(c.change_count().unwrap(), 2);
        assert_eq!(c.read().unwrap(), ClipRead::Concealed);
        ext.copy_unsupported();
        assert_eq!(c.read().unwrap(), ClipRead::Unsupported);
        c.write(&ClipContent::Png(vec![1, 2, 3])).unwrap();
        assert_eq!(c.change_count().unwrap(), 4);
        assert_eq!(c.read().unwrap(), ClipRead::Content(ClipContent::Png(vec![1, 2, 3])));
        assert_eq!(ext.writes(), vec![ClipContent::Png(vec![1, 2, 3])]);
    }
}
