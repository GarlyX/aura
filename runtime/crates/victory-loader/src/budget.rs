//! The title's memory budget (`VICTORY_BUDGET`, `VICTORY_BUDGET_POOLS`; off
//! without them).
//!
//! What the title does (build 1.0.2.1, found with IDA): at start-up it picks
//! one of two 80-byte rows of a table in `.rdata` (image +0x6186250) by the
//! console's device type (6, the smaller console: row 0; any other: row 1)
//! and sizes its nine memory pools from it. Each row is ten little-endian
//! u64s: the total, then the nine pools. Row 0 asks for 8572 MiB in all,
//! row 1 for 13312 MiB. Its allocators are those pools by index (+0x4fc99c0
//! returns pool `n` of a table the set-up fills), and pools 0, 4 and 5 are
//! CPU memory, pool 2 graphics memory.
//!
//! The pools cannot simply all take the smaller row: the title's object
//! pools are sized from its configuration for the bigger console, and pool
//! 5 then runs out at start-up (a deliberate break at +0x22106e0, reached
//! from +0x16cbe00). So each pool is set on its own: `VICTORY_BUDGET_POOLS=
//! "7=4722,2=1277"` gives pool 7 4722 MiB and pool 2 1277 MiB in both rows
//! (the totals follow); `VICTORY_BUDGET=small` copies row 0 over row 1 first
//! (the whole smaller budget, for experiments).
//!
//! The table's 160 bytes are compared (BLAKE3) before anything is written; a
//! table that differs (another build) is left alone and said so.
//!
//! Measured (bigger console's profile, 16 GB PC): the whole smaller row and
//! pool 2 at the smaller row's 1277 MiB both stop the title at start-up the
//! same way (its pools are filled to fixed sizes from the configuration);
//! pools 6-8 are not set up from the table (only 0, 2, 4 and 5 are), so
//! pool 7 changes nothing. The smaller budget comes with the smaller
//! console's profile only; this stays a tool for experiments.

/// The table's offset in the image.
pub const TABLE_RVA: usize = 0x618_6250;
/// Two rows of ten u64s.
pub const TABLE_LEN: usize = 160;
/// Pools per row.
pub const POOLS: usize = 9;
const ROW: usize = 80;
/// BLAKE3 of the table as the build ships it.
const TABLE_HASH: &str = "4fdd1ed20435114a89f9c49f3727d3aa72e11bc8e7bad325f1141b7377d2676d";

/// What the switches ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Asked {
    /// Row 1 becomes row 0 before the pools below.
    pub small: bool,
    /// Pool sizes, MiB, in both rows.
    pub pools: [Option<u64>; POOLS],
}

impl Asked {
    pub fn any(&self) -> bool {
        self.small || self.pools.iter().any(Option::is_some)
    }
}

/// Reads the switches (pure: the values are passed in). `pools` is a list
/// of `index=MiB`; entries that do not parse are skipped.
pub fn parse(budget: Option<&str>, pools: Option<&str>) -> Asked {
    let mut asked = Asked {
        small: matches!(budget.map(str::trim), Some("small" | "series-s")),
        ..Asked::default()
    };
    for entry in pools.unwrap_or("").split(',') {
        let Some((index, mib)) = entry.split_once('=') else {
            continue;
        };
        if let (Ok(index), Ok(mib)) = (index.trim().parse::<usize>(), mib.trim().parse::<u64>())
            && index < POOLS
            && mib > 0
        {
            asked.pools[index] = Some(mib);
        }
    }
    asked
}

/// From the process environment.
pub fn asked() -> Asked {
    parse(
        std::env::var("VICTORY_BUDGET").ok().as_deref(),
        std::env::var("VICTORY_BUDGET_POOLS").ok().as_deref(),
    )
}

/// What happened to the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Rewritten; the row the bigger console reads now totals this many MiB.
    Done { total_mib: u64 },
    /// The bytes are not the known table: left alone.
    Unknown,
    /// Nothing asked.
    Off,
}

fn get(t: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(t[i * 8..i * 8 + 8].try_into().unwrap())
}

fn set(t: &mut [u8], i: usize, v: u64) {
    t[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
}

/// Checks the table and rewrites it in place as asked (pure).
pub fn rewrite(table: &mut [u8], asked: Asked) -> Outcome {
    if !asked.any() {
        return Outcome::Off;
    }
    if table.len() != TABLE_LEN || blake3::hash(table).to_hex().as_str() != TABLE_HASH {
        return Outcome::Unknown;
    }
    rewrite_unchecked(table, asked);
    Outcome::Done {
        total_mib: get(&table[ROW..], 0) >> 20,
    }
}

fn rewrite_unchecked(table: &mut [u8], asked: Asked) {
    if asked.small {
        let (small, big) = table.split_at_mut(ROW);
        big[..ROW].copy_from_slice(small);
    }
    for (pool, mib) in asked.pools.iter().enumerate() {
        let Some(mib) = mib else { continue };
        for row in table.chunks_exact_mut(ROW) {
            let old = get(row, pool + 1);
            let new = mib << 20;
            set(row, pool + 1, new);
            set(row, 0, (get(row, 0) + new).saturating_sub(old));
        }
    }
}

/// Rewrites the mapped image's table.
///
/// # Safety
/// `base` is a mapped image of `size` bytes and none of its code runs yet.
#[cfg(windows)]
pub unsafe fn apply(base: usize, size: usize, asked: Asked) -> Outcome {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    if !asked.any() {
        return Outcome::Off;
    }
    if TABLE_RVA + TABLE_LEN > size {
        return Outcome::Unknown;
    }
    let at = (base + TABLE_RVA) as *mut u8;
    let mut old = 0;
    // SAFETY: the table lies inside the mapped image (checked above).
    if unsafe { VirtualProtect(at.cast(), TABLE_LEN, PAGE_READWRITE, &mut old) } == 0 {
        return Outcome::Unknown;
    }
    // SAFETY: writable now, and no title code runs yet.
    let table = unsafe { std::slice::from_raw_parts_mut(at, TABLE_LEN) };
    let outcome = rewrite(table, asked);
    // SAFETY: restoring the protection we changed.
    unsafe { VirtualProtect(at.cast(), TABLE_LEN, old, &mut old) };
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<u8> {
        let mut t = vec![0; TABLE_LEN];
        for (i, v) in [100u64, 10, 0, 20, 0, 5, 5, 10, 40, 10].iter().enumerate() {
            set(&mut t, i, v << 20);
        }
        for (i, v) in [200u64, 10, 0, 40, 0, 10, 10, 20, 100, 10].iter().enumerate() {
            set(&mut t[ROW..], i, v << 20);
        }
        t
    }

    #[test]
    fn switches_parse() {
        assert_eq!(parse(None, None), Asked::default());
        assert!(parse(Some("small"), None).small);
        assert!(!parse(Some("off"), None).small);
        let a = parse(None, Some("7=3000, 2=1277,x=1,9=5,3=0"));
        assert_eq!(a.pools[7], Some(3000));
        assert_eq!(a.pools[2], Some(1277));
        assert_eq!(a.pools.iter().flatten().count(), 2);
    }

    #[test]
    fn small_copies_the_first_row_over_the_second() {
        let mut t = table();
        rewrite_unchecked(&mut t, Asked { small: true, ..Asked::default() });
        assert_eq!(t[..ROW], t[ROW..]);
        assert_eq!(get(&t[ROW..], 0) >> 20, 100);
    }

    #[test]
    fn a_pool_changes_in_both_rows_and_their_totals_follow() {
        let mut t = table();
        let mut asked = Asked::default();
        asked.pools[7] = Some(30);
        rewrite_unchecked(&mut t, asked);
        assert_eq!(get(&t, 8) >> 20, 30);
        assert_eq!(get(&t, 0) >> 20, 90);
        assert_eq!(get(&t[ROW..], 8) >> 20, 30);
        assert_eq!(get(&t[ROW..], 0) >> 20, 130);
    }

    #[test]
    fn an_unknown_table_is_left_alone() {
        let mut t = table();
        let before = t.clone();
        assert_eq!(rewrite(&mut t, Asked { small: true, ..Asked::default() }), Outcome::Unknown);
        assert_eq!(t, before);
        assert_eq!(rewrite(&mut t, Asked::default()), Outcome::Off);
    }
}
