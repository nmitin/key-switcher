//! Switch XKB group for diagnostics.
use layoutpilot_core::LayoutId;
use layoutpilot_x11::X11LayoutState;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let group = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0".to_owned())
        .parse::<u8>()?;
    let state = X11LayoutState::new(vec![LayoutId::Us, LayoutId::Ru])?;
    state.switch_group(group)?;
    println!("xkb group set to {group}");
    Ok(())
}
