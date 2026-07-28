"""Authoritative application metadata shared by runtime diagnostics."""

from __future__ import annotations

import re
import sys
from pathlib import Path


_VERSION_PATTERN = re.compile(r"\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?")


def _version_file() -> Path:
    frozen_root = getattr(sys, "_MEIPASS", None)
    if frozen_root:
        return Path(frozen_root) / "VERSION"
    return Path(__file__).resolve().with_name("VERSION")


def _read_app_version() -> str:
    version = _version_file().read_text(encoding="ascii").strip()
    if not _VERSION_PATTERN.fullmatch(version):
        raise RuntimeError("VERSION does not contain a valid application version")
    return version


APP_VERSION = _read_app_version()
