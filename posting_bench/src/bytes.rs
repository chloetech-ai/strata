//! Zero-copy storage for encoded structures.
//!
//! An encoded structure (a Loom list, a Pyramid bitmap, a bit-packed column)
//! keeps its arrays in [`Slice`]s. A `Slice<T>` is either an owned `Vec<T>`
//! (while building, or in tests) or a typed view into [`OwnedBytes`]: shared,
//! immutable bytes such as a memory-mapped file, kept alive by an `Arc`. Code
//! that reads the structure only sees `&[T]`, so the same algorithms run on
//! freshly built and on memory-mapped data.
//!
//! Serialised arrays are little-endian and 8-byte aligned within their
//! buffer; views require the buffer itself to be 8-byte aligned (a mapped
//! file and any section of it at an 8-byte offset are).

use std::fmt;
use std::ops::{Deref, Range};
use std::sync::Arc;

/// Immutable bytes shared through an `Arc`, sliceable without copying.
#[derive(Clone)]
pub struct OwnedBytes {
    owner: Arc<dyn AsRef<[u8]> + Send + Sync>,
    ptr: *const u8,
    len: usize,
}

// SAFETY: the bytes are immutable and owned by `owner`, which is Send + Sync.
unsafe impl Send for OwnedBytes {}
unsafe impl Sync for OwnedBytes {}

impl OwnedBytes {
    /// Wraps any byte owner (a `Vec<u8>`, a memory map, ...).
    pub fn new<S: AsRef<[u8]> + Send + Sync + 'static>(owner: S) -> Self {
        let owner: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(owner);
        let s = (*owner).as_ref();
        let (ptr, len) = (s.as_ptr(), s.len());
        OwnedBytes { owner, ptr, len }
    }

    pub fn empty() -> Self {
        OwnedBytes::new(Vec::<u8>::new())
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len describe a sub-slice of `owner`'s bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A sub-range sharing the same owner.
    pub fn slice(&self, r: Range<usize>) -> OwnedBytes {
        assert!(r.start <= r.end && r.end <= self.len, "slice {r:?} out of 0..{}", self.len);
        OwnedBytes { owner: self.owner.clone(), ptr: unsafe { self.ptr.add(r.start) }, len: r.end - r.start }
    }
}

impl Deref for OwnedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl fmt::Debug for OwnedBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OwnedBytes({} bytes)", self.len)
    }
}

/// Plain-old-data element types that may be viewed in place.
///
/// # Safety
/// Every bit pattern must be a valid value and the type must have no padding.
pub unsafe trait Pod: Copy + Send + Sync + 'static {}
unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for i8 {}
unsafe impl Pod for i16 {}
unsafe impl Pod for i32 {}
unsafe impl Pod for i64 {}
unsafe impl Pod for f32 {}
unsafe impl Pod for f64 {}

/// An array that is either owned or a zero-copy view into [`OwnedBytes`].
pub enum Slice<T: Pod> {
    Owned(Vec<T>),
    Shared { bytes: OwnedBytes, ptr: *const T, len: usize },
}

// SAFETY: Shared views are immutable and their owner is Send + Sync.
unsafe impl<T: Pod> Send for Slice<T> {}
unsafe impl<T: Pod> Sync for Slice<T> {}

impl<T: Pod> Slice<T> {
    /// A view of `bytes` as `[T]`. Fails if `bytes` is misaligned for `T` or
    /// its length is not a multiple of `size_of::<T>()`.
    pub fn view(bytes: OwnedBytes) -> Result<Self, String> {
        let sz = std::mem::size_of::<T>();
        if bytes.len() % sz != 0 {
            return Err(format!("{} bytes is not a multiple of {sz}", bytes.len()));
        }
        if (bytes.as_ptr() as usize) % std::mem::align_of::<T>() != 0 {
            return Err(format!("misaligned view: address {:p} for align {}", bytes.as_ptr(), std::mem::align_of::<T>()));
        }
        let (ptr, len) = (bytes.as_ptr() as *const T, bytes.len() / sz);
        Ok(Slice::Shared { bytes, ptr, len })
    }

    pub fn as_slice(&self) -> &[T] {
        match self {
            Slice::Owned(v) => v,
            // SAFETY: checked alignment and length in `view`; bytes outlive self.
            Slice::Shared { ptr, len, .. } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
        }
    }

    /// Size of the elements in bytes.
    pub fn byte_len(&self) -> usize {
        self.as_slice().len() * std::mem::size_of::<T>()
    }

    /// The raw little-endian bytes of the elements.
    pub fn as_bytes(&self) -> &[u8] {
        let s = self.as_slice();
        // SAFETY: Pod has no padding; any T may be read as bytes.
        unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
    }
}

impl<T: Pod> Deref for Slice<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Pod> Default for Slice<T> {
    fn default() -> Self {
        Slice::Owned(Vec::new())
    }
}

impl<T: Pod> Clone for Slice<T> {
    fn clone(&self) -> Self {
        match self {
            Slice::Owned(v) => Slice::Owned(v.clone()),
            Slice::Shared { bytes, ptr, len } => Slice::Shared { bytes: bytes.clone(), ptr: *ptr, len: *len },
        }
    }
}

impl<T: Pod + fmt::Debug> fmt::Debug for Slice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if matches!(self, Slice::Owned(_)) { "owned" } else { "shared" };
        write!(f, "Slice<{}>({kind}, {} elements)", std::any::type_name::<T>(), self.len())
    }
}

impl<T: Pod> From<Vec<T>> for Slice<T> {
    fn from(v: Vec<T>) -> Self {
        Slice::Owned(v)
    }
}

/// Appends arrays and scalars to a buffer, keeping every array 8-byte aligned
/// relative to the start of the buffer. Pair with [`ByteReader`].
#[derive(Default)]
pub struct ByteWriter {
    pub buf: Vec<u8>,
}

impl ByteWriter {
    pub fn new() -> Self {
        ByteWriter { buf: Vec::new() }
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes `len` (as u64), pads to 8 bytes, then the elements, then pads
    /// to 8 bytes again.
    pub fn array<T: Pod>(&mut self, a: &[T]) {
        self.u64(a.len() as u64);
        self.pad();
        // SAFETY: Pod has no padding.
        let bytes = unsafe { std::slice::from_raw_parts(a.as_ptr() as *const u8, std::mem::size_of_val(a)) };
        self.buf.extend_from_slice(bytes);
        self.pad();
    }

    pub fn pad(&mut self) {
        while self.buf.len() % 8 != 0 {
            self.buf.push(0);
        }
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads what [`ByteWriter`] wrote, returning arrays as zero-copy [`Slice`]s.
pub struct ByteReader {
    bytes: OwnedBytes,
    pos: usize,
}

impl ByteReader {
    pub fn new(bytes: OwnedBytes) -> Self {
        ByteReader { bytes, pos: 0 }
    }

    pub fn u64(&mut self) -> Result<u64, String> {
        let end = self.pos + 8;
        if end > self.bytes.len() {
            return Err("unexpected end of buffer".into());
        }
        let v = u64::from_le_bytes(self.bytes[self.pos..end].try_into().unwrap());
        self.pos = end;
        Ok(v)
    }

    pub fn array<T: Pod>(&mut self) -> Result<Slice<T>, String> {
        let n = self.u64()? as usize;
        let start = self.pos;
        let end = start + n * std::mem::size_of::<T>();
        if end > self.bytes.len() {
            return Err(format!("array of {n} elements runs past the buffer"));
        }
        self.pos = end.next_multiple_of(8);
        Slice::view(self.bytes.slice(start..end))
    }

    pub fn position(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_zero_copy() {
        let mut w = ByteWriter::new();
        w.u64(7);
        w.array(&[1u32, 2, 3]);
        w.array(&[10u64, 20]);
        w.array::<u16>(&[]);
        let bytes = OwnedBytes::new(w.finish());
        let base = bytes.as_ptr();
        let mut r = ByteReader::new(bytes);
        assert_eq!(r.u64().unwrap(), 7);
        let a: Slice<u32> = r.array().unwrap();
        let b: Slice<u64> = r.array().unwrap();
        let c: Slice<u16> = r.array().unwrap();
        assert_eq!(&*a, &[1, 2, 3]);
        assert_eq!(&*b, &[10, 20]);
        assert!(c.is_empty());
        assert!(matches!(b, Slice::Shared { .. }));
        assert_eq!(b.as_ptr() as usize % 8, 0);
        assert!(b.as_ptr() as usize > base as usize);
    }
}
