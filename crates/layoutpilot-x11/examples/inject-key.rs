//! Inject one X11 keycode via XTest. Development helper.
//!
//! ```sh
//! cargo run -p layoutpilot-x11 --example inject-key -- 127   # Pause
//! ```

use std::env;

use layoutpilot_x11::X11Injector;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let keycode = args
        .next()
        .ok_or("usage: inject-key <x11-keycode> [--shift]")?
        .parse::<u8>()?;
    let shift = args.any(|arg| arg == "--shift");
    let mut injector = X11Injector::new()?;
    injector.tap_keycode(keycode, shift)?;
    println!("injected keycode {keycode} (shift={shift})");
    Ok(())
}
