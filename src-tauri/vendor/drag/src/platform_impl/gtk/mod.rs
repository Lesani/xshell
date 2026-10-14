// Copyright 2023-2023 CrabNebula Ltd.
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

// XSHELL-PATCH (see XSHELL-PATCH.md): the result is recorded in `drop-performed` /
// `drag-failed`, and the callback plus the cleanup run in the source widget's `drag-end`, so
// the `drag-data-get` handler is still connected when the drop target asks for the data
// after the drop (tauri-apps/drag-rs#84). No `unwrap` on the cursor position or the handler
// list; file URIs are escaped.

use crate::{CursorPosition, DragItem, DragMode, DragResult, Error, Image, Options};
use gdkx11::{
    gdk,
    glib::{ObjectExt, Propagation, SignalHandlerId},
};
use gtk::{
    gdk_pixbuf,
    prelude::{
        DeviceExt, DragContextExtManual, PixbufLoaderExt, SeatExt, WidgetExt, WidgetExtManual,
    },
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

type Handlers = Rc<RefCell<Vec<SignalHandlerId>>>;

pub fn start_drag<F: Fn(DragResult, CursorPosition) + Send + 'static>(
    window: &gtk::ApplicationWindow,
    item: DragItem,
    image: Image,
    on_drop_callback: F,
    options: Options,
) -> crate::Result<()> {
    log::debug!("Starting drag operation with mode: {:?}", options.mode);
    let handler_ids: Handlers = Rc::new(RefCell::new(vec![]));
    let drag_action = match options.mode {
        DragMode::Copy => gdk::DragAction::COPY,
        DragMode::Move => gdk::DragAction::MOVE,
    };

    log::debug!("Setting drag source with action: {:?}", drag_action);
    window.drag_source_set(gdk::ModifierType::BUTTON1_MASK, &[], drag_action);

    match item {
        DragItem::Files(paths) => {
            log::debug!("Setting up file drag with {} paths", paths.len());
            window.drag_source_add_uri_targets();
            let uris: Vec<String> = paths
                .iter()
                .map(|path| {
                    gtk::glib::filename_to_uri(path, None)
                        .map(|u| u.to_string())
                        .unwrap_or_else(|_| format!("file://{}", path.display()))
                })
                .collect();
            handler_ids
                .borrow_mut()
                .push(window.connect_drag_data_get(move |_, _, data, _, _| {
                    log::debug!("Setting URIs: {:?}", uris);
                    let uris: Vec<&str> = uris.iter().map(|s| s.as_str()).collect();
                    data.set_uris(&uris);
                }));
        }
        DragItem::Data { .. } => {
            // Currently leaving it as is as we can utilize it as a dummy dragging feature
        }
    }

    let Some(target_list) = window.drag_source_get_target_list() else {
        cleanup_signal_handlers(&handler_ids, window);
        return Err(crate::Error::EmptyTargetList);
    };
    log::debug!("Got target list, initiating drag");
    let Some(drag_context) = window.drag_begin_with_coordinates(
        &target_list,
        drag_action,
        gdk::ffi::GDK_BUTTON1_MASK as i32,
        None,
        -1,
        -1,
    ) else {
        cleanup_signal_handlers(&handler_ids, window);
        return Err(crate::Error::FailedToStartDrag);
    };
    log::debug!("Drag context created successfully");
    // Cancel unless a drop is reported before the drag ends.
    let result = Rc::new(Cell::new(DragResult::Cancel));
    on_drop_failed(result.clone(), window, &handler_ids, &options);
    on_drop_performed(result.clone(), &drag_context);
    on_drag_end(Rc::new(on_drop_callback), result, window, &handler_ids);

    log::debug!("Setting up drag icon");
    let icon_pixbuf: Option<gdk_pixbuf::Pixbuf> = match &image {
        Image::Raw(data) => image_binary_to_pixbuf(data),
        Image::File(path) => match std::fs::read(path) {
            Ok(bytes) => image_binary_to_pixbuf(&bytes),
            Err(_) => None,
        },
    };
    if let Some(icon) = icon_pixbuf {
        drag_context.drag_set_icon_pixbuf(&icon, 0, 0);
    }

    Ok(())
}

fn image_binary_to_pixbuf(data: &[u8]) -> Option<gdk_pixbuf::Pixbuf> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader
        .write(data)
        .and_then(|_| loader.close())
        .map_err(|_| ())
        .and_then(|_| loader.pixbuf().ok_or(()))
        .ok()
}

fn on_drop_failed(
    result: Rc<Cell<DragResult>>,
    window: &gtk::ApplicationWindow,
    handler_ids: &Handlers,
    options: &Options,
) {
    log::debug!("Setting up drop failed handler");
    let skip_animatation_on_cancel_or_failure = options.skip_animatation_on_cancel_or_failure;

    let id = window.connect_drag_failed(move |_, _, _drag_result| {
        log::debug!("Drag failed or cancelled");
        result.set(DragResult::Cancel);
        if skip_animatation_on_cancel_or_failure {
            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    });
    handler_ids.borrow_mut().push(id);
}

fn on_drop_performed(result: Rc<Cell<DragResult>>, drag_context: &gdk::DragContext) {
    log::debug!("Setting up drop performed handler");
    // The target may still ask for the data after this: nothing is cleaned up here.
    drag_context.connect_drop_performed(move |context, _| {
        log::debug!("Drop performed");
        log::trace!("Selected action: {:?}", context.selected_action());
        result.set(DragResult::Dropped);
    });
}

fn on_drag_end<F: Fn(DragResult, CursorPosition) + Send + 'static>(
    callback: Rc<F>,
    result: Rc<Cell<DragResult>>,
    window: &gtk::ApplicationWindow,
    handler_ids: &Handlers,
) {
    log::debug!("Setting up drag end handler");
    let window_clone = window.clone();
    let handlers = handler_ids.clone();
    let id = window.connect_drag_end(move |_, _| {
        log::debug!("Drag ended");
        cleanup_signal_handlers(&handlers, &window_clone);
        callback(result.get(), get_cursor_position(&window_clone));
    });
    handler_ids.borrow_mut().push(id);
}

fn cleanup_signal_handlers(handler_ids: &Handlers, window: &gtk::ApplicationWindow) {
    log::debug!("Cleaning up signal handlers");
    let ids: Vec<SignalHandlerId> = handler_ids.borrow_mut().drain(..).collect();
    for handler_id in ids {
        window.disconnect(handler_id);
    }
    window.drag_source_unset();
}

/// The pointer's position, or the origin when the display has no pointer.
fn get_cursor_position(window: &gtk::ApplicationWindow) -> CursorPosition {
    match window
        .display()
        .default_seat()
        .and_then(|seat| seat.pointer())
    {
        Some(cursor) => {
            let (_, x, y) = cursor.position();
            CursorPosition { x, y }
        }
        None => {
            log::debug!("{}", Error::FailedToGetCursorPosition);
            CursorPosition { x: 0, y: 0 }
        }
    }
}
