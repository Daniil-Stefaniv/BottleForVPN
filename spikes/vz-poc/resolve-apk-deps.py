#!/usr/bin/env python3
"""Резолвер apk-пакетов Alpine по APKINDEX-файлам без apk-инструмента.

Семена → транзитивное замыкание по полю D: (имена пакетов и so:-
провайды разрешаются по установленным пакетам; so:libc.musl уже в
minirootfs). --urls печатает URL каждого пакета вместо имён.

Формат APKINDEX: параграфы «P:имя V:версия D:зависимости…».
"""

import sys
from dataclasses import dataclass, field


@dataclass
class Pkg:
    name: str
    ver: str
    deps: list[str] = field(default_factory=list)
    repo: str = ""
    provides: list[str] = field(default_factory=list)


def parse(index_path: str, repo: str) -> dict[str, Pkg]:
    packages: dict[str, Pkg] = {}
    provides_map: dict[str, str] = {}
    paragraph: dict[str, str] = {}
    for line in open(index_path, encoding="utf-8"):
        line = line.rstrip("\n")
        if not line:
            if "P" in paragraph:
                pkg = Pkg(
                    paragraph["P"],
                    paragraph.get("V", ""),
                    paragraph.get("D", "").split(),
                    repo,
                    paragraph.get("p", "").split(),
                )
                packages[pkg.name] = pkg
                for prov in pkg.provides:
                    provides_map.setdefault(prov.split("=")[0], pkg.name)
            paragraph = {}
            continue
        key, _, value = line.partition(":")
        paragraph[key] = value
    return packages, provides_map  # type: ignore[return-value]


def main() -> None:
    args = [a for a in sys.argv[1:]]
    urls = False
    if args and args[0] == "--urls":
        urls = True
        args = args[1:]
    if len(args) < 3:
        raise SystemExit("usage: resolve-apk-deps.py [--urls] APKINDEX-main APKINDEX-community SEED...")
    index_main, index_comm, seeds = args[0], args[1], args[2:]

    packages: dict[str, Pkg] = {}
    provides_map: dict[str, str] = {}
    for path, repo in [(index_main, "main"), (index_comm, "community")]:
        pkgs, provs = parse(path, repo)
        packages.update(pkgs)
        for so, name in provs.items():
            provides_map.setdefault(so, name)

    # libc/busybox уже в minirootfs; их so:-провайды не тянем.
    ignore = {"so:libc.musl-aarch64.so.1", "so:ld-musl-aarch64.so.1"}
    queue = list(seeds)
    seen: set[str] = set()
    result: list[Pkg] = []
    while queue:
        name = queue.pop(0)
        if name in seen or name in ignore:
            continue
        seen.add(name)
        pkg = packages.get(name) or packages.get(provides_map.get(name, ""))
        if pkg is None:
            print(f"# нет пакета: {name}", file=sys.stderr)
            continue
        result.append(pkg)
        for dep in pkg.deps:
            dep_name = dep.split("=")[0] if not dep.startswith("cmd:") else ""
            if dep_name:
                queue.append(dep_name)

    if urls:
        base = "https://dl-cdn.alpinelinux.org/alpine/v3.22"
        for pkg in result:
            print(f"{base}/{pkg.repo}/aarch64/{pkg.name}-{pkg.ver}.apk")
    else:
        for pkg in result:
            print(pkg.name, pkg.ver)


if __name__ == "__main__":
    main()
