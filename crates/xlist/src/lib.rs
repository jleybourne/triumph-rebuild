//! Command lists: the title's list calls recorded as data, then translated into PC calls.
//!
//! A console title records its lists through the D3D12.X API. We keep each call as a small
//! record ([`Cmd`]) and translate a whole list later, on any thread, into calls on a [`Sink`]: a
//! real D3D12 command list ([`d3d`]) or a counting sink, so that our own cost and the driver's
//! cost can be timed apart.
//!
//! Translation is three kinds of work, and none of it takes a lock:
//!
//! ```text
//! state     a set that repeats what is already bound is dropped (elision)
//! address   a console GPU address becomes a PC GPU address by one subtraction and one add
//! object    a console pipeline handle becomes a PC pipeline by one lookup in a prebuilt table
//! ```

#[cfg(windows)]
pub mod d3d;

pub mod frame;

/// One recorded list call. Addresses are console GPU addresses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmd {
    SetPipeline(u64),
    /// root slot, console address of the first descriptor of the table
    SetTable(u8, u64),
    /// root slot, console address of the constant buffer
    SetCbv(u8, u64),
    SetConstants([u32; 4]),
    /// console address, bytes, stride
    SetVertexBuffer(u64, u32, u32),
    /// console address, bytes (16-bit indices)
    SetIndexBuffer(u64, u32),
    /// a UAV barrier on the buffer at this console address
    Barrier(u64),
    /// index count, instance count, first index
    Draw(u32, u32, u32),
    /// console address of the arguments, command count
    DrawIndirect(u64, u32),
}

impl Cmd {
    pub fn is_draw(&self) -> bool {
        matches!(self, Cmd::Draw(..) | Cmd::DrawIndirect(..))
    }
}

/// What a translated list is written into. Every argument is already a PC value.
pub trait Sink {
    fn begin(&mut self);
    fn pipeline(&mut self, index: u32);
    fn table(&mut self, slot: u32, gpu_handle: u64);
    fn cbv(&mut self, slot: u32, gpu_va: u64);
    fn constants(&mut self, values: &[u32; 4]);
    fn vertex_buffer(&mut self, gpu_va: u64, bytes: u32, stride: u32);
    fn index_buffer(&mut self, gpu_va: u64, bytes: u32);
    fn barrier(&mut self, resource: u32);
    fn draw(&mut self, indices: u32, instances: u32, first: u32);
    fn draw_indirect(&mut self, gpu_va: u64, count: u32);
}

/// A console address range that maps onto a PC address range by an offset.
#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub console: u64,
    pub bytes: u64,
    pub pc: u64,
}

impl Range {
    #[inline]
    pub fn map(&self, va: u64) -> Option<u64> {
        let off = va.wrapping_sub(self.console);
        (off < self.bytes).then_some(self.pc + off)
    }
}

/// Console descriptor size, bytes (the title computes table addresses with it).
pub const CONSOLE_DESCRIPTOR: u64 = 32;

/// A fixed-size open-addressing table from console pipeline handle to PC pipeline index.
/// Built once at start; read without locks by every translating thread.
pub struct PipelineTable {
    keys: Vec<u64>,
    values: Vec<u32>,
    mask: usize,
}

impl PipelineTable {
    pub fn new(handles: &[u64]) -> PipelineTable {
        let cap = (handles.len() * 2).next_power_of_two().max(16);
        let mut t = PipelineTable { keys: vec![0; cap], values: vec![u32::MAX; cap], mask: cap - 1 };
        for (i, &h) in handles.iter().enumerate() {
            assert!(h != 0, "handle 0 is reserved");
            let mut s = Self::slot(h) & t.mask;
            while t.keys[s] != 0 && t.keys[s] != h {
                s = (s + 1) & t.mask;
            }
            t.keys[s] = h;
            t.values[s] = i as u32;
        }
        t
    }

    #[inline]
    fn slot(h: u64) -> usize {
        (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize
    }

    #[inline]
    pub fn get(&self, h: u64) -> Option<u32> {
        let mut s = Self::slot(h) & self.mask;
        loop {
            let k = self.keys[s];
            if k == h {
                return Some(self.values[s]);
            }
            if k == 0 {
                return None;
            }
            s = (s + 1) & self.mask;
        }
    }
}

/// Everything translation reads: built at start, shared read-only.
pub struct Context {
    pub pipelines: PipelineTable,
    /// console descriptor heap -> PC shader-visible heap (handles, not bytes)
    pub descriptors: Range,
    pub descriptor_increment: u64,
    /// console memory the GPU reads as buffers -> PC GPU addresses
    pub memory: Range,
    /// console addresses of buffers that take UAV barriers, sorted, -> resource index
    pub barrier_targets: Vec<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub calls: u32,
    pub draws: u32,
    pub elided: u32,
    pub refused: u32,
}

const NONE: u64 = u64::MAX;

/// Per-list translation state: what is bound, so repeats can be dropped.
struct Bound {
    pipeline: u64,
    tables: [u64; 4],
    cbvs: [u64; 4],
    vb: (u64, u32, u32),
    ib: (u64, u32),
}

impl Bound {
    fn new() -> Bound {
        Bound { pipeline: NONE, tables: [NONE; 4], cbvs: [NONE; 4], vb: (NONE, 0, 0), ib: (NONE, 0) }
    }
}

/// Translate one recorded list into `sink`. A call whose address or handle is unknown is refused
/// (counted, not emitted) — never guessed.
pub fn translate(ctx: &Context, list: &[Cmd], sink: &mut impl Sink) -> Stats {
    let mut st = Stats::default();
    let mut b = Bound::new();
    sink.begin();
    for cmd in list {
        st.calls += 1;
        match *cmd {
            Cmd::SetPipeline(h) => {
                if b.pipeline == h {
                    st.elided += 1;
                } else if let Some(i) = ctx.pipelines.get(h) {
                    b.pipeline = h;
                    sink.pipeline(i);
                } else {
                    st.refused += 1;
                }
            }
            Cmd::SetTable(slot, va) => {
                let s = slot as usize & 3;
                if b.tables[s] == va {
                    st.elided += 1;
                } else if let Some(off) = ctx.descriptors.map(va) {
                    b.tables[s] = va;
                    let index = (off - ctx.descriptors.pc) / CONSOLE_DESCRIPTOR;
                    sink.table(slot as u32, ctx.descriptors.pc + index * ctx.descriptor_increment);
                } else {
                    st.refused += 1;
                }
            }
            Cmd::SetCbv(slot, va) => {
                let s = slot as usize & 3;
                if b.cbvs[s] == va {
                    st.elided += 1;
                } else if let Some(pc) = ctx.memory.map(va) {
                    b.cbvs[s] = va;
                    sink.cbv(slot as u32, pc);
                } else {
                    st.refused += 1;
                }
            }
            Cmd::SetConstants(v) => sink.constants(&v),
            Cmd::SetVertexBuffer(va, bytes, stride) => {
                if b.vb == (va, bytes, stride) {
                    st.elided += 1;
                } else if let Some(pc) = ctx.memory.map(va) {
                    b.vb = (va, bytes, stride);
                    sink.vertex_buffer(pc, bytes, stride);
                } else {
                    st.refused += 1;
                }
            }
            Cmd::SetIndexBuffer(va, bytes) => {
                if b.ib == (va, bytes) {
                    st.elided += 1;
                } else if let Some(pc) = ctx.memory.map(va) {
                    b.ib = (va, bytes);
                    sink.index_buffer(pc, bytes);
                } else {
                    st.refused += 1;
                }
            }
            Cmd::Barrier(va) => match ctx.barrier_targets.binary_search(&va) {
                Ok(i) => sink.barrier(i as u32),
                Err(_) => st.refused += 1,
            },
            Cmd::Draw(n, inst, first) => {
                st.draws += 1;
                sink.draw(n, inst, first);
            }
            Cmd::DrawIndirect(va, count) => {
                st.draws += 1;
                match ctx.memory.map(va) {
                    Some(pc) => sink.draw_indirect(pc, count),
                    None => st.refused += 1,
                }
            }
        }
    }
    st
}

/// A sink that only counts: what translation costs with no driver behind it.
#[derive(Default)]
pub struct Count {
    pub emitted: u64,
    pub checksum: u64,
}

impl Count {
    #[inline]
    fn add(&mut self, v: u64) {
        self.emitted += 1;
        self.checksum = self.checksum.rotate_left(7) ^ v;
    }
}

impl Sink for Count {
    fn begin(&mut self) {}
    fn pipeline(&mut self, i: u32) { self.add(i as u64) }
    fn table(&mut self, s: u32, h: u64) { self.add(h ^ s as u64) }
    fn cbv(&mut self, s: u32, va: u64) { self.add(va ^ s as u64) }
    fn constants(&mut self, v: &[u32; 4]) { self.add(v[0] as u64) }
    fn vertex_buffer(&mut self, va: u64, b: u32, s: u32) { self.add(va ^ b as u64 ^ s as u64) }
    fn index_buffer(&mut self, va: u64, b: u32) { self.add(va ^ b as u64) }
    fn barrier(&mut self, r: u32) { self.add(r as u64) }
    fn draw(&mut self, n: u32, i: u32, f: u32) { self.add((n ^ i ^ f) as u64) }
    fn draw_indirect(&mut self, va: u64, c: u32) { self.add(va ^ c as u64) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);
    impl Sink for Log {
        fn begin(&mut self) { self.0.push("begin".into()) }
        fn pipeline(&mut self, i: u32) { self.0.push(format!("pso {i}")) }
        fn table(&mut self, s: u32, h: u64) { self.0.push(format!("table {s} {h:#x}")) }
        fn cbv(&mut self, s: u32, va: u64) { self.0.push(format!("cbv {s} {va:#x}")) }
        fn constants(&mut self, v: &[u32; 4]) { self.0.push(format!("const {}", v[0])) }
        fn vertex_buffer(&mut self, va: u64, _: u32, _: u32) { self.0.push(format!("vb {va:#x}")) }
        fn index_buffer(&mut self, va: u64, _: u32) { self.0.push(format!("ib {va:#x}")) }
        fn barrier(&mut self, r: u32) { self.0.push(format!("barrier {r}")) }
        fn draw(&mut self, n: u32, _: u32, _: u32) { self.0.push(format!("draw {n}")) }
        fn draw_indirect(&mut self, va: u64, c: u32) { self.0.push(format!("indirect {va:#x} {c}")) }
    }

    fn ctx() -> Context {
        Context {
            pipelines: PipelineTable::new(&[0xAAA, 0xBBB]),
            descriptors: Range { console: 0x1000_0000, bytes: 1024 * CONSOLE_DESCRIPTOR, pc: 0x9000 },
            descriptor_increment: 64,
            memory: Range { console: 0x2_0000_0000, bytes: 1 << 20, pc: 0x7_0000_0000 },
            barrier_targets: vec![0x2_0000_1000, 0x2_0000_2000],
        }
    }

    #[test]
    fn handles_and_addresses_are_translated() {
        let list = [
            Cmd::SetPipeline(0xBBB),
            Cmd::SetTable(0, 0x1000_0000 + 3 * CONSOLE_DESCRIPTOR),
            Cmd::SetCbv(1, 0x2_0000_0100),
            Cmd::Barrier(0x2_0000_2000),
            Cmd::DrawIndirect(0x2_0000_0200, 2),
        ];
        let mut log = Log::default();
        let st = translate(&ctx(), &list, &mut log);
        assert_eq!(
            log.0,
            vec!["begin", "pso 1", "table 0 0x90c0", "cbv 1 0x700000100", "barrier 1", "indirect 0x700000200 2"]
        );
        assert_eq!(st, Stats { calls: 5, draws: 1, elided: 0, refused: 0 });
    }

    #[test]
    fn repeated_state_is_dropped() {
        let list = [
            Cmd::SetPipeline(0xAAA),
            Cmd::SetTable(0, 0x1000_0000),
            Cmd::Draw(3, 1, 0),
            Cmd::SetPipeline(0xAAA),
            Cmd::SetTable(0, 0x1000_0000),
            Cmd::Draw(3, 1, 0),
        ];
        let mut log = Log::default();
        let st = translate(&ctx(), &list, &mut log);
        assert_eq!(st.elided, 2);
        assert_eq!(log.0.len(), 1 + 2 + 2);
    }

    #[test]
    fn unknown_handles_and_addresses_are_refused_not_guessed() {
        let list = [
            Cmd::SetPipeline(0xCCC),
            Cmd::SetTable(0, 0x5),
            Cmd::SetCbv(0, 0x1),
            Cmd::Barrier(0x2_0000_3000),
        ];
        let mut log = Log::default();
        let st = translate(&ctx(), &list, &mut log);
        assert_eq!(st.refused, 4);
        assert_eq!(log.0, vec!["begin"]);
    }

    #[test]
    fn pipeline_table_finds_every_handle() {
        let handles: Vec<u64> = (1..5000u64).map(|i| i.wrapping_mul(0x1234_5678_9ABC_DEF1) | 1).collect();
        let t = PipelineTable::new(&handles);
        for (i, h) in handles.iter().enumerate() {
            assert_eq!(t.get(*h), Some(i as u32));
        }
        assert_eq!(t.get(2), None);
    }
}
