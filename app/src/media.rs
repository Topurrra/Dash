//! Video and music playback for the preview pane. Windows Media Foundation
//! decodes (every format Windows plays), rodio plays the sound, and video
//! frames go to the pane as images. Two threads per file — sound and
//! picture — follow the sound's clock (or the wall clock when silent).
//! Other systems: not yet (the pane says so).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::RenderImage;

/// Times are in 100 ns units (Media Foundation's).
const HNS_PER_SEC: u64 = 10_000_000;
/// Largest frame kept for the pane; bigger videos are scaled down while decoding.
const MAX_FRAME: (u32, u32) = (1280, 720);

/// A duration in 100 ns units (saturating; real ones are far smaller).
fn hns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos() / 100).unwrap_or(u64::MAX)
}

const UNKNOWN: u8 = 0;
const YES: u8 = 1;
const NO: u8 = 2;

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    ended: AtomicBool,
    seek_generation: AtomicU64,
    seek_to: AtomicU64,
    position: AtomicU64,
    duration: AtomicU64,
    has_audio: AtomicU8,
    has_video: AtomicU8,
    volume: AtomicU32,
    frame: Mutex<Option<Arc<RenderImage>>>,
    error: Mutex<Option<String>>,
}

/// One file being played. Dropping it stops playback (the threads finish
/// on their own); `close` also waits for them to let go of the file.
pub struct Player {
    shared: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

impl Player {
    /// Opens `path` paused; the first video frame shows once decoded.
    pub fn open(path: &str) -> Player {
        let shared = Arc::new(Shared::default());
        shared.paused.store(true, Ordering::SeqCst);
        shared.volume.store(1f32.to_bits(), Ordering::SeqCst);
        let mut threads = Vec::new();
        #[cfg(windows)]
        {
            for video in [false, true] {
                let (shared, path) = (Arc::clone(&shared), path.to_string());
                let spawned = std::thread::Builder::new()
                    .name(if video { "dash-video" } else { "dash-audio" }.into())
                    .spawn(move || {
                        let result = if video { mf::run_video(&path, &shared) } else { mf::run_audio(&path, &shared) };
                        if let Err(e) = result {
                            let flag = if video { &shared.has_video } else { &shared.has_audio };
                            flag.store(NO, Ordering::SeqCst);
                            if video && shared.has_audio.load(Ordering::SeqCst) == NO {
                                *shared.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                            }
                        }
                    });
                threads.extend(spawned.ok());
            }
        }
        #[cfg(not(windows))]
        {
            // Sound only (rodio's own decoders); video needs a decoder this
            // build doesn't have yet.
            shared.has_video.store(NO, Ordering::SeqCst);
            let (shared, path) = (Arc::clone(&shared), path.to_string());
            let spawned = std::thread::Builder::new().name("dash-audio".into()).spawn(move || {
                if let Err(e) = sound::run(&path, &shared) {
                    shared.has_audio.store(NO, Ordering::SeqCst);
                    *shared.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                }
            });
            threads.extend(spawned.ok());
        }
        Player { shared, threads }
    }

    /// Stops playback and waits until the decoding threads have closed the
    /// file, so it can be renamed, moved or deleted. Blocks (each thread
    /// notices the stop within one read): call it off the UI thread.
    pub fn close(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for thread in std::mem::take(&mut self.threads) {
            let _ = thread.join();
        }
    }

    pub fn toggle(&self) {
        let s = &self.shared;
        if s.ended.swap(false, Ordering::SeqCst) {
            self.seek(0);
            s.paused.store(false, Ordering::SeqCst);
            return;
        }
        s.paused.fetch_xor(true, Ordering::SeqCst);
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::SeqCst) || self.shared.ended.load(Ordering::SeqCst)
    }

    pub fn seek(&self, to_hns: u64) {
        let s = &self.shared;
        let duration = s.duration.load(Ordering::SeqCst);
        let to = if duration > 0 { to_hns.min(duration.saturating_sub(HNS_PER_SEC / 10)) } else { to_hns };
        s.ended.store(false, Ordering::SeqCst);
        s.seek_to.store(to, Ordering::SeqCst);
        s.position.store(to, Ordering::SeqCst);
        s.seek_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Moves by `seconds` (negative: back).
    pub fn skip(&self, seconds: i64) {
        self.seek(self.position().saturating_add_signed(seconds.saturating_mul(HNS_PER_SEC.cast_signed())));
    }

    pub fn position(&self) -> u64 {
        self.shared.position.load(Ordering::SeqCst)
    }

    pub fn duration(&self) -> u64 {
        self.shared.duration.load(Ordering::SeqCst)
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.clamp(0., 1.).to_bits(), Ordering::SeqCst);
    }

    pub fn is_video(&self) -> bool {
        self.shared.has_video.load(Ordering::SeqCst) == YES
    }

    pub fn frame(&self) -> Option<Arc<RenderImage>> {
        self.shared.frame.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// "1:05" / "1:02:05".
pub fn clock(hns: u64) -> String {
    let s = hns / HNS_PER_SEC;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "durations are far below 2^52 hns (14 years), so exact as f64; \
              the fraction is clamped to 0..=1, so the product is between 0 and `duration`"
)]
pub fn hns_from_fraction(fraction: f32, duration: u64) -> u64 {
    (f64::from(fraction.clamp(0., 1.)) * duration as f64) as u64
}

/// macOS / Linux: music through rodio's decoders (MP3, FLAC, WAV, Ogg,
/// M4A/AAC and more). Video files report that they can't play here yet.
#[cfg(not(windows))]
mod sound {
    use super::*;
    use rodio::{Decoder, OutputStreamBuilder, Sink, Source};

    pub fn run(path: &str, shared: &Shared) -> Result<(), String> {
        let open = || -> Result<Decoder<std::io::BufReader<std::fs::File>>, String> {
            let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            Decoder::new(std::io::BufReader::new(file))
                .map_err(|_| "Video plays on Windows only for now; music works here.".to_string())
        };
        let source = open()?;
        if let Some(total) = source.total_duration() {
            shared.duration.store(hns(total), Ordering::SeqCst);
        }
        let mut output = OutputStreamBuilder::open_default_stream().map_err(|e| e.to_string())?;
        output.log_on_drop(false);
        let sink = Sink::connect_new(output.mixer());
        sink.pause();
        sink.append(source);
        shared.has_audio.store(YES, Ordering::SeqCst);

        let mut seen = 0u64;
        while !shared.stop.load(Ordering::SeqCst) {
            let generation = shared.seek_generation.load(Ordering::SeqCst);
            if generation != seen {
                seen = generation;
                let to = Duration::from_nanos(shared.seek_to.load(Ordering::SeqCst) * 100);
                if sink.empty() {
                    sink.append(open()?);
                }
                let _ = sink.try_seek(to);
            }
            let paused = shared.paused.load(Ordering::SeqCst) || shared.ended.load(Ordering::SeqCst);
            if paused != sink.is_paused() {
                if paused {
                    sink.pause()
                } else {
                    sink.play()
                }
            }
            sink.set_volume(f32::from_bits(shared.volume.load(Ordering::SeqCst)));
            shared.position.store(hns(sink.get_pos()), Ordering::SeqCst);
            if sink.empty() && !paused {
                shared.ended.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        sink.stop();
        Ok(())
    }
}

#[cfg(windows)]
mod mf {
    use super::*;
    use std::sync::Once;

    use rodio::{buffer::SamplesBuffer, OutputStreamBuilder, Sink};
    use windows::core::{HSTRING, PROPVARIANT};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    fn start() {
        static ONCE: Once = Once::new();
        // SAFETY: no pointers are passed; Media Foundation starts once per
        // process and is never shut down, so it outlives every reader.
        ONCE.call_once(|| unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        });
        // SAFETY: no reserved pointer is passed; this worker thread keeps COM
        // (MTA) until it exits.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "it's the `map_err` callback, which hands the error over by value"
    )]
    fn err(e: windows::core::Error) -> String {
        e.message().clone()
    }

    /// A reader with only the first `stream` (audio or video) selected.
    fn reader(path: &str, stream: MF_SOURCE_READER_CONSTANTS) -> Result<IMFSourceReader, String> {
        start();
        let mut attributes: Option<IMFAttributes> = None;
        // SAFETY: COM is initialized on this thread (`start`); `attributes`
        // is an out-param local.
        unsafe { MFCreateAttributes(&raw mut attributes, 1) }.map_err(err)?;
        let attributes = attributes.ok_or("attributes")?;
        // SAFETY: `attributes` is an owned, reference-counted wrapper.
        unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }.map_err(err)?;
        // SAFETY: the HSTRING outlives the call and `attributes` is live.
        let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path), &attributes) }.map_err(err)?;
        // SAFETY: `reader` is an owned, reference-counted wrapper.
        unsafe { reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0.cast_unsigned(), false) }.map_err(err)?;
        // SAFETY: `reader` is an owned, reference-counted wrapper.
        unsafe { reader.SetStreamSelection(stream.0.cast_unsigned(), true) }.map_err(err)?;
        Ok(reader)
    }

    fn duration(reader: &IMFSourceReader) -> u64 {
        // SAFETY: `reader` is a live source reader borrowed for the call.
        unsafe {
            reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0.cast_unsigned(), &MF_PD_DURATION)
                .ok()
                .and_then(|v| u64::try_from(&v).ok())
                .unwrap_or(0)
        }
    }

    fn seek(reader: &IMFSourceReader, to: u64) {
        // SAFETY: `reader` is live; the GUID and PROPVARIANT are temporaries
        // that outlive the call (GUID_NULL means 100 ns units).
        unsafe {
            let _ = reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &PROPVARIANT::from(to.cast_signed()));
        }
    }

    /// The next sample's bytes and time, or `None` at the end (or once
    /// `stop` is set). A decoding error ends the stream with an error.
    fn read(
        reader: &IMFSourceReader,
        stream: MF_SOURCE_READER_CONSTANTS,
        stop: &AtomicBool,
    ) -> Result<Option<(Vec<u8>, u64)>, String> {
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let (mut flags, mut time, mut sample) = (0u32, 0i64, None);
            // SAFETY: synchronous ReadSample with out-pointers to locals that
            // outlive the call.
            unsafe {
                reader.ReadSample(
                    stream.0.cast_unsigned(),
                    0,
                    None,
                    Some(&raw mut flags),
                    Some(&raw mut time),
                    Some(&raw mut sample),
                )
            }
            .map_err(err)?;
            if flags & MF_SOURCE_READERF_ERROR.0.cast_unsigned() != 0 {
                return Err("This file couldn't be decoded.".into());
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0.cast_unsigned() != 0 {
                return Ok(None);
            }
            let Some(sample) = sample else { continue };
            // SAFETY: `sample` is an owned, reference-counted wrapper.
            let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(err)?;
            let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
            // SAFETY: out-pointers to locals; unlocked below once the bytes
            // are copied.
            unsafe { buffer.Lock(&raw mut data, None, Some(&raw mut len)) }.map_err(err)?;
            let len = usize::try_from(len).unwrap_or(0);
            // SAFETY: while the buffer is locked, `data` points at `len`
            // readable bytes it owns.
            let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
            // SAFETY: pairs the successful Lock above; `bytes` is a copy.
            let _ = unsafe { buffer.Unlock() };
            return Ok(Some((bytes, u64::try_from(time).unwrap_or(0))));
        }
    }

    pub fn run_audio(path: &str, shared: &Shared) -> Result<(), String> {
        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM;
        let reader = reader(path, stream)?;
        // SAFETY: COM is initialized on this thread (`reader` called `start`).
        let wanted = unsafe { MFCreateMediaType() }.map_err(err)?;
        // SAFETY: `wanted` is an owned wrapper; the GUIDs are statics.
        unsafe { wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio) }.map_err(err)?;
        // SAFETY: `wanted` is an owned wrapper; the GUIDs are statics.
        unsafe { wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float) }.map_err(err)?;
        // SAFETY: `reader` and `wanted` are live; no reserved pointer is passed.
        unsafe { reader.SetCurrentMediaType(stream.0.cast_unsigned(), None, &wanted) }.map_err(err)?;
        // SAFETY: `reader` is live.
        let actual = unsafe { reader.GetCurrentMediaType(stream.0.cast_unsigned()) }.map_err(err)?;
        // SAFETY: `actual` is an owned wrapper; the GUID is a static.
        let channels = unsafe { actual.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }.map_err(err)?;
        let channels = u16::try_from(channels).map_err(|_| "This file's sound has too many channels.")?;
        // SAFETY: `actual` is an owned wrapper; the GUID is a static.
        let rate = unsafe { actual.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }.map_err(err)?;
        let mut output = OutputStreamBuilder::open_default_stream().map_err(|e| e.to_string())?;
        output.log_on_drop(false);
        shared.duration.fetch_max(duration(&reader), Ordering::SeqCst);
        shared.has_audio.store(YES, Ordering::SeqCst);

        let new_sink = || {
            let sink = Sink::connect_new(output.mixer());
            sink.pause();
            sink
        };
        let mut sink = new_sink();
        let (mut base, mut seen, mut at_end) = (0u64, 0u64, false);
        // rodio's position restarts with each queued buffer, so count finished ones.
        let (mut queued, mut played) = (std::collections::VecDeque::<u64>::new(), 0u64);
        while !shared.stop.load(Ordering::SeqCst) {
            let generation = shared.seek_generation.load(Ordering::SeqCst);
            if generation != seen {
                seen = generation;
                base = shared.seek_to.load(Ordering::SeqCst);
                seek(&reader, base);
                sink.stop();
                sink = new_sink();
                at_end = false;
                (queued, played) = (std::collections::VecDeque::default(), 0);
            }
            let paused = shared.paused.load(Ordering::SeqCst) || shared.ended.load(Ordering::SeqCst);
            if paused != sink.is_paused() {
                if paused {
                    sink.pause();
                } else {
                    sink.play();
                }
            }
            sink.set_volume(f32::from_bits(shared.volume.load(Ordering::SeqCst)));
            while queued.len() > sink.len() {
                played += queued.pop_front().unwrap_or(0);
            }
            let current = if queued.is_empty() { 0 } else { hns(sink.get_pos()) };
            shared.position.store(base + played + current, Ordering::SeqCst);
            // Keep about a second queued.
            if at_end || sink.len() >= 24 {
                if at_end && sink.empty() {
                    shared.ended.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            match read(&reader, stream, &shared.stop)? {
                None => at_end = true,
                Some((bytes, _)) => {
                    let samples: Vec<f32> =
                        bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
                    if !samples.is_empty() {
                        let frames = u64::try_from(samples.len() / usize::from(channels.max(1))).unwrap_or(0);
                        queued.push_back(frames * HNS_PER_SEC / u64::from(rate.max(1)));
                        sink.append(SamplesBuffer::new(channels, rate, samples));
                    }
                }
            }
        }
        sink.stop();
        Ok(())
    }

    pub fn run_video(path: &str, shared: &Shared) -> Result<(), String> {
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM;
        let reader = reader(path, stream)?;
        // SAFETY: COM is initialized on this thread (`reader` called
        // `start`), and `reader` is live.
        let native = unsafe { reader.GetNativeMediaType(stream.0.cast_unsigned(), 0) }.map_err(err)?;
        // SAFETY: `native` is an owned wrapper; the GUID is a static.
        let size = unsafe { native.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(err)?;
        // The two halves of the u64 always fit.
        let (w, h) = (u32::try_from(size >> 32).unwrap_or(0), u32::try_from(size & 0xFFFF_FFFF).unwrap_or(0));
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            reason = "video frame sides are far below 2^24 px, so exact as f32"
        )]
        let scale = (MAX_FRAME.0 as f32 / w as f32).min(MAX_FRAME.1 as f32 / h as f32).min(1.);
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "frame sides are far below 2^24 px (exact as f32); scale is in 0..=1, \
                      so each side is between 0 and the native size"
        )]
        let fit = (((w as f32 * scale) as u32) & !1, ((h as f32 * scale) as u32) & !1);
        // SAFETY: COM is initialized on this thread.
        let wanted = unsafe { MFCreateMediaType() }.map_err(err)?;
        // SAFETY: `wanted` is an owned wrapper; the GUIDs are statics.
        unsafe { wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }.map_err(err)?;
        // SAFETY: `wanted` is an owned wrapper; the GUIDs are statics.
        unsafe { wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }.map_err(err)?;
        // SAFETY: `wanted` is an owned wrapper; the GUID is a static.
        unsafe { wanted.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(fit.0) << 32) | u64::from(fit.1)) }.map_err(err)?;
        // SAFETY: `reader` and `wanted` are live; no reserved pointer is passed.
        if unsafe { reader.SetCurrentMediaType(stream.0.cast_unsigned(), None, &wanted) }.is_err() {
            // No scaling available: take it at its own size.
            // SAFETY: `wanted` is an owned wrapper; the GUID is a static.
            let _ = unsafe { wanted.DeleteItem(&MF_MT_FRAME_SIZE) };
            // SAFETY: `reader` and `wanted` are live; no reserved pointer is passed.
            unsafe { reader.SetCurrentMediaType(stream.0.cast_unsigned(), None, &wanted) }.map_err(err)?;
        }
        // SAFETY: `reader` is live.
        let actual = unsafe { reader.GetCurrentMediaType(stream.0.cast_unsigned()) }.map_err(err)?;
        // SAFETY: `actual` is an owned wrapper; the GUID is a static.
        let size = unsafe { actual.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(err)?;
        // Bytes per row, negative for bottom-up (so stored as u32); not
        // every decoder says.
        // SAFETY: `actual` is an owned wrapper; the GUID is a static.
        let stride = unsafe { actual.GetUINT32(&MF_MT_DEFAULT_STRIDE) }.ok().map(u32::cast_signed);
        let (width, height) = (u32::try_from(size >> 32).unwrap_or(0), u32::try_from(size & 0xFFFF_FFFF).unwrap_or(0));
        shared.duration.fetch_max(duration(&reader), Ordering::SeqCst);
        shared.has_video.store(YES, Ordering::SeqCst);

        // Silent video follows the wall clock; wait to learn whether there's sound.
        while shared.has_audio.load(Ordering::SeqCst) == UNKNOWN && !shared.stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let silent = shared.has_audio.load(Ordering::SeqCst) == NO;
        let (mut wall_base, mut wall_started): (u64, Option<Instant>) = (0, None);

        let (mut seen, mut skip_until, mut show_next) = (0u64, 0u64, true);
        let mut pending: Option<(u64, Arc<RenderImage>)> = None;
        let mut at_end = false;
        while !shared.stop.load(Ordering::SeqCst) {
            let generation = shared.seek_generation.load(Ordering::SeqCst);
            if generation != seen {
                seen = generation;
                skip_until = shared.seek_to.load(Ordering::SeqCst);
                seek(&reader, skip_until);
                (pending, show_next, at_end) = (None, true, false);
                (wall_base, wall_started) = (skip_until, None);
            }
            let paused = shared.paused.load(Ordering::SeqCst) || shared.ended.load(Ordering::SeqCst);
            let now = if silent {
                match (paused, wall_started) {
                    (true, Some(t)) => {
                        wall_base += hns(t.elapsed());
                        wall_started = None;
                    }
                    (false, None) => wall_started = Some(Instant::now()),
                    _ => {}
                }
                let now = wall_base + wall_started.map_or(0, |t| hns(t.elapsed()));
                shared.position.store(now, Ordering::SeqCst);
                now
            } else {
                shared.position.load(Ordering::SeqCst)
            };
            if pending.is_none() && !at_end {
                match read(&reader, stream, &shared.stop)? {
                    None => at_end = true,
                    Some((bytes, time)) if time + HNS_PER_SEC / 30 >= skip_until => {
                        pending = Some((time, Arc::new(frame(bytes, width, height, stride))));
                    }
                    Some(_) => continue,
                }
            }
            if at_end && pending.is_none() {
                if silent && !paused {
                    shared.ended.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if let Some((time, _)) = &pending {
                if show_next || (!paused && *time <= now) {
                    let (_, image) = pending.take().expect("`pending` was just matched as Some");
                    *shared.frame.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(image);
                    show_next = false;
                } else {
                    let wait = if paused { 15 } else { ((*time - now) / 10_000).clamp(1, 15) };
                    std::thread::sleep(Duration::from_millis(wait));
                }
            }
        }
        Ok(())
    }

    /// Media Foundation's RGB32 is BGRX, which is what GPUI draws (BGRA).
    fn frame(bytes: Vec<u8>, width: u32, height: u32, stride: Option<i32>) -> RenderImage {
        let mut bytes = tight_rows(bytes, width, height, stride);
        for px in bytes.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        let buffer = image::RgbaImage::from_raw(width, height, bytes).expect("frame size");
        RenderImage::new(vec![image::Frame::new(buffer)])
    }

    /// Exactly `width * 4 * height` bytes, top row first: padding dropped
    /// using the media type's stride (negative: bottom-up), or guessed from
    /// the buffer's length when the type has none (or one that doesn't fit).
    pub(super) fn tight_rows(mut bytes: Vec<u8>, width: u32, height: u32, stride: Option<i32>) -> Vec<u8> {
        // u32 always fits in usize on the platforms Dash runs on.
        let (row, rows) = (usize::try_from(width).unwrap_or(0) * 4, usize::try_from(height).unwrap_or(0));
        let need = row * rows;
        let pitch = stride.map_or(0, |s| usize::try_from(s.unsigned_abs()).unwrap_or(0));
        if rows > 0 && pitch >= row && bytes.len() >= pitch * (rows - 1) + row {
            let bottom_up = stride.is_some_and(|s| s < 0);
            let mut out = Vec::with_capacity(need);
            for y in 0..rows {
                let at = if bottom_up { rows - 1 - y } else { y } * pitch;
                out.extend_from_slice(&bytes[at..at + row]);
            }
            return out;
        }
        if bytes.len() > need && height > 0 {
            // Rows padded to a stride: drop the padding.
            let stride = bytes.len() / rows;
            bytes = bytes.chunks(stride).take(rows).flat_map(|r| r[..row.min(r.len())].to_vec()).collect();
        }
        bytes.resize(need, 0);
        bytes
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn wait(what: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn frame_rows_follow_the_stride() {
        // 1x2 pixels, rows padded to 8 bytes.
        let buf: Vec<u8> = vec![1, 1, 1, 1, 0, 0, 0, 0, 2, 2, 2, 2, 0, 0, 0, 0];
        assert_eq!(mf::tight_rows(buf.clone(), 1, 2, Some(8)), vec![1, 1, 1, 1, 2, 2, 2, 2]);
        // Bottom-up: the last row in memory is the top one.
        assert_eq!(mf::tight_rows(buf.clone(), 1, 2, Some(-8)), vec![2, 2, 2, 2, 1, 1, 1, 1]);
        // No stride (or one that doesn't fit): guessed from the length.
        assert_eq!(mf::tight_rows(buf.clone(), 1, 2, None), vec![1, 1, 1, 1, 2, 2, 2, 2]);
        assert_eq!(mf::tight_rows(buf, 1, 2, Some(64)), vec![1, 1, 1, 1, 2, 2, 2, 2]);
        // Short buffer: padded with zeros, never a panic.
        assert_eq!(mf::tight_rows(vec![9; 4], 1, 2, Some(4)), vec![9, 9, 9, 9, 0, 0, 0, 0]);
    }

    /// `close` returns once the threads are done (here: a file that fails
    /// to open, so they end at once).
    #[test]
    fn close_joins_the_threads() {
        let p = Player::open(r"C:\definitely\not\here.mp4");
        p.close();
    }

    /// Plays real files, muted: `DASH_TEST_VIDEO=<file> cargo test -p dash -- --ignored`.
    #[test]
    #[ignore = "needs a real file in DASH_TEST_VIDEO and plays it"]
    fn plays_video_and_sound() {
        if let Ok(path) = std::env::var("DASH_TEST_VIDEO") {
            let p = Player::open(&path);
            p.set_volume(0.);
            assert!(wait(|| p.frame().is_some()), "first frame: {:?}", p.error());
            assert!(p.is_video() && p.duration() > 0);
            eprintln!("video has sound: {}", p.shared.has_audio.load(Ordering::SeqCst) == YES);
            let first = p.frame().unwrap().id;
            p.toggle();
            assert!(wait(|| p.position() > HNS_PER_SEC / 2), "clock moves");
            assert_ne!(p.frame().unwrap().id, first, "frames advance");
            p.seek(0);
            assert!(wait(|| p.position() < HNS_PER_SEC / 2));
        }
        let wav = r"C:\Windows\Media\Alarm01.wav";
        let p = Player::open(wav);
        p.set_volume(0.);
        assert!(wait(|| p.duration() > 0), "duration: {:?}", p.error());
        assert!(wait(|| p.shared.has_video.load(Ordering::SeqCst) == NO));
        p.toggle();
        assert!(wait(|| p.position() > HNS_PER_SEC / 2), "sound clock moves");
    }
}
