//! Reversible Windows routing policy for the opt-in local-only tether mode.
//!
//! Android USB tethering normally installs a default route through the phone.
//! When this policy is active, the RNDIS interface keeps its connected phone
//! subnet but cannot become an internet gateway. The original interface flags
//! and default routes are restored when the guard is dropped.

use std::io;
use std::mem::size_of;
use std::net::{IpAddr, SocketAddr};
use std::ptr::{null, null_mut};
use std::slice;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_BUFFER_OVERFLOW, ERROR_FILE_NOT_FOUND,
    ERROR_NO_DATA, ERROR_NOT_FOUND, ERROR_OBJECT_ALREADY_EXISTS, ERROR_SUCCESS,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CreateIpForwardEntry2, DeleteIpForwardEntry2, FreeMibTable, GAA_FLAG_INCLUDE_GATEWAYS,
    GetAdaptersAddresses, GetBestInterfaceEx, GetIpForwardTable2, GetIpInterfaceEntry,
    IP_ADAPTER_ADDRESSES_LH, InitializeIpInterfaceEntry, MIB_IPFORWARD_ROW2, MIB_IPINTERFACE_ROW,
    SetIpInterfaceEntry,
};
use windows_sys::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, AF_UNSPEC, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0,
    SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
};

const IPV4: ADDRESS_FAMILY = AF_INET;
const IPV6: ADDRESS_FAMILY = AF_INET6;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RouteKey {
    interface_index: u32,
    family: ADDRESS_FAMILY,
    prefix_length: u8,
    prefix: [u8; 16],
    next_hop: [u8; 16],
    site_prefix_length: u8,
    metric: u32,
    protocol: i32,
    origin: i32,
}

#[derive(Clone)]
struct InterfaceSnapshot {
    interface_index: u32,
    family: ADDRESS_FAMILY,
    disable_default_routes: bool,
    default_routes: Vec<MIB_IPFORWARD_ROW2>,
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
    pub fn new() -> Self {
        Self {
            snapshots: Vec::new(),
        }
    }

    /// Protect currently-present Android/RNDIS adapters.
    ///
    /// It is valid for this to find no adapter yet. The caller can retry after
    /// discovery when Windows has not finished creating the tether interface.
    pub fn refresh(&mut self) -> io::Result<()> {
        for interface_index in tether_interface_indices()? {
            self.protect_interface(interface_index)?;
        }
        Ok(())
    }

    /// Protect the interface Windows used to receive a discovered phone.
    pub fn protect_peer(&mut self, peer: SocketAddr) -> io::Result<()> {
        let interface_index = interface_index_for_peer(peer)?;
        if !tether_interface_indices()?.contains(&interface_index) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "the discovered phone is not connected through a recognized Android/RNDIS adapter (interface {interface_index})"
                ),
            ));
        }
        self.protect_interface(interface_index)
    }

    /// Restore all interface flags and default routes captured by this guard.
    pub fn restore(&mut self) -> io::Result<()> {
        let snapshots = self.snapshots.clone();
        let mut first_error = None;
        for snapshot in snapshots.iter().rev() {
            if let Err(error) = restore_snapshot(snapshot)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            self.snapshots.clear();
            Ok(())
        }
    }

    fn protect_interface(&mut self, interface_index: u32) -> io::Result<()> {
        let protected_families: Vec<_> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.interface_index == interface_index)
            .map(|snapshot| snapshot.family)
            .collect();
        for family in &protected_families {
            enforce_family(interface_index, *family)?;
        }

        let mut added = Vec::new();
        for family in [IPV4, IPV6] {
            if protected_families.contains(&family) {
                continue;
            }
            match protect_family(interface_index, family) {
                Ok(Some(snapshot)) => added.push(snapshot),
                Ok(None) => {}
                Err(error) => {
                    for snapshot in added.iter().rev() {
                        let _ = restore_snapshot(snapshot);
                    }
                    return Err(error);
                }
            }
        }
        self.snapshots.extend(added);
        Ok(())
    }
}

impl Default for TetherRoutePolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TetherRoutePolicy {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("local-only tether cleanup failed: {error}");
        }
    }
}

fn protect_family(
    interface_index: u32,
    family: ADDRESS_FAMILY,
) -> io::Result<Option<InterfaceSnapshot>> {
    let Some(interface) = interface_row(interface_index, family)? else {
        return Ok(None);
    };
    let default_routes = default_routes(interface_index, family)?;
    let disable_default_routes = interface.DisableDefaultRoutes;

    if !disable_default_routes {
        let mut changed = interface;
        prepare_interface_row_for_set(&mut changed, true);
        check_win32(
            unsafe { SetIpInterfaceEntry(&mut changed) },
            "disable default routes on the tether interface",
        )?;
    }

    let mut deleted = Vec::new();
    for route in &default_routes {
        let result = unsafe { DeleteIpForwardEntry2(route) };
        if result != ERROR_SUCCESS && !is_missing(result) {
            for deleted_route in deleted.iter() {
                let _ = unsafe { CreateIpForwardEntry2(deleted_route) };
            }
            let _ = restore_interface_flag(interface_index, family, disable_default_routes);
            return Err(win32_error(
                "remove the tether interface default route",
                result,
            ));
        }
        if result == ERROR_SUCCESS {
            deleted.push(*route);
        }
    }

    Ok(Some(InterfaceSnapshot {
        interface_index,
        family,
        disable_default_routes,
        default_routes,
    }))
}

fn enforce_family(interface_index: u32, family: ADDRESS_FAMILY) -> io::Result<()> {
    let Some(mut interface) = interface_row(interface_index, family)? else {
        return Ok(());
    };
    if !interface.DisableDefaultRoutes {
        prepare_interface_row_for_set(&mut interface, true);
        check_win32(
            unsafe { SetIpInterfaceEntry(&mut interface) },
            "disable default routes on the tether interface",
        )?;
    }

    for route in default_routes(interface_index, family)? {
        let result = unsafe { DeleteIpForwardEntry2(&route) };
        if result != ERROR_SUCCESS && !is_missing(result) {
            return Err(win32_error(
                "remove a newly-added tether interface default route",
                result,
            ));
        }
    }
    Ok(())
}

fn restore_snapshot(snapshot: &InterfaceSnapshot) -> io::Result<()> {
    if interface_row(snapshot.interface_index, snapshot.family)?.is_none() {
        return Ok(());
    }
    let current_routes = match default_routes(snapshot.interface_index, snapshot.family) {
        Ok(routes) => routes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    let original_keys: Vec<RouteKey> = snapshot.default_routes.iter().map(route_key).collect();

    for route in &current_routes {
        if !original_keys.iter().any(|key| key == &route_key(route)) {
            let result = unsafe { DeleteIpForwardEntry2(route) };
            if result != ERROR_SUCCESS && !is_missing(result) {
                return Err(win32_error(
                    "remove a temporary tether default route",
                    result,
                ));
            }
        }
    }

    let remaining_routes = default_routes(snapshot.interface_index, snapshot.family)?;
    for route in &snapshot.default_routes {
        if !remaining_routes
            .iter()
            .any(|current| route_key(current) == route_key(route))
        {
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
        }
    }

    restore_interface_flag(
        snapshot.interface_index,
        snapshot.family,
        snapshot.disable_default_routes,
    )
}

fn restore_interface_flag(
    interface_index: u32,
    family: ADDRESS_FAMILY,
    disable_default_routes: bool,
) -> io::Result<()> {
    let Some(mut interface) = interface_row(interface_index, family)? else {
        return Ok(());
    };
    if interface.DisableDefaultRoutes == disable_default_routes {
        return Ok(());
    }
    prepare_interface_row_for_set(&mut interface, disable_default_routes);
    check_win32(
        unsafe { SetIpInterfaceEntry(&mut interface) },
        "restore the tether interface default-route setting",
    )
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

fn tether_interface_indices() -> io::Result<Vec<u32>> {
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

    let mut indices = Vec::new();
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
            && !indices.contains(&interface_index)
        {
            indices.push(interface_index);
        }
        adapter = current.Next;
    }
    Ok(indices)
}

fn interface_index_for_peer(peer: SocketAddr) -> io::Result<u32> {
    let mut index = 0_u32;
    match peer.ip() {
        IpAddr::V4(address) => {
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
        IpAddr::V6(address) => {
            let sockaddr = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: 0,
                sin6_flowinfo: 0,
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: address.octets(),
                    },
                },
                Anonymous: Default::default(),
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
        site_prefix_length: route.SitePrefixLength,
        metric: route.Metric,
        protocol: route.Protocol,
        origin: route.Origin,
    }
}

fn is_tether_adapter(identity: &str) -> bool {
    identity.contains("rndis")
        || identity.contains("remote ndis")
        || identity.contains("usb tether")
        || identity.contains("android")
        || identity.contains("ncm")
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

#[cfg(test)]
mod tests {
    use super::{IPV4, IPV6, is_tether_adapter, prepare_interface_row_for_set};
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
}
