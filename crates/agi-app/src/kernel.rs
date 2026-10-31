//! The kernel's address, remembered for the shell.
//!
//! The tray page is a window served by the kernel, so the shell needs the
//! address and the token that the window's URL carries. Rather than teach the
//! shell to read the plugin's file, the launch that started the kernel keeps the
//! pair here: one process, one address, one token.

use std::sync::OnceLock;

static ENDPOINT: OnceLock<(u16, String)> = OnceLock::new();

/// Remember the kernel this process started.
pub fn remember(port: u16, token: String) {
    let _ = ENDPOINT.set((port, token));
}
