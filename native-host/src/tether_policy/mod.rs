//! Reversible routing policy for the opt-in local-only tether mode
//! (`--local-only-tether`).
//!
//! Android USB tethering normally installs a default route through the
//! phone, so the moment a phone is tethered the OS may start sending the
//! PC's general internet traffic over the USB link instead of the LAN/Wi-Fi
//! uplink. This policy suppresses that for the duration of the host process:
//! it finds the tether interface, removes (and keeps re-removing) its
//! default routes while the host runs, and restores exactly what it removed
//! when the guard is dropped. The tether interface itself keeps working for
//! the phone's own subnet the whole time; only its ability to become the
//! system's default gateway is suppressed.
//!
//! This is strictly opt-in and strictly scoped to adapters recognized as
//! Android/USB-tethering adapters. A normal Ethernet or Wi-Fi adapter is
//! never touched merely because it currently holds the default route or
//! because a discovery packet happened to arrive through it.
//!
//! # Platform split
//!
//! The mechanism is entirely OS-specific (Windows `IP Helper` routing calls
//! vs. Linux `rtnetlink`), but the public shape is identical on every
//! supported platform: `new`, `refresh`, `protect_peer`, `restore`, and
//! `Drop`. `bin/host.rs` uses that shared shape and needs no per-platform
//! branching beyond the `#[cfg]` that already selects whether tethering
//! support is compiled in at all.
//!
//! - [`windows`] implements the policy with `GetIpForwardTable2` /
//!   `CreateIpForwardEntry2` / `DeleteIpForwardEntry2` and per-interface
//!   `DisableDefaultRoutes`. This is the shipped, supported implementation on
//!   Windows and is always compiled in there.
//! - [`linux`] implements the policy with raw `AF_NETLINK`/`NETLINK_ROUTE`
//!   requests (`RTM_GETROUTE`/`RTM_DELROUTE`/`RTM_NEWROUTE`) built by hand
//!   with `libc`, deliberately without a netlink crate (see that module's
//!   doc comment for why, and for the safety rules that govern which routes
//!   it is allowed to touch).
//!
//! # Linux support is experimental and off by default
//!
//! The [`linux`] module is kept in the tree as reference only. It is gated
//! behind the non-default `linux-tether-policy` cargo feature and is not
//! part of a default Linux build; `--local-only-tether` is rejected on Linux
//! unless the feature is enabled (see `bin/host.rs`).
//!
//! Why: this module's mutation path (`RTM_DELROUTE`/`RTM_NEWROUTE`) has never
//! been executed against a live routing table. It has unit tests for its
//! encode/parse/alignment logic and read-only `--ignored` checks against a
//! real routing table, but nothing has exercised the actual delete/recreate
//! calls end to end. Separately, on Linux the tether route normally loses to
//! the real uplink on metric anyway, so the Windows rationale for this
//! feature (Windows can install the tether route as *the* default route,
//! stealing all internet traffic) does not automatically carry over.
//!
//! The supported Linux alternative is NetworkManager's `ipv4.never-default
//! yes` setting on the tether connection profile, applied once by the user
//! rather than reasserted by a privileged host process every discovery
//! cycle.
//!
//! To build with the Linux implementation compiled in anyway (development
//! and review only):
//!
//! ```text
//! cargo build --features linux-tether-policy
//! ```

#[cfg(windows)]
mod windows;
#[cfg(all(target_os = "linux", feature = "linux-tether-policy"))]
mod linux;

#[cfg(windows)]
pub use windows::TetherRoutePolicy;
#[cfg(all(target_os = "linux", feature = "linux-tether-policy"))]
pub use linux::TetherRoutePolicy;
