//! Read-only classification of the network interface used to reach a phone.
//!
//! This is deliberately separate from the Windows-only route policy. Both
//! supported hosts reject discovery from ordinary LAN/Wi-Fi interfaces. Only
//! Windows mutates raw route state; Linux delegates persistent local-only
//! policy to NetworkManager in the desktop launcher.

#[cfg(windows)]
pub use crate::tether_policy::{TetherBinding, current_tether_binding, tether_ipv4_interfaces};

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CStr, OsStr, OsString};
    use std::fs;
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::ptr;

    const MAX_IP_OUTPUT: usize = 64 * 1024;
    const IP_CANDIDATES: [&str; 5] = [
        "/usr/sbin/ip",
        "/sbin/ip",
        "/usr/bin/ip",
        "/bin/ip",
        "/run/current-system/sw/bin/ip",
    ];

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct InterfaceIdentity {
        index: u32,
        name: OsString,
        device_path: PathBuf,
        driver: OsString,
        hardware_address: String,
    }

    /// Exact Linux RNDIS device identity shared with the desktop launcher.
    ///
    /// The launcher uses this only to select a NetworkManager connection
    /// profile. Keeping the identity implementation here ensures route-policy
    /// UI never grows a broader USB-driver allowlist than gameplay discovery.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct LinuxTetherDevice {
        identity: InterfaceIdentity,
    }

    impl LinuxTetherDevice {
        pub fn interface_name(&self) -> &OsStr {
            &self.identity.name
        }

        /// Re-read every immutable identity field before or after an external
        /// NetworkManager operation. An interface-index reuse or USB-device
        /// replacement therefore fails closed.
        pub fn verify_present(&self) -> io::Result<bool> {
            Ok(
                usb_network_identity(&self.identity.name, self.identity.index)?
                    .is_some_and(|current| current == self.identity),
            )
        }

        /// Read every kernel routing table and report whether this exact
        /// interface still owns an IPv4 or IPv6 default route.
        pub fn default_routes_present(&self) -> io::Result<(bool, bool)> {
            default_routes_for_identity(&self.identity)
        }
    }

    /// Immutable identity of the USB network interface accepted at discovery.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TetherBinding {
        identity: InterfaceIdentity,
        validate_runtime: bool,
    }

    impl TetherBinding {
        pub fn interface_index(&self) -> u32 {
            self.identity.index
        }

        pub fn interface_name(&self) -> &OsStr {
            &self.identity.name
        }

        /// Read-only local-only-policy verification used when a Linux host
        /// accepts a newly discovered tether connection.
        pub fn default_routes_present(&self) -> io::Result<(bool, bool)> {
            default_routes_for_identity(&self.identity)
        }

        /// Require receive-side packet metadata to name the same interface
        /// accepted during discovery. Tests use synthetic bindings without
        /// runtime validation because loopback is not a USB device.
        pub(crate) fn accepts_ingress_interface(&self, interface_index: Option<u32>) -> bool {
            !self.validate_runtime || interface_index == Some(self.identity.index)
        }

        /// Re-resolve the route and USB-device identity before accepting a
        /// repeated or migrated discovery hello. Interface-index reuse alone
        /// is not sufficient to pass this check.
        pub fn verify_peer(&self, peer: SocketAddr) -> io::Result<()> {
            if !self.validate_runtime {
                return Ok(());
            }
            let Some(current) = current_tether_binding(peer)? else {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!("the peer {peer} is no longer on a confirmed USB tether route"),
                ));
            };
            if self.identity == current.identity {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!(
                        "the peer {peer} no longer uses the USB interface accepted at discovery"
                    ),
                ))
            }
        }

        #[cfg(test)]
        pub(crate) fn for_test(interface_index: u32) -> Self {
            Self {
                identity: InterfaceIdentity {
                    index: interface_index,
                    name: OsString::from("test"),
                    device_path: PathBuf::from("/test"),
                    driver: OsString::from("rndis_host"),
                    hardware_address: "00:00:00:00:00:00".to_owned(),
                },
                validate_runtime: false,
            }
        }
    }

    /// Return a binding only when the kernel route to `peer` resolves to one
    /// unambiguous, up USB-network interface using Linux's RNDIS host driver.
    /// This is read-only; it never changes routes or link state.
    pub fn current_tether_binding(peer: SocketAddr) -> io::Result<Option<TetherBinding>> {
        let local_ip = routed_local_ip(peer)?;
        let mut interfaces = interfaces_with_address(local_ip, peer.ip())?;
        // A connected UDP socket exposes the source address selected by the
        // kernel, not its egress ifindex. Fail closed when that address is
        // configured on more than one up interface: filtering the list down
        // to its one USB member could otherwise misidentify an ordinary LAN
        // interface carrying the same address as the selected route.
        if interfaces.len() != 1 {
            return Ok(None);
        }
        let (name, index) = interfaces.remove(0);
        let Some(identity) = usb_network_identity(&name, index)? else {
            return Ok(None);
        };
        Ok(Some(TetherBinding {
            identity,
            validate_runtime: true,
        }))
    }

    /// IPv4 addresses and interface indices that are safe for a v5 USB
    /// discovery listener before the peer address is known.
    pub fn tether_ipv4_interfaces() -> io::Result<Vec<(Ipv4Addr, u32)>> {
        let devices = linux_tether_devices()?;
        let mut head = ptr::null_mut();
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let addresses = IfAddrs(head);
        let mut listeners = Vec::new();
        let mut current = addresses.0;
        while !current.is_null() {
            let entry = unsafe { &*current };
            if !entry.ifa_name.is_null()
                && !entry.ifa_addr.is_null()
                && !entry.ifa_netmask.is_null()
                && entry.ifa_flags & libc::IFF_UP as u32 != 0
                && entry.ifa_flags & libc::IFF_LOOPBACK as u32 == 0
            {
                let name = OsString::from_vec(
                    unsafe { CStr::from_ptr(entry.ifa_name) }
                        .to_bytes()
                        .to_vec(),
                );
                let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
                let local = sockaddr_ip(entry.ifa_addr);
                let netmask = sockaddr_ip(entry.ifa_netmask);
                if index != 0
                    && devices.iter().any(|device| {
                        device.identity.index == index && device.identity.name == name
                    })
                    && let (Some(IpAddr::V4(local)), Some(IpAddr::V4(netmask))) = (local, netmask)
                    && valid_tether_prefix(
                        IpAddr::V4(local),
                        IpAddr::V4(local),
                        IpAddr::V4(netmask),
                    )
                {
                    let candidate = (local, index);
                    if !listeners.contains(&candidate) {
                        listeners.push(candidate);
                    }
                }
            }
            current = entry.ifa_next;
        }
        Ok(listeners)
    }

    /// Enumerate present USB network devices accepted by Linux gameplay
    /// discovery. This is read-only and intentionally returns only
    /// `rndis_host` devices; NetworkManager decides which one is connected.
    pub fn linux_tether_devices() -> io::Result<Vec<LinuxTetherDevice>> {
        let entries = match fs::read_dir("/sys/class/net") {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut devices = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let index = match fs::read_to_string(entry.path().join("ifindex")) {
                Ok(value) => value.trim().parse::<u32>().ok(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let Some(index) = index else {
                continue;
            };
            if let Some(identity) = usb_network_identity(&name, index)? {
                devices.push(LinuxTetherDevice { identity });
            }
        }
        Ok(devices)
    }

    fn routed_local_ip(peer: SocketAddr) -> io::Result<IpAddr> {
        let socket = match peer {
            SocketAddr::V4(_) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?,
            SocketAddr::V6(_) => UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))?,
        };
        socket.connect(peer)?;
        Ok(socket.local_addr()?.ip())
    }

    struct IfAddrs(*mut libc::ifaddrs);

    impl Drop for IfAddrs {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { libc::freeifaddrs(self.0) };
            }
        }
    }

    fn interfaces_with_address(address: IpAddr, peer: IpAddr) -> io::Result<Vec<(OsString, u32)>> {
        let mut head = ptr::null_mut();
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let addresses = IfAddrs(head);
        let mut current = addresses.0;
        let mut matches: Vec<(OsString, u32)> = Vec::new();
        while !current.is_null() {
            let entry = unsafe { &*current };
            if !entry.ifa_name.is_null()
                && !entry.ifa_addr.is_null()
                && !entry.ifa_netmask.is_null()
                && entry.ifa_flags & libc::IFF_UP as u32 != 0
                && sockaddr_ip(entry.ifa_addr) == Some(address)
                && sockaddr_ip(entry.ifa_netmask)
                    .is_some_and(|netmask| valid_tether_prefix(address, peer, netmask))
            {
                let name_bytes = unsafe { CStr::from_ptr(entry.ifa_name) }.to_bytes();
                let name = OsString::from_vec(name_bytes.to_vec());
                let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
                if index != 0 && !matches.iter().any(|item| item.1 == index) {
                    matches.push((name, index));
                }
            }
            current = entry.ifa_next;
        }
        Ok(matches)
    }

    fn sockaddr_ip(address: *const libc::sockaddr) -> Option<IpAddr> {
        match i32::from(unsafe { (*address).sa_family }) {
            libc::AF_INET => {
                let address = unsafe { &*address.cast::<libc::sockaddr_in>() };
                Some(IpAddr::V4(Ipv4Addr::from(
                    address.sin_addr.s_addr.to_ne_bytes(),
                )))
            }
            libc::AF_INET6 => {
                let address = unsafe { &*address.cast::<libc::sockaddr_in6>() };
                Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }

    fn valid_tether_prefix(local: IpAddr, peer: IpAddr, netmask: IpAddr) -> bool {
        match (local, peer, netmask) {
            (IpAddr::V4(local), IpAddr::V4(peer), IpAddr::V4(netmask)) => {
                let local = u32::from_be_bytes(local.octets());
                let peer = u32::from_be_bytes(peer.octets());
                let mask = u32::from_be_bytes(netmask.octets());
                let prefix_length = mask.leading_ones();
                if mask != u32::MAX.checked_shl(32 - prefix_length).unwrap_or(0) {
                    return false;
                }
                let first = local.to_be_bytes();
                let local_block_length = match first {
                    [10, _, _, _] => 8,
                    [172, second, _, _] if (16..=31).contains(&second) => 12,
                    [192, 168, _, _] | [169, 254, _, _] => 16,
                    _ => return false,
                };
                (local_block_length..=30).contains(&prefix_length) && peer & mask == local & mask
            }
            (IpAddr::V6(local), IpAddr::V6(peer), IpAddr::V6(netmask)) => {
                let local = u128::from_be_bytes(local.octets());
                let peer = u128::from_be_bytes(peer.octets());
                let mask = u128::from_be_bytes(netmask.octets());
                let prefix_length = mask.leading_ones();
                if mask != u128::MAX.checked_shl(128 - prefix_length).unwrap_or(0) {
                    return false;
                }
                let first = (local >> 112) as u16;
                let local_block_length = if first & 0xfe00 == 0xfc00 {
                    7
                } else if first & 0xffc0 == 0xfe80 {
                    10
                } else {
                    return false;
                };
                (local_block_length..=127).contains(&prefix_length) && peer & mask == local & mask
            }
            _ => false,
        }
    }

    fn usb_network_identity(name: &OsString, index: u32) -> io::Result<Option<InterfaceIdentity>> {
        let interface_dir = Path::new("/sys/class/net").join(name);
        let recorded_index = match fs::read_to_string(interface_dir.join("ifindex")) {
            Ok(value) => value.trim().parse::<u32>().ok(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if recorded_index != Some(index) {
            return Ok(None);
        }

        let device_dir = match fs::canonicalize(interface_dir.join("device")) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let subsystem = match fs::canonicalize(device_dir.join("subsystem")) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if subsystem.file_name().is_none_or(|value| value != "usb") {
            return Ok(None);
        }

        let driver_path = match fs::read_link(device_dir.join("driver")) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(driver) = driver_path.file_name().map(OsString::from) else {
            return Ok(None);
        };
        if !is_tether_capable_driver(&driver) {
            return Ok(None);
        }
        let hardware_address = fs::read_to_string(interface_dir.join("address"))
            .map(|value| value.trim().to_owned())?;

        Ok(Some(InterfaceIdentity {
            index,
            name: name.clone(),
            device_path: device_dir,
            driver,
            hardware_address,
        }))
    }

    fn is_tether_capable_driver(driver: &std::ffi::OsStr) -> bool {
        // Fail closed even though v5 authenticates its peer. Generic USB
        // Ethernet adapters commonly use cdc_ncm/cdc_ether/cdc_subset; treating
        // those as the explicitly selected phone tether would violate route
        // confinement. The documented Linux USB transport is RNDIS.
        driver.as_bytes() == b"rndis_host"
    }

    fn default_routes_for_identity(identity: &InterfaceIdentity) -> io::Result<(bool, bool)> {
        if usb_network_identity(&identity.name, identity.index)?.as_ref() != Some(identity) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the RNDIS interface changed before its routes could be inspected",
            ));
        }
        if !valid_command_interface_name(&identity.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the RNDIS interface has an unsafe or unsupported name",
            ));
        }
        let ip = trusted_ip_path()?;
        let ipv4 = default_route_present(&ip, "-4", &identity.name)?;
        let ipv6 = default_route_present(&ip, "-6", &identity.name)?;
        if usb_network_identity(&identity.name, identity.index)?.as_ref() != Some(identity) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the RNDIS interface changed while its routes were being inspected",
            ));
        }
        Ok((ipv4, ipv6))
    }

    fn default_route_present(ip: &Path, family: &str, interface: &OsStr) -> io::Result<bool> {
        let output = Command::new(ip)
            .arg(family)
            .args(["route", "show", "table", "all", "default", "dev"])
            .arg(interface)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        if output.stdout.len() > MAX_IP_OUTPUT || output.stderr.len() > MAX_IP_OUTPUT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "iproute2 returned an unexpectedly large response",
            ));
        }
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .take(512)
                .collect::<String>();
            return Err(io::Error::other(if detail.is_empty() {
                format!("iproute2 route inspection failed with {}", output.status)
            } else {
                format!("iproute2 route inspection failed: {detail}")
            }));
        }
        if output.stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
            let detail = String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .take(512)
                .collect::<String>();
            return Err(io::Error::other(format!(
                "iproute2 route inspection returned a warning: {detail}"
            )));
        }
        Ok(output.stdout.iter().any(|byte| !byte.is_ascii_whitespace()))
    }

    fn valid_command_interface_name(name: &OsStr) -> bool {
        let bytes = name.as_bytes();
        !bytes.is_empty()
            && bytes.len() < libc::IFNAMSIZ
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    }

    fn trusted_ip_path() -> io::Result<PathBuf> {
        for candidate in IP_CANDIDATES.map(Path::new) {
            if !candidate.exists() {
                continue;
            }
            let canonical = fs::canonicalize(candidate)?;
            if !trusted_executable_and_parents(&canonical)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing untrusted iproute2 executable at {}",
                        canonical.display()
                    ),
                ));
            }
            return Ok(canonical);
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "a trusted iproute2 `ip` executable was not found",
        ))
    }

    fn trusted_executable_and_parents(path: &Path) -> io::Result<bool> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o111 == 0
        {
            return Ok(false);
        }
        for parent in path.ancestors().skip(1) {
            let metadata = fs::metadata(parent)?;
            if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn recognizes_only_the_rndis_host_driver() {
            assert!(is_tether_capable_driver("rndis_host".as_ref()));
            for driver in [
                "cdc_ncm",
                "cdc_ether",
                "cdc_subset",
                "cdc_mbim",
                "ipheth",
                "r8169",
                "e1000e",
                "iwlwifi",
                "veth",
                "bridge",
                "",
            ] {
                assert!(!is_tether_capable_driver(driver.as_ref()));
            }
        }

        #[test]
        fn accepts_only_safe_interface_names_for_iproute2() {
            for name in ["usb0", "enp0s20f0u1", "rndis_0", "usb.1"] {
                assert!(valid_command_interface_name(name.as_ref()));
            }
            for name in ["", "--help", "usb 0", "usb/0", "abcdefghijklmnop"] {
                assert!(!valid_command_interface_name(name.as_ref()));
            }
        }

        #[test]
        fn accepts_only_contiguous_local_prefixes_containing_the_peer() {
            assert!(valid_tether_prefix(
                "192.168.42.10".parse().unwrap(),
                "192.168.42.129".parse().unwrap(),
                "255.255.255.0".parse().unwrap(),
            ));
            assert!(!valid_tether_prefix(
                "192.168.42.10".parse().unwrap(),
                "192.168.43.1".parse().unwrap(),
                "255.255.255.0".parse().unwrap(),
            ));
            assert!(!valid_tether_prefix(
                "8.8.8.10".parse().unwrap(),
                "8.8.8.11".parse().unwrap(),
                "255.255.255.0".parse().unwrap(),
            ));
            assert!(!valid_tether_prefix(
                "192.168.42.10".parse().unwrap(),
                "192.168.42.11".parse().unwrap(),
                "255.0.255.0".parse().unwrap(),
            ));
            assert!(valid_tether_prefix(
                "fe80::1".parse().unwrap(),
                "fe80::2".parse().unwrap(),
                "ffff:ffff:ffff:ffff::".parse().unwrap(),
            ));
            assert!(!valid_tether_prefix(
                "2001:db8::1".parse().unwrap(),
                "2001:db8::2".parse().unwrap(),
                "ffff:ffff:ffff:ffff::".parse().unwrap(),
            ));
        }

        #[test]
        fn runtime_binding_requires_the_exact_ingress_interface() {
            let mut binding = TetherBinding::for_test(7);
            binding.validate_runtime = true;
            assert!(binding.accepts_ingress_interface(Some(7)));
            assert!(!binding.accepts_ingress_interface(Some(8)));
            assert!(!binding.accepts_ingress_interface(None));
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    LinuxTetherDevice, TetherBinding, current_tether_binding, linux_tether_devices,
    tether_ipv4_interfaces,
};
