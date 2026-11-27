// Derived from VIctory (Marvisak and contributors) via jleybourne/Triumph, GPL-3.0-or-later.
// Used here under GPL-3.0-only. Changes: only the constant data of `victory-formats/src/bcpack.rs` (lane
// widths, the public BC6H/BC7 layout tables and the console's BC6H run order); the decoder is ours.

/// Lanes and symbols per lane per block, by BC number (index 1 and 3-7).
pub(crate) const BC1_LANES: [u8; 12] = [7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7];
pub(crate) const BC3_LANES: [u8; 12] = [3, 3, 7, 5, 5, 5, 5, 4, 5, 4, 7, 7];
pub(crate) const BC4_LANES: [u8; 12] = [7, 7, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5];
pub(crate) const BC5_LANES: [u8; 12] = [3, 3, 7, 7, 5, 5, 5, 5, 5, 5, 5, 5];
pub(crate) const BC6_LANES: [u8; 16] = [3, 3, 7, 7, 7, 4, 4, 4, 5, 5, 5, 7, 7, 7, 7, 4];
pub(crate) const BC7_LANES: [u8; 16] = [3, 3, 7, 7, 7, 7, 7, 6, 6, 4, 5, 7, 7, 7, 7, 4];

/// Public BC7 anchor indices: second subset of two; second and third of three.
pub(crate) const ANCHOR2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8, 2, 2, 8, 8, 2, 2, 15,
    15, 6, 8, 2, 8, 15, 15, 2, 8, 2, 2, 2, 15, 15, 6, 6, 2, 6, 8, 15, 15, 2, 2, 15, 15, 15, 15, 15, 2, 2, 15,
];
pub(crate) const ANCHOR3A: [u8; 64] = [
    3, 3, 15, 15, 8, 3, 15, 15, 8, 8, 6, 6, 6, 5, 3, 3, 3, 3, 8, 15, 3, 3, 6, 10, 5, 8, 8, 6, 8, 5, 15, 15, 8, 15, 3, 5,
    6, 10, 8, 15, 15, 3, 15, 5, 15, 15, 15, 15, 3, 15, 5, 5, 5, 8, 5, 10, 5, 10, 8, 13, 15, 12, 3, 3,
];
pub(crate) const ANCHOR3B: [u8; 64] = [
    15, 8, 8, 3, 15, 15, 3, 8, 15, 15, 15, 15, 15, 15, 15, 8, 15, 8, 15, 3, 15, 8, 15, 8, 3, 15, 6, 10, 15, 15, 10, 8, 15,
    3, 15, 10, 10, 8, 9, 10, 6, 15, 8, 15, 3, 6, 6, 8, 15, 3, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 3, 15, 15, 8,
];

/// BC7 endpoint bits of R G B A per mode, before p-bits and rotation.
pub(crate) const BC7_BITS: [[u8; 4]; 8] =
    [[4, 4, 4, 0], [6, 6, 6, 0], [5, 5, 5, 0], [7, 7, 7, 0], [5, 5, 5, 6], [7, 7, 7, 8], [7, 7, 7, 7], [5, 5, 5, 5]];
/// BC7 index bits of the primary and secondary sets per mode.
pub(crate) const BC7_INDEX: [[u8; 2]; 8] = [[3, 0], [3, 0], [2, 0], [2, 0], [3, 2], [2, 2], [4, 0], [2, 0]];

/// BC6H bits of endpoint 0, then of the other endpoints' R, G, B, per mode 1-14.
pub(crate) const BC6_BITS: [[u8; 4]; 15] = [
    [0; 4],
    [10, 5, 5, 5],
    [7, 6, 6, 6],
    [11, 5, 4, 4],
    [11, 4, 5, 4],
    [11, 4, 4, 5],
    [9, 5, 5, 5],
    [8, 6, 5, 5],
    [8, 5, 6, 5],
    [8, 5, 5, 6],
    [6, 6, 6, 6],
    [10, 10, 10, 10],
    [11, 9, 9, 9],
    [12, 8, 8, 8],
    [16, 4, 4, 4],
];
/// BC6H mode code and its width, modes 1-14.
pub(crate) const BC6_CODE: [(u64, u32); 15] = [
    (0, 0),
    (0, 2),
    (1, 2),
    (2, 5),
    (6, 5),
    (10, 5),
    (14, 5),
    (18, 5),
    (22, 5),
    (26, 5),
    (30, 5),
    (3, 5),
    (7, 5),
    (11, 5),
    (15, 5),
];

/// A run of the BC6H header: `len` bits of channel `ch` (0-2; 3 is the partition) of endpoint `ep`, from bit
/// `first` up.
#[derive(Clone, Copy)]
pub(crate) struct Run {
    pub ch: u8,
    pub ep: u8,
    pub first: u8,
    pub len: u8,
}

const fn r(ch: u8, ep: u8, first: u8, len: u8) -> Run {
    Run { ch, ep, first, len }
}

const R: u8 = 0;
const G: u8 = 1;
const B: u8 = 2;
const D: Run = r(3, 0, 0, 5);

/// The BC6H header after the mode bits, per mode 1-14. Modes 13 and 14 write the high base bits in ascending
/// order (what the console's packer does).
#[rustfmt::skip]
pub(crate) const BC6_LAYOUT: [&[Run]; 15] = [
    &[],
    &[
        r(G, 2, 4, 1), r(B, 2, 4, 1), r(B, 3, 4, 1), r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10),
        r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4), r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4),
        r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5),
        r(B, 3, 3, 1), D,
    ],
    &[
        r(G, 2, 5, 1), r(G, 3, 4, 1), r(G, 3, 5, 1), r(R, 0, 0, 7), r(B, 3, 0, 1), r(B, 3, 1, 1),
        r(B, 2, 4, 1), r(G, 0, 0, 7), r(B, 2, 5, 1), r(B, 3, 2, 1), r(G, 2, 4, 1), r(B, 0, 0, 7),
        r(B, 3, 3, 1), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 6),
        r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 6), r(R, 3, 0, 6), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 5), r(R, 0, 10, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 4), r(G, 0, 10, 1), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 4), r(B, 0, 10, 1),
        r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 1), r(G, 3, 4, 1),
        r(G, 2, 0, 4), r(G, 1, 0, 5), r(G, 0, 10, 1), r(G, 3, 0, 4), r(B, 1, 0, 4), r(B, 0, 10, 1),
        r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 4), r(B, 3, 0, 1), r(B, 3, 2, 1), r(R, 3, 0, 4),
        r(G, 2, 4, 1), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 1), r(B, 2, 4, 1),
        r(G, 2, 0, 4), r(G, 1, 0, 4), r(G, 0, 10, 1), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 5),
        r(B, 0, 10, 1), r(B, 2, 0, 4), r(R, 2, 0, 4), r(B, 3, 1, 1), r(B, 3, 2, 1), r(R, 3, 0, 4),
        r(B, 3, 4, 1), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 9), r(B, 2, 4, 1), r(G, 0, 0, 9), r(G, 2, 4, 1), r(B, 0, 0, 9), r(B, 3, 4, 1),
        r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4), r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4),
        r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5), r(B, 3, 2, 1), r(R, 3, 0, 5),
        r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 8), r(G, 3, 4, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(B, 3, 2, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(B, 3, 3, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 5),
        r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 6),
        r(R, 3, 0, 6), D,
    ],
    &[
        r(R, 0, 0, 8), r(B, 3, 0, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(G, 2, 5, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(G, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 6), r(G, 3, 0, 4), r(B, 1, 0, 5), r(B, 3, 1, 1), r(B, 2, 0, 4), r(R, 2, 0, 5),
        r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 8), r(B, 3, 1, 1), r(B, 2, 4, 1), r(G, 0, 0, 8), r(B, 2, 5, 1), r(G, 2, 4, 1),
        r(B, 0, 0, 8), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 5), r(G, 3, 4, 1), r(G, 2, 0, 4),
        r(G, 1, 0, 5), r(B, 3, 0, 1), r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 5),
        r(B, 3, 2, 1), r(R, 3, 0, 5), r(B, 3, 3, 1), D,
    ],
    &[
        r(R, 0, 0, 6), r(G, 3, 4, 1), r(B, 3, 0, 1), r(B, 3, 1, 1), r(B, 2, 4, 1), r(G, 0, 0, 6),
        r(G, 2, 5, 1), r(B, 2, 5, 1), r(B, 3, 2, 1), r(G, 2, 4, 1), r(B, 0, 0, 6), r(G, 3, 5, 1),
        r(B, 3, 3, 1), r(B, 3, 5, 1), r(B, 3, 4, 1), r(R, 1, 0, 6), r(G, 2, 0, 4), r(G, 1, 0, 6),
        r(G, 3, 0, 4), r(B, 1, 0, 6), r(B, 2, 0, 4), r(R, 2, 0, 6), r(R, 3, 0, 6), D,
    ],
    &[r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 10), r(G, 1, 0, 10), r(B, 1, 0, 10)],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 9), r(R, 0, 10, 1), r(G, 1, 0, 9),
        r(G, 0, 10, 1), r(B, 1, 0, 9), r(B, 0, 10, 1),
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 8), r(R, 0, 10, 2), r(G, 1, 0, 8),
        r(G, 0, 10, 2), r(B, 1, 0, 8), r(B, 0, 10, 2),
    ],
    &[
        r(R, 0, 0, 10), r(G, 0, 0, 10), r(B, 0, 0, 10), r(R, 1, 0, 4), r(R, 0, 10, 6), r(G, 1, 0, 4),
        r(G, 0, 10, 6), r(B, 1, 0, 4), r(B, 0, 10, 6),
    ],
];
