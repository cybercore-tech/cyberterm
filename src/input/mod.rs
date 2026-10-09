// src/input/mod.rs
//
// Pure encoders from user input to PTY bytes, plus the keybinding table.
// Nothing in here touches the window or the terminal state directly.

pub mod bindings;
pub mod keyboard;
pub mod links;
pub mod mouse;
pub mod paste;
