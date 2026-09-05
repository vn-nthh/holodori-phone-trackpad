//! Windows-only, reversible routing policy for the opt-in
//! `--local-only-tether` mode.
//!
//! Linux deliberately has no in-process route mutation. The contributed
//! netlink implementation had not been exercised against a live routing
//! table and could mistake a generic USB Ethernet adapter for a phone. The
//! Linux launcher instead delegates the persistent `never-default` profile
//! settings to NetworkManager after resolving one exact `rndis_host` device.

mod windows;
pub use windows::{
    RecoveryOutcome, TetherBinding, TetherRoutePolicy, current_tether_binding,
    recover_orphaned_policy, tether_ipv4_interfaces,
};
