// The Rust engine (crates/brar-ffi) seen from Swift: jobs that run on the engine's own
// threads, polled for their status, as the Windows window polls its worker.

import Foundation

struct JobStatus: Decodable {
    let state: String
    let totalBytes: UInt64
    let doneBytes: UInt64
    let files: UInt64
    let current: String
    let paused: Bool
    let question: Question?
    let summary: Summary?
    let error: String?
    let errorKind: String?

    var isFinished: Bool { state == "done" || state == "failed" }
}

struct Question: Decodable, Equatable {
    /// "conflict" or "part".
    let kind: String
    let path: String
    let newSize: UInt64?
    let newModified: Double?
}

struct Summary: Decodable {
    let files: UInt64
    let dirs: UInt64
    let bytes: UInt64
    let skipped: UInt64
    let keptExisting: UInt64
}

struct Entry: Decodable {
    let name: String
    let isDir: Bool
    let size: UInt64
    let packed: UInt64?
    let modified: Double?
}

private let decoder: JSONDecoder = {
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    return decoder
}()

/// Takes a string the engine returned, and frees it.
private func take(_ s: UnsafeMutablePointer<CChar>?) -> String? {
    guard let s else { return nil }
    defer { brar_string_free(s) }
    return String(cString: s)
}

/// A C string for the length of `body`, or null for nil.
private func withOptionalCString<T>(_ s: String?, _ body: (UnsafePointer<CChar>?) -> T) -> T {
    guard let s else { return body(nil) }
    return s.withCString { body($0) }
}

/// One operation running in the engine. Freed (and cancelled, if still running) on deinit.
final class Job {
    private let handle: OpaquePointer

    fileprivate init?(_ handle: OpaquePointer?) {
        guard let handle else { return nil }
        self.handle = handle
    }

    deinit { brar_job_free(handle) }

    func status() -> JobStatus? {
        take(brar_job_status(handle)).flatMap { try? decoder.decode(JobStatus.self, from: Data($0.utf8)) }
    }

    /// The entries of a finished listing, once.
    func takeEntries() -> [Entry]? {
        take(brar_job_take_entries(handle)).flatMap { try? decoder.decode([Entry].self, from: Data($0.utf8)) }
    }

    enum Resolution: Int32 { case replace = 0, skip, keepBoth, cancel }

    func answerConflict(_ resolution: Resolution, applyToAll: Bool) {
        brar_job_answer_conflict(handle, resolution.rawValue, applyToAll)
    }

    func answerPart(_ url: URL?) {
        withOptionalCString(url?.path) { brar_job_answer_part(handle, $0) }
    }

    func setPaused(_ paused: Bool) { brar_job_set_paused(handle, paused) }
    func cancel() { brar_job_cancel(handle) }
}

enum Engine {
    static var version: String { String(cString: brar_version()) }

    static func list(_ archive: URL, password: String?) -> Job? {
        archive.path.withCString { a in withOptionalCString(password) { Job(brar_list(a, $0)) } }
    }

    /// Extracts into `dest`, or tests the archive when `dest` is nil. `items` and `base`
    /// pick part of it (archive paths); `conflicts`: 0 asks, 1 replaces, 2 skips, 3 keeps both.
    static func extract(_ archive: URL, to dest: URL?, password: String?, items: [String]? = nil, base: String = "", conflicts: Int32 = 0) -> Job? {
        var selection: String?
        if let items, let data = try? JSONSerialization.data(withJSONObject: ["items": items, "base": base]) {
            selection = String(data: data, encoding: .utf8)
        }
        return archive.path.withCString { a in
            withOptionalCString(dest?.path) { d in
                withOptionalCString(password) { p in
                    withOptionalCString(selection) { s in Job(brar_extract(a, d, p, s, conflicts)) }
                }
            }
        }
    }

    static func create(_ archive: URL, from inputs: [URL], level: UInt32, password: String?, overwrite: Bool) -> Job? {
        guard let data = try? JSONSerialization.data(withJSONObject: inputs.map(\.path)),
              let json = String(data: data, encoding: .utf8) else { return nil }
        return archive.path.withCString { a in
            json.withCString { i in withOptionalCString(password) { Job(brar_create(a, i, level, $0, overwrite)) } }
        }
    }

    /// The folder "Extract to" uses: the archive's name without its extensions, next to it.
    static func suggestedDir(_ archive: URL) -> URL {
        archive.path.withCString { take(brar_suggested_dir($0)) }.map { URL(fileURLWithPath: $0) }
            ?? archive.deletingPathExtension()
    }

    static func isArchive(_ url: URL) -> Bool {
        url.path.withCString { brar_is_archive($0) }
    }
}
