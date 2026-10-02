//! The terminal: raw mode, size, and bytes in as [`Key`]s.
//!
//! Three things in here are load-bearing for the door rather than for looks.
//!
//! **A menu that dies must not leave the login in raw mode.** The release profile is
//! `panic = "abort"`, so `Drop` never runs on a panic in the shipped binary. A panic hook does:
//! it restores the saved termios and leaves the alternate screen before the panic message is
//! printed, so the message lands on an ordinary screen and the door's fallback shell gets a
//! terminal that echoes.
//!
//! **Waiting writes nothing.** The link is metered and an idle menu emits zero bytes, so
//! [`RawTerminal::wait`] only ever reads; and mouse reporting is clicks only (`?1000` with SGR
//! `?1006`), never `?1002`/`?1003`, which would send a report every time a pointer crossed the
//! window.
//!
//! **No escape sequence leaks as characters.** A half-read `ESC [ 5 ~` that came out as `[`,
//! `5` and `~` would type into the menu — and `Esc` quits it. The [`Decoder`] holds an
//! incomplete sequence across reads and resolves it only after [`ESC_WAIT`] of silence.
//!
//! The C functions are declared here rather than taken from the `libc` crate, because this
//! crate has no dependencies — see Cargo.toml. Every layout and constant below is the same on
//! x86_64 and aarch64 Linux musl, the two targets this ships for; each says why.

use crate::ui::{Key, Terminal};
use std::ffi::{c_int, c_short, c_ulong, c_void};
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, Once, OnceLock, TryLockError};
use std::time::{Duration, Instant};

/// How long a lone `ESC` waits for the rest of a sequence before it counts as the `Esc` key.
///
/// A terminal writes a whole sequence in one write and ssh forwards it in one packet, so the
/// rest normally arrives with the `ESC`. The wait is for the rare split; 50 ms rather than
/// the customary 25 because the cost of guessing wrong here is unusually high — `Esc` quits
/// the menu, quitting ends the ssh login, and a fresh login costs a Cloudflare Access
/// handshake — while 50 ms on a deliberate `Esc` is still below what anyone can feel.
pub const ESC_WAIT: Duration = Duration::from_millis(50);

/// Longest escape sequence held while waiting for its final byte. The longest real one this
/// reads is an SGR mouse report, `ESC [ < 65 ; 65535 ; 65535 M`, at 18 bytes; past this it is
/// garbage, and holding it forever would let a stuck sender grow the buffer without bound.
const MAX_SEQ: usize = 64;

const ESC: u8 = 0x1b;

/// Alternate screen, cursor hidden, mouse clicks on (`?1000`), reported as SGR (`?1006`) so
/// coordinates past column 223 survive and a release is told from a press.
///
/// NEVER `?1002` or `?1003`: those report motion, and motion reports put bytes on the metered
/// link whenever a pointer crosses the window — the idle traffic the menu exists not to make.
/// A test pins it.
const ENTER_SEQ: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h";

/// Exactly what [`ENTER_SEQ`] turns on, turned off in the reverse order. A test pins that too.
const LEAVE_SEQ: &[u8] = b"\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l";

// ── the C side ──────────────────────────────────────────────────────────────

/// musl's `struct termios`, from `arch/generic/bits/termios.h`. Neither x86_64 nor aarch64
/// overrides that header (only mips, powerpc and sparc do), so the layout is the same on
/// both: four `unsigned int` flag words, `c_line`, `NCCS = 32` control characters and the two
/// speeds — 60 bytes with the padding before the speeds. Only pointers to it cross into C,
/// and `cfmakeraw` is what edits the fields, so a wrong layout here would show up as the
/// size assertion below failing rather than as a corrupt terminal.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}
const _: () = assert!(std::mem::size_of::<Termios>() == 60);

/// `struct winsize`: four `unsigned short`s, in the kernel's uapi and identical everywhere.
#[repr(C)]
#[derive(Default)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

/// `struct pollfd`: `int`, `short`, `short` — the same on every Linux.
#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

// x86_64 uses the asm-generic ioctl numbers for terminals, as aarch64 does; 0x5413 on both.
const TIOCGWINSZ: c_int = 0x5413;
// tcsetattr's `optional_actions`: 0 is TCSANOW in the generic and x86 headers alike. NOW
// rather than DRAIN because DRAIN waits for output to reach the other end, and a stalled ssh
// connection would then hold the door shut inside a mode change. Linux applies output
// processing when bytes are written, not when they leave, so nothing already written changes.
const TCSANOW: c_int = 0;
// asm-generic/poll.h, which x86 includes unchanged.
const POLLIN: c_short = 0x001;
const POLLERR: c_short = 0x008;
const POLLHUP: c_short = 0x010;
const POLLNVAL: c_short = 0x020;
// x86_64 and aarch64 share these signal numbers (asm-generic/signal.h; x86's matches for all
// three), unlike the old SIGWINCH = 20 of the BSDs and mips.
const SIGINT: c_int = 2;
const SIGQUIT: c_int = 3;
const SIGWINCH: c_int = 28;
// The dispositions as `sighandler_t` values: SIG_DFL is 0, SIG_IGN is 1, SIG_ERR is -1.
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const SIG_ERR: usize = usize::MAX;

unsafe extern "C" {
    fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, act: c_int, t: *const Termios) -> c_int;
    fn cfmakeraw(t: *mut Termios);
    // musl declares the request as `int` (glibc's is `unsigned long`); the kernel reads 32
    // bits either way.
    fn ioctl(fd: c_int, request: c_int, ...) -> c_int;
    // `nfds_t` is `unsigned long` in musl.
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    // `signal` rather than `sigaction`, because musl's `struct sigaction` is a layout to get
    // right and `signal` is not: musl implements it as `sigaction` with SA_RESTART, which is
    // what is wanted — reads restart, and poll(2) is never restarted whatever the flags
    // (signal(7)), so a resize still wakes a wait.
    fn signal(sig: c_int, handler: usize) -> usize;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn __errno_location() -> *mut c_int;
}

// ── decoding ────────────────────────────────────────────────────────────────

/// Bytes in, [`Key`]s out. Pure: it neither reads nor waits, so every split of every sequence
/// can be tested without a terminal.
///
/// Feed it whatever `read` returned; it keeps an incomplete sequence (or UTF-8 character) for
/// the next feed. When nothing more arrived within [`ESC_WAIT`], call [`Decoder::timeout`]:
/// a lone `ESC` is then the `Esc` key, and anything else still held is an incomplete sequence
/// and is dropped.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

enum Parsed {
    /// A key, from this many bytes.
    Key(Key, usize),
    /// This many bytes meant nothing the menu reads — an unmapped sequence, a mouse release,
    /// an invalid byte. Consumed whole so none of it reaches the menu as characters.
    Drop(usize),
    /// Needs more bytes to tell.
    Incomplete,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// Whether bytes are held waiting for the rest of a sequence — when the caller should
    /// wait [`ESC_WAIT`] for more before calling [`Decoder::timeout`].
    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.buf.extend_from_slice(bytes);
        self.drain(false)
    }

    /// No byte arrived within [`ESC_WAIT`]: resolve whatever is held.
    pub fn timeout(&mut self) -> Vec<Key> {
        self.drain(true)
    }

    fn drain(&mut self, timed_out: bool) -> Vec<Key> {
        let mut keys = Vec::new();
        let mut i = 0;
        while i < self.buf.len() {
            match parse(&self.buf[i..]) {
                Parsed::Key(k, n) => {
                    keys.push(k);
                    i += n;
                }
                Parsed::Drop(n) => i += n,
                Parsed::Incomplete if !timed_out => break,
                Parsed::Incomplete => {
                    // Incomplete only ever describes the tail, so this is the last of it.
                    if self.buf[i..] == [ESC] {
                        keys.push(Key::Esc);
                    }
                    i = self.buf.len();
                }
            }
        }
        self.buf.drain(..i);
        keys
    }
}

fn parse(b: &[u8]) -> Parsed {
    match b[0] {
        ESC => parse_escape(b),
        // Raw mode turns off ICRNL, so Enter is `\r`; `\n` is Ctrl-J, and a pasted line ends
        // in either.
        b'\r' | b'\n' => Parsed::Key(Key::Enter, 1),
        // Every other ASCII byte, controls included: `ui::Key` has no Ctrl variant, so Ctrl-C
        // (0x03 — raw mode turns ISIG off, so it arrives as a byte, not a signal) rides on
        // `Char` for the menu to bind or ignore.
        c if c < 0x80 => Parsed::Key(Key::Char(c as char), 1),
        _ => match utf8(b) {
            Utf8::Char(c, n) => Parsed::Key(Key::Char(c), n),
            Utf8::Invalid(n) => Parsed::Drop(n),
            Utf8::Incomplete => Parsed::Incomplete,
        },
    }
}

fn parse_escape(b: &[u8]) -> Parsed {
    let Some(&next) = b.get(1) else {
        return Parsed::Incomplete;
    };
    match next {
        b'[' => parse_csi(b),
        b'O' => match b.get(2) {
            None => Parsed::Incomplete,
            // SS3: what arrows, Home and End send in application cursor mode.
            Some(&f) if (0x40..=0x7e).contains(&f) => match f {
                b'A' => Parsed::Key(Key::Up, 3),
                b'B' => Parsed::Key(Key::Down, 3),
                b'H' => Parsed::Key(Key::Home, 3),
                b'F' => Parsed::Key(Key::End, 3),
                _ => Parsed::Drop(3),
            },
            // Not a final byte, so not SS3: an Alt-O chord, dropped like any other.
            Some(_) => Parsed::Drop(2),
        },
        // Two Escs pressed, or coalesced on a slow link: the first is a key in its own right.
        ESC => Parsed::Key(Key::Esc, 1),
        // An Alt chord on a multi-byte character: drop the character with it.
        c if c >= 0x80 => match utf8(&b[1..]) {
            Utf8::Char(_, n) => Parsed::Drop(1 + n),
            Utf8::Invalid(_) => Parsed::Drop(1),
            Utf8::Incomplete => Parsed::Incomplete,
        },
        // ESC then a printable or control byte is an Alt chord (meta sends escape). The menu
        // binds none, and reading it as Esc-then-key would quit on Alt-anything, so it is
        // dropped whole. String sequences (OSC, DCS) start this way too; they are only ever
        // replies to queries, and the menu sends none.
        _ => Parsed::Drop(2),
    }
}

fn parse_csi(b: &[u8]) -> Parsed {
    // ECMA-48: parameter bytes 0x30–0x3F, intermediates 0x20–0x2F, then one final byte
    // 0x40–0x7E.
    let mut i = 2;
    while i < b.len() && (0x20..=0x3f).contains(&b[i]) {
        i += 1;
    }
    if i == b.len() {
        return if b.len() > MAX_SEQ {
            Parsed::Drop(b.len())
        } else {
            Parsed::Incomplete
        };
    }
    let fin = b[i];
    if !(0x40..=0x7e).contains(&fin) {
        // Broken off by a control, another ESC or a high byte: drop what came before it and
        // read that byte afresh, so a sequence cut short does not swallow the next one.
        return Parsed::Drop(i);
    }
    let params = &b[2..i];
    let len = i + 1;

    if params.first() == Some(&b'<') && matches!(fin, b'M' | b'm') {
        return sgr_mouse(&params[1..], fin, len);
    }
    if params.is_empty() && fin == b'M' {
        // X10 mouse, `ESC [ M` then three raw bytes: what a terminal without SGR (`?1006`)
        // sends for `?1000`. Those three bytes must be consumed or they would leak as text.
        let Some(raw) = b.get(len..len + 3) else {
            return Parsed::Incomplete;
        };
        let button = u32::from(raw[0].saturating_sub(32));
        let col = u32::from(raw[1].saturating_sub(33));
        let row = u32::from(raw[2].saturating_sub(33));
        return mouse(button, col, row, true, len + 3);
    }
    // A private marker (`?`, `>`, `=`, `<` other than SGR mouse) or an intermediate byte: a
    // report or a reply, never a key this reads.
    if params
        .iter()
        .any(|&p| (0x3c..=0x3f).contains(&p) || p < 0x30)
    {
        return Parsed::Drop(len);
    }
    // Modifiers ride in a second parameter (`ESC [ 1 ; 5 A` is Ctrl-Up); a modified arrow is
    // still that arrow.
    let first = params
        .split(|&p| p == b';')
        .next()
        .and_then(|p| std::str::from_utf8(p).ok())
        .and_then(|p| p.parse::<u32>().ok());
    let key = match fin {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'~' => match first {
            // 1 and 4 are the VT220's Find and Select, which xterm sends for Home and End;
            // 7 and 8 are rxvt's.
            Some(1 | 7) => Some(Key::Home),
            Some(4 | 8) => Some(Key::End),
            Some(5) => Some(Key::PageUp),
            Some(6) => Some(Key::PageDown),
            _ => None,
        },
        _ => None,
    };
    match key {
        Some(k) => Parsed::Key(k, len),
        None => Parsed::Drop(len),
    }
}

/// `ESC [ < button ; x ; y M` (press) or `m` (release), coordinates 1-based.
fn sgr_mouse(params: &[u8], fin: u8, len: usize) -> Parsed {
    let fields: Vec<Option<u32>> = params
        .split(|&p| p == b';')
        .map(|f| std::str::from_utf8(f).ok()?.parse().ok())
        .collect();
    let [Some(button), Some(x), Some(y)] = fields[..] else {
        return Parsed::Drop(len);
    };
    mouse(
        button,
        x.saturating_sub(1),
        y.saturating_sub(1),
        fin == b'M',
        len,
    )
}

/// A mouse report's button word, decoded the same for SGR and X10. Clicks are a left press
/// and wheels are 64/65; releases, the other buttons and motion (bit 32 — which `?1000`
/// never sends, but a terminal is not to be trusted) are dropped. Shift, Meta and Ctrl (4, 8,
/// 16) are masked off: a Ctrl-click is still a click.
fn mouse(button: u32, col: u32, row: u32, press: bool, len: usize) -> Parsed {
    let (Ok(col), Ok(row)) = (u16::try_from(col), u16::try_from(row)) else {
        return Parsed::Drop(len);
    };
    let key = match (press, button & !(4 | 8 | 16)) {
        (true, 0) => Key::Click { row, col },
        (true, 64) => Key::WheelUp,
        (true, 65) => Key::WheelDown,
        _ => return Parsed::Drop(len),
    };
    Parsed::Key(key, len)
}

enum Utf8 {
    Char(char, usize),
    Invalid(usize),
    Incomplete,
}

fn utf8(b: &[u8]) -> Utf8 {
    let n = match b[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        // A stray continuation byte, an overlong lead (C0, C1) or past U+10FFFF.
        _ => return Utf8::Invalid(1),
    };
    for (k, &c) in b.iter().enumerate().take(n).skip(1) {
        if c & 0xc0 != 0x80 {
            // Cut short: drop the lead and what continued it, then read this byte afresh.
            return Utf8::Invalid(k);
        }
    }
    if b.len() < n {
        return Utf8::Incomplete;
    }
    // The lead and continuation shape are right; this rejects the overlongs and surrogates
    // the shape alone lets through.
    match std::str::from_utf8(&b[..n])
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(c) => Utf8::Char(c, n),
        None => Utf8::Invalid(n),
    }
}

// ── signals and the panic hook ──────────────────────────────────────────────

/// Set by SIGWINCH, taken by [`RawTerminal::wait`].
static WINCH: AtomicBool = AtomicBool::new(false);
/// The write end of the self-pipe, so a resize wakes a wait that is already blocked in
/// poll(2). The flag alone cannot: a signal landing between checking it and entering poll
/// would be noticed only at the next keypress.
static WINCH_WAKE: AtomicI32 = AtomicI32::new(-1);
/// The read end, polled beside the input.
static WINCH_PIPE: OnceLock<UnixStream> = OnceLock::new();

/// What the panic hook needs to put the terminal back: the input fd whose termios was
/// changed, the output fd the leave sequence goes to, and the termios to restore. `Some`
/// exactly while a [`RawTerminal`] is raw.
static RAW: Mutex<Option<(c_int, c_int, Termios)>> = Mutex::new(None);

static INSTALL: Once = Once::new();

extern "C" fn on_winch(_: c_int) {
    // errno is saved because write(2) can set it, and the interrupted code may be about to
    // read the errno of the call this signal cut into.
    // SAFETY: __errno_location is async-signal-safe and returns this thread's errno.
    let saved = unsafe { *__errno_location() };
    WINCH.store(true, Ordering::SeqCst);
    let fd = WINCH_WAKE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: write(2) is async-signal-safe; the socket is non-blocking, so a full buffer
        // (a burst of resizes nobody has read yet) fails instead of hanging the handler, and
        // the flag already says what the byte would have.
        unsafe { write(fd, (&raw const byte).cast(), 1) };
    }
    // SAFETY: as above.
    unsafe { *__errno_location() = saved };
}

/// The handler for SIGINT and SIGQUIT while a child has the terminal: nothing. Not SIG_IGN,
/// because an ignored signal stays ignored across exec and the child would lose Ctrl-C; a
/// handler is reset to the default by exec, so the child gets the signal and the menu, in
/// the same process group, survives it.
extern "C" fn on_ignored(_: c_int) {}

/// Once per process: the SIGWINCH handler and its self-pipe, and the panic hook.
fn install() {
    INSTALL.call_once(|| {
        if let Ok((r, w)) = UnixStream::pair() {
            if r.set_nonblocking(true).is_ok() && w.set_nonblocking(true).is_ok() {
                // The write end lives as long as the process: the handler may run at any time.
                WINCH_WAKE.store(w.into_raw_fd(), Ordering::SeqCst);
                let _ = WINCH_PIPE.set(r);
            }
        }
        // SAFETY: installs a handler that only touches atomics and calls write(2).
        unsafe { signal(SIGWINCH, on_winch as extern "C" fn(c_int) as usize) };

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // First, so the message below lands on the main screen in cooked mode.
            restore_after_panic();
            previous(info);
        }));
    });
}

/// Put the terminal back from inside a panic. `try_lock`, not `lock`: the panic may have
/// started while this thread held the lock, and a std mutex is not re-entrant. A poisoned
/// lock still holds a good termios.
fn restore_after_panic() {
    let mut guard = match RAW.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    if let Some((input, output, saved)) = guard.take() {
        let _ = write_fd(output, LEAVE_SEQ);
        // SAFETY: a termios read from this fd by tcgetattr.
        unsafe { tcsetattr(input, TCSANOW, &saved) };
    }
}

fn take_winch() -> bool {
    if let Some(mut pipe) = WINCH_PIPE.get() {
        let mut sink = [0u8; 64];
        // Drain every wake byte; the flag is the record of what happened.
        while matches!(io::Read::read(&mut pipe, &mut sink), Ok(n) if n > 0) {}
    }
    WINCH.swap(false, Ordering::SeqCst)
}

fn write_fd(fd: c_int, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: a valid slice; write(2) reads at most its length.
        let n = unsafe { write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

// ── the terminal ────────────────────────────────────────────────────────────

/// What [`RawTerminal::wait`] came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Never empty. A resize arrives as [`Key::Resize`] among them.
    Keys(Vec<Key>),
    Timeout,
}

/// The menu's terminal: raw, on the alternate screen, cursor hidden, mouse clicks reported.
/// Restored by [`Terminal::suspend`], by `Drop`, and by the panic hook.
pub struct RawTerminal {
    input: c_int,
    output: c_int,
    saved: Termios,
    raw: bool,
    decoder: Decoder,
    /// SIGINT's and SIGQUIT's dispositions from before `suspend`, while a child has the
    /// terminal. See [`on_ignored`].
    displaced: Option<(usize, usize)>,
}

impl RawTerminal {
    /// Take over stdin and stdout. Fails if stdin is not a terminal.
    pub fn enter() -> io::Result<RawTerminal> {
        RawTerminal::on(0, 1)
    }

    /// The same on any pair of fds, so the tests can run it on a pty of their own.
    fn on(input: c_int, output: c_int) -> io::Result<RawTerminal> {
        let mut saved = std::mem::MaybeUninit::<Termios>::zeroed();
        // SAFETY: tcgetattr fills the struct, whose layout is musl's (see Termios).
        if unsafe { tcgetattr(input, saved.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed is a valid Termios (plain integers), and tcgetattr succeeded.
        let saved = unsafe { saved.assume_init() };
        install();
        // A resize from before the menu existed is not one it needs telling about.
        take_winch();
        let mut term = RawTerminal {
            input,
            output,
            saved,
            raw: false,
            decoder: Decoder::new(),
            displaced: None,
        };
        term.go_raw()?;
        Ok(term)
    }

    fn go_raw(&mut self) -> io::Result<()> {
        let mut raw = self.saved;
        // SAFETY: a valid termios to edit in place.
        unsafe { cfmakeraw(&mut raw) };
        // Registered before the switch, so there is no instant at which the terminal is raw
        // and the panic hook does not know how to undo it.
        *lock_raw() = Some((self.input, self.output, self.saved));
        // SAFETY: a termios derived from one tcgetattr returned for this fd.
        if unsafe { tcsetattr(self.input, TCSANOW, &raw) } != 0 {
            let e = io::Error::last_os_error();
            *lock_raw() = None;
            return Err(e);
        }
        self.raw = true;
        self.decoder = Decoder::new();
        write_fd(self.output, ENTER_SEQ)
    }

    fn go_cooked(&mut self) -> io::Result<()> {
        if !self.raw {
            return Ok(());
        }
        // The leave sequence first, while still raw, then the mode: in the other order a
        // terminal in cooked mode would briefly echo a mouse report typed in between.
        let wrote = write_fd(self.output, LEAVE_SEQ);
        // SAFETY: the termios tcgetattr gave for this fd.
        let rc = unsafe { tcsetattr(self.input, TCSANOW, &self.saved) };
        let restored = if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
        *lock_raw() = None;
        self.raw = false;
        // Whatever was half-read belonged to the menu, and the next bytes belong to a child.
        self.decoder = Decoder::new();
        wrote.and(restored)
    }

    /// Wait up to `timeout` (forever with `None`) for input. Returns as soon as there is at
    /// least one key or a resize; bytes that decode to nothing (a mouse release) keep it
    /// waiting. **Writes nothing**: an idle menu emits zero bytes.
    ///
    /// A closed input (the ssh connection went away) is `UnexpectedEof`, so the menu exits
    /// instead of spinning on a descriptor that is always readable.
    pub fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Input> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if take_winch() {
                return Ok(Input::Keys(vec![Key::Resize]));
            }
            let ms = match deadline {
                None => -1,
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => millis_up(left),
                    _ => return Ok(Input::Timeout),
                },
            };
            let wake = WINCH_PIPE.get().map_or(-1, |p| p.as_raw_fd());
            let mut fds = [
                PollFd {
                    fd: self.input,
                    events: POLLIN,
                    revents: 0,
                },
                PollFd {
                    fd: wake,
                    events: POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: two valid pollfds; a negative fd is ignored by poll(2).
            let rc = unsafe { poll(fds.as_mut_ptr(), 2, ms) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            let mut keys = Vec::new();
            let ready = fds[0].revents;
            if ready & POLLIN != 0 {
                keys = self.read_keys()?;
            } else if ready & (POLLHUP | POLLERR | POLLNVAL) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the terminal went away",
                ));
            }
            if take_winch() {
                keys.push(Key::Resize);
            }
            if !keys.is_empty() {
                return Ok(Input::Keys(keys));
            }
        }
    }

    /// Read what is there, then keep reading while the decoder holds part of a sequence and
    /// more arrives within [`ESC_WAIT`].
    fn read_keys(&mut self) -> io::Result<Vec<Key>> {
        let mut keys = self.read_once()?;
        while self.decoder.pending() {
            let mut fd = [PollFd {
                fd: self.input,
                events: POLLIN,
                revents: 0,
            }];
            // SAFETY: one valid pollfd.
            let rc = unsafe { poll(fd.as_mut_ptr(), 1, millis_up(ESC_WAIT)) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if rc == 0 || fd[0].revents & POLLIN == 0 {
                keys.extend(self.decoder.timeout());
                break;
            }
            keys.extend(self.read_once()?);
        }
        Ok(keys)
    }

    fn read_once(&mut self) -> io::Result<Vec<Key>> {
        let mut buf = [0u8; 1024];
        loop {
            // SAFETY: a valid buffer; read(2) writes at most its length.
            let n = unsafe { read(self.input, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the terminal went away",
                ));
            }
            return Ok(self.decoder.feed(&buf[..n as usize]));
        }
    }

    /// Write `bytes` straight to the terminal. Unbuffered — one write(2) for a whole frame
    /// where the kernel takes it, which is also the fewest packets on the link — so there is
    /// nothing to flush.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_fd(self.output, bytes)
    }
}

impl Terminal for RawTerminal {
    /// Hand the terminal to a child: mouse off, cursor shown, main screen, the saved termios.
    /// SIGINT and SIGQUIT get a do-nothing handler meanwhile, so Ctrl-C in a cooked child
    /// does not also kill the menu that shares its process group.
    fn suspend(&mut self) -> io::Result<()> {
        let result = self.go_cooked();
        if self.displaced.is_none() {
            let handler = on_ignored as extern "C" fn(c_int) as usize;
            // SAFETY: the handler does nothing at all.
            let int = unsafe { signal(SIGINT, handler) };
            let quit = unsafe { signal(SIGQUIT, handler) };
            // A signal the menu was started with ignored stays ignored, for the child too.
            for (sig, prev) in [(SIGINT, int), (SIGQUIT, quit)] {
                if prev == SIG_IGN {
                    unsafe { signal(sig, SIG_IGN) };
                }
            }
            self.displaced = Some((int, quit));
        }
        result
    }

    /// Take it back. The alternate screen comes back blank, so the menu redraws in full; a
    /// resize while the child had it is still flagged and the next `wait` reports it.
    fn resume(&mut self) -> io::Result<()> {
        self.restore_signals();
        if self.raw {
            return Ok(());
        }
        self.go_raw()
    }
}

impl RawTerminal {
    fn restore_signals(&mut self) {
        if let Some((int, quit)) = self.displaced.take() {
            for (sig, prev) in [(SIGINT, int), (SIGQUIT, quit)] {
                let prev = if prev == SIG_ERR { SIG_DFL } else { prev };
                // SAFETY: putting back a disposition signal(2) handed us.
                unsafe { signal(sig, prev) };
            }
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = self.go_cooked();
        self.restore_signals();
    }
}

fn lock_raw() -> std::sync::MutexGuard<'static, Option<(c_int, c_int, Termios)>> {
    RAW.lock().unwrap_or_else(|p| p.into_inner())
}

/// Milliseconds for poll(2), rounded up so a wait never spins on a zero timeout just short
/// of its deadline.
fn millis_up(d: Duration) -> c_int {
    let ms = d.as_nanos().div_ceil(1_000_000);
    c_int::try_from(ms).unwrap_or(c_int::MAX)
}

/// The window's size as (cols, rows), from stdout, or stdin when stdout is not the terminal.
pub fn size() -> Option<(u16, u16)> {
    size_of(1).or_else(|| size_of(0))
}

fn size_of(fd: c_int) -> Option<(u16, u16)> {
    let mut ws = Winsize::default();
    // SAFETY: TIOCGWINSZ fills a struct winsize.
    let rc = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws as *mut Winsize) };
    // A pty nobody has sized reports 0×0, which is no answer either.
    (rc == 0 && ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col, ws.ws_row))
}

#[cfg(test)]
#[path = "../tests/support/pty.rs"]
mod pty;

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Key> {
        let mut d = Decoder::new();
        let mut keys = d.feed(bytes);
        keys.extend(d.timeout());
        keys
    }

    fn chars(s: &str) -> Vec<Key> {
        s.chars().map(Key::Char).collect()
    }

    #[test]
    fn printable_text_and_utf8_are_chars() {
        assert_eq!(decode(b"nq?"), chars("nq?"));
        assert_eq!(decode("é€😀a".as_bytes()), chars("é€😀a"));
    }

    #[test]
    fn carriage_return_and_newline_are_enter() {
        assert_eq!(decode(b"\r\n"), vec![Key::Enter, Key::Enter]);
    }

    #[test]
    fn controls_ride_on_char() {
        // No Ctrl variant in ui::Key; Ctrl-C must still reach the menu as something.
        assert_eq!(decode(b"\x03\x7f"), chars("\u{3}\u{7f}"));
    }

    #[test]
    fn arrows_in_both_cursor_modes() {
        assert_eq!(
            decode(b"\x1b[A\x1b[B\x1bOA\x1bOB"),
            vec![Key::Up, Key::Down, Key::Up, Key::Down]
        );
        // Modifiers are a second parameter, and a modified arrow is still the arrow.
        assert_eq!(decode(b"\x1b[1;5A\x1b[1;2B"), vec![Key::Up, Key::Down]);
    }

    #[test]
    fn paging_keys() {
        assert_eq!(decode(b"\x1b[5~\x1b[6~"), vec![Key::PageUp, Key::PageDown]);
    }

    #[test]
    fn every_home_and_end() {
        assert_eq!(
            decode(b"\x1b[H\x1b[1~\x1b[7~\x1bOH"),
            vec![Key::Home; 4],
            "Home"
        );
        assert_eq!(
            decode(b"\x1b[F\x1b[4~\x1b[8~\x1bOF"),
            vec![Key::End; 4],
            "End"
        );
    }

    #[test]
    fn sgr_mouse_left_press_is_a_zero_based_click() {
        assert_eq!(
            decode(b"\x1b[<0;10;3M"),
            vec![Key::Click { row: 2, col: 9 }]
        );
        // A Ctrl-click is still a click.
        assert_eq!(
            decode(b"\x1b[<16;1;1M"),
            vec![Key::Click { row: 0, col: 0 }]
        );
    }

    #[test]
    fn sgr_mouse_wheel() {
        assert_eq!(
            decode(b"\x1b[<64;5;5M\x1b[<65;5;5M"),
            vec![Key::WheelUp, Key::WheelDown]
        );
    }

    #[test]
    fn sgr_mouse_releases_other_buttons_and_motion_are_dropped() {
        // Calibration: the same report as a press is a click, so the drops below are the
        // decoder choosing to drop, not failing to parse.
        assert_eq!(
            decode(b"\x1b[<0;10;3M"),
            vec![Key::Click { row: 2, col: 9 }]
        );
        assert_eq!(decode(b"\x1b[<0;10;3m"), vec![], "left release");
        assert_eq!(decode(b"\x1b[<2;10;3M"), vec![], "right press");
        assert_eq!(decode(b"\x1b[<1;10;3M"), vec![], "middle press");
        assert_eq!(decode(b"\x1b[<32;10;3M"), vec![], "drag");
        assert_eq!(decode(b"\x1b[<35;10;3M"), vec![], "motion");
        assert_eq!(decode(b"\x1b[<0;99999;3M"), vec![], "off any screen");
        assert_eq!(decode(b"\x1b[<0;10M"), vec![], "two fields");
        assert_eq!(decode(b"\x1b[<0;10;3;4M"), vec![], "four fields");
        assert_eq!(decode(b"\x1b[<0;;3M"), vec![], "an empty field");
    }

    #[test]
    fn x10_mouse_is_consumed_whole() {
        // ESC [ M, then button+32, col+33, row+33 as raw bytes.
        assert_eq!(
            decode(b"\x1b[M\x20\x2a\x23"),
            vec![Key::Click { row: 2, col: 9 }]
        );
        // A release (button 3) is dropped, raw bytes and all.
        assert_eq!(decode(b"\x1b[M\x23\x2a\x23x"), chars("x"));
    }

    #[test]
    fn unmapped_sequences_never_leak_as_characters() {
        for seq in [
            &b"\x1b[C"[..],     // Right
            b"\x1b[D",          // Left
            b"\x1b[15~",        // F5
            b"\x1b[2~",         // Insert
            b"\x1bOP",          // F1
            b"\x1b[?1;2c",      // device attributes reply
            b"\x1b[>0;95;0c",   // secondary DA reply
            b"\x1b[I",          // focus in
            b"\x1b[O",          // focus out
            b"\x1b[200~",       // bracketed paste start
            b"\x1b[12;40R",     // cursor position report
            b"\x1b[1 q",        // an intermediate byte
            b"\x1bx",           // Alt-x
            b"\x1b\r",          // Alt-Enter
            "\x1bé".as_bytes(), // Alt-é
        ] {
            assert_eq!(decode(seq), vec![], "{seq:?}");
            // And it does not take the next key with it.
            let mut then_n = seq.to_vec();
            then_n.push(b'n');
            assert_eq!(decode(&then_n), chars("n"), "{seq:?} then n");
        }
    }

    #[test]
    fn a_lone_esc_waits_for_the_timeout() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b"), vec![]);
        assert!(d.pending(), "a lone ESC may be the start of a sequence");
        assert_eq!(d.timeout(), vec![Key::Esc]);
        assert!(!d.pending());
    }

    #[test]
    fn two_escs_are_two_keys() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b\x1b"), vec![Key::Esc]);
        assert_eq!(d.timeout(), vec![Key::Esc]);
        // And ESC directly before a sequence is a key, then the sequence.
        assert_eq!(decode(b"\x1b\x1b[A"), vec![Key::Esc, Key::Up]);
    }

    #[test]
    fn an_incomplete_sequence_is_dropped_at_the_timeout() {
        for tail in [
            &b"\x1b["[..],
            b"\x1b[5",
            b"\x1b[<0;10",
            b"\x1bO",
            b"\x1b[M\x20",
        ] {
            let mut d = Decoder::new();
            assert_eq!(d.feed(tail), vec![], "{tail:?}");
            assert!(d.pending(), "{tail:?}");
            assert_eq!(d.timeout(), vec![], "{tail:?}");
            assert!(!d.pending(), "{tail:?}");
        }
    }

    #[test]
    fn a_sequence_cut_short_does_not_swallow_the_next() {
        assert_eq!(decode(b"\x1b[1\x1b[A"), vec![Key::Up]);
        assert_eq!(decode(b"\x1b[5\rq"), vec![Key::Enter, Key::Char('q')]);
    }

    #[test]
    fn runaway_sequences_are_bounded() {
        let mut d = Decoder::new();
        let mut junk = b"\x1b[".to_vec();
        junk.extend(std::iter::repeat_n(b'1', MAX_SEQ * 2));
        assert_eq!(d.feed(&junk), vec![]);
        assert!(!d.pending(), "the buffer must not grow without bound");
    }

    #[test]
    fn invalid_utf8_is_dropped_and_the_rest_survives() {
        assert_eq!(decode(b"\x80a\xffb\xc0\xafc"), chars("abc"));
        // A surrogate, which has the right shape and is still not a character.
        assert_eq!(decode(b"\xed\xa0\x80d"), chars("d"));
        // A lead cut short by ASCII.
        assert_eq!(decode(b"\xe2\x82e"), chars("e"));
        // Incomplete at the timeout.
        let mut d = Decoder::new();
        assert_eq!(d.feed("€".as_bytes().get(..2).unwrap()), vec![]);
        assert_eq!(d.timeout(), vec![]);
    }

    /// Every sequence the decoder knows, back to back with text between.
    const CORPUS: &[u8] = b"a\x1b[A\x1b[B\x1bOA\x1bOBb\x1b[5~\x1b[6~\x1b[H\x1b[F\x1b[1~\x1b[4~\
        \x1b[7~\x1b[8~\x1bOH\x1bOF\x1b[1;5A\x1b[<0;10;3M\x1b[<0;10;3m\x1b[<64;1;1M\
        \x1b[<65;1;1M\x1b[M\x20\x2a\x23\x1b[15~\x1b[?1;2c\x1bx\r\n\xc3\xa9\xe2\x82\xac\
        \xf0\x9f\x98\x80\x1b\x1b[Bz";

    fn corpus_keys() -> Vec<Key> {
        let mut want = vec![Key::Char('a'), Key::Up, Key::Down, Key::Up, Key::Down];
        want.push(Key::Char('b'));
        want.extend([Key::PageUp, Key::PageDown]);
        want.extend([
            Key::Home,
            Key::End,
            Key::Home,
            Key::End,
            Key::Home,
            Key::End,
        ]);
        want.extend([Key::Home, Key::End, Key::Up]);
        want.push(Key::Click { row: 2, col: 9 });
        want.extend([Key::WheelUp, Key::WheelDown]);
        want.push(Key::Click { row: 2, col: 9 });
        want.extend([Key::Enter, Key::Enter]);
        want.extend(chars("é€😀"));
        want.extend([Key::Esc, Key::Down, Key::Char('z')]);
        want
    }

    #[test]
    fn the_corpus_decodes_whole() {
        // The reference the split tests compare against is itself checked against a list
        // written out by hand, so a split test cannot pass by agreeing with a wrong answer.
        assert_eq!(decode(CORPUS), corpus_keys());
    }

    #[test]
    fn sequences_split_at_any_byte_boundary_decode_the_same() {
        for cut in 0..=CORPUS.len() {
            let mut d = Decoder::new();
            let mut keys = d.feed(&CORPUS[..cut]);
            keys.extend(d.feed(&CORPUS[cut..]));
            keys.extend(d.timeout());
            assert_eq!(keys, corpus_keys(), "split at {cut}");
        }
    }

    #[test]
    fn sequences_fed_one_byte_at_a_time_decode_the_same() {
        let mut d = Decoder::new();
        let mut keys = Vec::new();
        for b in CORPUS {
            keys.extend(d.feed(std::slice::from_ref(b)));
        }
        keys.extend(d.timeout());
        assert_eq!(keys, corpus_keys());
    }

    /// Every `?N` private mode `seq` touches, in order: its number and whether it is set
    /// (`h`) or reset (`l`). Hiding the cursor is a reset (`?25l`), so the direction matters.
    fn private_modes(seq: &[u8]) -> Vec<(String, bool)> {
        let s = std::str::from_utf8(seq).unwrap();
        s.split("\x1b[?")
            .skip(1)
            .map(|m| {
                let (num, end) = m.split_at(m.len() - 1);
                assert!(end == "h" || end == "l", "not a mode change: {m:?}");
                (num.to_string(), end == "h")
            })
            .collect()
    }

    fn motion_tracking(seq: &[u8]) -> bool {
        let s = String::from_utf8_lossy(seq);
        s.contains("?1002") || s.contains("?1003")
    }

    #[test]
    fn mouse_reporting_is_clicks_only_and_never_motion() {
        // Calibration: the check does catch motion tracking when it is there.
        assert!(motion_tracking(b"\x1b[?1000h\x1b[?1003h"));
        assert!(motion_tracking(b"\x1b[?1002h"));
        assert!(!motion_tracking(ENTER_SEQ), "ENTER_SEQ asks for motion");
        assert!(!motion_tracking(LEAVE_SEQ), "LEAVE_SEQ mentions motion");
        let on = private_modes(ENTER_SEQ);
        assert!(
            on.contains(&("1000".to_string(), true)),
            "clicks are reported"
        );
        assert!(on.contains(&("1006".to_string(), true)), "as SGR");
    }

    #[test]
    fn leaving_undoes_exactly_what_entering_did() {
        let on = private_modes(ENTER_SEQ);
        assert_eq!(on.len(), 4, "the parse found every mode");
        let undo: Vec<(String, bool)> = on.into_iter().rev().map(|(n, set)| (n, !set)).collect();
        assert_eq!(private_modes(LEAVE_SEQ), undo);
    }

    // ── against a real pty ──────────────────────────────────────────────────

    /// The terminal tests share process-wide state — the SIGWINCH flag, the panic hook's
    /// record — so they take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    // Generic and x86 values alike.
    const ICANON: u32 = 0o2;
    const ECHO: u32 = 0o10;

    unsafe extern "C" {
        fn raise(sig: c_int) -> c_int;
    }

    fn termios(fd: c_int) -> Termios {
        let mut t = std::mem::MaybeUninit::<Termios>::zeroed();
        assert_eq!(unsafe { tcgetattr(fd, t.as_mut_ptr()) }, 0, "tcgetattr");
        unsafe { t.assume_init() }
    }

    fn cooked(t: &Termios) -> bool {
        t.c_lflag & (ICANON | ECHO) == (ICANON | ECHO)
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn raw_mode_comes_and_goes_with_suspend_resume_and_drop() {
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let mut p = pty::Pty::open(80, 24).expect("pty");
        let fd = p.slave_fd();
        let before = termios(fd);
        assert!(
            cooked(&before),
            "a fresh pty is cooked; the check is calibrated on it"
        );

        let mut t = RawTerminal::on(fd, fd).expect("enter");
        assert!(!cooked(&termios(fd)), "raw after enter");
        assert!(contains(
            &p.read_until(ENTER_SEQ, Duration::from_secs(2)),
            ENTER_SEQ
        ));

        t.suspend().unwrap();
        assert!(
            termios(fd) == before,
            "suspend restores the termios exactly"
        );
        assert!(contains(
            &p.read_until(LEAVE_SEQ, Duration::from_secs(2)),
            LEAVE_SEQ
        ));

        t.resume().unwrap();
        assert!(!cooked(&termios(fd)), "raw again after resume");
        assert!(contains(
            &p.read_until(ENTER_SEQ, Duration::from_secs(2)),
            ENTER_SEQ
        ));

        drop(t);
        assert!(termios(fd) == before, "drop restores the termios exactly");
        assert!(contains(
            &p.read_until(LEAVE_SEQ, Duration::from_secs(2)),
            LEAVE_SEQ
        ));
    }

    #[test]
    fn waiting_decodes_keys_and_writes_nothing() {
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let mut p = pty::Pty::open(80, 24).expect("pty");
        let fd = p.slave_fd();
        let mut t = RawTerminal::on(fd, fd).expect("enter");
        p.read_until(ENTER_SEQ, Duration::from_secs(2));

        p.write(b"\x1b[Bq");
        assert_eq!(
            t.wait(Some(Duration::from_secs(2))).unwrap(),
            Input::Keys(vec![Key::Down, Key::Char('q')])
        );

        // A lone ESC is a key once ESC_WAIT has passed with nothing after it.
        p.write(b"\x1b");
        let start = Instant::now();
        assert_eq!(
            t.wait(Some(Duration::from_secs(2))).unwrap(),
            Input::Keys(vec![Key::Esc])
        );
        assert!(start.elapsed() >= ESC_WAIT, "{:?}", start.elapsed());

        // A mouse release decodes to nothing, so it does not end the wait.
        p.write(b"\x1b[<0;1;1m");
        assert_eq!(
            t.wait(Some(Duration::from_millis(200))).unwrap(),
            Input::Timeout
        );

        // Calibration: the master does see what the terminal writes...
        t.write_all(b"frame").unwrap();
        assert!(contains(
            &p.read_until(b"frame", Duration::from_secs(2)),
            b"frame"
        ));
        // ...and sees nothing at all from a wait.
        assert_eq!(
            t.wait(Some(Duration::from_millis(300))).unwrap(),
            Input::Timeout
        );
        assert_eq!(p.read_for(Duration::from_millis(300)), b"");
    }

    #[test]
    fn a_resize_wakes_a_wait_already_blocked() {
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let p = pty::Pty::open(80, 24).expect("pty");
        let fd = p.slave_fd();
        let mut t = RawTerminal::on(fd, fd).expect("enter");
        // Calibration: with no signal, the wait runs to its timeout.
        assert_eq!(
            t.wait(Some(Duration::from_millis(100))).unwrap(),
            Input::Timeout
        );
        // raise() from another thread delivers the signal to THAT thread, so this wait's
        // poll is not interrupted: only the self-pipe can wake it.
        let signaller = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            unsafe { raise(SIGWINCH) };
        });
        let start = Instant::now();
        assert_eq!(
            t.wait(Some(Duration::from_secs(10))).unwrap(),
            Input::Keys(vec![Key::Resize])
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        signaller.join().unwrap();
    }

    /// SIGINT's current disposition, read the only way signal(2) offers: by setting it and
    /// putting it straight back.
    fn sigint_disposition() -> usize {
        let prev = unsafe { signal(SIGINT, SIG_DFL) };
        unsafe { signal(SIGINT, prev) };
        prev
    }

    #[test]
    fn ctrl_c_in_a_child_does_not_kill_the_menu() {
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let p = pty::Pty::open(80, 24).expect("pty");
        let fd = p.slave_fd();
        assert_eq!(
            sigint_disposition(),
            SIG_DFL,
            "calibration: the default to start"
        );
        let mut t = RawTerminal::on(fd, fd).expect("enter");
        assert_eq!(
            sigint_disposition(),
            SIG_DFL,
            "raw mode needs no handler: ISIG is off"
        );
        t.suspend().unwrap();
        assert_ne!(sigint_disposition(), SIG_DFL);
        assert_ne!(
            sigint_disposition(),
            SIG_IGN,
            "an ignored SIGINT would reach the child"
        );
        // Were the handler missing, this would end the test binary, loudly.
        unsafe { raise(SIGINT) };
        t.resume().unwrap();
        assert_eq!(
            sigint_disposition(),
            SIG_DFL,
            "resume puts the default back"
        );
        t.suspend().unwrap();
        drop(t);
        assert_eq!(
            sigint_disposition(),
            SIG_DFL,
            "so does drop while suspended"
        );
    }

    #[test]
    fn size_reads_the_window() {
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let p = pty::Pty::open(40, 24).expect("pty");
        assert_eq!(size_of(p.slave_fd()), Some((40, 24)));
        p.resize(80, 30);
        assert_eq!(size_of(p.slave_fd()), Some((80, 30)));
        // Not a terminal at all.
        let f = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(size_of(f.as_raw_fd()), None);
    }

    /// Run one of the `child_*` tests below in a fresh copy of this test binary, on a pty of
    /// its own, and hand back the pty once it has exited.
    fn run_child(name: &str) -> (pty::Pty, Vec<u8>) {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", name, "--ignored", "--test-threads=1"])
            .env("CLAUDE_SESSIONS_TERM_CHILD", "1");
        let mut p = pty::Pty::spawn(cmd, 80, 24).expect("spawn");
        let out = p.read_until(ENTER_SEQ, Duration::from_secs(10));
        assert!(
            contains(&out, ENTER_SEQ),
            "the child never went raw: {out:?}"
        );
        p.wait(Duration::from_secs(10)).expect("the child exits");
        let mut all = out;
        all.extend(p.read_for(Duration::from_millis(200)));
        (p, all)
    }

    fn after<'a>(hay: &'a [u8], needle: &[u8]) -> &'a [u8] {
        let at = hay
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("needle present");
        &hay[at + needle.len()..]
    }

    #[test]
    fn a_panic_restores_the_terminal_even_when_drop_never_runs() {
        // Calibration: a child that leaves raw mode behind without panicking IS seen as raw,
        // so the restore below is the panic hook's doing and not the check failing to look.
        let (p, out) = run_child("term::tests::child_exits_raw");
        assert!(!cooked(&termios(p.slave_fd())), "the leak is visible");
        assert!(!contains(after(&out, ENTER_SEQ), LEAVE_SEQ));

        // `forget` stands in for `panic = "abort"`: no Drop, only the hook.
        let (p, out) = run_child("term::tests::child_panics_raw");
        assert!(
            cooked(&termios(p.slave_fd())),
            "the panic hook restored the termios"
        );
        let tail = after(&out, ENTER_SEQ);
        assert!(contains(tail, LEAVE_SEQ), "and left the alternate screen");
        // Restored before the message, so the message is on the main screen.
        assert!(contains(after(tail, LEAVE_SEQ), b"deliberate"), "{out:?}");
    }

    fn is_child() -> bool {
        std::env::var_os("CLAUDE_SESSIONS_TERM_CHILD").is_some()
    }

    #[test]
    #[ignore = "run by a_panic_restores_the_terminal_even_when_drop_never_runs"]
    fn child_exits_raw() {
        if !is_child() {
            return;
        }
        std::mem::forget(RawTerminal::enter().expect("enter"));
    }

    #[test]
    #[ignore = "run by a_panic_restores_the_terminal_even_when_drop_never_runs"]
    fn child_panics_raw() {
        if !is_child() {
            return;
        }
        std::mem::forget(RawTerminal::enter().expect("enter"));
        panic!("deliberate, to test the restore");
    }
}
