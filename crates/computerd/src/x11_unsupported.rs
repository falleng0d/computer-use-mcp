//! Stand-in so `computerd` builds off Linux. Screens only work inside the container.

#![expect(clippy::unused_self, reason = "mirrors the Linux Capturer API")]

use anyhow::{Result, bail};
use computer_protocol::{Cursor, ScreenSize};

pub struct Capturer;

impl Capturer {
    pub fn connect(_display: &str, _size: ScreenSize) -> Result<Self> {
        bail!("screens need Linux")
    }

    pub fn window_manager_ready(&self) -> Result<bool> {
        bail!("screens need Linux")
    }

    pub fn take_damage(&self) -> Result<bool> {
        bail!("screens need Linux")
    }

    pub fn cursor(&self) -> Result<Cursor> {
        bail!("screens need Linux")
    }

    pub fn active_window_title(&self) -> String {
        String::new()
    }

    pub fn grab(&mut self) -> Result<&[u8]> {
        bail!("screens need Linux")
    }
}
