use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_long: u64,
    pub reasoning: u64,
}

impl Usage {
    #[must_use]
    pub fn total_input(self) -> Option<u64> {
        self.input
            .checked_add(self.cache_read)?
            .checked_add(self.cache_write)
    }
}
