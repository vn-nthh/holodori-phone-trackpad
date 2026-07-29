"""AOA controller mode shared by the CLI and PC overlay."""

from __future__ import annotations

import sys
import threading
import time
from typing import Callable, Optional

from aoa_transport import (
    ACTION_CANCEL,
    ACTION_DOWN,
    ACTION_HEARTBEAT,
    ACTION_UP,
    AoaReceiver,
    TouchEvent,
)
from connection_doctor import (
    ConnectionDoctor,
    ConnectionState,
    DiagnosticEvent,
    Severity,
)
import connection_doctor as doctor_codes
from touch_overlay import TouchOverlay

STATUS_REPEAT_WINDOW_SECONDS = 5.0
DIAGNOSTIC_VIEW_INTERVAL_SECONDS = 3.0


class AoaTouchRouter:
    def __init__(
        self,
        keys: list[str],
        test_mode: bool,
        press_key: Callable[[str], bool],
        release_key: Callable[[str], bool],
        overlay: Optional[TouchOverlay],
    ) -> None:
        self.keys = keys
        self.test_mode = test_mode
        self.press_key = press_key
        self.release_key = release_key
        self.overlay = overlay
        self.active_keys: dict[int, str | None] = {}
        self.key_counts: dict[str, int] = {}
        self.stats = {"presses": 0, "releases": 0, "drags": 0, "events": 0}
        self.last_sequence: int | None = None
        self.sequence_gaps = 0

    def handle(self, event: TouchEvent) -> None:
        if self.last_sequence is not None:
            expected = (self.last_sequence + 1) & 0xFFFFFFFF
            if event.sequence != expected:
                gap = (event.sequence - expected) & 0xFFFFFFFF
                self.sequence_gaps += gap
                # The missing record may have been an UP or CANCEL. Clear the
                # host state before applying the newest complete sample so a
                # dropped edge can never leave a key held indefinitely.
                self.release_all(reset_sequence=False)
        self.last_sequence = event.sequence

        if event.action == ACTION_HEARTBEAT:
            return

        self.stats["events"] += 1
        if event.action == ACTION_CANCEL:
            self.release_all()
            return

        released = event.action == ACTION_UP
        if self.overlay:
            self.overlay.publish_touch(
                event.pointer_id,
                event.x,
                event.y,
                event.inside,
                released,
            )

        old_key = self.active_keys.get(event.pointer_id)
        new_key = None
        if not released and event.locked and self.keys:
            column = max(
                0,
                min(len(self.keys) - 1, int(event.x * len(self.keys))),
            )
            new_key = self.keys[column]

        if new_key == old_key:
            return

        # Press first on lane transitions, matching the legacy no-gap behavior.
        if new_key:
            count = self.key_counts.get(new_key, 0)
            if count == 0:
                if self.test_mode:
                    print(
                        f"  [DOWN] {new_key.upper()} "
                        f"(finger {event.pointer_id}, "
                        f"{event.x:.3f},{event.y:.3f})"
                    )
                else:
                    self.press_key(new_key)
                self.stats["presses"] += 1
            self.key_counts[new_key] = count + 1
            if old_key:
                self.stats["drags"] += 1

        if old_key:
            count = max(0, self.key_counts.get(old_key, 1) - 1)
            if count == 0:
                self.key_counts.pop(old_key, None)
                if self.test_mode:
                    print(
                        f"  [UP] {old_key.upper()} "
                        f"(finger {event.pointer_id})"
                    )
                else:
                    self.release_key(old_key)
                self.stats["releases"] += 1
            else:
                self.key_counts[old_key] = count

        self.active_keys[event.pointer_id] = new_key

    def release_all(self, reset_sequence: bool = True) -> None:
        for key in list(self.key_counts):
            if not self.test_mode:
                self.release_key(key)
            self.stats["releases"] += 1
        self.key_counts.clear()
        self.active_keys.clear()
        if reset_sequence:
            self.last_sequence = None
        if self.overlay:
            self.overlay.publish_cancel()


class RateLimitedStatusPrinter:
    """Print connection status lines without spamming repeated retries."""

    def __init__(self, overlay: Optional[TouchOverlay]) -> None:
        self.overlay = overlay
        self._last_text: Optional[str] = None
        self._last_print_at = 0.0
        self._suppressed = 0

    def __call__(self, text: str, connected: bool) -> None:
        if self.overlay:
            self.overlay.publish_status(text, connected)
        now = time.monotonic()
        if (
            text == self._last_text
            and not connected
            and now - self._last_print_at < STATUS_REPEAT_WINDOW_SECONDS
        ):
            self._suppressed += 1
            return
        suffix = f" (x{self._suppressed + 1})" if self._suppressed else ""
        self._suppressed = 0
        self._last_text = text
        self._last_print_at = now
        print(f"[AOA] {text}{suffix}")


def make_disconnect_handler(
    router: "AoaTouchRouter",
    doctor: Optional[ConnectionDoctor],
) -> Callable[[], None]:
    """Release every held key, then record the diagnostic event."""

    def disconnect() -> None:
        keys_held = bool(router.key_counts)
        router.release_all()
        if doctor is not None and keys_held:
            doctor.emit(
                doctor_codes.HPT_LINK_DISCONNECT_ACTIVE_INPUT,
                state=ConnectionState.DISCONNECT,
            )

    return disconnect


def _print_diagnostic_event(event: DiagnosticEvent) -> None:
    line = (
        f"  [doctor +{event.elapsed_s:8.3f}s {event.state.value}] "
        f"{event.code} ({event.severity.value}): {event.summary}"
    )
    if event.repeats:
        line += f" (x{event.repeats + 1})"
    print(line)
    if event.severity is not Severity.INFO and event.action:
        print(f"      action: {event.action}")


def _diagnostic_view_loop(
    doctor: ConnectionDoctor, stop: threading.Event
) -> None:
    last_revision = -1
    while not stop.wait(DIAGNOSTIC_VIEW_INTERVAL_SECONDS):
        snapshot = doctor.snapshot()
        if snapshot.revision == last_revision:
            continue
        last_revision = snapshot.revision
        stream = (
            f"packets={snapshot.touch_packets} flowing"
            if snapshot.packets_flowing
            else "no packets"
        )
        print(
            f"  [doctor] stage={snapshot.state.value} "
            f"backend={snapshot.backend} "
            f"winusb={snapshot.winusb_status} "
            f"usbdk={snapshot.usbdk_status} {stream}"
        )


def _diagnostic_command_loop(
    doctor: ConnectionDoctor,
    receiver: AoaReceiver,
    stop: threading.Event,
) -> None:
    print(
        "  [doctor] live diagnostics on — type 'report', 'retry', or "
        "'quit' then press Enter."
    )
    while not stop.is_set():
        try:
            line = sys.stdin.readline()
        except (OSError, ValueError):
            return
        if not line:  # stdin closed
            return
        command = line.strip().lower()
        if command in ("report", "r"):
            print(doctor.render_report())
        elif command in ("retry", ""):
            receiver.request_retry()
        elif command in ("quit", "q", "exit"):
            stop.set()
            receiver.stop()
            return


def run_aoa_mode(
    keys: list[str],
    test_mode: bool,
    overlay_enabled: bool,
    overlay_edit: bool,
    config: dict,
    save_config: Callable[[dict], None],
    press_key: Callable[[str], bool],
    release_key: Callable[[str], bool],
    use_usbdk: bool,
    extra_vendor_id: Optional[int],
    winusb_read_depth: int,
    benchmark: bool,
    diagnostics: bool = False,
) -> None:
    overlay_config = config.setdefault("pc_overlay", {})
    doctor = ConnectionDoctor()
    doctor_view_stop = threading.Event()

    def persist_overlay(value: dict) -> None:
        config["pc_overlay"] = dict(value)
        save_config(config)

    overlay = (
        TouchOverlay(
            lane_count=len(keys),
            config=overlay_config,
            save_config=persist_overlay,
            start_in_edit_mode=overlay_edit,
        )
        if overlay_enabled
        else None
    )
    router = AoaTouchRouter(
        keys=keys,
        test_mode=test_mode,
        press_key=press_key,
        release_key=release_key,
        overlay=overlay,
    )
    status = RateLimitedStatusPrinter(overlay)

    privilege_check_started = threading.Event()

    def handle_event(event: TouchEvent) -> None:
        router.handle(event)
        # One-shot, off-thread: never gates the key path.
        if (
            event.action == ACTION_DOWN
            and not privilege_check_started.is_set()
        ):
            privilege_check_started.set()
            threading.Thread(
                target=doctor.check_input_privilege, daemon=True
            ).start()

    if diagnostics:
        doctor.subscribe(_print_diagnostic_event)

    receiver = AoaReceiver(
        on_event=handle_event,
        on_status=status,
        on_disconnect=make_disconnect_handler(router, doctor),
        lane_count=len(keys),
        use_usbdk=use_usbdk,
        extra_vendor_id=extra_vendor_id,
        winusb_read_depth=winusb_read_depth,
        benchmark=benchmark,
        doctor=doctor,
    )
    receiver.start()

    view_thread: Optional[threading.Thread] = None
    command_thread: Optional[threading.Thread] = None
    if diagnostics and not overlay and sys.stdin and sys.stdin.isatty():
        view_thread = threading.Thread(
            target=_diagnostic_view_loop,
            args=(doctor, doctor_view_stop),
            name="Connection Doctor view",
            daemon=True,
        )
        view_thread.start()
        command_thread = threading.Thread(
            target=_diagnostic_command_loop,
            args=(doctor, receiver, doctor_view_stop),
            name="Connection Doctor commands",
            daemon=True,
        )
        command_thread.start()

    print("[PLAY] AOA mode active. USB debugging is not required.")
    if overlay:
        print("       Ctrl+Shift+O edits the PC zone; Ctrl+Shift+Q quits.")
    else:
        print("       Press Ctrl+C to quit.")

    try:
        if overlay:
            overlay.run()
        elif diagnostics and sys.stdin and sys.stdin.isatty():
            while not (
                receiver.finished.wait(0.25) or doctor_view_stop.wait(0.01)
            ):
                pass
        else:
            while not receiver.finished.wait(0.25):
                pass
    except KeyboardInterrupt:
        pass
    finally:
        doctor_view_stop.set()
        receiver.stop()
        router.release_all()
        doctor.close()

    warning_seen = any(
        e.severity in (Severity.WARNING, Severity.ERROR)
        for e in doctor.events()
    )
    if diagnostics and warning_seen:
        print()
        print(doctor.render_report())
    elif warning_seen:
        print(
            "[doctor] connection problems were recorded; rerun with "
            "--diagnose for a live view and a copyable diagnostic report."
        )

    stats = router.stats
    print(
        f"[STATS] Session: {stats['presses']} presses, "
        f"{stats['releases']} releases, {stats['drags']} drags, "
        f"{router.sequence_gaps} dropped records"
    )
    queue_stats = receiver.queue_telemetry_snapshot()
    if queue_stats.reports or queue_stats.host_recoveries:
        print(
            f"[AOA QUEUE] max {queue_stats.max_age_ms:.2f} ms old, "
            f"depth {queue_stats.max_depth}, "
            f"{queue_stats.warning_reports} warning reports, "
            f"{queue_stats.resyncs} resyncs, "
            f"{queue_stats.failsafe_reports} failsafe reports, "
            f"{queue_stats.host_recoveries} host recoveries"
        )
        if queue_stats.warning_reports_from_first_stroke_s:
            warning_times = ", ".join(
                f"{offset:+.3f}s"
                for offset in queue_stats.warning_reports_from_first_stroke_s
            )
            print(
                "[AOA QUEUE] warning reports from first stroke: "
                f"{warning_times}"
            )
    if benchmark:
        latency = receiver.latency_snapshot()
        if latency.samples:
            print(
                "[AOA BENCH] clock-skew-corrected event-to-host excess: "
                f"mean {latency.mean_excess_ms:.3f} ms, "
                f"max {latency.max_excess_ms:.3f} ms across "
                f"{latency.samples} recent touch records"
            )
            print(
                "            "
                f"p50 {latency.p50_excess_ms:.3f} ms, "
                f"p90 {latency.p90_excess_ms:.3f} ms, "
                f"p95 {latency.p95_excess_ms:.3f} ms, "
                f"p99 {latency.p99_excess_ms:.3f} ms, "
                f"p99.9 {latency.p99_9_excess_ms:.3f} ms"
            )
            print(
                f"            Rolling window {latency.window_seconds:.1f}s; "
                f"{latency.session_samples} session records total. "
                "Fastest recent sample is the zero baseline."
            )
            print(
                "            This is relative jitter, not absolute "
                "one-way latency."
            )
