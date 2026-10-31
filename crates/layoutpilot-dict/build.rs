//! Build FST dictionaries and compact bigram tables from FrequencyWords data.
//!
//! The generated files live in `OUT_DIR` and are embedded into the crate:
//! `ru.fst`, `en.fst`, `ngrams.rs`. The source lists are vendored under
//! `data/dict/` with their own CC BY-SA 4.0 license and attribution.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;

const START: char = '\u{2}';
const END: char = '\u{3}';

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let data_dir = manifest_dir.join("../../data/dict");
    let mut ngrams = String::new();

    for (language, prefix) in [("ru", "RU"), ("en", "EN")] {
        let path = data_dir.join(format!("{language}_50k.txt"));
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("cannot read {}: {error}", path.display());
        });

        let mut words: BTreeMap<String, u64> = BTreeMap::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let (Some(raw_word), Some(raw_count)) = (fields.next(), fields.next()) else {
                continue;
            };
            let Ok(count) = raw_count.parse::<u64>() else {
                continue;
            };
            if !raw_word.chars().all(char::is_alphabetic) {
                continue;
            }
            let word = raw_word.to_lowercase();
            if word.is_empty() || word.chars().count() > 48 {
                continue;
            }
            *words.entry(word).or_insert(0) += count;
        }
        if words.is_empty() {
            panic!("no words parsed from {}", path.display());
        }

        let fst_path = out_dir.join(format!("{language}.fst"));
        let writer = BufWriter::new(
            File::create(&fst_path)
                .unwrap_or_else(|error| panic!("cannot create {}: {error}", fst_path.display())),
        );
        let mut builder = fst::MapBuilder::new(writer).expect("FST builder");
        let mut total_freq = 0_u64;
        for (word, count) in &words {
            total_freq += count;
            builder.insert(word.as_bytes(), *count).expect("FST insert");
        }
        builder.finish().expect("FST finish");

        let mut bigrams: BTreeMap<(char, char), u64> = BTreeMap::new();
        for (word, count) in &words {
            let mut sequence = Vec::with_capacity(word.chars().count() + 2);
            sequence.push(START);
            sequence.extend(word.chars());
            sequence.push(END);
            for pair in sequence.windows(2) {
                *bigrams.entry((pair[0], pair[1])).or_insert(0) += *count;
            }
        }
        let bigram_total: u64 = bigrams.values().sum();

        writeln!(ngrams, "pub const {prefix}_TOTAL_FREQ: u64 = {total_freq};").expect("write");
        writeln!(
            ngrams,
            "pub const {prefix}_BIGRAM_TOTAL: u64 = {bigram_total};"
        )
        .expect("write");
        writeln!(
            ngrams,
            "pub static {prefix}_BIGRAMS: &[(char, char, u64)] = &["
        )
        .expect("write");
        for ((first, second), count) in &bigrams {
            writeln!(
                ngrams,
                "    ({}, {}, {count}),",
                char_literal(*first),
                char_literal(*second)
            )
            .expect("write");
        }
        writeln!(ngrams, "];\n").expect("write");
    }

    let ngrams_path = out_dir.join("ngrams.rs");
    fs::write(&ngrams_path, ngrams).expect("write ngrams.rs");
    println!("cargo:rerun-if-changed=build.rs");
}

fn char_literal(ch: char) -> String {
    match ch {
        '\'' => "'\\''".to_owned(),
        '\\' => "'\\\\'".to_owned(),
        '\u{2}' => "'\\u{2}'".to_owned(),
        '\u{3}' => "'\\u{3}'".to_owned(),
        ch if ch.is_control() => format!("'\\u{{{:x}}}'", ch as u32),
        ch => format!("'{ch}'"),
    }
}
