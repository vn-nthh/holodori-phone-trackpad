import contextlib
import io
import subprocess
import unittest
from unittest import mock

import phone_trackpad
from phone_trackpad import (
    ABS_MT_POSITION_X,
    ABS_MT_POSITION_Y,
    ABS_MT_SLOT,
    ABS_MT_TRACKING_ID,
    EV_ABS,
    EV_SYN,
    SYN_REPORT,
    AdbCleanupSession,
    AdbCommandError,
    AdbReverseMapping,
    CleanupFailure,
    PhoneSettingsBackup,
    SharedConfig,
    TouchProcessor,
)


def completed(stdout="", stderr="", returncode=0):
    return subprocess.CompletedProcess(
        args=[], returncode=returncode, stdout=stdout, stderr=stderr,
    )


class CheckedAdbTests(unittest.TestCase):
    def test_checked_adb_rejects_command_failures(self):
        cases = (
            (
                "nonzero exit",
                {"return_value": completed(
                    returncode=17, stderr="private detail",
                )},
                "command exited with status 17",
            ),
            (
                "timeout",
                {"side_effect": subprocess.TimeoutExpired("adb", 5)},
                "timed out",
            ),
            (
                "reported failure",
                {"return_value": completed(
                    stderr="Error: permission denied",
                )},
                "rejected",
            ),
        )
        for label, behavior, reason in cases:
            with self.subTest(label=label), mock.patch.object(
                phone_trackpad.subprocess, "run", **behavior,
            ):
                with self.assertRaisesRegex(AdbCommandError, reason) as raised:
                    phone_trackpad.run_adb_checked(
                        "shell", "settings", "put", "system", "x", "y",
                    )
                self.assertNotIn("private detail", str(raised.exception))


class ReverseMappingTests(unittest.TestCase):
    def test_owned_mapping_creation_and_targeted_removal(self):
        mapping = AdbReverseMapping(phone_trackpad.SERVER_PORT)

        with mock.patch.object(
            phone_trackpad, "run_adb_checked", return_value="",
        ) as checked:
            mapping.create()
            self.assertTrue(mapping.owned)
            self.assertEqual(mapping.remove(), [])
            self.assertFalse(mapping.owned)

        endpoint = f"tcp:{phone_trackpad.SERVER_PORT}"
        self.assertEqual(
            checked.call_args_list,
            [
                mock.call("reverse", "--list", timeout=5),
                mock.call("reverse", endpoint, endpoint, timeout=5),
                mock.call("reverse", "--remove", endpoint, timeout=5),
            ],
        )

    def test_setup_failure_before_creation_owns_nothing(self):
        mapping = AdbReverseMapping(phone_trackpad.SERVER_PORT)
        with mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=AdbCommandError(("reverse", "--list"), "offline"),
        ) as checked:
            with self.assertRaises(AdbCommandError):
                mapping.create()
            self.assertEqual(mapping.remove(), [])

        checked.assert_called_once_with("reverse", "--list", timeout=5)
        self.assertFalse(mapping.owned)

    def test_failed_removal_retains_ownership_for_retry(self):
        mapping = AdbReverseMapping(phone_trackpad.SERVER_PORT)
        mapping._owned = True
        failure = AdbCommandError(("reverse", "--remove"), "offline")

        with mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=[failure, ""],
        ):
            first = mapping.remove()
            second = mapping.remove()

        self.assertEqual(len(first), 1)
        self.assertEqual(second, [])
        self.assertFalse(mapping.owned)


class SetupBoundaryTests(unittest.TestCase):
    def test_controller_setup_failure_before_tunnel_creation_is_safe(self):
        session = AdbCleanupSession()
        with mock.patch.object(
            phone_trackpad.http.server,
            "HTTPServer",
            side_effect=OSError("port unavailable"),
        ), mock.patch.object(phone_trackpad, "run_adb_checked") as checked:
            with contextlib.redirect_stdout(io.StringIO()):
                server = phone_trackpad.start_controller_server(
                    SharedConfig(["a"]), session,
                )
            failures = session.cleanup()

        self.assertIsNone(server)
        self.assertEqual(failures, [])
        self.assertFalse(session.reverse_mapping.owned)
        checked.assert_not_called()

    def test_controller_setup_failure_after_tunnel_creation_cleans_owned_state(self):
        session = AdbCleanupSession()
        server = mock.Mock()
        command_failure = AdbCommandError(
            ("shell", "am", "start"), "device rejected the command",
        )

        with mock.patch.object(
            phone_trackpad.http.server, "HTTPServer", return_value=server,
        ), mock.patch.object(
            phone_trackpad.threading, "Thread",
        ) as thread, mock.patch.object(
            phone_trackpad.time, "sleep",
        ), mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=["", "", command_failure, ""],
        ) as checked:
            with self.assertRaises(AdbCommandError):
                with contextlib.redirect_stdout(io.StringIO()):
                    phone_trackpad.start_controller_server(
                        SharedConfig(["a"]), session,
                    )
            failures = session.cleanup()

        self.assertEqual(failures, [])
        thread.return_value.start.assert_called_once_with()
        server.shutdown.assert_called_once_with()
        server.server_close.assert_called_once_with()
        self.assertFalse(session.reverse_mapping.owned)
        endpoint = f"tcp:{phone_trackpad.SERVER_PORT}"
        self.assertEqual(
            checked.call_args_list[-1],
            mock.call("reverse", "--remove", endpoint, timeout=5),
        )


class KeyReferenceCountTests(unittest.TestCase):
    def make_processor(self, keys=("a", "b", "c"), test_mode=False):
        config = SharedConfig(list(keys))
        config.update_from_phone({"locked": True})
        return TouchProcessor(
            list(keys), 100, 100,
            shared_config=config,
            test_mode=test_mode,
        )

    @staticmethod
    def touch(processor, slot, x, tracking_id=None):
        KeyReferenceCountTests.stage_touch(
            processor, slot, x, tracking_id,
        )
        KeyReferenceCountTests.sync(processor)

    @staticmethod
    def stage_touch(processor, slot, x, tracking_id=None):
        tracking_id = slot + 10 if tracking_id is None else tracking_id
        processor.process_event(EV_ABS, ABS_MT_SLOT, slot)
        processor.process_event(
            EV_ABS, ABS_MT_TRACKING_ID, tracking_id,
        )
        processor.process_event(EV_ABS, ABS_MT_POSITION_X, x)
        processor.process_event(EV_ABS, ABS_MT_POSITION_Y, 50)

    @staticmethod
    def move(processor, slot, x):
        KeyReferenceCountTests.stage_move(processor, slot, x)
        KeyReferenceCountTests.sync(processor)

    @staticmethod
    def stage_move(processor, slot, x):
        processor.process_event(EV_ABS, ABS_MT_SLOT, slot)
        processor.process_event(EV_ABS, ABS_MT_POSITION_X, x)
        processor.process_event(EV_ABS, ABS_MT_POSITION_Y, 50)

    @staticmethod
    def lift(processor, slot):
        KeyReferenceCountTests.stage_lift(processor, slot)
        KeyReferenceCountTests.sync(processor)

    @staticmethod
    def stage_lift(processor, slot):
        processor.process_event(EV_ABS, ABS_MT_SLOT, slot)
        processor.process_event(EV_ABS, ABS_MT_TRACKING_ID, -1)

    @staticmethod
    def sync(processor):
        processor.process_event(EV_SYN, SYN_REPORT, 0)

    @staticmethod
    def set_locked(processor, locked):
        processor.shared_config.update_from_phone({"locked": locked})
        KeyReferenceCountTests.sync(processor)

    def test_shared_lane_uses_reference_count_for_full_touch_lifecycle(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press, mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.touch(processor, 0, 10)
            self.touch(processor, 1, 20)
            self.assertEqual(processor.active_keys, {0: "a", 1: "a"})
            self.assertEqual(processor.key_counts, {"a": 2})

            self.lift(processor, 0)
            release.assert_not_called()
            self.assertEqual(processor.active_keys, {1: "a"})
            self.assertEqual(processor.key_counts, {"a": 1})

            self.lift(processor, 1)

        press.assert_called_once_with("a")
        release.assert_called_once_with("a")
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})

    def test_sequential_moves_converge_and_diverge_shared_lanes(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press, mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.touch(processor, 0, 10)
            self.touch(processor, 1, 90)
            self.move(processor, 0, 50)
            self.move(processor, 1, 50)

            self.assertEqual(
                press.call_args_list,
                [mock.call("a"), mock.call("c"), mock.call("b")],
            )
            self.assertEqual(
                release.call_args_list,
                [mock.call("a"), mock.call("c")],
            )
            self.assertEqual(processor.active_keys, {0: "b", 1: "b"})
            self.assertEqual(processor.key_counts, {"b": 2})

            press.reset_mock()
            release.reset_mock()
            self.move(processor, 0, 10)

        press.assert_called_once_with("a")
        release.assert_not_called()
        self.assertEqual(processor.active_keys, {0: "a", 1: "b"})
        self.assertEqual(processor.key_counts, {"b": 1, "a": 1})

    def test_cleanup_releases_shared_lanes_once_each(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ):
            self.touch(processor, 0, 10)
            self.touch(processor, 1, 20)
            self.touch(processor, 2, 50)

        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            failures = processor.release_all_keys()
            self.assertEqual(processor.release_all_keys(), [])

        self.assertEqual(failures, [])
        self.assertEqual(
            release.call_args_list,
            [mock.call("a"), mock.call("b")],
        )
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})
        self.assertTrue(all(slot.tracking_id == -1 for slot in processor.slots.values()))

    def test_release_failure_does_not_prevent_other_lane_releases(self):
        processor = self.make_processor()
        processor.active_keys = {0: "a", 1: "b", 2: "c"}
        processor.key_counts = {"a": 1, "b": 1, "c": 1}
        with mock.patch.object(
            phone_trackpad,
            "release_key",
            side_effect=[RuntimeError("failed"), True, True],
        ) as release:
            failures = processor.release_all_keys()

        self.assertEqual(failures, ["a"])
        self.assertEqual(release.call_count, 3)
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})
        self.assertEqual(processor._possibly_held_keys, {"a"})

        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as retry:
            self.assertEqual(processor.release_all_keys(), [])

        retry.assert_called_once_with("a")
        self.assertEqual(processor._possibly_held_keys, set())

    def test_test_mode_uses_same_reference_counts_without_injection(self):
        processor = self.make_processor(test_mode=True)
        with mock.patch.object(phone_trackpad, "press_key") as press, \
                mock.patch.object(phone_trackpad, "release_key") as release, \
                contextlib.redirect_stdout(io.StringIO()):
            self.touch(processor, 0, 10)
            self.touch(processor, 1, 20)
            self.lift(processor, 0)
            self.assertEqual(processor.key_counts, {"a": 1})
            self.lift(processor, 1)
            processor.release_all_keys()

        press.assert_not_called()
        release.assert_not_called()
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})

    def test_unlocked_mode_tracks_touches_without_injection(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press, mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.stage_touch(processor, 0, 10)
            self.stage_touch(processor, 1, 20)
            self.sync(processor)
            self.assertEqual(processor.key_counts, {"a": 2})

            self.set_locked(processor, False)
            press.assert_called_once_with("a")
            release.assert_called_once_with("a")
            self.assertEqual(processor.active_keys, {})
            self.assertEqual(processor.key_counts, {})
            self.assertEqual(processor._possibly_held_keys, set())
            self.assertFalse(processor._previous_locked)
            self.assertTrue(all(
                not slot.changed for slot in processor.slots.values()
            ))

            press.reset_mock()
            release.reset_mock()
            self.stage_touch(processor, 2, 10, tracking_id=41)
            self.sync(processor)
            self.stage_move(processor, 2, 90)
            self.sync(processor)
            self.stage_lift(processor, 0)
            self.stage_lift(processor, 1)
            self.stage_lift(processor, 2)
            self.sync(processor)

        press.assert_not_called()
        release.assert_not_called()
        self.assertEqual(processor.slots[2].x, 90)
        self.assertTrue(all(
            slot.tracking_id == -1 and not slot.changed
            for slot in processor.slots.values()
        ))
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})

    def test_relock_suppresses_fingers_still_touching_until_fresh_touch(self):
        processor = self.make_processor()
        self.sync(processor)
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press:
            self.set_locked(processor, False)
            self.stage_touch(processor, 0, 10, tracking_id=41)
            self.sync(processor)

            self.set_locked(processor, True)
            self.assertTrue(processor.slots[0].blocked_until_lift)
            self.move(processor, 0, 50)
            press.assert_not_called()

            self.lift(processor, 0)
            self.assertFalse(processor.slots[0].blocked_until_lift)
            self.touch(processor, 0, 90, tracking_id=42)

        press.assert_called_once_with("c")
        self.assertEqual(processor.active_keys, {0: "c"})
        self.assertEqual(processor.key_counts, {"c": 1})

    def test_release_all_runs_once_on_unlock_not_each_unlocked_frame(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ), mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ), mock.patch.object(
            processor,
            "release_all_keys",
            wraps=processor.release_all_keys,
        ) as release_all:
            self.touch(processor, 0, 10)
            self.set_locked(processor, False)
            self.sync(processor)
            self.stage_move(processor, 0, 50)
            self.sync(processor)
            self.sync(processor)

        release_all.assert_called_once_with(
            preserve_touches=True,
            raise_on_failure=True,
        )

    def test_unlock_release_failure_attempts_all_and_remains_retryable(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ):
            self.stage_touch(processor, 0, 10, tracking_id=41)
            self.stage_touch(processor, 1, 50, tracking_id=42)
            self.sync(processor)

        release_error = RuntimeError("release a failed")
        processor.shared_config.update_from_phone({"locked": False})
        with mock.patch.object(
            phone_trackpad,
            "release_key",
            side_effect=[release_error, True],
        ) as release:
            with self.assertRaises(RuntimeError) as raised:
                self.sync(processor)

        self.assertIs(raised.exception, release_error)
        self.assertEqual(
            release.call_args_list,
            [mock.call("a"), mock.call("b")],
        )
        self.assertEqual(
            raised.exception.touch_failures,
            (("up", "a", release_error),),
        )
        self.assertFalse(processor._previous_locked)
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})
        self.assertEqual(processor._possibly_held_keys, {"a"})
        self.assertEqual(processor.slots[0].tracking_id, 41)
        self.assertEqual(processor.slots[1].tracking_id, 42)
        self.assertTrue(
            all(not slot.changed for slot in processor.slots.values()),
        )

        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as cleanup_release:
            self.assertEqual(processor.release_all_keys(), [])

        cleanup_release.assert_called_once_with("a")
        self.assertEqual(processor._possibly_held_keys, set())

    def test_atomic_frame_swaps_two_lanes_without_key_events(self):
        processor = self.make_processor(keys=("a", "b"))
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press, mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.touch(processor, 0, 10)
            self.touch(processor, 1, 90)
            press.reset_mock()
            release.reset_mock()

            self.stage_move(processor, 0, 90)
            self.stage_move(processor, 1, 10)
            self.sync(processor)

        press.assert_not_called()
        release.assert_not_called()
        self.assertEqual(processor.active_keys, {0: "b", 1: "a"})
        self.assertEqual(processor.key_counts, {"b": 1, "a": 1})

    def test_atomic_frames_converge_and_diverge_shared_lanes(self):
        processor = self.make_processor()
        events = []
        with mock.patch.object(
            phone_trackpad, "press_key",
            side_effect=lambda key: events.append(("down", key)) or True,
        ), mock.patch.object(
            phone_trackpad, "release_key",
            side_effect=lambda key: events.append(("up", key)) or True,
        ):
            self.touch(processor, 0, 10)
            self.touch(processor, 1, 90)
            events.clear()

            self.stage_move(processor, 0, 50)
            self.stage_move(processor, 1, 50)
            self.sync(processor)

            self.assertEqual(
                events,
                [("down", "b"), ("up", "a"), ("up", "c")],
            )
            self.assertEqual(processor.active_keys, {0: "b", 1: "b"})
            self.assertEqual(processor.key_counts, {"b": 2})

            events.clear()
            self.stage_move(processor, 0, 10)
            self.stage_move(processor, 1, 90)
            self.sync(processor)

        self.assertEqual(
            events,
            [("down", "a"), ("down", "c"), ("up", "b")],
        )
        self.assertEqual(processor.active_keys, {0: "a", 1: "c"})
        self.assertEqual(processor.key_counts, {"a": 1, "c": 1})

    def test_atomic_frame_lift_and_entry_keep_lane_held(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press, mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.touch(processor, 0, 10)
            press.reset_mock()
            release.reset_mock()

            self.stage_lift(processor, 0)
            self.stage_touch(processor, 1, 20)
            self.sync(processor)

        press.assert_not_called()
        release.assert_not_called()
        self.assertEqual(processor.active_keys, {1: "a"})
        self.assertEqual(processor.key_counts, {"a": 1})

    def test_atomic_frame_uses_final_updates_and_ignores_stale_releases(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press, mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.touch(processor, 0, 10)
            press.reset_mock()
            release.reset_mock()

            self.stage_move(processor, 0, 90)
            self.stage_move(processor, 0, 10)
            self.stage_lift(processor, 7)
            self.stage_lift(processor, 7)
            self.sync(processor)
            release.assert_not_called()
            self.assertEqual(processor.active_keys, {0: "a"})
            self.assertEqual(processor.key_counts, {"a": 1})

            self.stage_lift(processor, 0)
            self.stage_lift(processor, 0)
            self.stage_lift(processor, 8)
            self.sync(processor)

            self.stage_lift(processor, 0)
            self.stage_lift(processor, 8)
            self.sync(processor)

        press.assert_not_called()
        release.assert_called_once_with("a")
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})
        self.assertTrue(
            all(count >= 0 for count in processor.key_counts.values()),
        )

    def test_atomic_frame_injection_failure_keeps_cleanup_recoverable(self):
        processor = self.make_processor()
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ), mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ):
            self.touch(processor, 0, 10)
            self.touch(processor, 1, 90)

        self.stage_move(processor, 0, 50)
        self.stage_move(processor, 1, 50)
        release_error = RuntimeError("release a failed")
        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ) as press, mock.patch.object(
            phone_trackpad, "release_key",
            side_effect=[release_error, True],
        ) as release:
            with self.assertRaises(RuntimeError) as raised:
                self.sync(processor)

        self.assertIs(raised.exception, release_error)
        press.assert_called_once_with("b")
        self.assertEqual(
            release.call_args_list,
            [mock.call("a"), mock.call("c")],
        )
        self.assertEqual(processor.active_keys, {})
        self.assertEqual(processor.key_counts, {})
        self.assertEqual(processor._possibly_held_keys, {"a", "b"})
        self.assertTrue(
            all(slot.tracking_id == -1 for slot in processor.slots.values()),
        )

        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as cleanup_release:
            failures = processor.release_all_keys()

        self.assertEqual(failures, [])
        self.assertEqual(
            cleanup_release.call_args_list,
            [mock.call("a"), mock.call("b")],
        )
        self.assertEqual(processor.key_counts, {})
        self.assertEqual(processor._possibly_held_keys, set())


class PhoneSettingsBackupTests(unittest.TestCase):
    def make_backup(self, originals):
        backup = PhoneSettingsBackup()
        backup._snapshotted = True
        backup._originals = dict(originals)
        return backup

    @staticmethod
    def all_originals():
        return {
            ("system", "screen_off_timeout"): "60000",
            ("global", "stay_on_while_plugged_in"): "0",
            ("global", "policy_control"): None,
            ("global", "zen_mode"): "0",
        }

    def test_ctrl_c_after_modified_setting_restores_pending_values(self):
        backup = self.make_backup(self.all_originals())
        screen = ("system", "screen_off_timeout")
        stay = ("global", "stay_on_while_plugged_in")

        with mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=["", KeyboardInterrupt],
        ) as checked:
            with self.assertRaises(KeyboardInterrupt):
                backup.apply_overrides()

        self.assertEqual(checked.call_count, 2)
        self.assertEqual(backup.pending_settings, {screen, stay})

        with mock.patch.object(
            phone_trackpad, "run_adb_checked", return_value="",
        ) as restored:
            self.assertEqual(backup.restore(), [])

        self.assertEqual(
            restored.call_args_list,
            [
                mock.call(
                    "shell", "settings", "put", "system",
                    "screen_off_timeout", "60000", timeout=5,
                ),
                mock.call(
                    "shell", "settings", "put", "global",
                    "stay_on_while_plugged_in", "0", timeout=5,
                ),
            ],
        )
        self.assertEqual(backup.pending_settings, set())

    def test_ordinary_override_failure_is_collected_and_processing_continues(self):
        backup = self.make_backup(self.all_originals())
        with mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=[RuntimeError("ordinary failure"), "", "", ""],
        ) as checked:
            failures = backup.apply_overrides()

        self.assertEqual(len(failures), 1)
        self.assertIn("unexpectedly", failures[0].detail)
        self.assertEqual(checked.call_count, 4)
        self.assertEqual(
            backup.pending_settings,
            set(self.all_originals()),
        )

    def test_cleanup_failure_does_not_replace_ctrl_c(self):
        screen = ("system", "screen_off_timeout")
        backup = self.make_backup({screen: "60000"})
        restore_failure = AdbCommandError((), "restore failed")
        cleanup_failures = []

        with mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=[KeyboardInterrupt, restore_failure],
        ):
            with self.assertRaises(KeyboardInterrupt):
                try:
                    backup.apply_overrides()
                finally:
                    cleanup_failures.extend(backup.restore())

        self.assertEqual(len(cleanup_failures), 1)
        self.assertEqual(backup.pending_settings, {screen})

    def test_main_outer_finally_restores_after_ctrl_c_override(self):
        session = AdbCleanupSession()
        originals = iter(("60000", "0", None, "0"))
        with mock.patch.object(
            phone_trackpad.sys,
            "argv",
            ["phone_trackpad.py", "--transport", "adb", "--no-ui"],
        ), mock.patch.object(
            phone_trackpad, "load_config",
            return_value={"device": "/dev/input/event1", "max_x": 100, "max_y": 100},
        ), mock.patch.object(
            phone_trackpad, "save_config",
        ), mock.patch.object(
            phone_trackpad, "check_device_connected", return_value=True,
        ), mock.patch.object(
            phone_trackpad, "AdbCleanupSession", return_value=session,
        ), mock.patch.object(
            phone_trackpad,
            "_get_setting",
            side_effect=lambda *_: next(originals),
        ), mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=[KeyboardInterrupt, ""],
        ) as checked, contextlib.redirect_stdout(io.StringIO()):
            phone_trackpad.main()

        self.assertEqual(
            checked.call_args_list,
            [
                mock.call(
                    "shell", "settings", "put", "system",
                    "screen_off_timeout", "2147483647", timeout=5,
                ),
                mock.call(
                    "shell", "settings", "put", "system",
                    "screen_off_timeout", "60000", timeout=5,
                ),
            ],
        )
        self.assertEqual(session.settings_backup.pending_settings, set())

    def test_known_dnd_modes_use_consistent_command_path(self):
        setting = ("global", "zen_mode")
        for original, expected in (
            ("0", "off"),
            ("1", "priority"),
            ("2", "none"),
            ("3", "alarms"),
        ):
            with self.subTest(original=original):
                backup = self.make_backup({setting: original})
                with mock.patch.object(
                    phone_trackpad, "run_adb_checked", return_value="",
                ) as checked:
                    self.assertEqual(backup.apply_overrides(), [])
                    self.assertEqual(backup.restore(), [])

                self.assertEqual(
                    checked.call_args_list,
                    [
                        mock.call(
                            "shell", "cmd", "notification", "set_dnd",
                            "priority", timeout=5,
                        ),
                        mock.call(
                            "shell", "cmd", "notification", "set_dnd",
                            expected, timeout=5,
                        ),
                    ],
                )

    def test_unknown_dnd_mode_is_never_modified(self):
        setting = ("global", "zen_mode")
        backup = self.make_backup({setting: "vendor-mode"})

        with mock.patch.object(phone_trackpad, "run_adb_checked") as checked:
            failures = backup.apply_overrides()
            restored = backup.restore()

        self.assertEqual(len(failures), 1)
        self.assertIn("unknown", failures[0].detail)
        self.assertEqual(restored, [])
        checked.assert_not_called()
        self.assertEqual(backup.pending_settings, set())

    def test_failed_dnd_read_does_not_block_other_settings(self):
        values = iter(("60000", "0", None, phone_trackpad._SETTING_READ_FAILED))
        backup = PhoneSettingsBackup()

        with mock.patch.object(
            phone_trackpad, "_get_setting", side_effect=lambda *_: next(values),
        ):
            read_failures = backup.snapshot()
        with mock.patch.object(
            phone_trackpad, "run_adb_checked", return_value="",
        ) as checked:
            apply_failures = backup.apply_overrides()

        self.assertEqual(len(read_failures), 1)
        self.assertEqual(apply_failures, [])
        self.assertEqual(checked.call_count, 3)
        self.assertFalse(any(
            "notification" in call.args for call in checked.call_args_list
        ))

    def test_failed_write_is_checked_and_retained_for_restore(self):
        setting = ("system", "screen_off_timeout")
        backup = self.make_backup({setting: "60000"})
        failure = AdbCommandError((), "command exited with status 1")

        with mock.patch.object(
            phone_trackpad, "run_adb_checked", side_effect=failure,
        ):
            failures = backup.apply_overrides()

        self.assertEqual(len(failures), 1)
        self.assertIn(setting, backup.pending_settings)
        self.assertEqual(
            failures[0].recovery_command,
            "adb shell settings put system screen_off_timeout 60000",
        )

    def test_partial_restore_attempts_all_and_can_be_retried(self):
        screen = ("system", "screen_off_timeout")
        stay = ("global", "stay_on_while_plugged_in")
        policy = ("global", "policy_control")
        backup = self.make_backup({
            screen: "60000",
            stay: "0",
            policy: None,
        })

        with mock.patch.object(
            phone_trackpad, "run_adb_checked", return_value="",
        ):
            backup.apply_overrides()

        failure = AdbCommandError((), "command timed out")
        with mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=[failure, "", ""],
        ) as checked:
            first = backup.restore()

        self.assertEqual(checked.call_count, 3)
        self.assertEqual(len(first), 1)
        self.assertEqual(backup.pending_settings, {screen})

        with mock.patch.object(
            phone_trackpad, "run_adb_checked", return_value="",
        ) as checked:
            second = backup.restore()

        self.assertEqual(second, [])
        checked.assert_called_once_with(
            "shell", "settings", "put", "system",
            "screen_off_timeout", "60000", timeout=5,
        )
        self.assertEqual(backup.pending_settings, set())


class CleanupSessionTests(unittest.TestCase):
    def test_cleanup_order_is_explicit(self):
        order = []
        session = AdbCleanupSession()
        session.input_transport = mock.Mock()
        session.input_transport.stop_input_processing.side_effect = (
            lambda: order.append("stop")
        )
        session.input_transport.terminate.side_effect = (
            lambda: order.append("terminate") or []
        )
        session.processor = mock.Mock()
        session.processor.release_all_keys.side_effect = (
            lambda: order.append("release") or []
        )
        session.reverse_mapping = mock.Mock()
        session.reverse_mapping.remove.side_effect = (
            lambda: order.append("reverse") or []
        )
        session.settings_backup = mock.Mock()
        session.settings_backup.restore.side_effect = (
            lambda: order.append("settings") or []
        )

        self.assertEqual(session.cleanup(), [])
        self.assertEqual(
            order,
            ["stop", "release", "terminate", "reverse", "settings"],
        )

    def test_cleanup_failure_does_not_hide_original_exception(self):
        session = AdbCleanupSession()
        session.input_transport.terminate = mock.Mock(
            side_effect=RuntimeError("cleanup exploded"),
        )
        captured = []

        with self.assertRaisesRegex(RuntimeError, "original failure"):
            try:
                raise RuntimeError("original failure")
            finally:
                captured.extend(session.cleanup())

        self.assertTrue(any(
            failure.component == "ADB input stream" for failure in captured
        ))

    def test_repeated_cleanup_retries_only_incomplete_resources(self):
        session = AdbCleanupSession()
        session.reverse_mapping._owned = True
        session.settings_backup._pending_restore = {
            ("system", "screen_off_timeout"),
        }
        session.settings_backup._originals = {
            ("system", "screen_off_timeout"): "60000",
        }
        failure = AdbCommandError((), "offline")

        with mock.patch.object(
            phone_trackpad,
            "run_adb_checked",
            side_effect=[failure, failure, "", ""],
        ):
            first = session.cleanup()
            second = session.cleanup()

        self.assertEqual(len(first), 2)
        self.assertEqual(second, [])
        self.assertFalse(session.reverse_mapping.owned)
        self.assertEqual(session.settings_backup.pending_settings, set())

    def test_restoration_warning_names_setting_and_safe_command(self):
        failure = CleanupFailure(
            "screen timeout",
            "could not be restored: command timed out",
            "adb shell settings put system screen_off_timeout 60000",
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            phone_trackpad.report_failures("Cleanup incomplete", [failure])

        rendered = output.getvalue()
        self.assertIn("screen timeout", rendered)
        self.assertIn("adb shell settings put", rendered)
        self.assertNotIn("private", rendered)

    def test_main_cleanup_error_does_not_hide_stream_error(self):
        session = mock.Mock()
        session.settings_backup.snapshot.return_value = []
        session.controller_started = False
        session.cleanup.side_effect = RuntimeError("cleanup failure")
        processor = mock.Mock()
        processor.stats = {
            "presses": 0, "releases": 0, "drags": 0,
        }

        with mock.patch.object(
            phone_trackpad.sys,
            "argv",
            ["phone_trackpad.py", "--transport", "adb", "--no-ui"],
        ), mock.patch.object(
            phone_trackpad, "load_config",
            return_value={"device": "/dev/input/event1", "max_x": 100, "max_y": 100},
        ), mock.patch.object(
            phone_trackpad, "save_config",
        ), mock.patch.object(
            phone_trackpad, "check_device_connected", return_value=True,
        ), mock.patch.object(
            phone_trackpad, "AdbCleanupSession", return_value=session,
        ), mock.patch.object(
            phone_trackpad, "prevent_interruptions", return_value=[],
        ), mock.patch.object(
            phone_trackpad, "TouchProcessor", return_value=processor,
        ), mock.patch.object(
            phone_trackpad, "print_banner",
        ), mock.patch.object(
            phone_trackpad,
            "stream_events",
            side_effect=RuntimeError("original stream failure"),
        ), mock.patch.object(
            phone_trackpad, "report_failures",
        ):
            with contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(
                    RuntimeError, "original stream failure",
                ):
                    phone_trackpad.main()

        session.cleanup.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
