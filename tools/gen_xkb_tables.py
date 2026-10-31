#!/usr/bin/env python3
"""Generate layout tables for LayoutPilot from the system XKB data.

The generator asks `xkbcli how-to-type` for every ASCII/Cyrillic letter in a
two-group keymap (us,ru) and records the canonical physical key + Shift level
for each produced character. The result is a deterministic TSV file plus a
generated Rust module, so building the project does not require xkbcommon.

Letters are required; printable ASCII (digits, punctuation, space) is recorded
too so that physical reinterpretation can produce punctuation for Russian
letters that live on punctuation keys, and so strict reversibility can check
the whole candidate.

Usage:
    python3 tools/gen_xkb_tables.py [--layout us,ru] [--out-dir .]

Requirements:
    xkbcli (package xkbcommon-tools), python3 >= 3.9
"""

from __future__ import annotations

import argparse
import csv
import re
import string
import subprocess
import sys
from pathlib import Path

# Direct-access row of `xkbcli how-to-type`:
# 24  AD01  1  English (US)  1  [ ]
ROW_RE = re.compile(
    r"^(?P<keycode>\d+)\s+(?P<keyname>\S+)\s+(?P<layout>[12])\s+"
    r"(?P<layout_name>.+?)\s+(?P<level>\d+)\s+\[(?P<mods>[^\]]*)\]\s*$"
)

CYRILLIC = (
    "АБВГДЕЁЖЗИЙКЛМНОПРСТУФХЦЧШЩЪЫЬЭЮЯ"
    "абвгдеёжзийклмнопрстуфхцчшщъыьэюя"
)
ASCII_PRINTABLE = "".join(chr(code) for code in range(0x20, 0x7F))
CHARS = ASCII_PRINTABLE + CYRILLIC
LAYOUT_NAMES = {1: "us", 2: "ru"}


def xkbcli_version() -> str:
    proc = subprocess.run(["xkbcli", "--version"], capture_output=True, text=True)
    return proc.stdout.strip() or "unknown"


def how_to_type(char: str, layout: str) -> list[dict[str, str]]:
    """Return direct-access rows for one character."""
    proc = subprocess.run(
        ["xkbcli", "how-to-type", "--layout", layout, char],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"xkbcli failed for {char!r}: {proc.stderr.strip() or proc.returncode}"
        )
    rows = []
    in_direct = False
    for line in proc.stdout.splitlines():
        if line.startswith("=== Direct access ==="):
            in_direct = True
            continue
        if line.startswith("=== Access via Compose"):
            in_direct = False
            continue
        if not in_direct:
            continue
        match = ROW_RE.match(line)
        if match:
            rows.append(match.groupdict())
    return rows


def canonical_row(rows: list[dict[str, str]]) -> dict[str, str] | None:
    """Pick the canonical key for a character.

    Prefer the standard keycode range (8..255) over synthetic high keycodes,
    then a row without modifiers, then the lowest keycode. Caps Lock rows are
    ignored: the table records the physical level, not the input state.
    """
    usable = []
    for row in rows:
        mods = row["mods"].strip()
        if "Lock" in mods or "AltGr" in mods or "LevelThree" in mods:
            continue
        if mods not in ("", "Shift"):
            continue
        usable.append(row)
    if not usable:
        return None
    usable.sort(
        key=lambda r: (
            0 if 8 <= int(r["keycode"]) <= 255 else 1,
            0 if r["mods"].strip() == "" else 1,
            int(r["keycode"]),
        )
    )
    return usable[0]


def generate_order(
    layout: str, order: tuple[str, str]
) -> tuple[dict[tuple[str, int, int], str], list[str]]:
    """Return entries for one XKB layout order.

    `xkbcli how-to-type` reports only one group for symbols shared by both
    groups (space, digits, some punctuation). Generating with both "us,ru" and
    "ru,us" and merging fills both tables.
    """
    entries: dict[tuple[str, int, int], str] = {}
    warnings: list[str] = []
    for char in CHARS:
        rows = how_to_type(char, layout)
        found_in_layout = False
        for layout_no in (1, 2):
            candidates = [row for row in rows if int(row["layout"]) == layout_no]
            row = canonical_row(candidates)
            if row is None:
                continue
            found_in_layout = True
            keycode = int(row["keycode"])
            level = 2 if row["mods"].strip() == "Shift" else 1
            slot = (order[layout_no - 1], keycode, level)
            if slot in entries and entries[slot] != char:
                warnings.append(
                    f"conflict at {slot[0]}, keycode {keycode}, level {level}: "
                    f"{entries[slot]!r} vs {char!r}; keeping first"
                )
                continue
            entries[slot] = char
        if not found_in_layout:
            warnings.append(f"no direct access for {char!r}")
    return entries, warnings


def generate(layout: str) -> tuple[list[tuple[str, int, int, str]], list[str]]:
    """Generate both orders and merge the results."""
    merged: dict[tuple[str, int, int], str] = {}
    warnings: list[str] = []
    for spec, order in ((layout, tuple(LAYOUT_NAMES.values())), ("ru,us", ("ru", "us"))):
        entries, run_warnings = generate_order(spec, order)
        warnings.extend(run_warnings)
        for slot, char in entries.items():
            if slot in merged and merged[slot] != char:
                warnings.append(
                    f"merge conflict at {slot}: {merged[slot]!r} vs {char!r}; keeping first"
                )
                continue
            merged[slot] = char

    result = [
        (layout_id, keycode, level, char)
        for (layout_id, keycode, level), char in merged.items()
    ]
    result.sort(key=lambda entry: (entry[0], entry[1], entry[2]))
    return result, warnings


def rust_char(char: str) -> str:
    if char == "\\":
        return "'\\\\'"
    if char == "'":
        return "'\\''"
    if char == "\t":
        return "'\\t'"
    if char == "\n":
        return "'\\n'"
    return f"'{char}'"


def write_tsv(entries: list[tuple[str, int, int, str]], path: Path) -> None:
    with path.open("w", encoding="utf-8", newline="") as file:
        writer = csv.writer(file, delimiter="\t", lineterminator="\n")
        writer.writerow(["layout", "keycode", "level", "char"])
        writer.writerows(entries)


def write_rust(entries: list[tuple[str, int, int, str]], path: Path) -> None:
    by_layout: dict[str, list[tuple[int, int, str]]] = {"us": [], "ru": []}
    for layout_id, keycode, level, char in entries:
        by_layout[layout_id].append((keycode, level, char))

    with path.open("w", encoding="utf-8") as file:
        file.write("// @generated by tools/gen_xkb_tables.py; do not edit.\n")
        file.write('// Source: XKB keymap "us,ru" (XKB keycode = evdev keycode + 8).\n\n')
        for layout_id in ("us", "ru"):
            name = layout_id.upper()
            title = "English (US)" if layout_id == "us" else "Russian"
            file.write(f"/// {title} letters: (XKB keycode, level, character).\n")
            file.write(f"pub const {name}: &[(u16, u8, char)] = &[\n")
            for keycode, level, char in by_layout[layout_id]:
                file.write(f"    ({keycode}, {level}, {rust_char(char)}),\n")
            file.write("];\n\n")


def write_readme(path: Path, layout: str, version: str, count: int) -> None:
    path.write_text(
        "# XKB layout tables\n\n"
        "Generated from the system XKB data by `tools/gen_xkb_tables.py`.\n\n"
        f"- XKB layout spec: `{layout}`\n"
        f"- `xkbcli` version: `{version}`\n"
        f"- entries: {count}\n\n"
        "Regenerate on a machine with `xkbcommon-tools` installed:\n\n"
        "```sh\n"
        "python3 tools/gen_xkb_tables.py\n"
        "```\n\n"
        "`us_ru.tsv` is for inspection; the canonical data is in\n"
        "`crates/layoutpilot-layouts/src/generated.rs`.\n"
        "XKB keycode = evdev keycode + 8.\n",
        encoding="utf-8",
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--layout", default="us,ru", help="XKB layout spec (default: us,ru)")
    parser.add_argument("--out-dir", default=".", help="repository root (default: .)")
    args = parser.parse_args()

    entries, warnings = generate(args.layout)
    for warning in warnings:
        print(f"warning: {warning}", file=sys.stderr)

    expected_us = set(string.ascii_letters)
    expected_ru = set(CYRILLIC)
    actual = {"us": set(), "ru": set()}
    for layout_id, _, _, char in entries:
        actual[layout_id].add(char)
    for layout_id, expected in (("us", expected_us), ("ru", expected_ru)):
        missing = sorted(expected - actual[layout_id])
        if missing:
            print(f"error: missing {layout_id} letters: {missing}", file=sys.stderr)
            return 1

    out_dir = Path(args.out_dir)
    data_dir = out_dir / "data" / "xkb"
    src_dir = out_dir / "crates" / "layoutpilot-layouts" / "src"
    data_dir.mkdir(parents=True, exist_ok=True)
    src_dir.mkdir(parents=True, exist_ok=True)

    version = xkbcli_version()
    write_tsv(entries, data_dir / "us_ru.tsv")
    write_rust(entries, src_dir / "generated.rs")
    write_readme(data_dir / "README.md", args.layout, version, len(entries))

    print(f"wrote {len(entries)} entries: {data_dir / 'us_ru.tsv'}")
    print(f"wrote {src_dir / 'generated.rs'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
