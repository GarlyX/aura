//! aura-gpu — the console's GPU on host D3D12; owns title memory.
//! See docs/architecture.md.

#[cfg(windows)]
pub mod titlemem;
#[cfg(windows)]
pub mod watch;
