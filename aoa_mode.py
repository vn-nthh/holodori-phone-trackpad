"""AOA controller mode shared by the CLI and PC overlay."""

from __future__ import annotations
from typing import Callable, Optional

from aoa_transport import (
    ACTION_CANCEL,
    ACTION_DOWN,
    ACTION_HEARTBEAT,
    ACTION_MOVE,
    ACTION_UP,
    AoaReceiver,
    TouchEvent,
)
from touch_overlay import TouchOverlay


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
        if (
            not released
            and event.locked
            and event.inside
            and 0.0 <= event.x <= 1.0
            and 0.0 <= event.y <= 1.0
        ):
            column = min(len(self.keys) - 1, int(event.x * len(self.keys)))
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
) -> None:
    overlay_config = config.setdefault("pc_overlay", {})

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

    def status(text: str, connected: bool) -> None:
        print(f"[AOA] {text}")
        if overlay:
            overlay.publish_status(text, connected)

    receiver = AoaReceiver(
        on_event=router.handle,
        on_status=status,
        on_disconnect=router.release_all,
        lane_count=len(keys),
        use_usbdk=use_usbdk,
        extra_vendor_id=extra_vendor_id,
        winusb_read_depth=winusb_read_depth,
        benchmark=benchmark,
    )
    receiver.start()

    print("[PLAY] AOA mode active. USB debugging is not required.")
    if overlay:
        print("       Ctrl+Shift+O edits the PC zone; Ctrl+Shift+Q quits.")
    else:
        print("       Press Ctrl+C to quit.")

    try:
        if overlay:
            overlay.run()
        else:
            while not receiver.finished.wait(0.25):
                pass
    except KeyboardInterrupt:
        pass
    finally:
        receiver.stop()
        router.release_all()

    stats = router.stats
    print(
        f"[STATS] Session: {stats['presses']} presses, "
        f"{stats['releases']} releases, {stats['drags']} drags, "
        f"{router.sequence_gaps} dropped records"
    )
    queue_stats = receiver.queue_telemetry_snapshot()
    if queue_stats.reports:
        print(
            f"[AOA QUEUE] max {queue_stats.max_age_ms:.2f} ms old, "
            f"depth {queue_stats.max_depth}, "
            f"{queue_stats.warning_reports} warning reports, "
            f"{queue_stats.resyncs} resyncs, "
            f"{queue_stats.failsafe_reports} failsafe reports"
        )
    if benchmark:
        latency = receiver.latency_snapshot()
        if latency.samples:
            print(
                "[AOA BENCH] clock-normalized event-to-host excess: "
                f"mean {latency.mean_excess_ms:.3f} ms, "
                f"max {latency.max_excess_ms:.3f} ms across "
                f"{latency.samples} touch records"
            )
            print(
                "            Fastest sample is the zero baseline; this is "
                "relative jitter, not absolute one-way latency."
            )
