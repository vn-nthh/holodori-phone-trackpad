import contextlib
import io
import pathlib
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
    def test_checked_adb_rejects_nonzero_exit(self):
        with mock.patch.object(
            phone_trackpad.subprocess,
            "run",
            return_value=completed(returncode=17, stderr="private detail"),
        ):
            with self.assertRaises(AdbCommandError) as raised:
                phone_trackpad.run_adb_checked(
                    "shell", "settings", "put", "system", "x", "y",
                )

        self.assertEqual(raised.exception.reason, "command exited with status 17")
        self.assertNotIn("private detail", str(raised.exception))

    def test_checked_adb_rejects_timeout(self):
        with mock.patch.object(
            phone_trackpad.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired("adb", 5),
        ):
            with self.assertRaisesRegex(AdbCommandError, "timed out"):
                phone_trackpad.run_adb_checked("reverse", "--list", timeout=5)

    def test_checked_adb_rejects_reported_command_failure(self):
        with mock.patch.object(
            phone_trackpad.subprocess,
            "run",
            return_value=completed(stderr="Error: permission denied"),
        ):
            with self.assertRaisesRegex(AdbCommandError, "rejected"):
                phone_trackpad.run_adb_checked(
                    "shell", "settings", "put", "global", "x", "y",
                )


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

    def test_cleanup_never_uses_remove_all(self):
        forbidden = "--remove-" + "all"
        source = pathlib.Path(phone_trackpad.__file__).read_text(
            encoding="utf-8",
        )
        self.assertNotIn(forbidden, source)

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

    def test_setup_failure_after_creation_removes_owned_mapping(self):
        mapping = AdbReverseMapping(phone_trackpad.SERVER_PORT)
        with mock.patch.object(
            phone_trackpad, "run_adb_checked", return_value="",
        ) as checked:
            try:
                mapping.create()
                raise RuntimeError("later setup failure")
            except RuntimeError:
                failures = mapping.remove()

        self.assertEqual(failures, [])
        self.assertFalse(mapping.owned)
        self.assertEqual(checked.call_args[0][1], "--remove")

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


class KeyReleaseTests(unittest.TestCase):
    def make_processor(self, test_mode=False):
        return TouchProcessor(["a", "b", "c"], 100, 100, test_mode=test_mode)

    def test_one_held_key_released_at_shutdown(self):
        processor = self.make_processor()
        processor.active_keys = {0: "a"}
        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.assertEqual(processor.release_all_keys(), [])

        release.assert_called_once_with("a")
        self.assertEqual(processor.active_keys, {0: None})

    def test_several_held_keys_are_all_released(self):
        processor = self.make_processor()
        processor.active_keys = {0: "a", 1: "b", 2: "c"}
        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            self.assertEqual(processor.release_all_keys(), [])

        self.assertEqual(
            release.call_args_list,
            [mock.call("a"), mock.call("b"), mock.call("c")],
        )

    def test_duplicate_cleanup_is_safe(self):
        processor = self.make_processor()
        processor.active_keys = {0: "a"}
        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            processor.release_all_keys()
            processor.release_all_keys()

        release.assert_called_once_with("a")

    def test_one_release_failure_does_not_prevent_others(self):
        processor = self.make_processor()
        processor.active_keys = {0: "a", 1: "b", 2: "c"}
        with mock.patch.object(
            phone_trackpad,
            "release_key",
            side_effect=[RuntimeError("failed"), True, True],
        ) as release:
            failures = processor.release_all_keys()

        self.assertEqual(failures, ["a"])
        self.assertEqual(release.call_count, 3)
        self.assertTrue(all(value is None for value in processor.active_keys.values()))
        self.assertEqual(processor._held_keys, {"a"})

    def test_failed_release_is_retried_by_repeated_cleanup(self):
        processor = self.make_processor()
        processor.active_keys = {0: "a"}
        with mock.patch.object(
            phone_trackpad, "release_key", side_effect=[False, True],
        ) as release:
            self.assertEqual(processor.release_all_keys(), ["a"])
            self.assertEqual(processor.release_all_keys(), [])

        self.assertEqual(release.call_count, 2)
        self.assertEqual(processor._held_keys, set())

    def test_test_mode_clears_state_without_injection(self):
        processor = self.make_processor(test_mode=True)
        processor.active_keys = {0: "a", 1: "b"}
        with mock.patch.object(phone_trackpad, "release_key") as release:
            self.assertEqual(processor.release_all_keys(), [])

        release.assert_not_called()
        self.assertTrue(all(value is None for value in processor.active_keys.values()))

    def test_disconnect_during_active_multitouch_releases_every_key(self):
        config = SharedConfig(["a", "b"])
        config.update_from_phone({"locked": True})
        processor = TouchProcessor(
            ["a", "b"], 100, 100, shared_config=config,
        )

        with mock.patch.object(
            phone_trackpad, "press_key", return_value=True,
        ):
            for slot, tracking_id, x in ((0, 10, 10), (1, 11, 90)):
                processor.process_event(EV_ABS, ABS_MT_SLOT, slot)
                processor.process_event(
                    EV_ABS, ABS_MT_TRACKING_ID, tracking_id,
                )
                processor.process_event(EV_ABS, ABS_MT_POSITION_X, x)
                processor.process_event(EV_ABS, ABS_MT_POSITION_Y, 50)
                processor.process_event(EV_SYN, SYN_REPORT, 0)

        session = AdbCleanupSession()
        session.processor = processor
        disconnected = mock.Mock()
        disconnected.poll.return_value = 0
        session.input_transport.proc = disconnected

        with mock.patch.object(
            phone_trackpad, "release_key", return_value=True,
        ) as release:
            failures = session.cleanup()

        self.assertEqual(failures, [])
        self.assertCountEqual(
            [call.args[0] for call in release.call_args_list],
            ["a", "b"],
        )
        self.assertTrue(all(value is None for value in processor.active_keys.values()))


class PhoneSettingsBackupTests(unittest.TestCase):
    def make_backup(self, originals):
        backup = PhoneSettingsBackup()
        backup._snapshotted = True
        backup._originals = dict(originals)
        return backup

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
