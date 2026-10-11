//! A test-only hang, for checking that health notices a stuck loop
//! (002 quickstart §1, step 3).
//!
//! `REAPER_DEV_HANG_AFTER=<n>` makes cycle `n + 1` await forever after it has
//! started, the way a request that never returns would. Debug builds only:
//! `main.rs` compiles this module under `#[cfg(debug_assertions)]`, so a
//! release image cannot be made to hang (`tests/release_build.rs`).

use tracing::warn;

/// The variable that turns the hang on.
pub const VAR: &str = "REAPER_DEV_HANG_AFTER";

/// Counts cycles and hangs the one after the `n`th.
#[derive(Debug, Default)]
pub struct DevHang {
    after: Option<u64>,
    cycles: u64,
}

impl DevHang {
    /// Reads [`VAR`]. Unset means never hang; a value that is not a number
    /// is logged and ignored.
    pub fn from_env() -> Self {
        let raw = std::env::var(VAR).ok();
        let after = parse(raw.as_deref());
        if raw.is_some() && after.is_none() {
            warn!(
                var = VAR,
                value = raw.as_deref(),
                "not a cycle count; ignored"
            );
        }
        if let Some(n) = after {
            warn!(
                var = VAR,
                n, "debug build: the cycle after this many will hang"
            );
        }
        Self { after, cycles: 0 }
    }

    /// Called at the start of each cycle. Returns at once, except in the
    /// cycle that must hang, where it never returns.
    pub async fn cycle(&mut self) {
        self.cycles += 1;
        if hangs(self.after, self.cycles) {
            warn!(cycle = self.cycles, "{VAR}: hanging this cycle");
            std::future::pending::<()>().await;
        }
    }
}

fn parse(raw: Option<&str>) -> Option<u64> {
    raw?.trim().parse().ok()
}

/// Cycles count from 1; with `after = n`, cycle `n + 1` hangs.
fn hangs(after: Option<u64>, cycle: u64) -> bool {
    after.is_some_and(|n| cycle > n)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_cycle_after_n_hangs() {
        assert!(!hangs(None, 1));
        assert!(!hangs(Some(1), 1));
        assert!(hangs(Some(1), 2));
        assert!(hangs(Some(0), 1));
    }

    #[test]
    fn only_a_count_turns_it_on() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("2")), Some(2));
        assert_eq!(parse(Some(" 3 ")), Some(3));
        assert_eq!(parse(Some("")), None);
        assert_eq!(parse(Some("yes")), None);
        assert_eq!(parse(Some("-1")), None);
    }

    #[tokio::test]
    async fn cycle_n_plus_one_never_returns() {
        let mut hang = DevHang {
            after: Some(1),
            cycles: 0,
        };
        hang.cycle().await;
        let second = tokio::time::timeout(Duration::from_millis(50), hang.cycle()).await;
        assert!(second.is_err(), "cycle 2 returned");
    }
}
