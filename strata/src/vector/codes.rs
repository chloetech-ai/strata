//! The compressed codes a vector field is searched with: SQ8 (a byte per
//! dimension, [`super::sq8`]) or binary (a bit per dimension, [`super::bits`]). Either
//! way the best candidates are rescored with the float32 vectors.

use crate::schema::VectorCodes;
use crate::vector::bits::{Bits, BitsQuery};
use crate::vector::kernels;
use crate::vector::sq8::{Sq8, Sq8Query};

/// How many times more candidates a binary-code search rescores with
/// float32 than its `rerank` asks for (see `notes/vector.md`).
pub const BINARY_RERANK: usize = 8;

/// A field's quantiser.
#[derive(Clone, Debug)]
pub enum Quantiser {
    Sq8(Sq8),
    Bits(Bits),
}

impl Quantiser {
    pub fn kind(&self) -> VectorCodes {
        match self {
            Quantiser::Sq8(_) => VectorCodes::Sq8,
            Quantiser::Bits(_) => VectorCodes::Binary,
        }
    }

    /// Prepares `q` for scoring code rows of `stride` bytes.
    pub fn prepare(&self, q: &[f32], stride: usize) -> QueryCode {
        match self {
            Quantiser::Sq8(s) => QueryCode::Sq8(s.prepare(q, stride)),
            Quantiser::Bits(b) => QueryCode::Bits(b.prepare(q, stride)),
        }
    }

    /// The code of `x` into `out` (a whole code row).
    pub fn encode(&self, x: &[f32], out: &mut [u8]) {
        match self {
            Quantiser::Sq8(s) => s.encode(x, out),
            Quantiser::Bits(b) => b.encode(x, out),
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
}

impl QueryCode {
    #[inline]
    pub fn dot(&self, row: &[u8]) -> i32 {
        match self {
            QueryCode::Sq8(q) => q.dot(row),
            QueryCode::Bits(q) => q.dot(row),
        }
    }

    /// Scores of four rows (each query chunk is loaded once for all four).
    #[inline]
    pub fn dot4(&self, rows: [&[u8]; 4]) -> [i32; 4] {
        match self {
            QueryCode::Sq8(q) => kernels::dot_u8_i8_x4(rows, &q.q8),
            QueryCode::Bits(q) => q.dot4(rows),
        }
    }

    #[inline]
    pub fn score(&self, dot: i32) -> f32 {
        match self {
            QueryCode::Sq8(q) => q.score(dot),
            QueryCode::Bits(q) => q.score(dot),
        }
    }
}
