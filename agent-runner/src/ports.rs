//! Per-run port blocks (spec §5.8): the daemon allocates a unique
//! `REMOTER_AGENT_PORT_BASE` per run so projects can parameterize their devenv
//! service ports and parallel tickets of one project don't fight over a
//! hardcoded port. Blocks are 50 ports wide, released when the run ends.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// First allocatable base.
pub const FIRST_BASE: u16 = 20_000;
/// End of the allocatable range (exclusive): a base is allocatable while
/// `base + BLOCK <= LAST_BASE`, so the last actual base is `LAST_BASE - BLOCK`.
pub const LAST_BASE: u16 = 60_000;
/// Ports per run block.
pub const BLOCK: u16 = 50;

/// An allocated block. Release happens automatically on drop (run end).
#[derive(Debug)]
pub struct PortBlock {
    base: u16,
    in_use: Arc<Mutex<HashSet<u16>>>,
}

impl PortBlock {
    pub fn base(&self) -> u16 {
        self.base
    }
}

impl Drop for PortBlock {
    fn drop(&mut self) {
        self.in_use.lock().unwrap().remove(&self.base);
    }
}

#[derive(Clone, Default)]
pub struct Ports {
    in_use: Arc<Mutex<HashSet<u16>>>,
}

impl Ports {
    /// The smallest free block base, or `None` when the range is exhausted
    /// ((LAST - FIRST) / BLOCK ≈ 800 blocks — exhaustion means a leak).
    pub fn allocate(&self) -> Option<PortBlock> {
        let mut in_use = self.in_use.lock().unwrap();
        let mut base = FIRST_BASE;
        while base + BLOCK <= LAST_BASE {
            if in_use.insert(base) {
                return Some(PortBlock {
                    base,
                    in_use: self.in_use.clone(),
                });
            }
            base += BLOCK;
        }
        None
    }

    #[cfg(test)]
    fn allocated_count(&self) -> usize {
        self.in_use.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_distinct_blocks_and_releases_on_drop() {
        let ports = Ports::default();
        let a = ports.allocate().unwrap();
        let b = ports.allocate().unwrap();
        assert_eq!(a.base(), FIRST_BASE);
        assert_eq!(b.base(), FIRST_BASE + BLOCK);
        assert_eq!(ports.allocated_count(), 2);

        drop(a);
        assert_eq!(ports.allocated_count(), 1);
        // The released block is handed out again.
        let c = ports.allocate().unwrap();
        assert_eq!(c.base(), FIRST_BASE);
    }

    #[test]
    fn never_exceeds_the_range() {
        let ports = Ports::default();
        let mut blocks = Vec::new();
        while let Some(b) = ports.allocate() {
            blocks.push(b);
        }
        assert!(!blocks.is_empty());
        for b in &blocks {
            assert!(b.base() >= FIRST_BASE);
            assert!(b.base() + BLOCK <= LAST_BASE);
        }
    }
}
