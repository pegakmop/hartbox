//! Host terminal integration: raw mode for the guest console and a stdin
//! reader thread that feeds the UART.

use std::io::Read;
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::thread;

/// Ctrl-A x quits the emulator, as in QEMU's monitor escape.
const ESCAPE: u8 = 0x01;

static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// Whether standard output is a terminal.
pub fn stdout_is_tty() -> bool {
    // SAFETY: isatty only inspects the file descriptor.
    unsafe { libc::isatty(1) != 0 }
}

/// Make guest output readable in a log or pipe: CRLF becomes LF, and the
/// cursor-position query a shell sends to a terminal (ESC [ 6 n) is dropped,
/// since no terminal is there to answer it.
pub fn plain_text(bytes: &[u8]) -> Vec<u8> {
    const DSR: &[u8] = b"\x1b[6n";
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(DSR) {
            i += DSR.len();
        } else if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Restores the host terminal when dropped.
pub struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    /// Put stdin into raw mode if it is a terminal. Keystrokes, including
    /// Ctrl-C, then reach the guest unchanged.
    pub fn enable_raw_mode() -> Self {
        // SAFETY: isatty/tcgetattr/tcsetattr only read or write the given fd
        // and a termios struct we own.
        unsafe {
            if libc::isatty(0) == 0 {
                return TerminalGuard { active: false };
            }
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut orig) != 0 {
                return TerminalGuard { active: false };
            }
            let _ = ORIGINAL_TERMIOS.set(orig);
            let mut raw = orig;
            raw.c_iflag &= !(libc::IGNBRK | libc::BRKINT | libc::PARMRK | libc::ISTRIP | libc::INLCR | libc::IGNCR | libc::ICRNL | libc::IXON);
            raw.c_oflag &= !libc::OPOST;
            raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            raw.c_cflag |= libc::CS8;
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return TerminalGuard { active: false };
            }
            TerminalGuard { active: true }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            restore_terminal();
        }
    }
}

/// Restore the terminal settings saved by `enable_raw_mode`, if any.
pub fn restore_terminal() {
    if let Some(orig) = ORIGINAL_TERMIOS.get() {
        // SAFETY: restores a termios value previously read from fd 0.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, orig);
        }
    }
}

/// Spawn a thread that forwards stdin to the guest. The channel closes on EOF.
pub fn spawn_stdin_reader() -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 1024];
            let mut escape_pending = false;
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut chunk = Vec::with_capacity(n);
                for &b in &buf[..n] {
                    if escape_pending {
                        escape_pending = false;
                        if b == b'x' || b == b'X' {
                            restore_terminal();
                            eprintln!("\r\n[rvemu] quit requested");
                            std::process::exit(0);
                        }
                        if b == ESCAPE {
                            chunk.push(ESCAPE);
                            continue;
                        }
                        chunk.push(ESCAPE);
                        chunk.push(b);
                    } else if b == ESCAPE {
                        escape_pending = true;
                    } else {
                        chunk.push(b);
                    }
                }
                if !chunk.is_empty() && tx.send(chunk).is_err() {
                    break;
                }
            }
        })
        .expect("failed to spawn stdin reader");
    rx
}
