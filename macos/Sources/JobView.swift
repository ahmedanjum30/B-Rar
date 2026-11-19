// The window of one extraction, test or new archive: progress, then "file already exists",
// password and missing-part questions as they come, then the result. The Mac counterpart of
// crates/brar-gui/src/app.rs; it polls the job ten times a second, as that window does.

import AppKit
import SwiftUI

enum JobAction {
    /// Extracts into `dest`; `items` and `base` pick part of the archive (from the browser).
    /// `password`: one the browser already has, so it isn't asked for again.
    case extract(archive: URL, dest: URL, items: [String]? = nil, base: String = "", password: String? = nil)
    case test(archive: URL)
    case create(inputs: [URL], archive: URL, level: UInt32, password: String?, overwrite: Bool)

    var windowTitle: String {
        switch self {
        case .extract: "Extracting - B-Rar"
        case .test: "Testing - B-Rar"
        case .create: "Compressing - B-Rar"
        }
    }

    var archive: URL {
        switch self {
        case let .extract(archive, _, _, _, _), let .test(archive), let .create(_, archive, _, _, _): archive
        }
    }
}

@MainActor
final class JobModel: ObservableObject {
    enum Phase {
        case running
        /// The archive is encrypted: `wrong` after a password that didn't work.
        case password(wrong: Bool)
        case done(Summary)
        case failed(String, canRetryElsewhere: Bool)
    }

    @Published var phase: Phase = .running
    @Published var status: JobStatus?
    @Published var conflict: Question?
    @Published private(set) var elapsed: TimeInterval = 0

    private(set) var action: JobAction
    private var job: Job?
    private var timer: Timer?
    private var started = Date()
    private var password: String?
    private var askedPart: String?

    init(action: JobAction) {
        self.action = action
        switch action {
        case let .create(_, _, _, password, _), let .extract(_, _, _, _, password): self.password = password
        case .test: break
        }
    }

    func start() {
        switch action {
        case let .extract(archive, dest, items, base, _):
            job = Engine.extract(archive, to: dest, password: password, items: items, base: base)
        case let .test(archive):
            job = Engine.extract(archive, to: nil, password: password)
        case let .create(inputs, archive, level, password, overwrite):
            job = Engine.create(archive, from: inputs, level: level, password: password, overwrite: overwrite)
        }
        guard job != nil else {
            phase = .failed("B-Rar couldn't start.", canRetryElsewhere: false)
            return
        }
        phase = .running
        started = Date()
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.poll() }
        }
    }

    private func poll() {
        guard let job, let status = job.status() else { return }
        self.status = status
        if !status.paused { elapsed = Date().timeIntervalSince(started) }
        if let question = status.question {
            switch question.kind {
            case "conflict": if conflict != question { conflict = question }
            case "part": askForPart(question.path)
            default: break
            }
        }
        guard status.isFinished else { return }
        timer?.invalidate()
        timer = nil
        self.job = nil
        if let summary = status.summary {
            phase = .done(summary)
            return
        }
        switch status.errorKind {
        case "password_required": phase = .password(wrong: false)
        case "wrong_password": phase = .password(wrong: true)
        case "cancelled": NotificationCenter.default.post(name: .jobCancelled, object: self)
        case "access_denied":
            phase = .failed("B-Rar can't write to this folder. Choose another folder to extract to.", canRetryElsewhere: true)
        default: phase = .failed(status.error ?? "Something went wrong.", canRetryElsewhere: false)
        }
    }

    /// A part of a multi-part archive is in another folder: ask for it, as on Windows.
    private func askForPart(_ path: String) {
        guard askedPart != path, let job else { return }
        askedPart = path
        let panel = NSOpenPanel()
        panel.message = "B-Rar needs \((path as NSString).lastPathComponent) to go on. Where is it?"
        panel.prompt = "Use This Part"
        panel.directoryURL = URL(fileURLWithPath: path).deletingLastPathComponent()
        job.answerPart(panel.runModal() == .OK ? panel.url : nil)
    }

    func answer(_ resolution: Job.Resolution, applyToAll: Bool) {
        job?.answerConflict(resolution, applyToAll: applyToAll)
        conflict = nil
    }

    func retry(password: String) {
        self.password = password
        start()
    }

    func retryElsewhere() {
        guard case let .extract(archive, _, items, base, _) = action, let dest = chooseFolder(for: archive) else { return }
        action = .extract(archive: archive, dest: dest, items: items, base: base, password: password)
        start()
    }

    func togglePause() {
        guard let status else { return }
        job?.setPaused(!status.paused)
    }

    func cancel() {
        job?.cancel()
        // A job waiting on an answer stops at once.
        conflict = nil
    }

    var isPaused: Bool { status?.paused ?? false }
}

extension Notification.Name {
    static let jobCancelled = Notification.Name("B-Rar.jobCancelled")
}

struct JobView: View {
    @StateObject private var model: JobModel
    let window: WindowHandle
    @State private var password = ""
    @State private var showPassword = false
    @State private var applyToAll = false

    init(action: JobAction, window: WindowHandle) {
        _model = StateObject(wrappedValue: JobModel(action: action))
        self.window = window
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            switch model.phase {
            case .running: progress
            case let .password(wrong): passwordPage(wrong: wrong)
            case let .done(summary): result(summary)
            case let .failed(message, elsewhere): failure(message, elsewhere: elsewhere)
            }
        }
        .padding(24)
        .frame(width: 520, alignment: .topLeading)
        .fixedSize(horizontal: false, vertical: true)
        .onAppear { model.start() }
        .onReceive(NotificationCenter.default.publisher(for: .jobCancelled)) { note in
            if note.object as AnyObject === model { window.close() }
        }
        .sheet(item: Binding(get: { model.conflict.map(ConflictItem.init) }, set: { if $0 == nil { model.conflict = nil } })) { item in
            conflictSheet(item.question)
        }
    }

    // MARK: Progress

    private var heading: String {
        switch model.action {
        case let .extract(_, dest, _, _, _): "Extracting to \(dest.lastPathComponent)"
        case let .test(archive): "Testing \(archive.lastPathComponent)"
        case let .create(_, archive, _, _, _): "Compressing to \(archive.lastPathComponent)"
        }
    }

    private var progress: some View {
        let status = model.status
        let total = Double(status?.totalBytes ?? 0)
        let done = Double(status?.doneBytes ?? 0)
        return VStack(alignment: .leading, spacing: 10) {
            Text(heading).font(.title3.weight(.semibold)).lineLimit(1).truncationMode(.middle)
            if total > 0 {
                ProgressView(value: min(done, total), total: total)
            } else {
                ProgressView().progressViewStyle(.linear)
            }
            HStack {
                Text(total > 0 ? "\(Format.bytes(done)) of \(Format.bytes(total))" : Format.bytes(done))
                Spacer()
                Text(Format.files(status?.files ?? 0))
            }
            .font(.callout).foregroundStyle(.secondary).monospacedDigit()
            Text(status?.current ?? " ").font(.callout).lineLimit(1).truncationMode(.middle).foregroundStyle(.secondary)
            HStack {
                Text(timing(done: done, total: total)).font(.callout).foregroundStyle(.secondary).monospacedDigit()
                Spacer()
                Button(model.isPaused ? "Resume" : "Pause") { model.togglePause() }
                Button("Cancel", role: .cancel) { model.cancel() }.keyboardShortcut(.cancelAction)
            }
        }
    }

    private func timing(done: Double, total: Double) -> String {
        let elapsed = model.elapsed
        var parts = ["Elapsed \(Format.duration(elapsed))"]
        if elapsed > 1, done > 0 {
            let speed = done / elapsed
            parts.append("\(Format.bytes(speed))/s")
            if total > done { parts.append("\(Format.duration((total - done) / speed)) left") }
        }
        return parts.joined(separator: "  ·  ")
    }

    // MARK: Questions

    private struct ConflictItem: Identifiable {
        let question: Question
        var id: String { question.path }
    }

    private func conflictSheet(_ question: Question) -> some View {
        let url = URL(fileURLWithPath: question.path)
        let existing = try? FileManager.default.attributesOfItem(atPath: question.path)
        return VStack(alignment: .leading, spacing: 12) {
            Text("The folder already has \(url.lastPathComponent)").font(.headline)
            Text(url.deletingLastPathComponent().path).font(.callout).foregroundStyle(.secondary).lineLimit(2).truncationMode(.middle)
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                GridRow {
                    Text("In the folder:").foregroundStyle(.secondary)
                    Text(describe(size: (existing?[.size] as? NSNumber)?.uint64Value, date: existing?[.modificationDate] as? Date))
                }
                GridRow {
                    Text("In the archive:").foregroundStyle(.secondary)
                    Text(describe(size: question.newSize, date: question.newModified.map { Date(timeIntervalSince1970: $0) }))
                }
            }
            .font(.callout)
            Toggle("Apply to all", isOn: $applyToAll)
            HStack {
                Button("Cancel", role: .cancel) { model.answer(.cancel, applyToAll: true) }.keyboardShortcut(.cancelAction)
                Spacer()
                Button("Keep Both") { model.answer(.keepBoth, applyToAll: applyToAll) }
                Button("Skip") { model.answer(.skip, applyToAll: applyToAll) }
                Button("Replace") { model.answer(.replace, applyToAll: applyToAll) }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private func describe(size: UInt64?, date: Date?) -> String {
        [size.map { Format.bytes(Double($0)) }, date.map { $0.formatted(date: .abbreviated, time: .shortened) }]
            .compactMap { $0 }.joined(separator: ", ")
    }

    private func passwordPage(wrong: Bool) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("\(model.action.archive.lastPathComponent) is encrypted", systemImage: "lock.fill").font(.title3.weight(.semibold))
            Group {
                if showPassword {
                    TextField("Enter password", text: $password)
                } else {
                    SecureField("Enter password", text: $password)
                }
            }
            .textFieldStyle(.roundedBorder)
            .onSubmit(submitPassword)
            Toggle("Show password", isOn: $showPassword)
            if wrong {
                Text("That password is incorrect. Try again.").foregroundStyle(.red).font(.callout)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { window.close() }.keyboardShortcut(.cancelAction)
                Button("OK", action: submitPassword).keyboardShortcut(.defaultAction).disabled(password.isEmpty)
            }
        }
    }

    private func submitPassword() {
        guard !password.isEmpty else { return }
        model.retry(password: password)
    }

    // MARK: Result

    private func result(_ summary: Summary) -> some View {
        let (title, folder): (String, URL?) = {
            switch model.action {
            case let .extract(_, dest, _, _, _): ("Done: \(Format.files(summary.files)) extracted, no errors", dest)
            case .test: ("No errors found in \(Format.files(summary.files))", nil)
            case let .create(_, archive, _, _, _): ("Archive created: \(archive.lastPathComponent)", archive)
            }
        }()
        var notes: [String] = []
        if summary.keptExisting > 0 { notes.append("\(Format.files(summary.keptExisting)) already there, left as they were") }
        if summary.skipped > 0 { notes.append("\(summary.skipped) links or special files not extracted") }
        return VStack(alignment: .leading, spacing: 12) {
            Label(title, systemImage: "checkmark.circle.fill").font(.title3.weight(.semibold)).symbolRenderingMode(.multicolor)
            if let folder { Text("Saved to \(folder.path)").font(.callout).foregroundStyle(.secondary).lineLimit(2).truncationMode(.middle) }
            ForEach(notes, id: \.self) { Text($0).font(.callout).foregroundStyle(.secondary) }
            HStack {
                Spacer()
                if let folder {
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([folder])
                        window.close()
                    }
                }
                Button("Close") { window.close() }.keyboardShortcut(.defaultAction)
            }
        }
        .onAppear { window.window?.title = "Done - B-Rar" }
    }

    private func failure(_ message: String, elsewhere: Bool) -> some View {
        let verb: String = {
            switch model.action {
            case .extract: "extract"
            case .test: "test"
            case .create: "create"
            }
        }()
        return VStack(alignment: .leading, spacing: 12) {
            Label("Couldn't \(verb) the archive", systemImage: "exclamationmark.triangle.fill")
                .font(.title3.weight(.semibold)).symbolRenderingMode(.multicolor)
            Text(message).textSelection(.enabled)
            HStack {
                Spacer()
                if elsewhere { Button("Choose Folder…") { model.retryElsewhere() } }
                Button("Close") { window.close() }.keyboardShortcut(.defaultAction)
            }
        }
        .onAppear { window.window?.title = "Error - B-Rar" }
    }
}
