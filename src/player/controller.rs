//! Player controller: owns the mpv instance, stream selection, SponsorBlock, queue and progress.
use super::compressor::{self, Strength};
use super::mpv::{Mpv, MpvEvent, PropValue};
use crate::i18n::{t as translate, tf};
use crate::api::services::{self, Segment};
use crate::api::streams;
use crate::api::types::*;
use crate::store;
use crate::ui::{blocking, toast};
use gpui_kit::*;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct FrameProgress {
    presented: u64,
    position: f64,
    since: Instant,
    min_advance: f64,
}

impl FrameProgress {
    fn new(presented: u64, position: f64) -> Self {
        Self { presented, position, since: Instant::now(), min_advance: 3.0 }
    }
}

pub enum PlayerEvent {
    Ended,
    Loaded,
    PlaybackReadyChanged,
    /// average colour of the picture changed (ambient mode)
    Ambient,
    QueueChanged,
}

#[derive(Clone, Copy)]
pub enum KeyboardFeedback {
    Volume(f64),
    Seek(f64),
}

const SEEK_DISK_CACHE_LIMIT: u64 = 128 * 1024 * 1024;
const BACK_CACHE_SIZE: &str = "12MiB";
const LIVE_EDGE_MARGIN: f64 = 2.0;
const LIVE_AT_EDGE_THRESHOLD: f64 = 15.0;
const LIVE_RELOAD_BEHIND_THRESHOLD: f64 = 30.0;
const LIVE_GO_LIVE_SEEK_MAX_BEHIND: f64 = 120.0;
const LIVE_EDGE_STALE_SECS: u64 = 15;
const LIVE_EDGE_PAUSED_STALE_SECS: u64 = 8;
const LIVE_EOF_RECOVERY_WINDOW: Duration = Duration::from_secs(30);
const LIVE_EOF_MAX_RECOVERIES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
enum LoadTarget {
    LiveEdge,
    LiveBehind(f64),
    VodTime(f64),
}

impl LoadTarget {
    fn from_vod(start: Option<f64>) -> Self {
        match start.filter(|t| t.is_finite() && *t > 0.0) {
            Some(t) => Self::VodTime(t),
            None => Self::VodTime(0.0),
        }
    }
}

#[derive(Default)]
struct LiveEofGuard {
    attempts: Vec<Instant>,
}

impl LiveEofGuard {
    fn clear(&mut self) {
        self.attempts.clear();
    }

    /// Records a live-EOF recovery attempt at `now`. Returns `true` if recovery
    /// is allowed (at most `LIVE_EOF_MAX_RECOVERIES` within `LIVE_EOF_RECOVERY_WINDOW`).
    fn allow(&mut self, now: Instant) -> bool {
        self.attempts
            .retain(|&t| now.saturating_duration_since(t) < LIVE_EOF_RECOVERY_WINDOW);
        if self.attempts.len() >= LIVE_EOF_MAX_RECOVERIES {
            return false;
        }
        self.attempts.push(now);
        true
    }
}

/// The caption margin (mpv's 720-line scale) while the controls are hidden.
pub const CAPTION_MARGIN: u32 = 40;

pub struct Player {
    /// where captions sit above the bottom edge (see [`Player::set_caption_margin`])
    caption_margin: std::cell::Cell<u32>,
    /// speed to go back to while a press-and-hold plays at 2x
    pub boost: Option<f64>,
    mpv: Option<Arc<Mpv>>,
    engine_generation: u64,
    foreground: bool,
    in_pip: bool,
    pub init_error: Option<String>,
    pub video: Option<VideoItem>,
    pub streams: Option<Arc<StreamInfo>>,
    pub loading: bool,
    pub playback_ready: bool,
    pub error: Option<String>,
    pub quality: u32,
    pub audio_only: bool,
    pub time: f64,
    pub duration: f64,
    pub paused: bool,
    pub buffering: bool,
    pub seeking: bool,
    cache_time: f64,
    cache_ranges: Option<Vec<(f64, f64)>>,
    // End of contiguous data from the last seek, and the farthest buffered end to reconnect.
    seek_cache_target: Option<(f64, f64)>,
    seek_disk_bytes: u64,
    preloaded_until: f64,
    pub volume: f64,
    pub muted: bool,
    pub compressor_enabled: bool,
    pub compressor_strength: Strength,
    pub speed: f64,
    keyboard_feedback: Option<(KeyboardFeedback, Task<()>)>,
    pub eof: bool,
    pub frame: Option<Arc<RenderImage>>,
    old_frames: Vec<Arc<RenderImage>>,
    pub video_size: (f64, f64),
    pub segments: Vec<Segment>,
    skipped: HashSet<String>,
    pub sb_prompt: Option<Segment>,
    muted_by_sb: bool,
    pub caption: Option<usize>,
    pub queue: Vec<VideoItem>,
    pub queue_pos: Option<usize>,
    pub(crate) autoplay_next: Option<VideoItem>,
    /// Videos played before the current one in this session, newest last,
    /// for "previous" when there is no queue to step back through.
    played_before: Vec<VideoItem>,
    /// The video "previous" is going back to, whose `play` isn't remembered.
    going_back: Option<String>,
    queue_serial: u64,
    queue_shuffle: bool,
    queue_scroll_y: Option<(u64, Pixels)>,
    pub chapters: Vec<Chapter>,
    pub heatmap: Vec<(f64, f64, f32)>,
    /// archived annotations of the current video
    pub annotations: Arc<Vec<crate::api::annotations::Annotation>>,
    /// average colour of recent frames, for the ambient glow
    pub ambient: Option<Rgba>,
    ambient_tick: u32,
    /// black bars found in recent frames, for the "Auto" aspect mode
    pub auto_crop: super::aspect::AutoCrop,
    pub loop_video: bool,
    load_serial: u64,
    last_progress_save: f64,
    history_report: Option<crate::api::web::PlaybackReport>,
    pub storyboard: Option<Storyboard>,
    last_prop_notify: std::time::Instant,
    fallback_tried: bool,
    /// times a live stream reached its end and was joined again (#264)
    live_rejoins: u32,
    frame_progress: Option<FrameProgress>,
    hold_picture: Option<u64>,
    playback_timing: Option<(&'static str, Instant)>,
    recovering_picture: bool,
    pub channel_id: Option<String>,
    pub last_activity: std::time::Instant,
    pub audio_track: Option<String>,
    /// "Stats for nerds" overlay
    pub show_stats: bool,
    /// Last observed live edge (`max(duration, cache_time)`) and when it last advanced.
    live_edge_at: Option<(f64, Instant)>,
    /// Active loopback live DASH session (1440p/4K live streams) for virtual DVR seeking.
    live_dash: Option<crate::api::streams::live_dash::LiveDashHandle>,
    /// Maps mpv's `time-pos` to wall-clock behind-live seconds on live DASH:
    /// `(first_pts, observed_wall_time, initial_behind_secs)`.
    live_dash_anchor: Option<(f64, Instant, f64)>,
    /// Behind-live offset requested when opening/reloading a live DASH session.
    live_dash_pending_behind: f64,
    /// Bounds automatic reload recoveries when a live stream hits EOF.
    live_eof_guard: LiveEofGuard,
    /// Per-`loadfile` generation tracking via `CommandReply` + `StartFile` so stale
    /// events from a replaced file cannot clear `loading` or anchor the wrong PTS.
    expected_load_gen: u64,
    reply_load_gen: u64,
    active_load_gen: u64,
    /// True once `PlaybackRestart` arrived for the active live DASH load.
    live_dash_restart_seen: bool,
    /// Invalidates in-flight background `live_dash::probe_head` tasks.
    dash_probe_serial: u64,
    /// Forces HLS playback for the current live video after a fatal live DASH failure.
    force_live_hls: bool,
    live_dash_fallback_used: bool,
    /// Desired behind-live offset to apply once a newly loaded live HLS rendition reports its edge.
    pending_hls_live_behind: Option<f64>,
}

impl EventEmitter<PlayerEvent> for Player {}

pub struct PlayerHandle {
    active: Entity<Player>,
    tabs: Vec<WeakEntity<Player>>,
}
impl Global for PlayerHandle {}

pub fn player(cx: &App) -> Entity<Player> {
    cx.global::<PlayerHandle>().active.clone()
}

pub fn init(cx: &mut App) {
    let active = cx.new(Player::new);
    cx.set_global(PlayerHandle { tabs: vec![active.downgrade()], active });
}

pub(crate) fn new_tab_player(cx: &mut App) -> Entity<Player> {
    let player = cx.new(Player::new);
    let registry = cx.global_mut::<PlayerHandle>();
    registry.tabs.retain(|tab| tab.upgrade().is_some());
    registry.tabs.push(player.downgrade());
    player
}

pub(crate) fn set_active_player(player: Entity<Player>, cx: &mut App) {
    cx.global_mut::<PlayerHandle>().active = player;
}

pub(crate) fn tab_players(cx: &App) -> Vec<Entity<Player>> {
    cx.global::<PlayerHandle>().tabs.iter().filter_map(WeakEntity::upgrade).collect()
}

/// How many videos back "previous" can go.
const PLAYED_BEFORE_LIMIT: usize = 50;

pub fn enqueue(v: VideoItem, cx: &mut App) {
    player(cx).update(cx, |p, cx| {
        if p.video.is_none() {
            p.queue.push(v.clone());
            p.queue_pos = Some(p.queue.len() - 1);
            p.play(v, None, cx);
        } else {
            if p.queue.is_empty() {
                if let Some(cur) = p.video.clone() {
                    p.queue.push(cur);
                    p.queue_pos = Some(0);
                }
            }
            p.queue.push(v);
        }
        cx.notify();
    });
}

pub const EQ_PRESETS: &[(&str, &str, &str)] = &[
    ("flat", "Off", ""),
    ("bass", "Bass boost", "equalizer=f=60:width_type=o:w=1.5:g=6,equalizer=f=150:width_type=o:w=1.5:g=4"),
    ("voice", "Voice", "equalizer=f=300:width_type=o:w=1.5:g=-3,equalizer=f=1500:width_type=o:w=2:g=4,equalizer=f=3500:width_type=o:w=1.5:g=3"),
    ("treble", "Treble boost", "equalizer=f=4000:width_type=o:w=1.5:g=4,equalizer=f=10000:width_type=o:w=1.5:g=6"),
    ("warm", "Warm", "equalizer=f=100:width_type=o:w=1.5:g=3,equalizer=f=8000:width_type=o:w=1.5:g=-3"),
];

pub fn eq_label(id: &str) -> &'static str {
    EQ_PRESETS.iter().find(|&&(k, _, _)| k == id).map(|&(_, l, _)| l).unwrap_or("Off")
}

fn eq_filter(id: &str) -> &'static str {
    EQ_PRESETS.iter().find(|&&(k, _, _)| k == id).map(|&(_, _, f)| f).unwrap_or("")
}

impl Player {
    fn new(cx: &mut Context<Self>) -> Self {
        Self::empty().with_cx(cx)
    }

    fn empty() -> Self {
        let s = store::settings_snapshot();
        Self {
            caption_margin: std::cell::Cell::new(CAPTION_MARGIN),
            mpv: None,
            engine_generation: 0,
            foreground: true,
            in_pip: false,
            init_error: None,
            video: None,
            streams: None,
            loading: false,
            playback_ready: false,
            error: None,
            quality: s.default_quality,
            audio_only: s.audio_only,
            time: 0.0,
            duration: 0.0,
            paused: true,
            buffering: false,
            seeking: false,
            cache_time: 0.0,
            cache_ranges: None,
            seek_cache_target: None,
            seek_disk_bytes: 0,
            preloaded_until: 0.0,
            volume: s.volume,
            muted: false,
            compressor_enabled: s.audio_compressor,
            compressor_strength: Strength::parse(&s.audio_compressor_strength),
            speed: s.default_speed,
            boost: None,
            keyboard_feedback: None,
            eof: false,
            frame: None,
            old_frames: Vec::new(),
            video_size: (16.0, 9.0),
            segments: Vec::new(),
            skipped: HashSet::new(),
            sb_prompt: None,
            muted_by_sb: false,
            caption: None,
            queue: Vec::new(),
            queue_pos: None,
            autoplay_next: None,
            played_before: Vec::new(),
            going_back: None,
            queue_serial: 0,
            queue_shuffle: false,
            queue_scroll_y: None,
            chapters: Vec::new(),
            heatmap: Vec::new(),
            annotations: Arc::new(Vec::new()),
            ambient: None,
            ambient_tick: 0,
            auto_crop: Default::default(),
            loop_video: s.loop_video,
            load_serial: 0,
            history_report: None,
            last_progress_save: 0.0,
            storyboard: None,
            last_prop_notify: std::time::Instant::now(),
            fallback_tried: false,
            live_rejoins: 0,
            frame_progress: None,
            hold_picture: None,
            playback_timing: None,
            recovering_picture: false,
            channel_id: None,
            last_activity: std::time::Instant::now(),
            audio_track: None,
            show_stats: false,
            live_edge_at: None,
            live_dash: None,
            live_dash_anchor: None,
            live_dash_pending_behind: 0.0,
            live_eof_guard: LiveEofGuard::default(),
            expected_load_gen: 0,
            reply_load_gen: 0,
            active_load_gen: 0,
            live_dash_restart_seen: false,
            dash_probe_serial: 0,
            force_live_hls: false,
            live_dash_fallback_used: false,
            pending_hls_live_behind: None,
        }
    }

    pub fn is_live(&self) -> bool {
        self.streams.as_ref().is_some_and(|s| s.is_live)
            || self.video.as_ref().is_some_and(|v| v.is_live)
    }

    pub fn live_edge(&self) -> f64 {
        let d = if self.duration.is_finite() && self.duration > 0.0 { self.duration } else { 0.0 };
        let c = if self.cache_time.is_finite() && self.cache_time > 0.0 { self.cache_time } else { 0.0 };
        d.max(c)
    }

    /// Seekable DVR timeline length in seconds for the current live stream
    /// (`live_dash` DVR window on 1440p/4K DASH, or `live_edge` on HLS).
    pub fn live_dvr_window(&self) -> f64 {
        if !self.is_live() {
            return 0.0;
        }
        if let Some(dash) = &self.live_dash {
            dash.dvr_window_secs()
        } else {
            self.live_edge()
        }
    }

    pub fn live_behind(&self) -> f64 {
        if !self.is_live() {
            return 0.0;
        }
        if let Some(dash) = &self.live_dash {
            if let Some((base_pts, base_wall, base_behind)) = self.live_dash_anchor {
                return live_dash_behind_at(
                    base_pts,
                    self.time,
                    base_wall.elapsed().as_secs_f64(),
                    base_behind,
                    dash.dvr_window_secs(),
                );
            }
            return self.live_dash_pending_behind.max(0.0);
        }
        let edge = self.live_edge();
        if edge > 0.0 && self.time.is_finite() {
            (edge - self.time).max(0.0)
        } else {
            0.0
        }
    }

    /// Current playback position along the `0..live_dvr_window()` seekbar.
    pub fn live_position(&self) -> f64 {
        let win = self.live_dvr_window();
        if win > 0.0 {
            (win - self.live_behind()).clamp(0.0, win)
        } else {
            0.0
        }
    }

    pub fn is_at_live_edge(&self) -> bool {
        self.is_live() && !self.paused && !self.eof && self.live_behind() <= LIVE_AT_EDGE_THRESHOLD
    }

    /// Seeks a live broadcast to `behind_secs` before the live edge (`< 3s` jumps to live).
    pub fn seek_live_behind(&mut self, behind_secs: f64, cx: &mut Context<Self>) {
        if !self.is_live() {
            return;
        }
        let behind = if behind_secs.is_finite() { behind_secs.max(0.0) } else { 0.0 };
        if behind < 3.0 {
            self.go_live(cx);
            return;
        }
        let win = self.live_dvr_window();
        if win <= 0.0 {
            return;
        }
        let clamped = behind.clamp(0.0, win);
        if self.live_dash.is_some() {
            self.eof = false;
            self.error = None;
            self.load_current(LoadTarget::LiveBehind(clamped), true, cx);
            self.sync_idle(cx);
            cx.notify();
            return;
        }
        let edge = self.live_edge();
        if edge > 0.0 {
            self.seek_interactive((edge - clamped).max(0.0));
            cx.notify();
        }
    }

    /// Moves a live broadcast forward (`delta_secs > 0`) or backward (`delta_secs < 0`)
    /// inside its DVR window, snapping to the live edge when stepping past it.
    pub fn seek_live_delta(&mut self, delta_secs: f64, cx: &mut Context<Self>) -> f64 {
        if !self.is_live() || !delta_secs.is_finite() || delta_secs == 0.0 {
            return 0.0;
        }
        let win = self.live_dvr_window();
        if win <= 0.0 {
            return 0.0;
        }
        let before_behind = self.live_behind();
        let target_behind = (before_behind - delta_secs).clamp(0.0, win);
        if delta_secs > 0.0 && target_behind <= 5.0 {
            if !self.is_at_live_edge() {
                self.go_live(cx);
            }
            return before_behind;
        }
        let moved = before_behind - target_behind;
        if moved.abs() >= 0.5 {
            self.seek_live_behind(target_behind, cx);
        }
        moved
    }

    pub fn go_live(&mut self, cx: &mut Context<Self>) {
        if !self.is_live() {
            return;
        }
        let was_eof = self.eof;
        let was_paused = self.paused;
        self.eof = false;
        if self.live_dash.is_some() {
            self.error = None;
            if self.streams.is_some() {
                self.update_paused(false);
                self.load_current(LoadTarget::LiveEdge, false, cx);
            } else if let Some(video) = self.video.clone() {
                self.play(video, None, cx);
            }
            self.sync_idle(cx);
            cx.notify();
            return;
        }
        let edge = self.live_edge();
        let behind = self.live_behind();
        let stale = self.live_edge_is_stale(Instant::now());
        // When mpv paused/rewound long enough for the HLS playlist to stop refreshing
        // (verified with libmpv: duration freezes once the demuxer cache fills), or
        // after EOF / large rewind / error, reload so we land at the true live edge.
        if should_reload_to_go_live(edge, behind, was_eof, self.error.is_some(), stale) {
            self.error = None;
            if self.streams.is_some() {
                self.update_paused(false);
                self.load_current(LoadTarget::LiveEdge, false, cx);
            } else if let Some(video) = self.video.clone() {
                self.play(video, None, cx);
            }
            self.sync_idle(cx);
            cx.notify();
            return;
        }
        let target = clamp_live_seek(edge, edge);
        self.seek(target, "absolute+keyframes");
        if was_paused {
            self.set_paused(false, cx);
        }
        cx.notify();
    }

    fn record_live_edge(&mut self, now: Instant) {
        if !self.is_live() {
            return;
        }
        let edge = self.live_edge();
        if edge <= 0.0 {
            return;
        }
        let advanced = match self.live_edge_at {
            None => true,
            Some((prev, _)) => edge > prev + 0.5,
        };
        if advanced {
            self.live_edge_at = Some((edge, now));
        }
    }

    fn live_edge_is_stale(&self, now: Instant) -> bool {
        is_live_edge_stale(self.live_edge_at, self.paused, now)
    }

    fn with_cx(self, _cx: &mut Context<Self>) -> Self {
        self
    }

    /// True while mpv is actually progressing through a video.
    fn should_inhibit_idle(&self) -> bool {
        self.mpv.is_some()
            && self.video.is_some()
            && !self.loading
            && !self.paused
            && !self.eof
            && self.error.is_none()
    }

    fn sync_idle(&self, cx: &mut App) {
        if self.in_pip || self.foreground && crate::ui::pip::handle(cx).is_none() {
            crate::idle_inhibit::set_playing(cx, self.should_inhibit_idle());
        }
    }

    pub(crate) fn is_foreground(&self) -> bool {
        self.foreground
    }

    pub(crate) fn set_foreground(&mut self, active: bool, cx: &mut Context<Self>) {
        self.foreground = active;
        self.update_video_output(cx);
    }

    pub(crate) fn set_in_pip(&mut self, in_pip: bool, cx: &mut Context<Self>) {
        self.in_pip = in_pip;
        self.update_video_output(cx);
    }

    pub(crate) fn refresh_idle_guard(&self, cx: &mut App) {
        self.sync_idle(cx);
    }

    pub(crate) fn has_engine(&self) -> bool {
        self.mpv.is_some()
    }

    fn update_video_output(&mut self, cx: &mut Context<Self>) {
        let visible = self.foreground || self.in_pip;
        if let Some(mpv) = &self.mpv {
            mpv.set("vid", if visible { "auto" } else { "no" });
        }
        if !visible {
            if let Some(frame) = self.frame.take() {
                cx.drop_image(frame, None);
            }
            for frame in self.old_frames.drain(..) {
                cx.drop_image(frame, None);
            }
            self.frame_progress = None;
        }
        self.sync_idle(cx);
        cx.notify();
    }

    pub(crate) fn release_tab_engine(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.engine_generation += 1;
        self.mpv = None;
    }

    fn ensure_mpv(&mut self, cx: &mut Context<Self>) -> Option<Arc<Mpv>> {
        if let Some(m) = &self.mpv {
            return Some(m.clone());
        }
        let s = store::settings_snapshot();
        let (tx, rx) = futures::channel::mpsc::unbounded::<MpvEvent>();
        let vol = format!("{}", s.volume.round());
        let ua = crate::api::http::BROWSER_UA;
        let hw = if s.hwdec { "auto-copy" } else { "no" };
        let af = self.audio_filter_chain(&s.eq_preset);
        let opts: Vec<(&str, &str)> = vec![
            ("vo", "libmpv"),
            ("vid", if self.foreground || self.in_pip { "auto" } else { "no" }),
            ("hwdec", hw),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("ytdl", "no"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("osc", "no"),
            ("cache", "yes"),
            ("demuxer-max-bytes", "48MiB"),
            ("demuxer-max-back-bytes", BACK_CACHE_SIZE),
            ("demuxer-readahead-secs", "40"),
            ("network-timeout", "20"),
            ("user-agent", ua),
            ("volume", &vol),
            ("volume-max", "150"),
            ("hr-seek", "yes"),
            ("force-seekable", "yes"),
            ("audio-client-name", "Stream Tube"),
            ("af", &af),
            ("sub-font-size", "42"),
            ("sub-border-size", "2.5"),
            ("sub-auto", "no"),
            ("osd-level", "0"),
            // software render path: convert colors only, never scale on the CPU
            ("zimg-fast", "yes"),
            ("zimg-scaler", "bilinear"),
            ("zimg-dither", "no"),
            ("zimg-threads", "auto"),
            ("sws-scaler", "fast-bilinear"),
            // libswscale's integer SIMD path converts yuv420p to bgr0 in about a quarter
            // of the CPU time zimg's float pipeline needs (10.7 -> 2.9 ms a frame at
            // 1080p60 on an M-series Mac), with the same colours
            ("sws-allow-zimg", "no"),
            ("sws-fast", "yes"),
            ("video-sync", "audio"),
            ("framedrop", "vo"),
        ];
        let opts: Vec<(&str, &str)> = opts.into_iter().collect();
        let proxy = s.proxy.clone();
        let mut opts_owned: Vec<(String, String)> = opts.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        // Embedded libmpv can have no default cache directory.
        let cache_dir = store::data_dir().join("playback-cache");
        let cache_dir = match std::fs::create_dir_all(&cache_dir) {
            Ok(()) => cache_dir,
            Err(e) => {
                eprintln!("[player] Could not create playback cache directory: {e}");
                std::env::temp_dir()
            }
        };
        opts_owned.push(("demuxer-cache-dir".into(), cache_dir.to_string_lossy().into_owned()));
        // normal videos reach mpv through the app's local stream proxy, which already
        // uses the user's proxy; mpv itself only fetches live streams directly, and can
        // only use HTTP proxies (127.0.0.1 is excluded through no_proxy, see main)
        if crate::store::mpv_http_proxy(&proxy) {
            opts_owned.push(("http-proxy".into(), proxy));
        }
        if let Ok(f) = std::env::var("STREAMTUBE_MPV_LOG") {
            opts_owned.push(("log-file".into(), f));
        }
        let refs: Vec<(&str, &str)> = opts_owned.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let t_init = std::time::Instant::now();
        let created = Mpv::new(&refs, move |ev| {
            let _ = tx.unbounded_send(ev);
        });
        if crate::perf::on() {
            eprintln!("[mpv] engine created in {:?}", t_init.elapsed());
        }
        match created {
            Ok(m) => {
                self.mpv = Some(m.clone());
                self.engine_generation += 1;
                self.expected_load_gen = 0;
                self.reply_load_gen = 0;
                self.active_load_gen = 0;
                self.live_dash_restart_seen = false;
                let generation = self.engine_generation;
                cx.spawn(async move |this, cx| {
                    use futures::StreamExt;
                    let mut rx = rx;
                    while let Some(ev) = rx.next().await {
                        // coalesce queued events
                        let mut batch = vec![ev];
                        while let Ok(e) = rx.try_recv() {
                            batch.push(e);
                        }
                        let ok = this
                            .update(cx, |p, cx| {
                                if p.engine_generation != generation { return false; }
                                for e in batch {
                                    p.handle_event(e, cx);
                                }
                                true
                            })
                            .unwrap_or(false);
                        if !ok {
                            break;
                        }
                    }
                })
                .detach();
                Some(m)
            }
            Err(e) => {
                let error = e.to_string();
                self.init_error = Some(match error.as_str() {
                    #[cfg(windows)]
                    super::mpv::MISSING_WINDOWS_MPV => translate(super::mpv::MISSING_WINDOWS_MPV).to_owned(),
                    _ => tf!("Could not start the video engine (libmpv): {0}", error),
                });
                None
            }
        }
    }

    /// Moves captions up (over the controls) or back down; a no-op when unchanged.
    pub fn set_caption_margin(&self, margin: u32) {
        if self.caption_margin.replace(margin) != margin {
            if let Some(mpv) = &self.mpv {
                mpv.set("sub-margin-y", &margin.to_string());
            }
        }
    }

    pub fn mpv(&self) -> Option<Arc<Mpv>> {
        self.mpv.clone()
    }

    fn is_active_load(&self) -> bool {
        self.expected_load_gen > 0 && self.active_load_gen == self.expected_load_gen
    }

    fn maybe_apply_pending_hls_dvr(&mut self) {
        if self.loading || self.live_dash.is_some() {
            return;
        }
        let Some(behind) = self.pending_hls_live_behind else { return };
        let edge = self.live_edge();
        if edge <= 0.0 {
            return;
        }
        self.pending_hls_live_behind = None;
        let target = clamp_live_seek((edge - behind).max(0.0), edge);
        self.seek_interactive(target);
    }

    fn try_fallback_live_dash_to_hls(&mut self, cx: &mut Context<Self>) -> bool {
        if self.live_dash.is_some()
            && !self.live_dash_fallback_used
            && self.streams.as_ref().is_some_and(|i| i.is_live && i.hls_url.is_some())
        {
            let target = self.reload_target();
            self.live_dash_fallback_used = true;
            self.force_live_hls = true;
            self.error = None;
            eprintln!("[player] Live DASH failed; falling back to HLS");
            self.load_current(target, true, cx);
            return true;
        }
        false
    }

    fn handle_event(&mut self, ev: MpvEvent, cx: &mut Context<Self>) {
        let tracked = matches!(
            &ev,
            MpvEvent::CommandReply { .. }
                | MpvEvent::StartFile
                | MpvEvent::FileLoaded
                | MpvEvent::PlaybackRestart
                | MpvEvent::EndFile { .. }
                | MpvEvent::Prop(..)
                | MpvEvent::Shutdown
        );
        match ev {
            MpvEvent::CommandReply { id, error } => {
                if id == self.expected_load_gen && id > 0 {
                    if let Some(e) = error {
                        if !self.try_fallback_live_dash_to_hls(cx) {
                            self.loading = false;
                            self.error = Some(tf!("Playback error: {0}", e));
                        }
                        cx.notify();
                    } else {
                        self.reply_load_gen = id;
                    }
                }
            }
            MpvEvent::StartFile => {
                if self.expected_load_gen > 0 && self.reply_load_gen == self.expected_load_gen {
                    self.active_load_gen = self.expected_load_gen;
                }
            }
            MpvEvent::Frame => {
                if !self.foreground && !self.in_pip {
                    if let Some(mpv) = &self.mpv { let _ = mpv.take_frame(); }
                    return;
                }
                let t0 = std::time::Instant::now();
                crate::perf::add(&crate::perf::UI_FRAMES, 1);
                let _g = scopeguard(move || crate::perf::add(&crate::perf::UI_HANDLE_US, t0.elapsed().as_micros() as u64));
                if let Some(m) = &self.mpv {
                    if let Some(f) = m.take_frame() {
                        // A seek or reload can redraw an empty surface before
                        // the first decoded picture at the new position arrives.
                        if self.hold_picture == Some(f.picture_serial) {
                            return;
                        }
                        self.hold_picture = None;
                        self.ambient_tick += 1;
                        if self.ambient_tick % 12 == 1 && store::settings_snapshot().ambient_mode {
                            if let Some(c) = average_color(&f.data, f.width, f.height) {
                                let changed = self.ambient.map(|o| (o.r - c.r).abs() + (o.g - c.g).abs() + (o.b - c.b).abs() > 0.04).unwrap_or(true);
                                if changed {
                                    self.ambient = Some(c);
                                    cx.emit(PlayerEvent::Ambient);
                                }
                            }
                        }
                        // a few looks a second are enough to follow the bars
                        if self.ambient_tick % 10 == 0 && store::settings_snapshot().aspect_mode == "auto" {
                            self.auto_crop.feed(super::aspect::detect_bars(&f.data, f.width, f.height));
                        }
                        if let Some(buf) = image::RgbaImage::from_raw(f.width, f.height, f.data) {
                            let img = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(buf)]));
                            if let Some(old) = self.frame.replace(img) {
                                self.old_frames.push(old);
                            }
                            // drop textures of frames that are no longer displayed
                            while self.old_frames.len() > 1 {
                                let o = self.old_frames.remove(0);
                                cx.drop_image(o, None);
                            }
                            cx.notify();
                        }
                    }
                }
            }
            MpvEvent::FileLoaded => {
                if !self.is_active_load() {
                    return;
                }
                if self.video.as_ref().is_some_and(|v| v.is_short) {
                    self.cache_time = 0.0;
                }
                self.loading = false;
                self.eof = false;
                self.apply_caption();
                self.maybe_apply_pending_hls_dvr();
                cx.emit(PlayerEvent::Loaded);
                cx.notify();
            }
            MpvEvent::PlaybackRestart => {
                if !self.is_active_load() {
                    return;
                }
                if self.live_dash.is_some() && !self.loading {
                    self.live_dash_restart_seen = true;
                }
                self.frame_progress = None;
                if !self.loading && !self.buffering && self.error.is_none() && self.streams.is_some() {
                    if let Some((action, started)) = self.playback_timing.take() {
                        eprintln!("[player] {action} to playback restart: {} ms", started.elapsed().as_millis());
                    }
                    self.set_playback_ready(true, cx);
                }
                self.seeking = false;
                cx.notify();
            }
            MpvEvent::EndFile { eof, error } => {
                if !self.is_active_load() {
                    return;
                }
                if let Some(e) = error {
                    if self.video.is_some() {
                        if self.try_fallback_live_dash_to_hls(cx) {
                            return;
                        }
                        if self.loading && !self.fallback_tried {
                            self.fallback_to_ytdlp(cx);
                        } else {
                            self.loading = false;
                            self.error = Some(tf!("Playback error: {0}", e));
                        }
                    }
                }
                if eof {
                    self.on_ended(cx);
                }
                cx.notify();
            }
            MpvEvent::Prop(name, val) => {
                let is_global_prop = matches!(name, "speed" | "volume" | "mute" | "pause");
                if !is_global_prop && !self.is_active_load() {
                    return;
                }
                // While frames are flowing, the frame event already redraws the player;
                // only redraw for clock/cache updates when no video frames arrive.
                let quiet = matches!(name, "time-pos" | "demuxer-cache-time" | "demuxer-cache-state")
                    && !self.paused
                    && !self.audio_only
                    && self.frame.is_some()
                    && self.last_prop_notify.elapsed() < std::time::Duration::from_millis(500);
                match (name, val) {
                    ("time-pos", PropValue::Double(t)) => {
                        self.time = t;
                        if self.live_dash_anchor.is_none()
                            && !self.loading
                            && self.live_dash_restart_seen
                            && t.is_finite()
                        {
                            if let Some(dash) = &self.live_dash {
                                let actual_behind = dash
                                    .actual_start_behind_secs()
                                    .unwrap_or(self.live_dash_pending_behind);
                                self.live_dash_anchor = Some((t, Instant::now(), actual_behind));
                            }
                        }
                        if t > 0. && self.error.is_none() && !self.loading && !self.buffering && !self.paused && !self.playback_ready {
                            self.set_playback_ready(true, cx);
                        }
                        if !self.recover_stalled_picture(cx) {
                            self.on_time(cx);
                        }
                    }
                    ("duration", PropValue::Double(d)) => {
                        self.duration = d;
                        self.record_live_edge(Instant::now());
                        self.maybe_apply_pending_hls_dvr();
                    }
                    ("pause", PropValue::Flag(b)) => {
                        self.update_paused(b);
                        self.frame_progress = None;
                    }
                    ("eof-reached", PropValue::Flag(b)) => {
                        if b {
                            self.on_ended(cx);
                        } else {
                            self.eof = false;
                        }
                    }
                    ("paused-for-cache", PropValue::Flag(b)) => {
                        self.buffering = b;
                        if b {
                            self.frame_progress = None;
                            self.set_playback_ready(false, cx);
                        }
                    }
                    ("seeking", PropValue::Flag(b)) => {
                        self.seeking = b;
                        self.frame_progress = None;
                    }
                    ("demuxer-cache-time", PropValue::Double(c)) => {
                        self.cache_time = c;
                        self.record_live_edge(Instant::now());
                        self.maybe_apply_pending_hls_dvr();
                    }
                    ("demuxer-cache-state", PropValue::Cache { ranges, disk_bytes }) if !self.loading && self.streams.is_some() => {
                        self.seek_disk_bytes = self.seek_disk_bytes.max(disk_bytes);
                        if let Some((from, end)) = self.seek_cache_target {
                            // Follow only the range that actually covers our last seek or
                            // refill point. A separate range ahead may itself be shrinking.
                            let filled = ranges.iter().filter(|&&(a, b)| a <= from && from <= b)
                                .map(|&(_, b)| b).fold(from, f64::max);
                            let target = (filled < end && self.seek_disk_bytes < SEEK_DISK_CACHE_LIMIT)
                                .then_some((filled, end));
                            self.set_seek_cache_target(target);
                        }
                        self.cache_ranges = Some(ranges);
                    }
                    ("speed", PropValue::Double(s)) => self.speed = s,
                    ("volume", PropValue::Double(v)) => self.volume = v,
                    ("mute", PropValue::Flag(m)) => self.muted = m,
                    ("dwidth", PropValue::Double(w)) => self.video_size.0 = w,
                    ("dheight", PropValue::Double(h)) => self.video_size.1 = h,
                    _ => {}
                }
                if !quiet {
                    self.last_prop_notify = std::time::Instant::now();
                    cx.notify();
                }
            }
            MpvEvent::Shutdown => {}
        }
        if tracked {
            self.sync_idle(cx);
        }
    }

    fn on_ended(&mut self, cx: &mut Context<Self>) {
        if self.video.is_none() || self.loading || self.error.is_some() || self.eof {
            return;
        }
        if self.is_live() {
            // Hitting EOF on a livestream (scrubbing to the live edge, expired URLs,
            // or a transient buffer underrun) must never set `eof`, advance the queue,
            // or emit `PlayerEvent::Ended` (which would autoplay-skip to the next video).
            self.eof = false;
            if self.live_dash.as_ref().is_some_and(|d| d.has_fatal_error())
                && self.try_fallback_live_dash_to_hls(cx)
            {
                return;
            }
            if self.live_rejoins < 5 && self.live_eof_guard.allow(Instant::now()) {
                self.live_rejoins += 1;
                eprintln!("[player] Live stream hit EOF; rejoining broadcast (attempt {})", self.live_rejoins);
                self.rejoin_live(cx);
            } else {
                eprintln!("[player] Live stream hit EOF repeatedly; stopping recovery");
                self.set_paused(true, cx);
                self.set_playback_ready(false, cx);
                self.error = Some(translate("The live stream has ended or was interrupted.").into());
            }
            self.sync_idle(cx);
            cx.notify();
            return;
        }
        let position = self.mpv.as_ref().and_then(|mpv| mpv.get_f64("time-pos")).unwrap_or(self.time);
        let looping = self.loop_video;
        if premature_eof(position, self.duration) {
            self.time = position;
            self.error = Some("Playback stopped before the end of the video.".into());
            self.save_progress(true);
            return;
        }
        self.eof = true;
        self.time = position;
        if looping {
            self.eof = false;
            self.save_progress(true);
            if let Some(report) = &self.history_report {
                report.progress(self.time, true, true);
            }
            self.seek_to(0.0);
            self.set_paused(false, cx);
            return;
        }
        self.finish_playback_history();
        if let Some(pos) = self.queue_pos {
            if pos + 1 < self.queue.len() {
                self.play_queue_index(pos + 1, cx);
                return;
            }
        }
        cx.emit(PlayerEvent::Ended);
        self.sync_idle(cx);
    }

    pub fn play_queue_index(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(v) = self.queue.get(idx).cloned() {
            self.queue_pos = Some(idx);
            let id = v.id.clone();
            self.play(v, None, cx);
            if self.foreground {
                let source = cx.entity_id();
                cx.defer(move |cx| {
                    if player(cx).entity_id() == source {
                        crate::ui::app::sync_watch_route(id, cx);
                    }
                });
            }
        }
    }

    /// Replaces the queue with playlist items and keeps draining any remaining
    /// continuation pages in the background so playback starts immediately.
    pub fn set_playlist_queue(
        &mut self,
        videos: Vec<VideoItem>,
        start_idx: usize,
        continuation: Option<String>,
        authed: bool,
        shuffle: bool,
        cx: &mut Context<Self>,
    ) {
        self.queue_serial += 1;
        let serial = self.queue_serial;
        self.queue = videos;
        self.queue_pos = Some(start_idx);
        self.queue_shuffle = shuffle;
        self.queue_scroll_y = None;
        cx.emit(PlayerEvent::QueueChanged);
        cx.notify();
        let Some(first) = continuation.filter(|t| !t.is_empty()) else { return };
        cx.spawn(async move |this, cx| {
            let mut pager = crate::api::innertube::PlaylistPager::new(Some(first), crate::api::innertube::MAX_PLAYLIST_PAGES);
            while let Some(tok) = pager.take_next() {
                let Ok((vids, next)) = blocking(move || crate::api::innertube::playlist_continue(&tok, authed)).await else { break };
                pager.advance(next);
                let keep_going = this
                    .update(cx, |p, cx| {
                        if p.queue_serial != serial {
                            return false;
                        }
                        if extend_queue(&mut p.queue, p.queue_pos, vids, p.queue_shuffle, &mut rand::rng()) > 0 {
                            cx.emit(PlayerEvent::QueueChanged);
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Remembers the video being left for `next`, unless "previous" is going back.
    fn remember_played(&mut self, next: &VideoItem) {
        let going_back = self.going_back.take().is_some_and(|id| id == next.id);
        let Some(current) = self.video.clone() else { return };
        // Shorts are swiped through, not watched one after another
        if going_back || current.id == next.id || current.is_short || next.is_short {
            return;
        }
        self.played_before.retain(|v| v.id != current.id);
        self.played_before.push(current);
        let excess = self.played_before.len().saturating_sub(PLAYED_BEFORE_LIMIT);
        self.played_before.drain(..excess);
    }

    /// The video "previous" goes back to; the `play` that follows isn't remembered.
    pub(crate) fn take_played_before(&mut self) -> Option<VideoItem> {
        let v = self.played_before.pop()?;
        self.going_back = Some(v.id.clone());
        Some(v)
    }

    pub fn replace_queue(&mut self, queue: Vec<VideoItem>, pos: Option<usize>) {
        self.queue_serial += 1;
        self.queue = queue;
        self.queue_pos = pos;
        self.queue_shuffle = false;
        self.queue_scroll_y = None;
    }

    pub fn reset_queue(&mut self) {
        self.replace_queue(Vec::new(), None);
    }

    pub fn on_queue_replaced(&mut self) {
        self.queue_serial += 1;
        self.queue_shuffle = false;
        self.queue_scroll_y = None;
    }

    pub fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.reset_queue();
        cx.emit(PlayerEvent::QueueChanged);
        cx.notify();
    }

    pub fn shuffle_queue(&mut self, cx: &mut Context<Self>) {
        use rand::seq::SliceRandom;
        let cur = self.queue_pos.and_then(|i| self.queue.get(i).cloned());
        self.queue.shuffle(&mut rand::rng());
        if let Some(c) = cur {
            self.queue_pos = self.queue.iter().position(|v| v.id == c.id);
        }
        self.queue_shuffle = true;
        self.queue_scroll_y = None;
        cx.emit(PlayerEvent::QueueChanged);
        cx.notify();
    }

    pub fn save_queue_scroll(&mut self, y: Pixels) {
        self.queue_scroll_y = Some((self.queue_serial, y));
    }

    pub fn take_queue_scroll(&mut self) -> Option<Pixels> {
        let (serial, y) = self.queue_scroll_y.take()?;
        (serial == self.queue_serial).then_some(y)
    }

    /// A failed video decoder can leave mpv playing the audio indefinitely
    /// without an EndFile event. Compare rendered pictures with the media clock.
    fn recover_stalled_picture(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(mpv), Some(info)) = (&self.mpv, &self.streams) else { return false };
        if !(self.foreground || self.in_pip) || self.audio_only || self.loading || self.paused || self.buffering || self.seeking || self.eof || self.error.is_some()
            || info.is_live || (!info.hls_only && info.video.is_empty() && info.muxed.is_empty())
            || (self.duration > 0.0 && self.duration - self.time < 2.0)
        {
            self.frame_progress = None;
            return false;
        }
        let presented = mpv.presented_frames();
        let progress = self.frame_progress.get_or_insert_with(|| FrameProgress::new(presented, self.time));
        if progress.presented != presented || self.time < progress.position {
            *progress = FrameProgress::new(presented, self.time);
            return false;
        }
        let advanced = self.time - progress.position;
        if progress.since.elapsed() < Duration::from_secs(5) || advanced < progress.min_advance {
            return false;
        }
        // Still-image tracks and very low frame rates have legitimate gaps.
        let fps = mpv.get_f64("container-fps").unwrap_or(0.0);
        if fps > 0.0 && advanced < 2.0 / fps {
            progress.min_advance = 2.0 / fps;
            return false;
        }
        if ["current-tracks/video/image", "current-tracks/video/albumart"].iter().any(|p| mpv.get_string(p).as_deref() == Some("yes")) {
            self.frame_progress = None;
            return false;
        }
        self.frame_progress = None;
        if self.recovering_picture {
            self.set_paused(true, cx);
            self.set_playback_ready(false, cx);
            self.error = Some(translate("The video decoder stopped producing frames.").into());
            cx.notify();
        } else {
            self.recovering_picture = true;
            eprintln!("[player] Video frames stalled while playback advanced. Reopening at {:.1}s with software decoding.", self.time);
            self.load_current(LoadTarget::VodTime(self.time), true, cx);
        }
        true
    }

    fn on_time(&mut self, cx: &mut Context<Self>) {
        let t = self.time;
        if !self.loading && !self.paused && !self.buffering && !self.eof {
            if let Some(report) = &mut self.history_report {
                report.start(t, self.duration);
            }
        }
        // progress save
        if !self.loading && !self.eof && (t - self.last_progress_save).abs() >= 5.0 {
            if self.is_live() {
                self.last_progress_save = t;
            } else {
                self.save_progress(false);
                if let Some(id) = self.video.as_ref().map(|v| v.id.clone()) {
                    cx.default_global::<crate::ui::cards::Watched>().0.insert(id, (t, self.duration));
                }
            }
            if let Some(report) = &self.history_report {
                report.progress(t, !self.paused, false);
            }
        }
        // SponsorBlock
        let s = store::settings_snapshot();
        if !s.sponsorblock || self.segments.is_empty() {
            return;
        }
        let mut prompt = None;
        let mut in_mute = false;
        for seg in self.segments.clone() {
            let action = s.sb_actions.get(&seg.category).map(String::as_str).unwrap_or("off");
            if action == "off" || seg.action == "poi" || seg.action == "full" {
                continue;
            }
            let inside = t >= seg.start && t < seg.end - 0.3;
            if !inside {
                continue;
            }
            if seg.action == "mute" {
                in_mute = true;
                continue;
            }
            if seg.end - seg.start < s.sb_min_duration {
                continue;
            }
            match action {
                "skip" if !self.skipped.contains(&seg.uuid) => {
                    self.skipped.insert(seg.uuid.clone());
                    let back_to = seg.start;
                    self.seek_to(seg.end);
                    let server = s.sponsorblock_server.clone();
                    let uuid = seg.uuid.clone();
                    std::thread::spawn(move || services::sponsor_viewed(&server, &uuid));
                    if s.sb_notify {
                        let weak = cx.entity().downgrade();
                        let mut actions: Vec<(SharedString, Arc<dyn Fn(&mut Window, &mut App)>)> = vec![(
                            translate("Unskip").into(),
                            Arc::new(move |_, cx: &mut App| {
                                if let Some(p) = weak.upgrade() {
                                    p.update(cx, |p, _| p.seek_to(back_to));
                                }
                            }),
                        )];
                        if !s.sb_user_id.is_empty() {
                            for (label, up) in [("Upvote", true), ("Downvote", false)] {
                                let (server, uuid, user) = (s.sponsorblock_server.clone(), seg.uuid.clone(), s.sb_user_id.clone());
                                actions.push((
                                    translate(label).into(),
                                    Arc::new(move |_, cx: &mut App| {
                                        let (server, uuid, user) = (server.clone(), uuid.clone(), user.clone());
                                        cx.spawn(async move |cx| {
                                            let res = crate::ui::blocking(move || services::sponsor_vote(&server, &uuid, &user, up)).await;
                                            let _ = cx.update(|cx| match res {
                                                Ok(()) => toast(cx, translate("Thanks for voting")),
                                                Err(e) => toast(cx, tf!("Vote failed: {0}", e)),
                                            });
                                        })
                                        .detach();
                                    }),
                                ));
                            }
                        }
                        crate::ui::toast_with_actions(cx, tf!("Skipped {0}", translate(services::sponsor_category_name(&seg.category))), actions);
                    }
                }
                "ask" => prompt = Some(seg.clone()),
                _ => {}
            }
        }
        if in_mute != self.muted_by_sb {
            self.muted_by_sb = in_mute;
            if let Some(m) = &self.mpv {
                m.set_flag("mute", in_mute || self.muted);
            }
        }
        self.sb_prompt = prompt;
    }

    pub(crate) fn save_progress(&mut self, force: bool) {
        if self.is_live() {
            return;
        }
        if let Some(v) = &self.video {
            if self.time > 0.0 || (force && (self.playback_ready || self.last_progress_save > 0.0 || self.eof)) {
                // the periodic save goes to a background thread; a forced one (switching
                // videos, the end, quitting) is written before going on
                store::history_progress_queued(&v.id, self.time, self.duration, force);
                self.last_progress_save = self.time;
            }
        }
    }

    pub fn finish_playback_history(&mut self) {
        let report = self.history_report.take();
        if self.updated_position_while_paused().is_some() {
            // another device moved on while this one was paused: keep its position
            if let Some(report) = report {
                report.abandon();
            }
            return;
        }
        if !self.loading {
            self.save_progress(true);
        }
        if let Some(report) = report {
            report.finish(self.time);
        }
    }

    /// Starts playing a video. Fetches streams, SponsorBlock segments and resumes progress.
    fn set_playback_ready(&mut self, ready: bool, cx: &mut Context<Self>) {
        if self.playback_ready != ready {
            self.playback_ready = ready;
            if !ready && self.foreground {
                super::shorts_preload::suspend();
            }
            cx.emit(PlayerEvent::PlaybackReadyChanged);
        }
    }

    pub fn play(&mut self, video: VideoItem, start: Option<f64>, cx: &mut Context<Self>) {
        self.remember_played(&video);
        self.autoplay_next = None;
        self.playback_timing = std::env::var_os("STREAMTUBE_PLAYBACK_TIMING").map(|_| ("open", Instant::now()));
        self.keyboard_feedback = None;
        self.preloaded_until = 0.0;
        self.set_playback_ready(false, cx);
        self.save_progress(true);
        self.finish_playback_history();
        self.channel_id = video.channel_id.clone().filter(|id| !id.is_empty());
        self.compressor_enabled = store::effective_compressor(self.channel_id.as_deref());
        let reuse_engine = self.mpv.is_some();
        let Some(mpv) = self.ensure_mpv(cx) else {
            self.video = Some(video);
            self.sync_idle(cx);
            cx.notify();
            return;
        };
        self.load_serial += 1;
        self.expected_load_gen += 1;
        self.reply_load_gen = 0;
        self.active_load_gen = 0;
        self.live_dash_restart_seen = false;
        self.dash_probe_serial += 1;
        self.force_live_hls = false;
        self.live_dash_fallback_used = false;
        self.pending_hls_live_behind = None;
        self.fallback_tried = false;
        self.live_rejoins = 0;
        self.recovering_picture = false;
        self.frame_progress = None;
        self.live_edge_at = None;
        self.live_dash = None;
        self.live_dash_anchor = None;
        self.live_dash_pending_behind = 0.0;
        self.live_eof_guard.clear();
        let serial = self.load_serial;
        self.video = Some(video.clone());
        self.loading = true;
        self.error = None;
        self.time = 0.0;
        self.last_progress_save = 0.0;
        self.duration = video.duration_secs.unwrap_or(0) as f64;
        self.eof = false;
        self.segments.clear();
        self.skipped.clear();
        self.sb_prompt = None;
        self.streams = None;
        self.storyboard = None;
        self.chapters.clear();
        self.heatmap.clear();
        self.annotations = Arc::new(Vec::new());
        self.ambient = None;
        self.auto_crop = Default::default();
        self.audio_track = None;
        if let Some(old) = self.frame.take() {
            cx.drop_image(old, None);
        }
        let _ = mpv.command(&["stop"]);
        if reuse_engine {
            self.apply_audio_filters();
        }
        store::history_add(&video);
        self.history_report = crate::api::web::PlaybackReport::new(video.id.clone(), serial);
        self.sync_idle(cx);
        let s = store::settings_snapshot();
        let id = video.id.clone();
        let refresh = if s.resume_playback && start.is_none() && !video.is_short && !video.is_live {
            crate::ui::history::refresh_for_playback(cx)
        } else {
            None
        };
        cx.notify();
        if s.annotations {
            self.load_annotations(cx);
        }
        if s.sponsorblock && !video.is_live {
            let segment_id = id.clone();
            cx.spawn(async move |this, cx| {
                let segments = blocking(move || {
                    let categories: Vec<String> = s
                        .sb_actions
                        .iter()
                        .filter(|(_, action)| action.as_str() != "off")
                        .map(|(category, _)| category.clone())
                        .collect();
                    services::sponsor_segments(&segment_id, &s.sponsorblock_server, &categories)
                        .unwrap_or_default()
                })
                .await;
                let _ = this.update(cx, |p, cx| {
                    if p.load_serial == serial {
                        p.segments = segments;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.spawn(async move |this, cx| {
            let streams_id = id.clone();
            let res = blocking(move || streams::get_streams(&streams_id)).await;
            if res.is_ok() {
                if let Some(refresh) = refresh {
                    // Use fresher cross-device progress if it arrives shortly.
                    // A slow history endpoint must not hold up playback.
                    let limit = cx.background_executor().timer(std::time::Duration::from_millis(1200));
                    let _ = futures::future::select(refresh, Box::pin(limit)).await;
                }
            }
            let _ = this.update(cx, |p, cx| {
                if p.load_serial != serial {
                    return;
                }
                match res {
                    Ok(info) => {
                        let info = Arc::new(info);
                        if p.duration <= 0.0 {
                            p.duration = info.length_secs as f64;
                        }
                        if let Some(v) = p.video.as_mut() {
                            if info.is_live {
                                v.is_live = true;
                            }
                            if v.channel_id.is_none() && !info.channel_id.is_empty() {
                                v.channel_id = Some(info.channel_id.clone());
                            }
                            if v.title.is_empty() {
                                v.title = info.title.clone();
                            }
                            if v.channel_name.is_empty() {
                                v.channel_name = info.author.clone();
                            }
                        }
                        if p.channel_id.is_none() && !info.channel_id.is_empty() {
                            p.set_channel_id(Some(info.channel_id.clone()));
                        }
                        p.storyboard = info.storyboard.clone();
                        p.streams = Some(info);
                        // default caption
                        let s = store::settings_snapshot();
                        let resume = if p.is_live() {
                            start
                        } else {
                            start.or_else(|| s.resume_playback.then(|| store::resume_position(&id)).flatten())
                        };
                        p.caption = if s.captions_enabled {
                            p.streams.as_ref().and_then(|i| pick_caption(i, &s.captions_lang))
                        } else {
                            None
                        };
                        // speed per channel
                        let speed = p
                            .channel_id
                            .as_deref()
                            .filter(|_| s.remember_speed_per_channel)
                            .and_then(store::channel_speed)
                            .unwrap_or(s.default_speed);
                        p.speed = speed;
                        let target = if p.is_live() {
                            LoadTarget::LiveEdge
                        } else {
                            LoadTarget::from_vod(resume)
                        };
                        p.load_current(target, false, cx);
                    }
                    Err(e) => {
                        p.loading = false;
                        p.error = Some(e.to_string());
                    }
                }
                p.sync_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches the current video's archived annotations (most videos have none).
    pub fn load_annotations(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.video.as_ref().map(|v| v.id.clone()) else { return };
        let serial = self.load_serial;
        cx.spawn(async move |this, cx| {
            let list = blocking(move || crate::api::annotations::fetch(&id).unwrap_or_default()).await;
            let _ = this.update(cx, |p, cx| {
                if p.load_serial == serial && !list.is_empty() {
                    p.annotations = Arc::new(list);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A live stream stopped: resolve it again and keep playing if it is still on air.
    fn rejoin_live(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.video.as_ref().map(|v| v.id.clone()) else { return };
        streams::invalidate(&id);
        let serial = self.load_serial;
        self.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = blocking(move || streams::get_streams(&id)).await;
            let _ = this.update(cx, |p, cx| {
                if p.load_serial != serial {
                    return;
                }
                match res {
                    Ok(info) if info.is_live && (info.hls_url.is_some() || streams::has_live_dash(&info)) => {
                        p.streams = Some(Arc::new(info));
                        p.update_paused(false);
                        p.load_current(LoadTarget::LiveEdge, false, cx);
                    }
                    _ => {
                        p.loading = false;
                        p.live_rejoins = u32::MAX;
                        p.set_paused(true, cx);
                        p.set_playback_ready(false, cx);
                        p.error = Some(translate("The live stream has ended or was interrupted.").into());
                    }
                }
                p.sync_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The built-in streams failed to open: fetch them again through yt-dlp.
    fn fallback_to_ytdlp(&mut self, cx: &mut Context<Self>) {
        self.preloaded_until = 0.0;
        self.fallback_tried = true;
        let Some(id) = self.video.as_ref().map(|v| v.id.clone()) else { return };
        streams::invalidate(&id);
        let serial = self.load_serial;
        let vod_resume = (!self.is_live() && self.time > 1.0).then_some(self.time);
        cx.spawn(async move |this, cx| {
            let res = blocking(move || streams::ytdlp_streams(&id)).await;
            let _ = this.update(cx, |p, cx| {
                if p.load_serial != serial {
                    return;
                }
                match res {
                    Ok(info) => {
                        if p.storyboard.is_none() {
                            p.storyboard = info.storyboard.clone();
                        }
                        if info.is_live {
                            if let Some(v) = p.video.as_mut() {
                                v.is_live = true;
                            }
                        }
                        let target = if info.is_live {
                            p.reload_target()
                        } else {
                            LoadTarget::from_vod(vod_resume)
                        };
                        p.streams = Some(Arc::new(info));
                        p.load_current(target, false, cx);
                    }
                    Err(e) => {
                        p.loading = false;
                        p.error = Some(tf!("Could not play this video: {0}", e));
                    }
                }
                p.sync_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// (Re)loads current streams into mpv with current quality/audio settings.
    fn load_current(&mut self, target: LoadTarget, keep_pause: bool, cx: &mut Context<Self>) {
        self.load_current_with_head(target, keep_pause, None, cx);
    }

    fn load_current_with_head(
        &mut self,
        target: LoadTarget,
        keep_pause: bool,
        probed_head: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let prev_dash_head = probed_head.or_else(|| self.live_dash.as_ref().map(|d| d.latest_head()));
        // mpv restores file-local options when the previous file closes.
        self.dash_probe_serial += 1;
        self.pending_hls_live_behind = None;
        self.live_dash_restart_seen = false;
        self.seek_cache_target = None;
        self.seek_disk_bytes = 0;
        self.cache_ranges = None;
        self.cache_time = 0.0;
        self.live_edge_at = None;
        self.live_dash = None;
        self.live_dash_anchor = None;
        self.live_dash_pending_behind = 0.0;
        self.preloaded_until = 0.0;
        self.frame_progress = None;
        self.set_playback_ready(false, cx);
        let (Some(mpv), Some(info)) = (self.mpv.clone(), self.streams.clone()) else { return };
        if let Some(("open", started)) = self.playback_timing {
            eprintln!("[player] open to stream selection: {} ms ({})", started.elapsed().as_millis(), info.source);
        }
        self.hold_picture = Some(mpv.presented_frames());
        let s = store::settings_snapshot();
        let is_short = self.video.as_ref().is_some_and(|v| v.is_short) && !info.is_live;
        let selection = if info.is_live && self.force_live_hls {
            info.hls_url.as_deref().map(streams::PlaybackStream::Hls)
        } else {
            streams::playback_stream(
                &info,
                self.quality,
                &s.codec_pref,
                self.audio_track.as_deref(),
                self.audio_only,
                self.recovering_picture,
            )
        };
        let mut opening = 0.0;
        let mut is_hls_src = false;
        let mut is_live_dash = false;
        let src = match selection {
            Some(streams::PlaybackStream::Hls(url)) if info.hls_only || info.is_live || is_short => {
                is_hls_src = true;
                let audio = streams::hls_audio_choice(&info, self.audio_track.as_deref());
                let source = super::proxy::playback_hls(url, self.quality, audio.as_deref(), self.recovering_picture);
                let start_sec = match target {
                    LoadTarget::VodTime(t) => t,
                    _ => 0.0,
                };
                if !is_short && !info.is_live && source != url {
                    super::proxy::prefetch_opening(url, self.quality, audio.as_deref(), self.recovering_picture, start_sec);
                }
                let at_start = matches!(target, LoadTarget::VodTime(t) if t == 0.0);
                if is_short && !self.recovering_picture && source != url && at_start {
                    opening = super::shorts_preload::hls_opening_seconds(
                        url,
                        self.quality,
                        audio.as_deref(),
                    )
                    .unwrap_or(0.0);
                }
                Some(source)
            }
            Some(streams::PlaybackStream::Hls(url)) => {
                is_hls_src = true;
                // libmpv only speaks HTTP proxies: with a SOCKS proxy, live HLS goes through the local proxy
                if crate::store::proxy_setting().is_some_and(|p| !crate::store::mpv_http_proxy(&p)) {
                    let audio = streams::hls_audio_choice(&info, self.audio_track.as_deref());
                    Some(super::proxy::local_hls(url, self.quality, audio.as_deref()))
                } else {
                    Some(url.to_owned())
                }
            }
            Some(streams::PlaybackStream::Muxed(m)) if is_short => {
                Some(super::proxy::local_url(&m.url, m.content_length, &m.mime))
            }
            Some(streams::PlaybackStream::Muxed(m)) => Some(m.url.clone()),
            Some(streams::PlaybackStream::Adaptive(v, a)) => {
                if info.is_live
                    && (v.is_some_and(streams::is_live_dash_format)
                        || a.is_some_and(streams::is_live_dash_format))
                {
                    let behind = match target {
                        LoadTarget::LiveBehind(b) if b.is_finite() && b > 0.0 => b,
                        _ => 0.0,
                    };
                    if let Some(handle) = streams::live_dash::open_at_cached(v, a, behind, prev_dash_head) {
                        is_live_dash = true;
                        self.live_dash_pending_behind = behind;
                        let url = handle.url.clone();
                        self.live_dash = Some(handle);
                        Some(url)
                    } else if probed_head.is_none() {
                        // Head cache missed: probe off the UI thread so the window never blocks.
                        if let Some(probe_url) = v.or(a).map(|f| f.url.clone()) {
                            let load_serial = self.load_serial;
                            let probe_serial = self.dash_probe_serial;
                            self.expected_load_gen += 1;
                            self.reply_load_gen = 0;
                            self.active_load_gen = 0;
                            self.loading = true;
                            cx.spawn(async move |this, cx| {
                                let head = blocking(move || streams::live_dash::probe_head(&probe_url)).await;
                                let _ = this.update(cx, |p, cx| {
                                    if p.load_serial != load_serial || p.dash_probe_serial != probe_serial {
                                        return;
                                    }
                                    if let Some(h) = head {
                                        p.load_current_with_head(target, keep_pause, Some(h), cx);
                                    } else if p.streams.as_ref().is_some_and(|i| i.hls_url.is_some()) {
                                        p.force_live_hls = true;
                                        p.load_current_with_head(target, keep_pause, None, cx);
                                    } else {
                                        p.loading = false;
                                        p.error = Some(translate("No playable stream found").into());
                                        p.sync_idle(cx);
                                        cx.notify();
                                    }
                                });
                            })
                            .detach();
                            cx.notify();
                            return;
                        }
                        None
                    } else {
                        info.hls_url.as_deref().map(|url| {
                            is_hls_src = true;
                            let audio = streams::hls_audio_choice(&info, self.audio_track.as_deref());
                            super::proxy::playback_hls(url, self.quality, audio.as_deref(), self.recovering_picture)
                        })
                    }
                } else {
                    streams::mpv_source(v, a, info.length_secs)
                }
            }
            None => None,
        };
        let Some(src) = src else {
            self.error = Some(translate("No playable stream found").into());
            self.loading = false;
            self.sync_idle(cx);
            return;
        };
        let was_paused = self.paused;
        let mut opts = vec![format!("speed={}", self.speed)];
        if self.recovering_picture {
            opts.push("hwdec=no".into());
        }
        if info.is_live {
            // File-local low-latency live options; mpv restores global defaults when
            // the live stream closes so VOD caching and seeking stay unaffected.
            opts.extend(live_loadfile_options(is_hls_src).into_iter().map(str::to_string));
        } else if info.hls_only && !self.recovering_picture {
            // The proxy already selects one HLS rendition. Limit how much of its
            // opening segments libavformat reads before it can start or resume.
            opts.push("demuxer-lavf-analyzeduration=0.8".into());
        }
        // Bound network readahead so previously buffered ranges survive a seek.
        if !info.is_live {
            opts.push("cache-secs=40".into());
        }
        // Never pass `start=` to a dynamic live DASH MPD (FFmpeg's dash demuxer is
        // non-seekable and DVR offset is encoded in `suggestedPresentationDelay`),
        // and for live HLS apply DVR offset after the new rendition reports its own
        // timeline edge so DASH->HLS and variant switches never reuse a stale edge.
        if !is_live_dash {
            match target {
                LoadTarget::VodTime(t) if t.is_finite() && t > 0.0 => {
                    opts.push(format!("start={t}"));
                }
                LoadTarget::LiveBehind(b)
                    if is_hls_src && info.is_live && b.is_finite() && b > LIVE_RELOAD_BEHIND_THRESHOLD =>
                {
                    self.pending_hls_live_behind = Some(b);
                }
                _ => {}
            }
        } else {
            self.time = 0.0;
        }
        if keep_pause && was_paused {
            opts.push("pause=yes".into());
        } else {
            opts.push("pause=no".into());
        }
        if let Some(v) = &self.video {
            opts.push(mpv_media_title_option(&v.title));
        }
        mpv.set("vid", if self.audio_only { "no" } else { "auto" });
        self.loading = true;
        self.expected_load_gen += 1;
        let load_gen = self.expected_load_gen;
        self.reply_load_gen = 0;
        self.active_load_gen = 0;
        // mpv 0.38 (client API 2.3) added an index argument; older libmpv takes options third
        let joined = opts.join(",");
        let res = if mpv.client_api_version() >= (2 << 16) | 3 {
            mpv.command_with_id(load_gen, &["loadfile", &src, "replace", "-1", &joined])
        } else {
            mpv.command_with_id(load_gen, &["loadfile", &src, "replace", &joined])
        };
        if let Err(e) = res {
            self.loading = false;
            self.error = Some(e.to_string());
        } else {
            self.preloaded_until = opening;
        }
        cx.notify();
    }

    pub fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        if self.live_dash.is_some() {
            if !self.loading && self.cache_time > self.time {
                let win = self.live_dvr_window();
                if win > 0.0 {
                    let pos = self.live_position();
                    let ahead = (self.cache_time - self.time).max(0.0);
                    return vec![(pos, (pos + ahead).min(win))];
                }
            }
            return Vec::new();
        }
        let mut ranges = Vec::new();
        if !self.loading {
            if let Some(cached) = &self.cache_ranges {
                ranges.extend_from_slice(cached);
            } else if self.cache_time > self.time {
                ranges.push((self.time, self.cache_time));
            }
        }
        if self.preloaded_until > 0.0 {
            ranges.push((0.0, self.preloaded_until));
        }
        ranges
    }

    /// Applies caption styling settings to mpv (see [`caption_font`] and [`mpv_color`]).
    fn apply_caption_style(&self) {
        let Some(mpv) = &self.mpv else { return };
        let s = store::settings_snapshot();
        mpv.set("sub-font-size", &s.caption_size.to_string());
        mpv.set("sub-font", caption_font(&s.caption_font));
        mpv.set("sub-bold", "no");
        mpv.set("sub-margin-y", &self.caption_margin.get().to_string());
        mpv.set("sub-color", &mpv_color(s.caption_color, s.caption_opacity));
        if s.caption_background {
            mpv.set("sub-border-style", "background-box");
            mpv.set("sub-back-color", &mpv_color(s.caption_bg_color, s.caption_bg_opacity));
            mpv.set("sub-border-size", "2");
            mpv.set("sub-shadow-offset", "0");
        } else {
            mpv.set("sub-border-style", "outline-and-shadow");
            mpv.set("sub-border-color", "#FF000000");
            mpv.set("sub-shadow-color", "#80000000");
            let (border, shadow) = match s.caption_edge.as_str() {
                "outline" => ("2.5", "0"),
                "shadow" => ("0", "2"),
                "none" => ("0", "0"),
                _ => ("2.5", "1"),
            };
            mpv.set("sub-border-size", border);
            mpv.set("sub-shadow-offset", shadow);
        }
    }

    /// Re-applies the caption look after a caption setting changed.
    pub fn restyle_captions(&self) {
        self.apply_caption_style();
    }

    /// Re-applies settings that affect the current playback.
    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let s = store::settings_snapshot();
        self.loop_video = s.loop_video;
        self.compressor_enabled = store::effective_compressor(self.channel_id.as_deref());
        self.compressor_strength = Strength::parse(&s.audio_compressor_strength);
        self.apply_audio_filters();
        if let Some(m) = &self.mpv {
            m.set("hwdec", if s.hwdec { "auto-copy" } else { "no" });
        }
        self.apply_caption_style();
        if self.video.is_some() {
            if s.audio_only != self.audio_only {
                self.set_audio_only(s.audio_only, cx);
            }
            let want_caps = s.captions_enabled;
            if want_caps {
                let desired = self.streams.as_ref().and_then(|i| pick_caption(i, &s.captions_lang));
                if desired != self.caption {
                    self.caption = desired;
                    self.apply_caption();
                }
            } else if self.caption.is_some() {
                self.caption = None;
                self.apply_caption();
            }
        }
        cx.notify();
    }

    fn apply_caption(&mut self) {
        self.apply_caption_style();
        let (Some(mpv), Some(info)) = (&self.mpv, &self.streams) else { return };
        let _ = mpv.command(&["sub-remove"]);
        if let Some(c) = self.caption.and_then(|i| info.captions.get(i)) {
            let url = if !info.is_live {
                crate::player::proxy::local_caption(&c.url)
            } else if crate::store::proxy_setting().is_some() {
                crate::player::proxy::local_pass(&c.url)
            } else {
                c.url.clone()
            };
            let _ = mpv.command(&["sub-add", &url, "select", &c.name, &c.lang]);
        } else {
            mpv.set("sid", "no");
        }
    }

    pub fn set_caption(&mut self, idx: Option<usize>, cx: &mut Context<Self>) {
        self.caption = idx;
        self.apply_caption();
        if let Some(i) = idx {
            if let Some(c) = self.streams.as_ref().and_then(|s| s.captions.get(i)) {
                let lang = strip_orig_suffix(&c.lang).split('-').next().unwrap_or("en").to_string();
                store::update_settings(|s| {
                    s.captions_enabled = true;
                    s.captions_lang = lang;
                });
            }
        } else {
            store::update_settings(|s| s.captions_enabled = false);
        }
        cx.notify();
    }

    /// Toggles captions on/off (for the `C` shortcut) without overwriting `captions_lang`
    /// so `"original"` stays intact.
    pub fn toggle_captions(&mut self, cx: &mut Context<Self>) {
        if self.caption.is_some() {
            self.caption = None;
            self.apply_caption();
            store::update_settings(|s| s.captions_enabled = false);
        } else {
            let lang = store::settings_snapshot().captions_lang.clone();
            let next = self
                .streams
                .as_ref()
                .and_then(|i| pick_caption(i, &lang).or_else(|| (!i.captions.is_empty()).then_some(0)));
            self.caption = next;
            self.apply_caption();
            if next.is_some() {
                store::update_settings(|s| s.captions_enabled = true);
            }
        }
        cx.notify();
    }

    /// Where "Jump ahead" goes: the next part of the video most viewers skip to.
    pub fn jump_ahead_target(&self) -> Option<f64> {
        let from = self.time + 5.0;
        let later: Vec<&(f64, f64, f32)> = self.heatmap.iter().filter(|m| m.0 > from).collect();
        let max = later.iter().map(|m| m.2).fold(0.0f32, f32::max);
        if max <= 0.0 {
            return None;
        }
        // first local peak that is close to the strongest one ahead
        for (i, m) in later.iter().enumerate() {
            let next = later.get(i + 1).map(|n| n.2).unwrap_or(0.0);
            if m.2 >= max * 0.8 && m.2 >= next {
                return Some(m.0);
            }
        }
        None
    }

    fn reload_target(&self) -> LoadTarget {
        if !self.is_live() {
            return LoadTarget::VodTime(self.time);
        }
        if self.live_dash.is_some() {
            let behind = self.live_behind();
            return if behind > LIVE_AT_EDGE_THRESHOLD {
                LoadTarget::LiveBehind(behind)
            } else {
                LoadTarget::LiveEdge
            };
        }
        let edge = self.live_edge();
        match live_reload_start(true, self.time, edge) {
            Some(t) => LoadTarget::LiveBehind((edge - t).max(0.0)),
            None => LoadTarget::LiveEdge,
        }
    }

    pub fn set_quality(&mut self, q: u32, cx: &mut Context<Self>) {
        if q > 0 && store::settings_snapshot().remember_quality {
            store::update_settings(|s| s.default_quality = q);
        }
        self.quality = q;
        let target = self.reload_target();
        self.load_current(target, true, cx);
    }

    pub fn set_audio_track(&mut self, lang: Option<String>, cx: &mut Context<Self>) {
        self.audio_track = lang;
        let target = self.reload_target();
        self.load_current(target, true, cx);
    }

    pub fn set_audio_only(&mut self, on: bool, cx: &mut Context<Self>) {
        self.audio_only = on;
        store::update_settings(|s| s.audio_only = on);
        if let Some(old) = self.frame.take() {
            cx.drop_image(old, None);
        }
        let target = self.reload_target();
        self.load_current(target, true, cx);
    }

    pub fn available_qualities(&self) -> Vec<u32> {
        let mut q: Vec<u32> = self
            .streams
            .as_ref()
            .map(|s| s.video.iter().filter(|f| f.height > 0).map(streams::fmt_class).collect())
            .unwrap_or_default();
        q.sort_unstable_by(|a, b| b.cmp(a));
        q.dedup();
        q
    }

    pub fn audio_tracks(&self) -> Vec<String> {
        let mut t: Vec<String> = self
            .streams
            .as_ref()
            .map(|s| s.audio.iter().filter_map(|a| a.audio_language.clone()).collect())
            .unwrap_or_default();
        t.sort();
        t.dedup();
        t
    }

    pub fn current_height(&self) -> u32 {
        if self.video_size.1 <= 0.0 {
            return 0;
        }
        streams::quality_class(self.video_size.0 as u32, self.video_size.1 as u32)
    }

    /// Playback details for the "Stats for nerds" overlay, read from mpv.
    pub fn stats(&self) -> Vec<(&'static str, String)> {
        let Some(m) = &self.mpv else { return Vec::new() };
        let f = |k: &str| m.get_f64(k);
        let st = |k: &str| m.get_string(k).filter(|s| !s.is_empty());
        let mut out: Vec<(&'static str, String)> = Vec::new();
        if let Some(v) = &self.video {
            let source = self.streams.as_ref().map(|s| s.source).unwrap_or("");
            out.push(("Video ID", if source.is_empty() { v.id.clone() } else { tf!("{0}, via {1}", v.id, source) }));
            if v.is_short {
                let bytes = super::shorts_preload::served_bytes(&v.id);
                out.push((
                    "Read from Shorts preload",
                    tf!("{0} MB", format_args!("{:.2}", bytes as f64 / 1048576.0)),
                ));
            }
        }
        if !self.audio_only {
            let (w, h) = (f("video-params/w").unwrap_or(0.0), f("video-params/h").unwrap_or(0.0));
            let fps = f("container-fps").or_else(|| f("estimated-vf-fps")).unwrap_or(0.0);
            if w > 0.0 {
                out.push(("Resolution", format!("{w:.0}x{h:.0}@{fps:.0}")));
            }
            let dropped = f("frame-drop-count").unwrap_or(0.0) + f("decoder-frame-drop-count").unwrap_or(0.0);
            let shown = f("estimated-frame-number").unwrap_or(0.0);
            out.push(("Frames", tf!("{0} dropped of {1}", format_args!("{:.0}", dropped), format_args!("{:.0}", shown))));
            if let Some(c) = st("video-codec") {
                let pf = st("video-params/hw-pixelformat").or_else(|| st("video-params/pixelformat")).unwrap_or_default();
                out.push(("Video codec", if pf.is_empty() { c } else { format!("{c}, {pf}") }));
            }
            let hw = st("hwdec-current").filter(|s| s != "no").unwrap_or_else(|| translate("off (software)").into());
            out.push(("Hardware decoding", hw));
            let color = [st("video-params/colormatrix"), st("video-params/primaries"), st("video-params/gamma")].into_iter().flatten().collect::<Vec<_>>();
            if !color.is_empty() {
                out.push(("Color", color.join(" / ")));
            }
        }
        if let Some(c) = st("audio-codec-name").or_else(|| st("audio-codec")) {
            let rate = f("audio-params/samplerate").map(|r| tf!(", {0} kHz", format_args!("{:.1}", r / 1000.0))).unwrap_or_default();
            let ch = f("audio-params/channel-count").map(|c| tf!(", {0} ch", format_args!("{:.0}", c))).unwrap_or_default();
            out.push(("Audio codec", format!("{c}{rate}{ch}")));
        }
        if self.compressor_enabled {
            let db = self.compressor_reduction_db().unwrap_or(0.0);
            let level = if db >= 0.05 { tf!("-{0} dB", format_args!("{db:.1}")) } else { translate("0 dB").into() };
            out.push(("Audio compressor", format!("{} ({level})", translate(self.compressor_strength.label()))));
        }
        let kbps = |k: &str| f(k).filter(|b| *b > 0.0).map(|b| tf!("{0} kbps", format_args!("{:.0}", b / 1000.0)));
        let rates = [kbps("video-bitrate").map(|v| tf!("video {0}", v)), kbps("audio-bitrate").map(|a| tf!("audio {0}", a))].into_iter().flatten().collect::<Vec<_>>();
        if !rates.is_empty() {
            out.push(("Bitrate", rates.join(", ")));
        }
        if let Some(b) = f("demuxer-cache-duration") {
            out.push(("Buffer health", tf!("{0} s", format_args!("{:.2}", b))));
        }
        if let Some(sp) = f("cache-speed") {
            out.push(("Connection speed", tf!("{0} Kbps", format_args!("{:.0}", sp * 8.0 / 1000.0))));
        }
        if let Some(mb) = m.get_f64("demuxer-cache-state/fw-bytes") {
            out.push(("Buffered data", tf!("{0} MB", format_args!("{:.1}", mb / 1048576.0))));
        }
        out.push(("Volume", format!("{:.0}%{}", self.volume, if self.muted { translate(" (muted)") } else { "" })));
        out.push(("Speed", format!("{:.2}x", self.speed)));
        if let Some(a) = f("avsync") {
            out.push(("A/V sync", tf!("{0} ms", format_args!("{:+.1}", a * 1000.0))));
        }
        out.push(("Position", tf!("{0} / {1} s", format_args!("{:.1}", self.time), format_args!("{:.1}", self.duration))));
        out
    }

    // ───── transport controls ─────

    pub fn toggle_pause(&mut self, cx: &mut App) {
        if self.eof && !self.is_live() {
            self.eof = false;
            if let Some(video) = &self.video {
                self.history_report = crate::api::web::PlaybackReport::new(video.id.clone(), self.load_serial);
            }
            self.seek_to(0.0);
            self.set_paused(false, cx);
            return;
        }
        let p = !self.paused;
        self.set_paused(p, cx);
    }

    pub fn set_paused(&mut self, p: bool, cx: &mut App) {
        if !p && store::settings_snapshot().resume_playback {
            if let Some(position) = self.updated_position_while_paused() {
                self.seek_to(position);
            }
        }
        if let Some(m) = &self.mpv {
            m.set_flag("pause", p);
        }
        self.update_paused(p);
        self.sync_idle(cx);
    }

    fn updated_position_while_paused(&self) -> Option<f64> {
        if self.is_live()
            || !self.paused || !self.playback_ready || self.loading || self.eof || self.seeking
            || (self.time - self.last_progress_save).abs() >= 0.5
        {
            return None;
        }
        let (position, _) = store::playback_position(&self.video.as_ref()?.id)?;
        (position.is_finite() && (position - self.last_progress_save).abs() >= 1.0).then_some(position)
    }

    fn update_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if !self.loading {
            if let Some(report) = &self.history_report {
                report.progress(self.time, !paused, true);
            }
        }
        if paused && !self.loading {
            self.save_progress(true);
        }
    }

    pub fn seek_to(&mut self, t: f64) {
        self.seek(t, "absolute+exact");
    }

    pub fn seek_interactive(&mut self, t: f64) {
        // Landing on a keyframe avoids fetching and decoding an extra GOP
        // before showing the picture after a large jump through an HLS stream.
        let buffered = t >= self.time && t <= self.cache_time;
        let is_hls = self.is_live() || self.streams.as_ref().is_some_and(|s| s.hls_only);
        let remote_jump = (t - self.time).abs() >= 8.0 && !buffered && is_hls;
        self.seek(t, if remote_jump { "absolute+keyframes" } else { "absolute+exact" });
    }

    fn seek(&mut self, t: f64, mode: &str) {
        // Dynamic live DASH MPDs are non-seekable in FFmpeg; callers must use
        // `seek_live_behind` / `go_live` instead of sending a broken mpv seek.
        if self.live_dash.is_some() {
            return;
        }
        self.playback_timing = std::env::var_os("STREAMTUBE_PLAYBACK_TIMING").map(|_| ("seek", Instant::now()));
        self.frame_progress = None;
        self.preloaded_until = 0.0;
        let live = self.is_live();
        let (t, mode) = if live {
            let edge = self.live_edge();
            let clamped = clamp_live_seek(t, edge);
            // Clamping near the live edge uses keyframe seek so mpv doesn't
            // decode past the last available live frame into EOF.
            let at_edge = edge > 0.0 && t >= (edge - LIVE_EDGE_MARGIN).max(0.0);
            (clamped, if at_edge { "absolute+keyframes" } else { mode })
        } else {
            (clamp_seek(t, self.duration), mode)
        };
        if (t < self.time || self.seek_cache_target.is_some()) && !self.loading
            && self.seek_disk_bytes < SEEK_DISK_CACHE_LIMIT && !live
        {
            let ranges = self.cache_ranges.as_deref().unwrap_or_default();
            let end = ranges.iter().filter(|&&(start, _)| start > t).map(|&(_, end)| end)
                .chain(self.seek_cache_target.map(|(_, end)| end)).fold(t, f64::max);
            self.set_seek_cache_target((t < end).then_some((t, end)));
        }
        self.time = t;
        if let Some(report) = &self.history_report {
            report.seek(t);
        }
        if let Some(m) = &self.mpv {
            self.hold_picture = Some(m.presented_frames());
            let _ = m.command(&["seek", &format!("{t}"), mode]);
        }
    }

    fn set_seek_cache_target(&mut self, target: Option<(f64, f64)>) {
        if self.seek_cache_target.is_some() != target.is_some() {
            if let Some(mpv) = &self.mpv {
                // Write incoming packets to a temporary file while closing the gap.
                // Their headers still use RAM even when the payload is on disk.
                mpv.set("file-local-options/demuxer-max-back-bytes", if target.is_some() { "20MiB" } else { BACK_CACHE_SIZE });
                mpv.set("file-local-options/cache-on-disk", if target.is_some() { "yes" } else { "no" });
            }
        }
        self.seek_cache_target = target;
    }

    pub fn seek_by(&mut self, d: f64) {
        let t = self.time + d;
        self.seek_interactive(t);
    }

    pub fn keyboard_feedback(&self) -> Option<KeyboardFeedback> {
        self.keyboard_feedback.as_ref().map(|(feedback, _)| *feedback)
    }

    pub fn show_keyboard_feedback(&mut self, feedback: KeyboardFeedback, cx: &mut Context<Self>) {
        let hide = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(900)).await;
            let _ = this.update(cx, |player, cx| {
                player.keyboard_feedback = None;
                cx.notify();
            });
        });
        self.keyboard_feedback = Some((feedback, hide));
    }

    pub fn set_volume(&mut self, v: f64) {
        self.set_volume_live(v);
        self.save_volume();
    }

    pub(super) fn set_volume_live(&mut self, v: f64) {
        let v = v.clamp(0.0, 150.0);
        self.volume = v;
        if let Some(m) = &self.mpv {
            m.set_f64("volume", v);
            if self.muted && v > 0.0 {
                m.set_flag("mute", false);
            }
        }
    }

    pub(super) fn save_volume(&self) {
        let value = self.volume.min(100.0);
        if store::settings_snapshot().volume != value {
            store::update_settings(|s| s.volume = value);
        }
    }

    pub fn set_eq_preset(&mut self, preset: &str) {
        let p = if EQ_PRESETS.iter().any(|&(k, _, _)| k == preset) { preset } else { "flat" };
        store::update_settings(|s| s.eq_preset = p.to_string());
        self.apply_audio_filters();
    }

    fn audio_filter_chain(&self, eq_preset: &str) -> String {
        compressor::build_af_chain(eq_filter(eq_preset), self.compressor_enabled.then_some(self.compressor_strength))
    }

    fn apply_audio_filters(&self) {
        if let Some(mpv) = &self.mpv {
            mpv.set("af", &self.audio_filter_chain(&store::settings_snapshot().eq_preset));
        }
    }

    pub(crate) fn set_channel_id(&mut self, channel_id: Option<String>) {
        self.channel_id = channel_id.filter(|id| !id.is_empty());
        let enabled = store::effective_compressor(self.channel_id.as_deref());
        if self.compressor_enabled != enabled {
            self.compressor_enabled = enabled;
            self.apply_audio_filters();
        }
    }

    pub fn toggle_compressor(&mut self, cx: &mut Context<Self>) {
        self.set_compressor_enabled(!self.compressor_enabled, cx);
    }

    fn set_compressor_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let settings = store::settings_snapshot();
        if let Some(id) = self.channel_id.as_deref().filter(|_| settings.remember_compressor_per_channel) {
            store::set_channel_compressor(id, enabled);
        } else {
            store::update_settings(|s| s.audio_compressor = enabled);
        }
        self.compressor_enabled = enabled;
        self.apply_audio_filters();
        Self::sync_other_compressors(cx);
        cx.notify();
    }

    pub fn set_compressor_strength(&mut self, strength: Strength, cx: &mut Context<Self>) {
        if self.compressor_strength == strength {
            return;
        }
        self.compressor_strength = strength;
        store::update_settings(|s| s.audio_compressor_strength = strength.id().to_string());
        if self.compressor_enabled {
            self.apply_audio_filters();
        }
        Self::sync_other_compressors(cx);
        cx.notify();
    }

    fn sync_other_compressors(cx: &mut Context<Self>) {
        let current = cx.entity();
        let strength = Strength::parse(&store::settings_snapshot().audio_compressor_strength);
        for other in tab_players(cx).into_iter().filter(|tab| *tab != current) {
            other.update(cx, |p, cx| {
                let enabled = store::effective_compressor(p.channel_id.as_deref());
                let rebuild = enabled != p.compressor_enabled || enabled && strength != p.compressor_strength;
                p.compressor_enabled = enabled;
                p.compressor_strength = strength;
                if rebuild {
                    p.apply_audio_filters();
                }
                cx.notify();
            });
        }
    }

    pub fn compressor_reduction_db(&self) -> Option<f32> {
        if !self.compressor_enabled || self.paused || self.loading || self.buffering || self.seeking || self.eof {
            return None;
        }
        let mpv = self.mpv.as_ref()?;
        let input = mpv.get_string(compressor::INPUT_METADATA)?;
        let output = mpv.get_string(compressor::OUTPUT_METADATA)?;
        compressor::measure_reduction_db(&input, &output, self.compressor_strength)
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(m) = &self.mpv {
            m.set_flag("mute", self.muted);
        }
    }

    /// Press-and-hold: plays at 2x until `end_boost`, without touching the saved speed.
    pub fn start_boost(&mut self, cx: &mut App) {
        if self.boost.is_some() || self.video.is_none() {
            return;
        }
        self.boost = Some(self.speed);
        if self.paused {
            self.set_paused(false, cx);
        }
        if let Some(m) = &self.mpv {
            m.set_f64("speed", (self.speed * 2.0).clamp(2.0, 4.0));
        }
    }

    pub fn end_boost(&mut self) {
        if let Some(prev) = self.boost.take() {
            if let Some(m) = &self.mpv {
                m.set_f64("speed", prev);
            }
        }
    }

    pub fn set_speed(&mut self, s: f64) {
        let s = (s * 100.0).round() / 100.0;
        let s = s.clamp(0.1, 5.0);
        self.boost = None;
        self.speed = s;
        if let Some(m) = &self.mpv {
            m.set_f64("speed", s);
        }
        if store::settings_snapshot().remember_speed_per_channel {
            if let Some(c) = &self.channel_id {
                store::set_channel_speed(c, s);
            }
        }
    }

    pub fn step_speed(&mut self, increase: bool) {
        let steps = self.speed / 0.25;
        let next = if increase { steps.floor() + 1.0 } else { steps.ceil() - 1.0 };
        self.set_speed(next * 0.25);
    }

    pub fn set_loop(&mut self, l: bool) {
        self.loop_video = l;
    }

    pub fn frame_step(&mut self, forward: bool) {
        if let Some(m) = &self.mpv {
            let _ = m.command(&[if forward { "frame-step" } else { "frame-back-step" }]);
        }
    }

    pub fn screenshot(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = &self.mpv {
            let dir = dirs::picture_dir().unwrap_or_else(std::env::temp_dir).join("Stream Tube");
            let _ = std::fs::create_dir_all(&dir);
            let name = format!(
                "{}-{}.png",
                self.video.as_ref().map(|v| v.id.as_str()).unwrap_or("frame"),
                self.time as u64
            );
            let path = dir.join(name);
            let _ = m.command(&["screenshot-to-file", &path.to_string_lossy(), "video"]);
            toast(cx, tf!("Screenshot saved to {0}", path.display()));
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.playback_timing = None;
        self.keyboard_feedback = None;
        self.preloaded_until = 0.0;
        self.set_playback_ready(false, cx);
        self.save_progress(true);
        self.finish_playback_history();
        self.load_serial += 1;
        self.expected_load_gen += 1;
        self.reply_load_gen = 0;
        self.active_load_gen = 0;
        self.live_dash_restart_seen = false;
        self.dash_probe_serial += 1;
        self.force_live_hls = false;
        self.live_dash_fallback_used = false;
        self.pending_hls_live_behind = None;
        self.live_edge_at = None;
        self.live_dash = None;
        self.live_dash_anchor = None;
        self.live_dash_pending_behind = 0.0;
        self.live_eof_guard.clear();
        if let Some(m) = &self.mpv {
            let _ = m.command(&["stop"]);
        }
        self.video = None;
        self.set_channel_id(None);
        self.streams = None;
        self.loading = false;
        self.autoplay_next = None;
        self.played_before.clear();
        self.reset_queue();
        if let Some(old) = self.frame.take() {
            cx.drop_image(old, None);
        }
        self.sync_idle(cx);
        cx.notify();
    }

    /// Jumps to the start of the next (`dir` > 0) or previous chapter, as Ctrl+Right
    /// and Ctrl+Left do on YouTube. False when there is no chapter that way.
    pub fn seek_chapter(&mut self, dir: i32) -> bool {
        let starts: Vec<f64> = self.chapters.iter().map(|c| c.start).collect();
        match chapter_target(&starts, self.time, dir) {
            Some(t) => {
                self.seek_to(t);
                true
            }
            None => false,
        }
    }

    pub fn current_chapter(&self) -> Option<&Chapter> {
        self.chapters.iter().rev().find(|c| c.start <= self.time + 0.01)
    }

    pub fn highlight(&self) -> Option<&Segment> {
        self.segments.iter().find(|s| s.action == "poi" || s.category == "poi_highlight")
    }
}

/// Applies changed settings to the running player.
pub fn apply_settings(cx: &mut App) {
    for player in tab_players(cx) {
        player.update(cx, |p, cx| p.apply_settings(cx));
    }
}

struct Guard<F: FnMut()>(F);
impl<F: FnMut()> Drop for Guard<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
fn scopeguard<F: FnMut()>(f: F) -> Guard<F> {
    Guard(f)
}

/// Average colour of a BGRA frame from a sparse sample grid, slightly brightened
/// so dark scenes still give a visible glow.
fn average_color(data: &[u8], w: u32, h: u32) -> Option<Rgba> {
    if w < 4 || h < 4 || data.len() < (w * h * 4) as usize {
        return None;
    }
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    for gy in 0..12 {
        for gx in 0..16 {
            let x = (w - 1) * gx / 15;
            let y = (h - 1) * gy / 11;
            let i = ((y * w + x) * 4) as usize;
            b += data[i] as u64;
            g += data[i + 1] as u64;
            r += data[i + 2] as u64;
            n += 1;
        }
    }
    let f = |v: u64| ((v as f32 / n as f32) / 255.0 * 1.25).min(1.0);
    Some(Rgba { r: f(r), g: f(g), b: f(b), a: 1.0 })
}

fn strip_orig_suffix(code: &str) -> &str {
    code.strip_suffix("-orig").unwrap_or(code)
}

fn lang_matches(track: &str, target: &str) -> bool {
    let track = strip_orig_suffix(track);
    let target = strip_orig_suffix(target);
    if target.is_empty() {
        return false;
    }
    if track.eq_ignore_ascii_case(target) {
        return true;
    }
    if track.len() > target.len()
        && track.get(..target.len()).map(|p| p.eq_ignore_ascii_case(target)).unwrap_or(false)
        && track.as_bytes().get(target.len()) == Some(&b'-')
    {
        return true;
    }
    if let Some((t_base, _)) = target.split_once('-') {
        if track.eq_ignore_ascii_case(t_base) {
            return true;
        }
        if track.len() > t_base.len()
            && track.get(..t_base.len()).map(|p| p.eq_ignore_ascii_case(t_base)).unwrap_or(false)
            && track.as_bytes().get(t_base.len()) == Some(&b'-')
        {
            return true;
        }
    }
    false
}

fn pick_for_lang(captions: &[CaptionTrack], target: &str) -> Option<usize> {
    let target_clean = strip_orig_suffix(target);
    captions
        .iter()
        .position(|c| !c.auto && strip_orig_suffix(&c.lang).eq_ignore_ascii_case(target_clean))
        .or_else(|| captions.iter().position(|c| !c.auto && lang_matches(&c.lang, target)))
        .or_else(|| captions.iter().position(|c| c.auto && strip_orig_suffix(&c.lang).eq_ignore_ascii_case(target_clean)))
        .or_else(|| captions.iter().position(|c| c.auto && lang_matches(&c.lang, target)))
}

fn base_lang(code: &str) -> String {
    strip_orig_suffix(code)
        .split('-')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Strips URLs and handles from text so English links in descriptions don't bias language scoring.
fn clean_for_lang_detect(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(1024));
    for token in text.split_whitespace().take(180) {
        let low = token.to_ascii_lowercase();
        if low.starts_with("http://")
            || low.starts_with("https://")
            || low.starts_with("www.")
            || low.contains(".com")
            || low.contains(".org")
            || low.contains(".net")
            || low.contains("youtu.be")
            || token.starts_with('@')
        {
            continue;
        }
        out.push_str(token);
        out.push(' ');
    }
    out
}

/// Scores how strongly `text` matches a given base ISO language code.
fn score_text_for_lang(text: &str, base: &str) -> i32 {
    let mut score = 0i32;

    let mut hangul = 0i32;
    let mut kana = 0i32;
    let mut cjk = 0i32;
    let mut cyrillic = 0i32;
    let mut uk_chars = 0i32;
    let mut arabic = 0i32;
    let mut persian_chars = 0i32;
    let mut urdu_chars = 0i32;
    let mut greek = 0i32;
    let mut hebrew = 0i32;
    let mut devanagari = 0i32;
    let mut thai = 0i32;
    let mut tr_strong = 0i32;
    let mut tr_soft = 0i32;
    let mut az_chars = 0i32;
    let mut es_chars = 0i32;
    let mut de_chars = 0i32;
    let mut pt_chars = 0i32;
    let mut fr_chars = 0i32;
    let mut pl_chars = 0i32;
    let mut vi_chars = 0i32;

    for ch in text.chars() {
        match ch {
            '\u{AC00}'..='\u{D7AF}' | '\u{1100}'..='\u{11FF}' => hangul += 1,
            '\u{3040}'..='\u{30FF}' => kana += 1,
            '\u{4E00}'..='\u{9FFF}' => cjk += 1,
            '\u{0400}'..='\u{04FF}' => {
                cyrillic += 1;
                if matches!(ch, 'ї' | 'Ї' | 'є' | 'Є' | 'і' | 'І' | 'ґ' | 'Ґ') {
                    uk_chars += 1;
                }
            }
            '\u{0600}'..='\u{06FF}' => {
                arabic += 1;
                if matches!(ch, 'پ' | 'چ' | 'ژ' | 'گ' | 'ی' | 'ک') {
                    persian_chars += 1;
                }
                if matches!(ch, 'ٹ' | 'ڈ' | 'ڑ' | 'ں' | 'ے') {
                    urdu_chars += 1;
                }
            }
            '\u{0370}'..='\u{03FF}' => greek += 1,
            '\u{0590}'..='\u{05FF}' => hebrew += 1,
            '\u{0900}'..='\u{097F}' => devanagari += 1,
            '\u{0E00}'..='\u{0E7F}' => thai += 1,
            'ğ' | 'Ğ' | 'ş' | 'Ş' | 'ı' | 'İ' => tr_strong += 1,
            'ç' | 'Ç' | 'ö' | 'Ö' | 'ü' | 'Ü' => tr_soft += 1,
            'ə' | 'Ə' => az_chars += 1,
            'ñ' | 'Ñ' | '¿' | '¡' => es_chars += 1,
            'ß' => de_chars += 2,
            'ä' | 'Ä' => de_chars += 1,
            'ã' | 'Ã' | 'õ' | 'Õ' => pt_chars += 1,
            'é' | 'è' | 'ê' | 'ë' | 'à' | 'ù' | 'ô' | 'î' | 'œ' => fr_chars += 1,
            'ł' | 'Ł' | 'ś' | 'Ś' | 'ź' | 'Ź' | 'ż' | 'Ż' | 'ć' | 'Ć' | 'ń' | 'Ń' | 'ą' | 'ę' => pl_chars += 1,
            'ă' | 'đ' | 'ơ' | 'ư' | 'ệ' | 'ộ' | 'ữ' | 'ả' | 'ẻ' | 'ổ' | 'ớ' | 'ờ' | 'ứ' | 'ừ' => vi_chars += 1,
            _ => {}
        }
    }

    match base {
        "ko" => return hangul * 4,
        "ja" => return kana * 5 + if kana > 0 { cjk } else { 0 },
        "zh" => return if kana == 0 && hangul == 0 { cjk * 3 } else { 0 },
        "ru" => return if uk_chars == 0 { cyrillic * 3 } else { cyrillic },
        "uk" => return uk_chars * 6 + if uk_chars > 0 { cyrillic } else { 0 },
        "bg" | "sr" | "be" | "mk" | "kk" | "ky" | "mn" => return cyrillic * 2,
        "ar" => return if persian_chars == 0 && urdu_chars == 0 { arabic * 3 } else { arabic },
        "fa" => return persian_chars * 5 + if persian_chars > 0 { arabic } else { 0 },
        "ur" => return urdu_chars * 5 + if urdu_chars > 0 { arabic } else { 0 },
        "el" => return greek * 4,
        "iw" | "he" => return hebrew * 4,
        "hi" | "mr" | "ne" => return devanagari * 4,
        "th" => return thai * 4,
        "az" => score += az_chars * 8 + tr_strong * 2,
        "tr" => score += tr_strong * 5 + tr_soft,
        "es" => score += es_chars * 5,
        "de" => score += de_chars * 4 + tr_soft,
        "pt" => score += pt_chars * 5,
        "fr" => score += fr_chars * 2,
        "pl" => score += pl_chars * 5,
        "vi" => score += vi_chars * 5,
        _ => {}
    }

    let stopwords: &[&str] = match base {
        "tr" => &[
            "ve", "bir", "bu", "için", "icin", "ile", "ama", "değil", "degil", "nasıl", "nasil",
            "nedir", "neden", "gibi", "daha", "çok", "cok", "var", "yok", "ben", "sen", "biz",
            "siz", "olan", "olarak", "çünkü", "cunku", "kadar", "şimdi", "simdi", "yeni", "bölüm",
            "bolum", "izle", "kanal", "her", "şey", "sey", "son", "böyle", "boyle", "şu", "su",
            "benim", "senin", "bizim", "oldu", "olur", "olacak", "yapmak", "yaptım", "yaptim",
        ],
        "en" => &[
            "the", "and", "you", "that", "this", "with", "for", "are", "was", "have", "from",
            "your", "what", "how", "why", "when", "where", "about", "into", "just", "like",
            "more", "will", "can", "all", "not", "but", "they", "their", "our", "out", "who",
            "here", "there", "been", "were", "would", "could", "should", "every",
        ],
        "es" => &[
            "que", "los", "las", "por", "para", "con", "una", "uno", "del", "como", "pero",
            "más", "mas", "muy", "este", "esta", "todo", "porque", "cuando", "sobre", "desde",
            "hasta", "donde", "sin", "sus", "ese", "esa", "esto", "qué", "cómo",
        ],
        "fr" => &[
            "les", "des", "une", "pour", "dans", "avec", "pas", "que", "qui", "sur", "est",
            "sont", "plus", "tout", "mais", "comme", "vous", "nous", "cette", "aussi", "bien",
            "faire", "aux", "être", "avoir", "comment", "pourquoi",
        ],
        "de" => &[
            "der", "die", "das", "und", "ist", "ein", "eine", "mit", "für", "fur", "auf",
            "nicht", "von", "den", "dem", "sich", "des", "wie", "auch", "aber", "oder", "wenn",
            "noch", "nur", "über", "uber", "nach", "wir", "ihr", "warum",
        ],
        "pt" => &[
            "que", "não", "nao", "para", "com", "uma", "por", "mais", "como", "mas", "foi",
            "são", "sao", "isso", "este", "esta", "você", "voce", "dos", "das", "tem", "muito",
            "tudo", "pelo", "pela", "porque", "quando",
        ],
        "it" => &[
            "che", "non", "per", "con", "una", "sono", "della", "delle", "degli", "questo",
            "questa", "come", "più", "piu", "anche", "tutto", "nella", "essere", "stato",
            "molto", "perché", "perche", "dove",
        ],
        "pl" => &[
            "nie", "się", "sie", "jest", "jak", "tak", "ale", "czy", "dla", "przez", "tylko",
            "bardzo", "ten", "tego", "tej", "wszystko", "może", "moze", "dlaczego",
        ],
        "nl" => &[
            "het", "een", "van", "voor", "met", "niet", "dat", "zijn", "naar", "ook", "maar",
            "nog", "wel", "deze", "over", "worden", "mijn", "wat", "waarom",
        ],
        "id" | "ms" => &[
            "yang", "dan", "ini", "untuk", "dengan", "tidak", "dari", "pada", "akan", "bisa",
            "saya", "kamu", "ada", "juga", "dalam", "atau", "tapi", "kenapa", "bagaimana",
        ],
        _ => &[],
    };

    if !stopwords.is_empty() {
        for raw_word in text.split(|c: char| !c.is_alphabetic()) {
            if raw_word.len() < 2 {
                continue;
            }
            let w = raw_word.to_lowercase();
            if stopwords.contains(&w.as_str()) {
                score += 3;
            }
        }
    }
    score
}

/// Infers which of the available caption languages best matches the video's title and description.
/// Only returns a language when the match is clear and unambiguous.
fn infer_caption_lang_from_text(info: &StreamInfo) -> Option<String> {
    let title = clean_for_lang_detect(&info.title);
    let desc = clean_for_lang_detect(&info.description);
    if title.is_empty() && desc.is_empty() {
        return None;
    }

    let mut bases: Vec<String> = Vec::new();
    for c in &info.captions {
        let b = base_lang(&c.lang);
        if !b.is_empty() && !bases.contains(&b) {
            bases.push(b);
        }
    }
    if bases.len() < 2 {
        return None;
    }

    let mut best: Option<(String, i32)> = None;
    let mut second_score = 0i32;

    for b in bases {
        // Weight title higher than description because descriptions often contain boilerplate/credits
        let s = score_text_for_lang(&title, &b) * 2 + score_text_for_lang(&desc, &b);
        match &best {
            None => best = Some((b, s)),
            Some((_, bs)) if s > *bs => {
                second_score = *bs;
                best = Some((b, s));
            }
            Some(_) => {
                if s > second_score {
                    second_score = s;
                }
            }
        }
    }

    let (lang, best_score) = best?;
    if best_score >= 6 && (best_score >= second_score * 2 || best_score >= second_score + 6) {
        Some(lang)
    } else {
        None
    }
}

/// Picks the best caption track index for `pref` (either a language code like `"en"`
/// or `crate::locale::ORIGINAL_CAPTION_LANG` for the video's original language).
pub fn pick_caption(info: &StreamInfo, pref: &str) -> Option<usize> {
    if info.captions.is_empty() {
        return None;
    }
    if pref.eq_ignore_ascii_case(crate::locale::ORIGINAL_CAPTION_LANG) {
        let text_lang = infer_caption_lang_from_text(info);

        // 1. Explicit original audio language from stream metadata
        if let Some(orig) = info.original_language.as_deref() {
            if let Some(i) = pick_for_lang(&info.captions, orig) {
                // Guard against background-music misdetection: if `orig` only matched an auto (ASR)
                // track while the video's title/description clearly matches a MANUAL caption track
                // in a different language, prefer the manual track.
                if info.captions[i].auto {
                    if let Some(tl) = text_lang.as_deref() {
                        if !lang_matches(orig, tl) {
                            if let Some(manual_i) = info
                                .captions
                                .iter()
                                .position(|c| !c.auto && lang_matches(&c.lang, tl))
                            {
                                return Some(manual_i);
                            }
                        }
                    }
                }
                return Some(i);
            }
        }

        // 2. When stream metadata doesn't specify an original language (most single-audio videos),
        // use title/description language matching BEFORE blindly trusting auto-generated ASR
        // (which YouTube often misdetects from background music).
        if let Some(tl) = text_lang.as_deref() {
            if let Some(i) = pick_for_lang(&info.captions, tl) {
                return Some(i);
            }
        }

        // 3. Uploader / YouTube default caption track for the original audio track
        if let Some(i) = info.default_caption_index.filter(|&i| i < info.captions.len()) {
            return Some(i);
        }

        // 4. Auto-generated original ASR track (preferring a manual caption of the same language)
        if let Some(asr) = info
            .captions
            .iter()
            .find(|c| c.auto && c.lang.ends_with("-orig"))
            .or_else(|| info.captions.iter().find(|c| c.auto))
        {
            if let Some(i) = pick_for_lang(&info.captions, &asr.lang) {
                return Some(i);
            }
        }

        return Some(0);
    }
    pick_for_lang(&info.captions, pref)
}

/// The font for a caption font choice. libass looks fonts up by name, and finds
/// the nearest match when one isn't installed.
/// Caption looks offered in Settings and in the player's Subtitles/CC options, as (value, label).
pub const CAPTION_FONTS: &[(&str, &str)] = &[("sans", "Sans-serif"), ("serif", "Serif"), ("mono", "Monospace"), ("casual", "Casual")];
pub const CAPTION_SIZES: &[(u32, &str)] = &[(30, "Small"), (40, "size\u{4}Medium"), (52, "Large"), (64, "Extra large")];
pub const CAPTION_COLORS: &[(u32, &str)] = &[
    (0xFFFFFF, "White"),
    (0xFFFF00, "Yellow"),
    (0x00FF00, "Green"),
    (0x00FFFF, "Cyan"),
    (0x0000FF, "Blue"),
    (0xFF00FF, "Magenta"),
    (0xFF0000, "Red"),
    (0x000000, "Black"),
];
pub const CAPTION_OPACITIES: &[(u32, &str)] = &[(25, "25%"), (50, "50%"), (75, "75%"), (100, "100%")];
pub const CAPTION_EDGES: &[(&str, &str)] = &[("outline-shadow", "Outline and shadow"), ("outline", "Outline"), ("shadow", "Drop shadow"), ("none", "None")];

pub fn caption_font(key: &str) -> &'static str {
    if cfg!(windows) {
        match key {
            "serif" => "Georgia",
            "mono" => "Consolas",
            "casual" => "Comic Sans MS",
            _ => "Segoe UI",
        }
    } else {
        match key {
            "serif" => "DejaVu Serif",
            "mono" => "DejaVu Sans Mono",
            "casual" => "Comic Neue",
            _ => "DejaVu Sans",
        }
    }
}

/// 0xRRGGBB plus an opacity in percent, as mpv's "#AARRGGBB".
pub fn mpv_color(rgb: u32, opacity: u32) -> String {
    let a = (opacity.min(100) * 255 + 50) / 100;
    format!("#{a:02X}{:06X}", rgb & 0xFFFFFF)
}

fn mpv_media_title_option(title: &str) -> String {
    // mpv's %length% quoting takes a byte count and keeps punctuation literal.
    format!("force-media-title=%{}%{title}", title.len())
}

fn premature_eof(position: f64, duration: f64) -> bool {
    // libmpv can report EOF for truncated streams. Allow for the last frame's timestamp.
    position.is_finite()
        && position >= 0.0
        && duration.is_finite()
        && duration > 0.0
        && duration - position > (duration * 0.02).clamp(1.0, 2.0)
}

#[cfg(test)]
mod playback_end_tests {
    #[::core::prelude::v1::test]
    fn distinguishes_truncated_playback_from_a_normal_end() {
        assert!(super::premature_eof(3.76, 12.0));
        assert!(!super::premature_eof(11.96, 12.0));
        assert!(!super::premature_eof(0.0, 0.0));
        assert!(!super::premature_eof(0.0, 0.3));
    }
}

#[cfg(test)]
mod media_title_tests {
    #[::core::prelude::v1::test]
    fn quotes_title_using_utf8_byte_length() {
        assert_eq!(
            super::mpv_media_title_option("[É], 日本語"),
            "force-media-title=%15%[É], 日本語"
        );
    }
}

#[cfg(test)]
mod caption_tests {
    use crate::api::types::{CaptionTrack, StreamInfo};

    #[::core::prelude::v1::test]
    fn colors() {
        assert_eq!(super::mpv_color(0xFFFF00, 100), "#FFFFFF00");
        assert_eq!(super::mpv_color(0x000000, 75), "#BF000000");
        assert_eq!(super::mpv_color(0x123456, 0), "#00123456");
    }

    #[::core::prelude::v1::test]
    fn original_uses_stream_language_and_prefers_manual() {
        let info = StreamInfo {
            original_language: Some("ja".into()),
            captions: vec![
                CaptionTrack { lang: "en".into(), name: "English".into(), url: String::new(), json3_url: None, auto: false },
                CaptionTrack { lang: "ja".into(), name: "Japanese (auto-generated)".into(), url: String::new(), json3_url: None, auto: true },
                CaptionTrack { lang: "ja".into(), name: "Japanese".into(), url: String::new(), json3_url: None, auto: false },
            ],
            ..Default::default()
        };
        assert_eq!(super::pick_caption(&info, "original"), Some(2));
    }

    #[::core::prelude::v1::test]
    fn original_prefers_spoken_over_music_asr() {
        // Turkish video where background English song caused English ASR
        let info = StreamInfo {
            title: "Ev Genci Nedir? Gençler Neden Çalışmıyor?".into(),
            description: "Bu bölümde yeni neslin sorunlarını konuşuyoruz. https://instagram.com/test".into(),
            captions: vec![
                CaptionTrack { lang: "en".into(), name: "English (auto-generated)".into(), url: String::new(), json3_url: None, auto: true },
                CaptionTrack { lang: "tr".into(), name: "Turkish".into(), url: String::new(), json3_url: None, auto: false },
            ],
            ..Default::default()
        };
        assert_eq!(super::pick_caption(&info, "original"), Some(1));

        // English video with Turkish fan translation still picks English ASR
        let en_info = StreamInfo {
            title: "How Rockets Work and Why They Don't Fall".into(),
            description: "In this video we explain how orbital mechanics work for beginners.".into(),
            captions: vec![
                CaptionTrack { lang: "en".into(), name: "English (auto-generated)".into(), url: String::new(), json3_url: None, auto: true },
                CaptionTrack { lang: "tr".into(), name: "Turkish".into(), url: String::new(), json3_url: None, auto: false },
            ],
            ..Default::default()
        };
        assert_eq!(super::pick_caption(&en_info, "original"), Some(0));
    }
}

fn live_loadfile_options(is_hls: bool) -> Vec<&'static str> {
    // Benchmarked against libmpv-2.dll on YouTube 5s-segment live HLS:
    // - live_start_index=-1 stalls on segment boundaries (1-3 paused-for-cache hits / 25s);
    // - live_start_index=-2 cuts latency from ~13-19s to ~5-9.8s with 0 stalls.
    // `live_start_index` is an HLS-only FFmpeg option, so omit it for live DASH MPD.
    let mut opts = Vec::with_capacity(4);
    if is_hls {
        opts.push("demuxer-lavf-o=live_start_index=-2");
    }
    opts.push("demuxer-lavf-analyzeduration=0.5");
    opts.push("cache-pause-initial=no");
    opts.push("demuxer-readahead-secs=10");
    opts
}

fn clamp_seek(t: f64, duration: f64) -> f64 {
    if duration > 0.0 {
        t.clamp(0.0, (duration - 0.1).max(0.0))
    } else {
        t.max(0.0)
    }
}

fn clamp_live_seek(t: f64, live_edge: f64) -> f64 {
    if live_edge > 0.0 {
        t.clamp(0.0, (live_edge - LIVE_EDGE_MARGIN).max(0.0))
    } else {
        t.max(0.0)
    }
}

fn live_dash_behind_at(
    base_pts: f64,
    current_pts: f64,
    elapsed_wall_secs: f64,
    base_behind_secs: f64,
    dvr_window_secs: f64,
) -> f64 {
    let advanced_media = if current_pts.is_finite() {
        (current_pts - base_pts).max(0.0)
    } else {
        0.0
    };
    let raw = (base_behind_secs + elapsed_wall_secs.max(0.0) - advanced_media).max(0.0);
    if dvr_window_secs > 0.0 {
        raw.min(dvr_window_secs)
    } else {
        raw
    }
}

fn live_reload_start(is_live: bool, time: f64, live_edge: f64) -> Option<f64> {
    if !is_live {
        return Some(time);
    }
    if live_edge > 0.0 && time > 1.0 && (live_edge - time) > LIVE_RELOAD_BEHIND_THRESHOLD {
        Some(clamp_live_seek(time, live_edge))
    } else {
        None
    }
}

fn is_live_edge_stale(live_edge_at: Option<(f64, Instant)>, paused: bool, now: Instant) -> bool {
    let limit = if paused {
        Duration::from_secs(LIVE_EDGE_PAUSED_STALE_SECS)
    } else {
        Duration::from_secs(LIVE_EDGE_STALE_SECS)
    };
    match live_edge_at {
        Some((_, at)) => now.saturating_duration_since(at) >= limit,
        None => true,
    }
}

fn should_reload_to_go_live(
    edge: f64,
    behind: f64,
    was_eof: bool,
    has_error: bool,
    edge_stale: bool,
) -> bool {
    was_eof || has_error || edge_stale || edge <= 4.0 || behind >= LIVE_GO_LIVE_SEEK_MAX_BEHIND
}

/// Where a chapter jump from `now` lands: the next chapter's start (`dir` > 0), or
/// the previous chapter's start (in the first chapter, its own start).
fn chapter_target(starts: &[f64], now: f64, dir: i32) -> Option<f64> {
    let now = now + 0.01;
    if dir > 0 {
        return starts.iter().copied().find(|&s| s > now);
    }
    let cur = starts.iter().rposition(|&s| s <= now)?;
    Some(starts[cur.saturating_sub(1)])
}

#[cfg(test)]
mod chapter_tests {
    use super::chapter_target;

    #[::core::prelude::v1::test]
    fn jumps() {
        let c = [0.0, 60.0, 120.0];
        assert_eq!(chapter_target(&c, 30.0, 1), Some(60.0));
        assert_eq!(chapter_target(&c, 60.0, 1), Some(120.0));
        assert_eq!(chapter_target(&c, 130.0, 1), None);
        assert_eq!(chapter_target(&c, 90.0, -1), Some(0.0));
        assert_eq!(chapter_target(&c, 60.0, -1), Some(0.0));
        assert_eq!(chapter_target(&c, 10.0, -1), Some(0.0));
        assert_eq!(chapter_target(&[], 10.0, 1), None);
    }
}

#[cfg(test)]
mod seek_tests {
    use super::{
        LIVE_EDGE_PAUSED_STALE_SECS, LIVE_EDGE_STALE_SECS, LiveEofGuard, clamp_live_seek,
        clamp_seek, is_live_edge_stale, live_loadfile_options, live_reload_start,
        should_reload_to_go_live,
    };
    use std::time::{Duration, Instant};

    #[::core::prelude::v1::test]
    fn stays_inside_the_video_and_does_not_panic_when_it_is_very_short() {
        assert_eq!(clamp_seek(30.0, 120.0), 30.0);
        assert_eq!(clamp_seek(120.0, 120.0), 119.9);
        assert_eq!(clamp_seek(-1.0, 120.0), 0.0);
        assert_eq!(clamp_seek(5.0, 0.0), 5.0);
        assert_eq!(clamp_seek(0.05, 0.05), 0.0);
        assert_eq!(clamp_seek(0.1, 0.1), 0.0);
    }

    #[::core::prelude::v1::test]
    fn live_seek_clamps_two_seconds_before_live_edge() {
        assert_eq!(clamp_live_seek(3600.0, 3600.0), 3598.0);
        assert_eq!(clamp_live_seek(3610.0, 3600.0), 3598.0);
        assert_eq!(clamp_live_seek(3599.5, 3600.0), 3598.0);
        assert_eq!(clamp_live_seek(3500.0, 3600.0), 3500.0);
        assert_eq!(clamp_live_seek(-5.0, 3600.0), 0.0);
        assert_eq!(clamp_live_seek(1.5, 1.5), 0.0);
        assert_eq!(clamp_live_seek(10.0, 0.0), 10.0);
    }

    #[::core::prelude::v1::test]
    fn live_reload_start_returns_to_live_edge_unless_rewound_far_back() {
        // VOD always keeps its current time.
        assert_eq!(live_reload_start(false, 42.0, 120.0), Some(42.0));
        // Live near the edge opens fresh at live edge (None).
        assert_eq!(live_reload_start(true, 3592.0, 3600.0), None);
        assert_eq!(live_reload_start(true, 3570.0, 3600.0), None);
        assert_eq!(live_reload_start(true, 10.0, 0.0), None);
        // Live explicitly rewound far into DVR preserves the clamped DVR position.
        assert_eq!(live_reload_start(true, 3200.0, 3600.0), Some(3200.0));
    }

    #[::core::prelude::v1::test]
    fn go_live_reloads_when_eof_stale_far_behind_or_uninitialized() {
        // Fresh edge, recent rewind < 120s, not EOF/error: fast seek.
        assert!(!should_reload_to_go_live(3600.0, 45.0, false, false, false));
        // Stale edge (e.g. cache filled while paused or rewound): must reload.
        assert!(should_reload_to_go_live(3600.0, 45.0, false, false, true));
        // EOF, error, short edge, or >120s behind: must reload.
        assert!(should_reload_to_go_live(3600.0, 5.0, true, false, false));
        assert!(should_reload_to_go_live(3600.0, 5.0, false, true, false));
        assert!(should_reload_to_go_live(3.0, 1.0, false, false, false));
        assert!(should_reload_to_go_live(3600.0, 120.0, false, false, false));
    }

    #[::core::prelude::v1::test]
    fn live_edge_staleness_tightens_while_paused() {
        let t0 = Instant::now();
        assert!(is_live_edge_stale(None, false, t0));
        assert!(!is_live_edge_stale(
            Some((3600.0, t0)),
            false,
            t0 + Duration::from_secs(LIVE_EDGE_STALE_SECS - 1)
        ));
        assert!(is_live_edge_stale(
            Some((3600.0, t0)),
            false,
            t0 + Duration::from_secs(LIVE_EDGE_STALE_SECS)
        ));
        assert!(!is_live_edge_stale(
            Some((3600.0, t0)),
            true,
            t0 + Duration::from_secs(LIVE_EDGE_PAUSED_STALE_SECS - 1)
        ));
        assert!(is_live_edge_stale(
            Some((3600.0, t0)),
            true,
            t0 + Duration::from_secs(LIVE_EDGE_PAUSED_STALE_SECS)
        ));
    }

    #[::core::prelude::v1::test]
    fn live_eof_guard_caps_recoveries_to_two_per_30s() {
        let mut guard = LiveEofGuard::default();
        let t0 = Instant::now();
        assert!(guard.allow(t0));
        assert!(guard.allow(t0 + Duration::from_secs(5)));
        assert!(!guard.allow(t0 + Duration::from_secs(10)));
        assert!(!guard.allow(t0 + Duration::from_secs(29)));
        // Once the first attempt falls out of the 30s window, recovery is allowed again.
        assert!(guard.allow(t0 + Duration::from_secs(31)));
        assert!(!guard.allow(t0 + Duration::from_secs(32)));
        guard.clear();
        assert!(guard.allow(t0 + Duration::from_secs(32)));
    }

    #[::core::prelude::v1::test]
    fn live_loadfile_options_configure_low_latency_hls() {
        let hls = live_loadfile_options(true);
        assert!(hls.contains(&"demuxer-lavf-o=live_start_index=-2"));
        assert!(hls.contains(&"demuxer-lavf-analyzeduration=0.5"));
        assert!(hls.contains(&"cache-pause-initial=no"));
        assert!(hls.contains(&"demuxer-readahead-secs=10"));

        let dash = live_loadfile_options(false);
        assert!(!dash.contains(&"demuxer-lavf-o=live_start_index=-2"));
        assert!(dash.contains(&"demuxer-lavf-analyzeduration=0.5"));
        assert!(dash.contains(&"cache-pause-initial=no"));
        assert!(dash.contains(&"demuxer-readahead-secs=10"));
    }
}

#[cfg(test)]
mod live_state_tests {
    use super::*;

    fn live_player() -> Player {
        let mut p = Player::empty();
        p.video = Some(VideoItem { id: "live123".into(), is_live: true, ..Default::default() });
        p.streams = Some(Arc::new(StreamInfo { is_live: true, ..Default::default() }));
        p.paused = false;
        p.playback_ready = true;
        p
    }

    #[::core::prelude::v1::test]
    fn is_live_checks_both_video_and_streams() {
        let mut p = Player::empty();
        assert!(!p.is_live());

        p.video = Some(VideoItem { id: "v1".into(), is_live: true, ..Default::default() });
        assert!(p.is_live());

        p.video = Some(VideoItem { id: "v1".into(), is_live: false, ..Default::default() });
        assert!(!p.is_live());

        p.streams = Some(Arc::new(StreamInfo { is_live: true, ..Default::default() }));
        assert!(p.is_live());
    }

    #[::core::prelude::v1::test]
    fn live_edge_behind_and_at_edge_calculations() {
        let mut p = live_player();
        assert_eq!(p.live_edge(), 0.0);
        assert_eq!(p.live_behind(), 0.0);
        assert!(p.is_at_live_edge());

        p.duration = -5.0;
        p.cache_time = f64::NAN;
        assert_eq!(p.live_edge(), 0.0);

        p.duration = 3600.0;
        p.cache_time = 3605.0;
        p.time = 3597.0;
        assert_eq!(p.live_edge(), 3605.0);
        assert_eq!(p.live_behind(), 8.0);
        assert!(p.is_at_live_edge());

        // Rewound 20s behind live edge -> no longer at live edge.
        p.time = 3585.0;
        assert_eq!(p.live_behind(), 20.0);
        assert!(!p.is_at_live_edge());

        // At edge timestamp, but paused or eof -> not at live edge.
        p.time = 3600.0;
        p.paused = true;
        assert!(!p.is_at_live_edge());
        p.paused = false;
        p.eof = true;
        assert!(!p.is_at_live_edge());

        // Non-live video always reports 0.0 behind and false for is_at_live_edge.
        let mut vod = Player::empty();
        vod.duration = 120.0;
        vod.time = 10.0;
        vod.paused = false;
        assert_eq!(vod.live_behind(), 0.0);
        assert!(!vod.is_at_live_edge());
    }

    #[::core::prelude::v1::test]
    fn live_seeks_clamp_before_live_edge_and_never_overshoot() {
        let mut p = live_player();
        p.duration = 3600.0;
        p.cache_time = 3604.0;
        p.time = 3595.0;

        // Dragging to 100% or pressing Right arrow clamps to live_edge - 2.0.
        p.seek_interactive(3604.0);
        assert_eq!(p.time, 3602.0);
        p.seek_by(10.0);
        assert_eq!(p.time, 3602.0);
        p.seek_to(3700.0);
        assert_eq!(p.time, 3602.0);

        // Seeking backward into DVR works normally.
        p.seek_interactive(3000.0);
        assert_eq!(p.time, 3000.0);

        // Non-live VOD still uses duration - 0.1 clamp.
        let mut vod = Player::empty();
        vod.duration = 120.0;
        vod.seek_to(120.0);
        assert_eq!(vod.time, 119.9);
    }

    #[::core::prelude::v1::test]
    fn live_skips_paused_resume_and_reloads_at_live_edge_unless_far_back() {
        let mut p = live_player();
        p.paused = true;
        p.duration = 3600.0;
        p.time = 3592.0;
        p.last_progress_save = 3592.0;

        assert_eq!(p.updated_position_while_paused(), None);
        assert_eq!(p.reload_target(), LoadTarget::LiveEdge);

        p.time = 3000.0;
        assert_eq!(p.reload_target(), LoadTarget::LiveBehind(600.0));
    }

    #[::core::prelude::v1::test]
    fn live_dash_behind_tracks_1x_playback_pause_and_2x_catchup() {
        // Playing live at 1x: 10s initial latency stays 10s after 60s wall + 60s media
        assert_eq!(live_dash_behind_at(0.5, 60.5, 60.0, 10.0, 3600.0), 10.0);
        // Paused for 45s: media frozen, behind grows from 10s to 55s
        assert_eq!(live_dash_behind_at(0.5, 0.5, 45.0, 10.0, 3600.0), 55.0);
        // Rewound -13:00 (780s), watching at 2x for 60s: catches up by 60s -> 720s
        assert_eq!(live_dash_behind_at(100.0, 220.0, 60.0, 780.0, 3600.0), 720.0);
        // Clamped to DVR window length
        assert_eq!(live_dash_behind_at(0.0, 0.0, 5000.0, 10.0, 3600.0), 3600.0);
    }

    #[::core::prelude::v1::test]
    fn record_live_edge_tracks_advances_and_detects_staleness() {
        let mut p = live_player();
        let t0 = Instant::now();
        assert!(p.live_edge_is_stale(t0));

        p.duration = 3600.0;
        p.record_live_edge(t0);
        assert!(!p.live_edge_is_stale(t0 + Duration::from_secs(5)));

        // Tiny sub-0.5s jitter does not count as fresh playlist advance.
        p.duration = 3600.2;
        p.record_live_edge(t0 + Duration::from_secs(10));
        p.paused = true;
        assert!(p.live_edge_is_stale(t0 + Duration::from_secs(10)));

        // Real 5s segment advance refreshes the timestamp.
        p.duration = 3605.0;
        p.record_live_edge(t0 + Duration::from_secs(10));
        assert!(!p.live_edge_is_stale(t0 + Duration::from_secs(12)));
    }
}

/// Appends unseen playlist videos to the queue. When `shuffle` is set, the
/// upcoming tail after the playing video is shuffled so newly loaded pages are
/// mixed into playback without moving the current or already-played tracks.
fn extend_queue(
    queue: &mut Vec<VideoItem>,
    pos: Option<usize>,
    incoming: Vec<VideoItem>,
    shuffle: bool,
    rng: &mut impl rand::Rng,
) -> usize {
    let added = crate::api::innertube::append_unique_videos(queue, incoming);
    if added > 0 && shuffle {
        use rand::seq::SliceRandom;
        let after = pos.map_or(0, |i| i + 1).min(queue.len());
        queue[after..].shuffle(rng);
    }
    added
}

#[cfg(test)]
mod queue_extend_tests {
    use super::*;

    fn vids(range: std::ops::Range<u32>) -> Vec<VideoItem> {
        range.map(|i| VideoItem { id: format!("v{i}"), ..Default::default() }).collect()
    }

    #[::core::prelude::v1::test]
    fn extends_in_order_past_200_and_deduplicates() {
        let mut q = vids(0..100);
        let mut rng = rand::rng();
        assert_eq!(extend_queue(&mut q, Some(0), vids(90..200), false, &mut rng), 100);
        assert_eq!(extend_queue(&mut q, Some(50), vids(200..320), false, &mut rng), 120);
        assert_eq!(q.len(), 320);
        assert_eq!(q[199].id, "v199");
        assert_eq!(q[200].id, "v200");
        assert_eq!(q[319].id, "v319");
    }

    #[::core::prelude::v1::test]
    fn shuffled_extension_keeps_played_prefix_and_includes_all_new_items() {
        let mut q = vids(0..100);
        let mut rng = rand::rng();
        let added = extend_queue(&mut q, Some(2), vids(100..250), true, &mut rng);
        assert_eq!(added, 150);
        assert_eq!(q.len(), 250);
        // current track and already-played prefix stay fixed
        assert_eq!((q[0].id.as_str(), q[1].id.as_str(), q[2].id.as_str()), ("v0", "v1", "v2"));
        let unique: std::collections::HashSet<&str> = q.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(unique.len(), 250);
    }
}
