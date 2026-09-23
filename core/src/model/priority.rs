//! Package priority (`ARCHITECTURE.md` §4): "High / Normal / Low, then
//! queue order." Ties between packages of equal priority are broken by
//! their position in `queue.json`, not by anything on this type.

use serde::{Deserialize, Serialize};

/// Declared low-to-high so the derived `Ord` makes `High > Normal > Low`:
/// sorting a scheduling queue by priority then wants `.max()` / a
/// descending sort to put `High` first, which reads naturally with this
/// declaration order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_outranks_normal_outranks_low() {
        assert!(Priority::High > Priority::Normal);
        assert!(Priority::Normal > Priority::Low);
        assert!(Priority::High > Priority::Low);
    }

    #[test]
    fn descending_sort_puts_high_first() {
        let mut priorities = vec![Priority::Low, Priority::High, Priority::Normal];
        priorities.sort_by(|a, b| b.cmp(a));
        assert_eq!(
            priorities,
            [Priority::High, Priority::Normal, Priority::Low]
        );
    }

    #[test]
    fn default_is_normal() {
        assert_eq!(Priority::default(), Priority::Normal);
    }
}
