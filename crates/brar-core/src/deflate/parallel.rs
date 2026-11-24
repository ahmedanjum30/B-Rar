//! One Deflate stream on several cores.
//!
//! The compressed stream is cut every [`CHUNK`] bytes. For each cut a worker finds the first
//! block that starts after it ([`chunk::find_block`]) and decodes from there, without the
//! output before it, until a block ends past the next cut ([`chunk::decode_piece`]); so each
//! piece should start where the previous one ended.
//!
//! In order, on the calling thread, each piece is checked to start where the previous one
//! ended, and the 32 KB of output before it is worked out: usually the end of the previous
//! piece, at worst 32 KB of markers to replace. Where a piece can't be used (no block start
//! found, or a false one) the stretch is decoded there in order instead: slower, never wrong.
//! Replacing a piece's markers needs only those 32 KB, so the workers do it, in parallel; a
//! separate thread passes the finished pieces to the output in order.
//!
//! A cut inside a fixed-Huffman block (which has no header to find) is tried first: such a
//! piece starts at a symbol boundary [`SYNC`] bits past the cut ([`chunk::fixed_sync`]), and
//! every piece stops inside a fixed-Huffman block at the first symbol boundary that far past
//! the next cut, so the two meet. While the in-order stage is behind (the last cut gave
//! nothing usable) it decodes the next stretch itself rather than wait for a worker; and if
//! the whole first round of cuts gives nothing usable, the workers stop looking.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use super::{
    chunk::{self, Edge, Piece, WINDOW},
    inflate::{Bad, Tables},
};

/// Compressed bytes per piece.
pub(crate) const CHUNK: usize = 2 << 20;
/// Most bytes a piece may decode to: highly compressible data could otherwise expand a 2 MB
/// piece to gigabytes. Past it the whole stream is left to the single-threaded decoder.
const MAX_PIECE: usize = 256 << 20;
/// Bits past a cut where a piece inside a fixed-Huffman block starts: decoding from a wrong bit
/// falls into step with the real symbols within a few of them, far less than this.
const SYNC: usize = 1 << 14;

pub(crate) enum Failure<E> {
    /// The stream didn't decode here: another decoder should try (it may be damaged).
    Bad,
    /// The output callback failed: stop.
    Stop(E),
}

impl<E> From<Bad> for Failure<E> {
    fn from(_: Bad) -> Self {
        Failure::Bad
    }
}

/// A piece in order, with the 32 KB before it, waiting for its markers to be replaced.
struct Job {
    seq: usize,
    piece: Piece,
    window: Arc<Vec<u8>>,
}

/// A piece ready to go out: its head with markers replaced, then its tail.
struct Done {
    head: Vec<u8>,
    piece: Piece,
}

struct State {
    /// Decoded pieces by cut (`None` inside: no block start found after the cut).
    decoded: Vec<Option<Result<Option<Piece>, Bad>>>,
    /// Next cut to decode, and how many the in-order stage has taken or passed (bounds memory;
    /// cuts below it aren't needed any more).
    next_cut: usize,
    consumed: usize,
    /// The first round of cuts gave nothing usable: no more looking for block starts.
    give_up: bool,
    /// Pieces in order whose markers are to be replaced, and those done, by sequence number.
    jobs: VecDeque<Job>,
    done: Vec<Option<Result<Done, Bad>>>,
    /// Pieces in order so far, and whether that's all of them.
    sequenced: usize,
    finished: bool,
    abort: bool,
}

/// Decodes the Deflate stream `input[..len]` (followed by the decoder's padding) on `threads`
/// workers, passing the output to `out` in order. Returns the total output size.
pub(crate) fn inflate_parallel<E: Send>(
    input: &[u8],
    len: usize,
    threads: usize,
    out: &mut (dyn FnMut(&[u8]) -> Result<(), E> + Send),
) -> Result<u64, Failure<E>> {
    let n = len.div_ceil(CHUNK).max(1);
    let cut = |k: usize| (k * CHUNK * 8).min(len * 8);
    let workers = threads.max(1);
    let ahead = 2 * workers + 2;
    let state = Mutex::new(State {
        decoded: (0..n).map(|_| None).collect(),
        next_cut: 0,
        consumed: 0,
        give_up: false,
        jobs: VecDeque::new(),
        done: Vec::new(),
        sequenced: 0,
        finished: false,
        abort: false,
    });
    // One condition for everything: workers, the in-order stage and the output wait on it.
    let changed = Condvar::new();
    let stats = std::env::var_os("BRAR_STATS").is_some();
    let failed_out = AtomicBool::new(false);
    // BRAR_STATS: wall time the workers spend decoding and replacing markers, the output thread's.
    let (decode_ns, resolve_ns, output_ns) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut t = Tables::default();
                let mut s = state.lock().unwrap();
                loop {
                    if s.abort {
                        return;
                    }
                    // Replacing markers first: it unblocks the output.
                    if let Some(job) = s.jobs.pop_front() {
                        drop(s);
                        let w = std::time::Instant::now();
                        let mut head = Vec::new();
                        let r = chunk::resolve(&job.piece.head, &job.window, &mut head).map(|()| Done { head, piece: job.piece });
                        resolve_ns.fetch_add(w.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        s = state.lock().unwrap();
                        if s.done.len() <= job.seq {
                            s.done.resize_with(job.seq + 1, || None);
                        }
                        s.done[job.seq] = Some(r);
                        changed.notify_all();
                        continue;
                    }
                    if !s.give_up && s.next_cut < n && s.next_cut < s.consumed + ahead {
                        let k = s.next_cut;
                        s.next_cut += 1;
                        if k < s.consumed {
                            continue; // the in-order stage decoded past it already
                        }
                        drop(s);
                        let w = std::time::Instant::now();
                        let start = if k == 0 {
                            Some((0, Edge::Block))
                        } else {
                            chunk::fixed_sync(&mut t, input, len, cut(k), cut(k) + SYNC)
                                .map(|b| (b, Edge::Fixed(None)))
                                .or_else(|| chunk::find_block(&mut t, input, len, cut(k), cut(k + 1)).map(|b| (b, Edge::Block)))
                        };
                        let r = match start {
                            None => Ok(None),
                            Some((b, edge)) => chunk::decode_from(&mut t, input, len, b, edge, cut(k + 1), cut(k + 1) + SYNC, MAX_PIECE).map(Some),
                        };
                        decode_ns.fetch_add(w.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        s = state.lock().unwrap();
                        if k >= s.consumed {
                            s.decoded[k] = Some(r);
                            changed.notify_all();
                        }
                        continue;
                    }
                    if s.finished && s.jobs.is_empty() && (s.next_cut >= n || s.give_up) {
                        return;
                    }
                    s = changed.wait(s).unwrap();
                }
            });
        }

        // The output, in order, on its own thread.
        let output = scope.spawn(|| -> Result<u64, Failure<E>> {
            let mut total = 0u64;
            let mut seq = 0;
            loop {
                let d = {
                    let mut s = state.lock().unwrap();
                    loop {
                        if s.abort {
                            return Err(Failure::Bad);
                        }
                        if let Some(d) = s.done.get_mut(seq).and_then(Option::take) {
                            break d;
                        }
                        if s.finished && seq >= s.sequenced {
                            return Ok(total);
                        }
                        s = changed.wait(s).unwrap();
                    }
                };
                let d = d?;
                let w = std::time::Instant::now();
                let tail = d.piece.tail();
                for part in [&d.head[..], tail] {
                    if !part.is_empty()
                        && let Err(e) = out(part)
                    {
                        failed_out.store(true, Ordering::Relaxed);
                        return Err(Failure::Stop(e));
                    }
                }
                total += (d.head.len() + tail.len()) as u64;
                seq += 1;
                output_ns.fetch_add(w.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
        });

        // In order: check starts, fill gaps, work out each piece's window, hand out jobs.
        let sequence = (|| -> Result<(), Bad> {
            let mut t = Tables::default();
            // Where the stream continues, and what's there (a block boundary, or the inside
            // of a fixed-Huffman block).
            let (mut expected, mut edge) = (0usize, Edge::Block);
            let mut window = Arc::new(Vec::<u8>::new());
            let mut done = false;
            let (mut head_len, mut skipped, mut gaps, mut in_order) = (0usize, 0usize, 0usize, 0usize);
            // Cuts after the first that workers gave a piece (or nothing) for, and usable ones.
            let (mut tried, mut hits) = (0usize, 0usize);
            // The last cut gave nothing usable: decode in order rather than wait.
            let mut behind = false;
            let t0 = std::time::Instant::now();
            // Queues a piece in order and works out the window after it.
            let mut push = |piece: Piece, window: &mut Arc<Vec<u8>>| -> Result<(), Bad> {
                let tail = piece.tail();
                let next = if tail.len() >= WINDOW {
                    tail[tail.len() - WINDOW..].to_vec()
                } else {
                    // The old window's end, the head's end (markers replaced), the tail.
                    let want = WINDOW - tail.len();
                    let from = piece.head.len().saturating_sub(want);
                    let keep = want - (piece.head.len() - from);
                    let mut w = window[window.len().saturating_sub(keep)..].to_vec();
                    chunk::resolve(&piece.head[from..], window, &mut w)?;
                    w.extend_from_slice(tail);
                    w
                };
                head_len += piece.head.len();
                let mut s = state.lock().unwrap();
                let seq = s.sequenced;
                s.sequenced += 1;
                s.jobs.push_back(Job {
                    seq,
                    piece,
                    window: Arc::clone(window),
                });
                changed.notify_all();
                drop(s);
                *window = Arc::new(next);
                Ok(())
            };
            // Where the stream continues after `p`, taken at `expected` (of kind `edge`), and
            // whether it ended.
            let advance = |p: &Piece, edge: Edge| -> (usize, Edge, bool) {
                let block_last = match edge {
                    Edge::Fixed(last) => last,
                    Edge::Block => None,
                };
                match p.end_edge {
                    Edge::Fixed(last) => (p.end, Edge::Fixed(last.or(block_last)), false),
                    Edge::Block => (p.end, Edge::Block, p.last),
                }
            };
            // Does `p` start where the stream continues? (One that went on past the end of the
            // block it started in only if that block wasn't the final one.)
            let starts_at = |p: &Piece, expected: usize, edge: Edge| {
                if p.start_mid {
                    p.start == expected && (edge == Edge::Fixed(Some(false)) || (!p.open && edge != Edge::Block))
                } else {
                    edge == Edge::Block && chunk::same_start(input, expected, p.start)
                }
            };
            enum Next {
                Piece(Result<Option<Piece>, Bad>),
                /// Decoded in order past this cut already.
                Passed,
                /// Not ready while behind: decode its stretch in order.
                InOrder,
            }
            for k in 0..n {
                if done {
                    break;
                }
                let next = {
                    let mut s = state.lock().unwrap();
                    let next = loop {
                        if s.abort {
                            return Err(Bad);
                        }
                        if k > 0 && cut(k + 1) <= expected {
                            s.decoded[k] = None;
                            break Next::Passed;
                        }
                        if let Some(r) = s.decoded[k].take() {
                            break Next::Piece(r);
                        }
                        if behind || s.give_up {
                            break Next::InOrder;
                        }
                        s = changed.wait(s).unwrap();
                    };
                    s.consumed = k + 1;
                    next
                };
                changed.notify_all();
                let r = match next {
                    Next::Passed => continue,
                    Next::InOrder => {
                        in_order += 1;
                        let p = chunk::decode_from(&mut t, input, len, expected, edge, cut(k + 1), cut(k + 1) + SYNC, MAX_PIECE)?;
                        (expected, edge, done) = advance(&p, edge);
                        push(p, &mut window)?;
                        continue;
                    }
                    Next::Piece(r) => r,
                };
                if k > 0 {
                    tried += 1;
                }
                let piece = match r {
                    Ok(Some(p)) if starts_at(&p, expected, edge) || p.start > expected => Some(p),
                    // No start found, a false one, or a piece that didn't decode: the stretch is
                    // decoded in order (by the next usable piece's gap, or by this stage).
                    _ => None,
                };
                let mut used = false;
                if let Some(piece) = piece {
                    let mut fits = true;
                    if !starts_at(&piece, expected, edge) {
                        // A gap before this piece (blocks the finder skips): decode it in order.
                        gaps += 1;
                        let gap = chunk::decode_from(&mut t, input, len, expected, edge, piece.start, piece.start, MAX_PIECE)?;
                        (expected, edge, done) = advance(&gap, edge);
                        push(gap, &mut window)?;
                        // The gap ran past it: the piece started falsely.
                        fits = !done && starts_at(&piece, expected, edge);
                    }
                    if fits {
                        (expected, edge, done) = advance(&piece, edge);
                        push(piece, &mut window)?;
                        used = true;
                    }
                }
                if used {
                    hits += usize::from(k > 0);
                    behind = false;
                } else {
                    skipped += 1;
                    behind = true;
                }
                if hits == 0 && tried >= workers.min(n - 1) {
                    state.lock().unwrap().give_up = true;
                }
            }
            // Whatever is left after the last usable piece, in order.
            while !done {
                let stop = expected + CHUNK * 8;
                let p = chunk::decode_from(&mut t, input, len, expected, edge, stop, stop, MAX_PIECE)?;
                (expected, edge, done) = advance(&p, edge);
                push(p, &mut window)?;
            }
            if stats {
                eprintln!(
                    "deflate: {n} pieces, {skipped} skipped, {gaps} gaps, {in_order} in order; heads {} MB; in order {:.3} s",
                    head_len >> 20,
                    t0.elapsed().as_secs_f64()
                );
            }
            Ok(())
        })();

        let mut s = state.lock().unwrap();
        s.finished = true;
        s.give_up = true; // no more cuts needed
        if sequence.is_err() {
            s.abort = true;
        }
        drop(s);
        changed.notify_all();
        let result = output.join().expect("output thread panicked");
        if result.is_err() {
            state.lock().unwrap().abort = true;
            changed.notify_all();
        }
        if stats {
            let s = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64 / 1e9;
            eprintln!(
                "deflate: workers {:.3} s decoding + {:.3} s replacing markers ({workers} workers); output thread busy {:.3} s",
                s(&decode_ns),
                s(&resolve_ns),
                s(&output_ns)
            );
        }
        match (sequence, result) {
            (Err(Bad), _) if !failed_out.load(Ordering::Relaxed) => Err(Failure::Bad),
            (_, r) => r,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::inflate::INPUT_PADDING;
    use super::*;
    use std::io::Write;

    /// The biggest Deflate entry of each ZIP in $BRAR_BENCH_ZIP (`;`-separated): libdeflate
    /// on one core against [`inflate_parallel`] on all of them, wall time, CRC checked. Run with
    /// `cargo test --release -p brar-core --lib parallel_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn parallel_bench() {
        let threads = std::thread::available_parallelism().unwrap().get();
        let paths = std::env::var("BRAR_BENCH_ZIP").expect("set BRAR_BENCH_ZIP");
        for path in paths.split(';').filter(|p| !p.is_empty()) {
            let file = std::fs::File::open(path).unwrap();
            let mut archive = ::zip::ZipArchive::new(&file).unwrap();
            let (mut start, mut packed_len, mut size, mut crc) = (0, 0, 0, 0);
            for i in 0..archive.len() {
                let f = archive.by_index_raw(i).unwrap();
                if f.compression() == ::zip::CompressionMethod::Deflated && f.size() > size {
                    (start, packed_len, size, crc) = (f.data_start().unwrap(), f.compressed_size() as usize, f.size(), f.crc32());
                }
            }
            let mut packed = vec![0u8; packed_len + INPUT_PADDING];
            crate::whole::read_at(&file, start, &mut packed[..packed_len]).unwrap();
            let mut out = vec![0u8; size as usize];
            let mut d = libdeflater::Decompressor::new();
            let one = (0..3)
                .map(|_| {
                    let t = std::time::Instant::now();
                    d.deflate_decompress(&packed[..packed_len], &mut out).unwrap();
                    t.elapsed().as_secs_f64()
                })
                .fold(f64::MAX, f64::min);
            let mut many = f64::MAX;
            for _ in 0..3 {
                let mut h = crc32fast::Hasher::new();
                let t = std::time::Instant::now();
                let total = inflate_parallel::<()>(&packed, packed_len, threads, &mut |b: &[u8]| {
                    h.update(b);
                    Ok(())
                })
                .ok()
                .expect("parallel decode failed");
                many = many.min(t.elapsed().as_secs_f64());
                assert_eq!(total, size);
                assert_eq!(h.finalize(), crc);
            }
            let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy();
            println!(
                "{name:<12} {} MB: libdeflate, 1 core {one:.3} s; parallel, {threads} threads {many:.3} s ({:.1}x)",
                size >> 20,
                one / many
            );
        }
    }

    fn check(data: &[u8], level: u32, threads: usize) {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
        e.write_all(data).unwrap();
        let mut packed = e.finish().unwrap();
        let len = packed.len();
        packed.resize(len + INPUT_PADDING, 0);
        let mut got = Vec::new();
        let total = inflate_parallel::<()>(&packed, len, threads, &mut |b: &[u8]| {
            got.extend_from_slice(b);
            Ok(())
        })
        .ok()
        .unwrap();
        assert_eq!(total as usize, data.len());
        assert!(got == data, "level {level}, {threads} threads, {} bytes", data.len());
    }

    #[test]
    fn matches_the_input() {
        let mut x = 3u64;
        let mut data = Vec::new();
        while data.len() < 24 << 20 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if x.is_multiple_of(3) {
                data.extend((0..500).map(|i| (x >> (i % 16)) as u8));
            } else if !data.is_empty() {
                let back = 1 + (x as usize >> 8) % 32_000.min(data.len());
                let start = data.len() - back;
                for i in 0..(30 + (x as usize >> 30) % 300) {
                    data.push(data[start + i % back]);
                }
            } else {
                data.push(1);
            }
        }
        for level in [1, 6] {
            for threads in [1, 4, 16] {
                check(&data, level, threads);
            }
        }
        check(b"tiny", 6, 4);
        check(&[], 6, 4);
    }

    /// Text in base64's alphabet: zlib-rs's level 1 writes it as one fixed-Huffman block.
    fn base64ish(size: usize, seed: u64) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut x = seed;
        (0..size)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if i % 77 == 76 { b'\n' } else { ALPHABET[(x % 64) as usize] }
            })
            .collect()
    }

    fn decode_all(packed: &[u8], len: usize, threads: usize) -> Option<Vec<u8>> {
        let mut got = Vec::new();
        inflate_parallel::<()>(packed, len, threads, &mut |b: &[u8]| {
            got.extend_from_slice(b);
            Ok(())
        })
        .ok()
        .map(|total| {
            assert_eq!(total as usize, got.len());
            got
        })
    }

    /// One stream from parts compressed at different levels: each part sync-flushed (an empty
    /// stored block, byte-aligned) and the last finished, so blocks of each kind follow each other.
    fn mixed(parts: &[(&[u8], u32)]) -> Vec<u8> {
        let mut packed = Vec::new();
        for (i, &(data, level)) in parts.iter().enumerate() {
            let mut c = flate2::Compress::new(flate2::Compression::new(level), false);
            let flush = if i + 1 == parts.len() {
                flate2::FlushCompress::Finish
            } else {
                flate2::FlushCompress::Sync
            };
            let mut out = Vec::with_capacity(data.len() * 2 + 1024);
            c.compress_vec(data, &mut out, flush).unwrap();
            packed.extend_from_slice(&out);
        }
        packed
    }

    #[test]
    fn fixed_huffman_streams() {
        // One fixed-Huffman block of 40 MB: pieces must start inside it.
        let data = base64ish(40 << 20, 11);
        for threads in [4, 16] {
            check(&data, 1, threads);
        }
        // Fixed and dynamic blocks in turn, ending with either kind.
        let (a, b, c, d) = (
            base64ish(9 << 20, 1),
            base64ish(7 << 20, 2),
            base64ish(13 << 20, 3),
            base64ish(5 << 20, 4),
        );
        for parts in [
            vec![(&a[..], 1), (&b[..], 6), (&c[..], 1), (&d[..], 6)],
            vec![(&a[..], 6), (&b[..], 1), (&c[..], 6), (&d[..], 1)],
            vec![(&a[..], 1), (&b[..], 1), (&c[..], 6), (&d[..], 1)],
        ] {
            let want: Vec<u8> = parts.iter().flat_map(|p| p.0.iter().copied()).collect();
            let mut packed = mixed(&parts);
            let len = packed.len();
            packed.resize(len + INPUT_PADDING, 0);
            for threads in [4, 16] {
                let got = decode_all(&packed, len, threads).expect("decodes");
                assert!(
                    got == want,
                    "{threads} threads, levels {:?}",
                    parts.iter().map(|p| p.1).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn damaged_fixed_stream_fails_cleanly() {
        let data = base64ish(24 << 20, 5);
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(1));
        e.write_all(&data).unwrap();
        let good = e.finish().unwrap();
        let len = good.len();
        let mut x = 99u64;
        for _ in 0..8 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let mut packed = good.clone();
            packed[(x as usize) % len] ^= 1 << ((x >> 40) % 8);
            packed.resize(len + INPUT_PADDING, 0);
            // Wrong output or an error, never a hang or a panic (the caller checks the CRC).
            if let Some(got) = decode_all(&packed, len, 8) {
                assert!(got != data || packed[..len] == good[..]);
            }
        }
    }
}
