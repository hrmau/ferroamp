//! Window management and input. Windows only ever redraw when something they
//! show has changed, and nothing wakes at all while idle.

pub mod font;
pub mod gfx;
pub mod panels;
pub mod skin;
pub mod sysfont;
mod present;
pub mod visuals;

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake};
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowId, WindowLevel};

use crate::engine::eq::{BANDS, PRESETS};
use crate::engine::gain::ReplayGain;
use crate::engine::spectrum::Analyzer;
use crate::engine::{Engine, PlayState, Status};
use crate::macos::{self, MenuItem, NowPlaying};
use crate::playlist::{self, Playlist, Repeat};
use crate::settings::{self, MainVis, Settings};
use gfx::Canvas;
use panels::*;
use present::Presenter;
use skin::Skin;
use visuals::{AudioFrame, Visualizer, WAVE_LEN};

const FRAME_VIS: Duration = Duration::from_millis(33);
const FRAME_VISUALS: Duration = Duration::from_millis(16);
const FRAME_TICKER: Duration = Duration::from_millis(125);
const FRAME_HIDDEN: Duration = Duration::from_millis(500);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const ALERT_TIME: Duration = Duration::from_secs(5);
const TICKER_SPEED: f32 = 24.0;
const CUSTOM_PRESET: usize = usize::MAX;
const AUTOSAVE_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum UserEvent {
    /// The engine or metadata scanner has something new to show.
    Wake,
    OpenFiles,
    /// A file dialog closed. Holds the paths it picked, if any.
    DialogClosed(Vec<PathBuf>),
    /// The add-station box closed, with what was typed unless cancelled.
    StationEntered(Option<String>),
    SaveFailed(String),
    Remote(macos::Remote),
    /// A line of a right-click menu was picked.
    MenuChosen(isize),
    /// A file's details are ready to show.
    FileInfo { title: String, body: String },
    /// The sheet showing a file's details was dismissed.
    SheetClosed,
    /// A skin finished loading in the background.
    SkinLoaded(PathBuf, Result<Box<Skin>, String>),
}

/// Seconds of crossfade the menu offers. 0 is off.
const CROSSFADE_CHOICES: [u32; 6] = [0, 2, 4, 6, 8, 12];

#[derive(Clone, Copy, Debug)]
enum MenuCmd {
    Play,
    Pause,
    Stop,
    Prev,
    Next,
    AddFiles,
    AddFolder,
    AddStation,
    Jump,
    FileInfo,
    PlaySelected,
    QueueSelected,
    RemoveSelected,
    Shuffle,
    SetRepeat(Repeat),
    SetCrossfade(u32),
    SetReplayGain(ReplayGain),
    ShowEq,
    ShowPlaylist,
    ShowVisuals,
    Shade(Kind),
    AlwaysOnTop,
    LoadSkin,
    DefaultSkin,
    SelectAll,
    SelectNone,
    ClearList,
    SortTitle,
    SortPath,
    SavePlaylist,
    Quit,
}

/// The search over the playlist, while it is open.
struct Jump {
    query: String,
    matches: Vec<u64>,
    cursor: usize,
    top: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Main,
    Eq,
    Playlist,
    /// The optional, freely resizable visuals window.
    Visuals,
}

impl Kind {
    fn art_height(self) -> usize {
        match self {
            Kind::Main => MAIN_H,
            Kind::Eq => EQ_H,
            Kind::Playlist => PLAYLIST_H,
            Kind::Visuals => 0,
        }
    }
}

struct Win {
    kind: Kind,
    window: Rc<Window>,
    presenter: Presenter,
    canvas: Canvas,
    dirty: bool,
    occluded: bool,
    focused: bool,
    cursor: (f64, f64),
}

impl Win {
    fn art_scale(&self) -> f64 {
        self.window.inner_size().width.max(1) as f64 / WIDTH as f64
    }
    fn art_cursor(&self) -> (i32, i32) {
        let k = self.art_scale();
        ((self.cursor.0 / k) as i32, (self.cursor.1 / k) as i32)
    }
    fn outer(&self) -> (i32, i32) {
        self.window
            .outer_position()
            .map_or((0, 0), |p| (p.x, p.y))
    }
    fn rect(&self) -> (i32, i32, i32, i32) {
        let (x, y) = self.outer();
        let s = self.window.inner_size();
        (x, y, s.width as i32, s.height as i32)
    }
}

/// A window being dragged, and those travelling with it. Tracked with the
/// global pointer location: positions relative to a window that is itself
/// moving would feed back into the drag and make it shake.
struct Drag {
    kind: Kind,
    start_mouse: (f64, f64),
    start_outer: (i32, i32),
    /// Other windows and their offsets from the dragged one.
    followers: Vec<(Kind, i32, i32)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Press {
    Main(MainCtl),
    Eq(EqCtl),
    Playlist(PlCtl),
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Arc::new(ThreadWaker(std::thread::current())).into();
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

pub struct App {
    proxy: EventLoopProxy<UserEvent>,
    dialog_open: Arc<AtomicBool>,
    dialog_home: Option<(i32, i32)>,
    save_due: Option<Instant>,
    playlist: Arc<Mutex<Playlist>>,
    engine: Engine,
    scanner: Sender<Vec<(u64, PathBuf)>>,
    settings: Settings,
    restore_current: Option<usize>,
    windows: Vec<Win>,
    status: Status,
    analyzer: Analyzer,
    scope: [f32; SCOPE_WIDTH],
    last_main_frame: Instant,
    /// Exists only while the visuals window is open, so closed costs nothing.
    visual: Option<Visualizer>,
    wave_left: Vec<f32>,
    wave_right: Vec<f32>,
    open_visuals: bool,
    last_visual_click: Option<Instant>,
    /// Most recent track and whether it is heard, for the Now Playing centre.
    published: Option<(u64, PlayState)>,
    last_position: f64,
    now_playing: NowPlaying,
    press: Option<Press>,
    drag: Option<Drag>,
    seek_preview: Option<f32>,
    modifiers: ModifiersState,
    scroll: usize,
    row_anchor: Option<usize>,
    drag_row: Option<isize>,
    last_click: Option<(Instant, usize)>,
    dropped: VecDeque<PathBuf>,
    alert: Option<(String, Instant)>,
    next_frame: Instant,
    last_frame: Instant,
    ticker_started: Instant,
    last_ticker_key: String,
    last_shown_second: i64,
    last_revision: u64,
    play_on_add: bool,
    /// The last station info generation copied into the playlist.
    live_seen: u64,
    jump: Option<Jump>,
    last_title_click: Option<(Instant, Kind)>,
    /// What each entry of the open right-click menu does, by tag.
    menu_cmds: Vec<MenuCmd>,
    /// `None` is the built-in look.
    skin: Option<Box<Skin>>,
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>, args: Vec<PathBuf>) -> Self {
        let (settings, session) = settings::load();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9e37_79b9, |d| d.as_nanos() as u64);
        let mut pl = Playlist::new(seed);
        pl.repeat = settings.repeat;
        let session = pl.add_items(session);
        pl.set_shuffle(settings.shuffle);
        let playlist = Arc::new(Mutex::new(pl));

        let wake = {
            let proxy = proxy.clone();
            Arc::new(move || {
                let _ = proxy.send_event(UserEvent::Wake);
            }) as Arc<dyn Fn() + Send + Sync>
        };
        let engine = Engine::new(
            Box::new(playlist::SharedQueue(playlist.clone())),
            wake.clone(),
        );
        engine.set_volume(settings.volume);
        engine.set_balance(settings.balance);
        engine.set_crossfade(settings.crossfade);
        engine.set_replay_gain(settings.replay_gain);
        engine.set_tap_enabled(settings.main_vis != MainVis::Off || settings.show_visuals);
        let eq = engine.eq();
        eq.set_enabled(settings.eq_on);
        eq.set_preamp(settings.preamp);
        for (band, &gain) in settings.gains.iter().enumerate() {
            eq.set_gain(band, gain);
        }

        let scanner = playlist::spawn_scanner(playlist.clone(), wake);
        let _ = scanner.send(session);

        // On this thread so the first frame already has the right look. The
        // file is small and a missing one simply means the built-in look.
        let skin = settings.skin.as_deref().and_then(|p| Skin::load(p).ok()).map(Box::new);
        let open_visuals = settings.show_visuals;
        let now = Instant::now();
        Self {
            proxy,
            dialog_open: Arc::new(AtomicBool::new(false)),
            dialog_home: None,
            save_due: None,
            playlist,
            engine,
            scanner,
            restore_current: settings.current_index,
            settings,
            windows: Vec::new(),
            status: Status {
                state: PlayState::Stopped,
                track: None,
                position: 0.0,
                duration: None,
                source_rate: 0,
            },
            analyzer: Analyzer::default(),
            scope: [0.0; SCOPE_WIDTH],
            last_main_frame: now,
            visual: None,
            wave_left: vec![0.0; WAVE_LEN],
            wave_right: vec![0.0; WAVE_LEN],
            open_visuals,
            last_visual_click: None,
            published: None,
            last_position: 0.0,
            now_playing: NowPlaying::new(),
            press: None,
            drag: None,
            seek_preview: None,
            modifiers: ModifiersState::empty(),
            scroll: 0,
            row_anchor: None,
            drag_row: None,
            last_click: None,
            dropped: args.into(),
            alert: None,
            next_frame: now,
            last_frame: now,
            ticker_started: now,
            last_ticker_key: String::new(),
            last_shown_second: -1,
            last_revision: 0,
            play_on_add: true,
            live_seen: 0,
            jump: None,
            last_title_click: None,
            menu_cmds: Vec::new(),
            skin,
        }
    }

    fn rows(&self) -> usize {
        if self.skin.is_some() { skin::ROWS } else { ROWS }
    }

    fn shaded(&self, kind: Kind) -> bool {
        match kind {
            Kind::Main => self.settings.shade_main,
            Kind::Eq => self.settings.shade_eq,
            Kind::Playlist => self.settings.shade_playlist,
            Kind::Visuals => false,
        }
    }

    fn art_height(&self, kind: Kind) -> usize {
        if self.shaded(kind) { SHADE_H } else { kind.art_height() }
    }

    /// The small display draws nothing while the main window is collapsed,
    /// so it costs nothing either.
    fn main_vis(&self) -> MainVis {
        if self.settings.shade_main {
            MainVis::Off
        } else {
            self.settings.main_vis
        }
    }

    fn window_level(&self) -> WindowLevel {
        if self.settings.always_on_top {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        }
    }

    fn win(&self, kind: Kind) -> Option<&Win> {
        self.windows.iter().find(|w| w.kind == kind)
    }

    fn mark(&mut self, kind: Kind) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.kind == kind) {
            w.dirty = true;
            w.window.request_redraw();
        }
    }

    fn mark_all(&mut self) {
        for kind in [Kind::Main, Kind::Eq, Kind::Playlist] {
            self.mark(kind);
        }
    }

    fn shown(&self, kind: Kind) -> bool {
        match kind {
            Kind::Main => true,
            Kind::Eq => self.settings.show_eq,
            Kind::Playlist => self.settings.show_playlist,
            Kind::Visuals => self.settings.show_visuals,
        }
    }

    // ------------------------------------------------------------ window setup

    fn create_windows(&mut self, el: &ActiveEventLoop) {
        let zoom = self.settings.zoom as f64;
        let scale = el
            .primary_monitor()
            .or_else(|| el.available_monitors().next())
            .map_or(2.0, |m| m.scale_factor());
        let unit = zoom * scale;
        // Default layout is a tidy rectangle: main over EQ, playlist beside them.
        let origin = (120.0 * scale, 120.0 * scale);
        let defaults = [
            (Kind::Main, origin.0, origin.1),
            (Kind::Eq, origin.0, origin.1 + MAIN_H as f64 * unit),
            (Kind::Playlist, origin.0 + WIDTH as f64 * unit, origin.1),
        ];
        for (kind, dx, dy) in defaults {
            let saved = match kind {
                Kind::Main => self.settings.main_pos,
                Kind::Eq => self.settings.eq_pos,
                Kind::Playlist => self.settings.playlist_pos,
                Kind::Visuals => None,
            };
            let on_screen = |p: &(i32, i32)| {
                el.available_monitors().any(|m| {
                    let (mp, ms) = (m.position(), m.size());
                    p.0 >= mp.x - 50
                        && p.1 >= mp.y - 10
                        && p.0 < mp.x + ms.width as i32 - 50
                        && p.1 < mp.y + ms.height as i32 - 10
                })
            };
            let pos = saved
                .filter(on_screen)
                .unwrap_or((dx as i32, dy as i32));
            let attrs = Window::default_attributes()
                .with_title("ferroamp")
                .with_inner_size(LogicalSize::new(
                    WIDTH as f64 * zoom,
                    self.art_height(kind) as f64 * zoom,
                ))
                .with_position(PhysicalPosition::new(pos.0, pos.1))
                .with_decorations(false)
                .with_resizable(false)
                .with_visible(false)
                .with_window_level(self.window_level());
            let window = match el.create_window(attrs) {
                Ok(w) => Rc::new(w),
                Err(e) => {
                    eprintln!("ferroamp: cannot create window: {e}");
                    el.exit();
                    return;
                }
            };
            let Some(presenter) = Presenter::new(&window, false) else {
                eprintln!("ferroamp: cannot attach to the window's layer");
                el.exit();
                return;
            };
            self.windows.push(Win {
                kind,
                window,
                presenter,
                canvas: Canvas::new(WIDTH, self.art_height(kind)),
                dirty: true,
                occluded: false,
                focused: false,
                cursor: (0.0, 0.0),
            });
        }
        for w in &self.windows {
            if match w.kind {
                Kind::Main => true,
                Kind::Eq => self.settings.show_eq,
                Kind::Playlist => self.settings.show_playlist,
                Kind::Visuals => false,
            } {
                w.window.set_visible(true);
            }
        }
        if let Some(w) = self.win(Kind::Main) {
            w.window.focus_window();
        }
    }

    fn create_visuals_window(&mut self, el: &ActiveEventLoop) {
        let mut attrs = Window::default_attributes()
            .with_title("ferroamp visuals")
            .with_inner_size(LogicalSize::new(640.0, 360.0))
            .with_min_inner_size(LogicalSize::new(160.0, 90.0));
        if let Some((x, y)) = self.settings.visuals_pos {
            attrs = attrs.with_position(PhysicalPosition::new(x, y));
        }
        let Ok(window) = el.create_window(attrs) else {
            self.alert("Could not open the visuals window".to_string());
            self.settings.show_visuals = false;
            return;
        };
        let window = Rc::new(window);
        let Some(presenter) = Presenter::new(&window, true) else {
            self.settings.show_visuals = false;
            return;
        };
        let size = window.inner_size();
        let seed = self.last_frame.elapsed().subsec_nanos() ^ 0x5bd1_e995;
        self.visual = Some(Visualizer::new(
            size.width,
            size.height,
            self.settings.visual_preset,
            seed,
        ));
        self.windows.push(Win {
            kind: Kind::Visuals,
            window,
            presenter,
            canvas: Canvas::new(0, 0),
            dirty: true,
            occluded: false,
            focused: true,
            cursor: (0.0, 0.0),
        });
        self.update_tap();
        // Wake straight away, even if nothing else is scheduled.
        self.next_frame = Instant::now();
        self.mark(Kind::Main);
    }

    fn close_visuals_window(&mut self) {
        if let Some(w) = self.win(Kind::Visuals) {
            self.settings.visuals_pos = Some(w.outer());
        }
        if let Some(v) = self.visual.take() {
            self.settings.visual_preset = v.preset();
        }
        self.windows.retain(|w| w.kind != Kind::Visuals);
        self.update_tap();
        self.mark(Kind::Main);
    }

    fn toggle_visuals(&mut self) {
        self.settings.show_visuals = !self.settings.show_visuals;
        if self.settings.show_visuals {
            // Windows can only be created where the event loop is at hand.
            self.open_visuals = true;
        } else {
            self.close_visuals_window();
        }
    }

    fn toggle_fullscreen(&mut self) {
        let Some(w) = self.win(Kind::Visuals) else {
            return;
        };
        if w.window.fullscreen().is_some() {
            w.window.set_fullscreen(None);
            w.window.set_cursor_visible(true);
        } else {
            w.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            w.window.set_cursor_visible(false);
        }
    }

    fn visuals_live(&self) -> bool {
        self.win(Kind::Visuals).is_some_and(|w| !w.occluded)
    }

    fn main_visible(&self) -> bool {
        self.win(Kind::Main)
            .is_some_and(|w| !w.occluded && w.window.is_visible().unwrap_or(true))
    }

    /// The audio callback only copies samples out when something will show them.
    fn update_tap(&self) {
        let main = self.main_vis() != MainVis::Off && self.main_visible();
        self.engine.set_tap_enabled(main || self.visuals_live());
    }

    fn set_shown(&mut self, kind: Kind, on: bool) {
        match kind {
            Kind::Eq => self.settings.show_eq = on,
            Kind::Playlist => self.settings.show_playlist = on,
            Kind::Main | Kind::Visuals => return,
        }
        if let Some(w) = self.win(kind) {
            w.window.set_visible(on);
        }
        self.mark(kind);
        self.mark(Kind::Main);
    }

    fn set_zoom(&mut self, zoom: u32) {
        let zoom = zoom.clamp(1, 4);
        if zoom == self.settings.zoom {
            return;
        }
        let ratio = zoom as f64 / self.settings.zoom as f64;
        self.settings.zoom = zoom;
        let Some(origin) = self.win(Kind::Main).map(Win::outer) else {
            return;
        };
        // Docked windows stay docked: their offsets scale with their size.
        for w in self.windows.iter().filter(|w| w.kind != Kind::Visuals) {
            let (x, y) = w.outer();
            let nx = origin.0 + ((x - origin.0) as f64 * ratio) as i32;
            let ny = origin.1 + ((y - origin.1) as f64 * ratio) as i32;
            let _ = w.window.request_inner_size(LogicalSize::new(
                WIDTH as f64 * zoom as f64,
                self.art_height(w.kind) as f64 * zoom as f64,
            ));
            w.window.set_outer_position(PhysicalPosition::new(nx, ny));
        }
        self.mark_all();
    }

    /// Collapses a window to a strip or restores it. Windows docked beneath
    /// it move with its bottom edge, so the stack stays together.
    fn toggle_shade(&mut self, kind: Kind) {
        let before = self.art_height(kind);
        match kind {
            Kind::Main => self.settings.shade_main = !self.settings.shade_main,
            Kind::Eq => self.settings.shade_eq = !self.settings.shade_eq,
            Kind::Playlist => self.settings.shade_playlist = !self.settings.shade_playlist,
            Kind::Visuals => return,
        }
        if kind == Kind::Playlist && self.settings.shade_playlist {
            self.jump = None;
        }
        let after = self.art_height(kind);
        let below = self.docked_below(kind);
        let zoom = self.settings.zoom as f64;
        let Some(win) = self.windows.iter_mut().find(|w| w.kind == kind) else {
            return;
        };
        let delta = ((after as f64 - before as f64) * zoom * win.window.scale_factor()).round() as i32;
        let (x, y) = win.outer();
        let _ = win
            .window
            .request_inner_size(LogicalSize::new(WIDTH as f64 * zoom, after as f64 * zoom));
        win.window.set_outer_position(PhysicalPosition::new(x, y));
        win.canvas = Canvas::new(WIDTH, after);
        for other in below {
            if let Some(w) = self.win(other) {
                let (x, y) = w.outer();
                w.window.set_outer_position(PhysicalPosition::new(x, y + delta));
            }
        }
        self.analyzer = Analyzer::default();
        self.update_tap();
        self.mark(kind);
        self.next_frame = Instant::now();
        self.save_soon();
    }

    /// Windows hanging from the bottom edge of `kind`, directly or through
    /// each other.
    fn docked_below(&self, kind: Kind) -> Vec<Kind> {
        const SLOP: i32 = 3;
        let mut group = vec![kind];
        let mut changed = true;
        while changed {
            changed = false;
            for w in &self.windows {
                if group.contains(&w.kind) || w.kind == Kind::Visuals || !self.shown(w.kind) {
                    continue;
                }
                let (x, y, width, _) = w.rect();
                let hangs = group.iter().filter_map(|&k| self.win(k)).any(|above| {
                    let (ax, ay, aw, ah) = above.rect();
                    (y - (ay + ah)).abs() <= SLOP && x < ax + aw && ax < x + width
                });
                if hangs {
                    group.push(w.kind);
                    changed = true;
                }
            }
        }
        group.retain(|&k| k != kind);
        group
    }

    fn toggle_always_on_top(&mut self) {
        self.settings.always_on_top = !self.settings.always_on_top;
        let level = self.window_level();
        for w in self.windows.iter().filter(|w| w.kind != Kind::Visuals) {
            w.window.set_window_level(level);
        }
        self.save_soon();
    }

    // ----------------------------------------------------------------- actions

    fn lock_playlist(&self) -> std::sync::MutexGuard<'_, Playlist> {
        self.playlist.lock().expect("playlist lock")
    }

    fn play_id(&mut self, id: u64) {
        let clip = {
            let mut pl = self.lock_playlist();
            pl.set_current(Some(id));
            pl.dequeue(id);
            pl.get(id).map(|e| e.clip())
        };
        if let Some(clip) = clip {
            self.engine.play(id, clip);
            self.ensure_row_visible(id);
            self.mark(Kind::Playlist);
        }
    }

    fn on_play(&mut self) {
        match self.engine.status().state {
            PlayState::Paused => self.engine.resume(),
            state => {
                let target = {
                    let pl = self.lock_playlist();
                    let selected = pl.selected.first().copied();
                    if state == PlayState::Playing {
                        pl.current
                    } else {
                        selected.or(pl.current).or_else(|| pl.first())
                    }
                };
                if let Some(id) = target {
                    self.play_id(id);
                }
            }
        }
    }

    fn on_pause(&mut self) {
        match self.engine.status().state {
            PlayState::Playing => self.engine.pause(),
            PlayState::Paused => self.engine.resume(),
            PlayState::Stopped => {}
        }
    }

    fn on_play_pause(&mut self) {
        match self.engine.status().state {
            PlayState::Stopped => self.on_play(),
            _ => self.on_pause(),
        }
    }

    fn on_next(&mut self) {
        let next = {
            let pl = self.lock_playlist();
            let from = self.status.track.or(pl.current);
            from.and_then(|c| pl.skip_from(c)).or_else(|| {
                from.is_none().then(|| pl.first()).flatten()
            })
        };
        if let Some(id) = next {
            self.play_id(id);
        }
    }

    fn on_prev(&mut self) {
        // Early in a track, go back a track. Later, go back to its start.
        if self.status.position > 3.0 && self.status.state != PlayState::Stopped {
            self.seek_to(0.0);
            return;
        }
        let prev = {
            let pl = self.lock_playlist();
            let from = self.status.track.or(pl.current);
            from.and_then(|c| pl.prev_before(c))
        };
        if let Some(id) = prev {
            self.play_id(id);
        }
    }

    fn seek_to(&mut self, secs: f64) {
        let Some(track) = self.status.track else {
            return;
        };
        let clip = self.lock_playlist().get(track).map(|e| e.clip());
        if let Some(clip) = clip {
            self.engine.seek(track, clip, secs.max(0.0));
            self.published = None;
        }
    }

    fn seek_relative(&mut self, delta: f64) {
        let target = self.status.position + delta;
        let target = match self.status.duration {
            Some(d) => target.min(d - 0.5).max(0.0),
            None => target,
        };
        self.seek_to(target);
    }

    fn set_volume(&mut self, volume: f32) {
        self.settings.volume = volume.clamp(0.0, 1.0);
        self.engine.set_volume(self.settings.volume);
        self.mark(Kind::Main);
    }

    fn toggle_shuffle(&mut self) {
        let on = !self.settings.shuffle;
        self.settings.shuffle = on;
        self.lock_playlist().set_shuffle(on);
        self.mark(Kind::Main);
    }

    fn toggle_repeat(&mut self) {
        self.set_repeat(self.settings.repeat.next());
    }

    fn set_repeat(&mut self, mode: Repeat) {
        self.settings.repeat = mode;
        self.lock_playlist().repeat = mode;
        self.mark(Kind::Main);
    }

    /// Queues the selected tracks, or what is playing if nothing is selected.
    fn queue_selected(&mut self) {
        let mut pl = self.lock_playlist();
        let ids = if pl.selected.is_empty() {
            pl.current.into_iter().collect()
        } else {
            pl.selected.clone()
        };
        for id in ids {
            pl.toggle_queued(id);
        }
    }

    /// Details of the selected track, or of what is playing.
    fn show_file_info(&mut self) {
        let target = {
            let pl = self.lock_playlist();
            let id = pl.selected.first().copied().or(self.status.track).or(pl.current);
            id.and_then(|id| pl.get(id)).cloned()
        };
        let Some(entry) = target else {
            self.alert("Nothing to show: the playlist is empty".into());
            return;
        };
        let title = entry.display_title();
        let proxy = self.proxy.clone();
        if entry.is_stream() {
            let mut body = format!("Address: {}\n", entry.path.display());
            let info = &entry.info;
            for (name, value) in [
                ("Station", info.station.clone()),
                ("Now playing", info.title.clone()),
                ("Bitrate", info.kbps.map(|k| format!("{k} kbps"))),
                ("Sample rate", info.sample_rate.map(|r| format!("{r} Hz"))),
            ] {
                if let Some(value) = value {
                    body.push_str(&format!("{name}: {value}\n"));
                }
            }
            let _ = proxy.send_event(UserEvent::FileInfo { title, body });
            return;
        }
        // Reading a file can be slow on a network drive or a sleeping disk.
        std::thread::spawn(move || {
            let body = crate::meta::report(&entry.path, entry.span)
                .unwrap_or_else(|e| format!("{}\n\n{e}", entry.path.display()));
            let _ = proxy.send_event(UserEvent::FileInfo { title, body });
        });
    }

    fn present_file_info(&mut self, title: &str, body: &str) {
        if self.dialog_open.swap(true, Ordering::AcqRel) {
            return;
        }
        let proxy = self.proxy.clone();
        let home = self.win(Kind::Main).map(Win::outer);
        let shown = self.win(Kind::Main).is_some_and(|win| {
            macos::show_text(&win.window, title, body, move || {
                let _ = proxy.send_event(UserEvent::SheetClosed);
            })
        });
        if shown {
            self.dialog_home = home;
        } else {
            self.dialog_open.store(false, Ordering::Release);
        }
    }

    // ------------------------------------------------------------ jump to file

    fn open_jump(&mut self) {
        if self.jump.is_some() {
            return;
        }
        if !self.settings.show_playlist {
            self.set_shown(Kind::Playlist, true);
        }
        if self.settings.shade_playlist {
            self.toggle_shade(Kind::Playlist);
        }
        let (matches, cursor) = {
            let pl = self.lock_playlist();
            let matches = pl.search("");
            let at = pl.current.and_then(|c| matches.iter().position(|&m| m == c));
            (matches, at.unwrap_or(0))
        };
        self.jump = Some(Jump { query: String::new(), matches, cursor, top: 0 });
        self.jump_keep_visible();
        self.mark(Kind::Playlist);
    }

    fn close_jump(&mut self) {
        self.jump = None;
        self.mark(Kind::Playlist);
    }

    fn jump_search(&mut self) {
        let Some(query) = self.jump.as_ref().map(|j| j.query.clone()) else {
            return;
        };
        let matches = self.lock_playlist().search(&query);
        if let Some(jump) = &mut self.jump {
            jump.matches = matches;
            jump.cursor = 0;
            jump.top = 0;
        }
        self.mark(Kind::Playlist);
    }

    fn jump_move(&mut self, delta: isize) {
        if let Some(jump) = &mut self.jump {
            let last = jump.matches.len().saturating_sub(1) as isize;
            jump.cursor = (jump.cursor as isize + delta).clamp(0, last) as usize;
        }
        self.jump_keep_visible();
        self.mark(Kind::Playlist);
    }

    fn jump_keep_visible(&mut self) {
        let rows = self.rows();
        if let Some(jump) = &mut self.jump {
            if jump.cursor < jump.top {
                jump.top = jump.cursor;
            } else if jump.cursor >= jump.top + rows {
                jump.top = jump.cursor + 1 - rows;
            }
        }
    }

    fn jump_target(&self) -> Option<u64> {
        let jump = self.jump.as_ref()?;
        jump.matches.get(jump.cursor).copied()
    }

    fn jump_play(&mut self) {
        let Some(id) = self.jump_target() else {
            return;
        };
        self.close_jump();
        self.lock_playlist().selected = vec![id];
        self.play_id(id);
    }

    fn jump_key(&mut self, event: &KeyEvent) {
        let typed = |app: &mut Self, text: &str| {
            if let Some(jump) = &mut app.jump {
                jump.query.push_str(text);
            }
            app.jump_search();
        };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.close_jump(),
            Key::Named(NamedKey::Enter) => {
                if self.modifiers.shift_key() {
                    if let Some(id) = self.jump_target() {
                        self.lock_playlist().toggle_queued(id);
                        self.mark(Kind::Playlist);
                    }
                } else {
                    self.jump_play();
                }
            }
            Key::Named(NamedKey::ArrowUp) => self.jump_move(-1),
            Key::Named(NamedKey::ArrowDown) => self.jump_move(1),
            Key::Named(NamedKey::PageUp) => self.jump_move(-(self.rows() as isize)),
            Key::Named(NamedKey::PageDown) => self.jump_move(self.rows() as isize),
            Key::Named(NamedKey::Backspace) => {
                if let Some(jump) = &mut self.jump {
                    jump.query.pop();
                }
                self.jump_search();
            }
            Key::Named(NamedKey::Space) => typed(self, " "),
            Key::Character(text) => {
                if !self.modifiers.super_key() {
                    typed(self, text.as_str());
                } else if text.eq_ignore_ascii_case("v") {
                    let clip = macos::clipboard_text();
                    let line = clip.lines().next().unwrap_or_default().to_owned();
                    typed(self, &line);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------- right click

    fn open_menu(&mut self, kind: Kind, row: Option<usize>) {
        let mut b = MenuBuilder::default();
        if let Some(row) = row {
            let found = {
                let mut pl = self.lock_playlist();
                let id = pl.entries.get(row).map(|e| e.id);
                if let Some(id) = id
                    && !pl.selected.contains(&id)
                {
                    pl.selected = vec![id];
                    pl.revision += 1;
                }
                id.map(|id| pl.queue_position(id).is_some())
            };
            if let Some(queued) = found {
                b.action("Play", "", false, MenuCmd::PlaySelected);
                let label = if queued { "Remove from Queue" } else { "Play Next" };
                b.action(label, "q", false, MenuCmd::QueueSelected);
                b.action("File Info...", "i", false, MenuCmd::FileInfo);
                b.action("Remove from Playlist", "", false, MenuCmd::RemoveSelected);
                b.separator();
            }
        }
        b.action("Play", "x", false, MenuCmd::Play);
        b.action("Pause", "c", false, MenuCmd::Pause);
        b.action("Stop", "v", false, MenuCmd::Stop);
        b.action("Previous", "z", false, MenuCmd::Prev);
        b.action("Next", "b", false, MenuCmd::Next);
        b.separator();
        b.action("Add Files...", "l", false, MenuCmd::AddFiles);
        b.action("Add Folder...", "", false, MenuCmd::AddFolder);
        b.action("Add Radio Station...", "", false, MenuCmd::AddStation);
        b.separator();
        b.action("Jump to File...", "j", false, MenuCmd::Jump);
        if row.is_none() {
            b.action("File Info...", "i", false, MenuCmd::FileInfo);
        }
        b.separator();
        b.action("Shuffle", "s", self.settings.shuffle, MenuCmd::Shuffle);
        let repeat = self.settings.repeat;
        b.submenu("Repeat", |b| {
            b.action("Off", "", repeat == Repeat::Off, MenuCmd::SetRepeat(Repeat::Off));
            b.action("All", "", repeat == Repeat::All, MenuCmd::SetRepeat(Repeat::All));
            b.action("One Track", "", repeat == Repeat::One, MenuCmd::SetRepeat(Repeat::One));
        });
        let crossfade = self.settings.crossfade;
        b.submenu("Crossfade", |b| {
            for secs in CROSSFADE_CHOICES {
                let label = if secs == 0 { "Off".to_string() } else { format!("{secs} Seconds") };
                b.action(&label, "", crossfade == secs, MenuCmd::SetCrossfade(secs));
            }
        });
        let gain = self.settings.replay_gain;
        b.submenu("ReplayGain", |b| {
            for (label, mode) in [
                ("Off", ReplayGain::Off),
                ("Track", ReplayGain::Track),
                ("Album", ReplayGain::Album),
            ] {
                b.action(label, "", gain == mode, MenuCmd::SetReplayGain(mode));
            }
        });
        b.separator();
        b.action("Equalizer", "e", self.settings.show_eq, MenuCmd::ShowEq);
        b.action("Playlist", "p", self.settings.show_playlist, MenuCmd::ShowPlaylist);
        b.action("Visuals", "w", self.settings.show_visuals, MenuCmd::ShowVisuals);
        if kind != Kind::Visuals {
            b.action("Windowshade", "m", self.shaded(kind), MenuCmd::Shade(kind));
        }
        b.action("Always on Top", "a", self.settings.always_on_top, MenuCmd::AlwaysOnTop);
        let skin_name = self.skin.as_ref().map(|s| s.name.clone());
        b.submenu("Skin", |b| {
            b.action("Built In", "", skin_name.is_none(), MenuCmd::DefaultSkin);
            if let Some(name) = &skin_name {
                b.action(name, "", true, MenuCmd::LoadSkin);
            }
            b.separator();
            b.action("Load Skin...", "", false, MenuCmd::LoadSkin);
        });
        b.separator();
        b.action("Quit ferroamp", "", false, MenuCmd::Quit);
        self.menu_cmds = b.cmds;
        macos::show_menu(b.items);
    }

    /// A skin's playlist has five buttons where the built-in look has seven.
    /// As in the players those skins were made for, each opens a short menu.
    fn open_skin_button_menu(&mut self, ctl: PlCtl) {
        let mut b = MenuBuilder::default();
        match ctl {
            PlCtl::MenuAdd => {
                b.action("Add Files...", "l", false, MenuCmd::AddFiles);
                b.action("Add Folder...", "", false, MenuCmd::AddFolder);
                b.action("Add Radio Station...", "", false, MenuCmd::AddStation);
            }
            PlCtl::MenuRemove => {
                b.action("Remove Selected", "", false, MenuCmd::RemoveSelected);
                b.action("Clear Playlist", "", false, MenuCmd::ClearList);
            }
            PlCtl::MenuSelect => {
                b.action("Select All", "", false, MenuCmd::SelectAll);
                b.action("Select None", "", false, MenuCmd::SelectNone);
            }
            PlCtl::MenuMisc => {
                b.action("Sort by Title", "", false, MenuCmd::SortTitle);
                b.action("Sort by Path", "", false, MenuCmd::SortPath);
                b.action("File Info...", "i", false, MenuCmd::FileInfo);
                b.action("Jump to File...", "j", false, MenuCmd::Jump);
            }
            _ => b.action("Save Playlist...", "", false, MenuCmd::SavePlaylist),
        }
        self.menu_cmds = b.cmds;
        macos::show_menu(b.items);
    }

    fn run_menu(&mut self, el: &ActiveEventLoop, cmd: MenuCmd) {
        match cmd {
            MenuCmd::Play => self.on_play(),
            MenuCmd::Pause => self.on_pause(),
            MenuCmd::Stop => self.engine.stop(),
            MenuCmd::Prev => self.on_prev(),
            MenuCmd::Next => self.on_next(),
            MenuCmd::AddFiles => self.pick_files(),
            MenuCmd::AddFolder => self.pick_folder(),
            MenuCmd::AddStation => self.ask_for_station(),
            MenuCmd::Jump => self.open_jump(),
            MenuCmd::FileInfo => self.show_file_info(),
            MenuCmd::PlaySelected => {
                let id = self.lock_playlist().selected.first().copied();
                if let Some(id) = id {
                    self.play_id(id);
                }
            }
            MenuCmd::QueueSelected => self.queue_selected(),
            MenuCmd::RemoveSelected => self.remove_selected(),
            MenuCmd::Shuffle => self.toggle_shuffle(),
            MenuCmd::SetRepeat(mode) => self.set_repeat(mode),
            MenuCmd::SetCrossfade(secs) => {
                self.settings.crossfade = secs;
                self.engine.set_crossfade(secs);
            }
            MenuCmd::SetReplayGain(mode) => {
                self.settings.replay_gain = mode;
                self.engine.set_replay_gain(mode);
            }
            MenuCmd::ShowEq => self.set_shown(Kind::Eq, !self.settings.show_eq),
            MenuCmd::ShowPlaylist => self.set_shown(Kind::Playlist, !self.settings.show_playlist),
            MenuCmd::ShowVisuals => self.toggle_visuals(),
            MenuCmd::Shade(kind) => self.toggle_shade(kind),
            MenuCmd::AlwaysOnTop => self.toggle_always_on_top(),
            MenuCmd::LoadSkin => self.pick_skin(),
            MenuCmd::DefaultSkin => self.set_skin(None, None),
            MenuCmd::SelectAll => {
                let mut pl = self.lock_playlist();
                pl.selected = pl.entries.iter().map(|e| e.id).collect();
                pl.revision += 1;
            }
            MenuCmd::SelectNone => {
                let mut pl = self.lock_playlist();
                pl.selected.clear();
                pl.revision += 1;
            }
            MenuCmd::ClearList => {
                self.lock_playlist().clear();
                self.engine.stop();
                self.scroll = 0;
            }
            MenuCmd::SortTitle => self.lock_playlist().sort_by_title(),
            MenuCmd::SortPath => self.lock_playlist().sort_by_path(),
            MenuCmd::SavePlaylist => self.save_playlist(),
            MenuCmd::Quit => el.exit(),
        }
        self.save_soon();
    }

    /// Runs a file dialog away from the event handler. winit panics if events
    /// re-enter a running handler, and this also keeps the clock and spectrum
    /// moving while the dialog is open.
    fn run_dialog<F>(&mut self, dialog: F)
    where
        F: Future<Output = Result<Vec<PathBuf>, String>> + Send + 'static,
    {
        if self.dialog_open.swap(true, Ordering::AcqRel) {
            return;
        }
        // The dialog is a sheet on the main window, and macOS shoves that
        // window along to make a wide sheet fit on screen. Remember where it was.
        self.dialog_home = self.win(Kind::Main).map(Win::outer);
        let open = self.dialog_open.clone();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = block_on(dialog);
            open.store(false, Ordering::Release);
            let event = match result {
                Ok(paths) => UserEvent::DialogClosed(paths),
                Err(message) => UserEvent::SaveFailed(message),
            };
            let _ = proxy.send_event(event);
        });
    }

    fn pick_files(&mut self) {
        let extensions = [playlist::AUDIO_EXTENSIONS, playlist::PLAYLIST_EXTENSIONS].concat();
        self.run_dialog(async move {
            let files = rfd::AsyncFileDialog::new()
                .set_title("Add audio files")
                .add_filter("Audio and playlists", &extensions)
                .pick_files()
                .await
                .unwrap_or_default();
            Ok(files.iter().map(|f| f.path().to_path_buf()).collect())
        });
    }

    fn pick_folder(&mut self) {
        self.run_dialog(async {
            let folders = rfd::AsyncFileDialog::new()
                .set_title("Add a folder of music")
                .pick_folders()
                .await
                .unwrap_or_default();
            Ok(folders.iter().map(|f| f.path().to_path_buf()).collect())
        });
    }

    fn save_playlist(&mut self) {
        let text = self.lock_playlist().to_m3u();
        self.run_dialog(async move {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Save playlist")
                .set_file_name("playlist.m3u8")
                .add_filter("Playlist", &["m3u8", "m3u"])
                .save_file()
                .await;
            if let Some(file) = file {
                std::fs::write(file.path(), text)
                    .map_err(|e| format!("Could not save playlist: {e}"))?;
            }
            Ok(Vec::new())
        });
    }

    fn dialog_closed(&mut self) {
        if let (Some((x, y)), Some(main)) = (self.dialog_home.take(), self.win(Kind::Main)) {
            main.window.set_outer_position(PhysicalPosition::new(x, y));
        }
    }

    /// Adds paths to the playlist. If nothing is playing, starts the first one.
    fn add(&mut self, paths: Vec<PathBuf>) {
        let (skins, rest): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|p| is_skin_file(p));
        if let Some(path) = skins.into_iter().next_back() {
            self.load_skin(path);
        }
        self.add_items(playlist::expand_paths(rest));
    }

    fn pick_skin(&mut self) {
        self.run_dialog(async {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Choose a skin")
                .add_filter("Skins", &["wsz", "zip"])
                .pick_file()
                .await;
            Ok(file.iter().map(|f| f.path().to_path_buf()).collect())
        });
    }

    fn load_skin(&mut self, path: PathBuf) {
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = Skin::load(&path).map(Box::new);
            let _ = proxy.send_event(UserEvent::SkinLoaded(path, result));
        });
    }

    fn set_skin(&mut self, skin: Option<Box<Skin>>, path: Option<PathBuf>) {
        self.skin = skin;
        self.settings.skin = path;
        self.scroll = 0;
        self.jump = None;
        self.last_ticker_key.clear();
        self.mark_all();
        self.save_soon();
    }


    /// Returns the first new entry, if any were added.
    fn add_items(&mut self, files: Vec<playlist::Item>) -> Option<u64> {
        if files.is_empty() {
            return None;
        }
        let added = self.lock_playlist().add_items(files);
        let first = added.first().map(|(id, _)| *id);
        let _ = self.scanner.send(added);
        if self.play_on_add
            && self.engine.status().state == PlayState::Stopped
            && let Some(id) = first
        {
            self.play_id(id);
        }
        self.mark(Kind::Playlist);
        first
    }

    /// Cmd+V: adds station URLs, files, or a pasted playlist.
    fn paste(&mut self) {
        let items = playlist::expand_items(playlist::parse_pasted(&macos::clipboard_text()));
        if items.is_empty() {
            self.alert("Nothing to add: copy a station address or some files".into());
        } else {
            self.add_items(items);
        }
    }

    /// Cmd+L or the URL button: asks for a station address.
    fn ask_for_station(&mut self) {
        if self.dialog_open.swap(true, Ordering::AcqRel) {
            return;
        }
        let clip = macos::clipboard_text();
        let prefill = clip.lines().next().map(str::trim).filter(|l| playlist::is_stream(Path::new(l)));
        let proxy = self.proxy.clone();
        let home = self.win(Kind::Main).map(Win::outer);
        let shown = self.win(Kind::Main).is_some_and(|win| {
            macos::ask_for_station(&win.window, prefill.unwrap_or_default(), move |text| {
                let _ = proxy.send_event(UserEvent::StationEntered(text));
            })
        });
        if shown {
            // The sheet can shove the small main window aside, as file dialogs do.
            self.dialog_home = home;
        } else {
            self.dialog_open.store(false, Ordering::Release);
        }
    }

    fn alert(&mut self, message: String) {
        eprintln!("ferroamp: {message}");
        self.alert = Some((message, Instant::now()));
        self.mark(Kind::Main);
    }

    fn apply_preset(&mut self, index: usize) {
        let Some((_, preamp, gains)) = PRESETS.get(index) else {
            return;
        };
        self.settings.preset = index;
        self.settings.preamp = *preamp;
        self.settings.gains = *gains;
        let eq = self.engine.eq();
        eq.set_preamp(*preamp);
        for (band, gain) in gains.iter().enumerate() {
            eq.set_gain(band, *gain);
        }
        self.mark(Kind::Eq);
    }

    fn ensure_row_visible(&mut self, id: u64) {
        let (index, total) = {
            let pl = self.lock_playlist();
            (pl.index_of(id), pl.entries.len())
        };
        let Some(index) = index else {
            return;
        };
        if index < self.scroll {
            self.scroll = index;
        } else if index >= self.scroll + self.rows() {
            self.scroll = index + 1 - self.rows();
        }
        self.scroll = self.scroll.min(total.saturating_sub(self.rows()));
    }

    // --------------------------------------------------------------- per frame

    fn animating(&self) -> Option<Duration> {
        let playing = self.status.state == PlayState::Playing;
        if self.visuals_live()
            && (playing || self.visual.as_ref().is_some_and(|v| !v.is_settled()))
        {
            return Some(FRAME_VISUALS);
        }
        let main_visible = self.main_visible();
        match self.status.state {
            PlayState::Playing if !main_visible => Some(FRAME_HIDDEN),
            PlayState::Playing if self.main_vis() != MainVis::Off => Some(FRAME_VIS),
            PlayState::Playing => Some(FRAME_TICKER),
            // Let the bars fall instead of freezing them on pause or stop.
            _ if self.bars_active() && main_visible => Some(FRAME_VIS),
            _ if self.alert.is_some() => Some(FRAME_TICKER),
            _ => None,
        }
    }

    fn bars_active(&self) -> bool {
        self.main_vis() == MainVis::Spectrum && self.analyzer.peaks.iter().any(|&p| p > 0.0)
    }

    fn update_scope(&mut self) {
        // Starting each trace at a rising zero crossing holds the wave still,
        // instead of letting it crawl sideways.
        const STRIDE: usize = 3;
        let mut wave = [0f32; SCOPE_WIDTH * STRIDE + 512];
        self.engine.tap().read_mono(&mut wave);
        let start = (1..512)
            .find(|&i| wave[i - 1] <= 0.0 && wave[i] > 0.0)
            .unwrap_or(0);
        for (i, slot) in self.scope.iter_mut().enumerate() {
            *slot = wave[start + i * STRIDE];
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;

        if let Some(error) = self.engine.take_error() {
            self.alert(error);
        }
        if self
            .alert
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > ALERT_TIME)
        {
            self.alert = None;
            self.mark(Kind::Main);
        }

        let status = self.engine.status();
        let track_changed = status.track != self.status.track && status.track.is_some();
        let state_changed = status.state != self.status.state;
        if let Some(previous) = self.status.track
            && (status.track != Some(previous) || status.state == PlayState::Stopped)
        {
            // A station's song is only true while it is playing.
            let mut pl = self.lock_playlist();
            if let Some(e) = pl.get_mut(previous).filter(|e| e.is_stream()) {
                e.info.title = None;
            }
        }
        self.status = status;
        if let Some((track, info)) = self.engine.take_live(&mut self.live_seen) {
            let mut pl = self.lock_playlist();
            if let Some(e) = pl.get_mut(track).filter(|e| e.is_stream()) {
                // A name from the user's own playlist beats the station's.
                if e.info.station.is_none() {
                    e.info.station = info.station;
                }
                e.info.title = info.title.filter(|t| !t.is_empty());
                e.info.kbps = info.kbps.or(e.info.kbps);
                e.info.sample_rate = info.rate.or(e.info.sample_rate);
                e.info.channels = info.channels.or(e.info.channels);
            }
        }
        if track_changed {
            let id = status.track.expect("checked above");
            {
                let mut pl = self.lock_playlist();
                pl.set_current(Some(id));
                pl.dequeue(id);
            }
            self.ensure_row_visible(id);
            self.mark(Kind::Playlist);
        }
        if state_changed {
            self.mark(Kind::Main);
        }

        let playing = status.state == PlayState::Playing;
        let visible = self.main_visible();
        let live = self.visuals_live();
        let main_vis = self.main_vis();
        if playing && ((visible && main_vis == MainVis::Spectrum) || live) {
            self.analyzer
                .update(self.engine.tap(), self.engine.device_rate(), dt);
        } else if self.bars_active() {
            self.analyzer.decay(dt);
        }
        if state_changed && !playing {
            self.scope = [0.0; SCOPE_WIDTH];
        }
        let animate_main = visible
            && match main_vis {
                MainVis::Spectrum => playing || self.bars_active(),
                MainVis::Scope => playing,
                MainVis::Off => false,
            };
        // The main window's small display has no need of the visuals' frame rate.
        if animate_main && self.last_main_frame.elapsed() >= FRAME_VIS - Duration::from_millis(2) {
            self.last_main_frame = now;
            if main_vis == MainVis::Scope {
                self.update_scope();
            }
            self.mark(Kind::Main);
        }
        if track_changed && self.visual.is_some() {
            let title = self
                .lock_playlist()
                .get(status.track.expect("checked above"))
                .map(|e| e.display_title());
            if let (Some(v), Some(title)) = (self.visual.as_mut(), title) {
                v.announce(title);
            }
        }
        if live && let Some(v) = self.visual.as_mut() && (playing || !v.is_settled()) {
            if playing {
                self.engine
                    .tap()
                    .read_stereo(&mut self.wave_left, &mut self.wave_right);
            } else {
                self.wave_left.fill(0.0);
                self.wave_right.fill(0.0);
            }
            v.step(
                dt,
                &AudioFrame {
                    left: &self.wave_left,
                    right: &self.wave_right,
                    bars: &self.analyzer.bars,
                    bass: if playing { self.analyzer.bass } else { 0.0 },
                    mid: if playing { self.analyzer.mid } else { 0.0 },
                    treble: if playing { self.analyzer.treble } else { 0.0 },
                },
            );
            self.mark(Kind::Visuals);
        }

        // The clock and a scrolling title need redraws even without the bars.
        let second = status.position as i64;
        if second != self.last_shown_second {
            self.last_shown_second = second;
            self.mark(Kind::Main);
        }
        if status.state == PlayState::Playing && self.ticker_scrolls() {
            self.mark(Kind::Main);
        }

        let revision = self.lock_playlist().revision;
        if revision != self.last_revision {
            self.last_revision = revision;
            self.mark(Kind::Playlist);
            self.mark(Kind::Main);
            // Metadata may have just arrived for what is playing.
            self.published = None;
            self.save_soon();
        }

        self.publish_now_playing();
    }

    fn publish_now_playing(&mut self) {
        let key = self.status.track.map(|t| (t, self.status.state));
        // A jump in position means a seek happened, which macOS cannot infer.
        let jumped = (self.status.position - self.last_position).abs() > 2.0;
        self.last_position = self.status.position;
        if key == self.published && !jumped {
            return;
        }
        self.published = key;
        let Some(track) = self.status.track else {
            self.now_playing.clear();
            return;
        };
        let (title, artist, album, live) = {
            let pl = self.lock_playlist();
            let Some(e) = pl.get(track) else {
                return;
            };
            if e.is_stream() {
                // The song as the title and the station as the artist, which
                // is how Control Center and the lock screen show radio best.
                match e.info.title.clone() {
                    Some(song) => (song, e.info.station.clone(), None, true),
                    None => (e.display_title(), None, None, true),
                }
            } else {
                (
                    e.info.title.clone().unwrap_or_else(|| e.display_title()),
                    e.info.artist.clone(),
                    e.info.album.clone(),
                    false,
                )
            }
        };
        self.now_playing.update(&macos::NowPlayingTrack {
            title: &title,
            artist: artist.as_deref(),
            album: album.as_deref(),
            duration: self.status.duration,
            elapsed: self.status.position,
            playing: self.status.state == PlayState::Playing,
            live,
        });
    }

    fn ticker_text(&self) -> (String, bool) {
        if let Some((message, _)) = &self.alert {
            return (message.clone(), true);
        }
        let pl = self.lock_playlist();
        let id = self.status.track.or(pl.current);
        let text = id
            .and_then(|id| pl.get(id).map(|e| (id, e)))
            .map(|(id, e)| {
                let number = pl.index_of(id).map_or(0, |i| i + 1);
                let time = e
                    .info
                    .duration
                    .map(|d| {
                        let s = d.round() as u64;
                        format!(" ({}:{:02})", s / 60, s % 60)
                    })
                    .unwrap_or_default();
                format!("{number}. {}{time}", e.display_title())
            })
            .unwrap_or_else(|| "ferroamp - drop some music on me".to_string());
        (text, false)
    }

    fn ticker_width(&self, text: &str) -> i32 {
        self.skin.as_ref().map_or_else(|| font::width(text), |s| s.text_width(text))
    }

    fn ticker_room(&self) -> i32 {
        if self.skin.is_some() {
            skin::TICKER_ROOM
        } else if self.settings.shade_main {
            SHADE_TICKER_ROOM
        } else {
            130
        }
    }

    fn ticker_scrolls(&self) -> bool {
        let (text, _) = self.ticker_text();
        self.ticker_width(&text) > self.ticker_room()
    }

    fn track_facts(&self) -> TrackFacts {
        let pl = self.lock_playlist();
        let id = self.status.track.or(pl.current);
        let Some(e) = id.and_then(|id| pl.get(id)) else {
            return TrackFacts::default();
        };
        // A station's real rate is only known once it is decoding.
        let rate = if self.status.source_rate > 0 && !e.is_stream() {
            Some(self.status.source_rate)
        } else {
            e.info.sample_rate
        };
        TrackFacts {
            kbps: e.info.kbps,
            khz: rate.map(|r| (r as f32 / 1000.0).round() as u32),
            channels: e.info.channels,
        }
    }

    fn info_text(&self) -> String {
        let facts = self.track_facts();
        let kbps = facts.kbps.map_or(String::new(), |k| format!("{k} KBPS "));
        let khz = facts.khz.map_or(String::new(), |k| format!("{k} KHZ "));
        let channels = match facts.channels {
            Some(1) => "MONO",
            Some(2) => "STEREO",
            Some(_) => "MULTI",
            None => "",
        };
        format!("{kbps}{khz}{channels}")
    }

    fn redraw(&mut self, id: WindowId) {
        let Some(index) = self.windows.iter().position(|w| w.window.id() == id) else {
            return;
        };
        let kind = self.windows[index].kind;
        if kind == Kind::Visuals {
            let win = &mut self.windows[index];
            win.dirty = false;
            if let Some(v) = &self.visual {
                win.window.pre_present_notify();
                win.presenter.present(&v.canvas);
            }
            return;
        }
        let focused = self.windows[index].focused;
        let mut canvas = std::mem::replace(&mut self.windows[index].canvas, Canvas::new(0, 0));
        // A strip just collapsed may still get a frame with the old canvas.
        if canvas.h != self.art_height(kind) {
            canvas = Canvas::new(WIDTH, self.art_height(kind));
        }
        match kind {
            Kind::Main => self.draw_main_window(&mut canvas, focused),
            Kind::Eq => self.draw_eq_window(&mut canvas, focused),
            Kind::Playlist => self.draw_playlist_window(&mut canvas, focused),
            Kind::Visuals => {}
        }
        let win = &mut self.windows[index];
        win.dirty = false;
        win.window.pre_present_notify();
        win.presenter.present(&canvas);
        win.canvas = canvas;
    }

    fn draw_main_window(&mut self, canvas: &mut Canvas, focused: bool) {
        let (mut ticker, alert) = self.ticker_text();
        let scrolls = self.ticker_width(&ticker) > self.ticker_room();
        if ticker != self.last_ticker_key {
            self.last_ticker_key = ticker.clone();
            self.ticker_started = Instant::now();
        }
        let offset = if scrolls {
            ticker.push_str("  ***  ");
            (self.ticker_started.elapsed().as_secs_f32() * TICKER_SPEED) as i32
        } else {
            0
        };
        let info = self.info_text();
        let pressed = match self.press {
            Some(Press::Main(ctl)) => Some(ctl),
            _ => None,
        };
        let view = MainView {
            focused,
            pressed,
            state: self.status.state,
            position: self.status.position,
            duration: self.status.duration,
            seek_preview: self.seek_preview,
            show_remaining: self.settings.time_remaining,
            ticker: &ticker,
            ticker_offset: offset,
            ticker_alert: alert,
            info: &info,
            facts: self.track_facts(),
            always_on_top: self.settings.always_on_top,
            volume: self.settings.volume,
            balance: self.settings.balance,
            shuffle: self.settings.shuffle,
            repeat: self.settings.repeat,
            eq_shown: self.settings.show_eq,
            playlist_shown: self.settings.show_playlist,
            visuals_shown: self.settings.show_visuals,
            main_vis: self.main_vis(),
            bars: &self.analyzer.bars,
            peaks: &self.analyzer.peaks,
            scope: &self.scope,
        };
        match (&self.skin, self.settings.shade_main) {
            (Some(skin), true) => skin.draw_main_shade(canvas, &view),
            (Some(skin), false) => skin.draw_main(canvas, &view),
            (None, true) => draw_main_shade(canvas, &view),
            (None, false) => draw_main(canvas, &view),
        }
    }

    fn draw_eq_window(&self, canvas: &mut Canvas, focused: bool) {
        let pressed = match self.press {
            Some(Press::Eq(ctl)) => Some(ctl),
            _ => None,
        };
        let view = EqView {
            focused,
            pressed,
            enabled: self.settings.eq_on,
            preset_name: preset_name(self.settings.preset),
            preamp: self.settings.preamp,
            gains: self.settings.gains,
        };
        match (&self.skin, self.settings.shade_eq) {
            (Some(skin), true) => skin.draw_eq_shade(canvas, &view),
            (Some(skin), false) => skin.draw_eq(canvas, &view),
            (None, true) => draw_eq_shade(canvas, &view),
            (None, false) => draw_eq(canvas, &view),
        }
    }

    fn draw_playlist_window(&self, canvas: &mut Canvas, focused: bool) {
        let pressed = match self.press {
            Some(Press::Playlist(ctl)) => Some(ctl),
            _ => None,
        };
        let pl = self.lock_playlist();
        let view = PlaylistView {
            focused,
            pressed,
            playlist: &pl,
            scroll: self.jump.as_ref().map_or(self.scroll, |j| j.top),
            jump: self.jump.as_ref().map(|j| JumpView {
                query: &j.query,
                matches: &j.matches,
                cursor: j.cursor,
            }),
        };
        match (&self.skin, self.settings.shade_playlist) {
            (Some(skin), true) => skin.draw_playlist_shade(canvas, &view),
            (Some(skin), false) => skin.draw_playlist(canvas, &view),
            (None, true) => draw_playlist_shade(canvas, &view),
            (None, false) => draw_playlist(canvas, &view),
        }
    }

    // ------------------------------------------------------------------- input

    fn hit_main(&self, x: i32, y: i32) -> Option<MainCtl> {
        match (self.skin.is_some(), self.settings.shade_main) {
            (true, true) => skin::hit_main_shade(x, y),
            (true, false) => skin::hit_main(x, y),
            (false, true) => hit_main_shade(x, y),
            (false, false) => hit_main(x, y),
        }
    }

    fn hit_eq(&self, x: i32, y: i32) -> Option<EqCtl> {
        match (self.skin.is_some(), self.settings.shade_eq) {
            (true, true) => skin::hit_eq_shade(x, y),
            (true, false) => skin::hit_eq(x, y),
            (false, true) => hit_eq_shade(x, y),
            (false, false) => hit_eq(x, y),
        }
    }

    fn hit_playlist(&self, x: i32, y: i32) -> Option<PlCtl> {
        let scroll = self.jump.as_ref().map_or(self.scroll, |j| j.top);
        match (self.skin.is_some(), self.settings.shade_playlist) {
            (true, true) => skin::hit_playlist_shade(x, y),
            (true, false) => skin::hit_playlist(x, y, scroll),
            (false, true) => hit_playlist_shade(x, y),
            (false, false) => hit_playlist(x, y, scroll),
        }
    }

    /// A press on no control: double-clicking a title bar collapses the
    /// window, and anything else starts moving it.
    fn press_background(&mut self, kind: Kind, y: i32) {
        const TITLE_H: i32 = 14;
        let now = Instant::now();
        let double = y < TITLE_H
            && self
                .last_title_click
                .is_some_and(|(at, k)| k == kind && now - at < DOUBLE_CLICK);
        if double {
            self.last_title_click = None;
            self.toggle_shade(kind);
            return;
        }
        self.last_title_click = (y < TITLE_H).then_some((now, kind));
        self.begin_drag(kind);
    }

    fn on_right_click(&mut self, kind: Kind) {
        let Some(win) = self.win(kind) else {
            return;
        };
        let (x, y) = win.art_cursor();
        let row = match (kind, self.jump.is_some(), self.hit_playlist(x, y)) {
            (Kind::Playlist, false, Some(PlCtl::Row(row))) => Some(row),
            _ => None,
        };
        self.open_menu(kind, row);
    }

    fn on_mouse(&mut self, el: &ActiveEventLoop, kind: Kind, state: ElementState, button: MouseButton) {
        if button == MouseButton::Right && state == ElementState::Pressed {
            self.on_right_click(kind);
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        let Some(win) = self.win(kind) else {
            return;
        };
        let (x, y) = win.art_cursor();
        match state {
            ElementState::Pressed => match kind {
                Kind::Main => match self.hit_main(x, y) {
                    Some(ctl) => {
                        self.press = Some(Press::Main(ctl));
                        self.main_drag(ctl, x);
                        self.mark(Kind::Main);
                    }
                    None => self.press_background(kind, y),
                },
                Kind::Eq => match self.hit_eq(x, y) {
                    Some(ctl) => {
                        self.press = Some(Press::Eq(ctl));
                        self.eq_drag(ctl, y);
                        self.mark(Kind::Eq);
                    }
                    None => self.press_background(kind, y),
                },
                Kind::Playlist => match self.hit_playlist(x, y) {
                    Some(ctl) => {
                        self.press = Some(Press::Playlist(ctl));
                        self.playlist_press(ctl, y);
                        self.mark(Kind::Playlist);
                    }
                    None => self.press_background(kind, y),
                },
                Kind::Visuals => {
                    let now = Instant::now();
                    if self.last_visual_click.is_some_and(|at| now - at < DOUBLE_CLICK) {
                        self.last_visual_click = None;
                        self.toggle_fullscreen();
                    } else {
                        self.last_visual_click = Some(now);
                    }
                }
            },
            ElementState::Released => {
                self.save_soon();
                self.drag = None;
                self.drag_row = None;
                let press = self.press.take();
                match press {
                    Some(Press::Main(ctl)) => {
                        if self.hit_main(x, y) == Some(ctl) || ctl == MainCtl::Position {
                            self.main_release(el, ctl);
                        }
                        self.mark(Kind::Main);
                    }
                    Some(Press::Eq(ctl)) => {
                        if self.hit_eq(x, y) == Some(ctl) {
                            self.eq_release(ctl);
                        }
                        self.mark(Kind::Eq);
                    }
                    Some(Press::Playlist(ctl)) => {
                        if self.hit_playlist(x, y) == Some(ctl) {
                            self.playlist_release(ctl);
                        }
                        self.mark(Kind::Playlist);
                    }
                    None => {}
                }
            }
        }
    }

    fn on_cursor(&mut self, kind: Kind, position: (f64, f64)) {
        if let Some(win) = self.windows.iter_mut().find(|w| w.kind == kind) {
            win.cursor = position;
        }
        if self.drag.as_ref().is_some_and(|d| d.kind == kind) {
            self.continue_drag(kind);
            return;
        }
        let Some(win) = self.win(kind) else {
            return;
        };
        let (x, y) = win.art_cursor();
        match self.press {
            Some(Press::Main(ctl)) if kind == Kind::Main => self.main_drag(ctl, x),
            Some(Press::Eq(ctl)) if kind == Kind::Eq => self.eq_drag(ctl, y),
            Some(Press::Playlist(ctl)) if kind == Kind::Playlist => self.playlist_drag(ctl, y),
            _ => {}
        }
    }

    fn main_drag(&mut self, ctl: MainCtl, x: i32) {
        match ctl {
            MainCtl::Volume => {
                let volume = if self.skin.is_some() { skin::volume_from_x(x) } else { volume_from_x(x) };
                self.set_volume(volume);
            }
            MainCtl::Balance => {
                self.settings.balance = if self.skin.is_some() { skin::balance_from_x(x) } else { balance_from_x(x) };
                self.engine.set_balance(self.settings.balance);
                self.mark(Kind::Main);
            }
            MainCtl::Position if self.status.duration.is_some() => {
                self.seek_preview = Some(if self.skin.is_some() { skin::position_from_x(x) } else { position_from_x(x) });
                self.mark(Kind::Main);
            }
            _ => {}
        }
    }

    fn main_release(&mut self, el: &ActiveEventLoop, ctl: MainCtl) {
        match ctl {
            MainCtl::Close => el.exit(),
            MainCtl::Shade => self.toggle_shade(Kind::Main),
            MainCtl::Options => self.open_menu(Kind::Main, None),
            MainCtl::AlwaysOnTop => self.toggle_always_on_top(),
            MainCtl::FileInfo => self.show_file_info(),
            MainCtl::Prev => self.on_prev(),
            MainCtl::Play => self.on_play(),
            MainCtl::Pause => self.on_pause(),
            MainCtl::Stop => self.engine.stop(),
            MainCtl::Next => self.on_next(),
            MainCtl::Open => {
                if self.modifiers.shift_key() {
                    self.pick_folder();
                } else {
                    self.pick_files();
                }
            }
            MainCtl::Shuffle => self.toggle_shuffle(),
            MainCtl::Repeat => self.toggle_repeat(),
            MainCtl::ToggleEq => self.set_shown(Kind::Eq, !self.settings.show_eq),
            MainCtl::TogglePlaylist => self.set_shown(Kind::Playlist, !self.settings.show_playlist),
            MainCtl::ToggleVisuals => self.toggle_visuals(),
            MainCtl::Position => {
                if let (Some(p), Some(d)) = (self.seek_preview.take(), self.status.duration) {
                    self.seek_to(p as f64 * d);
                }
            }
            MainCtl::Time => {
                self.settings.time_remaining = !self.settings.time_remaining;
            }
            MainCtl::Spectrum => {
                self.settings.main_vis = self.settings.main_vis.next();
                self.analyzer = Analyzer::default();
                self.scope = [0.0; SCOPE_WIDTH];
                self.update_tap();
            }
            MainCtl::Volume | MainCtl::Balance => {}
        }
    }

    fn eq_drag(&mut self, ctl: EqCtl, y: i32) {
        let db = if self.skin.is_some() { skin::db_from_y(y) } else { db_from_y(y) };
        match ctl {
            EqCtl::Preamp => {
                self.settings.preamp = db;
                self.engine.eq().set_preamp(self.settings.preamp);
            }
            EqCtl::Band(band) if band < BANDS => {
                self.settings.gains[band] = db;
                self.engine.eq().set_gain(band, self.settings.gains[band]);
            }
            _ => return,
        }
        self.settings.preset = CUSTOM_PRESET;
        self.mark(Kind::Eq);
    }

    fn eq_release(&mut self, ctl: EqCtl) {
        match ctl {
            EqCtl::Close => self.set_shown(Kind::Eq, false),
            EqCtl::Shade => self.toggle_shade(Kind::Eq),
            EqCtl::Power => {
                self.settings.eq_on = !self.settings.eq_on;
                self.engine.eq().set_enabled(self.settings.eq_on);
            }
            EqCtl::Preset => {
                let next = if self.settings.preset == CUSTOM_PRESET {
                    0
                } else {
                    (self.settings.preset + 1) % PRESETS.len()
                };
                self.apply_preset(next);
                // Picking a preset clearly means wanting to hear it.
                self.settings.eq_on = true;
                self.engine.eq().set_enabled(true);
                // A skin has nowhere to show the preset's name.
                if self.skin.is_some() {
                    self.alert(format!("EQ: {}", preset_name(next)));
                }
            }
            _ => {}
        }
    }

    fn playlist_press(&mut self, ctl: PlCtl, y: i32) {
        match ctl {
            PlCtl::Row(row) => self.click_row(row),
            PlCtl::Scrollbar => self.playlist_drag(ctl, y),
            _ => {}
        }
    }

    /// In the search list a click picks a row and a double-click plays it.
    fn jump_click(&mut self, row: usize) {
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(at, r)| r == row && now - at < DOUBLE_CLICK);
        let Some(jump) = &mut self.jump else {
            return;
        };
        if row >= jump.matches.len() {
            return;
        }
        jump.cursor = row;
        if double {
            self.last_click = None;
            self.jump_play();
        } else {
            self.last_click = Some((now, row));
        }
    }

    fn click_row(&mut self, row: usize) {
        if self.jump.is_some() {
            self.jump_click(row);
            return;
        }
        let (cmd, shift) = (self.modifiers.super_key(), self.modifiers.shift_key());
        let mut play = None;
        {
            let mut pl = self.playlist.lock().expect("playlist lock");
            let Some(id) = pl.entries.get(row).map(|e| e.id) else {
                pl.selected.clear();
                pl.revision += 1;
                return;
            };
            if shift {
                let anchor = self.row_anchor.unwrap_or(row);
                let (a, b) = (anchor.min(row), anchor.max(row));
                pl.selected = pl.entries[a..=b].iter().map(|e| e.id).collect();
            } else if cmd {
                if let Some(at) = pl.selected.iter().position(|&s| s == id) {
                    pl.selected.remove(at);
                } else {
                    pl.selected.push(id);
                }
                self.row_anchor = Some(row);
            } else {
                if !pl.selected.contains(&id) {
                    pl.selected = vec![id];
                }
                self.row_anchor = Some(row);
                let now = Instant::now();
                if self
                    .last_click
                    .is_some_and(|(at, r)| r == row && now - at < DOUBLE_CLICK)
                {
                    play = Some(id);
                    self.last_click = None;
                } else {
                    self.last_click = Some((now, row));
                }
            }
            pl.revision += 1;
        }
        self.drag_row = Some(row as isize);
        if let Some(id) = play {
            self.play_id(id);
        }
    }

    fn playlist_drag(&mut self, ctl: PlCtl, y: i32) {
        match ctl {
            PlCtl::Scrollbar => {
                let skinned = self.skin.is_some();
                let from_y = |total| if skinned { skin::scroll_from_y(y, total) } else { scroll_from_y(y, total) };
                if let Some(jump) = &mut self.jump {
                    jump.top = from_y(jump.matches.len());
                } else {
                    let total = self.lock_playlist().entries.len();
                    self.scroll = from_y(total);
                }
                self.mark(Kind::Playlist);
            }
            PlCtl::Row(_) => {
                let Some(from) = self.drag_row else {
                    return;
                };
                let row = if self.skin.is_some() { skin::row_at_y(y, self.scroll) } else { row_at_y(y, self.scroll) };
                let moved = self.lock_playlist().move_selected(row - from);
                if moved != 0 {
                    self.drag_row = Some(from + moved);
                    self.last_click = None;
                    self.mark(Kind::Playlist);
                }
            }
            _ => {}
        }
    }

    fn playlist_release(&mut self, ctl: PlCtl) {
        // The search box covers the row of buttons.
        let covered = !matches!(ctl, PlCtl::Close | PlCtl::Shade | PlCtl::Row(_) | PlCtl::Scrollbar);
        if self.jump.is_some() && covered {
            return;
        }
        match ctl {
            PlCtl::Close => {
                self.jump = None;
                self.set_shown(Kind::Playlist, false);
            }
            PlCtl::Shade => self.toggle_shade(Kind::Playlist),
            PlCtl::Add => self.pick_files(),
            PlCtl::Folder => self.pick_folder(),
            PlCtl::Url => self.ask_for_station(),
            PlCtl::Remove => self.remove_selected(),
            PlCtl::Clear => {
                self.lock_playlist().clear();
                self.engine.stop();
                self.scroll = 0;
            }
            PlCtl::Sort => {
                let mut pl = self.lock_playlist();
                if self.modifiers.shift_key() {
                    pl.sort_by_path();
                } else {
                    pl.sort_by_title();
                }
            }
            PlCtl::Save => self.save_playlist(),
            PlCtl::MenuAdd | PlCtl::MenuRemove | PlCtl::MenuSelect | PlCtl::MenuMisc | PlCtl::MenuList => {
                self.open_skin_button_menu(ctl);
            }
            PlCtl::Row(_) | PlCtl::Scrollbar => {}
        }
    }

    fn remove_selected(&mut self) {
        let total = {
            let mut pl = self.lock_playlist();
            let ids = pl.selected.clone();
            pl.remove(&ids);
            pl.entries.len()
        };
        self.scroll = self.scroll.min(total.saturating_sub(self.rows()));
        self.mark(Kind::Playlist);
    }

    fn on_wheel(&mut self, kind: Kind, delta: MouseScrollDelta) {
        let rows = match delta {
            MouseScrollDelta::LineDelta(_, y) => y as f64,
            MouseScrollDelta::PixelDelta(p) => p.y / 16.0,
        };
        let step = if rows > 0.0 { rows.ceil() } else { rows.floor() } as isize;
        let visible = self.rows();
        match kind {
            Kind::Playlist => {
                if let Some(jump) = &mut self.jump {
                    let max = jump.matches.len().saturating_sub(visible) as isize;
                    jump.top = (jump.top as isize - step).clamp(0, max) as usize;
                } else {
                    let total = self.lock_playlist().entries.len();
                    let max = total.saturating_sub(visible) as isize;
                    self.scroll = (self.scroll as isize - step).clamp(0, max) as usize;
                }
                self.mark(Kind::Playlist);
            }
            // The wheel over the main window is a quick volume knob.
            Kind::Main => self.set_volume(self.settings.volume + step as f32 * 0.02),
            Kind::Eq | Kind::Visuals => {}
        }
    }

    fn on_key(&mut self, kind: Kind, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        self.save_soon();
        if self.jump.is_some() {
            self.jump_key(event);
            return;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Space) if !event.repeat => self.on_play_pause(),
            Key::Named(NamedKey::ArrowLeft) => self.seek_relative(-5.0),
            Key::Named(NamedKey::ArrowRight) => self.seek_relative(5.0),
            Key::Named(NamedKey::ArrowUp) => self.set_volume(self.settings.volume + 0.04),
            Key::Named(NamedKey::ArrowDown) => self.set_volume(self.settings.volume - 0.04),
            Key::Named(NamedKey::Backspace | NamedKey::Delete) => self.remove_selected(),
            Key::Named(NamedKey::Escape) => {
                if self
                    .win(Kind::Visuals)
                    .is_some_and(|w| w.window.fullscreen().is_some())
                {
                    self.toggle_fullscreen();
                }
            }
            Key::Named(NamedKey::Enter) => {
                let id = self.lock_playlist().selected.first().copied();
                if let Some(id) = id {
                    self.play_id(id);
                }
            }
            Key::Character(text) if !event.repeat => {
                if self.modifiers.super_key() {
                    match text.to_lowercase().as_str() {
                        "a" => {
                            let mut pl = self.lock_playlist();
                            pl.selected = pl.entries.iter().map(|e| e.id).collect();
                            pl.revision += 1;
                        }
                        "v" => self.paste(),
                        "l" => self.ask_for_station(),
                        _ => {}
                    }
                    return;
                }
                match text.to_lowercase().as_str() {
                    "z" => self.on_prev(),
                    "x" => self.on_play(),
                    "c" => self.on_pause(),
                    "v" => self.engine.stop(),
                    "b" => self.on_next(),
                    "l" => self.pick_files(),
                    "s" => self.toggle_shuffle(),
                    "r" => self.toggle_repeat(),
                    "j" => self.open_jump(),
                    "q" => self.queue_selected(),
                    "i" => self.show_file_info(),
                    "a" => self.toggle_always_on_top(),
                    "m" => self.toggle_shade(if kind == Kind::Visuals { Kind::Main } else { kind }),
                    "e" => self.set_shown(Kind::Eq, !self.settings.show_eq),
                    "p" => self.set_shown(Kind::Playlist, !self.settings.show_playlist),
                    "w" => self.toggle_visuals(),
                    "f" => self.toggle_fullscreen(),
                    "n" => {
                        if let Some(v) = self.visual.as_mut() {
                            v.next_preset();
                            self.settings.visual_preset = v.preset();
                            self.next_frame = Instant::now();
                        }
                    }
                    "=" | "+" => self.set_zoom(self.settings.zoom + 1),
                    "-" => self.set_zoom(self.settings.zoom.saturating_sub(1)),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------- window dragging

    fn begin_drag(&mut self, kind: Kind) {
        let Some(start_outer) = self.win(kind).map(Win::outer) else {
            return;
        };
        // Dragging the main window takes whatever is docked to it. Other
        // windows peel away on their own, then snap back when brought close.
        let followers = if kind == Kind::Main {
            self.docked_to_main()
        } else {
            Vec::new()
        };
        self.drag = Some(Drag {
            kind,
            start_mouse: macos::mouse_location(),
            start_outer,
            followers,
        });
    }

    fn docked_to_main(&self) -> Vec<(Kind, i32, i32)> {
        let Some(main) = self.win(Kind::Main) else {
            return Vec::new();
        };
        let origin = main.outer();
        let mut group = vec![Kind::Main];
        let mut changed = true;
        while changed {
            changed = false;
            for w in &self.windows {
                if group.contains(&w.kind) || !self.shown(w.kind) || w.kind == Kind::Visuals {
                    continue;
                }
                let touching = group
                    .iter()
                    .filter_map(|&k| self.win(k))
                    .any(|other| rects_touch(w.rect(), other.rect()));
                if touching {
                    group.push(w.kind);
                    changed = true;
                }
            }
        }
        group
            .into_iter()
            .filter(|&k| k != Kind::Main)
            .filter_map(|k| self.win(k).map(|w| (k, w.outer().0 - origin.0, w.outer().1 - origin.1)))
            .collect()
    }

    fn continue_drag(&mut self, kind: Kind) {
        let Some(drag) = &self.drag else {
            return;
        };
        let Some(win) = self.win(kind) else {
            return;
        };
        let scale = win.window.scale_factor();
        let mouse = macos::mouse_location();
        let mut target = (
            drag.start_outer.0 + ((mouse.0 - drag.start_mouse.0) * scale) as i32,
            // Desktop coordinates have y up, window positions have y down.
            drag.start_outer.1 - ((mouse.1 - drag.start_mouse.1) * scale) as i32,
        );

        // Snap to edges of windows that are not moving with this one.
        let threshold = (10.0 * win.window.scale_factor()) as i32;
        let size = win.window.inner_size();
        let (w, h) = (size.width as i32, size.height as i32);
        let moving: Vec<Kind> = std::iter::once(kind)
            .chain(drag.followers.iter().map(|f| f.0))
            .collect();
        let others: Vec<(i32, i32, i32, i32)> = self
            .windows
            .iter()
            .filter(|o| !moving.contains(&o.kind) && self.shown(o.kind) && o.kind != Kind::Visuals)
            .map(Win::rect)
            .collect();
        for (ox, oy, ow, oh) in others {
            let overlaps_y = target.1 < oy + oh + threshold && target.1 + h > oy - threshold;
            let overlaps_x = target.0 < ox + ow + threshold && target.0 + w > ox - threshold;
            if overlaps_y {
                for (edge, value) in [(target.0, ox + ow), (target.0 + w, ox), (target.0, ox), (target.0 + w, ox + ow)] {
                    if (edge - value).abs() < threshold {
                        target.0 += value - edge;
                        break;
                    }
                }
            }
            if overlaps_x {
                for (edge, value) in [(target.1, oy + oh), (target.1 + h, oy), (target.1, oy), (target.1 + h, oy + oh)] {
                    if (edge - value).abs() < threshold {
                        target.1 += value - edge;
                        break;
                    }
                }
            }
        }

        win.window
            .set_outer_position(PhysicalPosition::new(target.0, target.1));
        for (follower, dx, dy) in &drag.followers {
            if let Some(w) = self.win(*follower) {
                w.window
                    .set_outer_position(PhysicalPosition::new(target.0 + dx, target.1 + dy));
            }
        }
    }

    /// Schedules a save a moment from now, so that a crash or a force quit
    /// costs nothing, and a burst of changes writes the disk only once.
    fn save_soon(&mut self) {
        self.save_due = Some(Instant::now() + AUTOSAVE_DELAY);
    }

    fn save(&mut self) {
        self.save_due = None;
        let (m3u, current_index) = {
            let pl = self.lock_playlist();
            (pl.to_m3u(), pl.current.and_then(|c| pl.index_of(c)))
        };
        self.settings.current_index = current_index;
        for w in &self.windows {
            let pos = Some(w.outer());
            match w.kind {
                Kind::Main => self.settings.main_pos = pos,
                Kind::Eq => self.settings.eq_pos = pos,
                Kind::Playlist => self.settings.playlist_pos = pos,
                Kind::Visuals => self.settings.visuals_pos = pos,
            }
        }
        if let Some(v) = &self.visual {
            self.settings.visual_preset = v.preset();
        }
        settings::save(&self.settings, &m3u);
    }
}

fn is_skin_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wsz") || e.eq_ignore_ascii_case("zip"))
}

/// Collects a right-click menu along with what each entry does.
#[derive(Default)]
struct MenuBuilder {
    cmds: Vec<MenuCmd>,
    items: Vec<MenuItem>,
}

impl MenuBuilder {
    fn action(&mut self, title: &str, key: &'static str, checked: bool, cmd: MenuCmd) {
        self.items.push(MenuItem::Action {
            title: title.to_string(),
            key,
            tag: self.cmds.len() as isize,
            checked,
            enabled: true,
        });
        self.cmds.push(cmd);
    }

    fn separator(&mut self) {
        self.items.push(MenuItem::Separator);
    }

    fn submenu(&mut self, title: &str, fill: impl FnOnce(&mut Self)) {
        let outer = std::mem::take(&mut self.items);
        fill(self);
        let items = std::mem::replace(&mut self.items, outer);
        self.items.push(MenuItem::Submenu {
            title: title.to_string(),
            items,
        });
    }
}

fn rects_touch(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    const SLOP: i32 = 3;
    let x_overlap = a.0 < b.0 + b.2 && b.0 < a.0 + a.2;
    let y_overlap = a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
    let v_touch = (a.1 + a.3 - b.1).abs() <= SLOP || (b.1 + b.3 - a.1).abs() <= SLOP;
    let h_touch = (a.0 + a.2 - b.0).abs() <= SLOP || (b.0 + b.2 - a.0).abs() <= SLOP;
    (x_overlap && v_touch) || (y_overlap && h_touch)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.windows.is_empty() {
            self.create_windows(el);
            let mut pl = self.playlist.lock().expect("playlist lock");
            if let Some(entry) = self.restore_current.take().and_then(|i| pl.entries.get(i)) {
                let id = entry.id;
                pl.set_current(Some(id));
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => {}
            UserEvent::OpenFiles => self.dropped.extend(macos::take_opened_files()),
            UserEvent::DialogClosed(paths) => {
                self.dialog_closed();
                self.dropped.extend(paths);
            }
            UserEvent::StationEntered(text) => {
                self.dialog_open.store(false, Ordering::Release);
                self.dialog_closed();
                let Some(text) = text else {
                    return;
                };
                let stations: Vec<_> = playlist::parse_pasted(&text)
                    .into_iter()
                    .filter(|i| playlist::is_stream(&i.path))
                    .collect();
                if stations.is_empty() {
                    self.alert(format!("Not a web address: {}", text.trim()));
                } else if let Some(id) = self.add_items(stations) {
                    // Asked for by name, so play it now, as Open Location did.
                    self.play_id(id);
                }
            }
            UserEvent::MenuChosen(tag) => {
                if let Some(&cmd) = usize::try_from(tag).ok().and_then(|t| self.menu_cmds.get(t)) {
                    self.run_menu(el, cmd);
                }
            }
            UserEvent::FileInfo { title, body } => self.present_file_info(&title, &body),
            UserEvent::SkinLoaded(path, result) => match result {
                Ok(skin) => self.set_skin(Some(skin), Some(path)),
                Err(e) => self.alert(format!("Skin: {e}")),
            },
            UserEvent::SheetClosed => {
                self.dialog_open.store(false, Ordering::Release);
                self.dialog_closed();
            }
            UserEvent::SaveFailed(message) => {
                self.dialog_closed();
                self.alert(message);
            }
            UserEvent::Remote(cmd) => match cmd {
                macos::Remote::Play => self.on_play(),
                macos::Remote::Pause => {
                    if self.engine.status().state == PlayState::Playing {
                        self.engine.pause();
                    }
                }
                macos::Remote::Toggle => self.on_play_pause(),
                macos::Remote::Stop => self.engine.stop(),
                macos::Remote::Next => self.on_next(),
                macos::Remote::Prev => self.on_prev(),
                macos::Remote::Seek(secs) => self.seek_to(secs),
            },
        }
        // Handle it now rather than waiting for the next scheduled frame.
        self.next_frame = Instant::now();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(kind) = self
            .windows
            .iter()
            .find(|w| w.window.id() == id)
            .map(|w| w.kind)
        else {
            return;
        };
        match event {
            WindowEvent::RedrawRequested => self.redraw(id),
            WindowEvent::CloseRequested => match kind {
                Kind::Main => el.exit(),
                Kind::Visuals => self.toggle_visuals(),
                other => self.set_shown(other, false),
            },
            WindowEvent::Resized(size) if kind == Kind::Visuals => {
                if let Some(v) = self.visual.as_mut() {
                    v.resize(size.width, size.height);
                }
                self.save_soon();
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.mark(kind),
            WindowEvent::Focused(on) => {
                if let Some(w) = self.windows.iter_mut().find(|w| w.kind == kind) {
                    w.focused = on;
                }
                self.mark(kind);
            }
            WindowEvent::Occluded(on) => {
                if let Some(w) = self.windows.iter_mut().find(|w| w.kind == kind) {
                    w.occluded = on;
                }
                if !on {
                    self.mark(kind);
                }
                self.update_tap();
                // Something may need to start animating again.
                self.next_frame = Instant::now();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.on_cursor(kind, (position.x, position.y));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.on_mouse(el, kind, state, button);
            }
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(kind, delta),
            WindowEvent::KeyboardInput { event, .. } => self.on_key(kind, &event),
            WindowEvent::DroppedFile(path) => self.dropped.push_back(path),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if std::mem::take(&mut self.open_visuals) && self.visual.is_none() {
            self.create_visuals_window(el);
        }
        if !self.dropped.is_empty() {
            let paths: Vec<PathBuf> = self.dropped.drain(..).collect();
            self.add(paths);
        }
        let now = Instant::now();
        if now >= self.next_frame {
            self.tick();
            self.next_frame = now + self.animating().unwrap_or(FRAME_HIDDEN);
        }
        if self.save_due.is_some_and(|due| now >= due) {
            self.save();
        }
        let frame = self.animating().map(|_| self.next_frame);
        let wake = match (frame, self.save_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        el.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.save();
    }
}
