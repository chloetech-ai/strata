//! Opening a text field and its posting lists copies no per-term,
//! per-posting or per-document array: a counting allocator watches every
//! allocation while a reader opens over an in-memory segment image.
//! (One test in this binary, so no other thread allocates meanwhile.)

mod text_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use strata::text::{PostingsFormat, TextFieldReader};
use text_common::*;

struct Counting;

static BYTES: AtomicUsize = AtomicUsize::new(0);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        BYTES.fetch_add(l.size(), Relaxed);
        LARGEST.fetch_max(l.size(), Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        BYTES.fetch_add(new, Relaxed);
        LARGEST.fetch_max(new, Relaxed);
        System.realloc(p, l, new)
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Bytes allocated (and the largest allocation) while running `f`.
fn allocated<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    BYTES.store(0, Relaxed);
    LARGEST.store(0, Relaxed);
    let r = f();
    (r, BYTES.load(Relaxed), LARGEST.load(Relaxed))
}

#[test]
fn open_and_cursors_do_not_copy() {
    let texts = synthetic(5, 20_000);
    for (format, min_df) in [(PostingsFormat::Loom, 256), (PostingsFormat::Bp128, 0)] {
        let seg = build(&texts, format, min_df, true);
        let image = seg.image().len();
        let (r, bytes, largest) = allocated(|| TextFieldReader::open(&seg, "body").unwrap());
        eprintln!("{format:?}: open allocated {bytes} bytes (largest {largest}) for a {image}-byte segment");
        assert!(bytes < 4096 && largest < 1024, "{format:?}: open allocated {bytes} bytes");
        assert!(r.is_zero_copy());
        // The longest list: a cursor allocates its small frontier only.
        let mut longest = (String::new(), 0);
        r.for_each_term(|t, i| {
            if i.df > longest.1 {
                longest = (String::from_utf8(t.to_vec()).unwrap(), i.df);
            }
        });
        let ((), bytes, _) = allocated(|| {
            let mut c = r.postings(&longest.0).unwrap();
            assert_eq!(c.is_loom(), format == PostingsFormat::Loom);
            while c.doc() != strata::text::END {
                c.freq();
                c.next();
            }
        });
        eprintln!("{format:?}: a pass over {} postings allocated {bytes} bytes", longest.1);
        assert!(bytes < 1024, "{format:?}: cursor allocated {bytes} bytes");
        assert!(longest.1 > 5000);
    }
}
