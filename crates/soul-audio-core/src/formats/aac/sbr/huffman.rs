//! SBR envelope/noise Huffman trees + tree-walk decode
//! (`sbr-reference.md` §6.1, verbatim tables in `sbr-reference-verbatim.md`
//! §3). Binary-tree form (faad2 `sbr_huff.c` convention): each row is a
//! non-leaf node `{left, right}`; walking starts at row index 0, reading one
//! bit at a time (`node = t[node][bit]`); once `node < 0` it's a leaf and the
//! decoded (signed) delta is `node + 64`.
//!
//! The 10 tables below were transcribed from `-verbatim.md` §3 by a
//! throwaway Python script (not committed) that also **verified, for every
//! table**: the declared row count, each table's own `[first]`/`[last]`
//! anchor pair, AND — beyond what the brief asked for — a full tree-shape
//! check (DFS from node 0 with cycle/out-of-bounds detection, confirming
//! every one of the N rows is reachable exactly once and the walk always
//! terminates in exactly N+1 distinct leaves, the structural signature of a
//! well-formed N-internal-node binary Huffman tree). All 10 tables passed
//! every check. See `task-7-report.md` for the exact anchor values checked.

use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// A Huffman tree in the faad2 binary-tree form: row `i` is `[left, right]`;
/// a non-negative entry is the next row index, a negative entry `-v` is leaf
/// value `v - 64` (i.e. decoded delta `= entry + 64`).
pub(super) type SbrHuff = &'static [[i16; 2]];

#[rustfmt::skip]
pub(super) static T_HUFF_ENV_1_5DB: [[i16; 2]; 120] = [
    [1, 2], [-64, -65], [3, 4], [-63, -66], [5, 6], [-62, -67], [7, 8], [-61, -68],
    [9, 10], [-60, -69], [11, 12], [-59, -70], [13, 14], [-58, -71], [15, 16], [-57, -72],
    [17, 18], [-73, -56], [19, 21], [-74, 20], [-55, -75], [22, 26], [23, 24], [-54, -76],
    [-77, 25], [-53, -78], [27, 34], [28, 29], [-52, -79], [30, 31], [-80, -51], [32, 33],
    [-83, -82], [-81, -50], [35, 57], [36, 40], [37, 38], [-88, -84], [-48, 39], [-90, -85],
    [41, 46], [42, 43], [-49, -87], [44, 45], [-89, -86], [-124, -123], [47, 50], [48, 49],
    [-122, -121], [-120, -119], [51, 54], [52, 53], [-118, -117], [-116, -115], [55, 56], [-114, -113],
    [-112, -111], [58, 89], [59, 74], [60, 67], [61, 64], [62, 63], [-110, -109], [-108, -107],
    [65, 66], [-106, -105], [-104, -103], [68, 71], [69, 70], [-102, -101], [-100, -99], [72, 73],
    [-98, -97], [-96, -95], [75, 82], [76, 79], [77, 78], [-94, -93], [-92, -91], [80, 81],
    [-47, -46], [-45, -44], [83, 86], [84, 85], [-43, -42], [-41, -40], [87, 88], [-39, -38],
    [-37, -36], [90, 105], [91, 98], [92, 95], [93, 94], [-35, -34], [-33, -32], [96, 97],
    [-31, -30], [-29, -28], [99, 102], [100, 101], [-27, -26], [-25, -24], [103, 104], [-23, -22],
    [-21, -20], [106, 113], [107, 110], [108, 109], [-19, -18], [-17, -16], [111, 112], [-15, -14],
    [-13, -12], [114, 117], [115, 116], [-11, -10], [-9, -8], [118, 119], [-7, -6], [-5, -4],
];

#[rustfmt::skip]
pub(super) static F_HUFF_ENV_1_5DB: [[i16; 2]; 120] = [
    [1, 2], [-64, -65], [3, 4], [-63, -66], [5, 6], [-67, -62], [7, 8], [-68, -61],
    [9, 10], [-69, -60], [11, 13], [-70, 12], [-59, -71], [14, 16], [-58, 15], [-72, -57],
    [17, 19], [-73, 18], [-56, -74], [20, 23], [21, 22], [-55, -75], [-54, -53], [24, 27],
    [25, 26], [-76, -52], [-77, -51], [28, 31], [29, 30], [-50, -78], [-79, -49], [32, 36],
    [33, 34], [-48, -47], [-80, 35], [-81, -82], [37, 47], [38, 41], [39, 40], [-83, -46],
    [-45, -84], [42, 44], [-85, 43], [-44, -43], [45, 46], [-88, -87], [-86, -90], [48, 66],
    [49, 56], [50, 53], [51, 52], [-92, -42], [-41, -39], [54, 55], [-105, -89], [-38, -37],
    [57, 60], [58, 59], [-94, -91], [-40, -36], [61, 63], [-20, 62], [-115, -110], [64, 65],
    [-108, -107], [-101, -97], [67, 89], [68, 75], [69, 72], [70, 71], [-95, -93], [-34, -27],
    [73, 74], [-22, -17], [-16, -124], [76, 82], [77, 79], [-123, 78], [-122, -121], [80, 81],
    [-120, -119], [-118, -117], [83, 86], [84, 85], [-116, -114], [-113, -112], [87, 88], [-111, -109],
    [-106, -104], [90, 105], [91, 98], [92, 95], [93, 94], [-103, -102], [-100, -99], [96, 97],
    [-98, -96], [-35, -33], [99, 102], [100, 101], [-32, -31], [-30, -29], [103, 104], [-28, -26],
    [-25, -24], [106, 113], [107, 110], [108, 109], [-23, -21], [-19, -18], [111, 112], [-15, -14],
    [-13, -12], [114, 117], [115, 116], [-11, -10], [-9, -8], [118, 119], [-7, -6], [-5, -4],
];

#[rustfmt::skip]
pub(super) static T_HUFF_ENV_BAL_1_5DB: [[i16; 2]; 48] = [
    [-64, 1], [-62, 2], [-66, 3], [-60, 4], [-68, 5], [-58, 6], [-70, 7], [-56, 8],
    [-72, 9], [10, 11], [-74, -54], [12, 13], [-76, -52], [14, 28], [15, 21], [16, 18],
    [-50, 17], [-78, -48], [19, 20], [-112, -110], [-108, -106], [22, 25], [23, 24], [-104, -102],
    [-100, -98], [26, 27], [-96, -94], [-92, -90], [29, 36], [30, 33], [31, 32], [-88, -86],
    [-84, -82], [34, 35], [-80, -46], [-44, -42], [37, 41], [38, 39], [-40, -38], [-36, 40],
    [-34, -32], [42, 45], [43, 44], [-30, -28], [-26, -24], [46, 47], [-22, -20], [-18, -16],
];

#[rustfmt::skip]
pub(super) static F_HUFF_ENV_BAL_1_5DB: [[i16; 2]; 48] = [
    [-64, 1], [-66, 2], [-62, 3], [-68, 4], [-60, 5], [-58, 6], [-70, 7], [-72, 8],
    [-56, 9], [10, 11], [-74, -54], [-76, 12], [-52, 13], [14, 17], [-78, 15], [-50, 16],
    [-48, -82], [18, 32], [19, 25], [20, 22], [-80, 21], [-112, -110], [23, 24], [-108, -106],
    [-104, -102], [26, 29], [27, 28], [-100, -98], [-96, -94], [30, 31], [-92, -90], [-88, -86],
    [33, 40], [34, 37], [35, 36], [-84, -46], [-44, -42], [38, 39], [-40, -38], [-36, -34],
    [41, 44], [42, 43], [-32, -30], [-28, -26], [45, 46], [-24, -22], [-20, 47], [-18, -16],
];

#[rustfmt::skip]
pub(super) static T_HUFF_ENV_3_0DB: [[i16; 2]; 62] = [
    [-64, 1], [-65, 2], [-63, 3], [-66, 4], [-62, 5], [-67, 6], [-61, 7], [-68, 8],
    [-60, 9], [10, 11], [-69, -59], [12, 14], [-70, 13], [-71, -58], [15, 18], [16, 17],
    [-72, -57], [-73, -74], [19, 22], [-56, 20], [-55, 21], [-54, -77], [23, 31], [24, 25],
    [-75, -76], [26, 27], [-78, -53], [28, 29], [-52, -95], [-94, 30], [-93, -92], [32, 47],
    [33, 40], [34, 37], [35, 36], [-91, -90], [-89, -88], [38, 39], [-87, -86], [-85, -84],
    [41, 44], [42, 43], [-83, -82], [-81, -80], [45, 46], [-79, -51], [-50, -49], [48, 55],
    [49, 52], [50, 51], [-48, -47], [-46, -45], [53, 54], [-44, -43], [-42, -41], [56, 59],
    [57, 58], [-40, -39], [-38, -37], [60, 61], [-36, -35], [-34, -33],
];

#[rustfmt::skip]
pub(super) static F_HUFF_ENV_3_0DB: [[i16; 2]; 62] = [
    [-64, 1], [-65, 2], [-63, 3], [-66, 4], [-62, 5], [-67, 6], [7, 8], [-61, -68],
    [9, 10], [-60, -69], [11, 12], [-59, -70], [13, 14], [-58, -71], [15, 16], [-57, -72],
    [17, 19], [-56, 18], [-55, -73], [20, 24], [21, 22], [-74, -54], [-53, 23], [-75, -76],
    [25, 30], [26, 27], [-52, -51], [28, 29], [-77, -79], [-50, -49], [31, 39], [32, 35],
    [33, 34], [-78, -46], [-82, -88], [36, 37], [-83, -48], [-47, 38], [-86, -85], [40, 47],
    [41, 44], [42, 43], [-80, -44], [-43, -42], [45, 46], [-39, -87], [-84, -40], [48, 55],
    [49, 52], [50, 51], [-95, -94], [-93, -92], [53, 54], [-91, -90], [-89, -81], [56, 59],
    [57, 58], [-45, -41], [-38, -37], [60, 61], [-36, -35], [-34, -33],
];

#[rustfmt::skip]
pub(super) static T_HUFF_ENV_BAL_3_0DB: [[i16; 2]; 24] = [
    [-64, 1], [-62, 2], [-66, 3], [-68, 4], [-60, 5], [-58, 6], [-70, 7], [-72, 8],
    [-56, 9], [10, 16], [11, 13], [-74, 12], [-88, -86], [14, 15], [-84, -82], [-80, -78],
    [17, 20], [18, 19], [-76, -54], [-52, -50], [21, 22], [-48, -46], [-44, 23], [-42, -40],
];

#[rustfmt::skip]
pub(super) static F_HUFF_ENV_BAL_3_0DB: [[i16; 2]; 24] = [
    [-64, 1], [-66, 2], [-62, 3], [-68, 4], [-60, 5], [-58, 6], [-70, 7], [-72, 8],
    [-56, 9], [10, 13], [-74, 11], [-54, 12], [-52, -88], [14, 17], [15, 16], [-86, -84],
    [-82, -80], [18, 21], [19, 20], [-78, -76], [-50, -48], [22, 23], [-46, -44], [-42, -40],
];

#[rustfmt::skip]
pub(super) static T_HUFF_NOISE_3_0DB: [[i16; 2]; 62] = [
    [-64, 1], [-63, 2], [-65, 3], [-66, 4], [-62, 5], [-67, 6], [7, 8], [-61, -68],
    [9, 30], [10, 15], [-60, 11], [-69, 12], [13, 14], [-59, -53], [-95, -94], [16, 23],
    [17, 20], [18, 19], [-93, -92], [-91, -90], [21, 22], [-89, -88], [-87, -86], [24, 27],
    [25, 26], [-85, -84], [-83, -82], [28, 29], [-81, -80], [-79, -78], [31, 46], [32, 39],
    [33, 36], [34, 35], [-77, -76], [-75, -74], [37, 38], [-73, -72], [-71, -70], [40, 43],
    [41, 42], [-58, -57], [-56, -55], [44, 45], [-54, -52], [-51, -50], [47, 54], [48, 51],
    [49, 50], [-49, -48], [-47, -46], [52, 53], [-45, -44], [-43, -42], [55, 58], [56, 57],
    [-41, -40], [-39, -38], [59, 60], [-37, -36], [-35, 61], [-34, -33],
];

#[rustfmt::skip]
pub(super) static T_HUFF_NOISE_BAL_3_0DB: [[i16; 2]; 24] = [
    [-64, 1], [-66, 2], [-62, 3], [4, 9], [-68, 5], [-60, 6], [7, 8], [-88, -86],
    [-84, -82], [10, 17], [11, 14], [12, 13], [-80, -78], [-76, -74], [15, 16], [-72, -70],
    [-58, -56], [18, 21], [19, 20], [-54, -52], [-50, -48], [22, 23], [-46, -44], [-42, -40],
];

/// Walk a binary Huffman tree (faad2 `sbr_huff_dec`): start at node index 0;
/// each step reads 1 bit (MSB-first) and moves to `t[node][bit]`; once the
/// new node value is negative it's a leaf and the decoded signed delta is
/// `node + 64`. Bounded to at most `t.len() + 1` steps — one more than the
/// table's row count, which is always enough for any acyclic tree built over
/// `t.len()` rows (a walk can visit each row at most once before hitting a
/// leaf) — so a malformed/corrupt table that formed a cycle, or an
/// out-of-range node index, can't spin forever or index out of bounds: both
/// become `Err(DecodeError::Malformed)`. Every bit read is clamped against
/// `end_bit` (a read that would cross it also becomes `Err`).
pub(super) fn sbr_huff_dec(
    bits: &mut BitReader,
    end_bit: usize,
    t: SbrHuff,
) -> Result<i32, DecodeError> {
    let mut node: i32 = 0;
    for _ in 0..=t.len() {
        let idx = usize::try_from(node).map_err(|_| DecodeError::Malformed)?;
        let row = t.get(idx).ok_or(DecodeError::Malformed)?;
        if bits.bit_pos() + 1 > end_bit {
            return Err(DecodeError::Malformed);
        }
        let bit = bits.read_bit();
        node = row[bit as usize] as i32;
        if node < 0 {
            return Ok(node + 64);
        }
    }
    Err(DecodeError::Malformed) // walk exceeded the tree's own size -> cycle/corrupt table
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn huff_tree_walk_and_offset() {
        // Synthetic 3-node tree: node0 -> bit0 leaf(-64)-> value 0, bit1 leaf(-63)-> value 1.
        static T: [[i16; 2]; 1] = [[-64, -63]];
        use crate::formats::aac::bits::BitReader;
        let buf = [0b0100_0000]; // bit0=0 then bit0=1
        let mut b = BitReader::new(&buf);
        assert_eq!(sbr_huff_dec(&mut b, 8, &T).unwrap(), 0); // -64 + 64
        assert_eq!(sbr_huff_dec(&mut b, 8, &T).unwrap(), 1); // -63 + 64
    }

    #[test]
    fn env_huffman_table_sizes() {
        // Transcription anchors from -verbatim.md §3.
        assert_eq!(T_HUFF_ENV_1_5DB.len(), 120);
        assert_eq!(T_HUFF_ENV_3_0DB.len(), 62);
        assert_eq!(T_HUFF_ENV_BAL_3_0DB.len(), 24);
    }

    #[test]
    fn all_ten_tree_sizes_and_endpoint_anchors() {
        // Every table's declared size + its own [first]/[last] anchor pair
        // from -verbatim.md §3 (beyond the brief's 3-table spot check).
        assert_eq!(F_HUFF_ENV_1_5DB.len(), 120);
        assert_eq!(T_HUFF_ENV_BAL_1_5DB.len(), 48);
        assert_eq!(F_HUFF_ENV_BAL_1_5DB.len(), 48);
        assert_eq!(F_HUFF_ENV_3_0DB.len(), 62);
        assert_eq!(F_HUFF_ENV_BAL_3_0DB.len(), 24);
        assert_eq!(T_HUFF_NOISE_3_0DB.len(), 62);
        assert_eq!(T_HUFF_NOISE_BAL_3_0DB.len(), 24);

        assert_eq!(T_HUFF_ENV_1_5DB[0], [1, 2]);
        assert_eq!(T_HUFF_ENV_1_5DB[119], [-5, -4]);
        assert_eq!(F_HUFF_ENV_1_5DB[0], [1, 2]);
        assert_eq!(F_HUFF_ENV_1_5DB[119], [-5, -4]);
        assert_eq!(T_HUFF_ENV_BAL_1_5DB[0], [-64, 1]);
        assert_eq!(T_HUFF_ENV_BAL_1_5DB[47], [-18, -16]);
        assert_eq!(F_HUFF_ENV_BAL_1_5DB[0], [-64, 1]);
        assert_eq!(F_HUFF_ENV_BAL_1_5DB[47], [-18, -16]);
        assert_eq!(T_HUFF_ENV_3_0DB[0], [-64, 1]);
        assert_eq!(T_HUFF_ENV_3_0DB[61], [-34, -33]);
        assert_eq!(F_HUFF_ENV_3_0DB[0], [-64, 1]);
        assert_eq!(F_HUFF_ENV_3_0DB[61], [-34, -33]);
        assert_eq!(T_HUFF_ENV_BAL_3_0DB[0], [-64, 1]);
        assert_eq!(T_HUFF_ENV_BAL_3_0DB[23], [-42, -40]);
        assert_eq!(F_HUFF_ENV_BAL_3_0DB[0], [-64, 1]);
        assert_eq!(F_HUFF_ENV_BAL_3_0DB[23], [-42, -40]);
        assert_eq!(T_HUFF_NOISE_3_0DB[0], [-64, 1]);
        assert_eq!(T_HUFF_NOISE_3_0DB[61], [-34, -33]);
        assert_eq!(T_HUFF_NOISE_BAL_3_0DB[0], [-64, 1]);
        assert_eq!(T_HUFF_NOISE_BAL_3_0DB[23], [-42, -40]);
    }

    #[test]
    fn read_past_end_bit_is_malformed_not_panic() {
        let buf = [0xFFu8];
        // T_HUFF_ENV_1_5DB's first bit alone can't reach a leaf; end_bit=0
        // leaves no room for even the first read.
        let mut b = BitReader::new(&buf);
        assert!(sbr_huff_dec(&mut b, 0, &T_HUFF_ENV_1_5DB).is_err());
    }

    #[test]
    fn malformed_out_of_range_root_is_rejected_not_panicking() {
        // A 1-row table whose only entries point past the table (node 5 is
        // out of range for a 1-row tree) must error, not panic/OOB-index.
        static BAD: [[i16; 2]; 1] = [[5, 5]];
        let buf = [0x00u8];
        let mut b = BitReader::new(&buf);
        assert!(sbr_huff_dec(&mut b, 8, &BAD).is_err());
    }
}
