//! The virtual terminal bootmap draws on.
//!
//! bootmap uses a VT of its own (12 by default) instead of the console's.
//! Whatever is in front receives the keyboard, and when a VT in front is put
//! back into text mode the kernel repaints the text console over us even
//! though we still hold the display. A login prompt starting on tty1 does
//! exactly that to tty1, so we stay off it.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_TEXT: libc::c_int = 0;
const KD_GRAPHICS: libc::c_int = 1;
const VT_GETSTATE: libc::c_ulong = 0x5603;
const VT_ACTIVATE: libc::c_ulong = 0x5606;

/// `struct vt_stat` from <linux/vt.h>.
#[repr(C)]
#[derive(Default)]
struct VtStat {
    v_active: libc::c_ushort,
    v_signal: libc::c_ushort,
    v_state: libc::c_ushort,
}

pub struct Vt {
    tty: File,
    /// VT to bring back to the front when we are done.
    back_to: Option<libc::c_int>,
    saved: Option<libc::termios>,
}

impl Vt {
    /// Bring VT `n` to the front in graphics mode, with its keyboard input
    /// readable raw through `read_keys`. Dropping this goes back to the VT
    /// that was in front before.
    pub fn enter(n: u32) -> Option<Self> {
        // O_NOCTTY: a service is a session leader without a terminal, and
        // would otherwise adopt this one (and its hangups and signals).
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(format!("/dev/tty{n}"))
            .inspect_err(|e| eprintln!("bootmap: /dev/tty{n}: {e}"))
            .ok()?;
        let fd = tty.as_raw_fd();
        let mut stat = VtStat::default();
        // SAFETY (all ioctls below): `fd` is an open VT, and each request is
        // given the argument type <linux/vt.h> and <linux/kd.h> document.
        let front = (unsafe { libc::ioctl(fd, VT_GETSTATE, &mut stat) } == 0).then_some(stat.v_active as libc::c_int);
        let back_to = front.filter(|&f| f != n as libc::c_int);
        unsafe {
            libc::ioctl(fd, VT_ACTIVATE, n as libc::c_int);
            libc::ioctl(fd, KDSETMODE, KD_GRAPHICS);
        }

        // Keys as typed: no line editing, no echo, no signals from Ctrl-C.
        // SAFETY: termios is plain data; tcgetattr fills it in on success.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        let saved = (unsafe { libc::tcgetattr(fd, &mut term) } == 0).then_some(term);
        if saved.is_some() {
            term.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            term.c_iflag &= !(libc::IXON | libc::IXOFF);
            term.c_iflag |= libc::ICRNL;
            term.c_cc[libc::VMIN] = 0;
            term.c_cc[libc::VTIME] = 0;
            // SAFETY: `term` is a valid termios for this fd.
            unsafe {
                libc::tcsetattr(fd, libc::TCSANOW, &term);
                libc::tcflush(fd, libc::TCIFLUSH);
            }
        }
        Some(Self { tty, back_to, saved })
    }

    /// Keys typed since the last call (never blocks).
    pub fn read_keys(&mut self, buf: &mut [u8]) -> usize {
        self.tty.read(buf).unwrap_or(0)
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        let fd = self.tty.as_raw_fd();
        // SAFETY: as in `enter`.
        unsafe {
            if let Some(term) = &self.saved {
                libc::tcflush(fd, libc::TCIFLUSH);
                libc::tcsetattr(fd, libc::TCSANOW, term);
            }
            libc::ioctl(fd, KDSETMODE, KD_TEXT);
            if let Some(back) = self.back_to {
                libc::ioctl(fd, VT_ACTIVATE, back);
            }
        }
    }
}
