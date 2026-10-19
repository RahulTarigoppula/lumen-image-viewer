//! Jobs: run a metric set on an image (A, optional B) or on a stream of video
//! frames, on a dedicated below-normal-priority rayon pool, with
//! cancellation, progress, a generation counter and a memory budget.
//!
//! Two layers:
//! - [`run_image`] / [`run_video`]: blocking functions on borrowed or
//!   caller-owned frames; they stream [`JobEvent`]s to a sink and return the
//!   final [`AnalysisReport`].
//! - [`Analyzer`]: owns the pool and runs those on a job thread, returning a
//!   [`JobHandle`] the UI polls each frame. A new submission supersedes
//!   (cancels) the previous one of the same kind.

use crate::frame::{FrameCtx, FrameSource, FrameView, InputKind, OwnedFrame, Roi};
use crate::metrics::temporal::{self, TemporalFrame, TemporalMetric};
use crate::metrics::{self, Metric, MetricError, MetricKind, MetricOptions, MetricResult, Value};
use crate::report::{AnalysisReport, ReportBuilder, Scope, ScopeKind};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared cancellation flag, checked per frame and per strip / band.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> Result<(), MetricError> {
        if self.is_cancelled() {
            Err(MetricError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Progress counters; the UI polls them each repaint.
#[derive(Debug)]
pub struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    /// Estimated work in microseconds (image jobs: each metric's cost
    /// estimate), so the fraction and ETA follow the slow metrics rather
    /// than the count; 0 when the units are all alike (video frames).
    work_done: AtomicU64,
    work_total: AtomicU64,
    started: Instant,
}

impl Default for Progress {
    fn default() -> Self {
        Progress { done: AtomicU64::new(0), total: AtomicU64::new(0), work_done: AtomicU64::new(0), work_total: AtomicU64::new(0), started: Instant::now() }
    }
}

impl Progress {
    /// Units finished (metrics for image jobs, frames for video jobs).
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Total units, 0 when unknown.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn fraction(&self) -> Option<f32> {
        let (d, t) = self.work();
        (t > 0).then(|| (d as f32 / t as f32).min(1.0))
    }

    /// (done, total) in the weighted units when there are any, else in units.
    fn work(&self) -> (u64, u64) {
        match self.work_total.load(Ordering::Relaxed) {
            0 => (self.done(), self.total()),
            t => (self.work_done.load(Ordering::Relaxed), t),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Remaining time extrapolated from the rate so far; before the first
    /// unit finishes, the cost estimate of weighted jobs.
    pub fn eta(&self) -> Option<Duration> {
        let (d, t) = self.work();
        if t == 0 || d >= t {
            return None;
        }
        if d == 0 {
            let est = Duration::from_micros(self.work_total.load(Ordering::Relaxed));
            return est.checked_sub(self.elapsed()).filter(|r| !r.is_zero());
        }
        Some(self.elapsed().mul_f64((t - d) as f64 / d as f64))
    }

    fn set_total(&self, t: u64) {
        self.total.store(t, Ordering::Relaxed);
    }

    /// Weighted units (see `work_total`) of the whole job.
    fn set_work(&self, t: u64) {
        self.work_total.store(t, Ordering::Relaxed);
    }

    fn step(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    fn step_work(&self, w: u64) {
        self.work_done.fetch_add(w, Ordering::Relaxed);
        self.step();
    }
}

/// Cancellation and progress of one job.
#[derive(Clone, Debug, Default)]
pub struct JobControl {
    pub cancel: CancelToken,
    pub progress: Arc<Progress>,
}

/// One frame of a video job: values keyed by full column name (`"psnr.y"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameResult {
    pub idx: u64,
    pub values: Vec<Value>,
}

/// Events streamed by a job.
#[derive(Debug)]
pub enum JobEvent {
    /// Preview result of a heavy metric (1/4 area); a `Metric` event follows.
    Partial(MetricResult),
    /// Final result of one metric of an image job (completion order).
    Metric(MetricResult),
    /// One frame of a video job, in frame order.
    Row(FrameResult),
    /// Terminal: the finished report.
    Done(Box<AnalysisReport>),
    /// Terminal: the job could not run.
    Failed(String),
    /// Terminal: cancelled (or superseded).
    Cancelled,
}

impl JobEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, JobEvent::Done(_) | JobEvent::Failed(_) | JobEvent::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobError {
    Cancelled,
    Invalid(String),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Cancelled => write!(f, "cancelled"),
            JobError::Invalid(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for JobError {}

/// What to run on an image (or a single video frame).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageSpec {
    /// Metric ids (frame metrics; temporal ids are ignored for images).
    pub metrics: Vec<String>,
    pub roi: Option<Roi>,
    pub options: MetricOptions,
    /// Run heavy metrics on a 1/4-area copy first and emit `Partial` events.
    pub preview_first: bool,
    /// File names, "rescaled" and notes for the report; size, ROI and kind are filled in.
    pub scope: Scope,
}

impl Default for ImageSpec {
    fn default() -> Self {
        ImageSpec { metrics: Vec::new(), roi: None, options: MetricOptions::default(), preview_first: true, scope: Scope::default() }
    }
}

/// What to run on a stream of video frames.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSpec {
    /// Frame metric and temporal metric ids.
    pub metrics: Vec<String>,
    pub roi: Option<Roi>,
    /// `want_map` defaults to false here: maps are not kept per frame.
    pub options: MetricOptions,
    /// Bytes for frames in flight plus their derived images (default 1 GB).
    pub memory_budget: u64,
    /// Upper bound on frames in flight (default: the pool size plus 2).
    pub max_in_flight: Option<usize>,
    /// Expected frame count, for progress and ETA.
    pub total_frames: Option<u64>,
    pub scope: Scope,
}

impl Default for VideoSpec {
    fn default() -> Self {
        VideoSpec {
            metrics: Vec::new(),
            roi: None,
            options: MetricOptions { want_map: false, ..MetricOptions::default() },
            memory_budget: 1 << 30,
            max_in_flight: None,
            total_frames: None,
            scope: Scope { kind: ScopeKind::VideoRange, ..Scope::default() },
        }
    }
}

/// Lowers the calling thread to the lowest normal-class priority (Windows;
/// no-op elsewhere), so decode, thumbnails and the UI thread win the cores
/// whenever they need them: with every core busy measuring, below-normal
/// alone still left UI frames of 30 ms and more.
pub fn lower_current_thread_priority() {
    #[cfg(windows)]
    // SAFETY: plain Win32 call on the current thread's pseudo handle.
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
}

/// The dedicated analysis thread pool (cores - 1 threads, lowest priority).
pub struct AnalysisPool {
    pool: rayon::ThreadPool,
}

impl AnalysisPool {
    /// `threads`: `None` for `cores - 1` (at least 1).
    pub fn new(threads: Option<usize>) -> Self {
        let n = threads.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1)).max(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("lumen-analysis-{i}"))
            .start_handler(|_| lower_current_thread_priority())
            .build()
            .expect("analysis thread pool");
        AnalysisPool { pool }
    }

    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Runs `f` inside the pool (its rayon calls use the pool's threads).
    pub fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        self.pool.install(f)
    }
}

/// Splits ids into frame metrics and temporal metric infos; unknown ids are an error.
fn resolve(ids: &[String]) -> Result<(Vec<&'static dyn Metric>, Vec<&'static temporal::TemporalInfo>), JobError> {
    let (mut fm, mut tm) = (Vec::new(), Vec::new());
    for id in ids {
        if let Some(m) = metrics::get(id) {
            fm.push(m);
        } else if let Some(t) = temporal::get(id) {
            tm.push(t);
        } else {
            return Err(JobError::Invalid(format!("unknown metric '{id}'")));
        }
    }
    Ok((fm, tm))
}

/// Crops A (and B, with A's snapped ROI) when a ROI is set.
fn crop_pair<'a>(a: &FrameView<'a>, b: Option<&FrameView<'a>>, roi: Option<Roi>) -> Result<(FrameView<'a>, Option<FrameView<'a>>, Option<Roi>), JobError> {
    match roi {
        None => Ok((*a, b.copied(), None)),
        Some(r) => {
            let (va, used) = a.crop(r).map_err(|e| JobError::Invalid(e.to_string()))?;
            let vb = match b {
                Some(b) => Some(b.crop(used).map_err(|e| JobError::Invalid(e.to_string()))?.0),
                None => None,
            };
            Ok((va, vb, Some(used)))
        }
    }
}

/// Moves a map from ROI space to frame space (and scales preview maps).
fn place_map(r: &mut MetricResult, roi: Option<Roi>, scale: f32) {
    if let Some(m) = &mut r.map {
        m.x0 *= scale;
        m.y0 *= scale;
        m.cell_w *= scale;
        m.cell_h *= scale;
        if let Some(roi) = roi {
            m.x0 += roi.x as f32;
            m.y0 += roi.y as f32;
        }
    }
}

/// Inputs B needs: those of full-reference metrics only.
fn inputs_b(ms: &[&'static dyn Metric]) -> Vec<InputKind> {
    let fr: Vec<&dyn Metric> = ms.iter().copied().filter(|m| m.info().kind == MetricKind::FullReference).collect();
    metrics::inputs_for(&fr)
}

fn run_one(m: &dyn Metric, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
    let t = Instant::now();
    let b = if m.info().kind == MetricKind::FullReference { b } else { None };
    let mut r = m.compute(a, b, o, cancel)?;
    r.elapsed_ms = t.elapsed().as_secs_f64() * 1000.0;
    Ok(r)
}

/// Runs `spec` on A (and B) and returns the report; `sink` receives
/// `Partial` and `Metric` events as results arrive (never terminal events).
pub fn run_image(pool: &AnalysisPool, spec: &ImageSpec, a: &FrameView<'_>, b: Option<&FrameView<'_>>, ctl: &JobControl, sink: &(dyn Fn(JobEvent) + Sync)) -> Result<AnalysisReport, JobError> {
    let (ms, tm) = resolve(&spec.metrics)?;
    let (va, vb, roi) = crop_pair(a, b, spec.roi)?;
    let mut scope = spec.scope.clone();
    if scope.kind == ScopeKind::VideoRange {
        scope.kind = ScopeKind::VideoFrame;
    }
    scope.width = a.width() as u32;
    scope.height = a.height() as u32;
    scope.roi = roi;
    let mut builder = ReportBuilder::new(scope);
    for t in &tm {
        builder.note(format!("{}: temporal metrics need a frame sequence; skipped", t.id));
    }
    ctl.progress.set_total(ms.len() as u64);
    // At least 1 us each, so even metrics of tiny regions count.
    let work = |m: &dyn Metric| ((m.info().cost_ms(va.width() as u32, va.height() as u32) * 1000.0) as u64).max(1);
    ctl.progress.set_work(ms.iter().map(|m| work(*m)).sum());
    let cancel = &ctl.cancel;
    let opts = &spec.options;
    let results: Vec<(&'static dyn Metric, Result<MetricResult, MetricError>)> = pool.install(|| {
        let ca = FrameCtx::new(va);
        let cb = vb.map(FrameCtx::new);
        let (ia, ib) = (metrics::inputs_for(&ms), inputs_b(&ms));
        rayon::join(|| ca.prepare(&ia), || cb.as_ref().map(|c| c.prepare(&ib)));
        // Heavy metrics: a quick look on a 1/4-area copy first.
        let heavy: Vec<&dyn Metric> = ms.iter().copied().filter(|m| m.info().heavy).collect();
        if spec.preview_first && !heavy.is_empty() && va.width() * va.height() >= 1 << 20 {
            let pa = OwnedFrame::downsample2(&va);
            let pb = vb.as_ref().map(OwnedFrame::downsample2);
            let ca2 = FrameCtx::new(pa.view());
            let cb2 = pb.as_ref().map(|p| FrameCtx::new(p.view()));
            ca2.prepare(&metrics::inputs_for(&heavy));
            if let Some(c) = &cb2 {
                c.prepare(&inputs_b(&heavy));
            }
            use rayon::prelude::*;
            heavy.par_iter().for_each(|m| {
                if let Ok(mut r) = run_one(*m, &ca2, cb2.as_ref(), opts, cancel) {
                    r.preview = true;
                    place_map(&mut r, roi, 2.0);
                    sink(JobEvent::Partial(r));
                }
            });
        }
        use rayon::prelude::*;
        ms.par_iter()
            .map(|m| {
                let mut r = run_one(*m, &ca, cb.as_ref(), opts, cancel);
                if let Ok(r) = &mut r {
                    place_map(r, roi, 1.0);
                    sink(JobEvent::Metric(r.clone()));
                }
                ctl.progress.step_work(work(*m));
                (*m, r)
            })
            .collect()
    });
    if cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    for (m, r) in results {
        match r {
            Ok(r) => builder.add_metric(m.info(), r),
            Err(MetricError::Cancelled) => return Err(JobError::Cancelled),
            Err(e) => builder.note(format!("{}: skipped ({e})", m.id())),
        }
    }
    Ok(builder.finish(None))
}

/// Per-frame output of a worker.
struct Done {
    seq: u64,
    idx: u64,
    results: Vec<(&'static str, Result<MetricResult, MetricError>)>,
    ta: Option<TemporalFrame>,
    tb: Option<TemporalFrame>,
}

/// Temporal metrics of one stream (A or B) with their previous frame.
struct TemporalStream {
    metrics: Vec<Box<dyn TemporalMetric>>,
    prev: Option<TemporalFrame>,
    suffix: &'static str,
}

impl TemporalStream {
    fn push(&mut self, idx: u64, cur: TemporalFrame, out: &mut Vec<Value>) {
        for m in &mut self.metrics {
            for mut v in m.push(idx, &cur, self.prev.as_ref()) {
                v.key = format!("{}.{}{}", m.id(), v.key, self.suffix);
                out.push(v);
            }
        }
        self.prev = Some(cur);
    }
}

/// Runs `spec` over `frames` = (frame index, A, optional B) in the order
/// given. Frames are processed in parallel on the pool (each frame's metric
/// set runs on one task) with a reorder buffer; temporal metrics run in
/// order on the calling thread. At most `budget / bytes-per-frame-pair`
/// frames are in flight. `sink` receives one `Row` per frame in order.
pub fn run_video<I, F>(pool: &AnalysisPool, spec: &VideoSpec, frames: I, ctl: &JobControl, sink: &(dyn Fn(JobEvent) + Sync)) -> Result<AnalysisReport, JobError>
where
    I: IntoIterator<Item = (u64, F, Option<F>)>,
    F: FrameSource + 'static,
{
    let (ms, tm) = resolve(&spec.metrics)?;
    let cancel = &ctl.cancel;
    if let Some(t) = spec.total_frames {
        ctl.progress.set_total(t);
    }
    let mut frames = frames.into_iter().peekable();
    let mut scope = spec.scope.clone();
    scope.kind = ScopeKind::VideoRange;
    scope.roi = spec.roi;
    let first = match frames.peek() {
        Some(f) => f,
        None => return Ok(ReportBuilder::new(scope).finish(None)),
    };
    // Size the in-flight window from the first pair.
    let va = first.1.view();
    let (w, h) = va.size();
    scope.width = w as u32;
    scope.height = h as u32;
    let peak = crate::metrics::reference::psnr::peak(&va);
    let has_b = first.2.is_some();
    let mut inputs = metrics::inputs_for(&ms);
    if !tm.is_empty() && !inputs.contains(&InputKind::Luma) {
        inputs.push(InputKind::Luma);
    }
    let (rw, rh) = spec.roi.and_then(|r| r.clamp(w as u32, h as u32)).map(|r| (r.w as usize, r.h as usize)).unwrap_or((w, h));
    // Native planes (16-bit samples for 10-bit video), the derived f32
    // images, about one f32 working image per metric (SSIM windows, MS-SSIM
    // pyramids), maps when asked and the temporal features.
    let px = rw * rh;
    let per_frame = va.byte_size() + FrameCtx::derived_bytes(rw, rh, &inputs) + ms.len() * px * 4 + if spec.options.want_map { px * 4 * ms.len() } else { 0 } + px;
    let per_pair = per_frame * if has_b { 2 } else { 1 };
    let cap = spec.max_in_flight.unwrap_or(pool.threads() + 2).max(1);
    let window = ((spec.memory_budget as usize / per_pair.max(1)).max(1)).min(cap);
    let mut builder = ReportBuilder::new(scope);
    let mut ta = TemporalStream { metrics: tm.iter().map(|t| (t.make)(&spec.options)).collect(), prev: None, suffix: "" };
    let mut tb = TemporalStream { metrics: tm.iter().map(|t| (t.make)(&spec.options)).collect(), prev: None, suffix: "_b" };
    let want_temporal = !tm.is_empty();
    let (tx, rx): (Sender<Done>, Receiver<Done>) = crossbeam_channel::unbounded();
    let roi = spec.roi;
    let opts = spec.options.clone();
    let mut noted: HashSet<String> = HashSet::new();
    let mut cancelled = false;
    pool.pool.in_place_scope(|s| {
        let mut seq = 0u64;
        let mut next = 0u64;
        let mut in_flight = 0usize;
        let mut buf: BTreeMap<u64, Done> = BTreeMap::new();
        let mut exhausted = false;
        loop {
            while !exhausted && !cancelled && in_flight + buf.len() < window {
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
                let Some((idx, fa, fb)) = frames.next() else {
                    exhausted = true;
                    break;
                };
                let (tx, ms, opts, cancel) = (tx.clone(), ms.clone(), opts.clone(), cancel.clone());
                let my = seq;
                seq += 1;
                in_flight += 1;
                s.spawn(move |_| {
                    // Always answer: the loop below waits for every frame it
                    // sent out, so a panicking metric must not leave it
                    // waiting forever (the scope would only rethrow the
                    // panic once every task is done).
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| frame_task(my, idx, &fa, fb.as_ref(), &ms, roi, &opts, &cancel, want_temporal)))
                        .unwrap_or_else(|_| Done { seq: my, idx, results: ms.iter().map(|m| (m.id(), Err(MetricError::NotApplicable("internal error".into())))).collect(), ta: None, tb: None });
                    let _ = tx.send(out);
                });
            }
            if in_flight == 0 {
                break;
            }
            let Ok(d) = rx.recv() else { break };
            in_flight -= 1;
            buf.insert(d.seq, d);
            while let Some(d) = buf.remove(&next) {
                next += 1;
                if cancel.is_cancelled() {
                    cancelled = true;
                    continue;
                }
                let mut values = Vec::new();
                for (id, r) in d.results {
                    match r {
                        Ok(r) => {
                            for mut v in r.values {
                                v.key = format!("{id}.{}", v.key);
                                values.push(v);
                            }
                        }
                        Err(MetricError::Cancelled) => cancelled = true,
                        Err(e) => {
                            if noted.insert(id.to_string()) {
                                builder.note(format!("{id}: skipped on some frames ({e})"));
                            }
                        }
                    }
                }
                // Temporal metrics use rayon too: in the analysis pool, not
                // the process-wide one (this is the caller's thread).
                pool.install(|| {
                    if let Some(t) = d.ta {
                        ta.push(d.idx, t, &mut values);
                    }
                    if let Some(t) = d.tb {
                        tb.push(d.idx, t, &mut values);
                    }
                });
                builder.add_row(d.idx, &values);
                ctl.progress.step();
                sink(JobEvent::Row(FrameResult { idx: d.idx, values }));
            }
        }
    });
    if cancelled || cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    Ok(builder.finish(Some(peak)))
}

#[allow(clippy::too_many_arguments)]
fn frame_task<F: FrameSource>(seq: u64, idx: u64, fa: &F, fb: Option<&F>, ms: &[&'static dyn Metric], roi: Option<Roi>, opts: &MetricOptions, cancel: &CancelToken, temporal: bool) -> Done {
    let mut done = Done { seq, idx, results: Vec::new(), ta: None, tb: None };
    if cancel.is_cancelled() {
        return done;
    }
    let (a, b) = (fa.view(), fb.map(|f| f.view()));
    let (va, vb, used) = match crop_pair(&a, b.as_ref(), roi) {
        Ok(v) => v,
        Err(e) => {
            done.results = ms.iter().map(|m| (m.id(), Err(MetricError::NotApplicable(e.to_string())))).collect();
            return done;
        }
    };
    let ca = FrameCtx::new(va);
    let cb = vb.map(FrameCtx::new);
    let mut ia = metrics::inputs_for(ms);
    if temporal && !ia.contains(&InputKind::Luma) {
        ia.push(InputKind::Luma);
    }
    let mut ib = inputs_b(ms);
    if temporal && !ib.contains(&InputKind::Luma) {
        ib.push(InputKind::Luma);
    }
    ca.prepare(&ia);
    if let Some(c) = &cb {
        c.prepare(&ib);
    }
    for m in ms {
        let mut r = run_one(*m, &ca, cb.as_ref(), opts, cancel);
        if let Ok(r) = &mut r {
            place_map(r, used, 1.0);
        }
        done.results.push((m.id(), r));
    }
    if temporal {
        done.ta = Some(TemporalFrame::from_ctx(&ca));
        done.tb = cb.as_ref().map(TemporalFrame::from_ctx);
    }
    done
}

/// Frames for [`Analyzer::submit_video`]: index, A, optional B.
pub type SharedFrame = Arc<dyn FrameSource>;

/// Handle to a submitted job. Dropping it cancels the job.
pub struct JobHandle {
    pub generation: u64,
    ctl: JobControl,
    rx: Receiver<JobEvent>,
    finished: bool,
}

impl JobHandle {
    pub fn cancel(&self) {
        self.ctl.cancel.cancel();
    }

    pub fn progress(&self) -> &Progress {
        &self.ctl.progress
    }

    /// Next event without blocking.
    pub fn try_recv(&mut self) -> Option<JobEvent> {
        let e = self.rx.try_recv().ok()?;
        self.finished |= e.is_terminal();
        Some(e)
    }

    pub fn recv_timeout(&mut self, d: Duration) -> Option<JobEvent> {
        let e = self.rx.recv_timeout(d).ok()?;
        self.finished |= e.is_terminal();
        Some(e)
    }

    /// True once a terminal event has been received.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Blocks until the job ends; returns the report or the reason it did not finish.
    pub fn wait(self) -> Result<AnalysisReport, String> {
        self.wait_timeout(Duration::MAX)
    }

    /// [`JobHandle::wait`] for at most `limit` without any event; the job is
    /// cancelled when it runs out ("timed out").
    pub fn wait_timeout(mut self, limit: Duration) -> Result<AnalysisReport, String> {
        loop {
            let e = match self.rx.recv_timeout(limit) {
                Ok(e) => e,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => return Err("timed out".into()),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            };
            match e {
                JobEvent::Done(r) => {
                    self.finished = true;
                    return Ok(*r);
                }
                JobEvent::Failed(s) => return Err(s),
                JobEvent::Cancelled => return Err("cancelled".into()),
                _ => {}
            }
        }
        Err("job ended without a result".into())
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.finished {
            self.ctl.cancel.cancel();
        }
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

/// The analysis service: one pool, one job thread per running job, and a
/// generation counter. A new image (or video) job cancels the previous one.
pub struct Analyzer {
    pool: Arc<AnalysisPool>,
    generation: AtomicU64,
    image_lane: Mutex<Option<CancelToken>>,
    video_lane: Mutex<Option<CancelToken>>,
    notify: Option<Notify>,
}

impl Analyzer {
    /// `threads`: pool size (`None` for cores - 1). `notify` is called after
    /// every event (e.g. `move || ctx.request_repaint()`).
    pub fn new(threads: Option<usize>, notify: Option<Notify>) -> Self {
        Analyzer { pool: Arc::new(AnalysisPool::new(threads)), generation: AtomicU64::new(0), image_lane: Mutex::new(None), video_lane: Mutex::new(None), notify }
    }

    pub fn pool(&self) -> &Arc<AnalysisPool> {
        &self.pool
    }

    /// Generation of the most recent submission; events of older handles are stale.
    pub fn latest_generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn start(&self, lane: &Mutex<Option<CancelToken>>) -> (JobControl, u64) {
        let ctl = JobControl::default();
        if let Some(old) = lane.lock().unwrap_or_else(|e| e.into_inner()).replace(ctl.cancel.clone()) {
            old.cancel();
        }
        (ctl, self.generation.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn spawn(&self, ctl: JobControl, generation: u64, body: impl FnOnce(&AnalysisPool, &JobControl, &(dyn Fn(JobEvent) + Sync)) -> Result<AnalysisReport, JobError> + Send + 'static) -> JobHandle {
        let (tx, rx) = crossbeam_channel::unbounded();
        let pool = self.pool.clone();
        let notify = self.notify.clone();
        let c2 = ctl.clone();
        std::thread::Builder::new()
            .name("lumen-analysis-job".into())
            .spawn(move || {
                lower_current_thread_priority();
                let send = |e: JobEvent| {
                    let _ = tx.send(e);
                    if let Some(n) = &notify {
                        n();
                    }
                };
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&pool, &c2, &send)));
                send(match res {
                    Ok(Ok(r)) => JobEvent::Done(Box::new(r)),
                    Ok(Err(JobError::Cancelled)) => JobEvent::Cancelled,
                    Ok(Err(JobError::Invalid(s))) => JobEvent::Failed(s),
                    Err(_) => JobEvent::Failed("internal error in analysis".into()),
                });
            })
            .expect("analysis job thread");
        JobHandle { generation, ctl, rx, finished: false }
    }

    /// Runs an image job on owned / shared frames.
    pub fn submit_image(&self, spec: ImageSpec, a: SharedFrame, b: Option<SharedFrame>) -> JobHandle {
        let (ctl, generation) = self.start(&self.image_lane);
        self.spawn(ctl, generation, move |pool, ctl, sink| {
            let va = a.view();
            let vb = b.as_ref().map(|b| b.view());
            run_image(pool, &spec, &va, vb.as_ref(), ctl, sink)
        })
    }

    /// Runs a video job over `frames` (e.g. a channel receiver's `into_iter()`
    /// fed by the app's decoder threads).
    pub fn submit_video<I>(&self, spec: VideoSpec, frames: I) -> JobHandle
    where
        I: IntoIterator<Item = (u64, SharedFrame, Option<SharedFrame>)> + Send + 'static,
    {
        let (ctl, generation) = self.start(&self.video_lane);
        self.spawn(ctl, generation, move |pool, ctl, sink| run_video(pool, &spec, frames, ctl, sink))
    }
}
