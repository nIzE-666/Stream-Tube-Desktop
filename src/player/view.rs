//! Video surface + YouTube-style controls.
use super::aspect;
use super::compressor;
use super::controller::{KeyboardFeedback, Player, CAPTION_MARGIN};
use crate::i18n::{t as translate, tf};
use crate::api::json::format_duration;
use crate::api::services;
use crate::store;
use crate::ui::app::{Route, UiState, navigate, open_menu};
use crate::ui::cards::download_menu_items;
use crate::ui::{icon, icon_button, pal};
use gpui_kit::*;
use gpui_kit::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn playback_error_message(error: &str) -> String {
    if let Some((_, signed_in_error)) = error.rsplit_once("\nyt-dlp (signed in): ") {
        return if crate::api::streams::verification_requested(signed_in_error) {
            crate::i18n::t("YouTube requested verification again when opening this video with your account.").into()
        } else if signed_in_error == "No playable stream found" {
            crate::i18n::t("No playable stream found").into()
        } else {
            crate::ui::friendly_error(signed_in_error)
        };
    }
    if error == "No playable stream found" {
        return crate::i18n::t("No playable stream found").into();
    }
    let lower = error.to_ascii_lowercase();
    if lower.contains("sign in to confirm") && (lower.contains("not a bot") || lower.contains("your age")) {
        let msg = if crate::api::web::connected() {
            "YouTube requested verification before providing a video stream."
        } else {
            "YouTube blocked anonymous playback for this video. Connect “YouTube watch history” in Settings to play it with your account, or try another network."
        };
        return crate::i18n::t(msg).into();
    }
    crate::ui::friendly_error(error)
}

#[cfg(test)]
mod playback_error_tests {
    use super::playback_error_message;

    #[::core::prelude::v1::test]
    fn explains_youtube_bot_check_errors() {
        let message = playback_error_message("visionOS: Sign in to confirm you’re not a bot");
        assert!(message.contains("anonymous playback"));
        assert!(message.contains("try another network."));
    }

    #[::core::prelude::v1::test]
    fn keeps_unrelated_playback_errors() {
        let error = "Playback error: invalid parameter";
        assert_eq!(playback_error_message(error), error);
    }

    #[::core::prelude::v1::test]
    fn keeps_the_premature_end_notice() {
        let error = "Playback stopped before the end of the video.";
        assert_eq!(playback_error_message(error), error);
    }
}

/// How close to the live edge playback counts as "at live" (seconds).
const LIVE_EDGE_TOLERANCE_SECS: f64 = 15.0;
/// How close to the live edge seekbar hover/click snaps to "LIVE" (seconds).
const LIVE_HOVER_EDGE_SECS: f64 = 5.0;

fn player_is_live(pl: &Player) -> bool {
    pl.is_live()
}

fn player_live_window(pl: &Player) -> f64 {
    pl.live_dvr_window()
}

fn player_live_pos(pl: &Player) -> f64 {
    pl.live_position()
}

fn player_live_behind(pl: &Player) -> f64 {
    pl.live_behind()
}

fn player_at_live_edge(pl: &Player) -> bool {
    pl.is_at_live_edge()
}

fn player_go_live(p: &mut Player, cx: &mut Context<Player>) {
    p.go_live(cx);
}

fn live_indicator_label(at_live_edge: bool, behind_secs: f64, live_label: &str) -> String {
    if at_live_edge || behind_secs < 1.0 {
        live_label.to_string()
    } else {
        format!("-{}  {}", format_duration(behind_secs as u64), live_label)
    }
}

fn live_hover_label(t: f64, edge: f64, live_label: &str) -> (String, bool) {
    let behind = (edge - t).max(0.0);
    if behind <= LIVE_HOVER_EDGE_SECS {
        (live_label.to_string(), true)
    } else {
        (format!("-{}", format_duration(behind as u64)), false)
    }
}

fn seekbar_fraction(time: f64, duration: f64, is_live: bool, at_live_edge: bool, drag_seek: Option<f32>) -> f32 {
    if let Some(f) = drag_seek {
        return f.clamp(0.0, 1.0);
    }
    if is_live && at_live_edge {
        return 1.0;
    }
    if duration > 0.0 {
        (time / duration).clamp(0.0, 1.0) as f32
    } else if is_live {
        1.0
    } else {
        0.0
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Main,
    Quality,
    Speed,
    Captions,
    Audio,
    Equalizer,
    Compressor,
    Aspect,
    AudioOnly,
    Ambient,
    Annotations,
    /// Subtitles/CC > Options: how captions look
    CaptionOptions,
    CaptionPick(CapField),
}

/// One caption look setting, picked from Subtitles/CC > Options (saved with the settings).
#[derive(Clone, Copy, PartialEq)]
enum CapField {
    Font,
    Color,
    Size,
    Opacity,
    Background,
    BgColor,
    BgOpacity,
    Edge,
}

pub struct PlayerView {
    player: Entity<Player>,
    /// Repaints "Stats for nerds" every second while it's open (one task, not one per frame)
    stats_tick: Option<Task<()>>,
    /// Repaints the live `-MM:SS` indicator every second while paused or behind live with controls shown.
    live_tick: Option<Task<()>>,
    comp_hover: bool,
    comp_tick: Option<Task<()>>,
    comp_reduction_db: f32,
    pub mini: bool,
    /// shown in the picture-in-picture window
    pub pip: bool,
    show_until: Instant,
    hovering: bool,
    menu: Option<Menu>,
    /// the whole player, measured each frame (captions sit above the controls)
    root_bounds: Rc<Cell<Bounds<Pixels>>>,
    menu_closed_at: std::time::Instant,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    vol_bounds: Rc<Cell<Bounds<Pixels>>>,
    hover_frac: Option<f32>,
    drag_seek: Option<f32>,
    /// A press near the seek bar started a seek; the click layer ignores it.
    zone_seek: Option<Instant>,
    drag_vol: bool,
    /// pointer on the volume button/slider: in the narrow (Shorts) frame the slider
    /// only slides out then, like YouTube's
    vol_hover: bool,
    last_click: Option<Instant>,
    /// left button held on the video; after a moment it plays at 2x
    press: Option<Instant>,
    press_boosted: bool,
    /// picture-in-picture: where a left press on the video started; moving
    /// from there drags the whole window, like a browser's PiP
    pip_press: Option<Point<Pixels>>,
    /// the last press moved the PiP window, so its release is not a click
    pip_moved: bool,
    /// a window move was just requested; the system echoes it as a press
    pip_move_echo: bool,
    /// the player's area, for placing annotations over the picture
    area: Rc<Cell<Bounds<Pixels>>>,
    /// the annotation under the pointer (shows a highlight's rollover text)
    hover_annotation: Option<String>,
    _sub: Subscription,
}

impl PlayerView {
    pub fn new(player: Entity<Player>, mini: bool, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&player, |_, _, cx| cx.notify());
        Self {
            stats_tick: None,
            live_tick: None,
            comp_hover: false,
            comp_tick: None,
            comp_reduction_db: 0.0,
            player,
            mini,
            pip: false,
            show_until: Instant::now() + Duration::from_secs(3),
            hovering: false,
            menu: None,
            root_bounds: Rc::new(Cell::new(Bounds::default())),
            menu_closed_at: std::time::Instant::now() - std::time::Duration::from_secs(5),
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            vol_bounds: Rc::new(Cell::new(Bounds::default())),
            hover_frac: None,
            drag_seek: None,
            zone_seek: None,
            drag_vol: false,
            vol_hover: false,
            last_click: None,
            press: None,
            press_boosted: false,
            pip_press: None,
            pip_moved: false,
            pip_move_echo: false,
            area: Rc::new(Cell::new(Bounds::default())),
            hover_annotation: None,
            _sub: sub,
        }
    }

    fn poke(&mut self, cx: &mut Context<Self>) {
        let hide = Duration::from_millis(store::settings_snapshot().controls_hide_ms.max(800));
        self.show_until = Instant::now() + hide;
        cx.notify();
        let until = self.show_until;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(hide + Duration::from_millis(100)).await;
            let _ = this.update(cx, |v, cx| {
                if v.show_until == until {
                    // the pointer goes with the controls while it's over the video,
                    // and comes back with them when the mouse moves
                    let paused = v.player.read(cx).paused;
                    if v.hovering && !v.mini && !v.controls_visible(paused) {
                        hide_pointer_until_moved();
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn frac_in(b: Bounds<Pixels>, x: Pixels) -> f32 {
        let w: f32 = b.size.width.into();
        if w <= 0.0 {
            return 0.0;
        }
        let dx: f32 = (x - b.origin.x).into();
        (dx / w).clamp(0.0, 1.0)
    }

    fn controls_visible(&self, paused: bool) -> bool {
        paused || self.menu.is_some() || self.drag_seek.is_some() || self.comp_hover || Instant::now() < self.show_until
    }

    /// Closes the gear menu if it is open (Esc closes it before leaving the page).
    pub fn close_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.menu.take().is_none() {
            return false;
        }
        self.menu_closed_at = std::time::Instant::now();
        cx.notify();
        true
    }

    pub(crate) fn compressor_hovered(&self) -> bool {
        self.comp_hover
    }

    pub(crate) fn clear_compressor_hover(&mut self) {
        self.comp_hover = false;
        self.comp_tick = None;
        self.comp_reduction_db = 0.0;
    }

    fn sync_compressor_meter(&mut self, cx: &mut Context<Self>) {
        let pl = self.player.read(cx);
        if !self.comp_hover
            || self.menu.is_some()
            || !pl.compressor_enabled
            || pl.video.is_none()
            || pl.paused
            || pl.loading
            || pl.buffering
            || pl.seeking
            || pl.eof
        {
            self.comp_tick = None;
            self.comp_reduction_db = 0.0;
            return;
        }
        if self.comp_tick.is_none() {
            if let Some(db) = pl.compressor_reduction_db() {
                self.comp_reduction_db = db;
            }
            self.comp_tick = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                if this.update(cx, |this, cx| {
                    let measured = this.player.read(cx).compressor_reduction_db();
                    this.comp_reduction_db = measured
                        .map(|db| compressor::smooth_reduction_db(this.comp_reduction_db, db))
                        .unwrap_or(0.0);
                    cx.notify();
                }).is_err() {
                    break;
                }
            }));
        }
    }

    fn finish_pointer_press(&mut self, cx: &mut Context<Self>) {
        if self.press.take().is_some() && self.press_boosted {
            self.player.update(cx, |p, _| p.end_boost());
            cx.notify();
        }
    }
}

/// Hides the mouse pointer until it moves again. GPUI has no public call for
/// this, so it's per platform; Linux keeps the pointer for now.
fn hide_pointer_until_moved() {
    #[cfg(target_os = "macos")]
    unsafe {
        let _: () = objc2::msg_send![objc2::class!(NSCursor), setHiddenUntilMouseMoves: true];
    }
    // GPUI's Windows backend sets the pointer again on the next WM_SETCURSOR,
    // which comes with the next mouse move (its own keyboard hide works the same way)
    #[cfg(target_os = "windows")]
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetCursor(None);
    }
}

/// Saves the aspect mode (a value of `aspect::MODES`) and redraws every player with it.
pub fn set_aspect_mode(value: &str, cx: &mut App) {
    store::update_settings(|s| s.aspect_mode = value.to_string());
    cx.refresh_windows();
}

fn speed_label(s: f64) -> String {
    if (s - 1.0).abs() < 0.001 { translate("Normal").into() } else { format!("{s}") }
}

pub(crate) fn mini_player_radius() -> Rems {
    rems(0.75 * crate::ui::thumbnail_corner_scale())
}

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::add(&crate::perf::VIEW_RENDERS, 1);
        let p = pal(cx);
        let pl = self.player.read(cx);
        let mini = self.mini;
        let paused = pl.paused;
        let is_live = player_is_live(pl);
        let duration = if is_live { player_live_window(pl) } else { pl.duration };
        let current_time = if is_live { player_live_pos(pl) } else { pl.time };
        let time = self.drag_seek.map(|f| f as f64 * duration).unwrap_or(current_time);
        let frame = pl.frame.clone();
        let loading = pl.loading || pl.buffering || (pl.seeking && !pl.paused);
        let error = pl.error.clone().or(pl.init_error.clone());
        let audio_only = pl.audio_only;
        let video = pl.video.clone();
        let mpv = pl.mpv();
        let vsize = pl.video_size;
        let show = self.controls_visible(paused) || mini && self.hovering;
        // like youtube.com, captions move up over the controls while they show
        // and back down when they hide
        let caption_margin = {
            let (r, s) = (self.root_bounds.get(), self.seek_bounds.get());
            let rh = f32::from(r.size.height);
            let lift = f32::from(r.origin.y + r.size.height - s.origin.y) + 10.0;
            if show && !mini && rh > 0.0 && f32::from(s.size.height) > 0.0 && lift > 0.0 && lift < rh / 2.0 {
                ((lift * 720.0 / rh).ceil() as u32).max(CAPTION_MARGIN)
            } else {
                CAPTION_MARGIN
            }
        };
        pl.set_caption_margin(caption_margin);
        let ui = *cx.global::<UiState>();
        let scale = window.scale_factor();
        let view = cx.entity().downgrade();
        let settings = store::settings_snapshot();
        let aspect = aspect::Mode::parse(&settings.aspect_mode);
        let full_height = settings.theater_mode
            && settings.theater_full_height
            && !ui.fullscreen
            && !ui.shorts
            && !mini
            && !self.pip;
        let aspect = if full_height { aspect.without_zoom() } else { aspect };
        let auto_trim = pl.auto_crop.trim;
        let short_cover = video.as_ref().filter(|_| ui.shorts && !mini && !self.pip && !audio_only && (pl.loading || frame.is_none()) && error.is_none());
        let picture = frame.clone().filter(|_| !audio_only && short_cover.is_none());

        // ── video surface ──
        // A card's overflow_hidden clips to a rectangle in GPUI, so the player rounds its
        // own corners: all four on the watch page, the top two in the mini player (its
        // title sits below), none when it fills the window.
        let round_all = !mini && !self.pip && !ui.fullscreen && !ui.windowed && !settings.theater_mode;
        // The mini player's video sits inside the card's one-pixel border.
        let radius = if mini { (mini_player_radius().to_pixels(window.rem_size()) - px(1.)).max(px(0.)) } else { px(12.) };
        let corners = if round_all {
            Corners::all(radius)
        } else if mini {
            Corners { top_left: radius, top_right: radius, bottom_left: px(0.), bottom_right: px(0.) }
        } else {
            Corners::default()
        };
        let surface = div()
            .absolute()
            .inset_0()
            .when(mini, |d| d.rounded_t(radius))
            .when(round_all, |d| d.rounded_xl())
            .bg(black())
            .child(
                canvas(
                    move |bounds, _, _| {
                        if let Some(m) = &mpv {
                            // Render at the video's own resolution (capped at 1080p): mpv then
                            // Render no larger than the video's native size, the viewport's
                            // physical pixel box, or the configured cap (default 720p on aarch64 Linux,
                            // 1080p elsewhere): keeps mini-player and tablet playback light while GPU scales cleanly.
                            static MAX_H: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
                            let max_h = *MAX_H.get_or_init(|| {
                                std::env::var("STREAMTUBE_MAX_RENDER_H")
                                    .ok()
                                    .and_then(|v| v.parse::<f32>().ok())
                                    .map(|v| v.clamp(360.0, 2160.0))
                                    .unwrap_or_else(|| {
                                        if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
                                            720.0
                                        } else {
                                            1080.0
                                        }
                                    })
                            });
                            let max_w = max_h * (16.0 / 9.0);
                            let aw = (f32::from(bounds.size.width) * scale).max(16.0);
                            let ah = (f32::from(bounds.size.height) * scale).max(16.0);
                            let (vw, vh) = (vsize.0 as f32, vsize.1 as f32);
                            if vw > 0.0 && vh > 0.0 {
                                let cap_w = max_w.min(aw);
                                let cap_h = max_h.min(ah);
                                let k = (cap_w / vw).min(cap_h / vh).min(1.0);
                                m.set_render_size(((vw * k).round() as u32).max(16), ((vh * k).round() as u32).max(16));
                            } else {
                                m.set_render_size((aw as u32).min(1280), (ah as u32).min(720));
                            }
                        }
                    },
                    move |bounds, _, window, _| {
                        // the frame, cropped / zoomed for the aspect mode; what falls
                        // outside the player is clipped by paint_image
                        if let Some(f) = picture {
                            let dims = f.size(0);
                            let fsize = (i32::from(dims.width) as f32, i32::from(dims.height) as f32);
                            let (region, fit) = aspect::resolve(aspect, fsize, (vsize.0 as f32, vsize.1 as f32), auto_trim);
                            let layout = aspect::layout((f32::from(bounds.size.width), f32::from(bounds.size.height)), fsize, region, fit);
                            let (x, y, w, h) = layout;
                            let image = Bounds::new(bounds.origin + point(px(x), px(y)), size(px(w), px(h)));
                            let clip = if full_height {
                                let (x, y, w, h) = region.in_frame(layout);
                                let visible = Bounds::new(bounds.origin + point(px(x), px(y)), size(px(w), px(h)));
                                bounds.intersect(&visible)
                            } else {
                                bounds
                            };
                            let _ = window.paint_image(clip, image, corners, f, 0, false);
                        }
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                                let _ = view.update(cx, |view, cx| view.finish_pointer_press(cx));
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .when_some(short_cover, |d, v| {
                d.child(img(SharedString::from(v.thumbnail.clone())).absolute().size_full().object_fit(ObjectFit::Cover))
            })
            .when(audio_only, |d| {
                d.when_some(video.as_ref().map(|v| v.thumbnail.clone()), |d, t| {
                    d.child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(img(SharedString::from(t)).h(relative(0.7)).rounded_xl().object_fit(ObjectFit::Contain))
                            .child(
                                div()
                                    .absolute()
                                    .top_3()
                                    .left_3()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(hsla(0., 0., 0., 0.6))
                                    .text_xs()
                                    .text_color(white())
                                    .child(icon("headphones").size_4().text_color(white()))
                                    .child(translate("Audio only")),
                            ),
                    )
                })
            });

        let mut root = div()
            .id(if mini { "player-mini" } else { "player-main" })
            .relative()
            .size_full()
            .overflow_hidden()
            .when(mini, |d| d.rounded_t(radius))
            .when(round_all, |d| d.rounded_xl())
            .bg(black())
            .child({
                let cell = self.root_bounds.clone();
                canvas(move |b, _, _| cell.set(b), |_, _, _, _| {}).absolute().size_full()
            })
            .on_hover(cx.listener(|this, h: &bool, _, cx| {
                this.hovering = *h;
                if !*h {
                    this.hover_frac = None;
                    this.clear_compressor_hover();
                    this.finish_pointer_press(cx);
                }
                cx.notify();
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                // the button was let go somewhere we didn't see
                if ev.pressed_button != Some(MouseButton::Left) {
                    this.finish_pointer_press(cx);
                    this.pip_press = None;
                }
                // PiP: dragging the picture moves the window
                if let Some(origin) = this.pip_press.filter(|_| this.drag_seek.is_none() && !this.drag_vol) {
                    if (ev.position - origin).magnitude() >= 5. {
                        this.pip_press = None;
                        this.pip_moved = true;
                        this.pip_move_echo = true;
                        crate::ui::pip::begin_window_move(window);
                        return;
                    }
                }
                // a seek or volume drag whose release we missed (something under
                // the pointer kept it) ends here, where the button was last held
                if (this.drag_seek.is_some() || this.drag_vol) && ev.pressed_button != Some(MouseButton::Left) {
                    this.finish_drag(cx);
                }
                if let Some(_) = this.drag_seek {
                    let f = Self::frac_in(this.seek_bounds.get(), ev.position.x);
                    this.drag_seek = Some(f);
                    this.hover_frac = Some(f);
                } else if this.drag_vol {
                    let f = Self::frac_in(this.vol_bounds.get(), ev.position.x);
                    this.player.update(cx, |p, _| p.set_volume_live(f as f64 * 100.0));
                } else {
                    let sb = this.seek_bounds.get();
                    let near = ev.position.y >= sb.origin.y - px(12.) && ev.position.y <= sb.origin.y + sb.size.height + px(8.);
                    this.hover_frac = if near && ev.position.x >= sb.origin.x && ev.position.x <= sb.origin.x + sb.size.width {
                        Some(Self::frac_in(sb, ev.position.x))
                    } else {
                        None
                    };
                }
                this.poke(cx);
            }))
            // a press anywhere the seek preview shows (just above/below the
            // thin bar) seeks there, the same as pressing the bar itself
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    if this.mini || this.drag_seek.is_some() || this.hover_frac.is_none() {
                        return;
                    }
                    let f = Self::frac_in(this.seek_bounds.get(), ev.position.x);
                    this.drag_seek = Some(f);
                    this.hover_frac = Some(f);
                    this.zone_seek = Some(Instant::now());
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.finish_drag(cx)),
            )
            .child(surface);

        // click layer: pause / double-click fullscreen
        root = root.child(
            div()
                .id("click-layer")
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        if this.pip {
                            if std::mem::take(&mut this.pip_move_echo) {
                                return;
                            }
                            this.pip_moved = false;
                            this.pip_press = Some(ev.position);
                            return;
                        }
                        if this.mini {
                            return;
                        }
                        let at = Instant::now();
                        this.press = Some(at);
                        this.press_boosted = false;
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(Duration::from_millis(450)).await;
                            let _ = this.update(cx, |this, cx| {
                                if this.press == Some(at)
                                    && this.drag_seek.is_none()
                                    && !player_at_live_edge(this.player.read(cx))
                                {
                                    this.press_boosted = true;
                                    this.player.update(cx, |p, cx| p.start_boost(cx));
                                    cx.notify();
                                }
                            });
                        })
                        .detach();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                        if this.mini {
                            return;
                        }
                        cx.stop_propagation();
                        this.menu = None;
                        let items = context_items(&this.player, this.pip, cx);
                        crate::ui::app::open_player_menu(ev.position, items, window, cx);
                    }),
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    // the release that ends a press-and-hold, or a PiP window move, isn't a click
                    this.pip_press = None;
                    if std::mem::take(&mut this.press_boosted) || std::mem::take(&mut this.pip_moved) {
                        return;
                    }
                    if this.zone_seek.take().map(|t| t.elapsed() < Duration::from_millis(800)).unwrap_or(false) {
                        return;
                    }
                    if this.menu.is_some() || this.menu_closed_at.elapsed() < std::time::Duration::from_millis(400) {
                        this.menu = None;
                        cx.notify();
                        return;
                    }
                    if this.mini {
                        navigate_to_current(this, window, cx);
                        return;
                    }
                    let now = Instant::now();
                    let dbl = this.last_click.map(|t| now.duration_since(t) < Duration::from_millis(300)).unwrap_or(false);
                    this.last_click = Some(now);
                    if dbl && store::settings_snapshot().double_click_fullscreen {
                        crate::ui::app::toggle_fullscreen(window, cx);
                        // undo the pause toggle from the first click
                        this.player.update(cx, |p, cx| p.toggle_pause(cx));
                    } else {
                        this.player.update(cx, |p, cx| p.toggle_pause(cx));
                    }
                    this.poke(cx);
                })),
        );

        if !mini && !audio_only && !is_live {
            if let Some(layer) = self.render_annotations(time, vsize, aspect, full_height, cx) {
                root = root.child(layer);
            }
        }

        let player = self.player.read(cx);
        let feedback = match player.keyboard_feedback() {
            Some(KeyboardFeedback::Volume(volume)) => Some(("volume-2", tf!("{0}%", volume.round()))),
            Some(KeyboardFeedback::Seek(seconds)) => {
                let icon = if seconds < 0.0 { "arrow-left" } else { "arrow-right" };
                let seconds = (seconds * 10.0).round() / 10.0;
                let label = if seconds == 0.0 { "0".into() } else { format!("{seconds:+}") };
                Some((icon, tf!("{0} s", label)))
            }
            None if player.boost.is_some() => Some(("fast-forward", "2x".into())),
            None => None,
        };
        if let Some((feedback_icon, text)) = feedback {
            root = root.child(
                div().absolute().top_4().left_0().right_0().flex().justify_center().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_3()
                        .py_1()
                        .rounded_full()
                        .bg(hsla(0., 0., 0., 0.6))
                        .text_color(white())
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(text)
                        .child(icon(feedback_icon).size_4().text_color(white())),
                ),
            );
        }

        // loading / error overlays
        if let Some(e) = error {
            let retry_player = self.player.clone();
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .p_6()
                    .when(mini, |d| d.rounded_t(radius))
                    .bg(hsla(0., 0., 0., 0.85))
                    .child(icon("circle-alert").size_10().text_color(white()))
                    .child(div().text_color(white()).text_center().max_w(px(560.)).text_sm().child(playback_error_message(&e)))
                    .when(!mini, |d| {
                        d.child(
                            div().flex().flex_wrap().justify_center().gap_2().child(
                                div()
                                    .id("retry")
                                    .px_4()
                                    .py_2()
                                    .rounded_full()
                                    .bg(white())
                                    .text_color(black())
                                    .text_sm()
                                    .cursor_pointer()
                                    .child(translate("Retry"))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        retry_player.update(cx, |p, cx| {
                                            if let Some(v) = p.video.clone() {
                                                let t = if player_is_live(p) { None } else { Some(p.time) };
                                                p.play(v, t, cx);
                                            }
                                        });
                                    }),
                            ).when_some(video.as_ref(), |buttons, video| {
                                let id = crate::api::http::urlencode(&video.id);
                                let url = format!("https://www.youtube.com/watch?v={id}");
                                buttons.child(
                                    div()
                                        .id("playback-open-youtube")
                                        .px_4()
                                        .py_2()
                                        .rounded_full()
                                        .bg(hsla(0., 0., 1., 0.15))
                                        .text_color(white())
                                        .text_sm()
                                        .cursor_pointer()
                                        .child(translate("Open on YouTube"))
                                        .on_click(move |_, _, cx| {
                                            cx.stop_propagation();
                                            let _ = open::that_detached(&url);
                                        }),
                                )
                            }),
                        )
                    }),
            );
        } else if loading {
            root = root.child(
                div().absolute().inset_0().flex().items_center().justify_center().child(
                    icon("loader-circle").size_12().text_color(white()).with_animation(
                        "player-spin",
                        Animation::new(Duration::from_millis(900)).repeat(),
                        |svg, d| svg.with_transformation(Transformation::rotate(percentage(d))),
                    ),
                ),
            );
        }

        if mini {
            self.clear_compressor_hover();
            return root.child(self.render_mini_controls(show, paused, time, duration, radius, cx)).into_any_element();
        }

        if let Some(video) = video.filter(|v| ui.shorts && !self.pip && v.is_short) {
            root = root.child(
                icon_button("sh-download", "download", cx)
                    .absolute()
                    .top_3()
                    .right_3()
                    .bg(p.chip)
                    .occlude()
                    .tooltip(|window, cx| component::tooltip::Tooltip::new(translate("Download")).build(window, cx))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |ev, window, cx| {
                        cx.stop_propagation();
                        open_menu(ev.position(), download_menu_items(&video, ev.position()), window, cx);
                    }),
            );
        }

        // SponsorBlock prompt / highlight (disabled on live streams)
        let pl = self.player.read(cx);
        let prompt = if is_live { None } else { pl.sb_prompt.clone() };
        let highlight = if is_live { None } else { pl.highlight().cloned().filter(|h| pl.time < h.start && pl.time < 30.0) };
        if let Some(seg) = prompt {
            let pp = self.player.clone();
            let end = seg.end;
            root = root.child(
                div()
                    .id("sb-skip")
                    .absolute()
                    .right_4()
                    .bottom(px(84.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(hsla(0., 0., 0., 0.75))
                    .border_1()
                    .border_color(hsla(0., 0., 1., 0.3))
                    .text_color(white())
                    .text_sm()
                    .cursor_pointer()
                    .child(div().size_2().rounded_full().bg(rgb(services::sponsor_category_color(&seg.category))))
                    .child(tf!("Skip {0}", translate(services::sponsor_category_name(&seg.category))))
                    .child(icon("skip-forward").size_4().text_color(white()))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        pp.update(cx, |p, _| p.seek_to(end))
                    }),
            );
        } else if let Some(h) = highlight {
            let pp = self.player.clone();
            let start = h.start;
            root = root.child(
                div()
                    .id("sb-hl")
                    .absolute()
                    .right_4()
                    .bottom(px(84.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(hsla(0., 0., 0., 0.75))
                    .text_color(white())
                    .text_sm()
                    .cursor_pointer()
                    .child(icon("sparkles").size_4().text_color(rgb(0xff1684)))
                    .child(translate("Jump to highlight"))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        pp.update(cx, |p, _| p.seek_to(start))
                    }),
            );
        }

        if self.player.read(cx).show_stats {
            root = root.child(self.render_stats(cx));
        }
        if !show {
            // the controls faded out under a still pointer: no hover-out comes for them
            self.vol_hover = false;
            self.clear_compressor_hover();
        }
        if show {
            if !self.mini && !self.pip && store::settings_snapshot().show_player_title {
                if let Some(v) = &self.player.read(cx).video {
                    root = root.child(
                        div()
                            .absolute()
                            .top_3()
                            .left_3()
                            .max_w(relative(0.65))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(hsla(0., 0., 0., 0.55))
                            .text_color(white())
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(v.title.clone()),
                    );
                }
            }
            root = root.child(self.render_controls(time, duration, ui, window, cx));
        }
        if let Some(m) = self.menu {
            root = root.child(self.render_menu(m, cx));
        }
        let _ = p;
        root.into_any_element()
    }
}

/// Items of the menu shown on right-clicking the video.
impl PlayerView {
    /// Archived annotations over the video picture (letterboxing left out).
    fn render_annotations(
        &mut self,
        time: f64,
        vsize: (f64, f64),
        mode: aspect::Mode,
        full_height: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use crate::api::annotations::{Action, Kind};
        if !store::settings_snapshot().annotations {
            return None;
        }
        let list = self.player.read(cx).annotations.clone();
        if list.is_empty() {
            return None;
        }
        let area = self.area.get();
        let (aw, ah) = (f32::from(area.size.width), f32::from(area.size.height));
        // the picture inside the player, placed as the surface draws it (cropped or zoomed for the aspect mode)
        let video = if vsize.0 > 0.0 && vsize.1 > 0.0 { (vsize.0 as f32, vsize.1 as f32) } else { (16.0, 9.0) };
        let (fsize, trim) = {
            let pl = self.player.read(cx);
            let dims = pl.frame.as_ref().map(|f| f.size(0));
            (dims.map(|d| (i32::from(d.width) as f32, i32::from(d.height) as f32)).unwrap_or(video), pl.auto_crop.trim)
        };
        let (region, fit) = aspect::resolve(mode, fsize, video, trim);
        let layout = aspect::layout((aw, ah), fsize, region, fit);
        let (ox, oy, pw, ph) = aspect::picture_region(fsize, video).in_frame(layout);
        let hovered = self.hover_annotation.clone();
        let rgba = |c: u32, a: f32| {
            let mut c = rgb(c);
            c.a = a.clamp(0.0, 1.0);
            c
        };
        let mut layer = div().absolute().inset_0().overflow_hidden();
        // measures the player so the next frame can place everything
        let cell = self.area.clone();
        layer = layer.child(canvas(move |b, _, _| cell.set(b), |_, _, _, _| {}).absolute().size_full());
        if pw <= 0.0 || ph <= 0.0 {
            return Some(layer.into_any_element());
        }
        let (clip_x, clip_y, clip_w, clip_h) = if full_height {
            region.in_frame(layout)
        } else {
            (0.0, 0.0, aw, ah)
        };
        let mut annotations = div()
            .absolute()
            .when(full_height, |d| d.left(px(clip_x)).top(px(clip_y)).w(px(clip_w)).h(px(clip_h)))
            .when(!full_height, |d| d.inset_0())
            .overflow_hidden();
        for (i, a) in list.iter().enumerate() {
            let shown = a.visible_at(time) || a.rollover_of.as_ref().is_some_and(|p| hovered.as_deref() == Some(p.as_str()));
            if !shown {
                continue;
            }
            let x = ox - clip_x + a.x / 100.0 * pw;
            let y = oy - clip_y + a.y / 100.0 * ph;
            let w = (a.w / 100.0 * pw).max(1.0);
            let h = (a.h / 100.0 * ph).max(1.0);
            let font = (a.text_size / 100.0 * ph).max(8.0);
            let is_hovered = hovered.as_deref() == Some(a.id.as_str());
            let mut el = div()
                .id(("annotation", i))
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(w))
                .h(px(h))
                .overflow_hidden()
                .px(px(pw * 0.008))
                .py(px(ph * 0.008))
                .text_size(px(font))
                .line_height(px(font * 1.2))
                .text_color(rgb(a.fg));
            el = match a.kind {
                Kind::Highlight => {
                    // an outline that lights up under the pointer
                    let c = if is_hovered { rgba(a.bg, 0.9) } else { rgba(0x85_7F85, a.bg_alpha.max(0.25)) };
                    el.border(px(2.5)).border_color(c)
                }
                Kind::Note if a.rollover_of.is_some() => el,
                Kind::Note => {
                    let alpha = if is_hovered && a.action.is_some() { 0.9 } else { a.bg_alpha };
                    el.bg(rgba(a.bg, alpha)).when(a.style == "speech" || a.style == "popup", |d| d.rounded(px(ph * 0.008)))
                }
            };
            if !a.text.is_empty() {
                el = el.child(a.text.clone());
            }
            // rollover text never takes the pointer away from its highlight
            if a.rollover_of.is_none() {
                let id = a.id.clone();
                el = el.on_hover(cx.listener(move |this, h: &bool, _, cx| {
                    if *h {
                        this.hover_annotation = Some(id.clone());
                    } else if this.hover_annotation.as_deref() == Some(id.as_str()) {
                        this.hover_annotation = None;
                    }
                    cx.notify();
                }));
            }
            if let Some(action) = a.action.clone().filter(|_| a.rollover_of.is_none()) {
                el = el
                    .cursor_pointer()
                    // keep the press from pausing or starting 2x playback underneath
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        match &action {
                            Action::Seek(t) => {
                                let t = *t;
                                this.player.update(cx, |p, _| p.seek_to(t));
                            }
                            Action::YouTube(url) => {
                                if let Some(route) = crate::ui::app::route_from_link(url) {
                                    crate::ui::app::navigate(route, window, cx);
                                } else {
                                    let _ = open::that_detached(url);
                                }
                            }
                            Action::Web(url) => {
                                this.player.update(cx, |p, cx| p.set_paused(true, cx));
                                let _ = open::that_detached(url);
                            }
                        }
                        this.poke(cx);
                    }));
            }
            annotations = annotations.child(el);
        }
        Some(layer.child(annotations).into_any_element())
    }
}

#[cfg(test)]
mod live_control_tests {
    use super::{live_hover_label, live_indicator_label, seekbar_fraction};

    #[::core::prelude::v1::test]
    fn live_hover_formats_negative_dvr_offset_and_snaps_near_edge() {
        assert_eq!(live_hover_label(3598.0, 3600.0, "LIVE"), ("LIVE".into(), true));
        assert_eq!(live_hover_label(3595.0, 3600.0, "В ЭФИРЕ"), ("В ЭФИРЕ".into(), true));
        assert_eq!(live_hover_label(3605.0, 3600.0, "LIVE"), ("LIVE".into(), true));
        assert_eq!(live_hover_label(3594.0, 3600.0, "LIVE"), ("-0:06".into(), false));
        assert_eq!(live_hover_label(3000.0, 3600.0, "LIVE"), ("-10:00".into(), false));
        assert_eq!(live_hover_label(0.0, 3661.0, "LIVE"), ("-1:01:01".into(), false));
    }

    #[::core::prelude::v1::test]
    fn live_indicator_shows_offset_only_when_behind_live_edge() {
        assert_eq!(live_indicator_label(true, 12.0, "LIVE"), "LIVE");
        assert_eq!(live_indicator_label(false, 0.4, "LIVE"), "LIVE");
        assert_eq!(live_indicator_label(false, 45.0, "LIVE"), "-0:45  LIVE");
        assert_eq!(live_indicator_label(false, 605.0, "В ЭФИРЕ"), "-10:05  В ЭФИРЕ");
    }

    #[::core::prelude::v1::test]
    fn live_seekbar_fraction_clamps_to_edge_when_at_live_and_tracks_dvr_when_behind() {
        assert_eq!(seekbar_fraction(985.0, 1000.0, true, true, None), 1.0);
        assert_eq!(seekbar_fraction(500.0, 1000.0, true, false, None), 0.5);
        assert_eq!(seekbar_fraction(985.0, 1000.0, true, true, Some(0.4)), 0.4);
        assert_eq!(seekbar_fraction(0.0, 0.0, true, true, None), 1.0);
        assert_eq!(seekbar_fraction(0.0, 0.0, false, false, None), 0.0);
        assert_eq!(seekbar_fraction(30.0, 120.0, false, false, None), 0.25);
    }
}

/// The video's right-click menu: youtube.com's items in youtube.com's order,
/// then the app's own actions below a divider.
fn context_items(player: &Entity<Player>, pip: bool, cx: &App) -> Vec<crate::ui::app::MenuItem> {
    use crate::ui::app::MenuItem;
    let pl = player.read(cx);
    let ui = *cx.global::<UiState>();
    let id = pl.video.as_ref().map(|v| v.id.clone()).unwrap_or_default();
    let title = pl.video.as_ref().map(|v| {
        if store::settings_snapshot().dearrow_titles {
            services::branding_cached(&v.id).flatten().and_then(|b| b.title).unwrap_or_else(|| v.title.clone())
        } else {
            v.title.clone()
        }
    }).unwrap_or_default();
    let t = pl.time.max(0.0) as u64;
    let looping = pl.loop_video;
    let stats = pl.show_stats;
    let live = pl.streams.as_ref().is_some_and(|s| s.is_live) || pl.video.as_ref().is_some_and(|v| v.is_live);
    let can_step = pl.video.is_some() && !pl.loading && !pl.audio_only;
    let fill = ui.windowed;
    let debug = {
        let mut lines = vec![format!("Stream Tube {}", crate::updater::current_version())];
        lines.extend(pl.stats().into_iter().map(|(k, v)| format!("{k}: {v}")));
        lines.join("\n")
    };
    let mut items = Vec::new();
    // youtube.com has no Loop for live streams
    if !live {
        let player = player.clone();
        items.push(
            MenuItem::new("repeat", translate("Loop"), move |_, cx| {
                player.update(cx, |p, cx| {
                    p.set_loop(!looping);
                    cx.notify()
                })
            })
            .checked(looping),
        );
    }
    // the control bar's Miniplayer button
    if !pip && !ui.shorts {
        items.push(MenuItem::new("picture-in-picture-2", translate("Miniplayer"), |window, cx| {
            if store::settings_snapshot().pip_always_on_top {
                crate::ui::app::exit_fullscreen(window, cx);
                crate::ui::pip::open(cx);
            } else {
                crate::ui::app::go_back_keep_playing(window, cx);
            }
        }));
    }
    if !id.is_empty() {
        let id1 = id.clone();
        items.push(MenuItem::new("link", translate("Copy video URL"), move |_, cx| {
            crate::ui::copy_to_clipboard(format!("https://youtu.be/{id1}"), cx);
        }));
        // a live stream has no fixed position to link to
        if !live {
            let id2 = id.clone();
            items.push(MenuItem::new("link", translate("Copy video URL at current time"), move |_, cx| {
                crate::ui::copy_to_clipboard(format!("https://youtu.be/{id2}?t={t}"), cx);
            }));
        }
        let id3 = id.clone();
        items.push(MenuItem::new("code", translate("Copy embed code"), move |_, cx| {
            crate::ui::copy_text_to_clipboard(
                format!(
                    "<iframe width=\"560\" height=\"315\" src=\"https://www.youtube.com/embed/{id3}\" title=\"YouTube video player\" frameborder=\"0\" allow=\"accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share\" referrerpolicy=\"strict-origin-when-cross-origin\" allowfullscreen></iframe>"
                ),
                cx,
            );
        }));
    }
    items.push(MenuItem::new("bug", translate("Copy debug info"), move |_, cx| {
        crate::ui::copy_text_to_clipboard(debug.clone(), cx);
    }));
    {
        // fetch the streams again and carry on from the same moment
        let player = player.clone();
        items.push(MenuItem::new("circle-question-mark", translate("Troubleshoot playback issue"), move |_, cx| {
            player.update(cx, |p, cx| {
                if let Some(v) = p.video.clone() {
                    crate::api::streams::invalidate(&v.id);
                    let time = (!live && p.time > 1.0).then_some(p.time);
                    p.play(v, time, cx);
                    crate::ui::toast(cx, translate("Reloading the video"));
                }
            })
        }));
    }
    {
        let player = player.clone();
        items.push(MenuItem::new("info", translate("Stats for nerds"), move |_, cx| {
            player.update(cx, |p, cx| {
                p.show_stats = !stats;
                cx.notify()
            })
        }).checked(stats));
    }
    // the app's own actions
    let first_extra = items.len();
    {
        let player = player.clone();
        items.push(MenuItem::new("camera", translate("Screenshot"), move |_, cx| player.update(cx, |p, cx| p.screenshot(cx))));
    }
    if !title.is_empty() {
        items.push(MenuItem::new("copy", translate("Copy title"), move |_, cx| {
            crate::ui::copy_text_to_clipboard(title.clone(), cx);
        }));
    }
    let music = crate::ui::music::on(cx);
    items.push(MenuItem::new("music", translate("Music mode"), move |_, cx| crate::ui::music::set(!music, cx)).checked(music));
    if !pip {
        items.push(MenuItem::new(if fill { "shrink" } else { "expand" }, translate(if fill { "Exit full window" } else { "Fill the window" }), |window, cx| {
            crate::ui::app::toggle_windowed_fullscreen(window, cx);
        }));
    }
    {
        let player = player.clone();
        items.push(MenuItem::new("external-link", translate("Open in external player"), move |_, cx| {
            let (id, time) = {
                let p = player.read(cx);
                (p.video.as_ref().map(|v| v.id.clone()), p.time)
            };
            if let Some(id) = id {
                player.update(cx, |p, cx| p.set_paused(true, cx));
                crate::downloads::open_external_at(&id, time, cx);
            }
        }));
    }
    if can_step && !live {
        for (forward, icon, label, key) in [
            (false, "chevron-left", translate("Previous frame"), ','),
            (true, "chevron-right", translate("Next frame"), '.'),
        ] {
            let player = player.clone();
            items.push(MenuItem::new(icon, format!("{label} ({key})"), move |_, cx| {
                player.update(cx, |p, cx| {
                    if !p.paused {
                        p.set_paused(true, cx);
                    }
                    p.frame_step(forward);
                    cx.notify();
                });
            }));
        }
    }
    if let Some(first) = items.get_mut(first_extra) {
        first.divider = true;
    }
    items
}

fn navigate_to_current(this: &mut PlayerView, window: &mut Window, cx: &mut Context<PlayerView>) {
    if let Some(v) = this.player.read(cx).video.clone() {
        let route = if v.is_short && !store::settings_snapshot().shorts_in_player {
            Route::Shorts(Some(v.id))
        } else {
            Route::Watch { id: v.id, playlist: None, start: None }
        };
        navigate(route, window, cx);
    }
}

impl PlayerView {
    fn render_mini_controls(&self, show: bool, paused: bool, time: f64, duration: f64, radius: Pixels, cx: &mut Context<Self>) -> impl IntoElement {
        let pl = self.player.read(cx);
        let is_live = player_is_live(pl);
        let at_live_edge = player_at_live_edge(pl);
        let frac = seekbar_fraction(time, duration, is_live, at_live_edge, None);
        let pp = self.player.clone();
        let pp2 = self.player.clone();
        let pp3 = self.player.clone();
        div()
            .absolute()
            .inset_0()
            .rounded_t(radius)
            .when(show, |d| {
                d.bg(hsla(0., 0., 0., 0.4))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_6()
                            .child(
                                div()
                                    .id("mini-prev")
                                    .cursor_pointer()
                                    .child(icon("skip-back").size_6().text_color(white()))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        pp3.update(cx, |p, cx| {
                                            if let Some(i) = p.queue_pos.filter(|i| *i > 0) {
                                                p.play_queue_index(i - 1, cx);
                                            } else {
                                                p.seek_to(0.0);
                                            }
                                        })
                                    }),
                            )
                            .child(
                                div()
                                    .id("mini-play")
                                    .cursor_pointer()
                                    .child(icon(if paused { "play" } else { "pause" }).size_10().text_color(white()))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        pp.update(cx, |p, cx| {
                                            p.toggle_pause(cx);
                                            cx.notify()
                                        })
                                    }),
                            )
                            .child(
                                div()
                                    .id("mini-next")
                                    .cursor_pointer()
                                    .child(icon("skip-forward").size_6().text_color(white()))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        pp2.update(cx, |p, cx| {
                                            if let Some(i) = p.queue_pos.filter(|i| i + 1 < p.queue.len()) {
                                                p.play_queue_index(i + 1, cx);
                                            }
                                        })
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_1()
                            .left_1()
                            .child(
                                div()
                                    .id("mini-expand")
                                    .p_1()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(hsla(0., 0., 1., 0.2)))
                                    .child(icon("maximize-2").size_5().text_color(white()))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        navigate_to_current(this, window, cx);
                                    })),
                            ),
                    )
                    .child(
                        div().absolute().top_1().right_1().child(
                            div()
                                .id("mini-close")
                                .p_1()
                                .rounded_full()
                                .cursor_pointer()
                                .hover(|s| s.bg(hsla(0., 0., 1., 0.2)))
                                .child(icon("x").size_5().text_color(white()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.player.update(cx, |p, cx| p.stop(cx));
                                })),
                        ),
                    )
            })
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .h(px(3.))
                    .bg(hsla(0., 0., 1., 0.2))
                    .child(div().h_full().w(relative(frac)).bg(pal(cx).accent)),
            )
    }

    fn render_controls(&mut self, time: f64, duration: f64, ui: UiState, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let pl = self.player.read(cx);
        let paused = pl.paused;
        let volume = pl.volume;
        let muted = pl.muted;
        let is_live = player_is_live(pl);
        let bar_duration = if is_live { player_live_window(pl) } else { duration };
        let behind = if is_live && self.drag_seek.is_some() {
            (bar_duration - time).max(0.0)
        } else {
            player_live_behind(pl)
        };
        let at_live_edge = if is_live && self.drag_seek.is_some() {
            !paused && behind <= LIVE_EDGE_TOLERANCE_SECS
        } else {
            player_at_live_edge(pl)
        };
        // Keep the behind-live `-MM:SS` indicator ticking while controls are visible and paused/behind.
        if is_live && (paused || !at_live_edge) && self.live_tick.is_none() {
            self.live_tick = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_millis(1000)).await;
                let keep = this.update(cx, |this, cx| {
                    let pl = this.player.read(cx);
                    let need = (pl.is_foreground() || this.pip)
                        && player_is_live(pl)
                        && this.controls_visible(pl.paused)
                        && (pl.paused || !player_at_live_edge(pl));
                    if need {
                        cx.notify();
                    } else {
                        this.live_tick = None;
                    }
                    need
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }));
        }
        let segments = if is_live { Vec::new() } else { pl.segments.clone() };
        let chapters = if is_live { Vec::new() } else { pl.chapters.clone() };
        let cache = pl.buffered_ranges();
        let caption_on = pl.caption.is_some();
        let has_captions = pl.streams.as_ref().map(|s| !s.captions.is_empty()).unwrap_or(false);
        let storyboard = if is_live { None } else { pl.storyboard.clone() };
        let st = store::settings_snapshot();
        let heatmap = if !is_live && st.show_heatmap { pl.heatmap.clone() } else { Vec::new() };
        let jump_to = if !is_live && st.jump_ahead && !self.mini { pl.jump_ahead_target() } else { None };
        let has_next = pl.queue_pos.map(|i| i + 1 < pl.queue.len()).unwrap_or(false)
            || crate::player::controller::player(cx) == self.player && crate::ui::app::has_autoplay_next(cx);
        let chapter_title = if is_live { None } else { pl.current_chapter().map(|c| c.title.clone()) };
        let sb_show = !is_live && st.sponsorblock;
        let sb_actions = st.sb_actions.clone();
        let frac = seekbar_fraction(time, bar_duration, is_live, at_live_edge, self.drag_seek);
        let hover = self.hover_frac.filter(|_| bar_duration > 0.0 && self.hovering);
        let accent = p.accent;
        let seek_b = self.seek_bounds.clone();
        let vol_b = self.vol_bounds.clone();
        let _ = window;
        // Narrow frames (the Shorts player) only get the essential buttons.
        let bar_w = f32::from(self.seek_bounds.get().size.width);
        let narrow = bar_w > 0.0 && bar_w < 560.0;
        // there the volume slider slides out from the volume button on hover (or while
        // dragged); in a very narrow frame it takes the time readout's place meanwhile
        let vol_open = !narrow || self.vol_hover || self.drag_vol;
        let hide_time = narrow && vol_open && bar_w < 400.0;
        let compact = bar_w > 0.0 && bar_w < 760.0;
        let label_jump = bar_w >= 900.0;
        if narrow {
            self.clear_compressor_hover();
        }
        self.sync_compressor_meter(cx);

        // seek bar: one segment per chapter (2px gaps), hovered chapter grows
        let active = hover.is_some() || self.drag_seek.is_some();
        let hover_t = hover.map(|h| h as f64 * bar_duration);
        let mut spans: Vec<(f32, f32)> = Vec::new();
        if bar_duration > 0.0 && !chapters.is_empty() && !st.hide_chapters {
            for (i, c) in chapters.iter().enumerate() {
                let a = (c.start / bar_duration).clamp(0.0, 1.0) as f32;
                let b = chapters.get(i + 1).map(|n| (n.start / bar_duration) as f32).unwrap_or(1.0).clamp(a, 1.0);
                if b > a {
                    spans.push((a, b));
                }
            }
            if let Some(first) = spans.first_mut() {
                first.0 = 0.0;
            }
        }
        if spans.is_empty() {
            spans.push((0.0, 1.0));
        }
        let multi = spans.len() > 1;
        let base_h = if active { 5. } else { 3. };
        let sb_spans: Vec<(f32, f32, u32)> = if sb_show && bar_duration > 0.0 {
            segments
                .iter()
                .filter(|s| sb_actions.get(&s.category).is_some_and(|a| a != "off"))
                .map(|s| {
                    let a = (s.start / bar_duration) as f32;
                    let w = (((s.end - s.start) / bar_duration) as f32).max(0.004);
                    (a, a + w, services::sponsor_category_color(&s.category))
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut bar = div()
            .id("seekbar")
            .relative()
            .w_full()
            .h(px(base_h))
            .cursor_pointer()
            .child(canvas(move |b, _, _| seek_b.set(b), |_, _, _, _| {}).absolute().size_full());
        for (i, (a, b)) in spans.iter().copied().enumerate() {
            let w = b - a;
            let local = |f: f32| ((f - a) / w).clamp(0.0, 1.0);
            let hovered_here = hover.map(|h| h >= a && h < b).unwrap_or(false);
            let h_px = if hovered_here && multi { 9. } else { base_h };
            let seg = div()
                .relative()
                .size_full()
                .overflow_hidden()
                .bg(hsla(0., 0., 1., 0.25))
                .children(cache.iter().filter_map(|&(start, end)| {
                    if bar_duration <= 0.0 { return None }
                    let left = local((start / bar_duration) as f32);
                    let right = local((end / bar_duration) as f32);
                    (right > left).then(|| div().absolute().top_0().left(relative(left)).h_full().w(relative(right - left)).bg(hsla(0., 0., 1., 0.4)))
                }))
                .when_some(hover, |d, hv| d.child(div().absolute().top_0().left_0().h_full().w(relative(local(hv))).bg(hsla(0., 0., 1., 0.5))))
                // SponsorBlock segments sit under the played part, so the red
                // progress still shows where playback is inside one (#285)
                .children(sb_spans.iter().filter_map(|&(start, end, color)| {
                    let left = local(start);
                    let right = local(end);
                    (right > left).then(|| div().absolute().top_0().left(relative(left)).h_full().w(relative(right - left)).bg(rgb(color)))
                }))
                .child(div().absolute().top_0().left_0().h_full().w(relative(local(frac))).bg(accent));
            bar = bar.child(
                div()
                    .absolute()
                    .left(relative(a))
                    .w(relative(w))
                    .bottom(px(-(h_px - base_h) / 2.))
                    .h(px(h_px))
                    .when(multi && i + 1 < spans.len(), |d| d.pr(px(2.)))
                    .child(seg),
            );
        }
        // "Most replayed" graph above the bar while hovering it, like YouTube
        if active && !is_live && bar_duration > 0.0 && !heatmap.is_empty() {
            let mut graph = div().absolute().left_0().right_0().bottom(px(base_h + 2.)).h(px(34.));
            for (start, len, k) in heatmap.iter().copied() {
                graph = graph.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left(relative((start / bar_duration) as f32))
                        .w(relative((len / bar_duration) as f32))
                        .h(relative(k.clamp(0.04, 1.0)))
                        .bg(hsla(0., 0., 1., 0.22)),
                );
            }
            bar = bar.child(graph);
        }
        bar = bar.child(
            div()
                .absolute()
                .top(px(base_h / 2. - 6.5))
                .left(relative(frac))
                .ml(px(-6.5))
                .size(px(13.))
                .rounded_full()
                .bg(accent)
                .when(!active, |d| d.invisible()),
        );
        let bar = bar.on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let f = Self::frac_in(this.seek_bounds.get(), ev.position.x);
                this.drag_seek = Some(f);
                this.hover_frac = Some(f);
                cx.notify();
            }),
        );

        // hover preview: storyboard frame, chapter title, time (or live DVR offset)
        let preview = hover_t.map(|t| {
            let chap = chapters.iter().rev().find(|c| c.start <= t).map(|c| c.title.clone()).filter(|_| multi);
            let (fw, fh) = storyboard.as_ref().map(|sb| {
                let k = 192.0 / sb.width.max(1) as f32;
                (px(sb.width as f32 * k), px(sb.height as f32 * k))
            }).unwrap_or((px(192.), px(108.)));
            let sb_el = storyboard.as_ref().filter(|_| st.show_storyboard).and_then(|sb| {
                let (url, col, row) = sb.frame_at(t, bar_duration)?;
                Some(
                    div()
                        .relative()
                        .w(fw)
                        .h(fh)
                        .overflow_hidden()
                        .rounded_lg()
                        .border_2()
                        .border_color(white())
                        .bg(black())
                        .child(
                            img(SharedString::from(url))
                                .absolute()
                                .left(-(fw * col as f32))
                                .top(-(fh * row as f32))
                                .w(fw * sb.cols as f32)
                                .h(fh * sb.rows as f32),
                        ),
                )
            });
            let (preview_label, preview_live_edge) = if is_live {
                live_hover_label(t, bar_duration, translate("LIVE"))
            } else {
                (format_duration(t as u64), false)
            };
            let b = self.seek_bounds.get();
            let half = fw / 2.;
            let x = (b.size.width * (t / bar_duration.max(0.001)) as f32).max(half).min((b.size.width - half).max(half));
            div()
                .absolute()
                .bottom(px(64.))
                .left(x + px(12.) - half)
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .w(fw)
                .children(sb_el)
                .when_some(chap, |d, c| d.child(div().text_sm().text_color(white()).font_weight(FontWeight::MEDIUM).truncate().max_w_full().child(c)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .text_sm()
                        .text_color(white())
                        .font_weight(FontWeight::MEDIUM)
                        .when(preview_live_edge, |d| {
                            d.child(div().size(px(6.)).flex_none().rounded_full().bg(rgb(0xff0000)))
                        })
                        .child(preview_label),
                )
        });

        let pp = self.player.clone();
        let ctl_btn = |id: &'static str, name: &str| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(if compact { px(32.) } else { px(40.) })
                .rounded_full()
                .cursor_pointer()
                .hover(|s| s.bg(hsla(0., 0., 1., 0.15)))
                .child(icon(name).size(if compact { px(20.) } else { px(24.) }).text_color(white()))
        };
        let vol_icon = if muted || volume <= 0.0 { "volume-x" } else if volume < 50.0 { "volume-1" } else { "volume-2" };
        let vol_frac = if muted { 0.0 } else { (volume / 100.0).min(1.0) as f32 };
        let time_text = if is_live {
            live_indicator_label(at_live_edge, behind, translate("LIVE"))
        } else {
            format!("{} / {}", format_duration(time as u64), format_duration(duration as u64))
        };

        let left = div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_1()
            .child(ctl_btn("c-play", if paused { "play" } else { "pause" }).on_click({
                let pp = pp.clone();
                move |_, _, cx| pp.update(cx, |p, cx| { p.toggle_pause(cx); cx.notify() })
            }))
            .when(has_next, |d| {
                let playback = self.player.clone();
                d.child(ctl_btn("c-next", "skip-forward").on_click(move |_, window, cx| {
                    if crate::player::controller::player(cx) == playback {
                        crate::ui::app::play_next(window, cx);
                    } else {
                        playback.update(cx, |p, cx| {
                            if let Some(index) = p.queue_pos.filter(|i| i + 1 < p.queue.len()) {
                                p.play_queue_index(index + 1, cx);
                            }
                        });
                    }
                }))
            })
            .child(
                div()
                    .id("vol-group")
                    .flex()
                    .flex_none()
                    .items_center()
                    .group("vol")
                    .on_hover(cx.listener(|this, h: &bool, _, cx| {
                        if this.vol_hover != *h {
                            this.vol_hover = *h;
                            cx.notify();
                        }
                    }))
                    .child(ctl_btn("c-mute", vol_icon).on_click({
                        let pp = pp.clone();
                        move |_, _, cx| pp.update(cx, |p, cx| { p.toggle_mute(); cx.notify() })
                    }))
                    .when(vol_open, |d| d.child(
                        div()
                            .id("vol-slider")
                            .relative()
                            .w(if compact { px(40.) } else { px(64.) })
                            .py_2()
                            .mx_2()
                            .cursor_pointer()
                            .child(canvas(move |b, _, _| vol_b.set(b), |_, _, _, _| {}).absolute().size_full())
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .h(px(4.))
                                    .rounded_full()
                                    .bg(hsla(0., 0., 1., 0.3))
                                    .child(div().h_full().rounded_full().w(relative(vol_frac)).bg(white()))
                                    .child(div().absolute().top(px(-4.)).left(relative(vol_frac)).ml(px(-6.)).size(px(12.)).rounded_full().bg(white())),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.drag_vol = true;
                                    let f = Self::frac_in(this.vol_bounds.get(), ev.position.x);
                                    this.player.update(cx, |p, _| p.set_volume_live(f as f64 * 100.0));
                                    cx.notify();
                                }),
                            ),
                    ))
                    .when(!narrow, |d| d.child(self.render_compressor_button(compact, cx))),
            )
            .when(!hide_time && is_live, |d| {
                let dot_color: Hsla = if at_live_edge { rgb(0xff0000).into() } else { hsla(0., 0., 0.65, 1.) };
                d.child(
                    div()
                        .id("c-live")
                        .ml_1()
                        .px_2()
                        .py_1()
                        .rounded_full()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .cursor_pointer()
                        .hover(|s| s.bg(hsla(0., 0., 1., 0.15)))
                        .when(narrow, |d| d.min_w_0())
                        .when(!narrow, |d| d.flex_none().whitespace_nowrap())
                        .when(!at_live_edge, |d| {
                            d.tooltip(|window, cx| component::tooltip::Tooltip::new(translate("Go live")).build(window, cx))
                        })
                        .child(div().size(px(7.)).flex_none().rounded_full().bg(dot_color))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(white())
                                .when(narrow, |d| d.min_w_0().truncate())
                                .child(time_text.clone()),
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.drag_seek = None;
                            this.player.update(cx, |p, cx| player_go_live(p, cx));
                            this.poke(cx);
                        })),
                )
            })
            .when(!hide_time && !is_live, |d| {
                d.child(
                    div()
                        .ml_1()
                        .when(narrow, |d| d.min_w_0().truncate())
                        .when(!narrow, |d| d.flex_none().whitespace_nowrap())
                        .text_sm()
                        .text_color(white())
                        .child(time_text),
                )
            })
            .when_some(chapter_title.filter(|_| !narrow && !store::settings_snapshot().hide_chapters), |d, c| {
                d.child(
                    div()
                        .id("c-chapter")
                        .flex()
                        .min_w_0()
                        .items_center()
                        .gap_1()
                        .ml_3()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(hsla(0., 0., 1., 0.15)))
                        .text_sm()
                        .text_color(white())
                        .child(div().flex_none().child("•"))
                        .child(div().min_w_0().max_w(px(260.)).truncate().child(c))
                        .child(icon("chevron-right").size_4().text_color(white()))
                        // opens the chapter list beside the player
                        .on_click(|_, _, cx| {
                            cx.stop_propagation();
                            crate::ui::pages::watch::toggle_chapters(cx);
                        }),
                )
            });

        let right = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .when_some(jump_to.filter(|_| bar_w >= 380.0), |d, t| {
                d.child(
                    div()
                        .id("c-jump")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .h_8()
                        .px_2()
                        .when(label_jump, |button| button.px_3())
                        .rounded_full()
                        .bg(hsla(0., 0., 1., 0.14))
                        .hover(|s| s.bg(hsla(0., 0., 1., 0.24)))
                        .cursor_pointer()
                        .text_sm()
                        .text_color(white())
                        .child(icon("fast-forward").size_4().text_color(white()))
                        .when(label_jump, |button| button.child(translate("Jump ahead")))
                        .tooltip(|window, cx| component::tooltip::Tooltip::new(translate("Jump ahead")).build(window, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.player.update(cx, |p, _| p.seek_to(t));
                            this.poke(cx);
                        })),
                )
            })
            // youtube.com's bar, right side: Autoplay, Subtitles/CC, Settings, Miniplayer,
            // Theater, Fullscreen
            .when(!self.pip && !narrow && !ui.shorts, |d| {
                let on = store::settings_snapshot().autoplay_next;
                d.child(
                    div()
                        .id("c-autoplay")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .h(if compact { px(32.) } else { px(40.) })
                        .px_2()
                        .cursor_pointer()
                        .tooltip(move |window, cx| component::tooltip::Tooltip::new(translate(if on { "Autoplay is on" } else { "Autoplay is off" })).build(window, cx))
                        .child(
                            div()
                                .relative()
                                .w(px(36.))
                                .h(px(14.))
                                .rounded_full()
                                .bg(hsla(0., 0., 1., if on { 0.55 } else { 0.3 }))
                                .child(
                                    div()
                                        .absolute()
                                        .top(px(-3.))
                                        .left(if on { px(16.) } else { px(0.) })
                                        .size(px(20.))
                                        .rounded_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .bg(if on { white() } else { hsla(0., 0., 0.85, 1.) })
                                        .child(icon(if on { "play" } else { "pause" }).size(px(11.)).text_color(black())),
                                ),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            store::update_settings(|s| s.autoplay_next ^= true);
                            cx.refresh_windows();
                            this.poke(cx);
                        })),
                )
            })
            .when(!has_captions, |d| {
                // like youtube.com, the button stays, dimmed, when a video has no captions
                d.child(
                    div().id("c-cc-none").flex().flex_none().items_center().justify_center().size(if compact { px(32.) } else { px(40.) })
                        .tooltip(|window, cx| component::tooltip::Tooltip::new(translate("Subtitles/closed captions unavailable")).build(window, cx))
                        .child(icon("captions-off").size(if compact { px(20.) } else { px(24.) }).text_color(hsla(0., 0., 1., 0.4))),
                )
            })
            .when(has_captions, |d| {
                d.child(
                    ctl_btn("c-cc", if caption_on { "captions" } else { "captions-off" }).on_click(cx.listener(|this, _, _, cx| {
                        let caption = this.player.read(cx).caption;
                        if caption.is_some() {
                            this.player.update(cx, |p, cx| p.set_caption(None, cx));
                        } else {
                            this.menu = Some(Menu::Captions);
                            cx.notify();
                        }
                    })),
                )
            })
            .child(ctl_btn("c-settings", "settings").on_click(cx.listener(|this, _, _, cx| {
                // a mouse-down here already closed an open menu (outside click); don't reopen it
                let just_closed = this.menu_closed_at.elapsed() < std::time::Duration::from_millis(400);
                this.menu = if this.menu.is_some() || just_closed { None } else { Some(Menu::Main) };
                cx.notify();
            })))
            .when(!self.pip && !narrow, |d| {
                d.child(ctl_btn("c-pip", "picture-in-picture").on_click(|_, window, cx| {
                    if store::settings_snapshot().pip_always_on_top {
                        crate::ui::app::exit_fullscreen(window, cx);
                        crate::ui::pip::open(cx);
                    } else {
                        crate::ui::app::go_back_keep_playing(window, cx);
                    }
                }))
            })
            .when(!ui.fullscreen && !self.pip && !narrow, |d| {
                d.child(ctl_btn("c-theater", "rectangle-horizontal").on_click(|_, _, cx| {
                    crate::ui::app::toggle_theater_mode(cx);
                }))
            })
            .when(!self.pip, |d| {
                d.child(ctl_btn("c-fs", if ui.fullscreen { "minimize" } else { "maximize" }).on_click(|_, window, cx| {
                    crate::ui::app::toggle_fullscreen(window, cx);
                }))
            });

        let _ = p;
        div()
            .id("player-control-bar")
            .on_hover(cx.listener(|this, h: &bool, _, cx| {
                if !*h {
                    // hide on the usual delay from here, not at once
                    this.poke(cx);
                }
                cx.notify();
            }))
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .flex_col()
            .px_3()
            .pt_10()
            .rounded_b_xl()
            .bg(linear_gradient(
                0.,
                linear_color_stop(
                    hsla(
                        0.,
                        0.,
                        0.,
                        match store::settings_snapshot().controls_shade.as_str() {
                            "light" => 0.45,
                            "strong" => 0.92,
                            _ => 0.75,
                        },
                    ),
                    0.,
                ),
                linear_color_stop(hsla(0., 0., 0., 0.), 1.),
            ))
            .children(preview)
            .child(
                div()
                    .id("seek-wrap")
                    .py_2()
                    // the click is the bar's release; it's kept from the pause layer,
                    // so the seek it ends is finished here
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.finish_drag(cx);
                    }))
                    .child(bar),
            )
            .child(
                div()
                    .id("ctl-row")
                    .flex()
                    .items_center()
                    .justify_between()
                    .h_12()
                    // keep clicks on the control row from reaching the click-to-pause layer,
                    // and presses from starting its press-and-hold 2x timer: the row eats
                    // the release, so the layer would think the button is still held
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(left)
                    .child(right),
            )
    }

    fn render_compressor_button(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let enabled = self.player.read(cx).compressor_enabled;
        let reducing = self.comp_reduction_db >= 0.5;
        div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id("c-comp")
                    .size(if compact { px(32.) } else { px(40.) })
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .when(enabled, |d| {
                        d.bg(p.accent.opacity(0.28))
                            .border_1()
                            .border_color(p.accent.opacity(if reducing { 0.9 } else { 0.4 }))
                    })
                    .hover(move |d| d.bg(if enabled { p.accent.opacity(0.4) } else { hsla(0., 0., 1., 0.15) }))
                    .child(icon("audio-lines").size(if compact { px(20.) } else { px(24.) })
                        .text_color(if enabled { p.accent } else { hsla(0., 0., 1., 0.7) }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.player.update(cx, |pl, cx| pl.toggle_compressor(cx));
                        this.poke(cx);
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.menu = Some(Menu::Compressor);
                        this.sync_compressor_meter(cx);
                        this.poke(cx);
                    }))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.comp_hover = true;
                            this.sync_compressor_meter(cx);
                        } else {
                            this.clear_compressor_hover();
                        }
                        cx.notify();
                    })),
            )
            .when(self.comp_hover && !self.mini && self.menu.is_none(), |d| d.child(self.render_compressor_card(cx)))
    }

    fn render_compressor_card(&self, cx: &App) -> impl IntoElement {
        let p = pal(cx);
        let pl = self.player.read(cx);
        let enabled = pl.compressor_enabled;
        let reduction = self.comp_reduction_db.round() as u32;
        let (status, description) = if !enabled {
            (translate("Off").to_string(), translate("Evens out sudden volume spikes").to_string())
        } else if self.comp_reduction_db >= 0.5 {
            (tf!("-{0} dB", reduction), tf!("Reducing loud audio by {0} dB", reduction))
        } else {
            (translate("0 dB").to_string(), translate("Active — audio level is normal").to_string())
        };
        let channel_aware = store::settings_snapshot().remember_compressor_per_channel && pl.channel_id.is_some();
        let click = match (enabled, channel_aware) {
            (true, true) => "Click to disable on this channel",
            (false, true) => "Click to enable on this channel",
            (true, false) => "Click to disable",
            (false, false) => "Click to enable",
        };
        let muted = hsla(0., 0., 1., 0.64);
        let detail = |label: &str, value: String| {
            div().flex().justify_between().gap_2()
                .child(div().flex_none().text_color(muted).child(translate(label).to_string()))
                .child(div().min_w_0().truncate().child(value))
        };
        div()
            .absolute()
            .left_0()
            .bottom(px(48.))
            .w(px(320.))
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(hsla(0., 0., 1., 0.15))
            .bg(hsla(0., 0., 0.09, 0.97))
            .shadow_lg()
            .text_xs()
            .text_color(white())
            .child(div().flex().justify_between().gap_2()
                .child(div().text_color(muted).child(translate("Audio compressor")))
                .child(div().font_weight(FontWeight::SEMIBOLD)
                    .text_color(if enabled { p.accent } else { muted }).child(status)))
            .child(div().text_sm().child(description))
            .child(detail("Strength", translate(pl.compressor_strength.label()).into()))
            .when_some(pl.video.as_ref().map(|v| &v.channel_name).filter(|name| !name.is_empty()), |d, name| {
                d.child(detail("Channel", name.clone()))
            })
            .child(div().border_t_1().border_color(hsla(0., 0., 1., 0.15)).pt_2().text_color(muted)
                .child(div().child(translate(click)))
                .child(div().child(translate("Right-click for strength"))))
    }

    /// Ends a seek-bar or volume drag: seeks to where the bar was let go.
    fn finish_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = self.drag_seek.take() {
            self.player.update(cx, |p, cx| {
                if player_is_live(p) {
                    let win = player_live_window(p);
                    if win > 0.0 {
                        let behind = ((1.0 - f as f64) * win).max(0.0);
                        if behind <= LIVE_HOVER_EDGE_SECS {
                            player_go_live(p, cx);
                        } else {
                            p.seek_live_behind(behind, cx);
                        }
                    }
                } else {
                    let d = p.duration;
                    p.seek_interactive(f as f64 * d);
                }
            });
        }
        if std::mem::take(&mut self.drag_vol) {
            self.player.read(cx).save_volume();
        }
        cx.notify();
    }

    /// The "Stats for nerds" panel; refreshes itself every second while open.
    fn render_stats(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.player.read(cx).stats();
        if self.stats_tick.is_none() {
            self.stats_tick = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_millis(1000)).await;
                let open = this.update(cx, |this, cx| {
                    let open = this.player.read(cx).show_stats;
                    if open {
                        cx.notify();
                    } else {
                        this.stats_tick = None;
                    }
                    open
                });
                if !matches!(open, Ok(true)) {
                    break;
                }
            }));
        }
        let player = self.player.clone();
        div()
            .id("stats")
            .absolute()
            .top_3()
            .left_3()
            .max_w(px(470.))
            .p_3()
            .pr_8()
            .rounded_lg()
            .bg(hsla(0., 0., 0., 0.72))
            .text_xs()
            .text_color(white())
            .occlude()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("stats-close")
                    .absolute()
                    .top_2()
                    .right_2()
                    .size_5()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|s| s.bg(hsla(0., 0., 1., 0.15)))
                    .child(icon("x").size_4().text_color(white()))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        player.update(cx, |p, cx| {
                            p.show_stats = false;
                            cx.notify()
                        })
                    }),
            )
            .children(rows.into_iter().enumerate().map(|(i, (k, v))| {
                div()
                    .flex()
                    .gap_3()
                    .child(div().w(px(130.)).flex_none().text_right().font_weight(FontWeight::SEMIBOLD).child(translate(k)))
                    .child(crate::ui::widgets::selectable_text(SharedString::from(format!("stats-v-{i}")), v).flex_1().text_color(hsla(0., 0., 1., 0.85)))
            }))
            .with_animation("stats-in", Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()), |d, t| d.opacity(t))
    }

    fn render_menu(&mut self, menu: Menu, cx: &mut Context<Self>) -> impl IntoElement {
        let pl = self.player.read(cx);
        let row = |id: SharedString, icon_name: Option<&str>, label: String, value: Option<String>, active: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_3()
                .px_4()
                .h_10()
                .w_full()
                .cursor_pointer()
                .text_sm()
                .text_color(white())
                .hover(|s| s.bg(hsla(0., 0., 1., 0.1)))
                .child(div().w_5().when_some(icon_name.map(str::to_string), |d, n| d.child(icon(&n).size_5().text_color(white()))).when(active && icon_name.is_none(), |d| d.child(icon("check").size_5().text_color(white()))))
                .child(div().flex_1().child(label))
                .when_some(value, |d, v| d.child(div().text_color(hsla(0., 0., 1., 0.7)).child(v)).child(icon("chevron-right").size_4().text_color(white())))
        };
        let header = |title: &str, back: Menu, cx: &mut Context<Self>| {
            div()
                .id("menu-back")
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .h_10()
                .border_b_1()
                .border_color(hsla(0., 0., 1., 0.2))
                .cursor_pointer()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(white())
                .child(icon("chevron-left").size_5().text_color(white()))
                .child(translate(title).to_string())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = Some(back);
                    cx.notify();
                }))
        };
        let mut panel = div()
            .id("player-menu")
            .absolute()
            .right_3()
            .bottom(px(64.))
            .w(px(300.))
            .max_h(px(380.))
            .overflow_y_scroll()
            .py_2()
            .rounded_xl()
            .bg(hsla(0., 0., 0.11, 0.92))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            // any press outside the menu closes it, anywhere in the window
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.menu.is_some() {
                    this.menu = None;
                    this.menu_closed_at = std::time::Instant::now();
                    cx.notify();
                }
            }));
        match menu {
            Menu::Main => {
                let q = if pl.audio_only {
                    translate("Audio only").to_string()
                } else {
                    let h = pl.current_height();
                    match (pl.quality, h) {
                        (_, 0) => translate("Auto").into(),
                        // like youtube.com: "Auto (1080p)" while Auto picks
                        (0, h) => format!("{} ({h}p)", translate("Auto")),
                        (_, h) => format!("{h}p"),
                    }
                };
                let caption = pl
                    .caption
                    .and_then(|i| pl.streams.as_ref()?.captions.get(i))
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| translate("Off").into());
                let audio_tracks = pl.audio_tracks();
                let audio_only = pl.audio_only;
                let speed = pl.speed;
                // youtube.com's order, top to bottom: Ambient mode, Annotations, Audio track,
                // Subtitles/CC, Playback speed, Quality. The app's own audio options go
                // first, where youtube.com has Stable Volume; Loop, Stats for nerds and the
                // rest are in the right-click menu, as on youtube.com.
                panel = panel
                    .child(
                        row("m-ao".into(), Some("headphones"), translate("Audio only (background)").into(), Some(translate(if audio_only { "On" } else { "Off" }).into()), false)
                            .on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::AudioOnly); cx.notify() })),
                    )
                    .child({
                        let cur = translate(crate::player::controller::eq_label(&store::settings_snapshot().eq_preset)).to_string();
                        row("m-eq".into(), Some("sliders-horizontal"), translate("Equalizer").into(), Some(cur), false)
                            .on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Equalizer); cx.notify() }))
                    })
                    .child({
                        let cur = if pl.compressor_enabled { pl.compressor_strength.label() } else { "Off" };
                        row("m-comp".into(), Some("audio-lines"), translate("Audio compressor").into(), Some(translate(cur).into()), false)
                            .on_click(cx.listener(|this, _, _, cx| { this.menu = Some(Menu::Compressor); cx.notify(); }))
                    })
                    .when(!audio_only, |d| {
                        let cur = translate(aspect::label(&store::settings_snapshot().aspect_mode)).to_string();
                        d.child(row("m-ar".into(), Some("ratio"), translate("Aspect ratio").into(), Some(cur), false).on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Aspect); cx.notify() })))
                    })
                    .child({
                        let ambient = store::settings_snapshot().ambient_mode;
                        row("m-amb".into(), Some("sun"), translate("Ambient mode").into(), Some(translate(if ambient { "On" } else { "Off" }).into()), false)
                            .on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Ambient); cx.notify() }))
                    })
                    .when(!pl.annotations.is_empty(), |d| {
                        let on = store::settings_snapshot().annotations;
                        d.child(
                            row("m-ann".into(), Some("message-square"), translate("Annotations").into(), Some(translate(if on { "On" } else { "Off" }).into()), false)
                                .on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Annotations); cx.notify() })),
                        )
                    })
                    .when(audio_tracks.len() > 1, |d| {
                        let cur = pl.audio_track.clone().unwrap_or_else(|| translate("Default").into());
                        d.child(row("m-a".into(), Some("languages"), translate("Audio track").into(), Some(cur), false).on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Audio); cx.notify() })))
                    })
                    .child(row("m-c".into(), Some("captions"), translate("Subtitles/CC").into(), Some(caption), false).on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Captions); cx.notify() })))
                    .child(row("m-s".into(), Some("gauge"), translate("Playback speed").into(), Some(speed_label(speed)), false).on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Speed); cx.notify() })))
                    .child(row("m-q".into(), Some("sliders-horizontal"), translate("Quality").into(), Some(q), false).on_click(cx.listener(|t, _, _, cx| { t.menu = Some(Menu::Quality); cx.notify() })));
            }
            Menu::Quality => {
                let cur = pl.quality;
                let quals = pl.available_qualities();
                let playing = pl.current_height();
                panel = panel.child(header("Quality", Menu::Main, cx));
                for q in quals {
                    panel = panel.child(
                        row(SharedString::from(format!("q-{q}")), None, format!("{q}p{}", if q >= 2160 { " 4K" } else if q >= 720 { " HD" } else { "" }), None, q == cur)
                            .on_click(cx.listener(move |t, _, _, cx| {
                                t.menu = None;
                                // set_quality keeps it as the default only when "Remember last quality" is on
                                t.player.update(cx, |p, cx| p.set_quality(q, cx));
                            })),
                    );
                }
                // last, like youtube.com: the best the video offers, showing what plays now
                let auto = if cur == 0 && playing > 0 { format!("{} ({playing}p)", translate("Auto")) } else { translate("Auto").to_string() };
                panel = panel.child(row("q-auto".into(), None, auto, None, cur == 0).on_click(cx.listener(|t, _, _, cx| {
                    t.menu = None;
                    t.player.update(cx, |p, cx| p.set_quality(0, cx));
                })));
            }
            Menu::Speed => {
                let cur = pl.speed;
                panel = panel.child(header("Playback speed", Menu::Main, cx));
                for s in [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0] {
                    panel = panel.child(
                        row(SharedString::from(format!("s-{s}")), None, speed_label(s), None, (cur - s).abs() < 0.001)
                            .on_click(cx.listener(move |t, _, _, cx| {
                                t.menu = None;
                                t.player.update(cx, |p, cx| { p.set_speed(s); cx.notify() });
                            })),
                    );
                }
                panel = panel.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .py_2()
                        .child(div().id("s-dec").px_3().py_1().rounded_md().bg(hsla(0., 0., 1., 0.15)).cursor_pointer().text_color(white()).child("− 0.05").on_click(cx.listener(|t, _, _, cx| t.player.update(cx, |p, cx| { let s = p.speed; p.set_speed(s - 0.05); cx.notify() }))))
                        .child(div().text_color(white()).text_sm().child(format!("{:.2}x", cur)))
                        .child(div().id("s-inc").px_3().py_1().rounded_md().bg(hsla(0., 0., 1., 0.15)).cursor_pointer().text_color(white()).child("+ 0.05").on_click(cx.listener(|t, _, _, cx| t.player.update(cx, |p, cx| { let s = p.speed; p.set_speed(s + 0.05); cx.notify() })))),
                );
            }
            Menu::Captions => {
                let cur = pl.caption;
                let tracks = pl.streams.as_ref().map(|s| s.captions.clone()).unwrap_or_default();
                panel = panel
                    .child(header("Subtitles/CC", Menu::Main, cx))
                    .child(row("cap-opts".into(), Some("settings"), translate("Options").into(), Some(String::new()), false).on_click(cx.listener(|t, _, _, cx| {
                        t.menu = Some(Menu::CaptionOptions);
                        cx.notify();
                    })))
                    .child(
                    row("cap-off".into(), None, translate("Off").into(), None, cur.is_none()).on_click(cx.listener(|t, _, _, cx| {
                        t.menu = None;
                        t.player.update(cx, |p, cx| p.set_caption(None, cx));
                    })),
                );
                for (i, c) in tracks.iter().enumerate() {
                    panel = panel.child(
                        row(SharedString::from(format!("cap-{i}")), None, format!("{}{}", c.name, if c.auto && !c.name.contains("auto") { translate(" (auto-generated)") } else { "" }), None, cur == Some(i))
                            .on_click(cx.listener(move |t, _, _, cx| {
                                t.menu = None;
                                t.player.update(cx, |p, cx| p.set_caption(Some(i), cx));
                            })),
                    );
                }
            }
            Menu::Audio => {
                let cur = pl.audio_track.clone();
                let tracks = pl.audio_tracks();
                panel = panel.child(header("Audio track", Menu::Main, cx));
                for (i, t) in tracks.into_iter().enumerate() {
                    let active = cur.as_deref() == Some(t.as_str());
                    let t2 = t.clone();
                    panel = panel.child(
                        row(SharedString::from(format!("at-{i}")), None, t, None, active).on_click(cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            let t3 = t2.clone();
                            this.player.update(cx, |p, cx| p.set_audio_track(Some(t3), cx));
                        })),
                    );
                }
            }
            Menu::Equalizer => {
                let cur = store::settings_snapshot().eq_preset.clone();
                panel = panel.child(header("Equalizer", Menu::Main, cx));
                for &(value, label, _) in crate::player::controller::EQ_PRESETS {
                    panel = panel.child(row(SharedString::from(format!("eq-{value}")), None, translate(label).into(), None, cur == value).on_click(cx.listener(move |t, _, _, cx| {
                        t.menu = None;
                        t.player.update(cx, |p, cx| {
                            p.set_eq_preset(value);
                            cx.notify();
                        });
                    })));
                }
            }
            Menu::AudioOnly | Menu::Ambient | Menu::Annotations => {
                let settings = store::settings_snapshot();
                let (title, cur) = match menu {
                    Menu::AudioOnly => ("Audio only (background)", pl.audio_only),
                    Menu::Ambient => ("Ambient mode", settings.ambient_mode),
                    _ => ("Annotations", settings.annotations),
                };
                panel = panel.child(header(title, Menu::Main, cx));
                for (on, label) in [(false, "Off"), (true, "On")] {
                    panel = panel.child(row(SharedString::from(format!("onoff-{on}")), None, translate(label).into(), None, cur == on).on_click(cx.listener(move |t, _, _, cx| {
                        t.menu = None;
                        if on != cur {
                            match menu {
                                Menu::AudioOnly => t.player.update(cx, |p, cx| p.set_audio_only(on, cx)),
                                Menu::Ambient => {
                                    store::update_settings(|s| s.ambient_mode = on);
                                    t.player.update(cx, |p, cx| {
                                        if !on {
                                            p.ambient = None;
                                        }
                                        cx.notify()
                                    });
                                    cx.refresh_windows();
                                }
                                _ => store::update_settings(|s| s.annotations = on),
                            }
                        }
                        cx.notify();
                    })));
                }
            }
            Menu::CaptionOptions => {
                use crate::player::controller::{CAPTION_COLORS, CAPTION_EDGES, CAPTION_FONTS, CAPTION_OPACITIES, CAPTION_SIZES};
                let s = store::settings_snapshot();
                fn name<T: PartialEq>(list: &[(T, &'static str)], v: &T) -> String {
                    translate(list.iter().find(|(k, _)| k == v).map(|(_, l)| *l).unwrap_or("Custom")).to_string()
                }
                let font = CAPTION_FONTS.iter().find(|(k, _)| *k == s.caption_font).map(|(_, l)| *l).unwrap_or("Sans-serif");
                let edge = CAPTION_EDGES.iter().find(|(k, _)| *k == s.caption_edge).map(|(_, l)| *l).unwrap_or("Outline and shadow");
                let mut rows = vec![
                    (CapField::Font, "Font family", translate(font).to_string()),
                    (CapField::Color, "Font color", name(CAPTION_COLORS, &s.caption_color)),
                    (CapField::Size, "Font size", name(CAPTION_SIZES, &s.caption_size)),
                    (CapField::Opacity, "Font opacity", name(CAPTION_OPACITIES, &s.caption_opacity)),
                    (CapField::Background, "Background", translate(if s.caption_background { "On" } else { "Off" }).to_string()),
                ];
                if s.caption_background {
                    rows.push((CapField::BgColor, "Background color", name(CAPTION_COLORS, &s.caption_bg_color)));
                    rows.push((CapField::BgOpacity, "Background opacity", name(CAPTION_OPACITIES, &s.caption_bg_opacity)));
                } else {
                    rows.push((CapField::Edge, "Character edge style", translate(edge).to_string()));
                }
                panel = panel.child(header("Options", Menu::Captions, cx));
                for (i, (field, label, value)) in rows.into_iter().enumerate() {
                    panel = panel.child(row(SharedString::from(format!("co-{i}")), None, translate(label).into(), Some(value), false).on_click(cx.listener(move |t, _, _, cx| {
                        t.menu = Some(Menu::CaptionPick(field));
                        cx.notify();
                    })));
                }
                panel = panel.child(row("co-reset".into(), None, translate("Reset").into(), None, false).on_click(cx.listener(|t, _, _, cx| {
                    let d = store::Settings::default();
                    store::update_settings(|s| {
                        s.caption_font = d.caption_font;
                        s.caption_color = d.caption_color;
                        s.caption_size = d.caption_size;
                        s.caption_opacity = d.caption_opacity;
                        s.caption_background = d.caption_background;
                        s.caption_bg_color = d.caption_bg_color;
                        s.caption_bg_opacity = d.caption_bg_opacity;
                        s.caption_edge = d.caption_edge;
                    });
                    t.player.read(cx).restyle_captions();
                    cx.notify();
                })));
            }
            Menu::CaptionPick(field) => {
                use crate::player::controller::{CAPTION_COLORS, CAPTION_EDGES, CAPTION_FONTS, CAPTION_OPACITIES, CAPTION_SIZES};
                type Set = Rc<dyn Fn(&mut store::Settings)>;
                fn num(list: &'static [(u32, &'static str)], cur: u32, set: fn(&mut store::Settings, u32)) -> Vec<(&'static str, bool, Set)> {
                    list.iter().map(|&(v, l)| (l, v == cur, Rc::new(move |s: &mut store::Settings| set(s, v)) as Set)).collect()
                }
                fn text(list: &'static [(&'static str, &'static str)], cur: &str, set: fn(&mut store::Settings, &'static str)) -> Vec<(&'static str, bool, Set)> {
                    list.iter().map(|&(v, l)| (l, v == cur, Rc::new(move |s: &mut store::Settings| set(s, v)) as Set)).collect()
                }
                let s = store::settings_snapshot();
                let (title, choices) = match field {
                    CapField::Font => ("Font family", text(CAPTION_FONTS, &s.caption_font, |s, v| s.caption_font = v.into())),
                    CapField::Color => ("Font color", num(CAPTION_COLORS, s.caption_color, |s, v| s.caption_color = v)),
                    CapField::Size => ("Font size", num(CAPTION_SIZES, s.caption_size, |s, v| s.caption_size = v)),
                    CapField::Opacity => ("Font opacity", num(CAPTION_OPACITIES, s.caption_opacity, |s, v| s.caption_opacity = v)),
                    CapField::Background => ("Background", num(&[(0, "Off"), (1, "On")], s.caption_background as u32, |s, v| s.caption_background = v == 1)),
                    CapField::BgColor => ("Background color", num(CAPTION_COLORS, s.caption_bg_color, |s, v| s.caption_bg_color = v)),
                    CapField::BgOpacity => ("Background opacity", num(CAPTION_OPACITIES, s.caption_bg_opacity, |s, v| s.caption_bg_opacity = v)),
                    CapField::Edge => ("Character edge style", text(CAPTION_EDGES, &s.caption_edge, |s, v| s.caption_edge = v.into())),
                };
                panel = panel.child(header(title, Menu::CaptionOptions, cx));
                for (i, (label, active, set)) in choices.into_iter().enumerate() {
                    panel = panel.child(row(SharedString::from(format!("cp-{i}")), None, translate(label).into(), None, active).on_click(cx.listener(move |t, _, _, cx| {
                        let set = set.clone();
                        store::update_settings(move |s| set(s));
                        t.player.read(cx).restyle_captions();
                        // back to the list, like youtube.com, to change something else
                        t.menu = Some(Menu::CaptionOptions);
                        cx.notify();
                    })));
                }
            }
            Menu::Compressor => {
                let enabled = pl.compressor_enabled;
                let selected = pl.compressor_strength;
                panel = panel
                    .child(header("Audio compressor", Menu::Main, cx))
                    .child(row("comp-enabled".into(), None, translate("Enabled").into(), None, false)
                        .child(div().text_color(hsla(0., 0., 1., 0.7)).child(translate(if enabled { "On" } else { "Off" })))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.player.update(cx, |pl, cx| pl.toggle_compressor(cx));
                        })))
                    .child(div().mx_3().my_1().h(px(1.)).bg(hsla(0., 0., 1., 0.15)));
                for preset in compressor::PRESETS {
                    panel = panel.child(row(SharedString::from(format!("comp-{}", preset.id)), None, translate(preset.label).into(), None, preset.strength == selected)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.player.update(cx, |pl, cx| pl.set_compressor_strength(preset.strength, cx));
                        })));
                }
            }
            Menu::Aspect => {
                let cur = store::settings_snapshot().aspect_mode.clone();
                panel = panel.child(header("Aspect ratio", Menu::Main, cx));
                for &(value, label) in aspect::MODES {
                    panel = panel.child(row(SharedString::from(format!("ar-{value}")), None, translate(label).into(), None, cur == value).on_click(cx.listener(move |t, _, _, cx| {
                        t.menu = None;
                        set_aspect_mode(value, cx);
                    })));
                }
            }
        }
        div()
            .id("player-menu-wrap")
            .absolute()
            .right_3()
            .bottom(px(64.))
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .child(panel.relative().right_0().bottom_0())
    }
}
