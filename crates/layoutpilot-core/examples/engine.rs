//! Simulate typing and show engine actions.
//!
//! ```sh
//! cargo run -p layoutpilot-core --example engine -- "ghbdtn "
//! cargo run -p layoutpilot-core --example engine -- "Ghbdtn, "
//! ```

use std::env;

use layoutpilot_core::{
    Action, Engine, EngineConfig, InputContext, LayoutId, LexiconDetector, Profile,
};

fn main() {
    let text = env::args().nth(1).unwrap_or_else(|| "ghbdtn ".to_owned());

    let detector = LexiconDetector::from_words(["привет", "мир", "hello"]);
    let mut engine = Engine::new(EngineConfig::default(), detector);
    let context = InputContext::new(Profile::Auto, LayoutId::Us);

    for ch in text.chars() {
        if let Some(action) = engine.on_char(ch, &context) {
            match action {
                Action::Replace(correction) => println!(
                    "replace {:?} -> {:?}: Backspace x{}, insert {:?}",
                    correction.original,
                    correction.converted,
                    correction.delete_chars(true),
                    correction.insert_text(true)
                ),
                Action::Undo(correction) => {
                    println!(
                        "undo {:?} -> {:?}",
                        correction.converted, correction.original
                    );
                }
                Action::Redo(correction) => {
                    println!(
                        "redo {:?} -> {:?}",
                        correction.original, correction.converted
                    );
                }
            }
        }
    }

    println!("buffer: {:?}", engine.buffer_text());
    println!("state: {:?}", engine.state());

    if let Some(action) = engine.undo() {
        println!("undo action: {action:?}");
        println!("buffer after undo: {:?}", engine.buffer_text());
    }
}
