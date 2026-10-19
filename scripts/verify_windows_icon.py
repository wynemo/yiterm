#!/usr/bin/env python3
"""On Windows, verify the executable embeds the project's exact icon images."""
import ctypes
from ctypes import wintypes
from pathlib import Path
import struct
import sys


def main() -> None:
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.LoadLibraryExW.argtypes = [wintypes.LPCWSTR, wintypes.HANDLE, wintypes.DWORD]
    kernel.LoadLibraryExW.restype = wintypes.HMODULE
    kernel.FindResourceW.argtypes = [wintypes.HMODULE, ctypes.c_void_p, ctypes.c_void_p]
    kernel.FindResourceW.restype = wintypes.HANDLE
    kernel.SizeofResource.argtypes = [wintypes.HMODULE, wintypes.HANDLE]
    kernel.SizeofResource.restype = wintypes.DWORD
    kernel.LoadResource.argtypes = [wintypes.HMODULE, wintypes.HANDLE]
    kernel.LoadResource.restype = wintypes.HANDLE
    kernel.LockResource.argtypes = [wintypes.HANDLE]
    kernel.LockResource.restype = ctypes.c_void_p
    kernel.FreeLibrary.argtypes = [wintypes.HMODULE]

    module = kernel.LoadLibraryExW(str(Path(sys.argv[1]).resolve()), None, 0x22)
    if not module:
        raise ctypes.WinError(ctypes.get_last_error())

    def resource(identifier: int, kind: int) -> bytes:
        handle = kernel.FindResourceW(module, identifier, kind)
        if not handle:
            raise ctypes.WinError(ctypes.get_last_error())
        size = kernel.SizeofResource(module, handle)
        pointer = kernel.LockResource(kernel.LoadResource(module, handle))
        if not size or not pointer:
            raise ctypes.WinError(ctypes.get_last_error())
        return ctypes.string_at(pointer, size)

    try:
        source = (Path(__file__).resolve().parents[1] / "assets/icons/yiterm.ico").read_bytes()
        group = resource(1, 14)  # GPUI loads RT_GROUP_ICON ID 1.
        count = struct.unpack_from("<H", source, 4)[0]
        assert group[:6] == source[:6], "Icon group header does not match"
        for index in range(count):
            original = struct.unpack_from("<BBBBHHII", source, 6 + 16 * index)
            embedded = struct.unpack_from("<BBBBHHIH", group, 6 + 14 * index)
            assert original[:7] == embedded[:7], "Icon dimensions or format do not match"
            assert resource(embedded[7], 3) == source[original[7]:original[7] + original[6]], "Icon image does not match"
        print(f"Verified {count} YiTerm icon images in {sys.argv[1]}")
    finally:
        kernel.FreeLibrary(module)


if __name__ == "__main__":
    main()
