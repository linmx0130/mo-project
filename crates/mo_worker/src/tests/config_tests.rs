//! Unit tests for the `config` module — production code lives in
//! `mo_worker/src/config.rs`. Wired from there with `#[cfg(test)] #[path = "tests/config_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;

#[test]
fn depth_cap_is_one() {
    assert_eq!(MAX_SUBAGENT_DEPTH, 1);
}
