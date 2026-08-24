//! Windows-only, reversible routing policy for the opt-in
//! `--local-only-tether` mode.
//!
//! Linux deliberately has no in-process route mutation. The contributed
//! netlink implementation had not been exercised against a live routing
//! table and could mistake a generic USB Ethernet adapter for a phone. Linux
//! users who need a never-default tether should configure that property in
//! their network manager instead.

mod windows;
pub use windows::{
    RecoveryOutcome, TetherBinding, TetherRoutePolicy, current_tether_binding,
    recover_orphaned_policy,
};
