//! Inspect dictionary counts, scores and confidence for a word pair.
//!
//! ```sh
//! cargo run -p layoutpilot-dict --example detect -- ghbdtn привет
//! cargo run -p layoutpilot-dict --example detect -- hello руддщ
//! ```

use std::env;

use layoutpilot_dict::{language_of, DictionaryDetector};

fn main() {
    let mut args = env::args().skip(1);
    let typed = args.next().unwrap_or_else(|| "ghbdtn".to_owned());
    let candidate = args.next().unwrap_or_else(|| "привет".to_owned());
    let detector = DictionaryDetector::new();

    for (label, word) in [("typed", typed.as_str()), ("candidate", candidate.as_str())] {
        match language_of(word) {
            Some(language) => {
                let count = detector.dict_count(language, word);
                let score = detector.score(language, word);
                println!(
                    "{label:9} {word:?}: language={}, count={count}, score={score:.4}",
                    language.as_str()
                );
            }
            None => println!("{label:9} {word:?}: language=unknown"),
        }
    }

    let confidence = detector.confidence_for(&typed, &candidate);
    println!("confidence: {confidence:.6}");
}
