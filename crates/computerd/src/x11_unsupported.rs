//! Stand-in so `computerd` builds off Linux. Screens only work inside the container.

#![expect(clippy::unused_self, reason = "mirrors the Linux Capturer API")]

use anyhow::{Result, bail};
use computer_protocol::{Cursor, ScreenSize};

use crate::plan::{Input, WindowInfo};

pub(crate) struct Capturer;

impl Capturer {
    pub(crate) fn connect(_display: &str, _size: ScreenSize) -> Result<Self> {
        bail!("screens need Linux")
    }

    pub(crate) fn window_manager_ready(&self) -> Result<bool> {
        bail!("screens need Linux")
    }

    pub(crate) fn take_damage(&self) -> Result<bool> {
        bail!("screens need Linux")
    }

    pub(crate) fn cursor(&self) -> Result<Cursor> {
        bail!("screens need Linux")
    }

    pub(crate) fn active_window_title(&self) -> String {
        String::new()
    }

    pub(crate) fn grab(&mut self) -> Result<&[u8]> {
        bail!("screens need Linux")
    }

    pub(crate) fn perform(&self, _input: &Input) -> Result<()> {
        bail!("screens need Linux")
    }

    pub(crate) fn check_keys(&self, _inputs: &[Input]) -> Result<()> {
        bail!("screens need Linux")
    }

    pub(crate) fn release_held(&self) -> Result<()> {
        bail!("screens need Linux")
    }

    pub(crate) fn windows(&self) -> Result<Vec<WindowInfo>> {
        bail!("screens need Linux")
    }

    pub(crate) fn activate(&self, _window: u32) -> Result<()> {
        bail!("screens need Linux")
    }
}
