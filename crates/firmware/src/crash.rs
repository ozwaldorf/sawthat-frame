//! Crash reports: the panic handler keeps the message and backtrace in RTC memory through a reset,
//! so the next boot can show them before shutting the frame down.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::panic::PanicInfo;

use esp_backtrace::Backtrace;
use esp_println::println;

use crate::{sleep, symbols};

/// Where crash reports are filed
const ISSUES_URL: &str = "https://github.com/ozwaldorf/sawthat-frame/issues";
/// Version and git revision of this build
pub const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_REVISION: &str = env!("GIT_REVISION");
/// Shown for an address with no known function
const UNKNOWN: &str = "???";

/// Marks a report that has not been shown yet
const PENDING: u32 = 0xDEAD_C0DE;
const MESSAGE_CAP: usize = 384;
const REVISION_CAP: usize = 32;
const FRAMES_CAP: usize = 16;

pub struct CrashReport {
    state: u32,
    /// Symbol table of the build that crashed, so names come only from the same build
    table_id: u32,
    message_len: u16,
    message: [u8; MESSAGE_CAP],
    revision_len: u8,
    revision: [u8; REVISION_CAP],
    frame_count: u8,
    frames: [u32; FRAMES_CAP],
}

impl CrashReport {
    const fn new() -> Self {
        Self {
            state: 0,
            table_id: 0,
            message_len: 0,
            message: [0; MESSAGE_CAP],
            revision_len: 0,
            revision: [0; REVISION_CAP],
            frame_count: 0,
            frames: [0; FRAMES_CAP],
        }
    }

    /// Panic message, followed by its source location on a new line
    pub fn message(&self) -> &str {
        let len = (self.message_len as usize).min(MESSAGE_CAP);
        core::str::from_utf8(&self.message[..len]).unwrap_or("(unreadable message)")
    }

    /// Git revision of the build that crashed
    pub fn revision(&self) -> &str {
        let len = (self.revision_len as usize).min(REVISION_CAP);
        core::str::from_utf8(&self.revision[..len]).unwrap_or(UNKNOWN)
    }

    /// Version and git revision of the build that crashed
    pub fn version(&self) -> String {
        format!("v{FIRMWARE_VERSION} ({})", self.revision())
    }

    /// Program counters of the backtrace, innermost first
    pub fn frames(&self) -> &[u32] {
        &self.frames[..(self.frame_count as usize).min(FRAMES_CAP)]
    }

    /// One line per frame: its address and short function name. Names are looked up only in the
    /// symbol table of the build that crashed; otherwise, or for an unknown address, `???`.
    pub fn trace(&self) -> Vec<String> {
        let same_build = self.table_id != 0 && symbols::table_id() == Some(self.table_id);
        self.frames()
            .iter()
            .map(|&pc| {
                let name = same_build
                    .then(|| symbols::lookup(pc))
                    .flatten()
                    .map_or(String::from(UNKNOWN), short_name);
                format!("0x{pc:08x} {name}")
            })
            .collect()
    }

    /// Links to a new GitHub issue pre-filled with this report, from the most to the least
    /// detailed: the stack trace as shown, with addresses only, and without it
    pub fn issue_urls(&self) -> [String; 3] {
        [Trace::Named, Trace::Addresses, Trace::None].map(|trace| self.issue_url(trace))
    }

    fn issue_url(&self, trace: Trace) -> String {
        let message = self.message();
        let summary: String = message
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let title = format!("Crash: {summary}");

        let mut body = format!(
            "**Firmware:** {}\n\n**Panic:**\n```\n{message}\n```\n",
            self.version()
        );
        if trace != Trace::None && !self.frames().is_empty() {
            body.push_str("\n**Stack trace:**\n```\n");
            if trace == Trace::Named {
                for line in self.trace() {
                    let _ = writeln!(body, "{line}");
                }
            } else {
                for pc in self.frames() {
                    let _ = writeln!(body, "0x{pc:08x}");
                }
            }
            body.push_str("```\n");
        }

        format!(
            "{ISSUES_URL}/new?title={}&body={}",
            QueryEncoded(&title),
            QueryEncoded(&body)
        )
    }
}

/// Stack trace detail in an issue link
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trace {
    Named,
    Addresses,
    None,
}

/// Function name without its path: the method with its type, e.g. `Executor::run`, or the
/// function, keeping the enclosing function of a closure
fn short_name(name: &str) -> String {
    let segments = path_segments(name);
    let Some(mut i) = segments.len().checked_sub(1) else {
        return String::from(name);
    };
    while i > 0 && segments[i].starts_with('{') {
        i -= 1;
    }
    let function = segments[i..].join("::");
    match i.checked_sub(1).map(|j| segments[j]) {
        Some(qualified) if qualified.starts_with('<') => {
            format!("{}::{function}", type_name(qualified))
        }
        _ => function,
    }
}

/// Split a path at its `::` separators, except those inside generic arguments
fn path_segments(name: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let (mut depth, mut start, mut prev) = (0i32, 0, '\0');
    for (i, c) in name.char_indices() {
        match c {
            '<' => depth += 1,
            // `->` in a function signature is not a closing bracket
            '>' if prev != '-' => depth -= 1,
            ':' if depth == 0 && name[i..].starts_with("::") && prev != ':' => {
                segments.push(&name[start..i]);
                start = i + 2;
            }
            _ => {}
        }
        prev = c;
    }
    segments.push(&name[start..]);
    segments
}

/// Last path segment of the type in a qualified path like `<a::Type<T> as Trait>`
fn type_name(qualified: &str) -> &str {
    let inner = qualified.trim_start_matches('<').trim_end_matches('>');
    let inner = inner.split(" as ").next().unwrap_or(inner);
    let inner = &inner[..inner.find('<').unwrap_or(inner.len())];
    inner.rsplit("::").next().unwrap_or(inner)
}

/// Query string encoding: characters allowed in a query as is, spaces as `+`, the rest as `%XX`
struct QueryEncoded<'a>(&'a str);

impl core::fmt::Display for QueryEncoded<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        for b in self.0.bytes() {
            match b {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'@'
                | b'!'
                | b'$'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b','
                | b';' => f.write_char(b as char)?,
                b' ' => f.write_char('+')?,
                _ => write!(f, "%{b:02X}")?,
            }
        }
        Ok(())
    }
}

// SAFETY: only integers and integer arrays, valid for any bit pattern
unsafe impl esp_hal::Persistable for CrashReport {}

/// Kept through software resets, and cleared on power-on
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut REPORT: CrashReport = CrashReport::new();

/// The report of a crash that has not been shown yet
// `&STATIC` is rejected for `static mut` in edition 2024
#[allow(clippy::deref_addrof)]
pub fn pending() -> Option<&'static CrashReport> {
    // SAFETY: only written by the panic handler, which does not return
    let report = unsafe { &*(&raw const REPORT) };
    (report.state == PENDING).then_some(report)
}

/// Mark the pending report as shown
#[allow(clippy::deref_addrof)]
pub fn clear() {
    unsafe { (*(&raw mut REPORT)).state = 0 };
}

/// Appends to the message buffer, dropping what does not fit
struct MessageWriter<'a> {
    buf: &'a mut [u8; MESSAGE_CAP],
    len: usize,
}

impl Write for MessageWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for c in s.chars() {
            let mut utf8 = [0; 4];
            let bytes = c.encode_utf8(&mut utf8).as_bytes();
            if self.len + bytes.len() > MESSAGE_CAP {
                break;
            }
            self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        }
        Ok(())
    }
}

#[panic_handler]
#[allow(clippy::deref_addrof)]
fn panic(info: &PanicInfo) -> ! {
    println!("\n====================== PANIC ======================");
    println!("{}", info);
    println!("Firmware v{FIRMWARE_VERSION} ({GIT_REVISION})");
    let backtrace = Backtrace::capture();
    println!("\nBacktrace:\n");
    for frame in backtrace.frames() {
        let pc = frame.program_counter() as u32;
        println!("0x{:08x} {}", pc, symbols::lookup(pc).unwrap_or(UNKNOWN));
    }

    // A report that was never shown means this boot crashed before it could show it; shut down
    // rather than crash again
    if pending().is_some() {
        println!("Unshown crash report pending, shutting down");
        sleep::shutdown();
    }

    // SAFETY: the panic handler does not return, and nothing else writes the report
    let report = unsafe { &mut *(&raw mut REPORT) };
    let mut writer = MessageWriter {
        buf: &mut report.message,
        len: 0,
    };
    let _ = write!(writer, "{}", info.message());
    if let Some(location) = info.location() {
        let _ = write!(writer, "\n{}:{}", location.file(), location.line());
    }
    report.message_len = writer.len as u16;
    let revision = &GIT_REVISION.as_bytes()[..GIT_REVISION.len().min(REVISION_CAP)];
    report.revision[..revision.len()].copy_from_slice(revision);
    report.revision_len = revision.len() as u8;
    report.table_id = symbols::table_id().unwrap_or(0);
    // The frames of the panic machinery come first, and say nothing about the crash
    // Without allocating, as the heap may be what failed
    let frames = backtrace
        .frames()
        .iter()
        .map(|frame| frame.program_counter() as u32)
        .skip_while(|&pc| {
            symbols::lookup(pc).is_some_and(|name| name.starts_with("core::panicking::"))
        });
    report.frame_count = 0;
    for (slot, pc) in report.frames.iter_mut().zip(frames) {
        *slot = pc;
        report.frame_count += 1;
    }
    report.state = PENDING;

    esp_hal::system::software_reset()
}
