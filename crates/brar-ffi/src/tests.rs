//! Drives the C functions as the Mac app does: start a job, poll, answer, take results.

use super::*;
use std::time::{Duration, Instant};

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn json(s: *mut c_char) -> serde_json::Value {
    assert!(!s.is_null());
    // SAFETY: a string from this library.
    let value = serde_json::from_str(&unsafe { CStr::from_ptr(s) }.to_string_lossy()).unwrap();
    unsafe { brar_string_free(s) };
    value
}

/// Polls until the job is done, failed or asking; returns the last status.
fn wait(job: *mut BrarJob) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = json(unsafe { brar_job_status(job) });
        if status["state"] != "running" || Instant::now() > deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A folder with two files, zipped to `<dir>/t.zip` (with `password`, if any).
fn archive(dir: &Path, password: Option<&str>) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), "hello").unwrap();
    std::fs::write(src.join("sub/b.bin"), vec![7u8; 100_000]).unwrap();
    let zip = dir.join("t.zip");
    let inputs = c(&serde_json::to_string(&[&src]).unwrap());
    let pw = password.map(c);
    let job = unsafe {
        brar_create(
            c(zip.to_str().unwrap()).as_ptr(),
            inputs.as_ptr(),
            6,
            pw.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            false,
        )
    };
    let status = wait(job);
    assert_eq!(status["state"], "done", "{status}");
    assert_eq!(status["summary"]["files"], 2);
    unsafe { brar_job_free(job) };
    zip
}

fn temp() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("brar-ffi-{}-{:?}", std::process::id(), std::thread::current().id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn lists_extracts_and_tests() {
    let dir = temp();
    let zip = archive(&dir, None);
    let path = c(zip.to_str().unwrap());

    let job = unsafe { brar_list(path.as_ptr(), std::ptr::null()) };
    assert_eq!(wait(job)["state"], "done");
    let entries = json(unsafe { brar_job_take_entries(job) });
    let names: Vec<&str> = entries
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["is_dir"] == false)
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"src/a.txt") && names.contains(&"src/sub/b.bin"), "{names:?}");
    assert!(unsafe { brar_job_take_entries(job) }.is_null(), "entries are handed over once");
    unsafe { brar_job_free(job) };

    let out = dir.join("out");
    let job = unsafe { brar_extract(path.as_ptr(), c(out.to_str().unwrap()).as_ptr(), std::ptr::null(), std::ptr::null(), 0) };
    let status = wait(job);
    assert_eq!(status["state"], "done", "{status}");
    assert_eq!(status["done_bytes"], 100_005);
    unsafe { brar_job_free(job) };
    assert_eq!(std::fs::read_to_string(out.join("src/a.txt")).unwrap(), "hello");

    // Only one file, relative to its folder.
    let only = dir.join("only");
    let selection = c(r#"{"items": ["src/sub/b.bin"], "base": "src/sub"}"#);
    let job = unsafe { brar_extract(path.as_ptr(), c(only.to_str().unwrap()).as_ptr(), std::ptr::null(), selection.as_ptr(), 0) };
    assert_eq!(wait(job)["state"], "done");
    unsafe { brar_job_free(job) };
    assert!(only.join("b.bin").exists() && !only.join("src").exists());

    let job = unsafe { brar_extract(path.as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0) };
    assert_eq!(wait(job)["summary"]["files"], 2, "test reads every file");
    unsafe { brar_job_free(job) };

    assert!(unsafe { brar_is_archive(path.as_ptr()) });
    assert!(!unsafe { brar_is_archive(c(dir.join("src/a.txt").to_str().unwrap()).as_ptr()) });
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn asks_before_replacing_and_takes_the_answer() {
    let dir = temp();
    let zip = archive(&dir, None);
    let path = c(zip.to_str().unwrap());
    let out = dir.join("out");
    std::fs::create_dir_all(out.join("src")).unwrap();
    std::fs::write(out.join("src/a.txt"), "mine").unwrap();

    let job = unsafe { brar_extract(path.as_ptr(), c(out.to_str().unwrap()).as_ptr(), std::ptr::null(), std::ptr::null(), 0) };
    let status = wait(job);
    assert_eq!(status["state"], "asking", "{status}");
    assert_eq!(status["question"]["kind"], "conflict");
    assert!(status["question"]["path"].as_str().unwrap().ends_with("a.txt"));
    unsafe { brar_job_answer_conflict(job, 1, false) }; // skip
    let status = wait(job);
    assert_eq!(status["state"], "done", "{status}");
    assert_eq!(status["summary"]["kept_existing"], 1);
    unsafe { brar_job_free(job) };
    assert_eq!(std::fs::read_to_string(out.join("src/a.txt")).unwrap(), "mine");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn passwords_and_cancel() {
    let dir = temp();
    let zip = archive(&dir, Some("secret"));
    let path = c(zip.to_str().unwrap());
    let out = c(dir.join("out").to_str().unwrap());

    let extract = |password: Option<&str>| {
        let pw = password.map(c);
        let job = unsafe {
            brar_extract(
                path.as_ptr(),
                out.as_ptr(),
                pw.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
                std::ptr::null(),
                1,
            )
        };
        let status = wait(job);
        unsafe { brar_job_free(job) };
        status
    };
    assert_eq!(extract(None)["error_kind"], "password_required");
    assert_eq!(extract(Some("wrong"))["error_kind"], "wrong_password");
    assert_eq!(extract(Some("secret"))["state"], "done");

    // Cancelled while it asks: the job ends as cancelled instead of waiting forever.
    std::fs::write(dir.join("out/src/a.txt"), "mine").unwrap();
    let pw = c("secret");
    let job = unsafe { brar_extract(path.as_ptr(), out.as_ptr(), pw.as_ptr(), std::ptr::null(), 0) };
    assert_eq!(wait(job)["state"], "asking");
    unsafe { brar_job_cancel(job) };
    let status = wait(job);
    assert_eq!(status["error_kind"], "cancelled", "{status}");
    unsafe { brar_job_free(job) };
    let _ = std::fs::remove_dir_all(dir);
}
