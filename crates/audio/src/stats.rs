//! How much the title used the console audio functions, for the run report: relaxed atomics, a few nanoseconds
//! per call.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `AcpHal` `SubmitCommand` calls by command number (0-31; the rest in the last).
pub static ACP_COMMANDS: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
/// The message kinds the title registered for (the coprocessor's mask), messages posted to it, taken by it, and
/// its polls (`PopMessage` and `GetMessageCount` calls)
pub static ACP_MASK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static ACP_POSTED: AtomicU64 = AtomicU64::new(0);
pub static ACP_POPPED: AtomicU64 = AtomicU64::new(0);
pub static ACP_POLLS: AtomicU64 = AtomicU64::new(0);
pub static XAPU_CONNECTS: AtomicU64 = AtomicU64::new(0);
/// Opus streams activated
pub static OPUS_STREAMS: AtomicU64 = AtomicU64::new(0);
/// Opus packets decoded, frames they gave, packets refused
pub static OPUS_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static OPUS_FRAMES: AtomicU64 = AtomicU64::new(0);
pub static OPUS_ERRORS: AtomicU64 = AtomicU64::new(0);
pub static DSP_STREAMS: AtomicU64 = AtomicU64::new(0);
pub static DSP_BLOCKS: AtomicU64 = AtomicU64::new(0);

pub fn count(c: &AtomicU64) {
    c.fetch_add(1, Relaxed);
}

pub(crate) fn acp_command(command: u32) {
    ACP_COMMANDS[(command as usize).min(31)].fetch_add(1, Relaxed);
}

/// One line: the coprocessor's commands, XMA decode, Opus decode, DSP.
pub fn report() -> String {
    let cmds: Vec<String> = ACP_COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, c)| c.load(Relaxed) > 0)
        .map(|(i, c)| format!("{i}:{}", c.load(Relaxed)))
        .collect();
    let xma = crate::acp::made().and_then(|h| h.xma()).map_or("no XMA contexts".to_string(), |x| {
        let (frames, bad, blocks) = x.counts();
        format!("XMA {frames} frames decoded ({bad} refused), {blocks} output blocks")
    });
    format!(
        "console audio: coprocessor commands {}, message mask {:#x}, {} messages posted, {} taken, {} polls; {xma}; Opus {} connections, {} streams, {} packets -> {} frames ({} refused); DSP {} streams, {} blocks (dry)",
        if cmds.is_empty() { "none".into() } else { cmds.join(" ") },
        ACP_MASK.load(Relaxed),
        ACP_POSTED.load(Relaxed),
        ACP_POPPED.load(Relaxed),
        ACP_POLLS.load(Relaxed),
        XAPU_CONNECTS.load(Relaxed),
        OPUS_STREAMS.load(Relaxed),
        OPUS_PACKETS.load(Relaxed),
        OPUS_FRAMES.load(Relaxed),
        OPUS_ERRORS.load(Relaxed),
        DSP_STREAMS.load(Relaxed),
        DSP_BLOCKS.load(Relaxed),
    )
}
