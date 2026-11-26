//! The window of the native macOS host: an `NSWindow` (1920x1080 content, "VIctory") whose view
//! is the `CAMetalLayer` the GPU presents into, driven by an AppKit event loop on the process
//! main thread while the title runs on its own threads. Closing the window ends the process.
//!
//! AppKit is reached through the Objective-C runtime (`objc2`'s `msg_send!`), with no binding
//! crate for it. The loop is `nextEventMatchingMask:` by hand rather than `[NSApp run]`, so the
//! title finishing is noticed without a delegate class.

use std::sync::{Arc, Mutex};

// No binding crate links AppKit, so its classes would not be registered without this.
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

use anyhow::{Context, Result};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// Content size in points.
pub const WIDTH: usize = 1920;
pub const HEIGHT: usize = 1080;

/// Shows a window over `layer` (a `CAMetalLayer`) and runs `title` on a thread of its own until
/// it returns, or the window is closed (then the process exits). Call on the main thread.
pub fn run<T: Send + 'static>(
    layer: *mut std::ffi::c_void,
    title: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<Option<T>> {
    anyhow::ensure!(
        // SAFETY: a plain libc query.
        unsafe { pthread_main_np() } != 0,
        "the window needs the process main thread"
    );
    // SAFETY: AppKit calls on the main thread with objects of the right classes.
    let window = unsafe { open(layer) }?;
    let done: Arc<Mutex<Option<Result<T>>>> = Arc::default();
    let slot = done.clone();
    std::thread::Builder::new()
        .name("title-launch".into())
        .spawn(move || {
            let result = title();
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        })
        .context("starting the title")?;
    // SAFETY: as above.
    unsafe { event_loop(&window, || done.lock().is_ok_and(|d| d.is_some())) };
    let finished = done.lock().unwrap_or_else(|e| e.into_inner()).take();
    match finished {
        Some(result) => result.map(Some),
        None => {
            // The window was closed.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
    }
}

unsafe extern "C" {
    fn pthread_main_np() -> i32;
}

/// The application, its menu (Quit) and the window, shown.
unsafe fn open(layer: *mut std::ffi::c_void) -> Result<Retained<AnyObject>> {
    // SAFETY: documented AppKit calls; every receiver is a class or an object just made.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        anyhow::ensure!(!app.is_null(), "no NSApplication");
        let _: Bool = msg_send![app, setActivationPolicy: 0isize]; // regular: a Dock icon and menu
        let menubar: Retained<AnyObject> = msg_send![class!(NSMenu), new];
        let item: Retained<AnyObject> = msg_send![class!(NSMenuItem), new];
        let _: () = msg_send![&*menubar, addItem: &*item];
        let menu: Retained<AnyObject> = msg_send![class!(NSMenu), new];
        let quit: Retained<AnyObject> = msg_send![
            msg_send![class!(NSMenuItem), alloc],
            initWithTitle: &*NSString::from_str("Quit VIctory"),
            action: objc2::sel!(terminate:),
            keyEquivalent: &*NSString::from_str("q")
        ];
        let _: () = msg_send![&*menu, addItem: &*quit];
        let _: () = msg_send![&*item, setSubmenu: &*menu];
        let _: () = msg_send![app, setMainMenu: &*menubar];

        let rect = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(WIDTH as f64, HEIGHT as f64),
        );
        // titled | closable | miniaturizable | resizable; buffered.
        let window: Retained<AnyObject> = msg_send![
            msg_send![class!(NSWindow), alloc],
            initWithContentRect: rect,
            styleMask: 15usize,
            backing: 2usize,
            defer: Bool::NO
        ];
        let _: () = msg_send![&*window, setTitle: &*NSString::from_str("VIctory")];
        // The view hosts our layer: set the layer first, then ask for layers.
        let view: *mut AnyObject = msg_send![&*window, contentView];
        let layer = layer.cast::<AnyObject>();
        let _: () = msg_send![layer, setFrame: rect];
        let _: () = msg_send![layer, setAutoresizingMask: 18u32]; // width and height sizable
        let _: () = msg_send![view, setLayer: layer];
        let _: () = msg_send![view, setWantsLayer: Bool::YES];
        let _: () = msg_send![&*window, center];
        let _: () = msg_send![&*window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![app, finishLaunching];
        let _: () = msg_send![app, activateIgnoringOtherApps: Bool::YES];
        Ok(window)
    }
}

/// macOS key code to the Windows virtual key the game's stand-in pad reads (README lists the
/// mapping); `None` for keys the pad does not use.
fn virtual_key(code: u16) -> Option<u8> {
    Some(match code {
        36 | 76 => 0x0D, // Return, keypad Enter
        48 => 0x09,      // Tab
        49 => 0x20,      // Space
        51 => 0x08,      // Delete (Backspace)
        53 => 0x1B,      // Escape
        123 => 0x25,     // arrows
        126 => 0x26,
        124 => 0x27,
        125 => 0x28,
        0 => b'A',
        1 => b'S',
        2 => b'D',
        3 => b'F',
        4 => b'H',
        6 => b'Z',
        7 => b'X',
        8 => b'C',
        9 => b'V',
        12 => b'Q',
        13 => b'W',
        14 => b'E',
        16 => b'Y',
        34 => b'I',
        38 => b'J',
        40 => b'K',
        37 => b'L',
        _ => return None,
    })
}

/// Hands a key-down or key-up to the stand-in pad. True when it was one (Command shortcuts
/// such as Cmd+Q are left to AppKit), so the caller does not forward it, and AppKit does not beep.
unsafe fn key_event(event: *mut AnyObject) -> bool {
    // SAFETY: `event` is a live NSEvent; `type` is valid for every event.
    let kind: usize = unsafe { msg_send![event, type] };
    if !matches!(kind, 10 | 11) {
        return false;
    }
    // Only key events have a key code: NSEvent raises for any other kind.
    // SAFETY: a key-down or key-up NSEvent; these are its plain getters.
    let (code, flags): (u16, usize) =
        unsafe { (msg_send![event, keyCode], msg_send![event, modifierFlags]) };
    if flags & (1 << 20) != 0 {
        return false;
    }
    if let Some(vk) = virtual_key(code) {
        victory_hle::modules::gameinput::set_live_key(vk, kind == 10);
    }
    true
}

/// Runs the loop until `finished`, or the window is gone.
unsafe fn event_loop(window: &AnyObject, finished: impl Fn() -> bool) {
    let mode = NSString::from_str("kCFRunLoopDefaultMode"); // NSDefaultRunLoopMode
    loop {
        let go_on = autoreleasepool(|_| {
            // SAFETY: AppKit calls on the main thread.
            unsafe {
                let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
                let until: *mut AnyObject =
                    msg_send![class!(NSDate), dateWithTimeIntervalSinceNow: 0.01f64];
                let event: *mut AnyObject = msg_send![
                    app,
                    nextEventMatchingMask: u64::MAX,
                    untilDate: until,
                    inMode: &*mode,
                    dequeue: Bool::YES
                ];
                if !event.is_null() && !key_event(event) {
                    let _: () = msg_send![app, sendEvent: event];
                }
                let key: Bool = msg_send![window, isKeyWindow];
                if !key.as_bool() {
                    // A key-up sent to another window never reaches us.
                    victory_hle::modules::gameinput::release_live_keys();
                }
                let _: () = msg_send![app, updateWindows];
                let shown: Bool = msg_send![window, isVisible];
                let minimized: Bool = msg_send![window, isMiniaturized];
                (shown.as_bool() || minimized.as_bool()) && !finished()
            }
        });
        if !go_on {
            // SAFETY: as above.
            let shown: Bool = unsafe { msg_send![window, isVisible] };
            if shown.as_bool() {
                // The title ended with the window still up: close it.
                // SAFETY: as above.
                unsafe {
                    let _: () = msg_send![window, close];
                }
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::virtual_key;

    #[test]
    fn keys_map_to_the_pad_keys() {
        // Return, Space, Esc, Delete, Tab, Left; W (13), A (0), D (2), Q (12), E (14), Z (6), C (8).
        let got = [36, 49, 53, 51, 48, 123, 13, 0, 2, 12, 14, 6, 8].map(virtual_key);
        let want = [0x0D, 0x20, 0x1B, 0x08, 0x09, 0x25, b'W', b'A', b'D', b'Q', b'E', b'Z', b'C'];
        assert_eq!(got, want.map(Some));
        assert_eq!(virtual_key(10), None); // section sign: not a pad key
        assert_eq!(virtual_key(50), None);
    }
}
