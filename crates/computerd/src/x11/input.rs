//! Mouse, keyboard, and window focus through XTEST and the window manager.

use std::{thread::sleep, time::Duration};

use anyhow::{Context, Result};
use computer_protocol::{Button, Direction, Point};
use x11rb::{
    connection::Connection as _,
    protocol::{
        xproto::{
            self, AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ClientMessageEvent,
            ConnectionExt as _, EventMask, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
        },
        xtest::ConnectionExt as _,
    },
};

use super::Capturer;
use crate::{
    keys::{self, Keymap},
    plan::{Edge, Input, WindowInfo},
};

const NO_SYMBOL: u32 = 0;

/// Time for applications to reread the keyboard after a temporary binding changes.
const KEYMAP_SETTLE: Duration = Duration::from_millis(20);

/// Source indication for `_NET_ACTIVE_WINDOW`: a request from a pager, which window managers always honor.
const SOURCE_PAGER: u32 = 2;

const CLIENT_LIST_LIMIT: u32 = 1024;
const CLASS_LIMIT: u32 = 256;

fn button_code(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
    }
}

fn wheel_code(direction: Direction) -> u8 {
    match direction {
        Direction::Up => 4,
        Direction::Down => 5,
        Direction::Left => 6,
        Direction::Right => 7,
    }
}

impl Capturer {
    /// Sends one input.
    pub fn perform(&self, input: &Input) -> Result<()> {
        match input {
            Input::Move(at) => self.motion(*at)?,
            Input::Click { at, button } => {
                self.motion(*at)?;
                self.button(button_code(*button), Edge::Press)?;
                self.button(button_code(*button), Edge::Release)?;
            }
            Input::Button { at, button, edge } => {
                if let Some(at) = at {
                    self.motion(*at)?;
                }
                self.button(button_code(*button), *edge)?;
            }
            Input::Type(keysyms) => self.keys(keysyms, &[])?,
            Input::Key { keysym, modifiers } => self.keys(&[*keysym], modifiers)?,
            Input::Scroll {
                at,
                direction,
                amount,
            } => {
                if let Some(at) = at {
                    self.motion(*at)?;
                }
                for _ in 0..*amount {
                    self.button(wheel_code(*direction), Edge::Press)?;
                    self.button(wheel_code(*direction), Edge::Release)?;
                }
            }
        }
        self.sync()
    }

    /// Waits until the server has handled every request sent so far.
    fn sync(&self) -> Result<()> {
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(kind, detail, 0, self.root, x, y, 0)?;
        Ok(())
    }

    fn motion(&self, at: Point) -> Result<()> {
        let x = i16::try_from(at.x).context("x is off the screen")?;
        let y = i16::try_from(at.y).context("y is off the screen")?;
        self.fake(MOTION_NOTIFY_EVENT, 0, x, y)
    }

    fn button(&self, code: u8, edge: Edge) -> Result<()> {
        let kind = match edge {
            Edge::Press => BUTTON_PRESS_EVENT,
            Edge::Release => BUTTON_RELEASE_EVENT,
        };
        self.fake(kind, code, 0, 0)
    }

    fn key_edge(&self, keycode: u8, edge: Edge) -> Result<()> {
        let kind = match edge {
            Edge::Press => KEY_PRESS_EVENT,
            Edge::Release => KEY_RELEASE_EVENT,
        };
        self.fake(kind, keycode, 0, 0)
    }

    fn keymap(&self) -> Result<Keymap> {
        let setup = self.conn.setup();
        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let reply = self
            .conn
            .get_keyboard_mapping(first, last - first + 1)?
            .reply()
            .context("reading the keyboard mapping")?;
        Ok(Keymap::new(first, reply.keysyms_per_keycode, reply.keysyms))
    }

    fn bind(&self, keycode: u8, keysym: u32) -> Result<()> {
        self.conn
            .change_keyboard_mapping(1, keycode, 2, &[keysym, keysym])?;
        Ok(())
    }

    /// Taps each keysym in turn while `modifiers` are held.
    fn keys(&self, keysyms: &[u32], modifiers: &[u32]) -> Result<()> {
        let keymap = self.keymap()?;
        let held = modifiers
            .iter()
            .map(|modifier| {
                keymap
                    .find(*modifier)
                    .map(|press| press.keycode)
                    .context("the keyboard has no key for that modifier")
            })
            .collect::<Result<Vec<u8>>>()?;
        for keycode in &held {
            self.key_edge(*keycode, Edge::Press)?;
        }
        let typed = self.tap_all(&keymap, keysyms);
        let mut released = Ok(());
        for keycode in held.iter().rev() {
            released = released.and(self.key_edge(*keycode, Edge::Release));
        }
        typed.and(released).and_then(|()| self.sync())
    }

    /// Taps each keysym. A keysym with no key is bound to a spare keycode just for its tap,
    /// and the spare keycode is cleared again at the end.
    fn tap_all(&self, keymap: &Keymap, keysyms: &[u32]) -> Result<()> {
        let mut bound: Option<(u8, u32)> = None;
        let typed = self.tap_each(keymap, keysyms, &mut bound);
        let cleared = match bound {
            Some((keycode, _)) => self.bind(keycode, NO_SYMBOL).and_then(|()| self.sync()),
            None => Ok(()),
        };
        typed.and(cleared)
    }

    fn tap_each(
        &self,
        keymap: &Keymap,
        keysyms: &[u32],
        bound: &mut Option<(u8, u32)>,
    ) -> Result<()> {
        let shift = keymap.find(keys::SHIFT_L).map(|press| press.keycode);
        for &keysym in keysyms {
            if let Some(press) = keymap.find(keysym) {
                let shift_key = shift.filter(|_| press.shift);
                if let Some(shift_key) = shift_key {
                    self.key_edge(shift_key, Edge::Press)?;
                }
                self.key_edge(press.keycode, Edge::Press)?;
                self.key_edge(press.keycode, Edge::Release)?;
                if let Some(shift_key) = shift_key {
                    self.key_edge(shift_key, Edge::Release)?;
                }
                continue;
            }
            let keycode = match *bound {
                Some((keycode, current)) if current == keysym => keycode,
                _ => {
                    let keycode = bound
                        .map(|(keycode, _)| keycode)
                        .or_else(|| keymap.spare())
                        .context("the keyboard has no free key to type this character with")?;
                    self.bind(keycode, keysym)?;
                    self.sync()?;
                    sleep(KEYMAP_SETTLE);
                    *bound = Some((keycode, keysym));
                    keycode
                }
            };
            self.key_edge(keycode, Edge::Press)?;
            self.key_edge(keycode, Edge::Release)?;
            self.sync()?;
            sleep(KEYMAP_SETTLE);
        }
        Ok(())
    }

    /// Asks the window manager to raise and focus `window`.
    pub fn activate(&self, window: u32) -> Result<()> {
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms.active_window,
            [SOURCE_PAGER, 0, 0, 0, 0],
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.sync()
    }

    /// The top-level windows the window manager lists.
    pub fn windows(&self) -> Result<Vec<WindowInfo>> {
        let list = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.client_list,
                AtomEnum::WINDOW,
                0,
                CLIENT_LIST_LIMIT,
            )?
            .reply()
            .context("listing the open windows")?;
        let ids: Vec<u32> = list.value32().map(Iterator::collect).unwrap_or_default();
        ids.into_iter()
            .map(|id| {
                let class = self
                    .conn
                    .get_property(
                        false,
                        id,
                        AtomEnum::WM_CLASS,
                        xproto::AtomEnum::STRING,
                        0,
                        CLASS_LIMIT,
                    )?
                    .reply()?;
                Ok(WindowInfo {
                    id,
                    class: class
                        .value
                        .split(|byte| *byte == 0)
                        .filter(|part| !part.is_empty())
                        .map(|part| String::from_utf8_lossy(part).into_owned())
                        .collect(),
                    title: self.window_title(id).unwrap_or_default(),
                })
            })
            .collect()
    }
}
