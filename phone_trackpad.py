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
        self.active_keys: Dict[int, Optional[str]] = {}  # slot -> key name
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
                if ev_value == -1:
                    self._handle_finger_up(self.current_slot)
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

    def _handle_finger_up(self, slot_idx: int):
        old_key = self.active_keys.get(slot_idx)
        if old_key:
            if self.test_mode:
                print(f"  [UP] RELEASE [{old_key.upper()}]  (slot {slot_idx})")
            else:
                release_key(old_key)
            self.stats["releases"] += 1
            self.active_keys[slot_idx] = None

    def _handle_sync(self):
        self._refresh_config()
        # Don't send keys if phone UI is unlocked (user is configuring zones)
        if not self._cfg_locked:
            # Still track positions but don't send keys
            for slot in self.slots.values():
                slot.changed = False
            return

        for slot_idx, slot in self.slots.items():
            if not slot.changed:
                continue
            slot.changed = False

            if slot.tracking_id == -1:
                continue

            nx, ny = self._normalize(slot.x, slot.y)
            new_key = self._find_key(nx, ny)
            old_key = self.active_keys.get(slot_idx)

            if new_key != old_key:
                # Press the new key before releasing the old one so drag
                # transitions never leave a gap (no mid-slide interruptions).
                if new_key:
                    if self.test_mode:
                        action = "DRAG ->" if old_key else "PRESS"
                        print(f"  [DOWN] {action} [{new_key.upper()}]  (slot {slot_idx}, pos {nx:.2f},{ny:.2f})")
                    else:
                        press_key(new_key)
                    self.stats["presses"] += 1
                    if old_key:
                        self.stats["drags"] += 1

                if old_key:
                    if self.test_mode:
                        print(f"  [UP] RELEASE [{old_key.upper()}]  (drag out, slot {slot_idx})")
                    else:
                        release_key(old_key)
                    self.stats["releases"] += 1

                self.active_keys[slot_idx] = new_key


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


def start_controller_server(shared_config: SharedConfig):
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

    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    print(f"   HTTP server on 127.0.0.1:{SERVER_PORT}")

    # ADB reverse tunnel: phone's localhost:PORT -> PC's localhost:PORT
    result = run_adb("reverse", f"tcp:{SERVER_PORT}", f"tcp:{SERVER_PORT}")
    print(f"   ADB reverse tunnel: {result if result else 'OK'}")

    # Open controller in Chrome on the phone
    print("   Opening controller UI in Chrome...")
    run_adb("shell", "am", "start",
            "-a", "android.intent.action.VIEW",
            "-d", f"http://localhost:{SERVER_PORT}/")
    time.sleep(1.5)

    # NOTE: screen brightness is intentionally left untouched — we never
    # dim the user's screen.

    print("   [OK] Controller UI active on phone.")
    return server


def _get_setting(namespace: str, key: str) -> Optional[str]:
    """Read a settings value; None when unset ('null')."""
    out = run_adb("shell", "settings", "get", namespace, key, timeout=5)
    out = (out or "").strip()
    return None if out in ("", "null") else out


class PhoneSettingsBackup:
    """
    Snapshot the user's phone settings before we override them for gameplay,
    and restore the exact original values afterwards (never hardcoded guesses).
    """

    # zen_mode int -> `cmd notification set_dnd` argument
    _ZEN_MODES = {0: "off", 1: "priority", 2: "total-silence", 3: "alarms"}

    def __init__(self):
        self._originals: Dict[Tuple[str, str], Optional[str]] = {}
        self._taken = False

    def snapshot(self):
        if self._taken:
            return
        keys = [
            ("system", "screen_off_timeout"),
            ("global", "stay_on_while_plugged_in"),
            ("global", "policy_control"),
            ("global", "zen_mode"),
        ]
        self._originals = {(ns, k): _get_setting(ns, k) for ns, k in keys}
        self._taken = True

    def restore(self):
        if not self._taken:
            return
        for (ns, key), value in self._originals.items():
            if key == "zen_mode":
                # Restore DND via the same command surface we changed it with
                mode = self._ZEN_MODES.get(int(value) if value else 0, "off")
                run_adb("shell", "cmd", "notification", "set_dnd", mode, timeout=5)
            elif value is None:
                run_adb("shell", "settings", "delete", ns, key, timeout=5)
            else:
                run_adb("shell", "settings", "put", ns, key, value, timeout=5)
        self._taken = False


def prevent_interruptions():
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
    # Stay awake while USB is connected (reverted on cleanup)
    run_adb("shell", "svc", "power", "stayon", "usb")
    # Long timeout fallback if stayon isn't honored
    run_adb("shell", "settings", "put", "system", "screen_off_timeout", "2147483647")
    # Hide system bars so edge swipes / status bar don't steal focus
    run_adb("shell", "settings", "put", "global", "policy_control", "immersive.full=*")
    # Block heads-up notifications during play
    run_adb("shell", "cmd", "notification", "set_dnd", "priority")


def restore_phone(backup: Optional[PhoneSettingsBackup] = None):
    """Restore original phone settings and remove the ADB tunnel."""
    if backup:
        backup.restore()
    else:
        # Shouldn't happen, but never leave the phone stuck: revert to sane
        # defaults without touching brightness.
        run_adb("shell", "svc", "power", "stayon", "false")
        run_adb("shell", "settings", "delete", "system", "screen_off_timeout")
        run_adb("shell", "settings", "delete", "global", "policy_control")
        run_adb("shell", "cmd", "notification", "set_dnd", "off")
    run_adb("reverse", "--remove-all")


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


def stream_events(device: str, processor: TouchProcessor):
    """
    Stream touch events at maximum rate.

    Prefers `adb exec-out getevent` (no PTY line-buffering) for higher
    effective polling rate / lower lag. Falls back to `adb shell getevent`.
    """
    def _open_stream(cmd):
        return subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,  # unbuffered reads
        )

    # exec-out = raw, unbuffered binary pipe from device (no PTY)
    cmd = [ADB_PATH, "exec-out", "getevent", device]
    mode = "high-rate exec-out"
    try:
        proc = _open_stream(cmd)
    except FileNotFoundError:
        print(f"[ERROR] ADB not found at '{ADB_PATH}'")
        sys.exit(1)

    # If exec-out dies immediately, fall back to shell mode
    time.sleep(0.08)
    if proc.poll() is not None:
        try:
            proc.wait(timeout=0.5)
        except Exception:
            pass
        cmd = [ADB_PATH, "shell", "getevent", device]
        mode = "shell fallback"
        try:
            proc = _open_stream(cmd)
        except FileNotFoundError:
            print(f"[ERROR] ADB not found at '{ADB_PATH}'")
            sys.exit(1)

    _boost_process_priority()
    print(f"[PLAY] Streaming touch events ({mode})... (Ctrl+C to stop)\n")

    event_count = 0
    start_time = time.time()
    # Read raw bytes and split lines ourselves — avoids Python text-IO
    # buffering, and parse hex straight from bytes (no per-line decode).
    leftover = b""
    process_event = processor.process_event

    try:
        while True:
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

    except KeyboardInterrupt:
        pass
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except Exception:
            proc.kill()


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
        )
        return

    global ADB_PATH
    ADB_PATH = resolve_adb_path(args.adb)

    # Check device
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

    # Detect touch device
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

    # Setup keys and save config
    config.update({"device": device, "max_x": max_x, "max_y": max_y, "keys": keys})
    save_config(config)

    # Create shared config for phone<->PC communication
    shared_config = SharedConfig(keys)

    # Load saved playzone if available
    if "playzone" in config:
        shared_config.update_from_phone({"playzone": config["playzone"]})

    # Start controller UI on phone
    server = None
    interruptions_armed = False
    settings_backup = PhoneSettingsBackup()
    if not args.no_ui:
        print("[PHONE] Setting up controller UI...")
        server = start_controller_server(shared_config)
        if server:
            # Snapshot user settings first, then apply gameplay overrides
            settings_backup.snapshot()
            prevent_interruptions()
            interruptions_armed = True
            print()
            print("  >> Configure the play zone on your phone,")
            print("  >> then tap the LOCK button to start playing.")
            print()
    else:
        # Still suppress phone interruptions even without the controller UI
        settings_backup.snapshot()
        prevent_interruptions()
        interruptions_armed = True

    # Create processor
    processor = TouchProcessor(
        keys=keys, max_x=max_x, max_y=max_y,
        shared_config=shared_config, test_mode=args.test,
    )

    print_banner(keys, device, max_x, max_y, args.test)

    # Run
    try:
        stream_events(device, processor)
    except KeyboardInterrupt:
        pass

    # Cleanup
    s = processor.stats
    print(f"\n[STATS] Session: {s['presses']} presses, {s['releases']} releases, {s['drags']} drags")

    if server or interruptions_armed:
        print("[CLEANUP] Restoring phone...")
        if server:
            # Save final playzone config
            config["playzone"] = shared_config.get_playzone()
            save_config(config)
        restore_phone(settings_backup if interruptions_armed else None)

    print("[BYE] Done!")


if __name__ == "__main__":
    main()
