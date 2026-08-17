import ctypes
import sys
import unittest
from unittest import mock

from winusb_transport import (
    ERROR_IO_PENDING,
    WAIT_OBJECT_0,
    WAIT_TIMEOUT,
    WinUsbConnection,
    WinUsbError,
)


class FakeKernel:
    def __init__(self):
        self.next_event = 100
        self.closed = []
        self.cancelled = []
        self.wait_results = []

    def CreateEventW(self, _attributes, _manual_reset, _initial, _name):
        event = self.next_event
        self.next_event += 1
        return event

    def ResetEvent(self, _event):
        return True

    def WaitForSingleObject(self, _event, _timeout):
        if self.wait_results:
            return self.wait_results.pop(0)
        return WAIT_OBJECT_0

    def CancelIoEx(self, _file_handle, overlapped):
        self.cancelled.append(
            None if overlapped is None else ctypes.addressof(overlapped._obj)
        )
        return True

    def CloseHandle(self, handle):
        self.closed.append(handle)
        return True


class FakeWinUsb:
    def __init__(self):
        self.payloads = [
            b"first",
            b"second",
            b"third",
            b"fourth",
            b"fifth",
            b"sixth",
        ]
        self.results = {}
        self.buffer_addresses = []
        self.freed = []
        self.pending = False

    def WinUsb_SetPipePolicy(self, *_args):
        return True

    def WinUsb_ReadPipe(
        self,
        _interface,
        _endpoint,
        buffer,
        _size,
        _transferred,
        overlapped,
    ):
        payload = self.payloads.pop(0)
        for index, value in enumerate(payload):
            buffer[index] = value
        overlap_address = ctypes.addressof(overlapped._obj)
        self.results[overlap_address] = len(payload)
        self.buffer_addresses.append(ctypes.addressof(buffer))
        return not self.pending

    def WinUsb_GetOverlappedResult(
        self, _interface, overlapped, transferred, _wait
    ):
        overlap_address = ctypes.addressof(overlapped._obj)
        transferred._obj.value = self.results[overlap_address]
        return True

    def WinUsb_Free(self, interface):
        self.freed.append(interface)
        return True


class FakeApi:
    def __init__(self):
        self.kernel = FakeKernel()
        self.winusb = FakeWinUsb()

    @staticmethod
    def last_error(operation, code=None):
        return WinUsbError(f"{operation}: {code}")


class WinUsbReadPipelineTests(unittest.TestCase):
    @staticmethod
    def make_connection(
        read_depth=None,
        pending=False,
        wait_results=(),
    ):
        api = FakeApi()
        api.winusb.pending = pending
        api.kernel.wait_results = list(wait_results)
        options = {}
        if read_depth is not None:
            options["read_depth"] = read_depth
        connection = WinUsbConnection(
            api=api,
            file_handle=1,
            interface_handle=ctypes.c_void_p(2),
            endpoint_in=0x81,
            endpoint_out=0x01,
            **options,
        )
        return api, connection

    def test_two_ordered_reads_reuse_the_same_two_buffers(self):
        api, connection = self.make_connection(read_depth=2)

        self.assertEqual(connection.read(), b"first")
        self.assertEqual(connection.read(), b"second")
        self.assertEqual(connection.read(), b"third")

        addresses = api.winusb.buffer_addresses
        self.assertNotEqual(addresses[0], addresses[1])
        self.assertEqual(addresses[0], addresses[2])
        self.assertEqual(addresses[1], addresses[3])
        self.assertEqual(addresses[0], addresses[4])

        connection.close()
        self.assertEqual(len(api.kernel.closed), 3)
        self.assertEqual(len(api.winusb.freed), 1)

    @unittest.skipUnless(sys.platform == "win32", "ctypes.get_last_error is Windows-only")
    def test_pending_read_timeout_keeps_request_posted(self):
        api, connection = self.make_connection(
            read_depth=2,
            pending=True,
            wait_results=(WAIT_TIMEOUT, WAIT_OBJECT_0),
        )

        with mock.patch(
            "winusb_transport.ctypes.get_last_error",
            return_value=ERROR_IO_PENDING,
        ):
            self.assertEqual(connection.read(), b"")
            self.assertEqual(len(api.winusb.buffer_addresses), 2)
            self.assertEqual(connection.read(), b"first")
            self.assertEqual(len(api.winusb.buffer_addresses), 3)
            connection.close()

        self.assertEqual(len(api.kernel.cancelled), 2)

    def test_cancel_pending_read_cancels_all_file_io(self):
        api, connection = self.make_connection()

        connection.cancel_pending_read()
        connection.cancel_pending_read()

        self.assertEqual(api.kernel.cancelled, [None, None])
        connection.close()


if __name__ == "__main__":
    unittest.main()
