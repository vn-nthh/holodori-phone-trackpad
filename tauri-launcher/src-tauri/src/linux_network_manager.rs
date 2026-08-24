//! Safe Linux local-only tether control through NetworkManager.
//!
//! This module never edits kernel routes directly. It resolves the one active
//! NetworkManager profile attached to an exact `rndis_host` device, updates
//! both `never-default` properties by UUID through a version-guarded persistent
//! D-Bus write, and applies only those two changes with NetworkManager's
//! versioned device API. Reapply uses the `preserve-external-ip` flag so
//! unrelated addresses and routes survive.
//! Every external command is executed directly (never through a shell).

use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::{stdintf::org_freedesktop_dbus::Properties, Connection};
use holodori_native_host::tether::{linux_tether_devices, LinuxTetherDevice};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const NMCLI_WAIT_SECONDS: &str = "15";
const MINIMUM_NETWORK_MANAGER_VERSION: (u32, u32, u32) = (1, 44, 0);
const MAX_NMCLI_OUTPUT: usize = 64 * 1024;
const DBUS_READ_TIMEOUT: Duration = Duration::from_secs(15);
// dbus-rs converts Duration to libdbus's signed millisecond timeout. i32::MAX
// is libdbus's practical infinite timeout (~24 days), so an interactive polkit
// request cannot time out locally and then mutate NetworkManager after rollback.
const DBUS_MUTATION_TIMEOUT: Duration = Duration::from_millis(i32::MAX as u64);
const NETWORK_MANAGER_DESTINATION: &str = "org.freedesktop.NetworkManager";
const NETWORK_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";
const NETWORK_MANAGER_DEVICE_INTERFACE: &str = "org.freedesktop.NetworkManager.Device";
const NETWORK_MANAGER_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NETWORK_MANAGER_SETTINGS_INTERFACE: &str = "org.freedesktop.NetworkManager.Settings";
const NETWORK_MANAGER_SETTINGS_CONNECTION_INTERFACE: &str =
    "org.freedesktop.NetworkManager.Settings.Connection";
const PRESERVE_EXTERNAL_IP: u32 = 1;
const UPDATE_TO_DISK: u32 = 1;
const NMCLI_CANDIDATES: [&str; 4] = [
    "/usr/bin/nmcli",
    "/bin/nmcli",
    "/usr/local/bin/nmcli",
    "/run/current-system/sw/bin/nmcli",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LinuxLocalOnlyStatus {
    pub available: bool,
    pub enabled: bool,
    pub configured: bool,
    pub mixed: bool,
    pub interface_name: Option<String>,
    pub message: String,
}

impl LinuxLocalOnlyStatus {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            available: false,
            enabled: false,
            configured: false,
            mixed: false,
            interface_name: None,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Target<D> {
    device: D,
    interface_name: String,
    connection_uuid: String,
    device_dbus_path: String,
    profile_ipv4_never_default: bool,
    profile_ipv6_never_default: bool,
    applied_ipv4_never_default: bool,
    applied_ipv6_never_default: bool,
    ipv4_default_route_present: bool,
    ipv6_default_route_present: bool,
}

enum Resolution<D> {
    Target(Target<D>),
    Unavailable(String),
}

trait Backend {
    type Device: Clone + Debug + Eq;

    fn devices(&mut self) -> Result<Vec<Self::Device>, String>;
    fn device_name(&self, device: &Self::Device) -> Result<String, String>;
    fn verify_device(&mut self, device: &Self::Device) -> Result<bool, String>;
    fn default_routes_present(&mut self, device: &Self::Device) -> Result<(bool, bool), String>;
    fn daemon_version(&mut self) -> Result<String, String>;
    fn nmcli(&mut self, args: &[String]) -> Result<String, String>;
    fn read_profile(&mut self, connection_uuid: &str) -> Result<(bool, bool), String>;
    fn update_profile(
        &mut self,
        connection_uuid: &str,
        expected_ipv4: bool,
        expected_ipv6: bool,
        ipv4: bool,
        ipv6: bool,
    ) -> Result<(), String>;
    fn read_active(&mut self, device_dbus_path: &str) -> Result<(String, bool, bool), String>;
    fn apply_active(
        &mut self,
        device_dbus_path: &str,
        connection_uuid: &str,
        expected_ipv4: bool,
        expected_ipv6: bool,
        ipv4: bool,
        ipv6: bool,
    ) -> Result<(), String>;
}

struct SystemBackend {
    nmcli_path: PathBuf,
}

impl SystemBackend {
    fn discover() -> Result<Option<Self>, String> {
        trusted_nmcli_path().map(|path| path.map(|nmcli_path| Self { nmcli_path }))
    }
}

impl Backend for SystemBackend {
    type Device = LinuxTetherDevice;

    fn devices(&mut self) -> Result<Vec<Self::Device>, String> {
        linux_tether_devices()
            .map_err(|error| format!("Could not inspect Linux RNDIS devices: {error}"))
    }

    fn device_name(&self, device: &Self::Device) -> Result<String, String> {
        device
            .interface_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "The RNDIS interface name is not valid UTF-8.".to_owned())
    }

    fn verify_device(&mut self, device: &Self::Device) -> Result<bool, String> {
        device
            .verify_present()
            .map_err(|error| format!("Could not revalidate the Linux RNDIS device: {error}"))
    }

    fn default_routes_present(&mut self, device: &Self::Device) -> Result<(bool, bool), String> {
        device
            .default_routes_present()
            .map_err(|error| format!("Could not inspect the Linux RNDIS default routes: {error}"))
    }

    fn daemon_version(&mut self) -> Result<String, String> {
        read_daemon_version()
    }

    fn nmcli(&mut self, args: &[String]) -> Result<String, String> {
        let output = Command::new(&self.nmcli_path)
            .args(args)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("Could not run NetworkManager's nmcli: {error}"))?;
        if output.stdout.len() > MAX_NMCLI_OUTPUT || output.stderr.len() > MAX_NMCLI_OUTPUT {
            return Err("NetworkManager returned an unexpectedly large response.".to_owned());
        }
        if !output.status.success() {
            let detail = concise_output(&output.stderr);
            return Err(if detail.is_empty() {
                format!("NetworkManager request failed with {}.", output.status)
            } else {
                format!("NetworkManager request failed: {detail}")
            });
        }
        if output.stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
            return Err(format!(
                "NetworkManager request returned a warning: {}",
                concise_output(&output.stderr)
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| "NetworkManager returned non-UTF-8 output in the C locale.".to_owned())
    }

    fn apply_active(
        &mut self,
        device_dbus_path: &str,
        connection_uuid: &str,
        expected_ipv4: bool,
        expected_ipv6: bool,
        ipv4: bool,
        ipv6: bool,
    ) -> Result<(), String> {
        apply_active_connection(
            device_dbus_path,
            connection_uuid,
            expected_ipv4,
            expected_ipv6,
            ipv4,
            ipv6,
        )
    }

    fn read_active(&mut self, device_dbus_path: &str) -> Result<(String, bool, bool), String> {
        read_active_connection(device_dbus_path)
    }

    fn read_profile(&mut self, connection_uuid: &str) -> Result<(bool, bool), String> {
        read_persistent_connection(connection_uuid)
    }

    fn update_profile(
        &mut self,
        connection_uuid: &str,
        expected_ipv4: bool,
        expected_ipv6: bool,
        ipv4: bool,
        ipv6: bool,
    ) -> Result<(), String> {
        update_persistent_connection(connection_uuid, expected_ipv4, expected_ipv6, ipv4, ipv6)
    }
}

pub fn status() -> Result<LinuxLocalOnlyStatus, String> {
    let Some(mut backend) = SystemBackend::discover()? else {
        return Ok(LinuxLocalOnlyStatus::unavailable(
            "NetworkManager is unavailable; configure never-default manually.",
        ));
    };
    status_with_backend(&mut backend)
}

pub fn set_enabled(enabled: bool) -> Result<LinuxLocalOnlyStatus, String> {
    let Some(mut backend) = SystemBackend::discover()? else {
        return Err(
            "NetworkManager's trusted nmcli executable was not found; configure never-default manually."
                .to_owned(),
        );
    };
    set_with_backend(&mut backend, enabled)
}

pub fn ensure_enabled() -> Result<(), String> {
    let Some(mut backend) = SystemBackend::discover()? else {
        return Err(
            "NetworkManager's trusted nmcli executable was not found, so local-only tethering cannot be verified."
                .to_owned(),
        );
    };
    match resolve_target(&mut backend)? {
        Resolution::Target(target) if policy_enabled(&target) => Ok(()),
        Resolution::Target(_) => Err(
            "Local-only tethering is not verified: require both persistent and active never-default settings and no IPv4 or IPv6 default route on the RNDIS interface."
                .to_owned(),
        ),
        Resolution::Unavailable(message) => Err(message),
    }
}

fn status_with_backend<B: Backend>(backend: &mut B) -> Result<LinuxLocalOnlyStatus, String> {
    match resolve_target(backend)? {
        Resolution::Target(target) => Ok(target_status(&target)),
        Resolution::Unavailable(message) => Ok(LinuxLocalOnlyStatus::unavailable(message)),
    }
}

fn target_status<D>(target: &Target<D>) -> LinuxLocalOnlyStatus {
    let enabled = policy_enabled(target);
    let configured = settings_all(target, true);
    let disabled = settings_all(target, false);
    let mixed = !enabled && !disabled;
    let message = if enabled {
        format!(
            "NetworkManager local-only policy is active on {}; no IPv4 or IPv6 default route is present.",
            target.interface_name
        )
    } else if disabled {
        if target.ipv4_default_route_present || target.ipv6_default_route_present {
            format!(
                "NetworkManager local-only policy is off; {} currently has a default route.",
                target.interface_name
            )
        } else {
            format!(
                "NetworkManager currently allows {} to become a default route.",
                target.interface_name
            )
        }
    } else if target.ipv4_default_route_present || target.ipv6_default_route_present {
        let route_subject = match (
            target.ipv4_default_route_present,
            target.ipv6_default_route_present,
        ) {
            (true, true) => "IPv4 and IPv6 default routes still use",
            (true, false) => "An IPv4 default route still uses",
            (false, true) => "An IPv6 default route still uses",
            (false, false) => unreachable!(),
        };
        format!(
            "{route_subject} {}; local-only mode is not safe to start. Reconnect after enabling or remove the route in NetworkManager.",
            target.interface_name
        )
    } else if mixed {
        format!(
            "{} has different persistent or active IPv4/IPv6 settings; choose the checkbox to normalize all four.",
            target.interface_name
        )
    } else {
        unreachable!("enabled, disabled, and mixed exhaust the policy state")
    };
    LinuxLocalOnlyStatus {
        available: true,
        enabled,
        configured,
        mixed,
        interface_name: Some(target.interface_name.clone()),
        message,
    }
}

fn settings_all<D>(target: &Target<D>, value: bool) -> bool {
    target.profile_ipv4_never_default == value
        && target.profile_ipv6_never_default == value
        && target.applied_ipv4_never_default == value
        && target.applied_ipv6_never_default == value
}

fn policy_enabled<D>(target: &Target<D>) -> bool {
    settings_all(target, true)
        && !target.ipv4_default_route_present
        && !target.ipv6_default_route_present
}

fn resolve_target<B: Backend>(backend: &mut B) -> Result<Resolution<B::Device>, String> {
    let running = backend.nmcli(&fixed_args(&[
        "--wait",
        NMCLI_WAIT_SECONDS,
        "--get-values",
        "RUNNING",
        "general",
    ]))?;
    if running.trim() != "running" {
        return Ok(Resolution::Unavailable(
            "NetworkManager is installed but is not running.".to_owned(),
        ));
    }
    let version = parse_network_manager_version(&backend.daemon_version()?)?;
    if version < MINIMUM_NETWORK_MANAGER_VERSION {
        return Ok(Resolution::Unavailable(format!(
            "NetworkManager {}.{}.{} is too old for version-guarded profile updates; version 1.44 or newer is required.",
            version.0, version.1, version.2
        )));
    }

    let mut active = Vec::new();
    for device in backend.devices()? {
        let interface_name = backend.device_name(&device)?;
        if !valid_interface_name(&interface_name) {
            return Err("The RNDIS interface has an unsafe or unsupported name.".to_owned());
        }
        let output = backend.nmcli(&[
            "--wait".to_owned(),
            NMCLI_WAIT_SECONDS.to_owned(),
            "--get-values".to_owned(),
            "GENERAL.STATE,GENERAL.CON-UUID,GENERAL.DBUS-PATH".to_owned(),
            "device".to_owned(),
            "show".to_owned(),
            interface_name.clone(),
        ])?;
        let Some((connection_uuid, device_dbus_path)) = parse_active_device(&output)? else {
            continue;
        };
        if !backend.verify_device(&device)? {
            return Err(format!(
                "RNDIS interface {interface_name} changed identity while NetworkManager was being inspected."
            ));
        }
        active.push((device, interface_name, connection_uuid, device_dbus_path));
    }

    let (device, interface_name, connection_uuid, device_dbus_path) = match active.len() {
        0 => {
            return Ok(Resolution::Unavailable(
                "Connect one Android phone with USB tethering, then check again.".to_owned(),
            ));
        }
        1 => active.pop().expect("length checked"),
        _ => {
            return Ok(Resolution::Unavailable(
                "More than one active RNDIS tether is present; disconnect extras before changing network policy."
                    .to_owned(),
            ));
        }
    };

    let (ipv4_never_default, ipv6_never_default) = read_profile(backend, &connection_uuid)?;
    let (applied_uuid, applied_ipv4_never_default, applied_ipv6_never_default) =
        backend.read_active(&device_dbus_path)?;
    if applied_uuid != connection_uuid {
        return Err(
            "The applied NetworkManager profile UUID differs from the active profile.".to_owned(),
        );
    }
    if !backend.verify_device(&device)? {
        return Err(format!(
            "RNDIS interface {interface_name} changed identity while its profile was being inspected."
        ));
    }
    let (ipv4_default_route_present, ipv6_default_route_present) =
        backend.default_routes_present(&device)?;
    if !backend.verify_device(&device)? {
        return Err(format!(
            "RNDIS interface {interface_name} changed identity while its routes were being inspected."
        ));
    }

    Ok(Resolution::Target(Target {
        device,
        interface_name,
        connection_uuid,
        device_dbus_path,
        profile_ipv4_never_default: ipv4_never_default,
        profile_ipv6_never_default: ipv6_never_default,
        applied_ipv4_never_default,
        applied_ipv6_never_default,
        ipv4_default_route_present,
        ipv6_default_route_present,
    }))
}

fn set_with_backend<B: Backend>(
    backend: &mut B,
    enabled: bool,
) -> Result<LinuxLocalOnlyStatus, String> {
    let target = match resolve_target(backend)? {
        Resolution::Target(target) => target,
        Resolution::Unavailable(message) => return Err(message),
    };
    if settings_all(&target, enabled) && (!enabled || policy_enabled(&target)) {
        return Ok(target_status(&target));
    }
    if !backend.verify_device(&target.device)? {
        return Err(
            "The selected RNDIS device changed before its profile could be updated.".to_owned(),
        );
    }
    let original_profile = (
        target.profile_ipv4_never_default,
        target.profile_ipv6_never_default,
    );
    if read_profile(backend, &target.connection_uuid)? != original_profile {
        return Err(
            "The persistent never-default values changed concurrently; their newer values were preserved. Check tether again before retrying."
                .to_owned(),
        );
    }

    if let Err(error) = modify_profile(
        backend,
        &target.connection_uuid,
        original_profile,
        (enabled, enabled),
    ) {
        let rollback = confirm_original_or_rollback(backend, &target, (enabled, enabled));
        return Err(with_rollback(
            &format!("Could not update the NetworkManager profile: {error}"),
            rollback,
        ));
    }
    match read_profile(backend, &target.connection_uuid) {
        Ok(current) if current == (enabled, enabled) => {}
        Ok(_) => {
            let rollback = rollback_profile(backend, &target, (enabled, enabled));
            return Err(with_rollback(
                "The persistent never-default values changed concurrently after the update.",
                rollback,
            ));
        }
        Err(error) => {
            let rollback = rollback_profile(backend, &target, (enabled, enabled));
            return Err(with_rollback(
                &format!("Could not verify the persistent profile update: {error}"),
                rollback,
            ));
        }
    }
    if !backend.verify_device(&target.device)? {
        let rollback = rollback_profile(backend, &target, (enabled, enabled));
        return Err(with_rollback(
            "The selected RNDIS device changed after its profile was updated.",
            rollback,
        ));
    }
    if let Err(error) = apply_profile(backend, &target, enabled, enabled) {
        let rollback = rollback_profile(backend, &target, (enabled, enabled));
        return Err(with_rollback(&error, rollback));
    }

    let verified = match resolve_target(backend) {
        Ok(Resolution::Target(current))
            if current.device == target.device
                && current.connection_uuid == target.connection_uuid
                && current.device_dbus_path == target.device_dbus_path
                && settings_all(&current, enabled)
                && (!enabled || policy_enabled(&current)) =>
        {
            current
        }
        Ok(Resolution::Target(current))
            if enabled
                && current.device == target.device
                && current.connection_uuid == target.connection_uuid
                && current.device_dbus_path == target.device_dbus_path
                && settings_all(&current, true) =>
        {
            // The user-authorized profile change is durable, but an external
            // route or the pre-1.58 DHCPv6 reapply behavior left a kernel
            // default in place. Keep the safe persistent intent so a reconnect
            // can activate it, but report a mixed/pending state. The frontend
            // blocks Start and ensure_enabled independently refuses it.
            current
        }
        Ok(_) => {
            let rollback = rollback_profile(backend, &target, (enabled, enabled));
            return Err(with_rollback(
                "NetworkManager did not retain the requested setting on the exact RNDIS profile.",
                rollback,
            ));
        }
        Err(error) => {
            let rollback = rollback_profile(backend, &target, (enabled, enabled));
            return Err(with_rollback(
                &format!("Could not verify the NetworkManager update: {error}"),
                rollback,
            ));
        }
    };
    Ok(target_status(&verified))
}

fn modify_profile<B: Backend>(
    backend: &mut B,
    uuid: &str,
    expected: (bool, bool),
    desired: (bool, bool),
) -> Result<(), String> {
    backend.update_profile(uuid, expected.0, expected.1, desired.0, desired.1)
}

fn read_profile<B: Backend>(backend: &mut B, uuid: &str) -> Result<(bool, bool), String> {
    backend.read_profile(uuid)
}

fn apply_profile<B: Backend>(
    backend: &mut B,
    target: &Target<B::Device>,
    ipv4: bool,
    ipv6: bool,
) -> Result<(), String> {
    let (uuid, current_ipv4, current_ipv6) = backend.read_active(&target.device_dbus_path)?;
    if uuid != target.connection_uuid {
        return Err("The active NetworkManager profile changed before application.".to_owned());
    }
    let original = (
        target.applied_ipv4_never_default,
        target.applied_ipv6_never_default,
    );
    if (current_ipv4, current_ipv6) != original && (current_ipv4, current_ipv6) != (ipv4, ipv6) {
        return Err(
            "The active never-default values changed concurrently; their newer values were preserved."
                .to_owned(),
        );
    }
    if (current_ipv4, current_ipv6) == (ipv4, ipv6) {
        return if backend.verify_device(&target.device)? {
            Ok(())
        } else {
            Err("The RNDIS device changed while NetworkManager applied its profile.".to_owned())
        };
    }
    apply_profile_from(backend, target, (current_ipv4, current_ipv6), (ipv4, ipv6))
}

fn apply_profile_from<B: Backend>(
    backend: &mut B,
    target: &Target<B::Device>,
    expected: (bool, bool),
    desired: (bool, bool),
) -> Result<(), String> {
    backend.apply_active(
        &target.device_dbus_path,
        &target.connection_uuid,
        expected.0,
        expected.1,
        desired.0,
        desired.1,
    )?;
    if backend.verify_device(&target.device)? {
        Ok(())
    } else {
        Err("The RNDIS device changed while NetworkManager applied its profile.".to_owned())
    }
}

fn rollback_profile<B: Backend>(
    backend: &mut B,
    target: &Target<B::Device>,
    owned: (bool, bool),
) -> Result<(), String> {
    let original_persistent = (
        target.profile_ipv4_never_default,
        target.profile_ipv6_never_default,
    );
    let persistent = match read_profile(backend, &target.connection_uuid) {
        Ok(current) if current == original_persistent => Ok(()),
        Ok(current) if current == owned => modify_profile(
            backend,
            &target.connection_uuid,
            owned,
            original_persistent,
        )
        .and_then(|()| match read_profile(backend, &target.connection_uuid) {
            Ok(current) if current == original_persistent => Ok(()),
            Ok(_) => Err(
                "the persistent values changed again after rollback; the latest values were preserved"
                    .to_owned(),
            ),
            Err(error) => Err(format!(
                "could not verify the persistent rollback: {error}"
            )),
        }),
        Ok(_) => Err(
            "the never-default values changed concurrently; the newer persistent values were preserved"
                .to_owned(),
        ),
        Err(error) => Err(format!("could not re-read the persistent profile: {error}")),
    };

    let original_active = (
        target.applied_ipv4_never_default,
        target.applied_ipv6_never_default,
    );
    let active = match backend.verify_device(&target.device) {
        Ok(false) => Ok(()),
        Err(error) => Err(error),
        Ok(true) => match backend.read_active(&target.device_dbus_path) {
            Ok((uuid, _, _)) if uuid != target.connection_uuid => Err(
                "the active profile changed concurrently; its newer values were preserved"
                    .to_owned(),
            ),
            Ok((_, ipv4, ipv6)) if (ipv4, ipv6) == original_active => Ok(()),
            Ok((_, ipv4, ipv6)) if (ipv4, ipv6) == owned => {
                apply_profile_from(backend, target, owned, original_active)
            }
            Ok(_) => Err(
                "the active never-default values changed concurrently; their newer values were preserved"
                    .to_owned(),
            ),
            Err(error) => Err(format!("could not re-read the active profile: {error}")),
        },
    };

    let routes = match backend.verify_device(&target.device) {
        Ok(false) => Ok(()),
        Err(error) => Err(error),
        Ok(true) => backend.default_routes_present(&target.device).and_then(|current| {
            let original = (
                target.ipv4_default_route_present,
                target.ipv6_default_route_present,
            );
            if current == original {
                Ok(())
            } else {
                Err(
                    "kernel default routes changed during the operation; no unowned route was added or removed"
                        .to_owned(),
                )
            }
        }),
    };

    let mut errors = Vec::new();
    if let Err(error) = persistent {
        errors.push(format!("persistent profile: {error}"));
    }
    if let Err(error) = active {
        errors.push(format!("active profile: {error}"));
    }
    if let Err(error) = routes {
        errors.push(format!("route verification: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn confirm_original_or_rollback<B: Backend>(
    backend: &mut B,
    target: &Target<B::Device>,
    owned: (bool, bool),
) -> Result<(), String> {
    match resolve_target(backend) {
        Ok(Resolution::Target(current))
            if same_selection(&current, target) && same_values(&current, target) =>
        {
            Ok(())
        }
        _ => rollback_profile(backend, target, owned),
    }
}

fn same_selection<D: Eq>(left: &Target<D>, right: &Target<D>) -> bool {
    left.device == right.device
        && left.connection_uuid == right.connection_uuid
        && left.device_dbus_path == right.device_dbus_path
}

fn same_values<D, E>(left: &Target<D>, right: &Target<E>) -> bool {
    left.profile_ipv4_never_default == right.profile_ipv4_never_default
        && left.profile_ipv6_never_default == right.profile_ipv6_never_default
        && left.applied_ipv4_never_default == right.applied_ipv4_never_default
        && left.applied_ipv6_never_default == right.applied_ipv6_never_default
}

fn with_rollback(error: &str, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => format!(
            "{error} The original persistent and active profile settings were restored or verified unchanged."
        ),
        Err(rollback_error) => format!(
            "{error} Automatic profile rollback was incomplete: {rollback_error}. Open NetworkManager settings before continuing."
        ),
    }
}

type ConnectionSettings = HashMap<String, PropMap>;

fn read_persistent_connection(connection_uuid: &str) -> Result<(bool, bool), String> {
    let connection = Connection::new_system()
        .map_err(|error| format!("Could not connect to NetworkManager: {error}"))?;
    let connection_path = settings_connection_path(&connection, connection_uuid)?;
    let proxy = connection.with_proxy(
        NETWORK_MANAGER_DESTINATION,
        connection_path,
        DBUS_READ_TIMEOUT,
    );
    let (settings,): (ConnectionSettings,) = proxy
        .method_call(
            NETWORK_MANAGER_SETTINGS_CONNECTION_INTERFACE,
            "GetSettings",
            (),
        )
        .map_err(|error| {
            format!("Could not read the persistent NetworkManager profile: {error}")
        })?;
    let (uuid, ipv4, ipv6) = persistent_state(&settings)?;
    if uuid != connection_uuid {
        return Err(
            "NetworkManager returned a different persistent profile UUID than requested."
                .to_owned(),
        );
    }
    Ok((ipv4, ipv6))
}

fn update_persistent_connection(
    connection_uuid: &str,
    expected_ipv4: bool,
    expected_ipv6: bool,
    ipv4: bool,
    ipv6: bool,
) -> Result<(), String> {
    let connection = Connection::new_system()
        .map_err(|error| format!("Could not connect to NetworkManager: {error}"))?;
    let connection_path = settings_connection_path(&connection, connection_uuid)?;
    let proxy = connection.with_proxy(
        NETWORK_MANAGER_DESTINATION,
        connection_path,
        DBUS_MUTATION_TIMEOUT,
    );

    // Read the version before the settings. D-Bus preserves call order, so a
    // profile edit before or during GetSettings makes Update2 reject our stale
    // token rather than letting a full-profile write erase that edit.
    let version_id: u64 = proxy
        .get(NETWORK_MANAGER_SETTINGS_CONNECTION_INTERFACE, "VersionId")
        .map_err(|error| format!("Could not read the persistent profile version: {error}"))?;
    if version_id == 0 {
        return Err(
            "NetworkManager returned an unusable persistent profile version guard.".to_owned(),
        );
    }
    let (mut settings,): (ConnectionSettings,) = proxy
        .method_call(
            NETWORK_MANAGER_SETTINGS_CONNECTION_INTERFACE,
            "GetSettings",
            (),
        )
        .map_err(|error| {
            format!("Could not read the persistent NetworkManager profile: {error}")
        })?;
    let (reported_uuid, current_ipv4, current_ipv6) = persistent_state(&settings)?;
    if reported_uuid != connection_uuid {
        return Err("The persistent NetworkManager profile UUID changed before update.".to_owned());
    }
    if (current_ipv4, current_ipv6) != (expected_ipv4, expected_ipv6) {
        return Err(
            "The persistent never-default values changed concurrently; refusing to overwrite them."
                .to_owned(),
        );
    }
    set_connection_boolean(&mut settings, "ipv4", "never-default", ipv4);
    set_connection_boolean(&mut settings, "ipv6", "never-default", ipv6);

    let update_args = PropMap::from([(
        "version-id".to_owned(),
        Variant(Box::new(version_id) as Box<dyn RefArg>),
    )]);
    let updated: Result<(PropMap,), dbus::Error> = proxy.method_call(
        NETWORK_MANAGER_SETTINGS_CONNECTION_INTERFACE,
        "Update2",
        (settings, UPDATE_TO_DISK, update_args),
    );
    updated.map_err(|error| {
        format!(
            "NetworkManager rejected the version-guarded persistent profile update (version 1.44 or newer is required): {error}"
        )
    })?;

    let verified_version: u64 = proxy
        .get(NETWORK_MANAGER_SETTINGS_CONNECTION_INTERFACE, "VersionId")
        .map_err(|error| format!("Could not verify the persistent profile version: {error}"))?;
    if verified_version == 0 || verified_version == version_id {
        return Err(
            "NetworkManager did not advance the persistent profile version after update."
                .to_owned(),
        );
    }
    let (verified,): (ConnectionSettings,) = proxy
        .method_call(
            NETWORK_MANAGER_SETTINGS_CONNECTION_INTERFACE,
            "GetSettings",
            (),
        )
        .map_err(|error| {
            format!("Could not verify the persistent NetworkManager profile: {error}")
        })?;
    let (verified_uuid, verified_ipv4, verified_ipv6) = persistent_state(&verified)?;
    if verified_uuid != connection_uuid || verified_ipv4 != ipv4 || verified_ipv6 != ipv6 {
        return Err(
            "NetworkManager did not persist both never-default settings on the selected profile."
                .to_owned(),
        );
    }
    Ok(())
}

fn settings_connection_path(
    connection: &Connection,
    connection_uuid: &str,
) -> Result<String, String> {
    if !valid_uuid(connection_uuid) {
        return Err("The requested NetworkManager profile UUID is invalid.".to_owned());
    }
    let proxy = connection.with_proxy(
        NETWORK_MANAGER_DESTINATION,
        NETWORK_MANAGER_SETTINGS_PATH,
        DBUS_READ_TIMEOUT,
    );
    let (path,): (dbus::Path<'static>,) = proxy
        .method_call(
            NETWORK_MANAGER_SETTINGS_INTERFACE,
            "GetConnectionByUuid",
            (connection_uuid,),
        )
        .map_err(|error| format!("Could not resolve the NetworkManager profile UUID: {error}"))?;
    let path = path.to_string();
    if valid_settings_connection_dbus_path(&path) {
        Ok(path)
    } else {
        Err("NetworkManager returned an invalid settings-profile D-Bus path.".to_owned())
    }
}

fn read_active_connection(device_dbus_path: &str) -> Result<(String, bool, bool), String> {
    if !valid_device_dbus_path(device_dbus_path) {
        return Err("NetworkManager returned an invalid device D-Bus path.".to_owned());
    }
    let connection = Connection::new_system()
        .map_err(|error| format!("Could not connect to NetworkManager: {error}"))?;
    let proxy = connection.with_proxy(
        NETWORK_MANAGER_DESTINATION,
        device_dbus_path,
        DBUS_READ_TIMEOUT,
    );
    let (settings, _): (ConnectionSettings, u64) = proxy
        .method_call(
            NETWORK_MANAGER_DEVICE_INTERFACE,
            "GetAppliedConnection",
            (0_u32,),
        )
        .map_err(|error| format!("Could not read the active NetworkManager profile: {error}"))?;
    applied_state(&settings)
}

fn apply_active_connection(
    device_dbus_path: &str,
    connection_uuid: &str,
    expected_ipv4: bool,
    expected_ipv6: bool,
    ipv4: bool,
    ipv6: bool,
) -> Result<(), String> {
    if !valid_device_dbus_path(device_dbus_path) {
        return Err("NetworkManager returned an invalid device D-Bus path.".to_owned());
    }
    let connection = Connection::new_system()
        .map_err(|error| format!("Could not connect to NetworkManager: {error}"))?;
    let proxy = connection.with_proxy(
        NETWORK_MANAGER_DESTINATION,
        device_dbus_path,
        DBUS_MUTATION_TIMEOUT,
    );
    let (mut settings, version_id): (ConnectionSettings, u64) = proxy
        .method_call(
            NETWORK_MANAGER_DEVICE_INTERFACE,
            "GetAppliedConnection",
            (0_u32,),
        )
        .map_err(|error| format!("Could not read the active NetworkManager profile: {error}"))?;
    verify_applied_uuid(&settings, connection_uuid)?;
    let (_, current_ipv4, current_ipv6) = applied_state(&settings)?;
    if (current_ipv4, current_ipv6) != (expected_ipv4, expected_ipv6) {
        return Err(
            "The active never-default values changed concurrently; refusing to overwrite them."
                .to_owned(),
        );
    }
    set_connection_boolean(&mut settings, "ipv4", "never-default", ipv4);
    set_connection_boolean(&mut settings, "ipv6", "never-default", ipv6);

    let reapplied: Result<(), dbus::Error> = proxy.method_call(
        NETWORK_MANAGER_DEVICE_INTERFACE,
        "Reapply",
        (settings, version_id, PRESERVE_EXTERNAL_IP),
    );
    reapplied.map_err(|error| {
        format!(
            "NetworkManager could not safely apply the profile while preserving external addresses and routes (version 1.44 or newer is required): {error}"
        )
    })?;

    let (verified, _): (ConnectionSettings, u64) = proxy
        .method_call(
            NETWORK_MANAGER_DEVICE_INTERFACE,
            "GetAppliedConnection",
            (0_u32,),
        )
        .map_err(|error| format!("Could not verify the active NetworkManager profile: {error}"))?;
    verify_applied_uuid(&verified, connection_uuid)?;
    let (verified_uuid, verified_ipv4, verified_ipv6) = applied_state(&verified)?;
    if verified_uuid != connection_uuid || verified_ipv4 != ipv4 || verified_ipv6 != ipv6 {
        return Err(
            "NetworkManager did not apply both never-default settings to the active profile."
                .to_owned(),
        );
    }
    Ok(())
}

fn applied_state(settings: &ConnectionSettings) -> Result<(String, bool, bool), String> {
    let uuid = settings
        .get("connection")
        .and_then(|section| dbus::arg::prop_cast::<String>(section, "uuid"))
        .filter(|value| valid_uuid(value))
        .cloned()
        .ok_or_else(|| "The active NetworkManager profile has an invalid UUID.".to_owned())?;
    Ok((
        uuid,
        applied_boolean(settings, "ipv4", "never-default").unwrap_or(false),
        applied_boolean(settings, "ipv6", "never-default").unwrap_or(false),
    ))
}

fn persistent_state(settings: &ConnectionSettings) -> Result<(String, bool, bool), String> {
    let uuid = settings
        .get("connection")
        .and_then(|section| dbus::arg::prop_cast::<String>(section, "uuid"))
        .filter(|value| valid_uuid(value))
        .cloned()
        .ok_or_else(|| "The persistent NetworkManager profile has an invalid UUID.".to_owned())?;
    Ok((
        uuid,
        applied_boolean(settings, "ipv4", "never-default").unwrap_or(false),
        applied_boolean(settings, "ipv6", "never-default").unwrap_or(false),
    ))
}

fn verify_applied_uuid(settings: &ConnectionSettings, expected: &str) -> Result<(), String> {
    let actual = settings
        .get("connection")
        .and_then(|section| dbus::arg::prop_cast::<String>(section, "uuid"));
    if actual.is_some_and(|value| value == expected) {
        Ok(())
    } else {
        Err("The active NetworkManager profile UUID changed before application.".to_owned())
    }
}

fn set_connection_boolean(
    settings: &mut ConnectionSettings,
    section: &str,
    property: &str,
    value: bool,
) {
    settings.entry(section.to_owned()).or_default().insert(
        property.to_owned(),
        Variant(Box::new(value) as Box<dyn RefArg>),
    );
}

fn applied_boolean(settings: &ConnectionSettings, section: &str, property: &str) -> Option<bool> {
    settings
        .get(section)
        .and_then(|values| dbus::arg::prop_cast::<bool>(values, property))
        .copied()
}

fn parse_active_device(output: &str) -> Result<Option<(String, String)>, String> {
    let lines = normalized_lines(output);
    if lines.len() != 3 {
        return Err("NetworkManager returned an unexpected device response.".to_owned());
    }
    if lines[0].split_whitespace().next() != Some("100") {
        return Ok(None);
    }
    if !valid_uuid(lines[1]) {
        return Err("The active RNDIS connection has an invalid NetworkManager UUID.".to_owned());
    }
    if !valid_device_dbus_path(lines[2]) {
        return Err(
            "The active RNDIS connection has an invalid NetworkManager D-Bus path.".to_owned(),
        );
    }
    Ok(Some((lines[1].to_owned(), lines[2].to_owned())))
}

fn parse_network_manager_version(output: &str) -> Result<(u32, u32, u32), String> {
    let value = output.trim();
    let token = value
        .split_whitespace()
        .next()
        .ok_or_else(|| "NetworkManager returned an empty version.".to_owned())?;
    let mut components = token.split('.');
    let major = numeric_prefix(components.next())?;
    let minor = numeric_prefix(components.next())?;
    let patch = numeric_prefix(components.next()).unwrap_or(0);
    Ok((major, minor, patch))
}

fn numeric_prefix(value: Option<&str>) -> Result<u32, String> {
    let digits = value
        .unwrap_or_default()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        return Err("NetworkManager returned an invalid version number.".to_owned());
    }
    digits
        .parse()
        .map_err(|_| "NetworkManager returned an invalid version number.".to_owned())
}

fn read_daemon_version() -> Result<String, String> {
    let connection = Connection::new_system()
        .map_err(|error| format!("Could not connect to NetworkManager: {error}"))?;
    let proxy = connection.with_proxy(
        NETWORK_MANAGER_DESTINATION,
        NETWORK_MANAGER_PATH,
        DBUS_READ_TIMEOUT,
    );
    proxy
        .get(NETWORK_MANAGER_DESTINATION, "Version")
        .map_err(|error| format!("Could not read the NetworkManager daemon version: {error}"))
}

fn normalized_lines(output: &str) -> Vec<&str> {
    output
        .trim_end_matches(['\r', '\n'])
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect()
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn valid_interface_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 15
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_device_dbus_path(value: &str) -> bool {
    value
        .strip_prefix("/org/freedesktop/NetworkManager/Devices/")
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn valid_settings_connection_dbus_path(value: &str) -> bool {
    value
        .strip_prefix("/org/freedesktop/NetworkManager/Settings/")
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn fixed_args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn concise_output(bytes: &[u8]) -> String {
    concise_text(&String::from_utf8_lossy(bytes))
}

fn concise_text(value: &str) -> String {
    value
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
        .collect()
}

fn trusted_nmcli_path() -> Result<Option<PathBuf>, String> {
    for candidate in NMCLI_CANDIDATES.map(Path::new) {
        if !candidate.exists() {
            continue;
        }
        let canonical = fs::canonicalize(candidate)
            .map_err(|error| format!("Could not resolve {}: {error}", candidate.display()))?;
        if !trusted_executable_and_parents(&canonical)? {
            return Err(format!(
                "Refusing untrusted nmcli executable at {}.",
                canonical.display()
            ));
        }
        return Ok(Some(canonical));
    }
    Ok(None)
}

fn trusted_executable_and_parents(path: &Path) -> Result<bool, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Ok(false);
    }
    for parent in path.ancestors().skip(1) {
        let metadata = fs::metadata(parent)
            .map_err(|error| format!("Could not inspect {}: {error}", parent.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const UUID_A: &str = "11111111-2222-3333-4444-555555555555";
    const UUID_B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    type PolicyPair = (bool, bool);
    type ProfileUpdate = (String, PolicyPair, PolicyPair);

    #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct FakeDevice(String);

    struct FakeBackend {
        devices: Vec<FakeDevice>,
        active: BTreeMap<String, (String, String)>,
        profiles: BTreeMap<String, (bool, bool)>,
        applied: BTreeMap<String, (bool, bool)>,
        profile_updates: Vec<ProfileUpdate>,
        apply_calls: Vec<(String, String, bool, bool)>,
        commands: Vec<Vec<String>>,
        identity_valid: bool,
        fail_apply: bool,
        fail_modify_after_update: bool,
        default_routes: (bool, bool),
        daemon_version: String,
        concurrent_profile_before_update: Option<(bool, bool)>,
        concurrent_profile_before_rollback_update_on_apply_failure: Option<(bool, bool)>,
        concurrent_profile_on_apply_failure: Option<(bool, bool)>,
        concurrent_applied_on_apply_failure: Option<(bool, bool)>,
    }

    impl FakeBackend {
        fn one(ipv4: bool, ipv6: bool) -> Self {
            Self {
                devices: vec![FakeDevice("usb0".to_owned())],
                active: BTreeMap::from([(
                    "usb0".to_owned(),
                    (
                        UUID_A.to_owned(),
                        "/org/freedesktop/NetworkManager/Devices/7".to_owned(),
                    ),
                )]),
                profiles: BTreeMap::from([(UUID_A.to_owned(), (ipv4, ipv6))]),
                applied: BTreeMap::from([(UUID_A.to_owned(), (ipv4, ipv6))]),
                profile_updates: Vec::new(),
                apply_calls: Vec::new(),
                commands: Vec::new(),
                identity_valid: true,
                fail_apply: false,
                fail_modify_after_update: false,
                default_routes: (false, false),
                daemon_version: "1.58.0".to_owned(),
                concurrent_profile_before_update: None,
                concurrent_profile_before_rollback_update_on_apply_failure: None,
                concurrent_profile_on_apply_failure: None,
                concurrent_applied_on_apply_failure: None,
            }
        }
    }

    impl Backend for FakeBackend {
        type Device = FakeDevice;

        fn devices(&mut self) -> Result<Vec<Self::Device>, String> {
            Ok(self.devices.clone())
        }

        fn device_name(&self, device: &Self::Device) -> Result<String, String> {
            Ok(device.0.clone())
        }

        fn verify_device(&mut self, _device: &Self::Device) -> Result<bool, String> {
            Ok(self.identity_valid)
        }

        fn default_routes_present(
            &mut self,
            _device: &Self::Device,
        ) -> Result<(bool, bool), String> {
            Ok(self.default_routes)
        }

        fn daemon_version(&mut self) -> Result<String, String> {
            Ok(self.daemon_version.clone())
        }

        fn nmcli(&mut self, args: &[String]) -> Result<String, String> {
            self.commands.push(args.to_vec());
            if args.last().is_some_and(|value| value == "general") {
                return Ok("running\n".to_owned());
            }
            if args.iter().any(|value| value == "device")
                && args.iter().any(|value| value == "show")
            {
                let interface = args.last().expect("device name");
                return Ok(match self.active.get(interface) {
                    Some((uuid, path)) => format!("100 (connected)\n{uuid}\n{path}\n"),
                    None => "30 (disconnected)\n--\n--\n".to_owned(),
                });
            }
            panic!("unexpected nmcli command: {args:?}");
        }

        fn read_profile(&mut self, connection_uuid: &str) -> Result<(bool, bool), String> {
            self.profiles
                .get(connection_uuid)
                .copied()
                .ok_or_else(|| "unknown fake profile UUID".to_owned())
        }

        fn update_profile(
            &mut self,
            connection_uuid: &str,
            expected_ipv4: bool,
            expected_ipv6: bool,
            ipv4: bool,
            ipv6: bool,
        ) -> Result<(), String> {
            if let Some(values) = self.concurrent_profile_before_update.take() {
                self.profiles.insert(connection_uuid.to_owned(), values);
            }
            let expected = (expected_ipv4, expected_ipv6);
            if self.profiles.get(connection_uuid).copied() != Some(expected) {
                return Err("fake persistent profile changed concurrently".to_owned());
            }
            let desired = (ipv4, ipv6);
            self.profile_updates
                .push((connection_uuid.to_owned(), expected, desired));
            self.profiles.insert(connection_uuid.to_owned(), desired);
            if self.fail_modify_after_update {
                self.fail_modify_after_update = false;
                return Err("simulated uncertain modify failure".to_owned());
            }
            Ok(())
        }

        fn read_active(&mut self, device_dbus_path: &str) -> Result<(String, bool, bool), String> {
            let uuid = self
                .active
                .values()
                .find(|(_, path)| path == device_dbus_path)
                .map(|(uuid, _)| uuid.clone())
                .ok_or_else(|| "unknown fake D-Bus path".to_owned())?;
            let (ipv4, ipv6) = self.applied[&uuid];
            Ok((uuid, ipv4, ipv6))
        }

        fn apply_active(
            &mut self,
            device_dbus_path: &str,
            connection_uuid: &str,
            expected_ipv4: bool,
            expected_ipv6: bool,
            ipv4: bool,
            ipv6: bool,
        ) -> Result<(), String> {
            if !self
                .active
                .values()
                .any(|(uuid, path)| uuid == connection_uuid && path == device_dbus_path)
            {
                return Err("fake active profile identity mismatch".to_owned());
            }
            if self.applied[connection_uuid] != (expected_ipv4, expected_ipv6) {
                return Err("fake active profile changed concurrently".to_owned());
            }
            self.apply_calls.push((
                device_dbus_path.to_owned(),
                connection_uuid.to_owned(),
                ipv4,
                ipv6,
            ));
            if self.fail_apply {
                self.concurrent_profile_before_update = self
                    .concurrent_profile_before_rollback_update_on_apply_failure
                    .take();
                if let Some(values) = self.concurrent_profile_on_apply_failure.take() {
                    self.profiles.insert(connection_uuid.to_owned(), values);
                }
                if let Some(values) = self.concurrent_applied_on_apply_failure.take() {
                    self.applied.insert(connection_uuid.to_owned(), values);
                }
                Err("simulated apply failure".to_owned())
            } else {
                self.applied
                    .insert(connection_uuid.to_owned(), (ipv4, ipv6));
                Ok(())
            }
        }
    }

    #[test]
    fn mixed_profile_is_visible_but_not_reported_as_enabled() {
        let mut backend = FakeBackend::one(true, false);
        let status = status_with_backend(&mut backend).unwrap();
        assert!(status.available);
        assert!(!status.enabled);
        assert!(!status.configured);
        assert!(status.mixed);
    }

    #[test]
    fn persistent_setting_without_applied_setting_is_not_enabled() {
        let mut backend = FakeBackend::one(true, true);
        backend.applied.insert(UUID_A.to_owned(), (false, false));
        let status = status_with_backend(&mut backend).unwrap();
        assert!(status.available);
        assert!(!status.enabled);
        assert!(!status.configured);
        assert!(status.mixed);
    }

    #[test]
    fn enabled_settings_with_a_default_route_are_not_reported_as_safe() {
        let mut backend = FakeBackend::one(true, true);
        backend.default_routes = (false, true);
        let status = status_with_backend(&mut backend).unwrap();
        assert!(status.available);
        assert!(!status.enabled);
        assert!(status.configured);
        assert!(status.mixed);
        assert!(status.message.contains("IPv6 default route"));
    }

    #[test]
    fn older_daemon_is_unavailable_even_when_the_nmcli_client_runs() {
        let mut backend = FakeBackend::one(false, false);
        backend.daemon_version = "1.42.8".to_owned();
        let status = status_with_backend(&mut backend).unwrap();
        assert!(!status.available);
        assert!(status.message.contains("1.44 or newer"));
    }

    #[test]
    fn refuses_to_choose_between_two_active_rndis_devices() {
        let mut backend = FakeBackend::one(false, false);
        backend.devices.push(FakeDevice("usb1".to_owned()));
        backend.active.insert(
            "usb1".to_owned(),
            (
                UUID_B.to_owned(),
                "/org/freedesktop/NetworkManager/Devices/8".to_owned(),
            ),
        );
        backend.profiles.insert(UUID_B.to_owned(), (false, false));
        backend.applied.insert(UUID_B.to_owned(), (false, false));
        let status = status_with_backend(&mut backend).unwrap();
        assert!(!status.available);
        assert!(status.message.contains("More than one"));
    }

    #[test]
    fn updates_profile_and_applied_state_for_the_exact_device() {
        let mut backend = FakeBackend::one(false, false);
        let status = set_with_backend(&mut backend, true).unwrap();
        assert!(status.enabled);
        assert!(status.configured);
        assert_eq!(backend.profiles[UUID_A], (true, true));
        assert_eq!(backend.applied[UUID_A], (true, true));
        assert!(backend
            .profile_updates
            .iter()
            .any(|update| { update == &(UUID_A.to_owned(), (false, false), (true, true),) }));
        assert!(backend.apply_calls.iter().any(|call| {
            call == &(
                "/org/freedesktop/NetworkManager/Devices/7".to_owned(),
                UUID_A.to_owned(),
                true,
                true,
            )
        }));
    }

    #[test]
    fn remaining_default_route_keeps_a_pending_profile_but_is_not_enabled() {
        let mut backend = FakeBackend::one(false, false);
        backend.default_routes = (true, false);
        let status = set_with_backend(&mut backend, true).unwrap();
        assert!(!status.enabled);
        assert!(status.configured);
        assert!(status.mixed);
        assert!(status.message.contains("not safe to start"));
        assert_eq!(backend.profiles[UUID_A], (true, true));
        assert_eq!(backend.applied[UUID_A], (true, true));
    }

    #[test]
    fn apply_refuses_active_values_changed_since_target_resolution() {
        let mut backend = FakeBackend::one(false, false);
        let target = match resolve_target(&mut backend).unwrap() {
            Resolution::Target(target) => target,
            Resolution::Unavailable(message) => panic!("unexpected unavailable status: {message}"),
        };
        backend.applied.insert(UUID_A.to_owned(), (false, true));
        let error = apply_profile(&mut backend, &target, true, true).unwrap_err();
        assert!(error.contains("changed concurrently"));
        assert_eq!(backend.applied[UUID_A], (false, true));
    }

    #[test]
    fn failed_activation_restores_the_original_profile_values() {
        let mut backend = FakeBackend::one(false, true);
        backend.fail_apply = true;
        let error = set_with_backend(&mut backend, true).unwrap_err();
        assert!(error.contains("restored or verified unchanged"));
        assert_eq!(backend.profiles[UUID_A], (false, true));
    }

    #[test]
    fn rollback_preserves_concurrent_persistent_and_active_changes() {
        let mut backend = FakeBackend::one(false, false);
        backend.fail_apply = true;
        backend.concurrent_profile_on_apply_failure = Some((false, true));
        backend.concurrent_applied_on_apply_failure = Some((true, false));
        let error = set_with_backend(&mut backend, true).unwrap_err();
        assert!(error.contains("newer persistent values were preserved"));
        assert!(error.contains("newer values were preserved"));
        assert_eq!(backend.profiles[UUID_A], (false, true));
        assert_eq!(backend.applied[UUID_A], (true, false));
    }

    #[test]
    fn rollback_version_guard_preserves_a_change_after_its_read() {
        let mut backend = FakeBackend::one(false, false);
        backend.fail_apply = true;
        backend.concurrent_profile_before_rollback_update_on_apply_failure = Some((false, true));
        let error = set_with_backend(&mut backend, true).unwrap_err();
        assert!(error.contains("Automatic profile rollback was incomplete"));
        assert!(error.contains("changed concurrently"));
        assert_eq!(backend.profiles[UUID_A], (false, true));
    }

    #[test]
    fn persistent_update_guard_preserves_a_change_after_initial_read() {
        let mut backend = FakeBackend::one(false, false);
        backend.concurrent_profile_before_update = Some((false, true));
        let error = set_with_backend(&mut backend, true).unwrap_err();
        assert!(error.contains("changed concurrently"));
        assert_eq!(backend.profiles[UUID_A], (false, true));
        assert!(backend.profile_updates.is_empty());
    }

    #[test]
    fn uncertain_profile_update_failure_restores_original_values() {
        let mut backend = FakeBackend::one(false, true);
        backend.fail_modify_after_update = true;
        let error = set_with_backend(&mut backend, true).unwrap_err();
        assert!(error.contains("restored or verified unchanged"));
        assert_eq!(backend.profiles[UUID_A], (false, true));
        assert_eq!(backend.applied[UUID_A], (false, true));
    }

    #[test]
    fn rejects_non_uuid_output_before_using_it_as_an_argument() {
        let mut backend = FakeBackend::one(false, false);
        backend.active.insert(
            "usb0".to_owned(),
            (
                "--help".to_owned(),
                "/org/freedesktop/NetworkManager/Devices/7".to_owned(),
            ),
        );
        let error = status_with_backend(&mut backend).unwrap_err();
        assert!(error.contains("invalid NetworkManager UUID"));
    }

    #[test]
    fn applied_update_changes_only_the_requested_properties() {
        let mut settings = ConnectionSettings::from([
            (
                "connection".to_owned(),
                PropMap::from([(
                    "uuid".to_owned(),
                    Variant(Box::new(UUID_A.to_owned()) as Box<dyn RefArg>),
                )]),
            ),
            (
                "ipv4".to_owned(),
                PropMap::from([(
                    "route-metric".to_owned(),
                    Variant(Box::new(600_i64) as Box<dyn RefArg>),
                )]),
            ),
        ]);
        set_connection_boolean(&mut settings, "ipv4", "never-default", true);
        set_connection_boolean(&mut settings, "ipv6", "never-default", true);
        assert_eq!(
            applied_boolean(&settings, "ipv4", "never-default"),
            Some(true)
        );
        assert_eq!(
            applied_boolean(&settings, "ipv6", "never-default"),
            Some(true)
        );
        assert_eq!(
            settings["ipv4"]
                .get("route-metric")
                .and_then(|value| dbus::arg::cast::<i64>(&value.0))
                .copied(),
            Some(600)
        );
    }

    #[test]
    fn validates_only_kernel_style_names_and_device_paths() {
        assert!(valid_interface_name("enp0s20f0u1"));
        assert!(!valid_interface_name("--help"));
        assert!(!valid_interface_name("usb 0"));
        assert!(valid_device_dbus_path(
            "/org/freedesktop/NetworkManager/Devices/42"
        ));
        assert!(!valid_device_dbus_path(
            "/org/freedesktop/NetworkManager/Devices/42/../../Settings"
        ));
        assert!(valid_settings_connection_dbus_path(
            "/org/freedesktop/NetworkManager/Settings/9"
        ));
        assert!(!valid_settings_connection_dbus_path(
            "/org/freedesktop/NetworkManager/Settings/9/../../Devices/1"
        ));
    }

    #[test]
    fn parses_network_manager_daemon_versions_with_distribution_suffixes() {
        assert_eq!(
            parse_network_manager_version("1.58.2\n").unwrap(),
            (1, 58, 2)
        );
        assert_eq!(
            parse_network_manager_version("1.46.0-ubuntu1\n").unwrap(),
            (1, 46, 0)
        );
        assert!(parse_network_manager_version("NetworkManager 1.58").is_err());
    }
}
