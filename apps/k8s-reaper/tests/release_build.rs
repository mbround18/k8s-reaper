//! The test-only hang hook (`src/dev_hang.rs`) is compiled into debug builds
//! only (T127). The binary Cargo built for this test run either names its
//! variable or does not: a debug build must (so this test can see it at all),
//! a release build must not.
//!
//! `cargo test` checks the debug side; `cargo test --release -p k8s-reaper
//! --test release_build` checks the side that ships.

const VAR: &[u8] = b"REAPER_DEV_HANG_AFTER";

#[test]
fn the_hang_hook_is_only_in_debug_builds() {
    let path = env!("CARGO_BIN_EXE_k8s-reaper");
    let binary = std::fs::read(path).expect("the k8s-reaper binary");
    let present = binary.windows(VAR.len()).any(|w| w == VAR);
    if cfg!(debug_assertions) {
        assert!(present, "debug build of {path} has no hang hook");
    } else {
        assert!(!present, "release build of {path} contains the hang hook");
    }
}
