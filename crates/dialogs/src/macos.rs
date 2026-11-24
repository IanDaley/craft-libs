//! macOS presentation.
//!
//! Open, Folder and Save show as free-standing panels (`-[NSOpenPanel beginWithCompletionHandler:]`,
//! `-[NSSavePanel beginWithCompletionHandler:]`), as native apps show them: the panel doesn't
//! block the window, and it stays where the user moves it when the window is resized. A sheet
//! stays centred on its window, which feels wrong for Open. rfd can't show this panel: its async
//! dialogs always begin a sheet, on the main or first window when no parent is given (rfd 0.17.2
//! `backend/macos/modal_future.rs:71-102`), and on macOS 27 that sheet never appears.
//!
//! Message boxes are sheets on the parent window through rfd, as native apps show them. rfd falls
//! back to the blocking `runModal` when the app isn't running or has no window
//! (`modal_future.rs:79-118`), so [`sheet_parent`] lets a sheet start only with a parent in a
//! running app.

use std::cell::Cell;
use std::ffi::c_void;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSModalResponse, NSModalResponseAbort, NSModalResponseOK, NSOpenPanel, NSPanel, NSSavePanel, NSView, NSWindow};
use objc2_foundation::{NSArray, NSString, NSURL};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{Answer, FileDialog, Kind, Reply};

const NOT_MAIN: &str = "dialogs show from the main thread only";
const NOT_RUNNING: &str = "the app isn't running yet, so the dialog can't be shown";
const SAVE_NOT_SHOWN: &str = "the save panel couldn't be shown";
const SAVE_NO_WINDOW: &str = "the window to show the save panel on isn't available";

/// The free-standing panel a [`crate::Dialogs`] has on screen.
pub(crate) enum Panel {
    /// An Open or Folder panel.
    Open(Retained<NSOpenPanel>),
    /// A Save panel.
    Save(Retained<NSSavePanel>),
}

/// Brings `panel` to the front, so a command that would show it again shows where it is. A panel
/// that has closed, but whose answer the app hasn't collected yet, stays closed.
pub(crate) fn raise(panel: &Panel) {
    let window: &NSPanel = match panel {
        Panel::Open(panel) => panel,
        Panel::Save(panel) => panel,
    };
    if window.isVisible() {
        window.makeKeyAndOrderFront(None);
    }
}

/// The window a save sheet attaches to, read from `parent`'s AppKit view.
#[allow(unsafe_code)]
fn parent_window<W: HasWindowHandle>(parent: Option<&W>) -> Option<Retained<NSWindow>> {
    let parent = parent?;
    let RawWindowHandle::AppKit(handle) = parent.window_handle().ok()?.as_raw() else { return None };
    // SAFETY: the pointer is the window's content view, which AppKit keeps alive while the window
    // is open, and winit hands out the handle only while it is.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

/// The parent a sheet can be shown on, or why there is none.
pub(crate) fn sheet_parent<W: HasWindowHandle>(parent: Option<&W>) -> Result<&W, String> {
    let mtm = MainThreadMarker::new().ok_or(NOT_MAIN)?;
    let app = NSApplication::sharedApplication(mtm);
    if !app.isRunning() {
        return Err(NOT_RUNNING.into());
    }
    let parent = parent.ok_or("no window to show the dialog on")?;
    // rfd `expect`s the parent's view to be in a window (rfd 0.17.2 `backend/macos/utils.rs:34-46`),
    // and a panic aborts under `panic = "abort"`. Look for the view among the app's windows.
    let view = match parent.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr().cast_const(),
        _ => return Err("the window has no macOS view".into()),
    };
    if !app.windows().iter().any(|window| window.contentView().is_some_and(|content| contains(&content, view))) {
        return Err("the window to show the dialog on isn't on screen".into());
    }
    Ok(parent)
}

/// Whether `view` is `root` or one of its subviews.
fn contains(root: &NSView, view: *const c_void) -> bool {
    std::ptr::eq((root as *const NSView).cast(), view) || root.subviews().iter().any(|child| contains(&child, view))
}

/// Shows an Open or Folder panel that doesn't block the window and answers through `reply` when
/// it closes. AppKit calls the completion handler on the main thread.
pub(crate) fn show_panel(d: &FileDialog, reply: Reply) -> Option<Panel> {
    let Some(mtm) = MainThreadMarker::new() else {
        reply.send(Answer::Unavailable(NOT_MAIN.into()));
        return None;
    };
    // Begun before the app runs, the panel would never show or answer.
    if !NSApplication::sharedApplication(mtm).isRunning() {
        reply.send(Answer::Unavailable(NOT_RUNNING.into()));
        return None;
    }
    let panel = NSOpenPanel::openPanel(mtm);
    let folder = d.kind == Kind::Folder;
    panel.setCanChooseFiles(!folder);
    panel.setCanChooseDirectories(folder);
    panel.setCanCreateDirectories(folder);
    panel.setAllowsMultipleSelection(d.kind == (Kind::Open { multiple: true }));
    if let Some(title) = &d.title {
        // Panels have no title bar on current macOS; the message line shows above the files.
        panel.setMessage(Some(&NSString::from_str(title)));
    }
    let extensions: Vec<_> =
        d.filters.iter().flat_map(|f| &f.extensions).map(|e| e.trim_start_matches('.')).filter(|e| !e.is_empty()).map(NSString::from_str).collect();
    if !extensions.is_empty() {
        // As rfd does. `allowedContentTypes` would add a dependency for UTType bindings; plain
        // extensions match in any letter case.
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&extensions)));
    }
    if let Some(directory) = d.directory.as_deref().and_then(NSURL::from_directory_path) {
        panel.setDirectoryURL(Some(&directory));
    }
    // The handler keeps the panel alive until it closes; AppKit releases the handler after
    // calling it, which ends the cycle. Never called, the reply drops and answers Unavailable.
    let reply = Cell::new(Some(reply));
    let answered = panel.clone();
    let handler = RcBlock::new(move |response: NSModalResponse| {
        let Some(reply) = reply.take() else { return };
        if response != NSModalResponseOK {
            return reply.send(Answer::Cancelled);
        }
        let paths: Vec<_> = answered.URLs().to_vec().iter().filter_map(|url| url.to_file_path()).collect();
        reply.send(if paths.is_empty() { Answer::Cancelled } else { Answer::Paths(paths) });
    });
    panel.beginWithCompletionHandler(&handler);
    Some(Panel::Open(panel))
}

/// Shows a Save panel that doesn't block the window and answers through `reply` when it closes.
/// AppKit calls the completion handler on the main thread.
pub(crate) fn show_save_panel<W: HasWindowHandle>(d: &FileDialog, parent: Option<&W>, reply: Reply) -> Option<Panel> {
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("macOS save panel: not the main thread");
        reply.send(Answer::Unavailable(NOT_MAIN.into()));
        return None;
    };
    // Begun before the app runs, the panel would never show or answer.
    if !NSApplication::sharedApplication(mtm).isRunning() {
        log::warn!("macOS save panel: the app isn't running yet");
        reply.send(Answer::Unavailable(NOT_RUNNING.into()));
        return None;
    }
    let Some(window) = parent_window(parent) else {
        log::warn!("macOS save panel: no window to attach the sheet to");
        reply.send(Answer::Unavailable(SAVE_NO_WINDOW.into()));
        return None;
    };
    // `+[NSSavePanel savePanel]` returned NULL on macOS 15.7 (#5980); `alloc`/`init` doesn't.
    let panel = NSSavePanel::new(mtm);
    panel.setCanCreateDirectories(true);
    if let Some(title) = &d.title {
        // Panels have no title bar on current macOS; the message line shows above the name.
        panel.setTitle(Some(&NSString::from_str(title)));
        panel.setMessage(Some(&NSString::from_str(title)));
    }
    if let Some(name) = &d.file_name {
        panel.setNameFieldStringValue(&NSString::from_str(name));
    }
    let extensions: Vec<_> =
        d.filters.iter().flat_map(|f| &f.extensions).map(|e| e.trim_start_matches('.')).filter(|e| !e.is_empty()).map(NSString::from_str).collect();
    if !extensions.is_empty() {
        // As rfd does. `allowedContentTypes` would add a dependency for UTType bindings; plain
        // extensions match in any letter case.
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&extensions)));
    }
    if let Some(directory) = d.directory.as_deref().and_then(NSURL::from_directory_path) {
        panel.setDirectoryURL(Some(&directory));
    }
    // The handler keeps the panel alive until it closes; AppKit releases the handler after
    // calling it, which ends the cycle. Never called, the reply drops and answers Unavailable.
    let reply = Cell::new(Some(reply));
    let answered = panel.clone();
    let handler = RcBlock::new(move |response: NSModalResponse| {
        let Some(reply) = reply.take() else { return };
        // Abort is the panel failing to display, not the user closing it.
        if response == NSModalResponseAbort {
            log::warn!("macOS save panel: AppKit aborted the sheet");
            return reply.send(Answer::Unavailable(SAVE_NOT_SHOWN.into()));
        }
        if response != NSModalResponseOK {
            return reply.send(Answer::Cancelled);
        }
        match answered.URL().and_then(|url| url.to_file_path()) {
            Some(path) => reply.send(Answer::Saved { path, filter: None }),
            None => reply.send(Answer::Cancelled),
        }
    });
    log::info!("macOS save panel: sheet on the app window, {} filter(s), name {:?}", d.filters.len(), d.file_name);
    panel.beginSheetModalForWindow_completionHandler(&window, &handler);
    Some(Panel::Save(panel))
}
