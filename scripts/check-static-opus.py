#!/usr/bin/env python3
"""Fail when a plugin library loads libopus from outside itself.

On Windows and macOS the IAMF plugin carries libopus inside it, built from
source and linked statically (scripts/build-static-opus.sh), so that it is
one file with nothing to install beside it. A build that picked up a shared
libopus instead still links, and runs wherever that library happens to be;
only its imports show it. This reads them and exits non-zero if one names
libopus:

    check-static-opus.py LIBRARY...

PE files (.dll) are read with the import-table reader of Omniphony's
scripts/check_windows_crt_imports.py, from the sibling checkout the bridge
builds against; Mach-O files (.dylib) with `otool -L`.
"""

from __future__ import annotations

import pathlib
import subprocess
import sys


def pe_imports(path: pathlib.Path) -> list[str]:
    scripts = pathlib.Path(__file__).resolve().parents[2] / "Omniphony" / "scripts"
    sys.path.insert(0, str(scripts))
    try:
        from check_windows_crt_imports import imported_dlls  # type: ignore
    except ImportError as error:
        raise SystemExit(f"{scripts}: no check_windows_crt_imports.py ({error})")
    return imported_dlls(path.read_bytes())


def macho_imports(path: pathlib.Path) -> list[str]:
    output = subprocess.run(
        ["otool", "-L", str(path)], check=True, capture_output=True, text=True
    ).stdout
    # The first line names the file itself.
    return [line.strip().split(" ")[0] for line in output.splitlines()[1:]]


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    failed = False
    for name in argv:
        path = pathlib.Path(name)
        magic = path.read_bytes()[:4]
        if magic[:2] == b"MZ":
            imports = pe_imports(path)
        elif magic in (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe"):
            imports = macho_imports(path)
        else:
            print(f"{path}: neither a PE nor a Mach-O file", file=sys.stderr)
            return 2
        shared = [i for i in imports if "opus" in i.lower()]
        if shared:
            print(f"{path}: loads libopus from outside: {', '.join(shared)}", file=sys.stderr)
            failed = True
        else:
            print(f"{path}: libopus is linked in ({len(imports)} imports, none of it)")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
