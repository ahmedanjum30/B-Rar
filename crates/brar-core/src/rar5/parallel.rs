//! Two-phase RAR5 decoding, for files too big for one core and for solid archives.
//!
//! Phase A, in parallel: the compressed stream's blocks, grouped into chunks that each start
//! at a block carrying its own Huffman tables, are decoded into tokens: literal bytes, and
//! match / repeat / filter instructions with the repeated distances still symbolic. Chunks
//! don't depend on each other, so any number of threads can work on them.
//!
//! Phase B, in order, on the calling thread: the tokens are applied into a sliding window
//! (the last `window` bytes of output, as far back as a match can reach), which resolves the
//! repeated distances, and finished bytes are streamed out. Filters are applied to a copy at
//! that point: later matches still read the unfiltered window.
//!
//! The stream runs over one file (a big file of a normal archive) or over every file of a
//! solid archive in order, with tables, recent distances and window carried across files.

use std::{
    collections::VecDeque,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    },
};

use super::{
    Entry, Source,
    unpack::{
        self, Bits, Corrupt, DCB, DCX, E_LITERAL, E_MATCH, Filter, INPUT_PADDING, MAX_MATCH, Recent, Res, Tables, filter_block, read_block_header,
        read_dist, read_filter, read_tables, slot_to_length,
    },
};

/// Where phase B sends finished bytes, file by file (indices into the stream's files).
pub(crate) trait Output {
    fn start(&mut self, file: usize) -> crate::Result<()>;
    fn write(&mut self, file: usize, data: &[u8]) -> crate::Result<()>;
    /// The file is complete and its CRC matched.
    fn finish(&mut self, file: usize) -> crate::Result<()>;
}

pub(crate) enum Failure {
    /// The data didn't decode, or a CRC didn't match: another decoder should try.
    Corrupt,
    /// Writing failed or the user cancelled: stop.
    Stop(crate::Error),
}

impl From<Corrupt> for Failure {
    fn from(_: Corrupt) -> Self {
        Failure::Corrupt
    }
}

/// CPU cycles this thread has used (for BRAR_STATS).
fn thread_cycles() -> u64 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> isize;
        fn QueryThreadCycleTime(thread: isize, cycles: *mut u64) -> i32;
    }
    let mut c = 0;
    // SAFETY: the current thread's pseudo-handle and a valid out pointer.
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
}

/// Compressed bytes per chunk (phase A's unit of work).
const CHUNK_PACKED: usize = 1 << 20;
/// Window space beyond what must be kept, so it slides (a memmove of everything kept) only
/// every so often: at least this much, and as much as the dictionary up to [`MAX_ROOM`], so a
/// big dictionary doesn't mean moving it all every few MB (256 MB per 32 MB of output, at a
/// 256 MB dictionary, measured +1.7 cycles per byte).
const SLIDE_ROOM: usize = 32 << 20;
const MAX_ROOM: usize = 1 << 30;

/// One block of the stream, found by the header scan.
#[derive(Clone, Copy)]
struct Span {
    file: usize,
    /// Offsets in the file's packed data of its header and of the next block's header.
    start: usize,
    end: usize,
    has_tables: bool,
}

/// Consecutive blocks decoded by one worker. `tables_from` is the block whose tables apply
/// at `first` (itself, or an earlier one).
struct Chunk {
    first: usize,
    last: usize,
    tables_from: usize,
}

/// Phase A's output for one chunk.
#[derive(Default)]
struct Tokens {
    lits: Vec<u8>,
    ops: Vec<Op>,
}

/// One non-literal symbol, after `lits` literals. `len`'s top 4 bits hold the kind.
#[derive(Clone, Copy)]
struct Op {
    lits: u32,
    len: u32,
    dist: u64,
}

const KIND_SHIFT: u32 = 28;
const MATCH: u32 = 0;
/// REPEAT + n: reuse recent distance n (0..=3).
const REPEAT: u32 = 1;
/// Repeat the last match exactly (symbol 257).
const LAST: u32 = 5;
/// `dist` = start (relative to the output position) | length << 32; `len` low bits =
/// channels, bits 8..: filter type.
const FILTER: u32 = 6;
/// Only the literals before it (the end of a chunk).
const TAIL: u32 = 7;
/// The current file's data ends after the literals before it; `dist` is the next file with
/// data (chunks span files).
const FILE_END: u32 = 8;

/// Decodes `files` (in stream order: one file, or every data file of a solid archive) from
/// the archive's volumes, sending the output to `out`. `window` is the dictionary size.
pub(crate) fn decode(src: &Source, files: &[&Entry], window: usize, threads: usize, out: &mut (dyn Output + Send)) -> Result<(), Failure> {
    let t0 = std::time::Instant::now();
    let spans = scan(src, files)?;
    let scanned = t0.elapsed();
    let chunks = group(&spans)?;
    let total: usize = files.iter().map(|e| e.size as usize).sum();
    // Phase A needs only a few cores to keep ahead of phase B (the serial step), and more
    // workers share physical cores with phase B and the drain thread, slowing them down:
    // measured best at about half the logical CPUs.
    let workers = if threads >= 8 { threads / 2 } else { threads.max(1) };
    // Chunks decoded ahead of phase B, which bounds the tokens held in memory.
    let ahead = 2 * workers + 2;

    struct Queue {
        slots: Vec<Option<Res<Tokens>>>,
        consumed: usize,
    }
    let queue = Mutex::new(Queue {
        slots: (0..chunks.len()).map(|_| None).collect(),
        consumed: 0,
    });
    // Phase B waits on `ready` for the next chunk; workers wait on `space` when too far ahead.
    let (ready, space) = (Condvar::new(), Condvar::new());
    // Token buffers phase B is done with, reused by the workers (fresh ones cost page faults).
    let spare: Mutex<Vec<Tokens>> = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let phase_a = AtomicU64::new(0);
    let stats = std::env::var_os("BRAR_STATS").is_some();
    let phase_b_start = thread_cycles();

    let (result, phase_b, drain_cycles) = std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut tables = Tables::new();
                let mut input = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i < chunks.len() {
                        let mut q = queue.lock().unwrap();
                        while i >= q.consumed + ahead && !abort.load(Ordering::Relaxed) {
                            q = space.wait(q).unwrap();
                        }
                    }
                    if i >= chunks.len() || abort.load(Ordering::Relaxed) {
                        phase_a.fetch_add(thread_cycles(), Ordering::Relaxed);
                        return;
                    }
                    let k = spare.lock().unwrap().pop().unwrap_or_default();
                    let tokens = decode_chunk(src, &spans, &chunks[i], files, &mut tables, &mut input, k);
                    queue.lock().unwrap().slots[i] = Some(tokens);
                    ready.notify_one();
                }
            });
        }

        let (tx, rx) = sync_channel(BATCHES);
        let (spare_tx, spare_rx) = channel();
        let abort = &abort;
        let drained = scope.spawn(move || {
            let r = drain(rx, spare_tx, abort, out);
            (r, thread_cycles())
        });
        let mut out = Drain {
            tx: Some(tx),
            spare: spare_rx,
            batch: Vec::with_capacity(BATCH),
            file: 0,
        };
        let out = &mut out;
        let mut w = Window::new(window.min(total), total);
        let result = (|| -> Result<(), Failure> {
            let mut current = FileState::new(files, 0, 0);
            out.start(0, &files[0].check)?;
            let mut waited = std::time::Duration::ZERO;
            for (i, chunk) in chunks.iter().enumerate() {
                let tokens = {
                    let mut q = queue.lock().unwrap();
                    loop {
                        if let Some(t) = q.slots[i].take() {
                            q.consumed = i + 1;
                            break t;
                        }
                        let t = std::time::Instant::now();
                        q = ready.wait(q).unwrap();
                        waited += t.elapsed();
                    }
                };
                space.notify_all();
                let tokens = tokens?;
                // A chunk starting with a later file's data: the files before it are done.
                let first_file = spans[chunk.first].file;
                if first_file > current.index {
                    w.switch(&mut current, first_file, files, out)?;
                }
                w.apply(&tokens, &mut current, files, out)?;
                spare.lock().unwrap().push(tokens);
            }
            w.finish_file(&mut current, out)?;
            if stats {
                eprintln!(
                    "engine: scan {:.3} s, phase B done at {:.3} s, waited {:.3} s for phase A",
                    scanned.as_secs_f64(),
                    t0.elapsed().as_secs_f64(),
                    waited.as_secs_f64()
                );
            }
            // Empty files after the last one with data.
            for index in current.index + 1..files.len() {
                let mut f = FileState::new(files, index, w.pos);
                out.start(index, &files[index].check)?;
                w.finish_file(&mut f, out)?;
            }
            Ok(())
        })();
        if result.is_err() {
            abort.store(true, Ordering::Relaxed);
            space.notify_all();
        }
        let phase_b = thread_cycles() - phase_b_start;
        // Hang up, so the drain thread finishes; its failure is the real one.
        out.tx = None;
        let (drained, drain_cycles) = drained.join().expect("drain thread panicked");
        (drained.and(result), phase_b, drain_cycles)
    });
    if stats {
        eprintln!(
            "engine: window {} MB, {} chunks, {} blocks, phase A {:.2} cycles/B, phase B {:.2} cycles/B, drain {:.2} cycles/B",
            window.min(total) >> 20,
            chunks.len(),
            spans.len(),
            phase_a.load(Ordering::Relaxed) as f64 / total.max(1) as f64,
            phase_b as f64 / total.max(1) as f64,
            drain_cycles as f64 / total.max(1) as f64
        );
    }
    result
}
/// Reads the block headers of every file's packed data (each header gives its block's size,
/// so nothing is decoded here).
fn scan(src: &Source, files: &[&Entry]) -> Res<Vec<Span>> {
    let mut spans = Vec::new();
    let mut h = Vec::new();
    for (file, e) in files.iter().enumerate() {
        if e.packed == 0 {
            continue;
        }
        let (mut at, stop) = (0usize, e.packed as usize);
        loop {
            // flags | check | size (1-3 bytes)
            h.clear();
            e.read(src, at as u64, (at + 5).min(stop) as u64, &mut h)?;
            let flags = *h.first().ok_or(Corrupt)?;
            let bytes = usize::from((flags >> 3) & 3) + 1;
            if bytes == 4 || at + 2 + bytes > stop {
                return Err(Corrupt);
            }
            let size = (0..bytes).fold(0u32, |v, i| v | u32::from(h[2 + i]) << (8 * i));
            let check = (0x5A ^ u32::from(flags) ^ size ^ (size >> 8) ^ (size >> 16)) & 0xff;
            let end = at + 2 + bytes + size as usize;
            if check != u32::from(h[1]) || end > stop {
                return Err(Corrupt);
            }
            spans.push(Span {
                file,
                start: at,
                end,
                has_tables: flags & 0x80 != 0,
            });
            if flags & 0x40 != 0 {
                break; // the file's last block
            }
            at = end;
        }
    }
    Ok(spans)
}

/// Groups blocks into chunks of about [`CHUNK_PACKED`], starting at a block with tables where
/// possible. Chunks span files: a solid archive of many small files still gets few chunks.
fn group(spans: &[Span]) -> Res<Vec<Chunk>> {
    let mut chunks = Vec::new();
    let mut tables_at = None;
    let mut i = 0;
    while i < spans.len() {
        if spans[i].has_tables {
            tables_at = Some(i);
        }
        // The first block of the stream must carry tables.
        let tables_from = tables_at.ok_or(Corrupt)?;
        let mut bytes = spans[i].end - spans[i].start;
        let mut j = i + 1;
        while j < spans.len() && !(spans[j].has_tables && bytes >= CHUNK_PACKED) {
            if spans[j].has_tables {
                tables_at = Some(j);
            }
            bytes += spans[j].end - spans[j].start;
            j += 1;
        }
        chunks.push(Chunk {
            first: i,
            last: j,
            tables_from,
        });
        i = j;
    }
    Ok(chunks)
}

/// `input` = bytes `from..to` of `e`'s packed data, followed by the zero padding the bit
/// reader needs.
fn fill(input: &mut Vec<u8>, src: &Source, e: &Entry, from: usize, to: usize) -> Res<()> {
    input.clear();
    e.read(src, from as u64, to as u64, input)?;
    input.resize(to - from + INPUT_PADDING, 0);
    Ok(())
}

fn dist_codes(e: &Entry) -> usize {
    if e.extra_dist { DCX } else { DCB }
}

/// Phase A for one chunk, into `k` (a recycled buffer). `input` is the worker's scratch.
fn decode_chunk(src: &Source, spans: &[Span], c: &Chunk, files: &[&Entry], t: &mut Tables, input: &mut Vec<u8>, mut k: Tokens) -> Res<Tokens> {
    k.lits.clear();
    k.ops.clear();
    if c.tables_from != c.first {
        // This chunk continues with an earlier block's tables: read them from that block.
        let s = spans[c.tables_from];
        fill(input, src, files[s.file], s.start, s.end)?;
        let mut bits = Bits::new(input, s.end - s.start);
        let limit = (s.end - s.start + 16) * 8;
        if !read_block_header(&mut bits, limit)?.has_tables {
            return Err(Corrupt);
        }
        read_tables(&mut bits, t, dist_codes(files[s.file]), limit)?;
    }
    let mut mark = 0;
    let mut b = c.first;
    while b < c.last {
        // The chunk's blocks of one file are contiguous in its packed data.
        let file = spans[b].file;
        if b > c.first {
            k.ops.push(Op {
                lits: (k.lits.len() - mark) as u32,
                len: FILE_END << KIND_SHIFT,
                dist: file as u64,
            });
            mark = k.lits.len();
        }
        let mut e = b + 1;
        while e < c.last && spans[e].file == file {
            e += 1;
        }
        let (from, to) = (spans[b].start, spans[e - 1].end);
        fill(input, src, files[file], from, to)?;
        let mut bits = Bits::new(input, to - from);
        let limit = (to - from + 16) * 8;
        let codes = dist_codes(files[file]);
        for _ in b..e {
            let block = read_block_header(&mut bits, limit)?;
            if block.has_tables {
                read_tables(&mut bits, t, codes, limit)?;
            }
            bits = tokenize(t, bits, block.end.min(limit), &mut k, &mut mark);
            if bits.pos() < block.end {
                return Err(Corrupt); // stopped at the input limit, not the block end
            }
        }
        b = e;
    }
    // Literals after the last op travel as an op with no payload.
    if k.lits.len() > mark {
        k.ops.push(Op {
            lits: (k.lits.len() - mark) as u32,
            len: TAIL << KIND_SHIFT,
            dist: 0,
        });
    }
    Ok(k)
}
/// Decodes one block's symbols into tokens. Like the single-threaded decoder's hot loop, it
/// takes the reader by value, so it stays in registers.
#[inline(never)]
fn tokenize<'a>(t: &Tables, mut bits: Bits<'a>, end: usize, k: &mut Tokens, mark: &mut usize) -> Bits<'a> {
    let fast_end = end.saturating_sub(32);
    loop {
        if k.lits.capacity() - k.lits.len() < 2 {
            k.lits.reserve(1 << 16);
        }
        let e;
        if bits.pos() < fast_end {
            bits.refill_below(32);
            let e1 = t.main_symbol(&mut bits);
            if e1 & E_LITERAL != 0 {
                push(&mut k.lits, (e1 >> 16) as u8);
                let e2 = t.main_symbol(&mut bits);
                if e2 & E_LITERAL != 0 {
                    push(&mut k.lits, (e2 >> 16) as u8);
                    continue;
                }
                e = e2;
            } else {
                e = e1;
            }
        } else {
            if bits.pos() >= end {
                return bits;
            }
            bits.refill_below(32);
            let e1 = t.main_symbol(&mut bits);
            if e1 & E_LITERAL != 0 {
                push(&mut k.lits, (e1 >> 16) as u8);
                continue;
            }
            e = e1;
        }
        bits.refill();
        let lits = (k.lits.len() - *mark) as u32;
        *mark = k.lits.len();
        let op = if e & E_MATCH != 0 {
            let mut len = (e >> 16) + bits.take_upto((e >> 10) & 15);
            bits.refill();
            let dist = read_dist(&mut bits, t) as u64;
            len += u32::from(dist > 0x100) + u32::from(dist > 0x2000) + u32::from(dist > 0x40000);
            Op { lits, len, dist }
        } else {
            match e >> 16 {
                256 => {
                    let mut b = bits;
                    let f = read_filter(&mut b, 0);
                    bits = b;
                    Op {
                        lits,
                        len: FILTER << KIND_SHIFT | f.kind << 8 | f.channels as u32,
                        dist: f.start as u64 | (f.len as u64) << 32,
                    }
                }
                257 => Op {
                    lits,
                    len: LAST << KIND_SHIFT,
                    dist: 0,
                },
                n => {
                    let slot = t.rep_slot(&mut bits);
                    let len = slot_to_length(&mut bits, slot) as u32;
                    Op {
                        lits,
                        len: (REPEAT + n - 258) << KIND_SHIFT | len,
                        dist: 0,
                    }
                }
            }
        };
        k.ops.push(op);
    }
}

#[inline(always)]
fn push(v: &mut Vec<u8>, b: u8) {
    // SAFETY: the caller reserved room for two bytes per loop round.
    unsafe {
        v.as_mut_ptr().add(v.len()).write(b);
        v.set_len(v.len() + 1);
    }
}

/// The file phase B is producing.
struct FileState {
    index: usize,
    /// Its bytes are stream positions start..end.
    start: usize,
    end: usize,
}

impl FileState {
    fn new(files: &[&Entry], index: usize, start: usize) -> Self {
        Self {
            index,
            start,
            end: start + files[index].size as usize,
        }
    }
}

/// Bytes handed from phase B to the drain thread at a time.
const BATCH: usize = 1 << 20;
/// Batches in flight between them.
const BATCHES: usize = 8;

enum Msg {
    /// A file starts; its data must have this checksum.
    Start(usize, super::Check),
    Data(usize, Vec<u8>),
    Finish(usize),
}

/// Phase B's end of the drain: finished bytes go out in batches to the drain thread, which
/// computes the CRCs and writes the files, so phase B (the one serial step) only decodes.
struct Drain {
    tx: Option<SyncSender<Msg>>,
    /// Batches the drain thread is done with.
    spare: Receiver<Vec<u8>>,
    batch: Vec<u8>,
    file: usize,
}

impl Drain {
    fn send(&self, m: Msg) -> Result<(), Failure> {
        // The drain thread only hangs up after a failure, which it returns itself.
        self.tx.as_ref().ok_or(Failure::Corrupt)?.send(m).map_err(|_| Failure::Corrupt)
    }

    fn flush(&mut self) -> Result<(), Failure> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let next = self.spare.try_recv().unwrap_or_else(|_| Vec::with_capacity(BATCH));
        let full = std::mem::replace(&mut self.batch, next);
        self.send(Msg::Data(self.file, full))
    }

    fn start(&mut self, file: usize, check: &super::Check) -> Result<(), Failure> {
        self.flush()?;
        self.file = file;
        self.send(Msg::Start(file, check.clone()))
    }

    fn write(&mut self, mut data: &[u8]) -> Result<(), Failure> {
        while !data.is_empty() {
            let n = data.len().min(BATCH - self.batch.len());
            self.batch.extend_from_slice(&data[..n]);
            data = &data[n..];
            if self.batch.len() == BATCH {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn finish(&mut self, file: usize) -> Result<(), Failure> {
        self.flush()?;
        self.send(Msg::Finish(file))
    }
}

/// The drain thread: CRCs and output, in order.
fn drain(rx: Receiver<Msg>, spare: Sender<Vec<u8>>, abort: &AtomicBool, out: &mut (dyn Output + Send)) -> Result<(), Failure> {
    let mut hasher = None;
    for m in rx {
        if abort.load(Ordering::Relaxed) {
            break;
        }
        match m {
            Msg::Start(file, check) => {
                hasher = Some((check.hasher(), check));
                out.start(file).map_err(Failure::Stop)?;
            }
            Msg::Data(file, mut data) => {
                hasher.as_mut().ok_or(Failure::Corrupt)?.0.update(&data);
                out.write(file, &data).map_err(Failure::Stop)?;
                data.clear();
                let _ = spare.send(data);
            }
            Msg::Finish(file) => {
                let (h, check) = hasher.take().ok_or(Failure::Corrupt)?;
                if !check.ok(h) {
                    return Err(Failure::Corrupt);
                }
                out.finish(file).map_err(Failure::Stop)?;
            }
        }
    }
    Ok(())
}

/// Phase B's sliding window: `buf[0]` is stream position `base`; output so far ends at `pos`;
/// bytes before `emitted` have been sent.
struct Window {
    buf: Vec<u8>,
    base: usize,
    pos: usize,
    emitted: usize,
    /// How far back matches may reach (the dictionary size).
    keep: usize,
    recent: Recent,
    /// Filters waiting for their block to be complete, in stream order.
    filters: VecDeque<Filter>,
    filtered: Vec<u8>,
    scratch: Vec<u8>,
    /// Free space after a slide.
    slide: usize,
}

/// Copies looked at ahead of phase B, so their (often far back, so uncached) source bytes are
/// on their way from memory by the time they're copied.
const PREFETCH: usize = 16;

/// Phase B's look-ahead: follows the ops with the same distance bookkeeping, without copying,
/// and prefetches each copy's source.
struct Ahead {
    i: usize,
    pos: usize,
    r: Recent,
}

impl Ahead {
    #[inline(always)]
    fn run(&mut self, ops: &[Op], until: usize, buf: &[u8], base: usize) {
        while self.i < until.min(ops.len()) {
            let op = ops[self.i];
            self.i += 1;
            self.pos += op.lits as usize;
            let len = (op.len & ((1 << KIND_SHIFT) - 1)) as usize;
            let r = &mut self.r;
            let dist = match op.len >> KIND_SHIFT {
                MATCH => {
                    *r = Recent {
                        d0: op.dist as usize,
                        d1: r.d0,
                        d2: r.d1,
                        d3: r.d2,
                        len,
                    };
                    r.d0
                }
                LAST => r.d0,
                n @ REPEAT..LAST => {
                    let d = match n - REPEAT {
                        0 => r.d0,
                        1 => {
                            let d = r.d1;
                            r.d1 = r.d0;
                            d
                        }
                        2 => {
                            let d = r.d2;
                            (r.d2, r.d1) = (r.d1, r.d0);
                            d
                        }
                        _ => {
                            let d = r.d3;
                            (r.d3, r.d2, r.d1) = (r.d2, r.d1, r.d0);
                            d
                        }
                    };
                    (r.d0, r.len) = (d, len);
                    d
                }
                _ => continue,
            };
            let len = if op.len >> KIND_SHIFT == LAST { r.len } else { len };
            // An estimate (files ending mid-chunk, a slide in between): a wrong guess only
            // costs a useless prefetch.
            if let Some(at) = self.pos.checked_sub(dist).and_then(|s| s.checked_sub(base))
                && at < buf.len()
            {
                // SAFETY: a prefetch of an address inside `buf`; it can't fault.
                unsafe { std::arch::x86_64::_mm_prefetch(buf.as_ptr().add(at).cast(), std::arch::x86_64::_MM_HINT_T0) };
            }
            self.pos += len;
        }
    }
}

impl Window {
    fn new(keep: usize, total: usize) -> Self {
        let slide = keep.clamp(SLIDE_ROOM, MAX_ROOM);
        let size = (keep + slide).min(total + MAX_MATCH * 2) + MAX_MATCH * 2;
        Self {
            buf: vec![0; size],
            base: 0,
            pos: 0,
            emitted: 0,
            keep,
            recent: Recent::default(),
            filters: VecDeque::new(),
            filtered: Vec::new(),
            scratch: Vec::new(),
            slide,
        }
    }

    /// Makes room for `need` more bytes: sends what's finished, then drops what no match can
    /// reach any more.
    fn room(&mut self, need: usize, f: &mut FileState, out: &mut Drain) -> Result<(), Failure> {
        if self.pos - self.base + need <= self.buf.len() {
            return Ok(());
        }
        self.emit(f, out)?;
        let keep_from = self.emitted.min(self.pos.saturating_sub(self.keep)).max(self.base);
        let shift = keep_from - self.base;
        self.buf.copy_within(shift..self.pos - self.base, 0);
        self.base = keep_from;
        if self.pos - self.base + need > self.buf.len() {
            self.buf.resize(self.pos - self.base + need + self.slide, 0);
        }
        Ok(())
    }

    /// Phase B for one chunk's tokens.
    fn apply(&mut self, k: &Tokens, f: &mut FileState, files: &[&Entry], out: &mut Drain) -> Result<(), Failure> {
        let mut lp = 0;
        let mut ahead = Ahead {
            i: 0,
            pos: self.pos,
            r: self.recent,
        };
        for (oi, op) in k.ops.iter().enumerate() {
            ahead.run(&k.ops, oi + PREFETCH, &self.buf, self.base);
            // Literals, in pieces that fit the window, up to the end of the file.
            let mut run = op.lits as usize;
            while run > 0 && self.pos < f.end {
                let n = run.min(self.slide / 2).min(f.end - self.pos);
                self.room(n + MAX_MATCH, f, out)?;
                let at = self.pos - self.base;
                self.buf[at..at + n].copy_from_slice(&k.lits[lp..lp + n]);
                (self.pos, lp, run) = (self.pos + n, lp + n, run - n);
            }
            lp += run; // past the end of the file: discarded
            let kind = op.len >> KIND_SHIFT;
            if kind == FILE_END {
                self.switch(f, op.dist as usize, files, out)?;
                continue;
            }
            if self.pos >= f.end {
                continue; // the file is complete; the rest of its data is discarded
            }
            let len = (op.len & ((1 << KIND_SHIFT) - 1)) as usize;
            let r = &mut self.recent;
            let dist = match kind {
                MATCH => {
                    let dist = op.dist as usize;
                    *r = Recent {
                        d0: dist,
                        d1: r.d0,
                        d2: r.d1,
                        d3: r.d2,
                        len,
                    };
                    dist
                }
                TAIL => continue,
                LAST => {
                    if r.len != 0 {
                        let (d, l) = (r.d0, r.len);
                        self.copy(d, l, f, out)?;
                    }
                    continue;
                }
                FILTER => {
                    let start = self.pos + (op.dist as u32) as usize;
                    let flen = (op.dist >> 32) as usize;
                    if flen == 0 {
                        continue; // an empty (or oversized) filter does nothing
                    }
                    if self.filters.back().is_some_and(|b| b.start > start) {
                        return Err(Failure::Corrupt); // filters out of order
                    }
                    self.filters.push_back(Filter {
                        start,
                        len: flen,
                        kind: (op.len >> 8) & 0xff,
                        channels: (op.len & 0xff) as usize,
                    });
                    continue;
                }
                n => {
                    let dist = match n - REPEAT {
                        0 => r.d0,
                        1 => {
                            let d = r.d1;
                            r.d1 = r.d0;
                            d
                        }
                        2 => {
                            let d = r.d2;
                            (r.d2, r.d1) = (r.d1, r.d0);
                            d
                        }
                        _ => {
                            let d = r.d3;
                            (r.d3, r.d2, r.d1) = (r.d2, r.d1, r.d0);
                            d
                        }
                    };
                    r.d0 = dist;
                    r.len = len;
                    dist
                }
            };
            self.copy(dist, len, f, out)?;
        }
        Ok(())
    }

    /// Ends the current file and moves on to file `next` (files in between are empty).
    fn switch(&mut self, f: &mut FileState, next: usize, files: &[&Entry], out: &mut Drain) -> Result<(), Failure> {
        if next <= f.index || next >= files.len() {
            return Err(Failure::Corrupt);
        }
        self.finish_file(f, out)?;
        for index in f.index + 1..next {
            let mut empty = FileState::new(files, index, self.pos);
            out.start(index, &files[index].check)?;
            self.finish_file(&mut empty, out)?;
        }
        *f = FileState::new(files, next, self.pos);
        out.start(next, &files[next].check)
    }
    fn copy(&mut self, dist: usize, len: usize, f: &mut FileState, out: &mut Drain) -> Result<(), Failure> {
        self.room(len + 32, f, out)?;
        let at = self.pos - self.base;
        if dist > self.pos || dist == 0 {
            // Before the start of the stream: zeros (the reference decoder's rule).
            if dist != 0 {
                self.buf[at..at + len].fill(0);
            }
        } else if dist > at {
            return Err(Failure::Corrupt); // further back than the dictionary
        } else {
            // SAFETY: dist <= at, and room() left len + 32 bytes past `at`.
            unsafe { unpack::copy_match(self.buf.as_mut_ptr(), at, dist, len) };
        }
        self.pos += len;
        Ok(())
    }

    /// Sends every finished byte of the current file: up to the output position, except
    /// filtered blocks that aren't complete yet.
    fn emit(&mut self, f: &mut FileState, out: &mut Drain) -> Result<(), Failure> {
        let ready = self.pos.min(f.end);
        while let Some(flt) = self.filters.front() {
            if flt.start >= ready {
                break;
            }
            if flt.start < self.emitted {
                return Err(Failure::Corrupt); // overlapping filters
            }
            let (start, end, kind, channels) = (flt.start, flt.start + flt.len, flt.kind, flt.channels);
            if end > f.end {
                return Err(Failure::Corrupt);
            }
            if self.pos < end {
                // Not complete yet: send what's before it.
                self.send(self.emitted, start, out)?;
                return Ok(());
            }
            self.send(self.emitted, start, out)?;
            self.filters.pop_front();
            self.filtered.clear();
            self.filtered.extend_from_slice(&self.buf[start - self.base..end - self.base]);
            filter_block(&mut self.filtered, (start - f.start) as u32, kind, channels, &mut self.scratch)?;
            out.write(&self.filtered)?;
            self.emitted = end;
        }
        self.send(self.emitted, ready, out)
    }

    fn send(&mut self, from: usize, to: usize, out: &mut Drain) -> Result<(), Failure> {
        if to > from {
            let bytes = &self.buf[from - self.base..to - self.base];
            out.write(bytes)?;
            self.emitted = to;
        }
        Ok(())
    }

    /// Ends the current file: everything sent, CRC checked.
    fn finish_file(&mut self, f: &mut FileState, out: &mut Drain) -> Result<(), Failure> {
        if self.pos < f.end {
            return Err(Failure::Corrupt); // the data ended early
        }
        // A match may have run past the end: those bytes belong to no file.
        self.pos = f.end;
        self.emit(f, out)?;
        if self.emitted != f.end || !self.filters.is_empty() {
            return Err(Failure::Corrupt);
        }
        // The drain thread checks the checksum.
        out.finish(f.index)
    }
}
