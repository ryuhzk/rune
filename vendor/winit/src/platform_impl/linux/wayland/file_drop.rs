//! Native wl_data_device file drops. Clipboard selection remains owned by the application.
use super::{state::WinitState, WindowId};
use crate::{dpi::PhysicalPosition, event::WindowEvent};
use ahash::AHashMap;
use sctk::{
    data_device_manager::{
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::DataSourceHandler,
        DataDeviceManagerState, WritePipe,
    },
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            PostAction, RegistrationToken,
        },
        client::{
            backend::ObjectId,
            globals::GlobalList,
            protocol::{
                wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
                wl_data_source::WlDataSource, wl_seat::WlSeat, wl_surface::WlSurface,
            },
            Connection, Proxy, QueueHandle,
        },
    },
};
use std::{
    collections::HashSet, io::Read, os::unix::ffi::OsStringExt, path::PathBuf, time::Duration,
};

const MIME: &str = "text/uri-list";
const MAX_BYTES: usize = 1024 * 1024;
// SCTK 0.19 retains source_actions but drops an action received before enter.
// Some compositors (including Hyprland) send it only in that order. We
// request only Copy and never acknowledge completion for an unknown action.
fn copy_drop(version: u32, selected: DndAction, offered: DndAction) -> bool {
    version < 3
        || selected == DndAction::Copy
        || (selected.is_empty() && offered.contains(DndAction::Copy))
}

struct Drag {
    offer: DragOffer,
    window: WindowId,
    at: (f64, f64),
    files: Option<Vec<PathBuf>>,
    dropped: bool,
    read: Option<RegistrationToken>,
    timer: Option<RegistrationToken>,
}
pub struct FileDropState {
    manager: Option<DataDeviceManagerState>,
    devices: AHashMap<ObjectId, DataDevice>,
    drags: AHashMap<ObjectId, Drag>,
}
impl FileDropState {
    pub fn new(globals: &GlobalList, qh: &QueueHandle<WinitState>) -> Self {
        Self {
            manager: DataDeviceManagerState::bind(globals, qh).ok(),
            devices: Default::default(),
            drags: Default::default(),
        }
    }
    pub fn add_seat(&mut self, qh: &QueueHandle<WinitState>, seat: &WlSeat) {
        if let Some(manager) = &self.manager {
            self.devices
                .insert(seat.id(), manager.get_data_device(qh, seat));
        }
    }
    pub fn remove_seat(&mut self, seat: &WlSeat) -> Option<ObjectId> {
        self.devices.remove(&seat.id()).map(|d| d.inner().id())
    }
    fn offer(&self, device: &WlDataDevice) -> Option<DragOffer> {
        self.devices
            .values()
            .find(|d| d.inner() == device)?
            .data()
            .drag_offer()
    }
}
impl WinitState {
    fn file_drag_position(&mut self, window: WindowId, at: (f64, f64)) {
        if let Some(state) = self.windows.get_mut().get(&window) {
            let scale = state.lock().unwrap().scale_factor();
            self.events_sink.push_window_event(
                WindowEvent::FileDragMoved {
                    position: PhysicalPosition::new(at.0 * scale, at.1 * scale),
                },
                window,
            );
        }
    }
    pub(super) fn cancel_window_file_drags(&mut self, window: WindowId) {
        let ids: Vec<_> = self
            .file_drop
            .drags
            .iter()
            .filter(|(_, d)| d.window == window)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.cancel_file_drag(&id);
        }
    }
    pub(super) fn cancel_file_drag(&mut self, id: &ObjectId) {
        if let Some(drag) = self.file_drop.drags.remove(id) {
            if let Some(token) = drag.read {
                self.loop_handle.remove(token);
            }
            if let Some(token) = drag.timer {
                self.loop_handle.remove(token);
            }
            self.events_sink
                .push_window_event(WindowEvent::HoveredFileCancelled, drag.window);
            if drag.dropped {
                if drag.offer.selected_action == DndAction::Copy {
                    drag.offer.finish();
                }
                drag.offer.destroy();
            }
        }
    }
    fn publish_file_drag(&mut self, id: &ObjectId) {
        let Some(drag) = self.file_drop.drags.get(id) else {
            return;
        };
        let Some(files) = drag.files.clone() else {
            return;
        };
        let (window, at, dropped) = (drag.window, drag.at, drag.dropped);
        self.file_drag_position(window, at);
        for file in files {
            self.events_sink.push_window_event(
                if dropped {
                    WindowEvent::DroppedFile(file)
                } else {
                    WindowEvent::HoveredFile(file)
                },
                window,
            );
        }
        if dropped {
            if let Some(drag) = self.file_drop.drags.remove(id) {
                if let Some(token) = drag.timer {
                    self.loop_handle.remove(token);
                }
                if drag.offer.selected_action == DndAction::Copy {
                    drag.offer.finish();
                }
                drag.offer.destroy();
            }
        }
    }
}
impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        let id = device.id();
        self.cancel_file_drag(&id);
        let Some(offer) = self.file_drop.offer(device) else {
            return;
        };
        if !offer.with_mime_types(|m| m.iter().any(|m| m == MIME)) {
            return;
        }
        let window = super::make_wid(surface);
        if !self.windows.get_mut().contains_key(&window) {
            return;
        }
        offer.accept_mime_type(offer.serial, Some(MIME.into()));
        offer.set_actions(DndAction::Copy, DndAction::Copy);
        let Ok(pipe) = offer.receive(MIME.into()) else {
            return;
        };
        self.file_drag_position(window, (x, y));
        self.file_drop.drags.insert(
            id.clone(),
            Drag {
                offer: offer.clone(),
                window,
                at: (x, y),
                files: None,
                dropped: false,
                read: None,
                timer: None,
            },
        );
        let key = id.clone();
        let identity = offer.inner().id();
        let mut bytes = Vec::new();
        let source = self.loop_handle.insert_source(pipe, move |_, file, state| {
            if !state
                .file_drop
                .drags
                .get(&key)
                .is_some_and(|d| d.offer.inner().id() == identity)
            {
                return PostAction::Remove;
            }
            let mut chunk = [0; 8192];
            // The readiness callback performs one bounded read; it never closes the registered FD.
            let result = unsafe { file.get_mut() }.read(&mut chunk);
            match result {
                Ok(0) => {
                    let paths = parse_uri_list(&bytes);
                    if let Some(drag) = state.file_drop.drags.get_mut(&key) {
                        drag.read = None;
                        drag.files = Some(paths);
                    }
                    state.publish_file_drag(&key);
                    PostAction::Remove
                }
                Ok(n) if bytes.len() + n <= MAX_BYTES => {
                    bytes.extend_from_slice(&chunk[..n]);
                    PostAction::Continue
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    PostAction::Continue
                }
                _ => {
                    if let Some(drag) = state.file_drop.drags.get_mut(&key) {
                        drag.read = None;
                    }
                    state.cancel_file_drag(&key);
                    PostAction::Remove
                }
            }
        });
        match source {
            Ok(token) => self.file_drop.drags.get_mut(&id).unwrap().read = Some(token),
            Err(_) => {
                self.cancel_file_drag(&id);
                return;
            }
        }
        let identity = offer.inner().id();
        let timer_id = id.clone();
        let timer = self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_secs(10)),
            move |_, _, state| {
                if let Some(drag) = state
                    .file_drop
                    .drags
                    .get_mut(&id)
                    .filter(|d| d.offer.inner().id() == identity)
                {
                    drag.timer = None;
                    if drag.read.is_some() {
                        state.cancel_file_drag(&id);
                    }
                }
                TimeoutAction::Drop
            },
        );
        if let Ok(token) = timer {
            if let Some(drag) = self.file_drop.drags.get_mut(&timer_id) {
                drag.timer = Some(token);
            }
        }
    }
    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        if let Some(drag) = self.file_drop.drags.get_mut(&device.id()) {
            drag.at = (x, y);
            let window = drag.window;
            self.file_drag_position(window, (x, y));
        }
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        if !self
            .file_drop
            .drags
            .get(&device.id())
            .is_some_and(|d| d.dropped)
        {
            self.cancel_file_drag(&device.id());
        }
    }
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        if let Some(drag) = self.file_drop.drags.get_mut(&device.id()) {
            if drag.dropped {
                return;
            }
            drag.dropped = true;
            if !copy_drop(
                drag.offer.inner().version(),
                drag.offer.selected_action,
                drag.offer.source_actions,
            ) {
                self.cancel_file_drag(&device.id());
            } else {
                self.publish_file_drag(&device.id());
            }
        }
    }
}
impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: DndAction,
    ) {
        for drag in self.file_drop.drags.values_mut() {
            if drag.offer.inner() == offer.inner() {
                drag.offer.source_actions = actions;
            }
        }
        offer.set_actions(DndAction::Copy, DndAction::Copy);
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        action: DndAction,
    ) {
        for drag in self.file_drop.drags.values_mut() {
            if drag.offer.inner() == offer.inner() {
                drag.offer.selected_action = action;
            }
        }
    }
}
// This receiver never creates sources or changes the clipboard selection.
impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: String,
        _: WritePipe,
    ) {
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}
sctk::delegate_data_device!(WinitState);

fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(uri) = line.strip_prefix(b"file://") else {
            continue;
        };
        let uri = uri.strip_prefix(b"localhost").unwrap_or(uri);
        if !uri.starts_with(b"/") || uri.contains(&b'?') || uri.contains(&b'#') {
            continue;
        }
        let mut decoded = Vec::new();
        let mut i = 0;
        while i < uri.len() {
            if uri[i] == b'%' {
                let hex = |b: u8| (b as char).to_digit(16).map(|n| n as u8);
                let Some(byte) = uri
                    .get(i + 1)
                    .and_then(|b| hex(*b))
                    .zip(uri.get(i + 2).and_then(|b| hex(*b)))
                    .map(|(a, b)| a * 16 + b)
                else {
                    decoded.clear();
                    break;
                };
                decoded.push(byte);
                i += 3;
            } else {
                decoded.push(uri[i]);
                i += 1;
            }
        }
        if decoded.is_empty() || decoded.contains(&0) {
            continue;
        }
        let path = PathBuf::from(std::ffi::OsString::from_vec(decoded));
        if seen.insert(path.clone()) {
            paths.push(path);
        }
        if paths.len() == 4096 {
            break;
        }
    }
    paths
}

#[cfg(test)]
#[path = "file_drop_tests.rs"]
mod tests;
