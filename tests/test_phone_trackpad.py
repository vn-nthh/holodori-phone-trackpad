import subprocess
import unittest
from unittest import mock

import phone_trackpad
from phone_trackpad import PhoneSettingsBackup


class PhoneSettingsBackupTests(unittest.TestCase):
    def test_snapshot_aborts_when_a_setting_cannot_be_read(self):
        completed = subprocess.CompletedProcess(
            args=[], returncode=1, stdout="", stderr="device offline",
        )

        with mock.patch.object(
            phone_trackpad.subprocess, "run", return_value=completed,
        ):
            backup = PhoneSettingsBackup()
            self.assertFalse(backup.snapshot())

        self.assertFalse(backup._taken)
        self.assertEqual(backup._originals, {})

    def test_restore_uses_supported_total_silence_argument(self):
        backup = PhoneSettingsBackup()
        backup._originals = {("global", "zen_mode"): "2"}
        backup._taken = True

        with mock.patch.object(phone_trackpad, "run_adb") as run_adb:
            backup.restore()

        run_adb.assert_called_once_with(
            "shell", "cmd", "notification", "set_dnd", "none", timeout=5,
        )

    def test_restore_returns_unset_zen_mode_to_unset(self):
        backup = PhoneSettingsBackup()
        backup._originals = {("global", "zen_mode"): None}
        backup._taken = True

        with mock.patch.object(phone_trackpad, "run_adb") as run_adb:
            backup.restore()

        self.assertEqual(
            run_adb.call_args_list,
            [
                mock.call(
                    "shell", "cmd", "notification", "set_dnd", "off",
                    timeout=5,
                ),
                mock.call(
                    "shell", "settings", "delete", "global", "zen_mode",
                    timeout=5,
                ),
            ],
        )

    def test_restore_preserves_unknown_oem_zen_mode(self):
        backup = PhoneSettingsBackup()
        backup._originals = {("global", "zen_mode"): "vendor-mode"}
        backup._taken = True

        with mock.patch.object(phone_trackpad, "run_adb") as run_adb:
            backup.restore()

        run_adb.assert_called_once_with(
            "shell", "settings", "put", "global", "zen_mode", "vendor-mode",
            timeout=5,
        )

    def test_cleanup_without_backup_only_removes_reverse_tunnel(self):
        with mock.patch.object(phone_trackpad, "run_adb") as run_adb:
            phone_trackpad.restore_phone()

        run_adb.assert_called_once_with("reverse", "--remove-all")

    def test_cleanup_removes_reverse_tunnel_when_restore_fails(self):
        backup = mock.Mock()
        backup.restore.side_effect = RuntimeError("restore failed")

        with mock.patch.object(phone_trackpad, "run_adb") as run_adb:
            with self.assertRaisesRegex(RuntimeError, "restore failed"):
                phone_trackpad.restore_phone(backup)

        run_adb.assert_called_once_with("reverse", "--remove-all")


if __name__ == "__main__":
    unittest.main()
