//! Small calibration harness over the vendored FrequencyWords lists.
//!
//! It measures two things on the top N words of each language:
//! * false positives: a correctly typed word should not be converted;
//! * true positives: a word typed through the wrong layout should be converted.
//!
//! ```sh
//! cargo run -p layoutpilot-dict --example calibrate -- 5000
//! ```

use std::env;
use std::fs;

use layoutpilot_core::convert_word;
use layoutpilot_core::LayoutId;
use layoutpilot_dict::DictionaryDetector;

fn main() {
    let limit: usize = env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(5_000);
    let detector = DictionaryDetector::new();

    for (language, path) in [
        (
            "en",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/dict/en_50k.txt"),
        ),
        (
            "ru",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/dict/ru_50k.txt"),
        ),
    ] {
        let text = fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"));
        let words: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|word| word.chars().all(char::is_alphabetic))
            .filter(|word| word.chars().count() >= 3)
            .take(limit)
            .collect();

        let (source, target) = if language == "en" {
            (LayoutId::Us.table(), LayoutId::Ru.table())
        } else {
            (LayoutId::Ru.table(), LayoutId::Us.table())
        };

        let mut false_positives = 0;
        let mut false_positive_examples = Vec::new();
        let mut true_positives = 0;
        let mut true_positive_total = 0;
        let mut low_confidence_examples = Vec::new();

        for word in &words {
            let Ok(other) = convert_word(word, source, target) else {
                continue;
            };

            let correct = detector.confidence_for(word, &other);
            if correct >= 0.97 {
                false_positives += 1;
                if false_positive_examples.len() < 10 {
                    false_positive_examples.push((*word, correct));
                }
            }

            let mistyped = detector.confidence_for(&other, word);
            true_positive_total += 1;
            if mistyped >= 0.97 {
                true_positives += 1;
            } else if low_confidence_examples.len() < 10 {
                low_confidence_examples.push((*word, mistyped));
            }
        }

        println!(
            "{language}: words={}, false positives={false_positives} ({:.3}%), \
             true positives={true_positives}/{true_positive_total} ({:.2}%)",
            words.len(),
            100.0 * false_positives as f64 / words.len() as f64,
            100.0 * true_positives as f64 / true_positive_total as f64,
        );
        if !false_positive_examples.is_empty() {
            println!("  false positive examples: {false_positive_examples:?}");
        }
        if !low_confidence_examples.is_empty() {
            println!("  missed examples: {low_confidence_examples:?}");
        }
    }
}
