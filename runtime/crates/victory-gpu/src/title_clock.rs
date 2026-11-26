//! The title's performance counter, slowed while presented frames are slow.
//!
//! The title times its frames by `QueryPerformanceCounter`, and some of its
//! work is sized by how much time it believes a frame took. Where a frame
//! takes seconds instead of the console's 33 ms (a 16 GB PC that pages the
//! title's 23-24 GB, 1-15 s a frame in the world), its world streaming
//! requests map data far faster than it releases them: in the opening
//! cutscene the building pool (22,500) and the instance-list pool (45,000)
//! filled within 113-173 world frames on every run, and the title's own
//! fatal check fired on every worker that created entities at once. With
//! the counter run 30 times slower in the world the same scene kept both
//! pools under a third full and played on past that point; the frames even
//! got faster (less to page).
//!
//! So the counter runs at the host's rate while frames are quick, and
//! slower in proportion once they are not: each frame looks about
//! [`frame_ms`] long to the title, however long it really took. The rate
//! follows a running mean of the last frames' lengths and comes back to the
//! host's within a few quick frames (menus, loading screens and movies run
//! at the title's own rate and are untouched). Waits the title times by the
//! counter last longer by the same factor; the title's other clocks (tick
//! count, system time) are left alone.
//!
//! `VICTORY_TITLE_FRAME_MS=<ms>` sets the longest frame the title is shown
//! (default 100); `0` gives the title the host's counter unchanged.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The longest a frame looks to the title, in milliseconds (0: off).
pub fn frame_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = std::env::var("VICTORY_TITLE_FRAME_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_FRAME_MS);
        tracing::info!(
            ms,
            "longest frame the title's clock shows (VICTORY_TITLE_FRAME_MS)"
        );
        ms
    })
}
const DEFAULT_FRAME_MS: u64 = 100;

/// The slowest the counter runs, against the host's.
const MIN_RATE: f64 = 1.0 / 1024.0;

/// How the title's counter follows the host's: from `host` on, `title`
/// plus `rate` times the host ticks since. Kept behind a sequence number
/// (odd while it changes) so that reads take no lock: the title asks for
/// the counter from every thread, many times a frame.
struct Anchor {
    seq: AtomicU64,
    host: AtomicU64,
    title: AtomicU64,
    rate: AtomicU64,
}

static ANCHOR: Anchor = Anchor {
    seq: AtomicU64::new(0),
    host: AtomicU64::new(0),
    title: AtomicU64::new(0),
    rate: AtomicU64::new(0x3FF0_0000_0000_0000), // 1.0
};

/// The pacing state the presents update (one at a time).
static PACE: Mutex<Pace> = Mutex::new(Pace::new());

/// The rate from the frames presented so far.
#[derive(Debug, Clone, Copy)]
struct Pace {
    /// Host ticks of the last present.
    last: Option<i64>,
    /// Running mean of frame lengths in host ticks.
    mean: f64,
    rate: f64,
}

impl Pace {
    const fn new() -> Pace {
        Pace {
            last: None,
            mean: 0.0,
            rate: 1.0,
        }
    }

    /// A present at host tick `now`: the rate for the frames that follow,
    /// so that one lasts about `target` ticks to the title.
    fn presented(&mut self, now: i64, target: f64) -> f64 {
        if let Some(last) = self.last.replace(now) {
            let length = (now - last).max(0) as f64;
            // Half the weight on the newest frame: a slow stretch is
            // followed within a frame or two, and quick frames bring the
            // rate back to 1 as fast.
            self.mean = if self.mean == 0.0 {
                length
            } else {
                (self.mean + length) / 2.0
            };
            self.rate = if self.mean <= target {
                1.0
            } else {
                (target / self.mean).max(MIN_RATE)
            };
        }
        self.rate
    }
}

/// The title's counter at host tick `now` from an anchor.
fn title_at(host: i64, title: i64, rate: f64, now: i64) -> i64 {
    title + ((now - host).max(0) as f64 * rate) as i64
}

#[cfg(windows)]
fn host_counter() -> i64 {
    let mut now = 0;
    // SAFETY: a valid out pointer.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut now) };
    now
}

#[cfg(windows)]
fn host_frequency() -> i64 {
    static FREQUENCY: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut f = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut f) };
        f.max(1)
    })
}

/// The counter the title is given (`QueryPerformanceCounter`); ticks at
/// the host's frequency.
#[cfg(windows)]
pub fn counter() -> i64 {
    loop {
        let seq = ANCHOR.seq.load(Ordering::Acquire);
        if seq & 1 == 1 {
            std::hint::spin_loop();
            continue;
        }
        let host = ANCHOR.host.load(Ordering::Acquire) as i64;
        let title = ANCHOR.title.load(Ordering::Acquire) as i64;
        let rate = f64::from_bits(ANCHOR.rate.load(Ordering::Acquire));
        // Read after the anchor and before the check: an anchor set later
        // was taken at a host tick at or after this one, so the old one
        // still applies here and the counter never goes back.
        let now = host_counter();
        if ANCHOR.seq.load(Ordering::Acquire) == seq {
            return if rate == 1.0 && host == title {
                now
            } else {
                title_at(host, title, rate, now)
            };
        }
    }
}

/// Called at every present: sets the rate for the frames that follow.
#[cfg(windows)]
pub(crate) fn frame_presented() {
    let ms = frame_ms();
    if ms == 0 {
        return;
    }
    let target = (ms as f64 / 1000.0) * host_frequency() as f64;
    let mut pace = PACE.lock().unwrap_or_else(|e| e.into_inner());
    let old = f64::from_bits(ANCHOR.rate.load(Ordering::Relaxed));
    let rate = pace.presented(host_counter(), target);
    if rate == old {
        return;
    }
    // Odd while the anchor changes; the host tick is taken after that, so
    // a reader that saw the old anchor read its tick before this one.
    ANCHOR.seq.fetch_add(1, Ordering::AcqRel);
    let now = host_counter();
    let host = ANCHOR.host.load(Ordering::Relaxed) as i64;
    let title = ANCHOR.title.load(Ordering::Relaxed) as i64;
    let at = title_at(host, title, old, now);
    ANCHOR.host.store(now as u64, Ordering::Release);
    ANCHOR.title.store(at as u64, Ordering::Release);
    ANCHOR.rate.store(rate.to_bits(), Ordering::Release);
    ANCHOR.seq.fetch_add(1, Ordering::AcqRel);
    // How far behind the host the title's clock is, now and then.
    static CHANGES: AtomicU64 = AtomicU64::new(0);
    let changes = CHANGES.fetch_add(1, Ordering::Relaxed);
    if changes < 4 || changes.is_power_of_two() {
        tracing::info!(
            rate = format_args!("{rate:.4}"),
            mean_frame_ms = format_args!("{:.0}", pace.mean * 1000.0 / host_frequency() as f64),
            behind_s = format_args!("{:.1}", (now - at) as f64 / host_frequency() as f64),
            "the title's clock rate against the host's changed (frames slower than VICTORY_TITLE_FRAME_MS slow it)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 10_000; // ticks of a 10 MHz counter

    #[test]
    fn quick_frames_keep_the_host_rate() {
        let mut pace = Pace::new();
        let target = 100.0 * MS as f64;
        for frame in 0..100 {
            assert_eq!(pace.presented(frame * 33 * MS, target), 1.0);
        }
    }

    #[test]
    fn slow_frames_look_target_long_and_quick_ones_bring_it_back() {
        let mut pace = Pace::new();
        let target = 100.0 * MS as f64;
        let mut now = 0;
        pace.presented(now, target);
        for _ in 0..8 {
            now += 2000 * MS;
            pace.presented(now, target);
        }
        // Two-second frames: each looks a tenth of a second long.
        let rate = pace.rate;
        assert!((rate - 0.05).abs() < 1e-3, "{rate}");
        // One long frame moves it at once, halfway.
        now += 6000 * MS;
        let rate = pace.presented(now, target);
        assert!((rate - 0.025).abs() < 1e-3, "{rate}");
        // Back at the title's own rate within a few frames.
        let mut frames = 0;
        while pace.presented(now + 33 * MS, target) < 1.0 {
            now += 33 * MS;
            frames += 1;
            assert!(frames < 12, "still slowed");
        }
    }

    #[test]
    fn the_rate_has_a_floor() {
        let mut pace = Pace::new();
        pace.presented(0, 1.0);
        assert_eq!(pace.presented(1_000_000 * MS, 1.0), MIN_RATE);
    }

    #[test]
    fn the_counter_is_continuous_across_rate_changes() {
        // Anchors as `frame_presented` makes them: each new one starts where
        // the old line was at its host tick.
        let (mut host, mut title, mut rate) = (0i64, 0i64, 1.0);
        let mut last = 0;
        for (at, next) in [(1000, 0.5), (3000, 0.01), (5000, 1.0), (9000, 0.2)] {
            for now in (host..=at).step_by(7) {
                let t = title_at(host, title, rate, now);
                assert!(t >= last, "went back at {now}");
                last = t;
            }
            title = title_at(host, title, rate, at);
            host = at;
            rate = next;
        }
        // 1000 at 1, 2000 at 0.5, 2000 at 0.01, 4000 at 1.
        assert_eq!(title, 1000 + 1000 + 20 + 4000);
    }

    #[cfg(windows)]
    #[test]
    fn the_title_counter_moves_with_the_host_at_first() {
        let a = counter();
        let b = host_counter();
        let c = counter();
        assert!(a <= b && b <= c);
    }
}
