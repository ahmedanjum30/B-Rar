//! B-Rar's engine for the macOS app (`macos/`), as a few C functions.
//!
//! Each operation (list, extract, test, create) runs as a *job* on a worker thread. The app
//! polls [`brar_job_status`] (a JSON object) a few times a second, as the Windows window
//! polls its worker. When the engine needs an answer ("replace this file?", "where is the
//! next part?") the job waits in an `asking_*` state until the app calls
//! [`brar_job_answer_conflict`] or [`brar_job_answer_part`]: nothing calls back into Swift.
//!
//! Strings go both ways as UTF-8 C strings; the ones returned are freed with
//! [`brar_string_free`]. Jobs are freed with [`brar_job_free`] (which cancels a running one).

use std::{
    ffi::{CStr, CString, c_char},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::SystemTime,
};

use brar_core::{Conflict, ConflictPolicy, CreateOptions, Decision, EntryInfo, Error, Options, PartFinder, Progress, Resolution, Selection, Summary};
use serde::Serialize;

/// What the job is waiting for from the app, if anything.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Question {
    /// An extracted file exists already.
    Conflict {
        path: String,
        new_size: Option<u64>,
        new_modified: Option<f64>,
    },
    /// A part of a multi-part archive isn't next to the others.
    Part { path: String },
}

/// The app's answer to a [`Question`].
enum Answer {
    Conflict(Decision),
    Part(Option<PathBuf>),
}

#[derive(Default)]
struct State {
    question: Option<Question>,
    answer: Option<Answer>,
    /// Set once, when the worker finishes.
    outcome: Option<Outcome>,
}

enum Outcome {
    Done { summary: Summary, entries: Option<Vec<EntryInfo>> },
    Failed(Error),
}

struct Shared {
    progress: Arc<Progress>,
    state: Mutex<State>,
    answered: Condvar,
}

impl Shared {
    /// Asks the app and blocks this worker until it answers (or the job is cancelled).
    fn ask(&self, question: Question) -> Option<Answer> {
        let mut state = self.state.lock().unwrap();
        if self.progress.is_cancelled() {
            return None;
        }
        state.answer = None;
        state.question = Some(question);
        while state.answer.is_none() && !self.progress.is_cancelled() {
            state = self.answered.wait_timeout(state, std::time::Duration::from_millis(100)).unwrap().0;
        }
        state.question = None;
        state.answer.take()
    }
}

/// A running or finished operation. Opaque to C.
pub struct BrarJob {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    /// The entries a finished `list` job read, handed over once by [`brar_job_take_entries`].
    entries: Option<Vec<EntryInfo>>,
}

impl BrarJob {
    fn start(work: impl FnOnce(&Arc<Shared>) -> Result<(Summary, Option<Vec<EntryInfo>>), Error> + Send + 'static) -> Box<BrarJob> {
        let shared = Arc::new(Shared {
            progress: Arc::new(Progress::default()),
            state: Mutex::new(State::default()),
            answered: Condvar::new(),
        });
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("B-Rar-job".into())
                .spawn(move || {
                    let outcome = match work(&shared) {
                        Ok((summary, entries)) => Outcome::Done { summary, entries },
                        Err(e) => Outcome::Failed(e),
                    };
                    shared.state.lock().unwrap().outcome = Some(outcome);
                })
                .expect("spawning a thread")
        };
        Box::new(BrarJob {
            shared,
            worker: Some(worker),
            entries: None,
        })
    }
}

/// Engine options wired to the job: progress, conflicts asked through the job (or one fixed
/// answer), and parts asked for through the job.
fn options(shared: &Arc<Shared>, password: Option<String>, conflicts: i32) -> Options {
    let on_conflict = match conflicts {
        1 => ConflictPolicy::Always(Resolution::Overwrite),
        2 => ConflictPolicy::Always(Resolution::Skip),
        3 => ConflictPolicy::Always(Resolution::Rename),
        _ => {
            let shared = shared.clone();
            ConflictPolicy::Ask(Arc::new(move |c: &Conflict<'_>| {
                let question = Question::Conflict {
                    path: c.path.display().to_string(),
                    new_size: c.new_size,
                    new_modified: c.new_modified.map(seconds),
                };
                match shared.ask(question) {
                    Some(Answer::Conflict(decision)) => decision,
                    _ => Decision {
                        resolution: Resolution::Cancel,
                        apply_to_all: true,
                    },
                }
            }))
        }
    };
    let find_part = {
        let shared = shared.clone();
        PartFinder::new(move |missing: &Path| {
            match shared.ask(Question::Part {
                path: missing.display().to_string(),
            }) {
                Some(Answer::Part(found)) => found,
                _ => None,
            }
        })
    };
    Options {
        threads: 0,
        password,
        progress: Some(shared.progress.clone()),
        on_conflict,
        selection: None,
        find_part,
    }
}

fn seconds(time: SystemTime) -> f64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// A C string argument; `None` for null.
///
/// # Safety
/// `s` is null or a valid NUL-terminated string.
unsafe fn arg(s: *const c_char) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: the caller passes a valid C string.
    Some(unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
}

fn out(s: String) -> *mut c_char {
    CString::new(s.replace('\0', "")).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Lists an archive's entries. Take them with [`brar_job_take_entries`] once it's done.
///
/// # Safety
/// `archive` is a valid C string; `password` is null or one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_list(archive: *const c_char, password: *const c_char) -> *mut BrarJob {
    // SAFETY: as documented.
    let (Some(archive), password) = (unsafe { arg(archive) }, unsafe { arg(password) }) else {
        return std::ptr::null_mut();
    };
    Box::into_raw(BrarJob::start(move |shared| {
        let entries = brar_core::list(Path::new(&archive), &options(shared, password, 1))?;
        Ok((Summary::default(), Some(entries)))
    }))
}

/// Extracts an archive into `dest`, or tests it when `dest` is null.
///
/// `selection_json` is null (everything) or `{"items": [...], "base": "..."}` (archive paths,
/// see `brar_core::Selection`). `conflicts`: 0 asks, 1 replaces, 2 skips, 3 keeps both.
///
/// # Safety
/// `archive` is a valid C string; the others are null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_extract(
    archive: *const c_char,
    dest: *const c_char,
    password: *const c_char,
    selection_json: *const c_char,
    conflicts: i32,
) -> *mut BrarJob {
    // SAFETY: as documented.
    let (Some(archive), dest, password, selection) = (unsafe { arg(archive) }, unsafe { arg(dest) }, unsafe { arg(password) }, unsafe {
        arg(selection_json)
    }) else {
        return std::ptr::null_mut();
    };
    let selection = selection.and_then(|json| {
        let value: serde_json::Value = serde_json::from_str(&json).ok()?;
        Some(Selection {
            items: value["items"].as_array()?.iter().filter_map(|i| i.as_str().map(str::to_owned)).collect(),
            base: value["base"].as_str().unwrap_or_default().to_owned(),
        })
    });
    Box::into_raw(BrarJob::start(move |shared| {
        let mut opts = options(shared, password, conflicts);
        opts.selection = selection;
        let summary = match dest {
            Some(dest) => brar_core::extract(Path::new(&archive), Path::new(&dest), &opts)?,
            None => brar_core::test(Path::new(&archive), &opts)?,
        };
        Ok((summary, None))
    }))
}

/// Makes a ZIP archive of `inputs_json` (a JSON array of paths). `level` is 0 (store) to 9;
/// `password` (null for none) encrypts with AES-256; `overwrite` replaces an existing archive.
///
/// # Safety
/// `archive` and `inputs_json` are valid C strings; `password` is null or one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_create(
    archive: *const c_char,
    inputs_json: *const c_char,
    level: u32,
    password: *const c_char,
    overwrite: bool,
) -> *mut BrarJob {
    // SAFETY: as documented.
    let (Some(archive), Some(inputs), password) = (unsafe { arg(archive) }, unsafe { arg(inputs_json) }, unsafe { arg(password) }) else {
        return std::ptr::null_mut();
    };
    let Ok(inputs) = serde_json::from_str::<Vec<PathBuf>>(&inputs) else {
        return std::ptr::null_mut();
    };
    Box::into_raw(BrarJob::start(move |shared| {
        let opts = CreateOptions {
            threads: 0,
            level: level.min(9),
            password,
            progress: Some(shared.progress.clone()),
            overwrite,
        };
        Ok((brar_core::create(Path::new(&archive), &inputs, &opts)?, None))
    }))
}

#[derive(Serialize)]
struct Status<'a> {
    /// "running", "asking", "done" or "failed".
    state: &'static str,
    total_bytes: u64,
    done_bytes: u64,
    files: u64,
    current: String,
    paused: bool,
    question: Option<Question>,
    summary: Option<SummaryJson>,
    /// For "failed": what to show, and a kind the app acts on ("password_required",
    /// "wrong_password", "cancelled", "access_denied", "unknown_format", "missing_part", "other").
    error: Option<String>,
    error_kind: Option<&'a str>,
}

#[derive(Serialize)]
struct SummaryJson {
    files: u64,
    dirs: u64,
    bytes: u64,
    skipped: u64,
    kept_existing: u64,
}

fn error_kind(e: &Error) -> &'static str {
    match e {
        Error::PasswordRequired => "password_required",
        Error::WrongPassword => "wrong_password",
        Error::Cancelled => "cancelled",
        Error::UnknownFormat => "unknown_format",
        Error::MissingVolume(_) => "missing_part",
        e if e.is_access_denied() => "access_denied",
        _ => "other",
    }
}

/// The job's state as JSON (see `Status`). Free with [`brar_string_free`].
///
/// # Safety
/// `job` is a job from this library, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_job_status(job: *mut BrarJob) -> *mut c_char {
    // SAFETY: as documented.
    let Some(job) = (unsafe { job.as_mut() }) else {
        return std::ptr::null_mut();
    };
    let progress = &job.shared.progress;
    let mut state = job.shared.state.lock().unwrap();
    let question = state.question.clone().filter(|_| !progress.is_cancelled());
    let (name, summary, error, kind) = match state.outcome.as_mut() {
        Some(Outcome::Done { summary, entries }) => {
            if let Some(entries) = entries.take() {
                job.entries = Some(entries);
            }
            let s = SummaryJson {
                files: summary.files,
                dirs: summary.dirs,
                bytes: summary.bytes,
                skipped: summary.skipped,
                kept_existing: summary.kept_existing,
            };
            ("done", Some(s), None, None)
        }
        Some(Outcome::Failed(e)) => ("failed", None, Some(e.to_string()), Some(error_kind(e))),
        None if question.is_some() && !progress.is_cancelled() => ("asking", None, None, None),
        None => ("running", None, None, None),
    };
    let status = Status {
        state: name,
        total_bytes: progress.total_bytes(),
        done_bytes: progress.done_bytes(),
        files: progress.files(),
        current: progress.current_file(),
        paused: progress.is_paused(),
        question,
        summary,
        error,
        error_kind: kind,
    };
    out(serde_json::to_string(&status).unwrap_or_default())
}

/// The entries of a finished `list` job, as a JSON array of `{"name", "is_dir", "size",
/// "packed", "modified"}` (modified in seconds since 1970). Once only; null before.
///
/// # Safety
/// `job` is a job from this library, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_job_take_entries(job: *mut BrarJob) -> *mut c_char {
    #[derive(Serialize)]
    struct Entry<'a> {
        name: &'a str,
        is_dir: bool,
        size: u64,
        packed: Option<u64>,
        modified: Option<f64>,
    }
    // SAFETY: as documented.
    let Some(job) = (unsafe { job.as_mut() }) else {
        return std::ptr::null_mut();
    };
    if job.entries.is_none()
        && let Some(Outcome::Done { entries, .. }) = job.shared.state.lock().unwrap().outcome.as_mut()
    {
        job.entries = entries.take();
    }
    let Some(entries) = job.entries.take() else {
        return std::ptr::null_mut();
    };
    let entries: Vec<Entry<'_>> = entries
        .iter()
        .map(|e| Entry {
            name: &e.name,
            is_dir: e.is_dir,
            size: e.size,
            packed: e.packed,
            modified: e.modified.map(seconds),
        })
        .collect();
    out(serde_json::to_string(&entries).unwrap_or_default())
}

/// Answers a conflict question. `resolution`: 0 replace, 1 skip, 2 keep both, 3 cancel.
///
/// # Safety
/// `job` is a job from this library, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_job_answer_conflict(job: *mut BrarJob, resolution: i32, apply_to_all: bool) {
    let resolution = match resolution {
        0 => Resolution::Overwrite,
        1 => Resolution::Skip,
        2 => Resolution::Rename,
        _ => Resolution::Cancel,
    };
    // SAFETY: as documented.
    unsafe { answer(job, Answer::Conflict(Decision { resolution, apply_to_all })) };
}

/// Answers a missing-part question: the part's path, or null to give up.
///
/// # Safety
/// `job` is a job from this library, not yet freed; `path` is null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_job_answer_part(job: *mut BrarJob, path: *const c_char) {
    // SAFETY: as documented.
    let path = unsafe { arg(path) }.map(PathBuf::from);
    // SAFETY: as documented.
    unsafe { answer(job, Answer::Part(path)) };
}

/// # Safety
/// `job` is a job from this library, not yet freed.
unsafe fn answer(job: *mut BrarJob, answer: Answer) {
    // SAFETY: as documented.
    let Some(job) = (unsafe { job.as_ref() }) else { return };
    let mut state = job.shared.state.lock().unwrap();
    // Answered: no longer asking, even before the worker wakes up.
    state.question = None;
    state.answer = Some(answer);
    job.shared.answered.notify_all();
}

/// # Safety
/// `job` is a job from this library, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_job_set_paused(job: *mut BrarJob, paused: bool) {
    // SAFETY: as documented.
    if let Some(job) = unsafe { job.as_ref() } {
        job.shared.progress.set_paused(paused);
    }
}

/// Stops the job; it ends as "failed" with the kind "cancelled".
///
/// # Safety
/// `job` is a job from this library, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_job_cancel(job: *mut BrarJob) {
    // SAFETY: as documented.
    if let Some(job) = unsafe { job.as_ref() } {
        job.shared.progress.cancel();
        job.shared.state.lock().unwrap().question = None;
        job.shared.answered.notify_all();
    }
}

/// Cancels the job if it's still running, waits for its worker and frees it.
///
/// # Safety
/// `job` is null or a job from this library, not yet freed; it's invalid afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_job_free(job: *mut BrarJob) {
    if job.is_null() {
        return;
    }
    // SAFETY: as documented; ownership comes back from `Box::into_raw`.
    let mut job = unsafe { Box::from_raw(job) };
    job.shared.progress.cancel();
    job.shared.answered.notify_all();
    if let Some(worker) = job.worker.take() {
        let _ = worker.join();
    }
}

/// The folder "Extract to" uses: the archive's name without its extensions, next to it.
///
/// # Safety
/// `archive` is a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_suggested_dir(archive: *const c_char) -> *mut c_char {
    // SAFETY: as documented.
    let Some(archive) = (unsafe { arg(archive) }) else {
        return std::ptr::null_mut();
    };
    out(brar_core::suggested_dir(Path::new(&archive)).display().to_string())
}

/// Whether B-Rar can open the file (by its first bytes).
///
/// # Safety
/// `path` is a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_is_archive(path: *const c_char) -> bool {
    // SAFETY: as documented.
    unsafe { arg(path) }.is_some_and(|p| brar_core::detect(Path::new(&p)).is_ok())
}

/// Frees a string returned by this library.
///
/// # Safety
/// `s` is null or a string from this library, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brar_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: as documented; it came from `CString::into_raw`.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// The engine's version, for the About window. Static; not freed.
#[unsafe(no_mangle)]
pub extern "C" fn brar_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

#[cfg(test)]
mod tests;
