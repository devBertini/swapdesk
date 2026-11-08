use std::collections::HashMap;
use std::sync::Mutex;

use swapdesk_platform::{Result, SecretStore};

use crate::lock;

/// A `SecretStore` in memory. Deleting a name that is not there succeeds.
#[derive(Default)]
pub struct MemorySecretStore(Mutex<HashMap<String, Vec<u8>>>);

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = lock(&self.0).keys().cloned().collect();
        names.sort();
        names
    }
}

impl SecretStore for MemorySecretStore {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(lock(&self.0).get(name).cloned())
    }

    fn store(&self, name: &str, bytes: &[u8]) -> Result<()> {
        lock(&self.0).insert(name.to_string(), bytes.to_vec());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<()> {
        lock(&self.0).remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_load_overwrite_delete() {
        let s = MemorySecretStore::new();
        assert_eq!(s.load("device.key").unwrap(), None);
        s.store("device.key", &[1, 2, 3]).unwrap();
        s.store("other", &[9]).unwrap();
        assert_eq!(s.load("device.key").unwrap(), Some(vec![1, 2, 3]));
        s.store("device.key", &[4]).unwrap();
        assert_eq!(s.load("device.key").unwrap(), Some(vec![4]));
        assert_eq!(s.names(), vec!["device.key".to_string(), "other".to_string()]);
        s.delete("device.key").unwrap();
        s.delete("device.key").unwrap();
        assert_eq!(s.load("device.key").unwrap(), None);
    }
}
