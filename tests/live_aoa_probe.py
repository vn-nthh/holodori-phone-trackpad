"""Live AOA heartbeat probe for an attached Android accessory."""

import argparse
import time

from aoa_transport import ACTION_HEARTBEAT, AoaHost, TouchPacketParser


parser_cli = argparse.ArgumentParser()
parser_cli.add_argument(
    "--usbdk",
    action="store_true",
    help="Use UsbDk for the normal-device AOA handshake",
)
parser_cli.add_argument("--duration", type=float, default=6.0)
args = parser_cli.parse_args()

host = AoaHost(use_usbdk=args.usbdk)
print(f"handshake_usbdk={host.usb.using_usbdk}", flush=True)
connection = host.connect()
parser = TouchPacketParser()
heartbeats = 0
touches = 0
deadline = time.monotonic() + max(1.0, args.duration)

try:
    print(
        f"connected in=0x{connection.endpoint_in:02x} "
        f"out=0x{connection.endpoint_out:02x}",
        flush=True,
    )
    while time.monotonic() < deadline:
        for event in parser.feed(connection.read(timeout_ms=500)):
            if event.action == ACTION_HEARTBEAT:
                heartbeats += 1
            else:
                touches += 1
finally:
    connection.close()
    host.close()

print(f"heartbeats={heartbeats} touches={touches}", flush=True)
raise SystemExit(0 if heartbeats >= 5 else 3)
