//! Index worker processes, job objects, the extractor pool.

use super::*;

pub(super) fn start_isolated_index_worker(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<FileSearchBuildResult, String> {
    let mut options = normalize_index_options(options)?;
    // A fresh rebuild should resume live watching when the desired setting is enabled.
    options.watcher_paused = false;
    // The Content Search index always indexes text content — that is the
    // entire purpose of this index, and the opt-out toggle was removed from
    // the UI. Pin it on so an older saved config cannot leave the content
    // index built without any content.
    options.index_content = true;
    let state_dir = search_index_dir(&app)?;
    fs::create_dir_all(&state_dir).map_err(|error| format!("Cannot create search index state directory: {error}"))?;
    recover_content_index_dirs_for_state(&state_dir)?;
    stop_isolated_watch_worker_for_state(&state_dir);

    {
        let mut worker_lock = SEARCH_INDEX_WORKER.lock().map_err(|_| "Search worker lock failed".to_string())?;
        if let Some(worker) = worker_lock.as_ref() {
            let still_running =
                worker.child.lock().map(|mut child| child.try_wait().ok().flatten().is_none()).unwrap_or(false);
            if still_running {
                return Err("Index build is already running".to_string());
            }
            *worker_lock = None;
        }
    }

    {
        let mut guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
        *guard = None;
    }

    let options_path = state_dir.join(INDEX_WORKER_OPTIONS_FILE);
    let status_path = state_dir.join(INDEX_WORKER_STATUS_FILE);
    let cancel_path = state_dir.join(INDEX_WORKER_CANCEL_FILE);
    let options_bytes = serde_json::to_vec_pretty(&options)
        .map_err(|error| format!("Cannot serialize index worker options: {error}"))?;
    fs::write(&options_path, options_bytes).map_err(|error| format!("Cannot write index worker options: {error}"))?;
    request_stale_worker_stop(&status_path, &cancel_path);
    let _ = fs::remove_file(&cancel_path);
    let _ = fs::remove_file(&status_path);
    write_index_worker_status(
        &status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Starting isolated index worker".to_string()),
    )?;

    let mut command = Command::new(
        std::env::current_exe().map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("index")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--options")
        .arg(&options_path)
        .arg("--status")
        .arg(&status_path)
        .arg("--cancel")
        .arg(&cancel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_worker_process(&mut command, &options.performance_mode, WorkerProcessKind::Index);

    let child = command.spawn().map_err(|error| format!("Cannot start isolated index worker: {error}"))?;
    assign_to_workers_job(&child);
    let worker = SearchIndexWorkerProcess {
        child: Arc::new(Mutex::new(child)),
        status_path: status_path.clone(),
        cancel_path: cancel_path.clone(),
    };

    {
        let mut worker_lock = SEARCH_INDEX_WORKER.lock().map_err(|_| "Search worker lock failed".to_string())?;
        *worker_lock = Some(worker.clone());
    }

    update_status(|status| {
        status.indexing = true;
        status.initialized = false;
        status.watching = false;
        status.roots = options.roots.clone();
        status.include_hidden = options.include_hidden;
        status.index_content = options.index_content;
        status.max_content_kb = options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(options.commit_every);
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.exclude_folders = options.exclude_folders.clone();
        status.exclude_extensions = options.exclude_extensions.clone();
        // A fresh build restarts the live scan counters from zero so a stale
        // count from the previous build never flashes before the first poll.
        status.scanned_entries = 0;
        status.skipped_files = 0;
        status.last_error = None;
        status.diagnostics.index_worker = "starting".to_string();
        status.diagnostics.watcher_worker = "stopped".to_string();
        status.diagnostics.watcher_strategy = "stopped during rebuild".to_string();
        status.diagnostics.performance_mode = options.performance_mode.clone();
        status.diagnostics.last_worker_message = Some("Index build started in isolated worker".to_string());
        status.diagnostics.last_status_at_ms = Some(unix_now_ms());
    });
    emit_progress(
        &app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "content".to_string(),
            indexed_files: 0,
            scanned_entries: 0,
            skipped_files: 0,
            current_path: String::new(),
            finished: false,
            canceled: false,
            success: false,
            message: "Index build started in isolated worker".to_string(),
            partial: false,
        },
    );

    start_index_worker_monitor(app, worker, options);

    Ok(FileSearchBuildResult {
        success: true,
        canceled: false,
        background: true,
        indexed_files: 0,
        scanned_entries: 0,
        skipped_files: 0,
        message: "Index build started in isolated worker".to_string(),
    })
}

#[derive(Clone, Copy)]
pub(super) enum WorkerProcessKind {
    Index,
    Watch,
    FilenameIndex,
}

#[cfg(target_os = "windows")]
pub(super) fn configure_worker_process(command: &mut Command, performance_mode: &str, worker_kind: WorkerProcessKind) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const IDLE_PRIORITY_CLASS: u32 = 0x00000040;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
    const NORMAL_PRIORITY_CLASS: u32 = 0x00000020;

    let mode = normalize_index_performance_mode(performance_mode);
    let priority = match (mode.as_str(), worker_kind) {
        ("fast", WorkerProcessKind::Index) => NORMAL_PRIORITY_CLASS,
        (_, WorkerProcessKind::Watch) => IDLE_PRIORITY_CLASS,
        ("quiet", _) => IDLE_PRIORITY_CLASS,
        _ => BELOW_NORMAL_PRIORITY_CLASS,
    };
    command.creation_flags(CREATE_NO_WINDOW | priority);
}

#[cfg(not(target_os = "windows"))]
pub(super) fn configure_worker_process(
    _command: &mut Command,
    _performance_mode: &str,
    _worker_kind: WorkerProcessKind,
) {
}

/// A Windows Job Object with a hard per-process memory cap — used to sandbox
/// the out-of-process content extractor children (Phase 4, task 11). The OS
/// terminates any assigned process that exceeds the cap, so a pathological file
/// can only ever take down one extractor child, never the whole app. The
/// `KILL_ON_JOB_CLOSE` flag means dropping this handle also kills every child
/// still assigned to it — a parent crash cannot leave orphan extractors.
#[cfg(windows)]
pub(super) struct MemoryCappedJob {
    pub(super) handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl MemoryCappedJob {
    /// Create a job object capping every assigned process at `memory_limit_bytes`
    /// (`None`: no cap, only ending with the job's owner).
    pub(super) fn new(memory_limit_bytes: impl Into<Option<usize>>) -> Result<Self, String> {
        let memory_limit_bytes = memory_limit_bytes.into();
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        };

        // SAFETY: standard Win32 job-object creation; the returned handle is
        // owned by `self` and closed exactly once in `Drop`.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|error| format!("Cannot create extractor job object: {error}"))?;

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(limit) = memory_limit_bytes {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = limit;
        }

        // SAFETY: `info` is a fully-initialized struct of the size declared for
        // the `JobObjectExtendedLimitInformation` class.
        let applied = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const core::ffi::c_void,
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("the limit struct is a few hundred bytes"),
            )
        };
        if let Err(error) = applied {
            // SAFETY: closing the handle just created, before discarding it.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(format!("Cannot set extractor job memory cap: {error}"));
        }

        Ok(Self { handle })
    }

    /// A `Copy`, non-owning handle to this job, for assigning children to it
    /// from the extractor slot threads (including respawned children). The job
    /// stays owned — and closed — by this `MemoryCappedJob`.
    pub(super) fn assigner(&self) -> JobAssigner {
        JobAssigner(self.handle.0)
    }
}

#[cfg(windows)]
impl Drop for MemoryCappedJob {
    fn drop(&mut self) {
        // SAFETY: `self.handle` was created in `new` and is closed exactly once
        // here. With KILL_ON_JOB_CLOSE this also terminates any still-assigned
        // extractor child, so no orphan processes survive the parent.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// Index workers (content build, filename build, watcher) go in this job,
/// held open for the app's whole life: when the app ends — quit, crash or
/// killed — Windows ends them too, so the next owner never finds orphans
/// still writing the index beside its own workers.
pub(super) fn assign_to_workers_job(child: &std::process::Child) {
    static JOB: std::sync::OnceLock<Option<MemoryCappedJob>> = std::sync::OnceLock::new();
    if let Some(job) = JOB.get_or_init(|| MemoryCappedJob::new(None).ok()) {
        let _ = job.assigner().assign(child);
    }
}

/// Non-Windows stand-in for `MemoryCappedJob`: there is no Job Object on other
/// platforms, so extractor children run un-capped (the in-child size guard from
/// task #1 stays the safety net). Keeps `ExtractorPool` cross-platform.
#[cfg(not(windows))]
pub(super) struct MemoryCappedJob;

#[cfg(not(windows))]
impl MemoryCappedJob {
    pub(super) fn new(_memory_limit_bytes: impl Into<Option<usize>>) -> Result<Self, String> {
        Ok(Self)
    }
    pub(super) fn assigner(&self) -> JobAssigner {
        JobAssigner
    }
}

/// A `Copy`, non-owning handle to the pool's Job Object. Each extractor slot
/// thread holds one so it can place a respawned child under the same memory
/// cap. The Job Object is owned (and closed) by the pool's single
/// `MemoryCappedJob`; once that handle is closed `assign` simply fails, which a
/// slot treats as a respawn failure.
#[cfg(windows)]
#[derive(Clone, Copy)]
pub(super) struct JobAssigner(isize);

#[cfg(windows)]
impl JobAssigner {
    /// Place a freshly-spawned child under the job's per-process memory cap.
    pub(super) fn assign(&self, child: &std::process::Child) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;

        let process = HANDLE(child.as_raw_handle() as isize);
        // SAFETY: `child` is a live process just spawned; `self.0` is the job
        // handle the pool created. AssignProcessToJobObject validates both and
        // returns an error (never UB) if the job handle has been closed.
        unsafe { AssignProcessToJobObject(HANDLE(self.0), process) }
            .map_err(|error| format!("Cannot assign extractor child to job object: {error}"))
    }
}

/// Non-Windows stand-in for `JobAssigner`: no Job Object, so `assign` is a
/// no-op and extractor children run un-capped (the in-child size guard from
/// task #1 stays the safety net).
#[cfg(not(windows))]
#[derive(Clone, Copy)]
pub(super) struct JobAssigner;

#[cfg(not(windows))]
impl JobAssigner {
    pub(super) fn assign(&self, _child: &std::process::Child) -> Result<(), String> {
        Ok(())
    }
}

/// Hard per-extractor-child memory cap. A child that exceeds this — a
/// pathological file parsing into a memory bomb — is killed by the OS, and that
/// file is then indexed by name only. Sized to fit normal PDF/Office parsing
/// while still bounding a small pool on a 4–8 GB machine.
pub(super) const EXTRACTOR_CHILD_MEMORY_CAP_BYTES: usize = 512 * 1024 * 1024;
/// Bounded paths channel — backpressure: the walker blocks submitting paths
/// once the pool is this far ahead of the children.
pub(super) const EXTRACTOR_POOL_PATH_BUFFER: usize = 64;
/// Bounded results channel — caps how much extracted text waits to be indexed.
pub(super) const EXTRACTOR_POOL_RESULT_BUFFER: usize = 16;
/// Global cap on extractor-child respawns for one index build. A healthy build
/// needs zero; a pathological one (memory-bomb files, a broken extractor) is
/// bounded here so a systemic failure cannot spawn processes without end. Once
/// it is reached, a slot whose child dies just exits and the pool shrinks.
pub(super) const EXTRACTOR_POOL_MAX_RESPAWNS: usize = 32;

/// One finished extraction handed back from a pool child.
pub(super) struct ExtractResult {
    pub(super) path: String,
    /// `Some` = extracted text. `None` = unsupported format, over the size
    /// guard, an extraction failure, or the child died mid-extraction — in
    /// every case the caller indexes the file by name only.
    pub(super) text: Option<String>,
}

/// A pool of out-of-process content extractor children (Phase 4, task 11). The
/// content-index build's walker submits document file paths; the children
/// extract text in parallel under an OS-enforced per-process memory cap; the
/// results stream back for the build to index.
///
/// `new` returns the pool plus the two channel ends — clone the `SyncSender`
/// for each producer (walker thread) and drain the `Receiver` on the indexing
/// side. The result channel closes (ending the drain) once every path sender
/// is dropped and all in-flight extractions have completed. The pool value only
/// has to be kept alive for the duration: it owns the one Job Object, so
/// dropping it terminates every extractor child still running.
pub(super) struct ExtractorPool {
    pub(super) _job: MemoryCappedJob,
}

/// Read one result frame from a child's stdout — `[u32 path_len][path][u8 ok]
/// [u32 text_len][text]`, little-endian. `None` on EOF or a malformed/partial
/// frame, i.e. the child has exited (cleanly or OS-killed).
pub(super) fn read_extract_frame(reader: &mut impl std::io::Read) -> Option<(String, Option<String>)> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).ok()?;
    let path_len = u32::from_le_bytes(len_buf) as usize;
    if path_len > 64 * 1024 {
        return None;
    }
    let mut path_bytes = vec![0u8; path_len];
    reader.read_exact(&mut path_bytes).ok()?;
    let path = String::from_utf8(path_bytes).ok()?;

    let mut ok_buf = [0u8; 1];
    reader.read_exact(&mut ok_buf).ok()?;

    reader.read_exact(&mut len_buf).ok()?;
    let text_len = u32::from_le_bytes(len_buf) as usize;
    if text_len > 64 * 1024 * 1024 {
        return None;
    }
    let mut text_bytes = vec![0u8; text_len];
    reader.read_exact(&mut text_bytes).ok()?;

    let text = if ok_buf[0] == 1 { Some(String::from_utf8_lossy(&text_bytes).into_owned()) } else { None };
    Some((path, text))
}

/// Spawn one extractor child (`--dash-index-worker extract`) with piped
/// stdin/stdout, no console window, and a process priority matching the build's
/// performance mode (fast → normal, balanced → below-normal, quiet → idle) so a
/// fast build's extractor children are not needlessly throttled.
pub(super) fn spawn_extractor_child(
    max_bytes: usize,
    performance_mode: &str,
    ocr_env: &crate::text_extract::OcrChildEnv,
) -> Result<std::process::Child, String> {
    let mut command = Command::new(
        std::env::current_exe().map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("extract")
        .arg("--max-bytes")
        .arg(max_bytes.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Wave 8 / task #88 (2026-05-28): set DASH_OCR_* env vars on the
    // child Command — children inherit them and OcrIndexConfig::from_env()
    // in text_extract.rs reads them once on first PDF. No-op when OCR is
    // disabled (apply_to does nothing on an empty OcrChildEnv).
    ocr_env.apply_to(&mut command);
    configure_worker_process(&mut command, performance_mode, WorkerProcessKind::Index);
    command.spawn().map_err(|error| format!("Cannot start extractor child: {error}"))
}

/// What one watched extraction ended with.
pub(super) enum ExtractAnswer {
    /// The child answered (`text` `None`: no content for that file).
    Done(String, Option<String>),
    /// The child died mid-file (crash, memory cap); it has been reaped.
    Died,
    /// The child ran past the per-file timeout and was killed and reaped.
    TimedOut,
}

/// An extractor child whose answers are read on a helper thread, so the
/// caller can stop waiting after a per-file timeout and kill it (a parser
/// stuck in a loop would otherwise stall its caller forever). The wait
/// doesn't rely on the pipe closing: a grandchild (tesseract) can hold it
/// open after the child is killed.
pub(super) struct WatchedChild {
    pub(super) child: Child,
    pub(super) stdin: Option<std::process::ChildStdin>,
    /// One result frame per request; disconnected once stdout ends.
    pub(super) frames: std::sync::mpsc::Receiver<(String, Option<String>)>,
}

impl WatchedChild {
    pub(super) fn new(child: Child, stdin: std::process::ChildStdin, stdout: std::process::ChildStdout) -> Self {
        let (frame_tx, frames) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut stdout = std::io::BufReader::new(stdout);
            while let Some(frame) = read_extract_frame(&mut stdout) {
                if frame_tx.send(frame).is_err() {
                    break;
                }
            }
        });
        WatchedChild { child, stdin: Some(stdin), frames }
    }

    /// Asks the child for one file's text (write the path to its stdin, take
    /// the result frame), waiting at most `timeout`. After `Died` or
    /// `TimedOut` the child is gone; the caller replaces it.
    pub(super) fn extract(&mut self, path: &Path, timeout: std::time::Duration) -> ExtractAnswer {
        use std::io::Write;
        use std::sync::mpsc::RecvTimeoutError;
        let line = format!("{}\n", path.to_string_lossy());
        let sent = match self.stdin.as_mut() {
            Some(stdin) => stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).is_ok(),
            None => false,
        };
        if !sent {
            // Can't reach it: make sure it's gone, then reap.
            let _ = self.child.kill();
            let _ = self.child.wait();
            return ExtractAnswer::Died;
        }
        match self.frames.recv_timeout(timeout) {
            Ok((answered_path, text)) => ExtractAnswer::Done(answered_path, text),
            Err(RecvTimeoutError::Disconnected) => {
                // Usually already exited; a garbled frame leaves it running.
                let _ = self.child.kill();
                let _ = self.child.wait();
                ExtractAnswer::Died
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                ExtractAnswer::TimedOut
            }
        }
    }
}

impl Drop for WatchedChild {
    /// Closes stdin (an idle child exits at EOF) and reaps the child.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// Spawn one extractor child, place it under the pool's memory-capped job, and
/// take its stdin/stdout pipes. Used both for the pool's initial children and
/// for respawning a child that died.
pub(super) fn spawn_assigned_extractor_child(
    max_bytes: usize,
    performance_mode: &str,
    assigner: JobAssigner,
    ocr_env: &crate::text_extract::OcrChildEnv,
) -> Result<(std::process::Child, std::process::ChildStdin, std::process::ChildStdout), String> {
    let mut child = spawn_extractor_child(max_bytes, performance_mode, ocr_env)?;
    assigner.assign(&child)?;
    let stdin = child.stdin.take().ok_or_else(|| "Extractor child has no stdin pipe".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "Extractor child has no stdout pipe".to_string())?;
    Ok((child, stdin, stdout))
}

/// Body of one extractor slot thread. The slot owns its child end-to-end —
/// stdin, stdout, lifecycle — so recovering from a child death is entirely
/// local: it reports the lost file as name-only and respawns a replacement
/// under the same job, keeping the pool at full strength. It signals readiness
/// on `free_tx` and receives one path at a time on `work_rx`.
#[expect(
    clippy::too_many_arguments,
    reason = "thread body: each argument is a separate handle the pool moves into the slot"
)]
pub(super) fn extractor_slot(
    index: usize,
    max_bytes: usize,
    performance_mode: String,
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    work_rx: std::sync::mpsc::Receiver<PathBuf>,
    result_tx: std::sync::mpsc::SyncSender<ExtractResult>,
    free_tx: std::sync::mpsc::Sender<usize>,
    assigner: JobAssigner,
    respawn_budget: Arc<std::sync::atomic::AtomicUsize>,
    should_stop: Arc<AtomicBool>,
    ocr_env: crate::text_extract::OcrChildEnv,
) {
    // The child is reaped when `watched` is dropped or replaced.
    let mut watched = WatchedChild::new(child, stdin, stdout);
    let timeout = ocr_env.extract_timeout();

    // Announce readiness for the first path.
    if free_tx.send(index).is_err() {
        return;
    }

    while let Ok(path) = work_rx.recv() {
        let timed_out = match watched.extract(&path, timeout) {
            ExtractAnswer::Done(result_path, text) => {
                // The child answered — forward the result and ask for more work.
                let delivered = result_tx.send(ExtractResult { path: result_path, text }).is_ok();
                if !delivered || free_tx.send(index).is_err() {
                    return;
                }
                continue;
            }
            ExtractAnswer::Died => false,
            ExtractAnswer::TimedOut => true,
        };

        // The child died (or was killed for taking too long) and has been
        // reaped. If the build is aborting (the pool was dropped to hard-kill
        // children) exit quietly — no name-only result, no respawn.
        if should_stop.load(Ordering::Relaxed) {
            return;
        }

        // A live build: index the lost file by name only, then recover the
        // slot. The toxic file is never retried — the walk submits each path
        // exactly once and the replacement child resumes from the next
        // `work_rx` item — so one bad file can take down at most one child.
        let _ = result_tx.send(ExtractResult { path: path.to_string_lossy().into_owned(), text: None });
        if timed_out {
            eprintln!(
                "dash-search: content extractor took over {}s on {} — indexed by name only",
                timeout.as_secs(),
                path.display()
            );
        } else {
            eprintln!("dash-search: content extractor child stopped on {} — indexed by name only", path.display());
        }

        // Respawn within the global budget so the pool stays at full strength.
        if respawn_budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(1)).is_err() {
            eprintln!("dash-search: content extractor respawn budget spent — pool shrinking");
            return;
        }
        match spawn_assigned_extractor_child(max_bytes, &performance_mode, assigner, &ocr_env) {
            Ok((new_child, new_stdin, new_stdout)) => {
                watched = WatchedChild::new(new_child, new_stdin, new_stdout);
                if free_tx.send(index).is_err() {
                    return;
                }
            }
            Err(error) => {
                eprintln!("dash-search: content extractor child could not be respawned ({error}) — pool shrinking");
                return;
            }
        }
    }

    // `work_rx` closed — the build is done. Dropping `watched` closes the
    // child's stdin (it exits at EOF) and reaps it so it does not linger.
}

impl ExtractorPool {
    /// Build a pool of `child_count` extractor children (clamped to 1..=8).
    /// Returns the pool, the path `SyncSender` (clone one per producer), and the
    /// result `Receiver` (drain on the indexing side). `should_stop` is the
    /// build's abort flag: when it is set, a slot whose child dies exits instead
    /// of respawning, because the build is tearing the pool down.
    ///
    /// Wave 8.5 (2026-05-28): when `cache_ctx` is `Some`, the dispatcher
    /// content-hashes OCR-eligible files (images + PDFs) before sending them
    /// to a slot and looks up `(hash, lang)` in the persistent OCR cache. On
    /// hit, the cached text bypasses the slot entirely — zero subprocess work
    /// for byte-identical duplicates across folders or rebuilds. On miss, the
    /// slot processes normally and a forwarder thread stores the result back
    /// to the cache. `None` (OCR off) keeps the pool's original behavior:
    /// no hashing, no redb writes, no overhead.
    pub(super) fn new(
        child_count: usize,
        max_bytes: usize,
        // Per-child Job Object memory cap in bytes. Normally
        // `EXTRACTOR_CHILD_MEMORY_CAP_BYTES`, but `clamp_pool_for_available_ram`
        // reduces it on low-RAM machines.
        memory_cap_bytes: usize,
        performance_mode: &str,
        should_stop: Arc<AtomicBool>,
        ocr_env: crate::text_extract::OcrChildEnv,
        cache_ctx: Option<crate::ocr_cache::OcrCacheCtx>,
    ) -> Result<(ExtractorPool, std::sync::mpsc::SyncSender<PathBuf>, std::sync::mpsc::Receiver<ExtractResult>), String>
    {
        use std::collections::HashMap;
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc::{channel, sync_channel};

        let child_count = child_count.clamp(1, 8);

        // One Job Object for the whole pool. `JOB_OBJECT_LIMIT_PROCESS_MEMORY`
        // caps each assigned process individually; every child — original or
        // respawned — gets the same ceiling. `KILL_ON_JOB_CLOSE` terminates
        // all children when the pool drops. The cap is profile-aware:
        // `clamp_pool_for_available_ram` reduces it on low-RAM machines so we
        // never commit N × 512 MB on a 4 GB laptop.
        let job = MemoryCappedJob::new(memory_cap_bytes)?;
        let assigner = job.assigner();

        let (path_tx, path_rx) = sync_channel::<PathBuf>(EXTRACTOR_POOL_PATH_BUFFER);
        // Wave 8.5 split: slots send to `inner_result_tx`. The forwarder
        // thread (below) reads `inner_result_rx`, stores any cacheable text
        // back to redb, and forwards the result on `outer_result_tx` — which
        // is the receiver the caller sees. When `cache_ctx` is `None` the
        // forwarder is still in the loop but does nothing besides forward,
        // costing one extra channel hop per result (microseconds, dwarfed
        // by extraction time).
        let (inner_result_tx, inner_result_rx) = sync_channel::<ExtractResult>(EXTRACTOR_POOL_RESULT_BUFFER);
        let (outer_result_tx, outer_result_rx) = sync_channel::<ExtractResult>(EXTRACTOR_POOL_RESULT_BUFFER);
        let (free_tx, free_rx) = channel::<usize>();
        let respawn_budget = Arc::new(AtomicUsize::new(EXTRACTOR_POOL_MAX_RESPAWNS));

        // Wave 8.5: dispatcher records `(path_string, content_hash)` here
        // for every cache MISS it sent to a slot; the forwarder pops the
        // hash when the slot answers, so cache stores only happen for
        // misses we actually wanted to cache. Wrapped in a Mutex because
        // the dispatcher (writer) and forwarder (reader/remover) run on
        // separate threads.
        let pending_hashes: Arc<Mutex<HashMap<String, crate::ocr_cache::ContentHash>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // One `work` channel per slot. The dispatcher routes each path to the
        // slot it just pulled from `free`; each slot thread fully owns a child.
        let mut work_txs = Vec::with_capacity(child_count);
        for index in 0..child_count {
            let (child, stdin, stdout) =
                spawn_assigned_extractor_child(max_bytes, performance_mode, assigner, &ocr_env)?;
            let (work_tx, work_rx) = channel::<PathBuf>();
            work_txs.push(work_tx);

            let inner_result_tx = inner_result_tx.clone();
            let free_tx = free_tx.clone();
            let respawn_budget = Arc::clone(&respawn_budget);
            let should_stop = Arc::clone(&should_stop);
            let performance_mode = performance_mode.to_string();
            // Wave 8 / task #88 (2026-05-28): each slot owns a clone of
            // the OCR env so respawns inside `extractor_slot` keep the
            // same OCR config the user opted into at build start.
            let ocr_env = ocr_env.clone();
            thread::spawn(move || {
                extractor_slot(
                    index,
                    max_bytes,
                    performance_mode,
                    child,
                    stdin,
                    stdout,
                    work_rx,
                    inner_result_tx,
                    free_tx,
                    assigner,
                    respawn_budget,
                    should_stop,
                    ocr_env,
                );
            });
        }

        // Wave 8.5 forwarder thread. Sits between slots/dispatcher and the
        // caller's result receiver. For each result on `inner_result_rx`:
        //  - Pop any pending hash for this path (so misses don't leak).
        //  - If we had a hash AND the slot returned text, store
        //    `(hash, lang) → text` in the persistent cache.
        //  - Forward the result to the caller unchanged.
        //
        // The thread exits when `inner_result_rx` closes — that happens once
        // every `inner_result_tx` clone has been dropped (slots' clones drop
        // when slots exit; the dispatcher's clone drops when the dispatcher
        // exits; the original is dropped immediately below). Dropping the
        // last `outer_result_tx` then closes the caller's receiver too.
        let dispatcher_inner_result_tx = inner_result_tx.clone();
        {
            let cache_ctx = cache_ctx.clone();
            let pending_hashes = Arc::clone(&pending_hashes);
            thread::spawn(move || {
                while let Ok(result) = inner_result_rx.recv() {
                    let hash = pending_hashes.lock().ok().and_then(|mut p| p.remove(&result.path));
                    if let (Some(ctx), Some(hash), Some(text)) = (cache_ctx.as_ref(), hash, result.text.as_deref()) {
                        // We store even empty text — that's a legitimate
                        // "extracted nothing useful" answer for a file we'd
                        // otherwise burn CPU on every rebuild.
                        crate::ocr_cache::store(ctx, &hash, text);
                    }
                    if outer_result_tx.send(result).is_err() {
                        // Caller dropped the receiver — nothing to forward to.
                        return;
                    }
                }
                drop(outer_result_tx);
            });
        }
        drop(inner_result_tx);

        // Dispatcher thread: routes each submitted path to the next idle slot.
        // It is the single consumer of `path_rx`; demand-driven via `free`, so
        // a slot stuck on a slow file never holds up the others.
        //
        // Wave 8.5: when `cache_ctx` is set and the file is OCR-eligible,
        // BLAKE3-hash the bytes and look up `(hash, lang)` in the cache.
        //  - HIT  → emit a synthetic result on `dispatcher_inner_result_tx`
        //           so the forwarder sees a normal event stream. No pending
        //           entry → forwarder doesn't double-cache. The slot is
        //           never touched, saving the full extraction round-trip.
        //  - MISS → record the hash in `pending_hashes`; fall through to
        //           normal slot dispatch. The forwarder will store the
        //           slot's text under that hash when it arrives.
        let cache_ctx_for_dispatcher = cache_ctx.clone();
        let pending_hashes_for_dispatcher = Arc::clone(&pending_hashes);
        thread::spawn(move || {
            while let Ok(path) = path_rx.recv() {
                if let Some(ctx) = cache_ctx_for_dispatcher.as_ref() {
                    if crate::ocr_cache::is_cache_eligible_path(&path) {
                        if let Ok(hash) = crate::ocr_cache::hash_file(&path) {
                            if let Some(cached_text) = crate::ocr_cache::lookup(ctx, &hash) {
                                let path_string = path.to_string_lossy().into_owned();
                                if dispatcher_inner_result_tx
                                    .send(ExtractResult { path: path_string, text: Some(cached_text) })
                                    .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            if let Ok(mut p) = pending_hashes_for_dispatcher.lock() {
                                p.insert(path.to_string_lossy().into_owned(), hash);
                            }
                        }
                    }
                }

                let Ok(slot) = free_rx.recv() else {
                    // No slot will ever free again — every slot thread has
                    // exited (respawn budget spent). Stop dispatching.
                    return;
                };
                // A slot pulled from `free` is alive and waiting, so this send
                // succeeds; if it somehow does not, the path is simply skipped
                // (still discoverable via the filename index).
                let _ = work_txs[slot].send(path);
            }
            // `path_rx` closed — the walk is done. Dropping every `work`
            // sender closes the live slots' `work_rx` so they finish and
            // exit. Dropping `dispatcher_inner_result_tx` (held by this
            // closure) plus the slots' clones is what eventually closes
            // `inner_result_rx` for the forwarder.
            drop(work_txs);
            drop(dispatcher_inner_result_tx);
        });

        Ok((ExtractorPool { _job: job }, path_tx, outer_result_rx))
    }
}

pub(super) fn start_index_worker_monitor(
    app: std::sync::Arc<dyn dash_core::Host>,
    worker: SearchIndexWorkerProcess,
    options: FileSearchIndexOptions,
) {
    thread::spawn(move || {
        let mut last_indexed = 0u64;
        loop {
            if let Ok(Some(status)) = read_index_worker_status(&worker.status_path) {
                last_indexed = status.indexed_files;
                apply_index_worker_status(&app, &options, &status);
            }

            let exited = worker.child.lock().map(|mut child| child.try_wait().ok().flatten()).unwrap_or(None);
            if exited.is_some() {
                let mut final_status =
                    read_index_worker_status(&worker.status_path).ok().flatten().unwrap_or(IndexWorkerStatusFile {
                        running: false,
                        finished: true,
                        success: false,
                        canceled: false,
                        indexed_files: last_indexed,
                        scanned_entries: 0,
                        skipped_files: 0,
                        message: "Index worker exited without a final status".to_string(),
                        updated_at_ms: unix_now_ms(),
                        error: Some("Index worker exited without a final status".to_string()),
                        partial: false,
                    });
                if !final_status.finished {
                    let was_canceled = worker.cancel_path.exists();
                    let message = if was_canceled {
                        "Index build cancelled".to_string()
                    } else {
                        "Index worker exited before finishing".to_string()
                    };
                    final_status = IndexWorkerStatusFile::finished(
                        false,
                        was_canceled,
                        final_status.indexed_files,
                        final_status.scanned_entries,
                        final_status.skipped_files,
                        message.clone(),
                        if was_canceled { None } else { Some(message) },
                    );
                    let _ = write_index_worker_status(&worker.status_path, &final_status);
                }
                apply_index_worker_status(&app, &options, &final_status);
                if final_status.success {
                    // The worker can't write the settings database while this
                    // process holds it, so record the finished build here.
                    let rebuild_schedule = read_search_config(&app)
                        .ok()
                        .flatten()
                        .map(|config| config.rebuild_schedule)
                        .unwrap_or_default();
                    let _ = write_search_config(
                        &app,
                        &StoredSearchConfig {
                            options: options.clone(),
                            indexed_files: final_status.indexed_files,
                            last_indexed_at_ms: Some(final_status.updated_at_ms),
                            rebuild_schedule,
                        },
                    );
                    let _ = ensure_engine_loaded(&app);
                }
                if let Ok(mut lock) = SEARCH_INDEX_WORKER.lock() {
                    *lock = None;
                }
                let _ = fs::remove_file(&worker.cancel_path);
                break;
            }

            thread::sleep(Duration::from_millis(700));
        }
    });
}

/// Spawn the standalone filename indexer as an isolated subprocess (Phase 3,
/// step 7b). Returns `Ok(true)` when a worker was started, `Ok(false)` when the
/// run was skipped. Safe to run alongside a content build.
///
/// `force` decides what happens when a filename worker is already running —
/// which, post-9d, is the long-lived USN tailer. A user-triggered rebuild
/// passes `force = true`: the running tailer is stopped so the index can be
/// rebuilt for the current roots. The scheduler passes `force = false`: a
/// running tailer is left alone, since it already keeps the index live.
pub(super) fn start_isolated_filename_index_worker(
    app: &std::sync::Arc<dyn dash_core::Host>,
    mut options: FileSearchIndexOptions,
    force: bool,
) -> Result<bool, String> {
    // The filename index indexes the File Search tab's sources
    // (`filename_roots`), stored separately from the content index's `roots`.
    options.roots = std::mem::take(&mut options.filename_roots);
    // The filename index ("My engine" / MFT) records names and paths only — its
    // schema has no content field, so it never extracts text. Pin the flag off
    // so the worker options file plainly reflects that.
    options.index_content = false;
    let options = normalize_index_options(options)?;
    if options.roots.is_empty() {
        return Ok(false);
    }

    let state_dir = search_index_dir(app)?;
    fs::create_dir_all(&state_dir).map_err(|error| format!("Cannot create search index state directory: {error}"))?;

    // A filename worker may already be running — post-9d that is the long-lived
    // tailer. Without `force`, leave it be; with `force` (a user rebuild), stop
    // it so the index can be rebuilt for the current roots.
    {
        let mut worker_lock =
            FILENAME_INDEX_WORKER.lock().map_err(|_| "Filename index worker lock failed".to_string())?;
        if let Some(worker) = worker_lock.as_ref() {
            let still_running =
                worker.child.lock().map(|mut child| child.try_wait().ok().flatten().is_none()).unwrap_or(false);
            if still_running {
                if !force {
                    return Ok(false);
                }
                // Stop the running worker / tailer: drop the cancel file so a
                // tailer mid-loop exits cleanly, and kill the child outright
                // so the rebuild can proceed without waiting.
                let _ = fs::write(&worker.cancel_path, b"cancel");
                if let Ok(mut child) = worker.child.lock() {
                    let _ = child.kill();
                }
            }
            *worker_lock = None;
        }
    }

    // Release the cached filename-index mmap so the worker's directory-rename
    // swap can succeed on Windows (an open mmap blocks the rename). The monitor
    // re-opens the handle once the build's status reports success.
    if let Ok(mut guard) = FILENAME_ENGINE.lock() {
        *guard = None;
    }

    let options_path = state_dir.join(FILENAME_WORKER_OPTIONS_FILE);
    let status_path = state_dir.join(FILENAME_WORKER_STATUS_FILE);
    let cancel_path = state_dir.join(FILENAME_WORKER_CANCEL_FILE);
    let options_bytes = serde_json::to_vec_pretty(&options)
        .map_err(|error| format!("Cannot serialize filename worker options: {error}"))?;
    fs::write(&options_path, options_bytes)
        .map_err(|error| format!("Cannot write filename worker options: {error}"))?;
    request_stale_worker_stop(&status_path, &cancel_path);
    let _ = fs::remove_file(&cancel_path);
    let _ = fs::remove_file(&status_path);
    write_index_worker_status(
        &status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Starting filename indexer".to_string()),
    )?;

    let mut command = Command::new(
        std::env::current_exe().map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("filename-index")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--options")
        .arg(&options_path)
        .arg("--status")
        .arg(&status_path)
        .arg("--cancel")
        .arg(&cancel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_worker_process(&mut command, &options.performance_mode, WorkerProcessKind::FilenameIndex);

    let child = command.spawn().map_err(|error| format!("Cannot start filename indexer: {error}"))?;
    assign_to_workers_job(&child);
    let worker = SearchIndexWorkerProcess { child: Arc::new(Mutex::new(child)), status_path, cancel_path };

    {
        let mut worker_lock =
            FILENAME_INDEX_WORKER.lock().map_err(|_| "Filename index worker lock failed".to_string())?;
        *worker_lock = Some(worker.clone());
    }

    // Mark the filename build in progress and raise the live "Indexing in
    // progress" panel immediately — before the monitor's first poll — so the
    // File Search tab reflects the build the same way the Content tab does.
    update_status(|status| {
        status.filename_indexing = true;
        status.filename_indexed_files = 0;
        // A fresh build restarts the live scan counters from zero.
        status.scanned_entries = 0;
        status.skipped_files = 0;
    });
    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "filename".to_string(),
            indexed_files: 0,
            scanned_entries: 0,
            skipped_files: 0,
            current_path: String::new(),
            finished: false,
            canceled: false,
            success: false,
            message: "Filename index build started".to_string(),
            partial: false,
        },
    );
    start_filename_index_worker_monitor(app.clone(), worker, state_dir);
    Ok(true)
}

/// Mirror one filename-build status update into `SEARCH_STATUS` and emit a
/// `file-search-index-progress` event — the filename-build twin of
/// `apply_index_worker_status`, so the File Search tab shows the same live
/// "Indexing in progress" panel + counters the Content tab shows. Only called
/// while the *build* is in flight; once the worker becomes the USN tailer /
/// notify watcher its messages are surfaced as the secondary status line.
pub(super) fn apply_filename_index_worker_status(
    app: &std::sync::Arc<dyn dash_core::Host>,
    status_file: &IndexWorkerStatusFile,
) {
    let finished = status_file.finished;
    let message = status_file.error.clone().unwrap_or_else(|| status_file.message.clone());
    update_status(|status| {
        status.filename_indexing = !finished;
        status.filename_indexed_files = status_file.indexed_files;
        // Live scan counters — keep `get_file_search_status` in sync with the
        // progress event so a mid-build view shows real numbers, not 0/0/0.
        status.scanned_entries = status_file.scanned_entries;
        status.skipped_files = status_file.skipped_files;
        if finished && status_file.success {
            status.filename_last_indexed_at_ms = Some(status_file.updated_at_ms);
        }
        status.diagnostics.last_status_at_ms = Some(status_file.updated_at_ms);
    });
    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "filename".to_string(),
            indexed_files: status_file.indexed_files,
            scanned_entries: status_file.scanned_entries,
            skipped_files: status_file.skipped_files,
            current_path: String::new(),
            finished,
            canceled: status_file.canceled,
            success: status_file.success,
            message,
            partial: false,
        },
    );
}

/// Watch the filename indexer subprocess. Relays the *build's* progress to the
/// UI (the same live panel the content build shows) until the build's status
/// reports `finished`; after that the worker lives on as the USN tailer / notify
/// watcher, so relaying stops. Also re-opens the cached filename handle once the
/// build succeeds and clears the worker slot on exit.
pub(super) fn start_filename_index_worker_monitor(
    app: std::sync::Arc<dyn dash_core::Host>,
    worker: SearchIndexWorkerProcess,
    state_dir: PathBuf,
) {
    thread::spawn(move || {
        let reopen_filename_handle = || {
            if let Ok(mut guard) = FILENAME_ENGINE.lock() {
                *guard = try_open_filename_index(&state_dir);
            }
        };
        let mut handle_reopened = false;
        // Latches once the build's status reports `finished`. After that the
        // worker keeps running as the live updater — its further status writes
        // are not "a build in progress" and must not re-raise the UI panel.
        let mut build_finished = false;
        let mut last_emitted_ms: u128 = 0;
        loop {
            let status = read_index_worker_status(&worker.status_path).ok().flatten();

            if let Some(status_file) = &status {
                if !build_finished && status_file.updated_at_ms != last_emitted_ms {
                    last_emitted_ms = status_file.updated_at_ms;
                    apply_filename_index_worker_status(&app, status_file);
                    if status_file.finished {
                        build_finished = true;
                    }
                }
                if !handle_reopened && status_file.finished && status_file.success {
                    reopen_filename_handle();
                    handle_reopened = true;
                }
            }

            let exited = worker.child.lock().map(|mut child| child.try_wait().ok().flatten()).unwrap_or(None);
            if exited.is_some() {
                // A build that never transitioned to a live updater (the walker
                // path with no watchable roots) reaches success only at exit.
                if !handle_reopened
                    && read_index_worker_status(&worker.status_path)
                        .ok()
                        .flatten()
                        .map(|status| status.finished && status.success)
                        .unwrap_or(false)
                {
                    reopen_filename_handle();
                }
                // If the worker died before its build reported `finished`
                // (killed, crashed, or cancelled), close out the UI so the
                // "Indexing in progress" panel does not stick on screen.
                if !build_finished {
                    let cancelled = worker.cancel_path.exists();
                    let message = if cancelled {
                        "Filename index build cancelled".to_string()
                    } else {
                        "Filename indexer exited before finishing".to_string()
                    };
                    update_status(|status| status.filename_indexing = false);
                    emit_progress(
                        &app,
                        FileSearchProgressEvent {
                            stage: "indexing".to_string(),
                            index_kind: "filename".to_string(),
                            indexed_files: 0,
                            scanned_entries: 0,
                            skipped_files: 0,
                            current_path: String::new(),
                            finished: true,
                            canceled: cancelled,
                            success: false,
                            message,
                            partial: false,
                        },
                    );
                }
                if let Ok(mut lock) = FILENAME_INDEX_WORKER.lock() {
                    *lock = None;
                }
                let _ = fs::remove_file(&worker.cancel_path);
                break;
            }
            thread::sleep(Duration::from_millis(700));
        }
    });
}

pub(super) fn start_isolated_watch_worker(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
    indexed_files: u64,
) -> Result<bool, String> {
    if options.roots.is_empty() {
        return Ok(false);
    }
    if !options.watcher_enabled {
        update_status(|status| {
            status.watcher_enabled = false;
            status.watcher_paused = false;
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "disabled manually".to_string();
            status.diagnostics.last_worker_message = Some("Live watcher is disabled in search settings".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        });
        return Ok(false);
    }
    if options.watcher_paused {
        update_status(|status| {
            status.watcher_enabled = true;
            status.watcher_paused = true;
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "paused manually".to_string();
            status.diagnostics.last_worker_message = Some("Live watcher is paused until the next rebuild".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        });
        return Ok(false);
    }

    let state_dir = search_index_dir(app)?;
    let active_dir = active_search_index_dir_for_state(&state_dir);
    if !active_dir.exists() {
        return Ok(false);
    }

    {
        let mut worker_lock =
            SEARCH_WATCHER_WORKER.lock().map_err(|_| "Search watcher worker lock failed".to_string())?;
        if let Some(worker) = worker_lock.as_ref() {
            let still_running =
                worker.child.lock().map(|mut child| child.try_wait().ok().flatten().is_none()).unwrap_or(false);
            if still_running {
                return Ok(true);
            }
            *worker_lock = None;
        }
    }

    let mut options = normalize_index_options(options)?;
    let watchable_roots = watchable_index_roots(&options);
    if watchable_roots.is_empty() {
        update_status(|status| {
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "disabled for drive roots/system roots".to_string();
            status.diagnostics.last_worker_message = Some(
                "Live watcher only works for explicit folders. Drive roots are not watched; use scheduled rebuilds or add specific folders.".to_string(),
            );
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        });
        return Ok(false);
    }
    options.roots = watchable_roots;
    let options_path = state_dir.join(WATCHER_WORKER_OPTIONS_FILE);
    let status_path = state_dir.join(WATCHER_WORKER_STATUS_FILE);
    let cancel_path = state_dir.join(WATCHER_WORKER_CANCEL_FILE);
    let options_bytes = serde_json::to_vec_pretty(&options)
        .map_err(|error| format!("Cannot serialize watcher worker options: {error}"))?;
    fs::write(&options_path, options_bytes).map_err(|error| format!("Cannot write watcher worker options: {error}"))?;
    request_stale_worker_stop(&status_path, &cancel_path);
    let _ = fs::remove_file(&cancel_path);
    let _ = fs::remove_file(&status_path);
    write_index_worker_status(
        &status_path,
        &IndexWorkerStatusFile::progress(0, indexed_files, 0, "Starting isolated index watcher".to_string()),
    )?;

    let mut command = Command::new(
        std::env::current_exe().map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("watch")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--options")
        .arg(&options_path)
        .arg("--status")
        .arg(&status_path)
        .arg("--cancel")
        .arg(&cancel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_worker_process(&mut command, &options.performance_mode, WorkerProcessKind::Watch);

    let child = command.spawn().map_err(|error| format!("Cannot start isolated index watcher: {error}"))?;
    assign_to_workers_job(&child);
    let worker = SearchIndexWorkerProcess {
        child: Arc::new(Mutex::new(child)),
        status_path: status_path.clone(),
        cancel_path: cancel_path.clone(),
    };

    {
        let mut worker_lock =
            SEARCH_WATCHER_WORKER.lock().map_err(|_| "Search watcher worker lock failed".to_string())?;
        *worker_lock = Some(worker.clone());
    }

    let watcher_strategy = watcher_strategy_for_index_size(indexed_files).name.to_string();
    update_status(|status| {
        status.watching = true;
        status.watcher_enabled = true;
        status.watcher_paused = false;
        status.diagnostics.watcher_worker = "starting".to_string();
        status.diagnostics.watcher_strategy = watcher_strategy;
        status.diagnostics.performance_mode = options.performance_mode.clone();
        status.diagnostics.last_worker_message = Some("Index watcher started in isolated worker".to_string());
        status.diagnostics.last_status_at_ms = Some(unix_now_ms());
    });
    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "watch".to_string(),
            index_kind: "content".to_string(),
            indexed_files: 0,
            scanned_entries: indexed_files,
            skipped_files: 0,
            current_path: String::new(),
            finished: false,
            canceled: false,
            success: false,
            message: "Index watcher started in isolated worker".to_string(),
            partial: false,
        },
    );

    start_watch_worker_monitor(app.clone(), worker, options);
    Ok(true)
}

pub(super) fn stop_isolated_watch_worker_for_state(state_dir: &Path) {
    let _ = fs::write(state_dir.join(WATCHER_WORKER_CANCEL_FILE), b"cancel");
    let _ = write_index_worker_status(
        &state_dir.join(WATCHER_WORKER_STATUS_FILE),
        &IndexWorkerStatusFile::finished(false, true, 0, 0, 0, "Index watcher stopped".to_string(), None),
    );
    if let Ok(mut guard) = SEARCH_WATCHER_WORKER.lock() {
        if let Some(worker) = guard.take() {
            let _ = fs::write(&worker.cancel_path, b"cancel");
            if let Ok(mut child) = worker.child.lock() {
                let _ = child.kill();
            }
        }
    }
}

pub(super) fn start_watch_worker_monitor(
    app: std::sync::Arc<dyn dash_core::Host>,
    worker: SearchIndexWorkerProcess,
    options: FileSearchIndexOptions,
) {
    thread::spawn(move || {
        // Track the last emitted status timestamp so we don't fire Tauri events
        // when nothing has changed. The watcher only rewrites status_path when
        // a batch produced changes, so an unchanged timestamp means no work to report.
        let mut last_emitted_ms: u128 = 0;
        loop {
            if let Ok(Some(status)) = read_index_worker_status(&worker.status_path) {
                if status.updated_at_ms != last_emitted_ms {
                    last_emitted_ms = status.updated_at_ms;
                    apply_watch_worker_status(&app, &options, &status);
                }
            }

            let exited = worker.child.lock().map(|mut child| child.try_wait().ok().flatten()).unwrap_or(None);
            if exited.is_some() {
                let mut final_status = read_index_worker_status(&worker.status_path).ok().flatten().unwrap_or(
                    IndexWorkerStatusFile::finished(
                        false,
                        false,
                        0,
                        0,
                        0,
                        "Index watcher exited without a final status".to_string(),
                        Some("Index watcher exited without a final status".to_string()),
                    ),
                );
                if !final_status.finished {
                    let was_canceled = worker.cancel_path.exists();
                    let message = if was_canceled {
                        "Index watcher stopped".to_string()
                    } else {
                        "Index watcher exited before finishing".to_string()
                    };
                    final_status = IndexWorkerStatusFile::finished(
                        false,
                        was_canceled,
                        final_status.indexed_files,
                        final_status.scanned_entries,
                        final_status.skipped_files,
                        message.clone(),
                        if was_canceled { None } else { Some(message) },
                    );
                }
                apply_watch_worker_status(&app, &options, &final_status);
                if let Ok(mut lock) = SEARCH_WATCHER_WORKER.lock() {
                    *lock = None;
                }
                let _ = fs::remove_file(&worker.cancel_path);
                break;
            }

            // Poll every 3s — the watcher rate-limits to 5s minimum between applies,
            // so faster polling just wastes CPU on duplicate status file reads.
            thread::sleep(Duration::from_millis(3_000));
        }
    });
}

pub(super) fn apply_watch_worker_status(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchIndexOptions,
    status_file: &IndexWorkerStatusFile,
) {
    let message = status_file.error.clone().unwrap_or_else(|| status_file.message.clone());
    update_status(|status| {
        status.watching = status_file.running && !status_file.finished;
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.updated_files = status_file.indexed_files;
        status.deleted_files = status_file.skipped_files;
        if status_file.finished && !status_file.canceled {
            status.last_error = status_file.error.clone();
        }
        status.diagnostics.watcher_worker = if status_file.running && !status_file.finished {
            "running".to_string()
        } else if status_file.canceled {
            "stopped".to_string()
        } else if status_file.success {
            "finished".to_string()
        } else {
            "stopped".to_string()
        };
        status.diagnostics.performance_mode = normalize_index_performance_mode(&options.performance_mode);
        status.diagnostics.last_worker_message = Some(message.clone());
        status.diagnostics.last_status_at_ms = Some(status_file.updated_at_ms);
    });

    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "watch".to_string(),
            index_kind: "content".to_string(),
            indexed_files: status_file.indexed_files,
            scanned_entries: status_file.scanned_entries,
            skipped_files: status_file.skipped_files,
            current_path: String::new(),
            finished: status_file.finished,
            canceled: status_file.canceled,
            success: false,
            message,
            partial: false,
        },
    );
}

pub(super) fn apply_index_worker_status(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchIndexOptions,
    status_file: &IndexWorkerStatusFile,
) {
    let finished = status_file.finished;
    let message = status_file.error.clone().unwrap_or_else(|| status_file.message.clone());

    // Compute the effective count BEFORE taking the write lock so we can use
    // the same value in both the status update and the emitted event.
    //
    // The count must never go backwards during an active build. The very first
    // status writes ("Starting…", "Preparing…", "Loaded cache") carry
    // indexed_files=0 and would drop both the stat card AND the progress panel
    // from e.g. 37000 → 0 before climbing back up. Rule: only advance the
    // counter when the new value is strictly higher, OR when the build is done
    // (finished=true) — so the final tally always lands, even if it's a
    // legitimate 0 on a canceled empty run.
    let prev_indexed = SEARCH_STATUS.lock().map(|s| s.indexed_files).unwrap_or(0);
    let effective_indexed =
        if finished || status_file.indexed_files > prev_indexed { status_file.indexed_files } else { prev_indexed };

    update_status(|status| {
        status.indexing = !finished;
        status.initialized = if finished { status_file.success } else { status.initialized };
        status.watching = false;
        status.roots = options.roots.clone();
        status.include_hidden = options.include_hidden;
        status.index_content = options.index_content;
        status.max_content_kb = options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(options.commit_every);
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.exclude_folders = options.exclude_folders.clone();
        status.exclude_extensions = options.exclude_extensions.clone();
        status.indexed_files = effective_indexed;
        // Live scan counters — mirrored so `get_file_search_status` reports the
        // same numbers the progress event carries (fixes 0/0/0 on a mid-build view).
        status.scanned_entries = status_file.scanned_entries;
        status.skipped_files = status_file.skipped_files;
        if finished && status_file.success {
            status.last_indexed_at_ms = Some(status_file.updated_at_ms);
            status.last_error = None;
        } else if finished && !status_file.canceled {
            status.last_error = Some(message.clone());
        }
        status.diagnostics.index_worker = if !finished {
            "running".to_string()
        } else if status_file.canceled {
            "stopped".to_string()
        } else if status_file.success {
            "finished".to_string()
        } else {
            "failed".to_string()
        };
        status.diagnostics.watcher_worker = "stopped".to_string();
        status.diagnostics.performance_mode = normalize_index_performance_mode(&options.performance_mode);
        status.diagnostics.last_worker_message = Some(message.clone());
        status.diagnostics.last_status_at_ms = Some(status_file.updated_at_ms);
    });

    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "content".to_string(),
            indexed_files: effective_indexed,
            scanned_entries: status_file.scanned_entries,
            skipped_files: status_file.skipped_files,
            current_path: String::new(),
            finished,
            canceled: status_file.canceled,
            success: status_file.success,
            message,
            partial: status_file.partial,
        },
    );
}

pub fn maybe_run_index_worker_from_args() -> bool {
    let args: Vec<std::ffi::OsString> = env::args_os().collect();
    let Some(worker_arg_position) = args.iter().position(|arg| arg.to_string_lossy() == INDEX_WORKER_ARG) else {
        return false;
    };

    let mode = args.get(worker_arg_position + 1).and_then(|value| value.to_str()).unwrap_or_default();

    let worker_args = args.get(worker_arg_position + 2..).map(|value| value.to_vec()).unwrap_or_default();

    let result = match mode {
        "index" | "watch" | "filename-index" => {
            let state_dir = worker_arg_path(&worker_args, "--state-dir");
            let options_path = worker_arg_path(&worker_args, "--options");
            let status_path = worker_arg_path(&worker_args, "--status");
            let cancel_path = worker_arg_path(&worker_args, "--cancel");
            match (state_dir, options_path, status_path, cancel_path) {
                (Ok(state_dir), Ok(options_path), Ok(status_path), Ok(cancel_path)) => match mode {
                    "index" => run_index_worker_index_mode(&state_dir, &options_path, &status_path, &cancel_path),
                    "filename-index" => {
                        run_index_worker_filename_mode(&state_dir, &options_path, &status_path, &cancel_path)
                    }
                    _ => run_index_worker_watch_mode(&state_dir, &options_path, &status_path, &cancel_path),
                },
                (_, _, Ok(status_path), _) => {
                    let message = "Index worker was started with invalid arguments".to_string();
                    let _ = write_index_worker_status(
                        &status_path,
                        &IndexWorkerStatusFile::finished(false, false, 0, 0, 0, message.clone(), Some(message.clone())),
                    );
                    Err(message)
                }
                _ => Err("Index worker was started with invalid arguments".to_string()),
            }
        }
        "extract" => run_extractor_worker(&worker_args),
        _ => Err(format!("Unknown index worker mode: {mode}")),
    };

    if let Err(error) = result {
        eprintln!("{error}");
    }

    true
}

pub(super) fn worker_arg_path(args: &[std::ffi::OsString], key: &str) -> Result<PathBuf, String> {
    let Some(position) = args.iter().position(|arg| arg.to_string_lossy() == key) else {
        return Err(format!("Missing worker argument {key}"));
    };
    args.get(position + 1).map(PathBuf::from).ok_or_else(|| format!("Missing value for worker argument {key}"))
}

/// Subprocess entry point for a content extractor child (`--dash-index-worker
/// extract`) — the out-of-process half of the Phase 4 task-11 pool. It loops
/// reading one file path per line from stdin, extracts text via
/// `text_extract::extract_text_for_indexing`, and writes one length-prefixed
/// result frame per path to stdout. The parent ends the child by closing its
/// stdin (EOF). The child runs inside a `MemoryCappedJob` (task 11a), so a
/// pathological file that blows the memory cap gets this child OS-killed — the
/// parent then respawns it (task 11e) and indexes that file by name only.
///
/// Result frame, all integers little-endian:
///   `[u32 path_len][path UTF-8][u8 ok][u32 text_len][text UTF-8]`
/// `ok` = 1 when text was extracted; 0 for an unsupported format, an
/// over-the-size-guard file, or an extraction failure (then `text_len` = 0).
/// File paths cannot contain newlines, so the stdin side is safely
/// line-delimited; the text side is length-prefixed since it can be anything.
pub(super) fn run_extractor_worker(worker_args: &[std::ffi::OsString]) -> Result<(), String> {
    use std::io::{BufRead, BufWriter, Write};

    let max_bytes = worker_args
        .iter()
        .position(|arg| arg.to_string_lossy() == "--max-bytes")
        .and_then(|index| worker_args.get(index + 1))
        .and_then(|value| value.to_string_lossy().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONTENT_KB as usize * 1024);

    let mut stdout = BufWriter::new(std::io::stdout().lock());

    for line in std::io::stdin().lock().lines() {
        // An unreadable line means stdin was closed — the parent is done with
        // this child, so exit cleanly.
        let Ok(raw) = line else {
            break;
        };
        let path_text = raw.trim();
        if path_text.is_empty() {
            continue;
        }

        let extracted = crate::text_extract::extract_text_for_indexing(Path::new(path_text), max_bytes);
        let (ok, text) = match extracted {
            Some(text) => (1u8, text),
            None => (0u8, String::new()),
        };

        let path_bytes = path_text.as_bytes();
        let text_bytes = text.as_bytes();
        let frame = (|| -> std::io::Result<()> {
            // Check both lengths before writing anything: a length that doesn't
            // fit the u32 header would desync every frame after it.
            let too_big = |_| std::io::Error::from(std::io::ErrorKind::InvalidData);
            let path_len = u32::try_from(path_bytes.len()).map_err(too_big)?;
            let text_len = u32::try_from(text_bytes.len()).map_err(too_big)?;
            stdout.write_all(&path_len.to_le_bytes())?;
            stdout.write_all(path_bytes)?;
            stdout.write_all(&[ok])?;
            stdout.write_all(&text_len.to_le_bytes())?;
            stdout.write_all(text_bytes)?;
            stdout.flush()
        })();
        // If the parent closed our stdout there is no one to report to — exit.
        if frame.is_err() {
            break;
        }
    }

    Ok(())
}
