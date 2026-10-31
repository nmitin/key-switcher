# Frequency dictionaries

Source data: [FrequencyWords](https://github.com/hermitdave/FrequencyWords)
by Hermit Dave, generated from the OpenSubtitles2018 corpus.

| File | Language | SHA-256 |
|---|---|---|
| `ru_50k.txt` | Russian | `6095f507cc167488ec66ada5a85ac50433503a08ad24a07c6eabdf54352c4e7f` |
| `en_50k.txt` | English | `5351ff405b1126ef555791dd4d9798a48e3e9a501a9fc481a9da957752cfb458` |

Retrieved: 2026-10-31, top 50,000 tokens per language.

## License

- The FrequencyWords *code* is MIT (see `LICENSE-FrequencyWords`).
- The word lists and derived data (FST dictionaries, bigram counts) are
  **CC BY-SA 4.0** (see `LICENSE-CC-BY-SA-4.0`).
- The LayoutPilot Rust code is MIT OR Apache-2.0. The data is a separate work
  with its own license, as required by ADR-015.

Attribution: "FrequencyWords by Hermit Dave, based on OpenSubtitles2018,
licensed under CC BY-SA 4.0; filtered to alphabetic lowercase words and
converted to FST/bigram form for LayoutPilot."

## Modifications

- Lines are parsed as `word count`.
- Only words consisting entirely of alphabetic characters are kept.
- Words are lowercased and deduplicated; words longer than 48 characters are
  dropped.
- The 50k lists are converted to FST maps by `crates/layoutpilot-dict/build.rs`.
- Character bigrams with start/end markers are counted from the same lists.

The full CC BY-SA 4.0 text is in `LICENSE-CC-BY-SA-4.0`.
