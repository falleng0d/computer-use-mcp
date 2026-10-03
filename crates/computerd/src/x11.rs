//! One X display: pixels over MIT-SHM, damage, pointer, the focused window title, and input over XTEST.

use anyhow::{Context, Result, bail, ensure};
use computer_protocol::{Cursor, ScreenSize};
use x11rb::{
    connection::{Connection, RequestConnection as _},
    protocol::{
        Event,
        damage::{self, ConnectionExt as _, ReportLevel},
        shm::{self, ConnectionExt as _},
        xproto::{AtomEnum, ConnectionExt as _, ImageFormat, Window},
        xtest::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

use crate::{frames::BYTES_PER_PIXEL, shm::Segment};

mod input;

const DEPTH: u8 = 24;

pub struct Capturer {
    conn: RustConnection,
    root: Window,
    size: ScreenSize,
    damage: damage::Damage,
    shm_seg: shm::Seg,
    segment: Segment,
    atoms: Atoms,
}

struct Atoms {
    active_window: u32,
    wm_name: u32,
    utf8_string: u32,
    wm_check: u32,
    client_list: u32,
}

fn atom(conn: &RustConnection, name: &str) -> Result<u32> {
    Ok(conn
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("looking up atom {name}"))?
        .atom)
}

impl Capturer {
    /// Connects to `display` (for example `:1`) and sets up shared memory and damage tracking.
    pub fn connect(display: &str, size: ScreenSize) -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(Some(display))
            .with_context(|| format!("connecting to display {display}"))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        ensure!(
            screen.root_depth == DEPTH
                && (screen.width_in_pixels, screen.height_in_pixels)
                    == (size.width(), size.height()),
            "display {display} is {}x{} at depth {}, expected {size} at depth {DEPTH}",
            screen.width_in_pixels,
            screen.height_in_pixels,
            screen.root_depth,
        );
        let format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == DEPTH)
            .context("the display has no depth 24 pixmap format")?;
        ensure!(
            usize::from(format.bits_per_pixel) == BYTES_PER_PIXEL * 8
                && format.scanline_pad == 32
                && conn.setup().image_byte_order == x11rb::protocol::xproto::ImageOrder::LSB_FIRST,
            "the display does not use little-endian 32-bit pixels"
        );

        conn.extension_information(shm::X11_EXTENSION_NAME)?
            .context("the display has no MIT-SHM extension")?;
        conn.extension_information(damage::X11_EXTENSION_NAME)?
            .context("the display has no DAMAGE extension")?;
        conn.extension_information(xtest::X11_EXTENSION_NAME)?
            .context("the display has no XTEST extension")?;
        conn.xtest_get_version(2, 2)?.reply()?;
        conn.damage_query_version(1, 1)?.reply()?;
        conn.shm_query_version()?.reply()?;

        let len = usize::from(size.width()) * usize::from(size.height()) * BYTES_PER_PIXEL;
        let segment = Segment::new(len).context("allocating the shared memory segment")?;
        let shm_seg = conn.generate_id()?;
        conn.shm_attach(shm_seg, segment.id(), false)?
            .check()
            .context("attaching the shared memory segment to the display")?;
        segment
            .remove_on_detach()
            .context("marking the segment for removal")?;

        let damage = conn.generate_id()?;
        conn.damage_create(damage, root, ReportLevel::NON_EMPTY)?
            .check()
            .context("watching the display for changes")?;

        let atoms = Atoms {
            active_window: atom(&conn, "_NET_ACTIVE_WINDOW")?,
            wm_name: atom(&conn, "_NET_WM_NAME")?,
            utf8_string: atom(&conn, "UTF8_STRING")?,
            wm_check: atom(&conn, "_NET_SUPPORTING_WM_CHECK")?,
            client_list: atom(&conn, "_NET_CLIENT_LIST")?,
        };
        Ok(Self {
            conn,
            root,
            size,
            damage,
            shm_seg,
            segment,
            atoms,
        })
    }

    /// True once a window manager has announced itself on the root window.
    pub fn window_manager_ready(&self) -> Result<bool> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.wm_check,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?;
        Ok(reply.value_len > 0)
    }

    /// Returns whether the screen changed since the previous call, and re-arms the watch.
    pub fn take_damage(&self) -> Result<bool> {
        // A reply arrives after every event sent before it, so this drains all pending damage.
        self.conn.get_input_focus()?.reply()?;
        let mut damaged = false;
        while let Some(event) = self.conn.poll_for_event()? {
            damaged |= matches!(event, Event::DamageNotify(_));
        }
        self.conn
            .damage_subtract(self.damage, x11rb::NONE, x11rb::NONE)?
            .check()?;
        Ok(damaged)
    }

    pub fn cursor(&self) -> Result<Cursor> {
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        Ok(Cursor {
            x: pointer.root_x,
            y: pointer.root_y,
        })
    }

    /// Title of the focused window, or an empty string when there is none or it has no title.
    pub fn active_window_title(&self) -> String {
        self.try_active_window_title().unwrap_or_default()
    }

    fn try_active_window_title(&self) -> Result<String> {
        let active = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.active_window,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?;
        let Some(window) = active.value32().and_then(|mut ids| ids.next()) else {
            return Ok(String::new());
        };
        if window == 0 {
            return Ok(String::new());
        }
        self.window_title(window)
    }

    /// Title of `window`, or an empty string when it has none.
    fn window_title(&self, window: Window) -> Result<String> {
        for (property, kind) in [
            (self.atoms.wm_name, self.atoms.utf8_string),
            (AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()),
        ] {
            let reply = self
                .conn
                .get_property(false, window, property, kind, 0, 256)?
                .reply()?;
            if !reply.value.is_empty() {
                return Ok(String::from_utf8_lossy(&reply.value).into_owned());
            }
        }
        Ok(String::new())
    }

    /// Copies the whole screen into shared memory and returns it as BGRX bytes.
    pub fn grab(&mut self) -> Result<&[u8]> {
        let reply = self
            .conn
            .shm_get_image(
                self.root,
                0,
                0,
                self.size.width(),
                self.size.height(),
                !0,
                ImageFormat::Z_PIXMAP.into(),
                self.shm_seg,
                0,
            )?
            .reply()
            .context("reading the screen through shared memory")?;
        if reply.depth != DEPTH {
            bail!("the screen has depth {}, expected {DEPTH}", reply.depth);
        }
        Ok(self.segment.bytes())
    }
}
