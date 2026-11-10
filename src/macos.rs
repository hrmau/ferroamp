//! The macOS integrations winit does not cover: the Now Playing centre, media
//! keys, files opened from Finder, the clipboard, right-click menus, and the
//! sheets for adding a radio station and showing a file's details.

use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::ffi;
use objc2::rc::Retained;
use dispatch2::DispatchQueue;
use objc2::runtime::{AnyClass, AnyObject, Imp, NSObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSControlStateValueOn, NSEvent,
    NSEventModifierFlags, NSMenu, NSMenuItem, NSModalResponse, NSPasteboard,
    NSPasteboardTypeFileURL, NSPasteboardTypeString, NSTextField, NSTextView, NSView, NSWindow,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyIsLiveStream,
    MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus,
};
use winit::event_loop::EventLoopProxy;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::ui::UserEvent;

#[derive(Clone, Copy, Debug)]
pub enum Remote {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Prev,
    Seek(f64),
}

static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

pub fn take_opened_files() -> Vec<PathBuf> {
    OPENED
        .lock()
        .map(|mut files| std::mem::take(&mut *files))
        .unwrap_or_default()
}

extern "C-unwind" fn open_urls(_this: &AnyObject, _cmd: Sel, _app: &AnyObject, urls: &NSArray<NSURL>) {
    if let Ok(mut files) = OPENED.lock() {
        files.extend(
            urls.iter()
                .filter_map(|url| url.path())
                .map(|p| PathBuf::from(p.to_string())),
        );
    }
    if let Some(proxy) = PROXY.get() {
        let _ = proxy.send_event(UserEvent::OpenFiles);
    }
}

/// Hooks up Finder's "Open With" and double-clicks.
///
/// winit installs its own application delegate and insists on staying the
/// delegate, so the handler is added to winit's delegate class rather than by
/// replacing it. Call after creating the event loop, before running it.
pub fn install_open_handler(proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(proxy);
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = app.delegate() else {
        return;
    };
    let delegate: &AnyObject = delegate.as_ref();
    let class: &AnyClass = delegate.class();
    // SAFETY: the function matches the selector's signature, as described by
    // the type encoding: void return, self, _cmd, and two object arguments.
    unsafe {
        let imp: Imp = std::mem::transmute(
            open_urls as extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &NSArray<NSURL>),
        );
        ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            sel!(application:openURLs:),
            imp,
            c"v@:@@".as_ptr(),
        );
    }
}

/// The pointer's location on the desktop in points, with y pointing up.
pub fn mouse_location() -> (f64, f64) {
    let p = NSEvent::mouseLocation();
    (p.x, p.y)
}

/// What to show in Control Center and on the lock screen.
#[derive(Clone, Copy, Debug)]
pub struct NowPlayingTrack<'a> {
    pub title: &'a str,
    pub artist: Option<&'a str>,
    pub album: Option<&'a str>,
    pub duration: Option<f64>,
    pub elapsed: f64,
    pub playing: bool,
    /// Internet radio: shown without a progress bar.
    pub live: bool,
}

pub struct NowPlaying {
    center: Retained<MPNowPlayingInfoCenter>,
}

impl NowPlaying {
    pub fn new() -> Self {
        Self {
            center: unsafe { MPNowPlayingInfoCenter::defaultCenter() },
        }
    }

    pub fn update(&self, track: &NowPlayingTrack) {
        let NowPlayingTrack {
            title,
            artist,
            album,
            duration,
            elapsed,
            playing,
            live,
        } = *track;
        let mut keys: Vec<&NSString> = Vec::with_capacity(6);
        let mut values: Vec<Retained<AnyObject>> = Vec::with_capacity(6);
        // SAFETY: these are framework-provided constant strings.
        let (k_title, k_artist, k_album, k_duration, k_elapsed, k_rate, k_live) = unsafe {
            (
                MPMediaItemPropertyTitle,
                MPMediaItemPropertyArtist,
                MPMediaItemPropertyAlbumTitle,
                MPMediaItemPropertyPlaybackDuration,
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                MPNowPlayingInfoPropertyPlaybackRate,
                MPNowPlayingInfoPropertyIsLiveStream,
            )
        };
        let string = |s: &str| Retained::into_super(Retained::into_super(NSString::from_str(s)));
        let number = |n: f64| {
            Retained::into_super(Retained::into_super(Retained::into_super(
                NSNumber::new_f64(n),
            )))
        };
        keys.push(k_title);
        values.push(string(title));
        if let Some(a) = artist {
            keys.push(k_artist);
            values.push(string(a));
        }
        if let Some(a) = album {
            keys.push(k_album);
            values.push(string(a));
        }
        if let Some(d) = duration {
            keys.push(k_duration);
            values.push(number(d));
        }
        keys.push(k_elapsed);
        values.push(number(elapsed));
        keys.push(k_rate);
        values.push(number(if playing { 1.0 } else { 0.0 }));
        if live {
            keys.push(k_live);
            values.push(number(1.0));
        }

        let value_refs: Vec<&AnyObject> = values.iter().map(|v| &**v).collect();
        let info = NSDictionary::from_slices(&keys, &value_refs);
        // SAFETY: the dictionary holds only strings and numbers, as documented.
        unsafe {
            self.center.setNowPlayingInfo(Some(&info));
            self.center.setPlaybackState(if playing {
                MPNowPlayingPlaybackState::Playing
            } else {
                MPNowPlayingPlaybackState::Paused
            });
        }
    }

    pub fn clear(&self) {
        // SAFETY: passing nil is the documented way to clear the info.
        unsafe {
            self.center.setNowPlayingInfo(None);
            self.center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
    }
}

impl Default for NowPlaying {
    fn default() -> Self {
        Self::new()
    }
}

fn on_command(command: &MPRemoteCommand, proxy: EventLoopProxy<UserEvent>, remote: Remote) {
    let block = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
        let _ = proxy.send_event(UserEvent::Remote(remote));
        MPRemoteCommandHandlerStatus::Success
    });
    // SAFETY: the framework copies and retains the handler block.
    unsafe {
        command.setEnabled(true);
        let _ = command.addTargetWithHandler(&block);
    }
}

/// Registers for media keys, headphone buttons and Control Center controls.
pub fn install_remote_commands(proxy: EventLoopProxy<UserEvent>) {
    // SAFETY: called on the main thread, once, before the event loop runs.
    unsafe {
        let center = MPRemoteCommandCenter::sharedCommandCenter();
        on_command(&center.playCommand(), proxy.clone(), Remote::Play);
        on_command(&center.pauseCommand(), proxy.clone(), Remote::Pause);
        on_command(&center.togglePlayPauseCommand(), proxy.clone(), Remote::Toggle);
        on_command(&center.stopCommand(), proxy.clone(), Remote::Stop);
        on_command(&center.nextTrackCommand(), proxy.clone(), Remote::Next);
        on_command(&center.previousTrackCommand(), proxy.clone(), Remote::Prev);

        let seek = center.changePlaybackPositionCommand();
        let block = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            let event: &MPChangePlaybackPositionCommandEvent = event.cast().as_ref();
            let _ = proxy.send_event(UserEvent::Remote(Remote::Seek(event.positionTime())));
            MPRemoteCommandHandlerStatus::Success
        });
        seek.setEnabled(true);
        let _ = seek.addTargetWithHandler(&block);
    }
}

/// What is on the clipboard as text, one entry per line. Files copied in
/// Finder come through as `file://` URLs, since their plain text is only the
/// file name.
pub fn clipboard_text() -> String {
    // SAFETY: the general pasteboard and these type constants are always
    // valid, and reading strings from it has no other requirements.
    unsafe {
        let board = NSPasteboard::generalPasteboard();
        let files: Vec<String> = board
            .pasteboardItems()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.stringForType(NSPasteboardTypeFileURL))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        if !files.is_empty() {
            return files.join("\n");
        }
        board
            .stringForType(NSPasteboardTypeString)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: winit's handle holds a valid NSView for the window's lifetime.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    view.window()
}

/// Asks for a station address in a sheet on `window`. `done` gets the text,
/// or `None` if cancelled. Returns false if the sheet could not be shown.
pub fn ask_for_station(window: &Window, prefill: &str, done: impl FnOnce(Option<String>) + 'static) -> bool {
    let (Some(mtm), Some(ns_window)) = (MainThreadMarker::new(), ns_window(window)) else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Add a radio station"));
    alert.setInformativeText(&NSString::from_str(
        "Paste the stream's address. Links to .pls and .m3u playlists work too.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Add"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let field = NSTextField::initWithFrame(
        mtm.alloc(),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 24.0)),
    );
    field.setStringValue(&NSString::from_str(prefill));
    field.setPlaceholderString(Some(&NSString::from_str("https://")));
    alert.setAccessoryView(Some(&field));
    alert.window().setInitialFirstResponder(Some(&field));
    let done = std::cell::Cell::new(Some(done));
    let handler = RcBlock::new(move |response: NSModalResponse| {
        if let Some(done) = done.take() {
            let text = field.stringValue().to_string();
            done((response == NSAlertFirstButtonReturn).then_some(text));
        }
    });
    alert.beginSheetModalForWindow_completionHandler(&ns_window, Some(&handler));
    true
}

/// Shows scrolling, selectable text in a sheet on `window`. `done` runs when
/// it is dismissed. Returns false if the sheet could not be shown.
pub fn show_text(window: &Window, title: &str, body: &str, done: impl FnOnce() + 'static) -> bool {
    let (Some(mtm), Some(ns_window)) = (MainThreadMarker::new(), ns_window(window)) else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    let scroll = NSTextView::scrollableTextView(mtm);
    scroll.setFrameSize(NSSize::new(440.0, 240.0));
    if let Some(document) = scroll.documentView()
        && let Ok(text) = document.downcast::<NSTextView>()
    {
        text.setString(&NSString::from_str(body));
        text.setEditable(false);
        text.setSelectable(true);
    }
    alert.setAccessoryView(Some(&scroll));
    let done = std::cell::Cell::new(Some(done));
    let handler = RcBlock::new(move |_: NSModalResponse| {
        if let Some(done) = done.take() {
            done();
        }
    });
    alert.beginSheetModalForWindow_completionHandler(&ns_window, Some(&handler));
    true
}

/// One line of a right-click menu.
#[derive(Clone, Debug)]
pub enum MenuItem {
    Action {
        title: String,
        /// The shortcut to show beside it, or empty.
        key: &'static str,
        /// Sent back in `UserEvent::MenuChosen`.
        tag: isize,
        checked: bool,
        enabled: bool,
    },
    Submenu {
        title: String,
        items: Vec<MenuItem>,
    },
    Separator,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FerroampMenuTarget"]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(chosen:))]
        fn chosen(&self, item: &NSMenuItem) {
            if let Some(proxy) = PROXY.get() {
                let _ = proxy.send_event(UserEvent::MenuChosen(item.tag()));
            }
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: NSObject's plain init, on a class that adds no fields.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

fn build_menu(mtm: MainThreadMarker, target: &MenuTarget, items: &[MenuItem]) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for item in items {
        match item {
            MenuItem::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
            MenuItem::Submenu { title, items } => {
                let entry = NSMenuItem::new(mtm);
                entry.setTitle(&NSString::from_str(title));
                entry.setSubmenu(Some(&build_menu(mtm, target, items)));
                menu.addItem(&entry);
            }
            MenuItem::Action { title, key, tag, checked, enabled } => {
                // SAFETY: `chosen:` is implemented by `target`, which outlives
                // every menu because it is never released.
                let entry = unsafe {
                    let entry = NSMenuItem::initWithTitle_action_keyEquivalent(
                        mtm.alloc(),
                        &NSString::from_str(title),
                        Some(sel!(chosen:)),
                        &NSString::from_str(key),
                    );
                    let target: &AnyObject = target.as_ref();
                    entry.setTarget(Some(target));
                    entry
                };
                entry.setKeyEquivalentModifierMask(NSEventModifierFlags(0));
                entry.setTag(*tag);
                entry.setEnabled(*enabled);
                if *checked {
                    entry.setState(NSControlStateValueOn);
                }
                menu.addItem(&entry);
            }
        }
    }
    menu
}

/// Opens a menu at the pointer. A menu runs its own event loop until closed,
/// which would re-enter winit if started from inside one of its callbacks, so
/// this waits for the main thread to come back around.
pub fn show_menu(items: Vec<MenuItem>) {
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        thread_local! {
            static TARGET: std::cell::OnceCell<Retained<MenuTarget>> = const { std::cell::OnceCell::new() };
        }
        TARGET.with(|cell| {
            let target = cell.get_or_init(|| MenuTarget::new(mtm));
            let menu = build_menu(mtm, target, &items);
            menu.popUpMenuPositioningItem_atLocation_inView(None, NSEvent::mouseLocation(), None);
        });
    });
}
