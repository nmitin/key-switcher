//! Minimal CLI example for the platform-independent core.
//!
//! ```sh
//! cargo run -p layoutpilot-core --example convert -- us ru "ghbdtn"
//! cargo run -p layoutpilot-core --example convert -- ru us "привет"
//! ```

use std::env;
use std::process::ExitCode;

use layoutpilot_core::{convert_text, LayoutId};

fn usage() -> &'static str {
    "usage: convert <from> <to> <text>\n\n\
     layouts: us, ru\n\
     example: convert us ru \"ghbdtn\""
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    }

    let parse = |value: &str| match value.parse::<LayoutId>() {
        Ok(layout) => Ok(layout),
        Err(error) => {
            eprintln!("{error}");
            Err(())
        }
    };
    let Ok(source) = parse(&args[0]) else {
        return ExitCode::from(2);
    };
    let Ok(target) = parse(&args[1]) else {
        return ExitCode::from(2);
    };

    let text = args[2..].join(" ");
    match convert_text(&text, source.table(), target.table()) {
        Ok(converted) => {
            println!("{converted}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("conversion failed: {error}");
            ExitCode::FAILURE
        }
    }
}
