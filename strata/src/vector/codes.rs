//! The compressed codes a vector field is searched with: SQ8 (a byte per
//! dimension, [`super::sq8`]), binary (a bit per dimension,
//! [`super::bits`]) or RaBitQ (1, 2 or 4 bits per dimension of the rotated
//! vectors, [`super::rabitq`]). Either way the best candidates are rescored
//! with the float32 vectors.

use crate::schema::VectorCodes;
use crate::vector::bits::{Bits, BitsQuery};
use crate::vector::kernels;
use crate::vector::rabitq::{Rabitq, RabitqQuery};
use crate::vector::sq8::{Sq8, Sq8Query};

/// How many times more candidates a binary-code search rescores with
/// float32 than its `rerank` asks for (see `notes/vector.md`).
pub const BINARY_RERANK: usize = 8;

/// How many times more candidates than its `rerank` a search over `codes`
/// hands to the float32 rescoring: the coarser the codes, the more (see
/// `notes/vector.md`). Over RaBitQ codes the rescoring reads only those
/// whose error bound reaches the `k`-th exact score found so far.
pub fn oversample(codes: VectorCodes) -> usize {
    match codes {
        VectorCodes::Sq8 => 1,
        VectorCodes::Binary | VectorCodes::Rabitq1 => BINARY_RERANK,
        VectorCodes::Rabitq2 => 4,
        VectorCodes::Rabitq4 => 2,
    }
}

/// A field's quantiser.
#[derive(Clone, Debug)]
pub enum Quantiser {
    Sq8(Sq8),
    Bits(Bits),
    Rabitq(Rabitq),
}

impl Quantiser {
    pub fn kind(&self) -> VectorCodes {
        match self {
            Quantiser::Sq8(_) => VectorCodes::Sq8,
            Quantiser::Bits(_) => VectorCodes::Binary,
            Quantiser::Rabitq(r) => match r.bits {
                1 => VectorCodes::Rabitq1,
                2 => VectorCodes::Rabitq2,
                _ => VectorCodes::Rabitq4,
            },
        }
    }

    /// Prepares `q` for scoring code rows of `stride` bytes.
    pub fn prepare(&self, q: &[f32], stride: usize) -> QueryCode {
        match self {
            Quantiser::Sq8(s) => QueryCode::Sq8(s.prepare(q, stride)),
            Quantiser::Bits(b) => QueryCode::Bits(b.prepare(q, stride)),
            Quantiser::Rabitq(r) => QueryCode::Rabitq(r.prepare(q, stride)),
        }
    }

    /// The code of `x` into `out` (a whole code row).
    pub fn encode(&self, x: &[f32], out: &mut [u8]) {
        match self {
            Quantiser::Sq8(s) => s.encode(x, out),
            Quantiser::Bits(b) => b.encode(x, out),
            Quantiser::Rabitq(r) => r.encode(x, out),
        }
    }
}

/// A query prepared for scoring one kind of code. Scores are integers,
/// higher is more similar; [`QueryCode::score`] turns one into an
/// approximate dot product.
#[derive(Clone, Debug)]
pub enum QueryCode {
    Sq8(Sq8Query),
    Bits(BitsQuery),
    Rabitq(RabitqQuery),
}

impl QueryCode {
    #[inline]
    pub fn dot(&self, row: &[u8]) -> i32 {
        match self {
            QueryCode::Sq8(q) => q.dot(row),
            QueryCode::Bits(q) => q.dot(row),
            QueryCode::Rabitq(q) => q.dot(row),
        }
    }

    /// Scores of four rows (each query chunk is loaded once for all four).
    #[inline]
    pub fn dot4(&self, rows: [&[u8]; 4]) -> [i32; 4] {
        match self {
            QueryCode::Sq8(q) => kernels::dot_u8_i8_x4(rows, &q.q8),
            QueryCode::Bits(q) => q.dot4(rows),
            QueryCode::Rabitq(q) => q.dot4(rows),
        }
    }

    #[inline]
    pub fn score(&self, dot: i32) -> f32 {
        match self {
            QueryCode::Sq8(q) => q.score(dot),
            QueryCode::Bits(q) => q.score(dot),
            QueryCode::Rabitq(q) => q.score(dot),
        }
    }
}
