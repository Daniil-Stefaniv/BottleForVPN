#!/usr/bin/env python3
"""Минимальный cpio-архиватор формата newc (070701).

Системный cpio на macOS пишет odc (0707) — Linux initramfs требует
newc. Каталоги/файлы/симлинки + статические узлы устройств (initramfs
без devtmpfs-автомонтирования).
"""

import os
import sys

DEVICE_NODES: dict[str, tuple[int, int]] = {
    # initramfs: ядро не монтирует devtmpfs само — статические узлы,
    # без которых busybox init и respawn-процессы не стартуют.
    "/dev/console": (5, 1),
    "/dev/null": (1, 3),
    "/dev/zero": (1, 5),
    "/dev/full": (1, 7),
    "/dev/random": (1, 8),
    "/dev/urandom": (1, 9),
    "/dev/tty": (5, 0),
    "/dev/ptmx": (5, 2),
}

# (имя, mode, uid, gid, данные|None, (rdev_major, rdev_minor))
Entry = tuple[str, int, int, int, bytes | None, tuple[int, int]]


def cpio_newc(tree: str, out: str) -> None:
    entries: list[Entry] = []
    for root, dirs, files in os.walk(tree, followlinks=False):
        for name in sorted(dirs):
            path = os.path.join(root, name)
            if os.path.islink(path):
                # симлинк на каталог — файловая запись с целью.
                entries.append(file_entry(path, tree))
            else:
                entries.append((archive_name(path, tree), 0o040755, 0, 0, None, (0, 0)))
        for name in sorted(files):
            path = os.path.join(root, name)
            entries.append(file_entry(path, tree))
    for name, (major, minor) in DEVICE_NODES.items():
        if not os.path.lexists(tree + name):
            entries.append((name, 0o020600, 0, 0, None, (major, minor)))

    with open(out, "wb") as fh:
        for name, mode, uid, gid, data, rdev in entries:
            fh.write(header(name, mode, uid, gid, data, rdev))
            if data is not None:
                fh.write(data)
                if len(data) % 4:
                    fh.write(b"\0" * (4 - len(data) % 4))
        fh.write(header("TRAILER!!!", 0, 0, 0, None, (0, 0)))


def archive_name(path: str, tree: str) -> str:
    rel = os.path.relpath(path, tree)
    return "/" if rel == "." else "/" + rel


def file_entry(path: str, tree: str) -> Entry:
    if os.path.islink(path):
        target = os.readlink(path).encode()
        return (archive_name(path, tree), 0o120777, 0, 0, target, (0, 0))
    st = os.lstat(path)
    with open(path, "rb") as fh:
        data = fh.read()
    return (archive_name(path, tree), 0o100000 | (st.st_mode & 0o777), 0, 0, data, (0, 0))


def header(
    name: str,
    mode: int,
    uid: int,
    gid: int,
    data: bytes | None,
    rdev: tuple[int, int],
) -> bytes:
    size = 0 if data is None else len(data)
    names = name.encode() + b"\0"
    # newc: 6-байтная магия «070701» + 13 полей по 8 hex-символов
    # (ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor,
    # rdevmajor, rdevminor, namesize, check).
    fields = [
        0,         # ino
        mode,
        uid,
        gid,
        1,         # nlink
        0,         # mtime
        size,
        0,         # devmajor
        0,         # devminor
        rdev[0],   # rdevmajor (для узлов устройств)
        rdev[1],   # rdevminor
        len(names),
        0,         # check
    ]
    out = b"070701" + b"".join(f"{value:08X}".encode() for value in fields) + names
    if len(out) % 4:
        out += b"\0" * (4 - len(out) % 4)
    return out


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: mkcpio.py <tree> <out.cpio>")
    cpio_newc(sys.argv[1], sys.argv[2])
    print(f"cpio готов: {sys.argv[2]}")
