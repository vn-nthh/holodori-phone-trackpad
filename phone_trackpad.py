"""
Holodori Phone Trackpad — community support script for hololive Dreams (holodori).

Use an Android phone as a multi-touch rhythm controller for the PC (Steam)
client. The default Android Open Accessory transport needs no USB debugging,
maps a native phone play zone to keys, and mirrors touches to a PC overlay.
The original raw ADB getevent transport remains available as a fallback.

Unofficial fan tool. Not affiliated with COVER Corp., hololive production,
or QualiArts.

Supports multi-touch (up to 10 fingers), drag notes (seamless key
transitions when dragging across zones), and a phone-side controller
UI with resizable/rotatable play zone.

Requirements:
  - Windows + Python 3.9+
  - Companion Android app
  - libusb-package (see requirements.txt)
  - AOA-capable Android phone connected over USB

Usage:
  python phone_trackpad.py              # AOA input, no PC overlay
  python phone_trackpad.py --overlay    # AOA input + PC touch overlay
  python phone_trackpad.py --transport adb  # Legacy raw ADB mode
  python phone_trackpad.py --test       # Test mode: print events without keys
  python phone_trackpad.py --selftest   # Verify key sending works
  python phone_trackpad.py --keys a s d f j k l   # Custom key layout
  python phone_trackpad.py --no-ui      # Don't launch phone controller UI
"""

import subprocess
import sys
import re
import ctypes
import json
import os
import shutil
import shlex
import time
import math
import threading
import http.server
from dataclasses import dataclass, field
from typing import Dict, Optional, List, Tuple

# ============================================================================
# Configuration
# ============================================================================

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
IS_BUNDLED = bool(getattr(sys, "frozen", False))
RESOURCE_DIR = getattr(sys, "_MEIPASS", SCRIPT_DIR)

if IS_BUNDLED:
    CONFIG_DIR = os.path.join(
        os.environ.get("LOCALAPPDATA", os.path.expanduser("~")),
        "Holodori Phone Trackpad",
    )
else:
    CONFIG_DIR = SCRIPT_DIR

CONFIG_FILE = os.path.join(CONFIG_DIR, "config.json")
CONTROLLER_HTML = os.path.join(RESOURCE_DIR, "controller.html")

# Default Holodori / hololive Dreams 6-key keyboard layout (left → right)
DEFAULT_KEYS = ["s", "d", "f", "j", "k", "l"]
SERVER_PORT = 53281


def resolve_adb_path(explicit: Optional[str] = None) -> str:
    """Find adb: explicit path, PATH, or common Windows install locations."""
    if explicit:
        return explicit
    env = os.environ.get("ADB_PATH")
    if env:
        return env
    which = shutil.which("adb")
    if which:
        return which
    candidates = [
        r"C:\Program Files\Software Fix\adb.exe",
        r"C:\Android\platform-tools\adb.exe",
        r"C:\platform-tools\adb.exe",
        os.path.expandvars(r"%LOCALAPPDATA%\Android\Sdk\platform-tools\adb.exe"),
        os.path.expandvars(r"%USERPROFILE%\AppData\Local\Android\Sdk\platform-tools\adb.exe"),
    ]
    for path in candidates:
        if path and os.path.isfile(path):
            return path
    return "adb"  # last resort: hope PATH works at runtime


ADB_PATH = resolve_adb_path()

# ============================================================================
# Windows keybd_event API
# ============================================================================

KEYEVENTF_EXTENDEDKEY = 0x0001
KEYEVENTF_KEYUP = 0x0002
MAPVK_VK_TO_VSC = 0

user32 = ctypes.windll.user32
MapVirtualKeyW = user32.MapVirtualKeyW
MapVirtualKeyW.argtypes = [ctypes.c_uint, ctypes.c_uint]
MapVirtualKeyW.restype = ctypes.c_uint

# Virtual key code map
VK_MAP = {}
for c in "abcdefghijklmnopqrstuvwxyz":
    VK_MAP[c] = ord(c.upper())
for c in "0123456789":
    VK_MAP[c] = ord(c)
VK_MAP.update({
    'space': 0x20, 'enter': 0x0D, 'esc': 0x1B, 'tab': 0x09,
    'backspace': 0x08,
    'lshift': 0xA0, 'rshift': 0xA1,
    'lctrl': 0xA2, 'rctrl': 0xA3,
    'lalt': 0xA4, 'ralt': 0xA5,
    'up': 0x26, 'down': 0x28, 'left': 0x25, 'right': 0x27,
    'f1': 0x70, 'f2': 0x71, 'f3': 0x72, 'f4': 0x73,
    'f5': 0x74, 'f6': 0x75, 'f7': 0x76, 'f8': 0x77,
    'f9': 0x78, 'f10': 0x79, 'f11': 0x7A, 'f12': 0x7B,
    'semicolon': 0xBA, 'equals': 0xBB, 'comma': 0xBC,
    'minus': 0xBD, 'period': 0xBE, 'slash': 0xBF,
    'backtick': 0xC0, 'lbracket': 0xDB, 'backslash': 0xDC,
    'rbracket': 0xDD, 'quote': 0xDE,
})
EXTENDED_VKS = {0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E, 0x21, 0x22, 0x23, 0x24}


def send_key_event(key_name: str, key_up: bool = False) -> bool:
    """Send a keyboard event via keybd_event."""
    vk = VK_MAP.get(key_name.lower())
    if vk is None:
        return False
    scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)
    flags = 0
    if key_up:
        flags |= KEYEVENTF_KEYUP
    if vk in EXTENDED_VKS:
        flags |= KEYEVENTF_EXTENDEDKEY
    user32.keybd_event(vk, scan, flags, 0)
    return True


def press_key(key_name: str) -> bool:
    return send_key_event(key_name, key_up=False)


def release_key(key_name: str) -> bool:
    return send_key_event(key_name, key_up=True)


def self_test_keys():
    """Type 'hello' into the focused window to verify key sending works."""
    print()
    print("=" * 60)
    print("  SELF-TEST: Key Sending Verification")
    print("=" * 60)
    print()
    print("  You have 3 seconds to click on a text field (e.g. Notepad)...")
    for i in range(3, 0, -1):
        print(f"    {i}...")
        time.sleep(1)

    test_keys = ["h", "e", "l", "l", "o"]
    print(f"\n  Typing: {''.join(test_keys)}")

    for key in test_keys:
        vk = VK_MAP[key]
        scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)
        press_key(key)
        time.sleep(0.02)
        release_key(key)
        time.sleep(0.03)
        print(f"    Key '{key}': VK=0x{vk:02X} Scan=0x{scan:02X} -> sent")

    print("\n  If text appeared in the focused window, it's working!")
    print("  If not, try running as Administrator.")


# ============================================================================
# Shared Configuration (thread-safe, shared between HTTP server and processor)
# ============================================================================

class SharedConfig:
    """Thread-safe configuration shared between the HTTP server and TouchProcessor."""

    def __init__(self, keys: List[str]):
        self._lock = threading.Lock()
        self.keys = list(keys)
        self.playzone = {'x': 0.10, 'y': 0.75, 'w': 0.80, 'h': 0.15, 'r': 0}
        self.locked = False
        self.hw_zones = []  # Pre-computed zone hitboxes in hardware coords
        self._version = 0   # Bumped on every update so consumers can cache

    def update_from_phone(self, data: dict):
        """Called by the HTTP server when the phone sends a config update."""
        with self._lock:
            if 'playzone' in data:
                self.playzone.update(data['playzone'])
            if 'locked' in data:
                self.locked = data['locked']
            if 'hw_zones' in data:
                self.hw_zones = data['hw_zones']
            self._version += 1

    @property
    def version(self) -> int:
        """Monotonic config version; int read is atomic, safe to poll lock-free."""
        return self._version

    def snapshot_runtime(self) -> Tuple[int, bool, list]:
        """(version, locked, hw_zones) consistently read under the lock."""
        with self._lock:
            return self._version, self.locked, list(self.hw_zones)

    def get_dict(self) -> dict:
        """Get a snapshot of the current config."""
        with self._lock:
            return {
                'keys': list(self.keys),
                'playzone': dict(self.playzone),
                'locked': self.locked,
            }

    def get_playzone(self) -> dict:
        with self._lock:
            return dict(self.playzone)

    def get_hw_zones(self) -> list:
        with self._lock:
            return list(self.hw_zones)

    def is_locked(self) -> bool:
        with self._lock:
            return self.locked


# ============================================================================
# Touch Event Parser (ADB getevent) with play zone support
# ============================================================================

EV_SYN = 0x0000
EV_ABS = 0x0003
ABS_MT_SLOT = 0x002F
ABS_MT_POSITION_X = 0x0035
ABS_MT_POSITION_Y = 0x0036
ABS_MT_TRACKING_ID = 0x0039
SYN_REPORT = 0x0000


@dataclass
class SlotState:
    tracking_id: int = -1
    x: int = 0
    y: int = 0
    changed: bool = False


class TouchProcessor:
    """Processes raw getevent data, maps touches to play zone columns, sends keys."""

    def __init__(self, keys: List[str], max_x: int, max_y: int,
                 shared_config: Optional[SharedConfig] = None,
                 test_mode: bool = False):
        self.keys = keys
        self.max_x = max_x
        self.max_y = max_y
        self.shared_config = shared_config
        self.test_mode = test_mode

        self.slots: Dict[int, SlotState] = {}
        self.current_slot = 0
        self.active_keys: Dict[int, str] = {}  # slot -> key name
        self.key_counts: Dict[str, int] = {}
        self.stats = {"events": 0, "presses": 0, "releases": 0, "drags": 0}

        # Cached view of SharedConfig, refreshed only when its version changes
        self._cfg_version = -1
        self._cfg_locked = True
        self._cfg_zones: list = []

    def _refresh_config(self):
        """Re-read shared config only when it actually changed (cheap hot path)."""
        cfg = self.shared_config
        if not cfg:
            self._cfg_locked = True
            self._cfg_zones = []
            return
        if cfg.version == self._cfg_version:
            return  # Nothing changed since last sync frame
        version, locked, zones = cfg.snapshot_runtime()
        self._cfg_version = version
        self._cfg_locked = locked
        self._cfg_zones = zones

    def _get_slot(self, idx: int) -> SlotState:
        if idx not in self.slots:
            self.slots[idx] = SlotState()
        return self.slots[idx]

    def _normalize(self, x: int, y: int) -> Tuple[float, float]:
        """Normalize raw hardware coords to 0-1 range."""
        nx = max(0.0, min(1.0, x / self.max_x)) if self.max_x > 0 else 0.0
        ny = max(0.0, min(1.0, y / self.max_y)) if self.max_y > 0 else 0.0
        return nx, ny

    def _find_key(self, nx: float, ny: float) -> Optional[str]:
        """
        Find which key a hardware-normalized touch point maps to.
        Uses pre-computed zone hitboxes from the phone (already in hw coords).
        """
        if self._cfg_zones:
            for z in self._cfg_zones:
                if (z['x_min'] <= nx <= z['x_max'] and
                        z['y_min'] <= ny <= z['y_max']):
                    return z['key']
            return None

        # Fallback: divide full screen into equal columns
        n = len(self.keys)
        col = int(nx * n)
        col = max(0, min(n - 1, col))
        return self.keys[col]

    def process_event(self, ev_type: int, ev_code: int, ev_value: int):
        self.stats["events"] += 1

        if ev_type == EV_ABS:
            if ev_code == ABS_MT_SLOT:
                self.current_slot = ev_value
            elif ev_code == ABS_MT_TRACKING_ID:
                slot = self._get_slot(self.current_slot)
                slot.tracking_id = ev_value
                slot.changed = True
            elif ev_code == ABS_MT_POSITION_X:
                slot = self._get_slot(self.current_slot)
                slot.x = ev_value
                slot.changed = True
            elif ev_code == ABS_MT_POSITION_Y:
                slot = self._get_slot(self.current_slot)
                slot.y = ev_value
                slot.changed = True
        elif ev_type == EV_SYN and ev_code == SYN_REPORT:
            self._handle_sync()

    @staticmethod
    def _counts_for(active_keys):
        counts = {}
        for key in active_keys.values():
            counts[key] = counts.get(key, 0) + 1
        return counts

    def _inject_key_down(self, key):
        if self.test_mode:
            print(f"  [DOWN] PRESS [{key.upper()}]  (frame transition)")
            return
        if not press_key(key):
            raise RuntimeError(f"Could not press key {key!r}")

    def _inject_key_up(self, key):
        if self.test_mode:
            print(f"  [UP] RELEASE [{key.upper()}]  (frame transition)")
            return
        if not release_key(key):
            raise RuntimeError(f"Could not release key {key!r}")

    def _handle_sync(self):
        self._refresh_config()
        # Don't send keys if phone UI is unlocked (user is configuring zones)
        if not self._cfg_locked:
            # Still track positions but don't send keys
            for slot in self.slots.values():
                slot.changed = False
            return

        old_active_keys = dict(self.active_keys)
        old_key_counts = self._counts_for(old_active_keys)
        next_active_keys = dict(old_active_keys)
        changed_slots = []

        # First resolve every changed slot to its final owner for this frame.
        for slot_idx, slot in self.slots.items():
            if not slot.changed:
                continue
            slot.changed = False

            if slot.tracking_id == -1:
                new_key = None
            else:
                nx, ny = self._normalize(slot.x, slot.y)
                new_key = self._find_key(nx, ny)
            old_key = self.active_keys.get(slot_idx)

            if new_key != old_key:
                changed_slots.append((slot_idx, old_key, new_key))
                if new_key:
                    next_active_keys[slot_idx] = new_key
                else:
                    next_active_keys.pop(slot_idx, None)

        if not changed_slots:
            # Repair any stale counts without emitting input.
            self.key_counts = old_key_counts
            return

        next_key_counts = self._counts_for(next_active_keys)
        keys_down = [
            key for key in next_key_counts
            if old_key_counts.get(key, 0) == 0
        ]
        keys_up = [
            key for key in old_key_counts
            if next_key_counts.get(key, 0) == 0
        ]

        failures = []
        primary_error = None

        # All key-down transitions precede every key-up transition.
        for key in keys_down:
            try:
                self._inject_key_down(key)
                self.stats["presses"] += 1
            except BaseException as exc:
                failures.append(("down", key, exc))
                if primary_error is None:
                    primary_error = exc

        for key in keys_up:
            try:
                self._inject_key_up(key)
                self.stats["releases"] += 1
            except BaseException as exc:
                failures.append(("up", key, exc))
                if primary_error is None:
                    primary_error = exc

        if failures:
            # Abort touch ownership and retain one conservative count for each
            # key that may still be down. Outer cleanup releases this union.
            possibly_held = dict.fromkeys(
                list(old_key_counts) + keys_down, 1,
            )
            self.active_keys.clear()
            self.key_counts = possibly_held
            for slot in self.slots.values():
                slot.tracking_id = -1
                slot.changed = False
            try:
                primary_error.touch_failures = tuple(failures)
            except BaseException:
                pass
            raise primary_error

        self.active_keys = next_active_keys
        self.key_counts = next_key_counts
        self.stats["drags"] += sum(
            1 for _, old_key, new_key in changed_slots
            if old_key and new_key
        )

    def release_all_keys(self):
        """Best-effort release of every synthesized key; safe to call twice."""
        held_keys = [
            key for key, count in self.key_counts.items() if count > 0
        ]

        # Counts are the authoritative Windows key-down ownership state.
        # Clear all logical state before injection so repeated cleanup is safe.
        self.key_counts.clear()
        self.active_keys.clear()
        for slot in self.slots.values():
            slot.tracking_id = -1
            slot.changed = False

        failures = []
        for key in held_keys:
            try:
                if self.test_mode:
                    released = True
                else:
                    released = release_key(key)
                if not released:
                    failures.append(key)
            except BaseException:
                failures.append(key)
            finally:
                self.stats["releases"] += 1
        return failures


# ============================================================================
# ADB Helpers
# ============================================================================

def run_adb(*args, timeout=10) -> str:
    cmd = [ADB_PATH] + list(args)
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return result.stdout.strip()
    except FileNotFoundError:
        print(f"[ERROR] ADB not found at '{ADB_PATH}'.")
        print("   Set ADB_PATH in the script or pass --adb <path>.")
        sys.exit(1)
    except subprocess.TimeoutExpired:
        return ""


class AdbCommandError(RuntimeError):
    """Sanitized checked-ADB failure safe for ordinary user output."""

    def __init__(self, args, reason):
        super().__init__(reason)
        self.args_list = tuple(args)
        self.reason = reason


def run_adb_checked(*args, timeout=10) -> str:
    """Run ADB and raise a sanitized error for any detectable failure."""
    cmd = [ADB_PATH] + list(args)
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
        )
    except OSError as exc:
        raise AdbCommandError(args, "ADB is unavailable") from exc
    except subprocess.TimeoutExpired as exc:
        raise AdbCommandError(args, "command timed out") from exc

    if result.returncode != 0:
        raise AdbCommandError(
            args, f"command exited with status {result.returncode}",
        )

    output = "\n".join((result.stdout or "", result.stderr or "")).lower()
    failure_markers = (
        "error:", "exception occurred", "permission denial",
        "unknown command", "not found",
    )
    if any(marker in output for marker in failure_markers):
        raise AdbCommandError(args, "device rejected the command")

    return (result.stdout or "").strip()


@dataclass(frozen=True)
class CleanupFailure:
    component: str
    detail: str
    recovery_command: Optional[str] = None


class AdbReverseMapping:
    """Own exactly one reverse mapping created by this process."""

    def __init__(self, port: int):
        self.device_endpoint = f"tcp:{port}"
        self.host_endpoint = f"tcp:{port}"
        self._owned = False

    @property
    def owned(self) -> bool:
        return self._owned

    def _mapping_exists(self) -> bool:
        listing = run_adb_checked("reverse", "--list", timeout=5)
        for line in listing.splitlines():
            fields = line.split()
            if (len(fields) >= 2
                    and fields[-2:] == [
                        self.device_endpoint, self.host_endpoint,
                    ]):
                return True
        return False

    def create(self):
        if self._owned:
            return
        if self._mapping_exists():
            raise AdbCommandError(
                ("reverse", self.device_endpoint, self.host_endpoint),
                f"{self.device_endpoint} already has a reverse mapping",
            )

        try:
            run_adb_checked(
                "reverse", self.device_endpoint, self.host_endpoint, timeout=5,
            )
        except AdbCommandError:
            # A timeout can race command completion. Since preflight proved the
            # endpoint was free, a newly visible exact mapping is ours.
            try:
                self._owned = self._mapping_exists()
            except AdbCommandError:
                pass
            raise
        else:
            self._owned = True

    def remove(self):
        if not self._owned:
            return []
        try:
            run_adb_checked(
                "reverse", "--remove", self.device_endpoint, timeout=5,
            )
        except AdbCommandError as exc:
            return [
                CleanupFailure(
                    "ADB reverse mapping",
                    exc.reason,
                    f"adb reverse --remove {self.device_endpoint}",
                ),
            ]
        self._owned = False
        return []


def check_device_connected() -> bool:
    output = run_adb("devices")
    for line in output.strip().split("\n")[1:]:
        if "\tdevice" in line:
            return True
    return False


def detect_touch_device() -> Tuple[Optional[str], int, int]:
    """Auto-detect touchscreen input device and resolution."""
    print("[SCAN] Detecting touchscreen device...")
    output = run_adb("shell", "getevent", "-lp")
    if not output:
        return None, 0, 0

    current_device = None
    is_touchscreen = False
    max_x = max_y = 0
    best = (None, 0, 0)

    for line in output.split("\n"):
        line = line.strip()
        device_match = re.match(r'add device \d+:\s*(/dev/input/event\d+)', line)
        if device_match:
            if current_device and is_touchscreen and max_x > 0:
                best = (current_device, max_x, max_y)
            current_device = device_match.group(1)
            is_touchscreen = False
            max_x = max_y = 0
            continue

        if "ABS_MT_POSITION_X" in line:
            is_touchscreen = True
            m = re.search(r'max\s+(\d+)', line)
            if m: max_x = int(m.group(1))
        if "ABS_MT_POSITION_Y" in line:
            m = re.search(r'max\s+(\d+)', line)
            if m: max_y = int(m.group(1))

    if current_device and is_touchscreen and max_x > 0:
        best = (current_device, max_x, max_y)
    return best


# ============================================================================
# HTTP Server (serves controller UI and config API)
# ============================================================================

class ControllerHandler(http.server.BaseHTTPRequestHandler):
    """HTTP handler for the phone controller UI and config API."""

    shared_config: SharedConfig = None  # Set by start_controller_server

    def do_GET(self):
        if self.path in ('/', '/controller.html', '/index.html'):
            self._serve_file(CONTROLLER_HTML, 'text/html; charset=utf-8')
        elif self.path == '/api/config':
            data = json.dumps(self.shared_config.get_dict()).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.send_header('Cache-Control', 'no-cache')
            self.end_headers()
            self.wfile.write(data)
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path == '/api/config':
            length = int(self.headers.get('Content-Length', 0))
            body = self.rfile.read(length)
            try:
                data = json.loads(body)
                self.shared_config.update_from_phone(data)
                locked = self.shared_config.is_locked()
                n_zones = len(self.shared_config.get_hw_zones())
                status = "LOCKED (play mode)" if locked else "UNLOCKED (configuring)"
                print(f"  [CONFIG] {status} | {n_zones} zones")
            except (json.JSONDecodeError, KeyError) as e:
                print(f"  [WARN] Bad config from phone: {e}")

            resp = b'{"ok":true}'
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(resp)))
            self.end_headers()
            self.wfile.write(resp)
        else:
            self.send_error(404)

    def _serve_file(self, filepath, content_type):
        try:
            with open(filepath, 'rb') as f:
                data = f.read()
            self.send_response(200)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except FileNotFoundError:
            self.send_error(404, f'File not found: {filepath}')

    def log_message(self, format, *args):
        pass  # Suppress HTTP request logs


def start_controller_server(shared_config: SharedConfig, session):
    """
    Start HTTP server, set up ADB reverse tunnel, and open controller in Chrome.
    Returns the server instance or None on failure.
    """
    if not os.path.exists(CONTROLLER_HTML):
        print(f"   [WARN] controller.html not found at {CONTROLLER_HTML}")
        return None

    ControllerHandler.shared_config = shared_config

    try:
        server = http.server.HTTPServer(("127.0.0.1", SERVER_PORT), ControllerHandler)
    except OSError as e:
        print(f"   [WARN] Could not start HTTP server on port {SERVER_PORT}: {e}")
        return None

    session.server = server
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    session.server_running = True
    session.controller_started = True
    print(f"   HTTP server on 127.0.0.1:{SERVER_PORT}")

    # ADB reverse tunnel: phone's localhost:PORT -> PC's localhost:PORT
    session.reverse_mapping.create()
    print("   ADB reverse tunnel: OK")

    # Open controller in Chrome on the phone
    print("   Opening controller UI in Chrome...")
    run_adb_checked(
        "shell", "am", "start",
        "-a", "android.intent.action.VIEW",
        "-d", f"http://localhost:{SERVER_PORT}/",
        timeout=10,
    )
    time.sleep(1.5)

    # NOTE: screen brightness is intentionally left untouched — we never
    # dim the user's screen.

    print("   [OK] Controller UI active on phone.")
    return server


_SETTING_READ_FAILED = object()


def _get_setting(namespace: str, key: str):
    """Read a setting without mistaking an ADB failure for an unset value."""
    try:
        out = run_adb_checked(
            "shell", "settings", "get", namespace, key, timeout=5,
        )
    except AdbCommandError:
        return _SETTING_READ_FAILED
    return None if out in ("", "null") else out


class PhoneSettingsBackup:
    """
    Snapshot the user's phone settings before we override them for gameplay,
    and restore the exact original values afterwards (never hardcoded guesses).
    """

    _KEYS = (
        ("system", "screen_off_timeout"),
        ("global", "stay_on_while_plugged_in"),
        ("global", "policy_control"),
        ("global", "zen_mode"),
    )
    _LABELS = {
        ("system", "screen_off_timeout"): "screen timeout",
        ("global", "stay_on_while_plugged_in"): "stay-awake mode",
        ("global", "policy_control"): "immersive-mode policy",
        ("global", "zen_mode"): "Do Not Disturb",
    }

    # zen_mode int -> `cmd notification set_dnd` argument.
    _ZEN_MODES = {0: "off", 1: "priority", 2: "none", 3: "alarms"}

    def __init__(self):
        self._originals: Dict[Tuple[str, str], Optional[str]] = {}
        self._pending_restore = set()
        self._snapshotted = False

    def snapshot(self):
        if self._snapshotted:
            return []

        failures = []
        for namespace, key in self._KEYS:
            value = _get_setting(namespace, key)
            if value is _SETTING_READ_FAILED:
                failures.append(
                    CleanupFailure(
                        self._LABELS[(namespace, key)],
                        "original value could not be read; override skipped",
                    ),
                )
                continue
            self._originals[(namespace, key)] = value

        self._snapshotted = True
        return failures

    @classmethod
    def _zen_mode(cls, value):
        if value is None:
            return None
        try:
            return cls._ZEN_MODES.get(int(value))
        except (TypeError, ValueError):
            return None

    @staticmethod
    def _manual_command(args):
        return "adb " + " ".join(shlex.quote(str(arg)) for arg in args)

    def _restore_args(self, setting):
        namespace, key = setting
        value = self._originals[setting]
        if key == "zen_mode":
            mode = self._zen_mode(value)
            if mode is None:
                return None
            return ("shell", "cmd", "notification", "set_dnd", mode)
        if value is None:
            return ("shell", "settings", "delete", namespace, key)
        return ("shell", "settings", "put", namespace, key, value)

    def apply_overrides(self):
        """Apply only overrides whose exact original state can be restored."""
        failures = []
        overrides = {
            ("system", "screen_off_timeout"): (
                "shell", "settings", "put", "system",
                "screen_off_timeout", "2147483647",
            ),
            ("global", "stay_on_while_plugged_in"): (
                "shell", "settings", "put", "global",
                "stay_on_while_plugged_in", "2",
            ),
            ("global", "policy_control"): (
                "shell", "settings", "put", "global",
                "policy_control", "immersive.full=*",
            ),
            ("global", "zen_mode"): (
                "shell", "cmd", "notification", "set_dnd", "priority",
            ),
        }

        for setting in self._KEYS:
            if setting not in self._originals:
                continue
            if (setting[1] == "zen_mode"
                    and self._zen_mode(self._originals[setting]) is None):
                failures.append(
                    CleanupFailure(
                        self._LABELS[setting],
                        "original state is unknown; DND override skipped",
                    ),
                )
                continue

            # A timeout or transport error can be ambiguous, so retain the
            # original for cleanup before attempting the write.
            self._pending_restore.add(setting)
            try:
                run_adb_checked(*overrides[setting], timeout=5)
            except (KeyboardInterrupt, SystemExit):
                raise
            except AdbCommandError as exc:
                restore_args = self._restore_args(setting)
                failures.append(
                    CleanupFailure(
                        self._LABELS[setting],
                        f"override failed: {exc.reason}",
                        self._manual_command(restore_args),
                    ),
                )
            except Exception:
                restore_args = self._restore_args(setting)
                failures.append(
                    CleanupFailure(
                        self._LABELS[setting],
                        "override failed unexpectedly",
                        self._manual_command(restore_args),
                    ),
                )
        return failures

    def restore(self):
        failures = []
        for setting in self._KEYS:
            if setting not in self._pending_restore:
                continue
            args = self._restore_args(setting)
            try:
                run_adb_checked(*args, timeout=5)
            except AdbCommandError as exc:
                failures.append(
                    CleanupFailure(
                        self._LABELS[setting],
                        f"could not be restored: {exc.reason}",
                        self._manual_command(args),
                    ),
                )
            except BaseException:
                failures.append(
                    CleanupFailure(
                        self._LABELS[setting],
                        "could not be restored: unexpected command failure",
                        self._manual_command(args),
                    ),
                )
            else:
                self._pending_restore.remove(setting)
        return failures

    @property
    def pending_settings(self):
        return set(self._pending_restore)


def prevent_interruptions(backup: PhoneSettingsBackup):
    """
    Keep the phone fully available for touch input:
    - screen stays on while USB-powered
    - immersive sticky (hide nav/status bars)
    - Do Not Disturb (block notification popups)
    - very long screen-off timeout as a fallback
    Original values must be snapshotted first (PhoneSettingsBackup).
    Brightness is deliberately NOT touched.
    """
    print("   Suppressing interruptions (stay-on / immersive / DND)...")
    return backup.apply_overrides()


def restore_phone(backup: Optional[PhoneSettingsBackup] = None):
    """Restore backed-up phone settings; reverse mappings are owned elsewhere."""
    return backup.restore() if backup else []


# ============================================================================
# Config Persistence
# ============================================================================

def load_config() -> dict:
    if os.path.exists(CONFIG_FILE):
        try:
            with open(CONFIG_FILE, "r") as f:
                return json.load(f)
        except (json.JSONDecodeError, IOError):
            pass
    return {}


def save_config(config: dict):
    os.makedirs(CONFIG_DIR, exist_ok=True)
    with open(CONFIG_FILE, "w") as f:
        json.dump(config, f, indent=2)


# ============================================================================
# Display
# ============================================================================

def print_banner(keys, device, max_x, max_y, test_mode):
    n = len(keys)
    print()
    print("+=========================================================+")
    print("|   Holodori Phone Trackpad — hololive Dreams support      |")
    print("+=========================================================+")
    print(f"|  Device:  {device:<47s} |")
    mode_str = "TEST MODE" if test_mode else "LIVE"
    res_str = f"{max_x} x {max_y}  [{mode_str}]"
    print(f"|  Touch:   {res_str:<47s} |")
    print("+---------------------------------------------------------+")

    cell_w = max(5, 55 // n)
    top = "+" + "+".join(["-" * (cell_w - 1)] * n) + "+"
    bar = "|" + "|".join(["#" * (cell_w - 1)] * n) + "|"
    labels = "|"
    for k in keys:
        lbl = f" {k.upper()} "
        pad = cell_w - 1 - len(lbl)
        labels += " " * (pad // 2) + lbl + " " * (pad - pad // 2) + "|"

    print(f"|  {top:<55s}  |")
    print(f"|  {bar:<55s}  |")
    print(f"|  {labels:<55s}  |")
    print(f"|  {bar:<55s}  |")
    print(f"|  {top:<55s}  |")
    print("+---------------------------------------------------------+")
    print("|  Configure play zone on phone, then tap LOCK to play.   |")
    print("|  Press Ctrl+C to quit.                                  |")
    print("+=========================================================+")
    print()


# ============================================================================
# Main Event Loop
# ============================================================================

def _boost_process_priority():
    """Raise process priority so key events aren't delayed by other work."""
    try:
        # HIGH_PRIORITY_CLASS — lower latency for rhythm-game key timing
        kernel32 = ctypes.windll.kernel32
        handle = kernel32.GetCurrentProcess()
        kernel32.SetPriorityClass(handle, 0x00000080)
    except Exception:
        pass


class AdbInputTransport:
    """Own the ADB getevent process so outer cleanup controls teardown."""

    def __init__(self):
        self.proc = None
        self.stopping = False

    @staticmethod
    def _open_stream(cmd):
        try:
            return subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                bufsize=0,
            )
        except FileNotFoundError as exc:
            raise AdbCommandError(cmd[1:], "ADB is unavailable") from exc

    def open(self, device):
        self.stopping = False
        self.proc = self._open_stream(
            [ADB_PATH, "exec-out", "getevent", device],
        )
        mode = "high-rate exec-out"

        time.sleep(0.08)
        if self.proc.poll() is not None:
            try:
                self.proc.wait(timeout=0.5)
            except BaseException:
                pass
            self.proc = self._open_stream(
                [ADB_PATH, "shell", "getevent", device],
            )
            mode = "shell fallback"
        return mode

    def stop_input_processing(self):
        self.stopping = True

    def terminate(self):
        proc = self.proc
        if proc is None:
            return []

        failures = []
        try:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=2)
        except BaseException:
            failures.append(
                CleanupFailure(
                    "ADB input stream",
                    "could not terminate the getevent process",
                ),
            )
        finally:
            try:
                stopped = proc.poll() is not None
            except BaseException:
                stopped = False
            if stopped:
                self.proc = None
        return failures


def stream_events(
        device: str,
        processor: TouchProcessor,
        transport: AdbInputTransport):
    """
    Stream touch events at maximum rate.

    Prefers `adb exec-out getevent` (no PTY line-buffering) for higher
    effective polling rate / lower lag. Falls back to `adb shell getevent`.
    """
    mode = transport.open(device)
    proc = transport.proc
    _boost_process_priority()
    print(f"[PLAY] Streaming touch events ({mode})... (Ctrl+C to stop)\n")

    event_count = 0
    start_time = time.time()
    # Read raw bytes and split lines ourselves — avoids Python text-IO
    # buffering, and parse hex straight from bytes (no per-line decode).
    leftover = b""
    process_event = processor.process_event

    while not transport.stopping:
        chunk = proc.stdout.read(65536)
        if not chunk:
            if proc.poll() is not None:
                break
            continue

        lines = (leftover + chunk).split(b"\n")
        leftover = lines.pop()

        for raw_line in lines:
            # getevent line: "/dev/input/eventN: 0003 0035 00000123"
            # rsplit from the right grabs the 3 hex fields without
            # touching the device-path prefix.
            parts = raw_line.rsplit(None, 3)
            try:
                ev_type = int(parts[-3], 16)
                ev_code = int(parts[-2], 16)
                ev_value = int(parts[-1], 16)
            except (ValueError, IndexError):
                continue
            if ev_value >= 0x80000000:
                ev_value -= 0x100000000
            process_event(ev_type, ev_code, ev_value)

        event_count += len(lines)
        if event_count >= 3000:
            elapsed = time.time() - start_time
            rate = event_count / elapsed if elapsed > 0 else 0
            s = processor.stats
            state_str = "PLAYING" if processor._cfg_locked else "CONFIGURING"
            print(f"  [STATS] {s['presses']} presses | {s['releases']} releases | "
                  f"{s['drags']} drags | {rate:.0f} ev/s | {state_str}")
            event_count = 0
            start_time = time.time()


class AdbCleanupSession:
    """Idempotent owner of every ADB-mode cleanup resource."""

    def __init__(self):
        self.processor = None
        self.input_transport = AdbInputTransport()
        self.reverse_mapping = AdbReverseMapping(SERVER_PORT)
        self.settings_backup = PhoneSettingsBackup()
        self.server = None
        self.server_running = False
        self.controller_started = False

    def _stop_input_processing(self):
        failures = []
        self.input_transport.stop_input_processing()
        server = self.server
        if server is None:
            return failures

        try:
            if self.server_running:
                server.shutdown()
        except BaseException:
            failures.append(
                CleanupFailure(
                    "controller server",
                    "could not stop the controller server",
                ),
            )
        try:
            server.server_close()
        except BaseException:
            failures.append(
                CleanupFailure(
                    "controller server",
                    "could not close the controller server",
                ),
            )
        self.server = None
        self.server_running = False
        return failures

    def cleanup(self):
        failures = []

        # 1. Stop input processing.
        try:
            failures.extend(self._stop_input_processing())
        except BaseException:
            failures.append(
                CleanupFailure(
                    "input processing",
                    "could not stop input processing",
                ),
            )

        # 2. Release all synthesized keys.
        if self.processor is not None:
            try:
                for key in self.processor.release_all_keys():
                    failures.append(
                        CleanupFailure(
                            f"key {key!r}",
                            "key-up injection failed",
                        ),
                    )
            except BaseException:
                failures.append(
                    CleanupFailure(
                        "synthesized keys",
                        "unexpected key-release failure",
                    ),
                )

        # 3. Terminate or cancel the ADB input stream.
        try:
            failures.extend(self.input_transport.terminate())
        except BaseException:
            failures.append(
                CleanupFailure(
                    "ADB input stream",
                    "unexpected stream-termination failure",
                ),
            )

        # 4. Remove only this session's owned reverse mapping.
        try:
            failures.extend(self.reverse_mapping.remove())
        except BaseException:
            failures.append(
                CleanupFailure(
                    "ADB reverse mapping",
                    "unexpected reverse-mapping cleanup failure",
                ),
            )

        # 5. Restore every setting that may have been changed.
        try:
            failures.extend(self.settings_backup.restore())
        except BaseException:
            failures.append(
                CleanupFailure(
                    "phone settings",
                    "unexpected restoration failure",
                ),
            )

        return failures


def report_failures(title, failures):
    if not failures:
        return
    print(f"[WARN] {title}:")
    for failure in failures:
        print(f"  - {failure.component}: {failure.detail}")
        if failure.recovery_command:
            print(f"    Manual recovery: {failure.recovery_command}")


# ============================================================================
# CLI Entry Point
# ============================================================================

def main():
    import argparse

    parser = argparse.ArgumentParser(
        description=(
            "Holodori Phone Trackpad — use your Android phone as a multi-touch "
            "controller for hololive Dreams (holodori) on PC"
        )
    )
    parser.add_argument("--keys", nargs="+", default=None,
                        help="Keys to map left-to-right. Default Holodori: s d f j k l")
    parser.add_argument("--device", type=str, default=None,
                        help="Input device path (e.g. /dev/input/event2)")
    parser.add_argument("--test", action="store_true",
                        help="Print events without sending keys")
    parser.add_argument("--selftest", action="store_true",
                        help="Test key sending by typing 'hello' into focused window")
    parser.add_argument("--no-ui", action="store_true",
                        help="Don't launch phone controller UI")
    parser.add_argument("--adb", type=str, default=None,
                        help="Path to ADB executable (or set ADB_PATH env)")
    parser.add_argument(
        "--transport", choices=("aoa", "adb"), default="aoa",
        help="USB transport. AOA is the default and does not require USB debugging",
    )
    overlay_flags = parser.add_mutually_exclusive_group()
    overlay_flags.add_argument(
        "--overlay", action="store_true",
        help="Show the click-through PC touch-position overlay (off by default)",
    )
    overlay_flags.add_argument(
        "--no-overlay", action="store_true",
        help="Keep the PC touch-position overlay disabled (the default)",
    )
    parser.add_argument(
        "--overlay-edit", action="store_true",
        help="Start with the PC touch zone in position/resize mode",
    )
    parser.add_argument(
        "--usb-vid", type=lambda value: int(value, 0), default=None,
        help="Additional Android USB vendor ID, for example 0x1234",
    )
    parser.add_argument(
        "--no-usbdk", action="store_true",
        help="Disable the UsbDk handshake and data fallback",
    )
    parser.add_argument(
        "--aoa-read-depth",
        type=int,
        choices=(1, 2),
        default=2,
        help="Number of preposted WinUSB reads (default: 2)",
    )
    parser.add_argument(
        "--aoa-benchmark",
        action="store_true",
        help="Report clock-normalized AOA transport jitter",
    )
    parser.add_argument(
        "--diagnose",
        action="store_true",
        help="Connection Doctor: live connection-stage view plus 'report' "
        "and 'retry' console commands",
    )

    args = parser.parse_args()

    if args.no_overlay and args.overlay_edit:
        parser.error("--overlay-edit cannot be used with --no-overlay")

    if args.selftest:
        self_test_keys()
        return

    config = load_config()
    keys = args.keys or config.get("keys", DEFAULT_KEYS)
    config["keys"] = keys
    save_config(config)

    if args.transport == "aoa":
        from aoa_mode import run_aoa_mode

        _boost_process_priority()
        run_aoa_mode(
            keys=keys,
            test_mode=args.test,
            overlay_enabled=args.overlay or args.overlay_edit,
            overlay_edit=args.overlay_edit,
            config=config,
            save_config=save_config,
            press_key=press_key,
            release_key=release_key,
            use_usbdk=not args.no_usbdk,
            extra_vendor_id=args.usb_vid,
            winusb_read_depth=args.aoa_read_depth,
            benchmark=args.aoa_benchmark,
            diagnostics=args.diagnose,
        )
        return

    global ADB_PATH
    session = AdbCleanupSession()
    shared_config = None
    try:
        ADB_PATH = resolve_adb_path(args.adb)

        print("[PHONE] Checking for connected Android device...")
        if not check_device_connected():
            print("[ERROR] No Android device found!")
            print()
            print("  1. Connect phone via USB")
            print("  2. Enable USB Debugging (Settings -> Developer Options)")
            print("  3. Accept the USB debugging prompt on phone")
            print("  4. Run again")
            sys.exit(1)
        print("[OK] Device connected!")

        device = args.device or config.get("device")
        max_x = config.get("max_x", 0)
        max_y = config.get("max_y", 0)

        if device and max_x and max_y:
            print(f"[INFO] Using saved device: {device} ({max_x}x{max_y})")
        else:
            device, max_x, max_y = detect_touch_device()

        if not device:
            print("[ERROR] Could not detect touchscreen!")
            print("   Run: adb shell getevent -lp")
            print("   Then use: --device /dev/input/eventN")
            sys.exit(1)

        if max_x == 0 or max_y == 0:
            max_x, max_y = 1080, 2400
            print(f"[WARN] Using default resolution {max_x}x{max_y}")

        print(f"[OK] Touchscreen: {device} ({max_x} x {max_y})")
        config.update({
            "device": device,
            "max_x": max_x,
            "max_y": max_y,
            "keys": keys,
        })
        save_config(config)

        shared_config = SharedConfig(keys)
        if "playzone" in config:
            shared_config.update_from_phone({"playzone": config["playzone"]})

        setup_failures = session.settings_backup.snapshot()
        report_failures("Phone setup incomplete", setup_failures)

        apply_phone_overrides = args.no_ui
        if not args.no_ui:
            print("[PHONE] Setting up controller UI...")
            server = start_controller_server(shared_config, session)
            apply_phone_overrides = server is not None
            if server:
                print()
                print("  >> Configure the play zone on your phone,")
                print("  >> then tap the LOCK button to start playing.")
                print()

        if apply_phone_overrides:
            setup_failures = prevent_interruptions(
                session.settings_backup,
            )
            report_failures("Phone setup incomplete", setup_failures)

        processor = TouchProcessor(
            keys=keys,
            max_x=max_x,
            max_y=max_y,
            shared_config=shared_config,
            test_mode=args.test,
        )
        session.processor = processor

        print_banner(keys, device, max_x, max_y, args.test)
        stream_events(device, processor, session.input_transport)
    except KeyboardInterrupt:
        pass
    finally:
        try:
            cleanup_failures = session.cleanup()
        except BaseException:
            cleanup_failures = [
                CleanupFailure(
                    "cleanup coordinator",
                    "unexpected cleanup failure",
                ),
            ]

        if session.controller_started and shared_config is not None:
            try:
                config["playzone"] = shared_config.get_playzone()
                save_config(config)
            except BaseException:
                cleanup_failures.append(
                    CleanupFailure(
                        "play-zone configuration",
                        "could not save the final play-zone configuration",
                    ),
                )

        if session.processor is not None:
            s = session.processor.stats
            print(
                f"\n[STATS] Session: {s['presses']} presses, "
                f"{s['releases']} releases, {s['drags']} drags"
            )

        report_failures("Cleanup incomplete", cleanup_failures)

    if cleanup_failures:
        print("[BYE] Exited with incomplete cleanup; see warnings above.")
    else:
        print("[BYE] Done!")


if __name__ == "__main__":
    main()
