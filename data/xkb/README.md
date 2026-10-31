# XKB layout tables

Generated from the system XKB data by `tools/gen_xkb_tables.py`.

- XKB layout spec: `us,ru`
- `xkbcli` version: `1.13.2`
- entries: 189

Regenerate on a machine with `xkbcommon-tools` installed:

```sh
python3 tools/gen_xkb_tables.py
```

`us_ru.tsv` is for inspection; the canonical data is in
`crates/layoutpilot-layouts/src/generated.rs`.
XKB keycode = evdev keycode + 8.
