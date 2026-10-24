//! Kitty's graphics protocol (artty), <https://sw.kovidgoyal.net/kitty/graphics-protocol/>:
//! commands `ESC _ G <keys> ; <payload> ESC \`.
//!
//! Supported: transmitting images (directly in base64, possibly in chunks,
//! or from a file; PNG, RGBA or RGB; optionally zlib-compressed), showing
//! them at the cursor (now or later by id, at a size in cells if given, the
//! cursor moving past them or not), deleting placements (all on screen, by
//! image id, or at the cursor) and freeing images, and queries. Not
//! supported: shared memory (answered as such, so programs fall back to
//! sending directly), animation, Unicode placeholders, source rectangles and
//! z-index (images go under the text).

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;

use super::Term;
use super::cell::{Cell, Image, ImageFormat};
use crate::event::{Event, EventListener};
use crate::grid::Dimensions;
use crate::index::{Column, Line};

/// The most image data kept for later display; the oldest goes first.
const MAX_STORED: usize = 320 << 20;
/// The largest single image, decoded.
const MAX_IMAGE: usize = 64 << 20;

/// Images transmitted and kept by id, and a chunked transmission under way.
#[derive(Default)]
pub(crate) struct KittyState {
    images: HashMap<u32, Stored>,
    stored: usize,
    /// Insertion order, for evicting the oldest.
    age: u64,
    /// For images sent with a number (`I`) instead of an id.
    next_id: u32,
    chunk: Option<(Command, Vec<u8>)>,
}

struct Stored {
    pixels: Pixels,
    age: u64,
}

/// An image's data, as received and checked.
#[derive(Clone)]
struct Pixels {
    format: ImageFormat,
    data: Arc<[u8]>,
    width: u32,
    height: u32,
}

/// Which placements a delete removes.
enum Doomed {
    /// Everything on screen.
    Visible,
    /// One kitty image's, anywhere (scrollback too).
    Kitty(u32),
    /// One placement (by its image's id).
    Placement(u64),
}

impl Doomed {
    fn matches(&self, image: &Image) -> bool {
        match self {
            Doomed::Visible => true,
            Doomed::Kitty(id) => image.kitty_id == *id,
            Doomed::Placement(id) => image.id == *id,
        }
    }
}

/// A command's keys (`a=T,f=100,...`); absent ones take the protocol's defaults.
#[derive(Clone, Debug, Default)]
struct Command {
    action: u8,
    format: u32,
    medium: u8,
    compression: u8,
    width: u32,
    height: u32,
    id: u32,
    number: u32,
    placement: u32,
    more: bool,
    quiet: u32,
    columns: u32,
    rows: u32,
    stay: bool,
    delete: u8,
}

impl Command {
    fn parse(keys: &[u8]) -> Self {
        let mut command = Command { action: b't', format: 32, medium: b'd', delete: b'a', ..Default::default() };
        for pair in keys.split(|&b| b == b',') {
            let [key, b'=', value @ ..] = pair else { continue };
            let number = || std::str::from_utf8(value).ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            let letter = value.first().copied().unwrap_or(0);
            match key {
                b'a' => command.action = letter,
                b'f' => command.format = number(),
                b't' => command.medium = letter,
                b'o' => command.compression = letter,
                b's' => command.width = number(),
                b'v' => command.height = number(),
                b'i' => command.id = number(),
                b'I' => command.number = number(),
                b'p' => command.placement = number(),
                b'm' => command.more = number() == 1,
                b'q' => command.quiet = number(),
                b'c' => command.columns = number(),
                b'r' => command.rows = number(),
                b'C' => command.stay = number() == 1,
                b'd' => command.delete = letter,
                _ => {},
            }
        }
        command
    }
}

impl<T: EventListener> Term<T> {
    pub(crate) fn kitty_graphics(&mut self, command: &[u8]) {
        let split = command.iter().position(|&b| b == b';').unwrap_or(command.len());
        let (keys, payload) = (&command[..split], command.get(split + 1..).unwrap_or_default());
        let mut command = Command::parse(keys);

        // Later chunks of a transmission carry only `m` (and maybe `q`).
        let payload = match self.kitty.chunk.take() {
            Some((first, mut data)) => {
                data.extend_from_slice(payload);
                if command.more {
                    if data.len() <= MAX_IMAGE * 4 / 3 + 4 {
                        self.kitty.chunk = Some((first, data));
                    }
                    return;
                }
                command = first;
                data
            },
            None if command.more && matches!(command.action, b't' | b'T' | b'q') => {
                self.kitty.chunk = Some((command, payload.to_vec()));
                return;
            },
            None => payload.to_vec(),
        };

        // An image number asks us to choose the id.
        if command.id == 0 && command.number != 0 && command.action != b'q' {
            self.kitty.next_id = self.kitty.next_id.wrapping_add(1).max(1) | (1 << 31);
            command.id = self.kitty.next_id;
        }

        let result = match command.action {
            b't' => self.kitty_transmit(&command, &payload).map(|_| ()),
            b'T' => self.kitty_transmit(&command, &payload).map(|stored| self.kitty_place(&command, stored)),
            b'q' => self.kitty_load(&command, &payload).map(|_| ()),
            b'p' => match self.kitty.images.get(&command.id) {
                Some(stored) => {
                    let pixels = stored.pixels.clone();
                    self.kitty_place(&command, pixels);
                    Ok(())
                },
                None => Err("ENOENT:no such image".to_owned()),
            },
            b'd' => {
                self.kitty_delete(&command);
                return;
            },
            _ => return,
        };
        self.kitty_reply(&command, result);
    }

    /// Answer a command that named its image, unless asked to be quiet.
    fn kitty_reply(&self, command: &Command, result: Result<(), String>) {
        if command.id == 0 && command.number == 0 {
            return;
        }
        let message = match &result {
            Ok(()) if command.quiet == 0 => "OK".to_owned(),
            Err(err) if command.quiet < 2 => err.clone(),
            _ => return,
        };
        let mut keys = format!("i={}", command.id);
        if command.number != 0 {
            keys.push_str(&format!(",I={}", command.number));
        }
        if command.placement != 0 {
            keys.push_str(&format!(",p={}", command.placement));
        }
        self.event_proxy.send_event(Event::PtyWrite(format!("\x1b_G{keys};{message}\x1b\\")));
    }

    /// The image a command carries: decoded payload, checked.
    fn kitty_load(&self, command: &Command, payload: &[u8]) -> Result<Pixels, String> {
        let payload: Vec<u8> = payload.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
        let decoded = Base64.decode(&payload).map_err(|_| "EINVAL:bad base64".to_owned())?;
        let mut data = match command.medium {
            b'd' => decoded,
            b'f' | b't' => {
                let path = String::from_utf8(decoded).map_err(|_| "EINVAL:bad file name".to_owned())?;
                let path = std::path::Path::new(&path);
                let size = std::fs::metadata(path).map_err(|err| format!("EBADF:{err}"))?.len() as usize;
                if size > MAX_IMAGE {
                    return Err("EFBIG:image too large".to_owned());
                }
                let data = std::fs::read(path).map_err(|err| format!("EBADF:{err}"))?;
                // A temporary file is ours to delete, if it looks like one.
                let temporary = path.to_string_lossy().contains("tty-graphics-protocol")
                    && (path.starts_with(std::env::temp_dir()) || path.starts_with("/tmp"));
                if command.medium == b't' && temporary {
                    let _ = std::fs::remove_file(path);
                }
                data
            },
            _ => return Err("EINVAL:transmission medium not supported".to_owned()),
        };
        if command.compression == b'z' {
            data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, MAX_IMAGE)
                .map_err(|_| "EINVAL:bad zlib data".to_owned())?;
        }
        let (width, height) = (command.width, command.height);
        let pixels = |bytes: usize| {
            let wanted = width as usize * height as usize * bytes;
            if width == 0 || height == 0 || wanted > MAX_IMAGE {
                Err("EINVAL:bad size".to_owned())
            } else if data.len() < wanted {
                Err("ENODATA:too little data".to_owned())
            } else {
                Ok(wanted)
            }
        };
        match command.format {
            100 => {
                let size = imagesize::blob_size(&data).map_err(|_| "EBADPNG:not an image".to_owned())?;
                if data.len() > MAX_IMAGE {
                    return Err("EFBIG:image too large".to_owned());
                }
                Ok(Pixels { format: ImageFormat::Encoded, data: data.into(), width: size.width as u32, height: size.height as u32 })
            },
            32 => {
                let wanted = pixels(4)?;
                data.truncate(wanted);
                Ok(Pixels { format: ImageFormat::Rgba, data: data.into(), width, height })
            },
            24 => {
                let wanted = pixels(3)?;
                let rgba: Vec<u8> =
                    data[..wanted].chunks_exact(3).flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255]).collect();
                Ok(Pixels { format: ImageFormat::Rgba, data: rgba.into(), width, height })
            },
            _ => Err("EINVAL:format not supported".to_owned()),
        }
    }

    /// Load and keep an image by id (evicting the oldest past the limit).
    fn kitty_transmit(&mut self, command: &Command, payload: &[u8]) -> Result<Pixels, String> {
        let pixels = self.kitty_load(command, payload)?;
        if command.id != 0 {
            let kitty = &mut self.kitty;
            if let Some(old) = kitty.images.remove(&command.id) {
                kitty.stored -= old.pixels.data.len();
            }
            kitty.age += 1;
            kitty.stored += pixels.data.len();
            kitty.images.insert(command.id, Stored { pixels: pixels.clone(), age: kitty.age });
            while kitty.stored > MAX_STORED {
                let Some((&oldest, _)) = kitty.images.iter().min_by_key(|(_, s)| s.age) else { break };
                let gone = kitty.images.remove(&oldest).unwrap();
                kitty.stored -= gone.pixels.data.len();
            }
        }
        Ok(pixels)
    }

    /// Show an image at the cursor, at the size asked for in cells or its
    /// own size in pixels.
    fn kitty_place(&mut self, command: &Command, Pixels { format, data, width, height }: Pixels) {
        let cell = (f32::from(self.cell_size.0), f32::from(self.cell_size.1));
        let (natural_columns, natural_rows) = (width as f32 / cell.0, height as f32 / cell.1);
        let size = match (command.columns, command.rows) {
            (0, 0) => (natural_columns, natural_rows),
            (c, 0) => (c as f32, natural_rows * c as f32 / natural_columns),
            (0, r) => (natural_columns * r as f32 / natural_rows, r as f32),
            (c, r) => (c as f32, r as f32),
        };
        let image = Image::new(format, data, (width, height), size, command.id);
        self.place_image(image, !command.stay);
    }

    /// Remove placements: all on screen (`a`), an image's (`i`), or the one
    /// at the cursor (`c`). Upper case also forgets the images.
    fn kitty_delete(&mut self, command: &Command) {
        let free = command.delete.is_ascii_uppercase();
        let doomed = match command.delete.to_ascii_lowercase() {
            b'i' => Doomed::Kitty(command.id),
            b'c' => match self.grid[self.grid.cursor.point].image() {
                Some(tile) => Doomed::Placement(tile.image.id),
                None => return,
            },
            _ => Doomed::Visible,
        };
        let top = if matches!(doomed, Doomed::Kitty(_)) { self.topmost_line().0 } else { 0 };
        let mut freed = Vec::new();
        for line in top..self.screen_lines() as i32 {
            for column in 0..self.columns() {
                let cell = &mut self.grid[Line(line)][Column(column)];
                if let Some(tile) = cell.image().filter(|tile| doomed.matches(&tile.image)) {
                    freed.push(tile.image.kitty_id);
                    *cell = Cell::default();
                }
            }
        }
        if free {
            if command.delete == b'I' {
                freed.push(command.id);
            }
            if command.delete == b'A' {
                self.kitty.images.clear();
                self.kitty.stored = 0;
            }
            for id in freed {
                if let Some(gone) = self.kitty.images.remove(&id) {
                    self.kitty.stored -= gone.pixels.data.len();
                }
            }
        }
        self.mark_fully_damaged();
    }
}
