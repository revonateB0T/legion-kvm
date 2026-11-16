//! Input from this VM's virtual keyboard, mouse, and tablet, never host input devices.
use anyhow::{Context, Result, ensure};
use legion_display_channel::input_events::{Batch, Event, TimedEvent};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, OpenOptionsExt},
    },
    path::Path,
};

#[derive(Clone, Copy)]
struct Axis {
    value: i32,
    min: i32,
    max: i32,
}
impl Axis {
    fn normalized(self) -> f64 {
        ((i64::from(self.value) - i64::from(self.min)) as f64
            / (i64::from(self.max) - i64::from(self.min)) as f64)
            .clamp(0.0, 1.0)
    }
}

fn axis(file: &File, code: u8) -> Result<Option<Axis>> {
    let request = rustix::ioctl::opcode::read::<[i32; 6]>(b'E', 0x40 + code);
    let mut info = [0_i32; 6];
    // SAFETY: the evdev EVIOCGABS request writes one initialized input_absinfo
    // (six i32 fields), into this live array for the duration of the ioctl.
    if unsafe { libc::ioctl(file.as_raw_fd(), request as _, info.as_mut_ptr()) } < 0 {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOTTY)) {
            return Ok(None);
        }
        return Err(error).context("query virtual input axis");
    }
    Ok((info[2] > info[1]).then_some(Axis {
        value: info[0],
        min: info[1],
        max: info[2],
    }))
}

struct Decoder {
    absolute: Option<[Axis; 2]>,
    abs_dirty: bool,
    rel_delta: [i32; 2],
    rel_dirty: bool,
    cursor: [f64; 2],
    dropped: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            absolute: None,
            abs_dirty: false,
            rel_delta: [0, 0],
            rel_dirty: false,
            cursor: [0.5, 0.5],
            dropped: false,
        }
    }
}

impl Decoder {
    fn flush(&mut self, width: u32, height: u32, events: &mut Vec<Event>) {
        if self.abs_dirty
            && let Some([x, y]) = self.absolute
        {
            let nx = x.normalized();
            let ny = y.normalized();
            self.cursor = [nx, ny];
            events.push(Event::Absolute { x: nx, y: ny });
        }
        self.abs_dirty = false;

        if self.rel_dirty {
            let dx = f64::from(self.rel_delta[0]);
            let dy = f64::from(self.rel_delta[1]);
            if dx != 0.0 || dy != 0.0 {
                events.push(Event::Relative {
                    dx,
                    dy,
                    raw_dx: dx,
                    raw_dy: dy,
                });
                if width > 0 && height > 0 {
                    self.cursor[0] = (self.cursor[0] + dx / f64::from(width)).clamp(0.0, 1.0);
                    self.cursor[1] = (self.cursor[1] + dy / f64::from(height)).clamp(0.0, 1.0);
                    events.push(Event::Absolute {
                        x: self.cursor[0],
                        y: self.cursor[1],
                    });
                }
            }
            self.rel_delta = [0, 0];
            self.rel_dirty = false;
        }
    }

    fn event(
        &mut self,
        kind: u16,
        code: u16,
        value: i32,
        width: u32,
        height: u32,
        events: &mut Vec<Event>,
    ) {
        if kind == 0 && code == 3 {
            // SYN_DROPPED: do not retain stuck keys or partial motion.
            events.push(Event::Reset);
            self.abs_dirty = false;
            self.rel_delta = [0, 0];
            self.rel_dirty = false;
            self.dropped = true;
            return;
        }
        if self.dropped {
            if kind == 0 && code == 0 {
                self.dropped = false;
            }
            return;
        }
        match (kind, code) {
            (0, 0) => self.flush(width, height, events),
            // EV_ABS: ABS_X (0) / ABS_Y (1)
            (3, 0..=1) => {
                if let Some(axes) = &mut self.absolute {
                    axes[usize::from(code)].value = value;
                    self.abs_dirty = true;
                }
            }
            // EV_REL: REL_X (0) / REL_Y (1)
            (2, 0..=1) => {
                let idx = usize::from(code);
                self.rel_delta[idx] = self.rel_delta[idx].saturating_add(value);
                self.rel_dirty = true;
            }
            // EV_KEY: buttons and keys
            (1, _) if value == 0 || value == 1 => {
                self.flush(width, height, events);
                events.push(if (0x110..=0x117).contains(&code) {
                    Event::Button {
                        code: u32::from(code),
                        down: value == 1,
                    }
                } else {
                    Event::Key {
                        code: u32::from(code),
                        down: value == 1,
                    }
                });
            }
            // EV_REL: REL_HWHEEL (6) / REL_WHEEL (8)
            (2, 6 | 8) => {
                self.flush(width, height, events);
                events.push(Event::Axis {
                    axis: u32::from(code == 6),
                    value: -f64::from(value) * 15.0,
                    v120: value.saturating_mul(-120),
                    stop: false,
                    source: 0,
                });
            }
            _ => {} // Keyboard repeats are generated by the Wayland client.
        }
    }
}

struct Device {
    file: File,
    decoder: Decoder,
}
pub(crate) struct Input {
    devices: Vec<Device>,
    started: bool,
}
impl Input {
    pub(crate) fn open() -> Result<Self> {
        let mut devices = Vec::new();
        if Path::new("/dev/input").is_dir() {
            for entry in fs::read_dir("/dev/input")? {
                let entry = entry?;
                if !entry.file_name().as_encoded_bytes().starts_with(b"event")
                    || !entry.file_type()?.is_char_device()
                {
                    continue;
                }
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(entry.path())
                    .context("open guest virtual input device")?;
                let absolute = axis(&file, 0)?.zip(axis(&file, 1)?).map(|(x, y)| [x, y]);
                devices.push(Device {
                    file,
                    decoder: Decoder {
                        absolute,
                        ..Default::default()
                    },
                });
            }
        }
        Ok(Self {
            devices,
            started: false,
        })
    }
    pub(crate) fn poll(&mut self, width: u32, height: u32, now_us: u64) -> Result<Batch> {
        let mut events = Vec::new();
        if !self.started && !self.devices.is_empty() {
            events.extend([
                Event::Focus {
                    keyboard: true,
                    focused: true,
                },
                Event::Focus {
                    keyboard: false,
                    focused: true,
                },
            ]);
            self.started = true;
        }
        for device in &mut self.devices {
            // Linux x86-64 input_event: timeval, u16 type/code, i32 value.
            for _ in 0..64 {
                let mut bytes = [0_u8; 24];
                match device.file.read(&mut bytes) {
                    Ok(0) => anyhow::bail!("virtual input device disconnected"),
                    Ok(size) => {
                        ensure!(size == bytes.len(), "incomplete virtual input event");
                        let kind = u16::from_ne_bytes([bytes[16], bytes[17]]);
                        let code = u16::from_ne_bytes([bytes[18], bytes[19]]);
                        let value =
                            i32::from_ne_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
                        device
                            .decoder
                            .event(kind, code, value, width, height, &mut events);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("read guest virtual input"),
                }
            }
        }
        Ok(Batch {
            width,
            height,
            // Both virtio-mouse EV_REL and tablet EV_ABS produce relative
            // events, so pointer-lock clients can rely on relative input.
            relative_supported: true,
            events: events
                .into_iter()
                .map(|event| TimedEvent {
                    time_us: now_us,
                    event,
                })
                .collect(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/evdev.rs"]
mod tests;
