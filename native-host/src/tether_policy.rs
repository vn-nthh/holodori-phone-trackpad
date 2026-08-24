//! Reversible Windows routing policy for the opt-in local-only tether mode.
//!
//! Android USB tethering normally installs a default route through the phone.
//! When this policy is active, the RNDIS interface keeps its connected phone
//! subnet but cannot become an internet gateway. The original interface flags
//! and default routes are restored when the guard is dropped. A durable,
//! adapter-bound snapshot lets a later launcher recover the same state after
//! an unexpected process exit.

use serde::{Deserialize, Serialize};
use std::ffi::CStr;
use std::io;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::slice;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_BUFFER_OVERFLOW,
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_DATA,
    ERROR_NO_MORE_ITEMS, ERROR_NOT_FOUND, ERROR_OBJECT_ALREADY_EXISTS, ERROR_SUCCESS, FILETIME,
    STILL_ACTIVE,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CreateIpForwardEntry2, DeleteIpForwardEntry2, FreeMibTable, GAA_FLAG_INCLUDE_GATEWAYS,
    GetAdaptersAddresses, GetBestInterfaceEx, GetIpForwardTable2, GetIpInterfaceEntry,
    IP_ADAPTER_ADDRESSES_LH, InitializeIpForwardEntry, InitializeIpInterfaceEntry,
    MIB_IPFORWARD_ROW2, MIB_IPINTERFACE_ROW, SetIpInterfaceEntry,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, AF_UNSPEC, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0,
    SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_INET, SOCKET_ADDRESS,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_BINARY,
    REG_OPTION_NON_VOLATILE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumValueW,
    RegFlushKey, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

const IPV4: ADDRESS_FAMILY = AF_INET;
const IPV6: ADDRESS_FAMILY = AF_INET6;
const POLICY_MAGIC: [u8; 4] = *b"HPTP";
const POLICY_VERSION: u32 = 1;
const POLICY_REGISTRY_PATH: &str = r"SOFTWARE\Holodori\PhoneTrackpad\RoutePolicy";
const POLICY_VALUE_PREFIX: &str = "tether-route-policy-v1-";
const POLICY_VALUE_SUFFIX: &str = ".bin";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ProcessOwner {
    pid: u32,
    creation_time: u64,
    image_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct AdapterIdentity {
    adapter_name: String,
    luid: u64,
    network_guid: [u8; 16],
    physical_address: Vec<u8>,
    interface_index: u32,
    description: String,
    friendly_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PersistedRoute {
    next_hop: IpAddr,
    next_hop_scope_id: u32,
    site_prefix_length: u8,
    valid_lifetime: u32,
    preferred_lifetime: u32,
    metric: u32,
    protocol: i32,
    loopback: bool,
    autoconfigure_address: bool,
    publish: bool,
    immortal: bool,
    origin: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PersistedPolicy {
    owner: ProcessOwner,
    adapter: AdapterIdentity,
    family: u16,
    original_disable_default_routes: bool,
    routes: Vec<PersistedRoute>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryOutcome {
    NothingToDo,
    Restored { snapshots: usize },
    OwnerStillRunning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TetherPrefix {
    pub interface_index: u32,
    pub address: IpAddr,
    pub prefix_length: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TetherSnapshot {
    prefixes: Vec<TetherPrefix>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TetherBinding {
    interface_index: u32,
    adapter: AdapterIdentity,
    validate_runtime: bool,
}

impl TetherBinding {
    pub fn interface_index(&self) -> u32 {
        self.interface_index
    }

    pub fn verify_peer(&self, peer: SocketAddr) -> io::Result<()> {
        if !self.validate_runtime {
            return Ok(());
        }
        let Some(current) = current_tether_binding(peer)? else {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("the migrated peer {peer} is no longer on a confirmed tether route"),
            ));
        };
        if self.matches_current(&current) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!(
                    "the migrated peer {peer} no longer uses the originally confirmed tether adapter"
                ),
            ))
        }
    }

    fn matches_current(&self, current: &Self) -> bool {
        self.interface_index == current.interface_index
            && same_adapter_instance(&self.adapter, &current.adapter)
    }

    #[cfg(test)]
    pub(crate) fn for_test(interface_index: u32) -> Self {
        Self {
            interface_index,
            adapter: AdapterIdentity {
                adapter_name: "test".to_owned(),
                luid: u64::from(interface_index),
                network_guid: [0; 16],
                physical_address: Vec::new(),
                interface_index,
                description: "test".to_owned(),
                friendly_name: "test".to_owned(),
            },
            validate_runtime: false,
        }
    }
}

impl TetherSnapshot {
    pub fn classify_peer(&self, peer: IpAddr) -> Option<u32> {
        let mut matched = None;
        for prefix in &self.prefixes {
            if same_prefix(peer, prefix.address, prefix.prefix_length) {
                match matched {
                    None => matched = Some(prefix.interface_index),
                    Some(index) if index == prefix.interface_index => {}
                    Some(_) => return None,
                }
            }
        }
        matched
    }

    #[cfg(test)]
    fn from_prefixes(prefixes: Vec<TetherPrefix>) -> Self {
        Self { prefixes }
    }
}

pub fn current_tether_snapshot() -> io::Result<TetherSnapshot> {
    let mut prefixes = Vec::new();
    for adapter in tether_adapters()? {
        prefixes.extend(adapter.prefixes);
    }
    Ok(TetherSnapshot { prefixes })
}

pub fn current_tether_binding(peer: SocketAddr) -> io::Result<Option<TetherBinding>> {
    let snapshot = current_tether_snapshot()?;
    let Some(interface_index) = snapshot.classify_peer(peer.ip()) else {
        return Ok(None);
    };
    if interface_index_for_peer(peer)? != interface_index {
        return Ok(None);
    }
    let Some(adapter) = adapter_identity(interface_index)? else {
        return Ok(None);
    };
    Ok(Some(TetherBinding {
        interface_index,
        adapter,
        validate_runtime: true,
    }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RouteKey {
    interface_index: u32,
    family: ADDRESS_FAMILY,
    prefix_length: u8,
    prefix: [u8; 16],
    next_hop: [u8; 16],
}

#[derive(Clone)]
struct InterfaceSnapshot {
    adapter: AdapterIdentity,
    interface_index: u32,
    family: ADDRESS_FAMILY,
    disable_default_routes: bool,
    default_routes: Vec<MIB_IPFORWARD_ROW2>,
    persisted_name: String,
}

/// Owns the temporary local-only policy for the duration of a native host.
///
/// The policy is deliberately opt-in. It is also deliberately scoped to
/// adapters recognized as Android/RNDIS tethering adapters. A normal Ethernet
/// or Wi-Fi adapter is never selected merely because it has a default route or
/// because a discovery packet happened to arrive through it.
pub struct TetherRoutePolicy {
    snapshots: Vec<InterfaceSnapshot>,
}

impl TetherRoutePolicy {
    pub fn new() -> io::Result<Self> {
        match recover_orphaned_policy()? {
            RecoveryOutcome::NothingToDo | RecoveryOutcome::Restored { .. } => Ok(Self {
                snapshots: Vec::new(),
            }),
            RecoveryOutcome::OwnerStillRunning => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "another tether route policy owner is still running",
            )),
        }
    }

    /// Re-assert policy only for interfaces already proven by phone discovery.
    ///
    /// In particular, this must not mutate every adapter whose display name
    /// happens to resemble RNDIS/NCM. Before the first accepted phone hello
    /// there are no snapshots and therefore no routing side effects.
    pub fn refresh(&mut self) -> io::Result<()> {
        for snapshot in self.snapshots.clone() {
            verify_enforced_snapshot(&snapshot)?;
        }
        Ok(())
    }

    /// Protect the exact interface identity accepted during phone discovery.
    pub fn protect_peer(&mut self, peer: SocketAddr, binding: &TetherBinding) -> io::Result<()> {
        // The privileged mutation must consume the immutable discovery
        // identity. Recomputing and discarding a new identity here would permit
        // an A -> B -> A adapter race around route protection.
        binding.verify_peer(peer)?;
        self.protect_interface(binding)
    }

    fn protect_interface(&mut self, binding: &TetherBinding) -> io::Result<()> {
        let interface_index = binding.interface_index;
        if !tether_interface_indices()?.contains(&interface_index) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "interface {interface_index} is no longer a recognized Android/RNDIS adapter"
                ),
            ));
        }
        require_binding_adapter(binding)?;
        self.protect_interface_inner(binding)
    }

    /// Restore all interface flags and default routes captured by this guard.
    pub fn restore(&mut self) -> io::Result<()> {
        let errors = rollback_suffix(&mut self.snapshots, 0, restore_snapshot);
        errors_to_result("restore the local-only tether policy", errors)
    }

    fn protect_interface_inner(&mut self, binding: &TetherBinding) -> io::Result<()> {
        let interface_index = binding.interface_index;
        let protected_snapshots: Vec<_> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.interface_index == interface_index)
            .cloned()
            .collect();
        for snapshot in &protected_snapshots {
            if snapshot.interface_index != binding.interface_index
                || !same_adapter_instance(&snapshot.adapter, &binding.adapter)
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "the protected route snapshot does not belong to the discovered tether adapter",
                ));
            }
            verify_enforced_snapshot(snapshot)?;
        }

        let transaction_start = self.snapshots.len();
        for family in [IPV4, IPV6] {
            if protected_snapshots
                .iter()
                .any(|snapshot| snapshot.family == family)
            {
                continue;
            }
            match capture_family(binding, family) {
                Ok(Some(snapshot)) => {
                    self.snapshots.push(snapshot);
                    let apply_result = apply_snapshot(
                        self.snapshots
                            .last()
                            .expect("captured snapshot was just appended"),
                    );
                    if let Err(error) = apply_result {
                        let rollback_errors = rollback_suffix(
                            &mut self.snapshots,
                            transaction_start,
                            restore_snapshot,
                        );
                        return Err(error_with_rollback(
                            "apply the local-only tether policy",
                            error,
                            rollback_errors,
                        ));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let rollback_errors =
                        rollback_suffix(&mut self.snapshots, transaction_start, restore_snapshot);
                    return Err(error_with_rollback(
                        "capture the local-only tether policy",
                        error,
                        rollback_errors,
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Drop for TetherRoutePolicy {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("local-only tether cleanup failed: {error}");
        }
    }
}

fn capture_family(
    binding: &TetherBinding,
    family: ADDRESS_FAMILY,
) -> io::Result<Option<InterfaceSnapshot>> {
    let adapter = require_binding_adapter(binding)?;
    let interface_index = binding.interface_index;
    let Some(interface) = interface_row_for_adapter(&adapter, family)? else {
        return Ok(None);
    };
    let default_routes = default_routes_for_adapter(&adapter, family)?;
    let disable_default_routes = interface.DisableDefaultRoutes;
    require_binding_adapter(binding)?;
    let policy = PersistedPolicy {
        owner: current_process_owner()?,
        adapter,
        family,
        original_disable_default_routes: disable_default_routes,
        routes: persisted_routes(&default_routes)?,
    };
    let persisted_name = persist_policy(&policy)?;

    Ok(Some(InterfaceSnapshot {
        adapter: policy.adapter,
        interface_index,
        family,
        disable_default_routes,
        default_routes,
        persisted_name,
    }))
}

fn apply_snapshot(snapshot: &InterfaceSnapshot) -> io::Result<()> {
    let adapter = require_same_adapter(snapshot)?;
    let captured_routes = persisted_routes(&snapshot.default_routes)?;
    let current_routes = default_routes_for_adapter(&adapter, snapshot.family)?;
    let current_routes = persisted_routes(&current_routes)?;
    if !same_multiset(&current_routes, &captured_routes) {
        return Err(io::Error::other(format!(
            "default routes on tether interface {} changed after capture; refusing to delete newer state",
            snapshot.interface_index,
        )));
    }
    let Some(mut interface) = interface_row_for_adapter(&adapter, snapshot.family)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "tether interface {} disappeared before policy application",
                snapshot.interface_index
            ),
        ));
    };
    if !interface.DisableDefaultRoutes {
        prepare_interface_row_for_set(&mut interface, true);
        check_win32(
            unsafe { SetIpInterfaceEntry(&mut interface) },
            "disable default routes on the tether interface",
        )?;
    }

    // Delete only routes captured in the durable snapshot. A route appearing
    // after capture belongs to Windows/the user and must never be destroyed
    // without first extending the journal transactionally.
    for (index, route) in snapshot.default_routes.iter().enumerate() {
        let expected_remaining = persisted_routes(&snapshot.default_routes[index..])?;
        let current = persisted_routes(&default_routes_for_adapter(&adapter, snapshot.family)?)?;
        if !same_multiset(&current, &expected_remaining) {
            return Err(io::Error::other(format!(
                "default routes on tether interface {} changed during policy application; refusing further deletion",
                snapshot.interface_index,
            )));
        }
        let result = unsafe { DeleteIpForwardEntry2(route) };
        if result != ERROR_SUCCESS {
            return Err(win32_error(
                "remove a captured tether interface default route",
                result,
            ));
        }
    }
    verify_enforced_snapshot(snapshot)
}

fn verify_enforced_snapshot(snapshot: &InterfaceSnapshot) -> io::Result<()> {
    let adapter = require_same_adapter(snapshot)?;
    let Some(mut interface) = interface_row_for_adapter(&adapter, snapshot.family)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "protected tether interface {} is no longer present",
                snapshot.interface_index
            ),
        ));
    };
    if !interface.DisableDefaultRoutes {
        prepare_interface_row_for_set(&mut interface, true);
        check_win32(
            unsafe { SetIpInterfaceEntry(&mut interface) },
            "reassert disabled default routes on the tether interface",
        )?;
    }

    let Some(verified_interface) = interface_row_for_adapter(&adapter, snapshot.family)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "protected tether interface {} disappeared during policy verification",
                snapshot.interface_index
            ),
        ));
    };
    if !verified_interface.DisableDefaultRoutes {
        return Err(io::Error::other(format!(
            "Windows reported route-policy success but default routes remain enabled on tether interface {}",
            snapshot.interface_index
        )));
    }

    let unexpected = default_routes_for_adapter(&adapter, snapshot.family)?;
    if !unexpected.is_empty() {
        return Err(io::Error::other(format!(
            "{} unjournaled default route(s) appeared on protected tether interface {}; refusing to delete them",
            unexpected.len(),
            snapshot.interface_index,
        )));
    }
    Ok(())
}

fn restore_snapshot(snapshot: &InterfaceSnapshot) -> io::Result<()> {
    let Some((adapter, gateways)) = adapter_details_for_identity(&snapshot.adapter)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "captured tether adapter {} is absent; reconnect it to finish route recovery",
                snapshot.interface_index,
            ),
        ));
    };
    let interface_index = adapter.interface_index;
    if interface_row_for_adapter(&adapter, snapshot.family)?.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "captured tether interface {} has no address-family row; reconnect it to finish route recovery",
                interface_index,
            ),
        ));
    }
    let current_routes = default_routes_for_adapter(&adapter, snapshot.family)?;
    let original_routes = persisted_routes(&snapshot.default_routes)?
        .iter()
        .map(|route| restore_route_row(route, &adapter, snapshot.family))
        .collect::<io::Result<Vec<_>>>()?;
    let original_keys: Vec<RouteKey> = original_routes.iter().map(route_key).collect();
    let current_keys: Vec<RouteKey> = current_routes.iter().map(route_key).collect();
    let mut verify_keys = Vec::new();

    // A route installed after capture is authoritative. Preserve it and do
    // not resurrect a potentially stale DHCP gateway alongside it. Missing
    // originals are recreated only when the current table is a subset of the
    // captured state (the normal partial-delete/rollback case).
    if let Some(missing) = missing_original_indices(&current_keys, &original_keys) {
        for index in missing {
            let route = &original_routes[index];
            let Some((next_hop, _)) = sockaddr_inet_ip(&route.NextHop) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "captured tether route has an unsupported next-hop family",
                ));
            };
            if !captured_gateway_is_available(next_hop, &gateways) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "captured gateway {next_hop} is no longer configured on tether interface {interface_index}; retaining the recovery journal",
                    ),
                ));
            }
            let result = unsafe { CreateIpForwardEntry2(route) };
            if result != ERROR_SUCCESS
                && result != ERROR_ALREADY_EXISTS
                && result != ERROR_OBJECT_ALREADY_EXISTS
            {
                return Err(win32_error(
                    "restore the tether interface default route",
                    result,
                ));
            }
            verify_keys.push(original_keys[index].clone());
        }
    }

    if !verify_keys.is_empty() {
        let restored_keys: Vec<RouteKey> = default_routes_for_adapter(&adapter, snapshot.family)?
            .iter()
            .map(route_key)
            .collect();
        if let Some(missing) = verify_keys.iter().find(|key| !restored_keys.contains(key)) {
            return Err(io::Error::other(format!(
                "Windows reported route restoration success but the route is still absent: {missing:?}"
            )));
        }
    }

    restore_owned_interface_flag(&adapter, snapshot.family, snapshot.disable_default_routes)?;
    remove_policy_value(&snapshot.persisted_name)
}

fn require_same_adapter(snapshot: &InterfaceSnapshot) -> io::Result<AdapterIdentity> {
    let Some(current) = adapter_identity(snapshot.interface_index)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "tether adapter {} is no longer present",
                snapshot.interface_index
            ),
        ));
    };
    if same_adapter_instance(&snapshot.adapter, &current) {
        Ok(current)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "interface {} no longer identifies the captured tether adapter",
                snapshot.interface_index
            ),
        ))
    }
}

fn require_binding_adapter(binding: &TetherBinding) -> io::Result<AdapterIdentity> {
    let Some(current) = adapter_identity(binding.interface_index)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "discovered tether adapter {} disappeared before route protection",
                binding.interface_index
            ),
        ));
    };
    let current_binding = TetherBinding {
        interface_index: current.interface_index,
        adapter: current.clone(),
        validate_runtime: true,
    };
    if binding.matches_current(&current_binding) {
        Ok(current)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "interface {} no longer identifies the discovered tether adapter",
                binding.interface_index
            ),
        ))
    }
}

/// Returns the indices of captured items missing from `current` only when
/// every current item can be matched to a distinct captured item. `None`
/// means a newer/foreign item exists and the captured state must not be
/// resurrected alongside it.
fn missing_original_indices<T: Eq>(current: &[T], original: &[T]) -> Option<Vec<usize>> {
    let mut unmatched = vec![true; original.len()];
    for item in current {
        let index = original
            .iter()
            .enumerate()
            .find(|(index, candidate)| unmatched[*index] && *candidate == item)
            .map(|(index, _)| index)?;
        unmatched[index] = false;
    }
    Some(
        unmatched
            .iter()
            .enumerate()
            .filter_map(|(index, missing)| missing.then_some(index))
            .collect(),
    )
}

fn same_multiset<T: Eq>(left: &[T], right: &[T]) -> bool {
    left.len() == right.len()
        && matches!(
            missing_original_indices(left, right),
            Some(missing) if missing.is_empty()
        )
}

fn captured_gateway_is_available(next_hop: IpAddr, current_gateways: &[IpAddr]) -> bool {
    next_hop.is_unspecified() || current_gateways.contains(&next_hop)
}

/// Restores a newly-added suffix in reverse mutation order. Successfully
/// restored entries are removed; failures remain owned by the caller so a
/// later explicit restore or `Drop` can retry them.
fn rollback_suffix<T, F>(items: &mut Vec<T>, start: usize, mut restore: F) -> Vec<io::Error>
where
    F: FnMut(&T) -> io::Result<()>,
{
    assert!(start <= items.len());
    let suffix = items.split_off(start);
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    for item in suffix.into_iter().rev() {
        match restore(&item) {
            Ok(()) => {}
            Err(error) => {
                errors.push(error);
                failed.push(item);
            }
        }
    }
    failed.reverse();
    items.extend(failed);
    errors
}

fn errors_to_result(context: &str, errors: Vec<io::Error>) -> io::Result<()> {
    let Some(first) = errors.first() else {
        return Ok(());
    };
    let kind = first.kind();
    let details = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    Err(io::Error::new(kind, format!("{context}: {details}")))
}

fn error_with_rollback(
    context: &str,
    primary: io::Error,
    rollback_errors: Vec<io::Error>,
) -> io::Error {
    let kind = primary.kind();
    if rollback_errors.is_empty() {
        return io::Error::new(kind, format!("{context}: {primary}"));
    }
    let rollback = rollback_errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    io::Error::new(
        kind,
        format!(
            "{context}: {primary}; rollback also failed: {rollback}; durable snapshot(s) retained for retry"
        ),
    )
}

/// Restore only the flag value this policy can prove it changed. The policy
/// enforces `true`, so a current `false` value or an originally-true flag is
/// newer/unowned state and must be left untouched.
fn restore_owned_interface_flag(
    adapter: &AdapterIdentity,
    family: ADDRESS_FAMILY,
    original_disable_default_routes: bool,
) -> io::Result<()> {
    let interface_index = adapter.interface_index;
    let Some(interface) = interface_row_for_adapter(adapter, family)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("tether interface {interface_index} disappeared before flag restoration"),
        ));
    };
    if should_restore_owned_flag(
        interface.DisableDefaultRoutes,
        original_disable_default_routes,
    ) {
        restore_interface_flag(adapter, family, false)?;
    }
    Ok(())
}

fn should_restore_owned_flag(current: bool, original: bool) -> bool {
    current && !original
}

fn restore_interface_flag(
    adapter: &AdapterIdentity,
    family: ADDRESS_FAMILY,
    disable_default_routes: bool,
) -> io::Result<()> {
    let Some(mut interface) = interface_row_for_adapter(adapter, family)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "tether interface {} disappeared during flag restoration",
                adapter.interface_index
            ),
        ));
    };
    if interface.DisableDefaultRoutes == disable_default_routes {
        return Ok(());
    }
    prepare_interface_row_for_set(&mut interface, disable_default_routes);
    check_win32(
        unsafe { SetIpInterfaceEntry(&mut interface) },
        "restore the tether interface default-route setting",
    )?;
    let Some(verified) = interface_row_for_adapter(adapter, family)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "tether interface {} disappeared after flag restoration",
                adapter.interface_index
            ),
        ));
    };
    if verified.DisableDefaultRoutes == disable_default_routes {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Windows reported flag restoration success but tether interface {} retained the old value",
            adapter.interface_index
        )))
    }
}

fn interface_row(
    interface_index: u32,
    family: ADDRESS_FAMILY,
) -> io::Result<Option<MIB_IPINTERFACE_ROW>> {
    let mut row = MIB_IPINTERFACE_ROW::default();
    unsafe { InitializeIpInterfaceEntry(&mut row) };
    row.Family = family;
    row.InterfaceIndex = interface_index;
    let result = unsafe { GetIpInterfaceEntry(&mut row) };
    if is_missing(result) {
        Ok(None)
    } else if result == ERROR_SUCCESS {
        Ok(Some(row))
    } else {
        Err(win32_error("read the tether interface settings", result))
    }
}

fn interface_row_for_adapter(
    adapter: &AdapterIdentity,
    family: ADDRESS_FAMILY,
) -> io::Result<Option<MIB_IPINTERFACE_ROW>> {
    let Some(row) = interface_row(adapter.interface_index, family)? else {
        return Ok(None);
    };
    if interface_identity_matches(
        row.InterfaceIndex,
        unsafe { row.InterfaceLuid.Value },
        adapter,
    ) {
        Ok(Some(row))
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "interface {} changed adapter identity while reading its route policy",
                adapter.interface_index
            ),
        ))
    }
}

fn prepare_interface_row_for_set(row: &mut MIB_IPINTERFACE_ROW, disable_default_routes: bool) {
    row.DisableDefaultRoutes = disable_default_routes;
    // SetIpInterfaceEntry rejects an IPv4 row unless SitePrefixLength is zero,
    // even when the value came from GetIpInterfaceEntry (ERROR_INVALID_PARAMETER).
    if row.Family == IPV4 {
        row.SitePrefixLength = 0;
    }
}

fn default_routes(
    interface_index: u32,
    family: ADDRESS_FAMILY,
) -> io::Result<Vec<MIB_IPFORWARD_ROW2>> {
    let mut table = null_mut();
    let result = unsafe { GetIpForwardTable2(family, &mut table) };
    if result != ERROR_SUCCESS {
        if is_missing(result) {
            return Ok(Vec::new());
        }
        return Err(win32_error("read the Windows route table", result));
    }

    let routes = unsafe {
        let count = (*table).NumEntries as usize;
        slice::from_raw_parts((*table).Table.as_ptr(), count)
            .iter()
            .filter(|route| {
                route.InterfaceIndex == interface_index && route.DestinationPrefix.PrefixLength == 0
            })
            .copied()
            .collect()
    };
    unsafe { FreeMibTable(table.cast()) };
    Ok(routes)
}

fn default_routes_for_adapter(
    adapter: &AdapterIdentity,
    family: ADDRESS_FAMILY,
) -> io::Result<Vec<MIB_IPFORWARD_ROW2>> {
    let routes = default_routes(adapter.interface_index, family)?;
    if routes.iter().all(|route| {
        interface_identity_matches(
            route.InterfaceIndex,
            unsafe { route.InterfaceLuid.Value },
            adapter,
        )
    }) {
        Ok(routes)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "interface {} changed adapter identity while reading its default routes",
                adapter.interface_index
            ),
        ))
    }
}

fn interface_identity_matches(
    interface_index: u32,
    interface_luid: u64,
    adapter: &AdapterIdentity,
) -> bool {
    interface_index == adapter.interface_index && interface_luid == adapter.luid
}

struct TetherAdapter {
    interface_index: u32,
    prefixes: Vec<TetherPrefix>,
}

fn tether_adapters() -> io::Result<Vec<TetherAdapter>> {
    let mut buffer = adapter_addresses_buffer()?;
    if buffer.is_empty() {
        return Ok(Vec::new());
    }
    let mut adapters = Vec::new();
    let mut adapter = buffer.as_mut_ptr();
    while !adapter.is_null() {
        let current = unsafe { &*adapter };
        let identity = format!(
            "{} {}",
            unsafe { wide_string(current.Description) },
            unsafe { wide_string(current.FriendlyName) },
        )
        .to_ascii_lowercase();
        let interface_index = unsafe { current.Anonymous1.Anonymous.IfIndex };
        if interface_index != 0
            && is_tether_adapter(&identity)
            && current.OperStatus == IfOperStatusUp
            && !adapters
                .iter()
                .any(|item: &TetherAdapter| item.interface_index == interface_index)
        {
            let mut prefixes = Vec::new();
            let mut unicast = current.FirstUnicastAddress;
            while !unicast.is_null() {
                let address = unsafe { &*unicast };
                if let Some(ip) = socket_address_ip(&address.Address)
                    && !ip.is_unspecified()
                    && !ip.is_loopback()
                    && valid_tether_prefix(ip, address.OnLinkPrefixLength)
                {
                    prefixes.push(TetherPrefix {
                        interface_index,
                        address: ip,
                        prefix_length: address.OnLinkPrefixLength,
                    });
                }
                unicast = address.Next;
            }
            adapters.push(TetherAdapter {
                interface_index,
                prefixes,
            });
        }
        adapter = current.Next;
    }
    Ok(adapters)
}

fn adapter_addresses_buffer() -> io::Result<Vec<IP_ADAPTER_ADDRESSES_LH>> {
    let item_size = size_of::<IP_ADAPTER_ADDRESSES_LH>();
    let mut buffer = vec![IP_ADAPTER_ADDRESSES_LH::default(); 32];
    let mut size = (buffer.len() * item_size) as u32;
    loop {
        let result = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                GAA_FLAG_INCLUDE_GATEWAYS,
                null(),
                buffer.as_mut_ptr(),
                &mut size,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW {
            let required = (size as usize).div_ceil(item_size).max(buffer.len() * 2);
            buffer.resize(required, IP_ADAPTER_ADDRESSES_LH::default());
            size = (buffer.len() * item_size) as u32;
            continue;
        }
        if result == ERROR_NO_DATA {
            return Ok(Vec::new());
        }
        check_win32(result, "enumerate Windows network adapters")?;
        break;
    }
    Ok(buffer)
}

fn tether_interface_indices() -> io::Result<Vec<u32>> {
    Ok(tether_adapters()?
        .into_iter()
        .map(|adapter| adapter.interface_index)
        .collect())
}

fn socket_address_ip(address: &SOCKET_ADDRESS) -> Option<IpAddr> {
    if address.lpSockaddr.is_null() {
        return None;
    }
    let family = unsafe { (*address.lpSockaddr).sa_family };
    if family == AF_INET {
        let sockaddr = unsafe { &*address.lpSockaddr.cast::<SOCKADDR_IN>() };
        let bytes = unsafe { sockaddr.sin_addr.S_un.S_addr }.to_ne_bytes();
        Some(IpAddr::V4(Ipv4Addr::from(bytes)))
    } else if family == AF_INET6 {
        let sockaddr = unsafe { &*address.lpSockaddr.cast::<SOCKADDR_IN6>() };
        Some(IpAddr::V6(Ipv6Addr::from(unsafe {
            sockaddr.sin6_addr.u.Byte
        })))
    } else {
        None
    }
}

fn same_prefix(candidate: IpAddr, local: IpAddr, prefix_length: u8) -> bool {
    match (candidate, local) {
        (IpAddr::V4(candidate), IpAddr::V4(local)) if (1..=32).contains(&prefix_length) => {
            let prefix = u32::from_be_bytes(local.octets());
            let candidate = u32::from_be_bytes(candidate.octets());
            let mask = if prefix_length == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix_length))
            };
            candidate & mask == prefix & mask
        }
        (IpAddr::V6(candidate), IpAddr::V6(local)) if (1..=128).contains(&prefix_length) => {
            let prefix = u128::from_be_bytes(local.octets());
            let candidate = u128::from_be_bytes(candidate.octets());
            let mask = if prefix_length == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix_length))
            };
            candidate & mask == prefix & mask
        }
        _ => false,
    }
}

fn valid_tether_prefix(address: IpAddr, prefix_length: u8) -> bool {
    match address {
        // Phone-tether LANs are local address space. Reject public prefixes
        // so a broadly named USB Ethernet adapter on a real uplink cannot be
        // selected merely because its description resembles RNDIS/NCM.
        IpAddr::V4(address) => {
            let octets = address.octets();
            let local_block_length = match octets {
                [10, _, _, _] => 8,
                [172, second, _, _] if (16..=31).contains(&second) => 12,
                [192, 168, _, _] | [169, 254, _, _] => 16,
                _ => return false,
            };
            (local_block_length..=30).contains(&prefix_length)
        }
        IpAddr::V6(address) => {
            let first = address.segments()[0];
            let local_block_length = if first & 0xfe00 == 0xfc00 {
                7
            } else if first & 0xffc0 == 0xfe80 {
                10
            } else {
                return false;
            };
            (local_block_length..=127).contains(&prefix_length)
        }
    }
}

fn interface_index_for_peer(peer: SocketAddr) -> io::Result<u32> {
    let mut index = 0_u32;
    match peer {
        SocketAddr::V4(peer) => {
            let address = *peer.ip();
            let sockaddr = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 0,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(address.octets()),
                    },
                },
                sin_zero: [0; 8],
            };
            check_win32(
                unsafe {
                    GetBestInterfaceEx(
                        (&sockaddr as *const SOCKADDR_IN).cast::<SOCKADDR>(),
                        &mut index,
                    )
                },
                "find the phone tether interface",
            )?;
        }
        SocketAddr::V6(peer) => {
            let address = *peer.ip();
            let sockaddr = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: address.octets(),
                    },
                },
                Anonymous: windows_sys::Win32::Networking::WinSock::SOCKADDR_IN6_0 {
                    sin6_scope_id: peer.scope_id(),
                },
            };
            check_win32(
                unsafe {
                    GetBestInterfaceEx(
                        (&sockaddr as *const SOCKADDR_IN6).cast::<SOCKADDR>(),
                        &mut index,
                    )
                },
                "find the phone tether interface",
            )?;
        }
    }
    if index == 0 {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows returned no interface for the discovered phone",
        ))
    } else {
        Ok(index)
    }
}

fn route_key(route: &MIB_IPFORWARD_ROW2) -> RouteKey {
    let family = unsafe { route.DestinationPrefix.Prefix.si_family };
    let (prefix, next_hop) = if family == AF_INET {
        let prefix = unsafe { route.DestinationPrefix.Prefix.Ipv4 };
        let next_hop = unsafe { route.NextHop.Ipv4 };
        let mut prefix_bytes = [0_u8; 16];
        let mut next_hop_bytes = [0_u8; 16];
        prefix_bytes[..4].copy_from_slice(&unsafe { prefix.sin_addr.S_un.S_addr }.to_ne_bytes());
        next_hop_bytes[..4]
            .copy_from_slice(&unsafe { next_hop.sin_addr.S_un.S_addr }.to_ne_bytes());
        (prefix_bytes, next_hop_bytes)
    } else {
        let prefix = unsafe { route.DestinationPrefix.Prefix.Ipv6 };
        let next_hop = unsafe { route.NextHop.Ipv6 };
        (unsafe { prefix.sin6_addr.u.Byte }, unsafe {
            next_hop.sin6_addr.u.Byte
        })
    };
    RouteKey {
        interface_index: route.InterfaceIndex,
        family,
        prefix_length: route.DestinationPrefix.PrefixLength,
        prefix,
        next_hop,
    }
}

fn is_tether_adapter(identity: &str) -> bool {
    let explicit_phone_tether = identity.contains("android")
        || identity.contains("tether")
        || identity.contains("internet sharing");
    ((identity.contains("rndis") || identity.contains("remote ndis"))
        && (identity.contains("remote ndis") || explicit_phone_tether))
        || identity.contains("usb tether")
        || identity.contains("usb-tether")
        || (identity.contains("ncm") && explicit_phone_tether)
}

unsafe fn wide_string(pointer: windows_sys::core::PWSTR) -> String {
    if pointer.is_null() {
        return String::new();
    }
    let mut length = 0;
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    String::from_utf16_lossy(unsafe { slice::from_raw_parts(pointer, length) })
}

fn is_missing(result: u32) -> bool {
    result == ERROR_FILE_NOT_FOUND || result == ERROR_NOT_FOUND
}

fn check_win32(result: u32, operation: &str) -> io::Result<()> {
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(win32_error(operation, result))
    }
}

fn win32_error(operation: &str, result: u32) -> io::Error {
    let kind = if result == ERROR_ACCESS_DENIED {
        io::ErrorKind::PermissionDenied
    } else if is_missing(result) {
        io::ErrorKind::NotFound
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(
        kind,
        format!("{operation} failed with Windows error {result}"),
    )
}

pub fn recover_orphaned_policy() -> io::Result<RecoveryOutcome> {
    let policies = persisted_policies()?;
    if policies.is_empty() {
        return Ok(RecoveryOutcome::NothingToDo);
    }

    let mut restored = 0;
    let mut owner_running = false;
    for (name, policy) in policies {
        if process_owner_is_running(&policy.owner)? {
            owner_running = true;
            continue;
        }
        recover_policy(&policy)?;
        remove_policy_value(&name)?;
        restored += 1;
    }

    if owner_running {
        Ok(RecoveryOutcome::OwnerStillRunning)
    } else if restored > 0 {
        Ok(RecoveryOutcome::Restored {
            snapshots: restored,
        })
    } else {
        Ok(RecoveryOutcome::NothingToDo)
    }
}

fn recover_policy(policy: &PersistedPolicy) -> io::Result<()> {
    let Some((adapter, gateways)) = adapter_details_for_identity(&policy.adapter)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "captured tether adapter {} is absent; reconnect it to finish route recovery",
                policy.adapter.interface_index,
            ),
        ));
    };
    // Recovery is bound to the durable GUID/LUID/MAC identity, not today's
    // display-name heuristic. Otherwise tightening adapter selection could
    // strand a journal written by an older build and leave its owned state
    // unrestored. Only new mutations must pass `is_tether_adapter`.
    let family = policy.family as ADDRESS_FAMILY;
    let Some(_interface) = interface_row_for_adapter(&adapter, family)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "captured tether interface {} has no address-family row; reconnect it to finish route recovery",
                adapter.interface_index,
            ),
        ));
    };
    let current_routes = default_routes_for_adapter(&adapter, family)?;
    let original_rows = policy
        .routes
        .iter()
        .map(|route| restore_route_row(route, &adapter, family))
        .collect::<io::Result<Vec<_>>>()?;
    let current_keys: Vec<RouteKey> = current_routes.iter().map(route_key).collect();
    let original_keys: Vec<RouteKey> = original_rows.iter().map(route_key).collect();
    let mut verify_keys = Vec::new();
    if let Some(missing) = missing_original_indices(&current_keys, &original_keys) {
        for index in missing {
            let route = &policy.routes[index];
            if !captured_gateway_is_available(route.next_hop, &gateways) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "captured gateway {} is no longer configured on tether interface {}; retaining the recovery journal",
                        route.next_hop, adapter.interface_index,
                    ),
                ));
            }
            let row = &original_rows[index];
            let result = unsafe { CreateIpForwardEntry2(row) };
            if result != ERROR_SUCCESS
                && result != ERROR_ALREADY_EXISTS
                && result != ERROR_OBJECT_ALREADY_EXISTS
            {
                return Err(win32_error(
                    "recover a tether interface default route",
                    result,
                ));
            }
            verify_keys.push(original_keys[index].clone());
        }
    }
    if !verify_keys.is_empty() {
        let restored_keys: Vec<RouteKey> = default_routes_for_adapter(&adapter, family)?
            .iter()
            .map(route_key)
            .collect();
        if let Some(missing) = verify_keys.iter().find(|key| !restored_keys.contains(key)) {
            return Err(io::Error::other(format!(
                "Windows reported route recovery success but the route is still absent: {missing:?}"
            )));
        }
    }

    restore_owned_interface_flag(&adapter, family, policy.original_disable_default_routes)?;
    Ok(())
}

fn same_adapter_instance(expected: &AdapterIdentity, actual: &AdapterIdentity) -> bool {
    expected.adapter_name == actual.adapter_name
        && expected.luid == actual.luid
        && expected.network_guid == actual.network_guid
        && expected.physical_address == actual.physical_address
}

fn persist_policy(policy: &PersistedPolicy) -> io::Result<String> {
    let name = format!(
        "{POLICY_VALUE_PREFIX}{}-{}-{}-{}{POLICY_VALUE_SUFFIX}",
        policy.owner.pid, policy.owner.creation_time, policy.adapter.interface_index, policy.family,
    );
    let encoded = encode_policy(policy)?;
    let key = create_policy_key()?;
    if let Some(existing) = read_policy_value(&key, &name)? {
        let existing = decode_policy(&existing, &registry_value_source(&name))?;
        if existing == *policy {
            return Ok(name);
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("a different tether policy snapshot already exists as {name}"),
        ));
    }

    let wide_name = wide_null(&name);
    check_win32(
        unsafe {
            RegSetValueExW(
                key.0,
                wide_name.as_ptr(),
                0,
                REG_BINARY,
                encoded.as_ptr(),
                encoded.len().try_into().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "policy snapshot is too large")
                })?,
            )
        },
        "write the protected tether recovery journal",
    )?;
    check_win32(
        unsafe { RegFlushKey(key.0) },
        "flush the protected tether recovery journal",
    )?;
    if read_policy_value(&key, &name)?.as_deref() != Some(encoded.as_slice()) {
        return Err(io::Error::other(
            "the protected tether recovery journal did not verify after writing",
        ));
    }
    Ok(name)
}

fn encode_policy(policy: &PersistedPolicy) -> io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(policy)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let payload_length: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "policy snapshot is too large"))?;
    let mut encoded = Vec::with_capacity(16 + payload.len());
    encoded.extend_from_slice(&POLICY_MAGIC);
    encoded.extend_from_slice(&POLICY_VERSION.to_le_bytes());
    encoded.extend_from_slice(&payload_length.to_le_bytes());
    encoded.extend_from_slice(&payload);
    let checksum = crate::protocol::crc32(&encoded);
    encoded.extend_from_slice(&checksum.to_le_bytes());
    Ok(encoded)
}

fn decode_policy(encoded: &[u8], source: &str) -> io::Result<PersistedPolicy> {
    if encoded.len() < 16 || encoded[..4] != POLICY_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid tether policy snapshot {source}"),
        ));
    }
    let version = u32::from_le_bytes(encoded[4..8].try_into().unwrap());
    if version != POLICY_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported tether policy snapshot version {version}"),
        ));
    }
    let payload_length = u32::from_le_bytes(encoded[8..12].try_into().unwrap()) as usize;
    if encoded.len() != 12 + payload_length + 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("truncated tether policy snapshot {source}"),
        ));
    }
    let expected = u32::from_le_bytes(
        encoded[encoded.len() - 4..]
            .try_into()
            .expect("four-byte snapshot checksum"),
    );
    let actual = crate::protocol::crc32(&encoded[..encoded.len() - 4]);
    if expected != actual {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("tether policy snapshot checksum failed for {source}"),
        ));
    }
    serde_json::from_slice(&encoded[12..12 + payload_length])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn create_policy_key() -> io::Result<RegistryKey> {
    let path = wide_null(POLICY_REGISTRY_PATH);
    let mut key = null_mut();
    check_win32(
        unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
                null(),
                &mut key,
                null_mut(),
            )
        },
        "open the machine-protected tether recovery journal",
    )?;
    Ok(RegistryKey(key))
}

fn open_policy_key(access: u32) -> io::Result<Option<RegistryKey>> {
    let path = wide_null(POLICY_REGISTRY_PATH);
    let mut key = null_mut();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            access | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if is_missing(result) {
        Ok(None)
    } else {
        check_win32(result, "open the machine-protected tether recovery journal")?;
        Ok(Some(RegistryKey(key)))
    }
}

fn read_policy_value(key: &RegistryKey, name: &str) -> io::Result<Option<Vec<u8>>> {
    let wide_name = wide_null(name);
    let mut value_type = 0;
    let mut length = 0;
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            wide_name.as_ptr(),
            null(),
            &mut value_type,
            null_mut(),
            &mut length,
        )
    };
    if is_missing(result) {
        return Ok(None);
    }
    check_win32(result, "read a protected tether recovery journal entry")?;
    if value_type != REG_BINARY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("protected tether recovery entry {name} is not binary"),
        ));
    }

    let mut encoded = vec![0_u8; length as usize];
    loop {
        let mut actual_length = encoded.len().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "policy snapshot is too large")
        })?;
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                wide_name.as_ptr(),
                null(),
                &mut value_type,
                encoded.as_mut_ptr(),
                &mut actual_length,
            )
        };
        if result == ERROR_MORE_DATA {
            encoded.resize(actual_length as usize, 0);
            continue;
        }
        check_win32(result, "read a protected tether recovery journal entry")?;
        if value_type != REG_BINARY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("protected tether recovery entry {name} changed type while reading"),
            ));
        }
        encoded.truncate(actual_length as usize);
        return Ok(Some(encoded));
    }
}

fn persisted_policies() -> io::Result<Vec<(String, PersistedPolicy)>> {
    let Some(key) = open_policy_key(KEY_QUERY_VALUE)? else {
        return Ok(Vec::new());
    };
    let mut value_count = 0;
    let mut max_name_length = 0;
    let mut max_data_length = 0;
    check_win32(
        unsafe {
            RegQueryInfoKeyW(
                key.0,
                null_mut(),
                null_mut(),
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut value_count,
                &mut max_name_length,
                &mut max_data_length,
                null_mut(),
                null_mut(),
            )
        },
        "inspect the protected tether recovery journal",
    )?;

    let mut policies = Vec::new();
    for index in 0..value_count {
        let mut name = vec![0_u16; max_name_length as usize + 1];
        let mut encoded = vec![0_u8; max_data_length.max(1) as usize];
        let mut name_length = name.len() as u32;
        let mut data_length = encoded.len() as u32;
        let mut value_type = 0;
        let result = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut name_length,
                null(),
                &mut value_type,
                encoded.as_mut_ptr(),
                &mut data_length,
            )
        };
        if result == ERROR_NO_MORE_ITEMS {
            break;
        }
        check_win32(result, "enumerate the protected tether recovery journal")?;
        let name = String::from_utf16_lossy(&name[..name_length as usize]);
        if !name.starts_with(POLICY_VALUE_PREFIX) || !name.ends_with(POLICY_VALUE_SUFFIX) {
            continue;
        }
        if value_type != REG_BINARY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("protected tether recovery entry {name} is not binary"),
            ));
        }
        encoded.truncate(data_length as usize);
        let policy = decode_policy(&encoded, &registry_value_source(&name))?;
        policies.push((name, policy));
    }
    Ok(policies)
}

fn remove_policy_value(name: &str) -> io::Result<()> {
    let Some(key) = open_policy_key(KEY_SET_VALUE)? else {
        return Ok(());
    };
    let wide_name = wide_null(name);
    let result = unsafe { RegDeleteValueW(key.0, wide_name.as_ptr()) };
    if !is_missing(result) {
        check_win32(result, "remove a protected tether recovery journal entry")?;
        check_win32(
            unsafe { RegFlushKey(key.0) },
            "flush removal of a protected tether recovery journal entry",
        )?;
    }
    Ok(())
}

fn registry_value_source(name: &str) -> String {
    format!("HKLM\\{POLICY_REGISTRY_PATH}\\{name}")
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn current_process_owner() -> io::Result<ProcessOwner> {
    let handle = unsafe { GetCurrentProcess() };
    Ok(ProcessOwner {
        pid: std::process::id(),
        creation_time: process_creation_time(handle)?,
        image_name: std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default(),
    })
}

fn process_owner_is_running(owner: &ProcessOwner) -> io::Result<bool> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, owner.pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error().map(|value| value as u32) {
            Some(ERROR_INVALID_PARAMETER) | Some(ERROR_NOT_FOUND) => Ok(false),
            Some(ERROR_ACCESS_DENIED) => Ok(true),
            _ => Err(error),
        };
    }

    let creation = process_creation_time(handle);
    let image = process_image_name(handle);
    let mut exit_code = 0_u32;
    let exit_code_result = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    let exit_code_error = (exit_code_result == 0).then(io::Error::last_os_error);
    unsafe { CloseHandle(handle) };
    if let Some(error) = exit_code_error {
        return Err(error);
    }
    if exit_code != STILL_ACTIVE as u32 {
        return Ok(false);
    }
    let creation_matches = creation? == owner.creation_time;
    let image_matches = owner.image_name.is_empty()
        || image
            .map(|image| image.eq_ignore_ascii_case(&owner.image_name))
            .unwrap_or(true);
    Ok(creation_matches && image_matches)
}

fn process_creation_time(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn process_image_name(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<String> {
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(
        PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]))
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
}

fn adapter_identity(interface_index: u32) -> io::Result<Option<AdapterIdentity>> {
    Ok(adapter_details(interface_index)?.map(|(identity, _)| identity))
}

fn adapter_details(interface_index: u32) -> io::Result<Option<(AdapterIdentity, Vec<IpAddr>)>> {
    Ok(all_adapter_details()?
        .into_iter()
        .find(|(identity, _)| identity.interface_index == interface_index))
}

fn adapter_details_for_identity(
    expected: &AdapterIdentity,
) -> io::Result<Option<(AdapterIdentity, Vec<IpAddr>)>> {
    let mut matched = None;
    for details in all_adapter_details()? {
        if !same_adapter_instance(expected, &details.0) {
            continue;
        }
        if matched.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "multiple Windows adapters match the captured tether identity; refusing recovery",
            ));
        }
        matched = Some(details);
    }
    Ok(matched)
}

fn all_adapter_details() -> io::Result<Vec<(AdapterIdentity, Vec<IpAddr>)>> {
    let mut buffer = adapter_addresses_buffer()?;
    if buffer.is_empty() {
        return Ok(Vec::new());
    }
    let mut details = Vec::new();
    let mut adapter = buffer.as_mut_ptr();
    while !adapter.is_null() {
        let current = unsafe { &*adapter };
        let current_index = unsafe { current.Anonymous1.Anonymous.IfIndex };
        if current_index != 0 {
            let description = unsafe { wide_string(current.Description) };
            let friendly_name = unsafe { wide_string(current.FriendlyName) };
            let physical_length =
                (current.PhysicalAddressLength as usize).min(current.PhysicalAddress.len());
            let identity = AdapterIdentity {
                adapter_name: unsafe { ansi_string(current.AdapterName) },
                luid: unsafe { current.Luid.Value },
                network_guid: guid_bytes(current.NetworkGuid),
                physical_address: current.PhysicalAddress[..physical_length].to_vec(),
                interface_index: current_index,
                description,
                friendly_name,
            };
            let mut gateways = Vec::new();
            let mut gateway = current.FirstGatewayAddress;
            while !gateway.is_null() {
                let current_gateway = unsafe { &*gateway };
                if let Some(address) = socket_address_ip(&current_gateway.Address) {
                    gateways.push(address);
                }
                gateway = current_gateway.Next;
            }
            details.push((identity, gateways));
        }
        adapter = current.Next;
    }
    Ok(details)
}

fn persist_route(route: &MIB_IPFORWARD_ROW2) -> Option<PersistedRoute> {
    let (next_hop, next_hop_scope_id) = sockaddr_inet_ip(&route.NextHop)?;
    Some(PersistedRoute {
        next_hop,
        next_hop_scope_id,
        site_prefix_length: route.SitePrefixLength,
        valid_lifetime: route.ValidLifetime,
        preferred_lifetime: route.PreferredLifetime,
        metric: route.Metric,
        protocol: route.Protocol,
        loopback: route.Loopback,
        autoconfigure_address: route.AutoconfigureAddress,
        publish: route.Publish,
        immortal: route.Immortal,
        origin: route.Origin,
    })
}

fn persisted_routes(routes: &[MIB_IPFORWARD_ROW2]) -> io::Result<Vec<PersistedRoute>> {
    routes
        .iter()
        .map(|route| {
            persist_route(route).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "could not serialize every tether default route before changing Windows",
                )
            })
        })
        .collect()
}

fn restore_route_row(
    route: &PersistedRoute,
    adapter: &AdapterIdentity,
    family: ADDRESS_FAMILY,
) -> io::Result<MIB_IPFORWARD_ROW2> {
    if !matches!(
        (family, route.next_hop),
        (AF_INET, IpAddr::V4(_)) | (AF_INET6, IpAddr::V6(_))
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persisted route address family does not match its interface family",
        ));
    }
    let mut row = MIB_IPFORWARD_ROW2::default();
    unsafe { InitializeIpForwardEntry(&mut row) };
    row.InterfaceIndex = adapter.interface_index;
    row.InterfaceLuid = windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH {
        Value: adapter.luid,
    };
    row.DestinationPrefix.PrefixLength = 0;
    set_sockaddr_inet(
        &mut row.DestinationPrefix.Prefix,
        unspecified_for_family(family),
        0,
    );
    let next_hop_scope_id = match route.next_hop {
        IpAddr::V6(address) if address.is_unicast_link_local() => adapter.interface_index,
        _ => route.next_hop_scope_id,
    };
    set_sockaddr_inet(&mut row.NextHop, route.next_hop, next_hop_scope_id);
    row.SitePrefixLength = route.site_prefix_length;
    row.ValidLifetime = route.valid_lifetime;
    row.PreferredLifetime = route.preferred_lifetime;
    row.Metric = route.metric;
    row.Protocol = route.protocol;
    row.Loopback = route.loopback;
    row.AutoconfigureAddress = route.autoconfigure_address;
    row.Publish = route.publish;
    row.Immortal = route.immortal;
    row.Origin = route.origin;
    Ok(row)
}

fn unspecified_for_family(family: ADDRESS_FAMILY) -> IpAddr {
    if family == AF_INET {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    }
}

fn set_sockaddr_inet(target: &mut SOCKADDR_INET, address: IpAddr, scope_id: u32) {
    match address {
        IpAddr::V4(address) => {
            target.Ipv4 = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 0,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(address.octets()),
                    },
                },
                sin_zero: [0; 8],
            };
        }
        IpAddr::V6(address) => {
            target.Ipv6 = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: address.octets(),
                    },
                },
                Anonymous: windows_sys::Win32::Networking::WinSock::SOCKADDR_IN6_0 {
                    sin6_scope_id: scope_id,
                },
            };
        }
    }
}

fn sockaddr_inet_ip(address: &SOCKADDR_INET) -> Option<(IpAddr, u32)> {
    let family = unsafe { address.si_family };
    if family == AF_INET {
        let sockaddr = unsafe { address.Ipv4 };
        Some((
            IpAddr::V4(Ipv4Addr::from(
                unsafe { sockaddr.sin_addr.S_un.S_addr }.to_ne_bytes(),
            )),
            0,
        ))
    } else if family == AF_INET6 {
        let sockaddr = unsafe { address.Ipv6 };
        Some((
            IpAddr::V6(Ipv6Addr::from(unsafe { sockaddr.sin6_addr.u.Byte })),
            unsafe { sockaddr.Anonymous.sin6_scope_id },
        ))
    } else {
        None
    }
}

fn guid_bytes(guid: windows_sys::core::GUID) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..4].copy_from_slice(&guid.data1.to_le_bytes());
    bytes[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    bytes[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    bytes[8..].copy_from_slice(&guid.data4);
    bytes
}

unsafe fn ansi_string(pointer: windows_sys::core::PSTR) -> String {
    if pointer.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(pointer.cast()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterIdentity, IPV4, IPV6, PersistedPolicy, PersistedRoute, ProcessOwner, TetherBinding,
        TetherPrefix, TetherSnapshot, captured_gateway_is_available, decode_policy, encode_policy,
        interface_identity_matches, is_tether_adapter, missing_original_indices,
        prepare_interface_row_for_set, restore_route_row, rollback_suffix, same_adapter_instance,
        same_multiset, same_prefix, should_restore_owned_flag, sockaddr_inet_ip,
        valid_tether_prefix,
    };
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use windows_sys::Win32::NetworkManagement::IpHelper::MIB_IPINTERFACE_ROW;

    #[test]
    fn recognizes_common_android_tether_descriptions() {
        assert!(is_tether_adapter(
            "remote ndis based internet sharing device ethernet 4"
        ));
        assert!(is_tether_adapter("android usb tether ncm0"));
    }

    #[test]
    fn does_not_select_normal_network_adapters() {
        assert!(!is_tether_adapter(
            "realtek pcie gbe family controller ethernet"
        ));
        assert!(!is_tether_adapter("intel wi-fi 6 ax201"));
        assert!(!is_tether_adapter(
            "generic usb ncm gigabit ethernet adapter"
        ));
        assert!(!is_tether_adapter("usb rndis ethernet adapter"));
        assert!(!is_tether_adapter("usb ncm mobile broadband adapter"));
        assert!(!is_tether_adapter("linux ethernet gadget"));
    }

    #[test]
    fn prepares_ipv4_interface_rows_for_windows_update() {
        let mut row = MIB_IPINTERFACE_ROW {
            Family: IPV4,
            SitePrefixLength: 24,
            ..Default::default()
        };

        prepare_interface_row_for_set(&mut row, true);

        assert!(row.DisableDefaultRoutes);
        assert_eq!(row.SitePrefixLength, 0);
    }

    #[test]
    fn preserves_ipv6_site_prefix_when_updating_interface() {
        let mut row = MIB_IPINTERFACE_ROW {
            Family: IPV6,
            SitePrefixLength: 64,
            ..Default::default()
        };

        prepare_interface_row_for_set(&mut row, false);

        assert!(!row.DisableDefaultRoutes);
        assert_eq!(row.SitePrefixLength, 64);
    }

    #[test]
    fn prefix_matching_handles_ipv4_ipv6_and_family_mismatches() {
        assert!(same_prefix(
            IpAddr::V4(Ipv4Addr::new(192, 168, 42, 129)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1)),
            24,
        ));
        assert!(!same_prefix(
            IpAddr::V4(Ipv4Addr::new(192, 168, 43, 2)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1)),
            24,
        ));
        assert!(same_prefix(
            IpAddr::V6("fd00::2".parse::<Ipv6Addr>().unwrap()),
            IpAddr::V6("fd00::1".parse::<Ipv6Addr>().unwrap()),
            64,
        ));
        assert!(!same_prefix(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            128,
        ));
        assert!(!valid_tether_prefix(
            IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1)),
            0,
        ));
        assert!(!valid_tether_prefix(
            IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1)),
            8,
        ));
        assert!(!valid_tether_prefix(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            24,
        ));
        assert!(!valid_tether_prefix(
            IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1)),
            32,
        ));
        assert!(valid_tether_prefix(
            IpAddr::V6("fd00::1".parse::<Ipv6Addr>().unwrap()),
            64,
        ));
        assert!(!valid_tether_prefix(
            IpAddr::V6("fd00::1".parse::<Ipv6Addr>().unwrap()),
            4,
        ));
        assert!(!valid_tether_prefix(
            IpAddr::V6("2001:4860:4860::8888".parse::<Ipv6Addr>().unwrap()),
            64,
        ));
    }

    #[test]
    fn route_restore_plan_never_replaces_newer_state() {
        assert_eq!(missing_original_indices(&[1], &[1, 2]), Some(vec![1]));
        assert_eq!(missing_original_indices(&[1, 3], &[1, 2]), None);
        assert_eq!(missing_original_indices(&[1, 2], &[1, 1, 2]), Some(vec![1]),);
        assert_eq!(missing_original_indices(&[1, 1], &[1]), None);
        assert!(same_multiset(&[1, 2, 1], &[1, 1, 2]));
        assert!(!same_multiset(&[1], &[1, 2]));
        assert!(!same_multiset(&[1, 3], &[1, 2]));
    }

    #[test]
    fn missing_captured_gateway_blocks_journal_completion() {
        let captured = IpAddr::V4(Ipv4Addr::new(192, 168, 42, 129));
        assert!(captured_gateway_is_available(captured, &[captured]));
        assert!(!captured_gateway_is_available(
            captured,
            &[IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1))],
        ));
        assert!(captured_gateway_is_available(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            &[],
        ));
    }

    #[test]
    fn rollback_retains_failed_entries_for_a_later_retry() {
        let mut entries = vec![0, 1, 2, 3];
        let mut order = Vec::new();
        let errors = rollback_suffix(&mut entries, 1, |entry| {
            order.push(*entry);
            if *entry == 2 {
                Err(io::Error::other("synthetic restore failure"))
            } else {
                Ok(())
            }
        });

        assert_eq!(order, vec![3, 2, 1]);
        assert_eq!(entries, vec![0, 2]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("synthetic restore failure"));
    }

    #[test]
    fn flag_restore_only_undoes_the_value_owned_by_the_policy() {
        assert!(should_restore_owned_flag(true, false));
        assert!(!should_restore_owned_flag(false, false));
        assert!(!should_restore_owned_flag(true, true));
        assert!(!should_restore_owned_flag(false, true));
    }

    #[test]
    fn peer_classification_rejects_non_tether_and_ambiguous_subnets() {
        let snapshot = TetherSnapshot::from_prefixes(vec![TetherPrefix {
            interface_index: 7,
            address: IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1)),
            prefix_length: 24,
        }]);
        assert_eq!(
            snapshot.classify_peer(IpAddr::V4(Ipv4Addr::new(192, 168, 42, 129))),
            Some(7),
        );
        assert_eq!(
            snapshot.classify_peer(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
            None,
        );

        let ambiguous = TetherSnapshot::from_prefixes(vec![
            TetherPrefix {
                interface_index: 7,
                address: IpAddr::V4(Ipv4Addr::new(192, 168, 42, 1)),
                prefix_length: 24,
            },
            TetherPrefix {
                interface_index: 9,
                address: IpAddr::V4(Ipv4Addr::new(192, 168, 42, 2)),
                prefix_length: 24,
            },
        ]);
        assert_eq!(
            ambiguous.classify_peer(IpAddr::V4(Ipv4Addr::new(192, 168, 42, 129))),
            None,
        );
    }

    #[test]
    fn durable_policy_round_trips_and_rejects_corruption() {
        let policy = sample_policy();
        let encoded = encode_policy(&policy).unwrap();
        assert_eq!(decode_policy(&encoded, "test").unwrap(), policy);

        let mut corrupted = encoded;
        corrupted[12] ^= 1;
        assert_eq!(
            decode_policy(&corrupted, "test").unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn recovery_requires_the_same_adapter_instance() {
        let expected = sample_policy().adapter;
        let mut current = expected.clone();
        assert!(same_adapter_instance(&expected, &current));
        current.interface_index ^= 1;
        assert!(same_adapter_instance(&expected, &current));
        current.luid ^= 1;
        assert!(!same_adapter_instance(&expected, &current));
    }

    #[test]
    fn live_binding_requires_the_original_interface_and_adapter() {
        let policy = sample_policy();
        let expected = TetherBinding {
            interface_index: policy.adapter.interface_index,
            adapter: policy.adapter,
            validate_runtime: true,
        };
        let mut current = expected.clone();
        assert!(expected.matches_current(&current));

        current.interface_index ^= 1;
        current.adapter.interface_index = current.interface_index;
        assert!(!expected.matches_current(&current));

        current = expected.clone();
        current.adapter.luid ^= 1;
        assert!(!expected.matches_current(&current));
    }

    #[test]
    fn privileged_rows_require_the_discovered_interface_luid() {
        let adapter = sample_policy().adapter;
        assert!(interface_identity_matches(
            adapter.interface_index,
            adapter.luid,
            &adapter,
        ));
        assert!(!interface_identity_matches(
            adapter.interface_index ^ 1,
            adapter.luid,
            &adapter,
        ));
        assert!(!interface_identity_matches(
            adapter.interface_index,
            adapter.luid ^ 1,
            &adapter,
        ));
    }

    #[test]
    fn reconstructed_link_local_gateway_uses_the_current_interface_scope() {
        let mut policy = sample_policy();
        policy.adapter.interface_index = 31;
        policy.routes[0].next_hop = IpAddr::V6("fe80::1".parse().unwrap());
        policy.routes[0].next_hop_scope_id = 9;

        let row = restore_route_row(&policy.routes[0], &policy.adapter, IPV6).unwrap();

        assert_eq!(
            sockaddr_inet_ip(&row.NextHop),
            Some((policy.routes[0].next_hop, 31)),
        );
    }

    fn sample_policy() -> PersistedPolicy {
        PersistedPolicy {
            owner: ProcessOwner {
                pid: 42,
                creation_time: 123,
                image_name: "host.exe".to_owned(),
            },
            adapter: AdapterIdentity {
                adapter_name: "{adapter-guid}".to_owned(),
                luid: 7,
                network_guid: [3; 16],
                physical_address: vec![1, 2, 3, 4, 5, 6],
                interface_index: 9,
                description: "Remote NDIS".to_owned(),
                friendly_name: "USB tether".to_owned(),
            },
            family: IPV4,
            original_disable_default_routes: false,
            routes: vec![PersistedRoute {
                next_hop: IpAddr::V4(Ipv4Addr::new(192, 168, 42, 129)),
                next_hop_scope_id: 0,
                site_prefix_length: 0,
                valid_lifetime: u32::MAX,
                preferred_lifetime: u32::MAX,
                metric: 5,
                protocol: 3,
                loopback: false,
                autoconfigure_address: false,
                publish: false,
                immortal: true,
                origin: 0,
            }],
        }
    }
}
