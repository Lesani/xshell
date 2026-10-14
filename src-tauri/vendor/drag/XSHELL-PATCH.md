# drag 2.1.1, patched for xshell

This is drag 2.1.1 from crates.io (CrabNebula, tauri-apps/drag-rs, `crates/drag`), with the
changes below. `src-tauri/Cargo.toml` uses it through `[patch.crates-io]`. The places are
marked `XSHELL-PATCH`. The registry's `Cargo.lock` is not copied.

## The patches

1. **Drops that never land (tauri-apps/drag-rs#84), `src/platform_impl/gtk/mod.rs`.**
   Upstream removes its `drag-data-get` handler and calls the callback in the drag
   context's `drop-performed` signal. A drop target asks for the data after
   `drop-performed` (always on Wayland, often on X11), when the handler is gone, so the drop
   reports success but no file lands. Now `drop-performed` and `drag-failed` only record
   the result. The cleanup (disconnect the handlers, `drag_source_unset`) and the callback
   run in the source widget's `drag-end`, which GTK emits after the target has finished.
   The handler list is an `Rc<RefCell<…>>` (everything runs on the GTK main thread) and a
   failed start disconnects what it connected.
2. **No panics on runtime paths.** xshell's release builds use `panic = "abort"`, so a
   panic would kill the app:
   - GTK: `get_cursor_position(..).unwrap()` in the callbacks is replaced by a fallback
     position (0, 0).
   - Windows: the `unwrap()`s on the data object, the shell item array and `GlobalAlloc`
     return errors instead. `GetCursorPos` failing no longer skips the callback.
     (`DataObject::new` keeps its `unwrap()`: it is dead code.)
   - macOS: the `expect()`s on the window, content view, drag image and event return
     errors instead.
3. **File URIs, GTK.** Upstream builds `file://{path}` without escaping, so a name with a
   space or `#` gives a broken URI. It now uses `glib::filename_to_uri` (escaped), with the
   old form as a fallback.

`diff -ru <crates.io drag-2.1.1> src-tauri/vendor/drag` shows the full change.

## How to drop it

When an upstream drag release fixes #84 and the panics:
1. Remove the `drag` entry from `[patch.crates-io]` in `src-tauri/Cargo.toml`, and bump
   the `drag` version there.
2. Delete `src-tauri/vendor/drag`.
3. Run `cargo update -p drag`.
4. Re-run the manual drag-out checks (plan-x46 §7, items 1 and 8) on Linux (Wayland and
   X11), Windows and macOS.
