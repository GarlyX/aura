//! Live status for the terminal (standard output, in Turkish like the
//! README): a short line every few seconds while the title runs, a line at
//! each notable event, and a warning when no frame has come for a while.
//! The detailed log (`VICTORY_LOG`, standard error) is unchanged.
//! `VICTORY_STATUS=0` turns this off; `VICTORY_STATUS=<seconds>` sets the
//! interval (5 by default).

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// The last second's report: fps, ms a frame, draws a frame, ms preparing
/// and ms waiting for the card, each times 100.
static LAST: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Milliseconds since [`start`] at the last report (0: none yet).
static REPORTED_AT: AtomicU64 = AtomicU64::new(0);
static WORLD: AtomicBool = AtomicBool::new(false);
static FIRST_FRAME: AtomicBool = AtomicBool::new(false);
/// Draws a frame from which a frame is taken to be the world's.
const WORLD_DRAWS: f64 = 500.0;

fn start() -> Instant {
    static AT: OnceLock<Instant> = OnceLock::new();
    *AT.get_or_init(Instant::now)
}

/// The interval, or `None` when switched off.
fn interval() -> Option<Duration> {
    static IT: OnceLock<Option<Duration>> = OnceLock::new();
    *IT.get_or_init(|| match std::env::var("VICTORY_STATUS") {
        Ok(v) if v.trim() == "0" => None,
        Ok(v) => Some(Duration::from_secs(v.trim().parse::<u64>().unwrap_or(5).clamp(1, 600))),
        Err(_) => Some(Duration::from_secs(5)),
    })
}

fn stamp() -> String {
    let s = start().elapsed().as_secs();
    format!("[aura +{:02}:{:02}]", s / 60, s % 60)
}

/// Writes one line to the terminal now (an event).
pub fn event(text: &str) {
    if interval().is_some() {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{} {text}", stamp());
        let _ = out.flush();
    }
}

/// The once-a-second frame report (`host`): kept for the next status line;
/// the first frame and the first world frame are events.
pub(crate) fn frame_report(fps: f64, ms: f64, draws: f64, preparing: f64, waiting: f64) {
    if interval().is_none() {
        return;
    }
    for (cell, v) in LAST.iter().zip([fps, ms, draws, preparing, waiting]) {
        cell.store((v.max(0.0) * 100.0) as u64, Relaxed);
    }
    REPORTED_AT.store(start().elapsed().as_millis().max(1) as u64, Relaxed);
    if !FIRST_FRAME.swap(true, Relaxed) {
        event("oyun ilk karelerini gösteriyor");
        spawn();
    }
    if draws >= WORLD_DRAWS && !WORLD.swap(true, Relaxed) {
        event(&format!("oyun dünyasına girildi ({draws:.0} çizim/kare)"));
    }
}

fn phase(draws: f64) -> &'static str {
    if draws >= WORLD_DRAWS {
        "dünya"
    } else if draws >= 50.0 {
        "ara sahne/yükleme"
    } else {
        "menü/video"
    }
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / (1u64 << 30) as f64
}

/// The process's private bytes and working set, and the PC's free memory.
fn memory() -> (u64, u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    (c.PagefileUsage as u64, c.WorkingSetSize as u64, m.ullAvailPhys)
}

/// The line for the last report, `since` ms after it was made.
fn line(since: u64) -> String {
    let [fps, ms, draws, preparing, waiting] = LAST.each_ref().map(|c| c.load(Relaxed) as f64 / 100.0);
    let (private, resident, free) = memory();
    let making = crate::pipeline::making();
    let mut text = if since > 5000 {
        format!(
            "kare yok: oyun {:.0} sn'dir yeni kare göstermedi (yükleme ya da takılma)",
            since as f64 / 1000.0
        )
    } else {
        format!(
            "{} | {fps:.1} fps ({ms:.0} ms/kare: hazırlık {preparing:.0} ms, kartı bekleme {waiting:.0} ms) | {draws:.0} çizim/kare",
            phase(draws)
        )
    };
    text.push_str(&format!(
        " | bellek: süreç {:.1} GB (RAM'de {:.1}), boş RAM {:.1} GB",
        gb(private),
        gb(resident),
        gb(free)
    ));
    if making > 0 {
        text.push_str(&format!(" | {making} shader derleniyor"));
    }
    text
}

fn spawn() {
    let Some(every) = interval() else { return };
    let _ = std::thread::Builder::new().name("aura status".into()).spawn(move || {
        loop {
            std::thread::sleep(every);
            let at = REPORTED_AT.load(Relaxed);
            let now = start().elapsed().as_millis() as u64;
            event(&line(now.saturating_sub(at)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_by_draws() {
        assert_eq!(phase(4.0), "menü/video");
        assert_eq!(phase(120.0), "ara sahne/yükleme");
        assert_eq!(phase(3730.0), "dünya");
    }
}
