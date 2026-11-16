use anyhow::{Context, Result};
use legion_display_channel::Input;
use smithay::{
    backend::{
        allocator::{Format, dmabuf::Dmabuf},
        input::KeyState,
        renderer::{ImportDma, gles::GlesRenderer},
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat,
    delegate_shm, delegate_viewporter, delegate_xdg_shell,
    input::{Seat, SeatHandler, SeatState, keyboard::FilterResult, pointer::MotionEvent},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_surface::WlSurface},
        },
    },
    utils::{SERIAL_COUNTER, Serial},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, TraversalAction, get_parent, with_states,
            with_surface_tree_downward,
        },
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
        viewporter::ViewporterState,
    },
};
use std::os::fd::OwnedFd;

pub struct State {
    compositor: CompositorState,
    syncobj: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,
    pending_sync: Option<crate::syncobj::Release>,
    acquire_watches: Vec<crate::syncobj::Watch>,
    xdg: XdgShellState,
    pub(crate) xwayland_shell: smithay::wayland::xwayland_shell::XWaylandShellState,
    pub(crate) xwm: Option<smithay::xwayland::X11Wm>,
    pub(crate) x11_windows: Vec<smithay::xwayland::X11Surface>,
    pub(crate) x11_maps: u64,
    pub(crate) x11_associations: u64,
    shm: ShmState,
    dmabuf: DmabufState,
    seats: SeatState<Self>,
    data: DataDeviceState,
    pub(crate) seat: Seat<Self>,
    pub(crate) ordered_input: crate::input_events::InputState,
    pub(crate) _output: Output,
    _output_manager: OutputManagerState,
    _viewporter: ViewporterState,
    pub renderer: Option<GlesRenderer>,
    pub pending_imports: Vec<(Dmabuf, ImportNotifier)>,
    pub destroyed_buffers: Vec<WlBuffer>,
    pub track_buffers: bool,
    pub gpu_only: bool,
    pub diagnose_surfaces: bool,
    pub(crate) cursor_image: smithay::input::pointer::CursorImageStatus,
    pub(crate) cursor_generation: u64,
    pub last_source_surface: Option<WlSurface>,
    presentation_root: Option<WlSurface>,
    pub inactive_surface_buffers: u64,
    pub discarded_on_selection_change: u64,
    pub presentation_switches: u64,
    pub shm_skipped: u64,
    pub pending: Option<WlBuffer>,
    pub error: Option<String>,
    pub last_input: Input,
    pub imports: u64,
    pub source_commits: u64,
    pub(crate) source_cadence: crate::frame_timing::Cadence,
    pub superseded: u64,
    pub keyboard_events: u64,
    pub pointer_events: u64,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pointer_width: i32,
    pointer_height: i32,
}
impl State {
    pub(crate) fn enable_explicit_sync(
        &mut self,
        dh: &DisplayHandle,
        file: std::fs::File,
    ) -> Result<bool> {
        use smithay::{
            backend::drm::DrmDeviceFd,
            utils::DeviceFd,
            wayland::drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd},
        };
        let device = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file)));
        if !supports_syncobj_eventfd(&device) {
            return Ok(false);
        }
        crate::syncobj::verify_device(&device).context("qualify guest timeline operations")?;
        self.syncobj = Some(DrmSyncobjState::new::<Self>(dh, device));
        Ok(true)
    }
    pub(crate) fn acquire_pending_release(&mut self) -> Option<crate::syncobj::Release> {
        self.pending_sync.take()
    }
    pub(crate) fn discard_pending(&mut self) {
        if let Some(buffer) = self.pending.take() {
            let sync = self.pending_sync.take();
            self.discard_buffer(buffer, sync);
        }
    }
    fn discard_buffer(&mut self, buffer: WlBuffer, sync: Option<crate::syncobj::Release>) {
        if let Some(sync) = sync
            && let Err(error) = sync.signal()
        {
            self.error = Some(error.to_string());
        }
        if buffer.is_alive() {
            buffer.release();
        }
    }
    pub(crate) fn poll_explicit_sync(&mut self, dh: &DisplayHandle) -> Result<()> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.acquire_watches.len() {
            if self.acquire_watches[index].ready()? {
                ready.push(self.acquire_watches.swap_remove(index));
            } else {
                index += 1;
            }
        }
        for watch in ready {
            watch.release();
            if let Some(client) = watch.surface.client() {
                self.client_compositor_state(&client)
                    .blocker_cleared(self, dh);
            }
        }
        Ok(())
    }
    pub(crate) fn pending_sync_acquires(&self) -> usize {
        self.acquire_watches.len()
    }
    pub fn close_clients(&self) {
        for surface in self.xdg.toplevel_surfaces() {
            surface.send_close();
        }
        for surface in &self.x11_windows {
            let _ = surface.close();
        }
    }
    pub fn new(
        dh: &DisplayHandle,
        renderer: GlesRenderer,
        dev: u64,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let formats = renderer
            .egl_context()
            .display()
            .dmabuf_texture_formats()
            .iter()
            .copied()
            .collect();
        Self::with_formats(dh, Some(renderer), formats, dev, width, height)
    }
    pub fn forwarding(
        dh: &DisplayHandle,
        formats: Vec<Format>,
        dev: u64,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let mut state = Self::with_formats(dh, None, formats, dev, width, height)?;
        // Local presentation owns the whole seat: the guest evdev source can
        // always provide relative motion (EV_REL, or deltas synthesized from
        // absolute axes), so pointer-lock clients must see the protocols from
        // their first registry roundtrip.
        state.enable_relative_input(dh);
        Ok(state)
    }
    pub(crate) fn with_formats(
        dh: &DisplayHandle,
        renderer: Option<GlesRenderer>,
        formats: Vec<Format>,
        dev: u64,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let width = i32::try_from(width)?;
        let height = i32::try_from(height)?;
        let mut seats = SeatState::new();
        let mut seat = seats.new_wl_seat(dh, "legion");
        seat.add_keyboard(Default::default(), 200, 25)
            .context("create keyboard keymap")?;
        seat.add_pointer();
        let mut dmabuf = DmabufState::new();
        if !formats.is_empty() {
            let feedback = DmabufFeedbackBuilder::new(dev, formats).build()?;
            dmabuf.create_global_with_default_feedback::<Self>(dh, &feedback);
        }
        let output_manager = OutputManagerState::new_with_xdg_output::<Self>(dh);
        let output = Output::new(
            "legion-0".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Legion".into(),
                model: "Shared presentation".into(),
            },
        );
        let mode = Mode {
            size: (width, height).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);
        output.create_global::<Self>(dh);
        Ok(Self {
            compositor: CompositorState::new::<Self>(dh),
            syncobj: None,
            pending_sync: None,
            acquire_watches: Vec::new(),
            xdg: XdgShellState::new::<Self>(dh),
            xwayland_shell: smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(dh),
            xwm: None,
            x11_windows: Vec::new(),
            x11_maps: 0,
            x11_associations: 0,
            shm: ShmState::new::<Self>(dh, vec![]),
            dmabuf,
            seats,
            data: DataDeviceState::new::<Self>(dh),
            seat,
            ordered_input: crate::input_events::InputState::default(),
            _output: output,
            _output_manager: output_manager,
            _viewporter: ViewporterState::new::<Self>(dh),
            renderer,
            pending_imports: Vec::new(),
            destroyed_buffers: Vec::new(),
            track_buffers: false,
            gpu_only: false,
            diagnose_surfaces: false,
            cursor_image: smithay::input::pointer::CursorImageStatus::default_named(),
            cursor_generation: 0,
            last_source_surface: None,
            presentation_root: None,
            inactive_surface_buffers: 0,
            discarded_on_selection_change: 0,
            presentation_switches: 0,
            shm_skipped: 0,
            pending: None,
            error: None,
            last_input: Input::new(0, 0, false, 0)?,
            imports: 0,
            source_commits: 0,
            source_cadence: crate::frame_timing::Cadence::default(),
            superseded: 0,
            keyboard_events: 0,
            pointer_events: 0,
            width,
            height,
            pointer_width: width,
            pointer_height: height,
        })
    }
    pub fn input(&mut self, input: Input, time: u32) {
        self.apply_input(input, time, true);
    }
    pub fn desktop_input(
        &mut self,
        snapshot: legion_display_channel::desktop_input::DesktopInput,
        time: u32,
    ) {
        // The decoded packet bounds both dimensions to small positive values.
        self.pointer_width = snapshot.width as i32;
        self.pointer_height = snapshot.height as i32;
        self.apply_input(snapshot.input, time, false);
    }
    pub(crate) fn focus_window(&mut self, focus: crate::xwayland::Focus) {
        use smithay::wayland::seat::WaylandFocus;
        let mut changed = false;
        if let Some(root) = focus.wl_surface().map(|surface| surface.into_owned())
            && self.presentation_root.as_ref() != Some(&root)
        {
            // A window switch happens on mapping/focus, never because an older splash or helper happened to repaint after the selected window.
            self.presentation_root = Some(root);
            changed = true;
            self.presentation_switches += 1;
            if self.pending.is_some() {
                self.discarded_on_selection_change += 1;
                self.discard_pending();
            }
        }
        self.ordered_input.selected = Some(focus.clone());
        if let Some(keyboard) = self.seat.get_keyboard()
            && self.ordered_input.host_keyboard != Some(false)
        {
            keyboard.set_focus(self, Some(focus), SERIAL_COUNTER.next_serial());
        }
        if changed {
            self.source_cadence.reset_clock();
            self.input_window_changed();
        }
    }
    pub(crate) fn clear_window_selection(&mut self) {
        self.source_cadence.reset_clock();
        self.presentation_root = None;
        self.ordered_input.selected = None;
        self.input_window_changed();
        if self.pending.is_some() {
            self.discarded_on_selection_change += 1;
            self.discard_pending();
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
    }
    fn accepts_root(&self, root: &WlSurface) -> bool {
        self.presentation_root
            .as_ref()
            .is_none_or(|selected| selected == root)
    }
    fn discard_inactive(&mut self, buffer: WlBuffer, sync: Option<crate::syncobj::Release>) {
        self.source_commits += 1;
        self.inactive_surface_buffers += 1;
        self.discard_buffer(buffer, sync);
    }
    fn apply_input(&mut self, input: Input, time: u32, toggle: bool) {
        if input.generation() == self.last_input.generation() {
            return;
        }
        let focus = self
            .xdg
            .toplevel_surfaces()
            .last()
            .map(|s| crate::xwayland::Focus::Wayland(s.wl_surface().clone()))
            .or_else(|| {
                self.x11_windows
                    .iter()
                    .rev()
                    .find(|w| w.is_mapped() && !w.is_override_redirect())
                    .cloned()
                    .map(crate::xwayland::Focus::X11)
            });
        if let Some(focus) = focus {
            use smithay::wayland::seat::WaylandFocus;
            let Some(surface) = focus.wl_surface().map(|s| s.into_owned()) else {
                return;
            };
            let serial = SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(focus), serial);
                if input.space() != self.last_input.space() {
                    // Legacy color probes encode toggles.
                    let states = if toggle {
                        [KeyState::Pressed, KeyState::Released]
                    } else {
                        [if input.space() {
                            KeyState::Pressed
                        } else {
                            KeyState::Released
                        }; 2]
                    };
                    for &key_state in &states[..if toggle { 2 } else { 1 }] {
                        keyboard.input::<(), _>(
                            self,
                            65_u32.into(),
                            key_state,
                            SERIAL_COUNTER.next_serial(),
                            time,
                            |_, _, _| FilterResult::Forward,
                        );
                        self.keyboard_events += 1;
                    }
                }
            }
            if let Some(pointer) = self.seat.get_pointer() {
                pointer.motion(
                    self,
                    Some((surface, (0.0, 0.0).into())),
                    &MotionEvent {
                        location: (
                            f64::from(input.x()) / 65535.0 * f64::from(self.pointer_width),
                            f64::from(input.y()) / 65535.0 * f64::from(self.pointer_height),
                        )
                            .into(),
                        serial: SERIAL_COUNTER.next_serial(),
                        time,
                    },
                );
                pointer.frame(self);
                self.pointer_events += 1;
            }
            self.last_input = input;
        }
    }
    fn accept_buffer(
        &mut self,
        buffer: WlBuffer,
        surface: Option<WlSurface>,
        sync: Option<crate::syncobj::Release>,
    ) {
        self.source_commits += 1;
        if self.gpu_only && smithay::wayland::dmabuf::get_dmabuf(&buffer).is_err() {
            if self.diagnose_surfaces && self.shm_skipped < 32 {
                // Capture bounded software UI only in explicit diagnostic runs.
                // Wine's error dialogs may be SHM even when game frames use DMA-BUF.
                let _ = smithay::wayland::shm::with_buffer_contents(
                    &buffer,
                    |pointer, len, data| {
                        use std::io::Write;
                        let (Ok(width), Ok(height), Ok(stride), Ok(offset)) = (
                            usize::try_from(data.width),
                            usize::try_from(data.height),
                            usize::try_from(data.stride),
                            usize::try_from(data.offset),
                        ) else {
                            return;
                        };
                        if width == 0 || height == 0 || width > 4096 || height > 2160
                        || stride < width * 4 || offset.checked_add(stride * height).is_none_or(|end| end > len)
                        || !matches!(data.format, smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888
                            | smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888)
                    { return; }
                        // SAFETY: Smithay keeps this SHM mapping live for the closure;
                        // the checked extent bounds every row read below.
                        let bytes = unsafe { std::slice::from_raw_parts(pointer, len) };
                        let path = format!("/frames/diagnostic-shm-{}.ppm", self.shm_skipped);
                        if let Ok(mut file) = std::fs::File::create(path) {
                            let _ = write!(file, "P6\n{width} {height}\n255\n");
                            for y in 0..height {
                                let row =
                                    &bytes[offset + y * stride..offset + y * stride + width * 4];
                                let rgb: Vec<u8> = row
                                    .as_chunks::<4>()
                                    .0
                                    .iter()
                                    .flat_map(|p| [p[2], p[1], p[0]])
                                    .collect();
                                let _ = file.write_all(&rgb);
                            }
                        }
                    },
                );
            }
            self.shm_skipped += 1;
            self.discard_buffer(buffer, sync);
            return;
        }
        if self.diagnose_surfaces {
            self.last_source_surface = surface;
            self.source_cadence.record(std::time::Instant::now());
        }
        if let Some(old) = self.pending.replace(buffer) {
            self.superseded += 1;
            let old_sync = self.pending_sync.take();
            self.discard_buffer(old, old_sync);
        }
        self.pending_sync = sync;
    }
    pub fn surface_diagnostics(&self) -> serde_json::Value {
        let roots = self.xdg.toplevel_surfaces();
        let mut trees = Vec::new();
        for root in roots
            .iter()
            .map(|s| s.wl_surface().clone())
            .chain(self.x11_windows.iter().filter_map(|s| s.wl_surface()))
        {
            let mut ids = Vec::new();
            with_surface_tree_downward(
                &root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, _, _| ids.push(format!("{:?}", surface.id())),
                |_, _, _| true,
            );
            trees.push(serde_json::json!({"root":format!("{:?}",root.id()),"surfaces":ids}));
        }
        serde_json::json!({
            "last_source":self.last_source_surface.as_ref().map(|s|format!("{:?}",s.id())),
            "presentation_root":self.presentation_root.as_ref().map(|s|format!("{:?}",s.id())),
            "keyboard_focus":self.seat.get_keyboard().and_then(|k|k.current_focus()).and_then(|focus|
                smithay::wayland::seat::WaylandFocus::wl_surface(&focus).map(|s|format!("{:?}",s.id()))),
            "trees":trees,
            "wayland_windows":roots.iter().map(|s|with_states(s.wl_surface(),|state| {
                state.data_map.get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|data|data.lock().ok()).map(|data|serde_json::json!({
                        "surface":format!("{:?}",s.wl_surface().id()),
                        "title":data.title,"app_id":data.app_id
                    }))
            })).collect::<Vec<_>>(),
            "x11_windows":self.x11_windows.iter().map(|s|serde_json::json!({
                "window":s.window_id(),"title":s.title(),"class":s.class(),"mapped":s.is_mapped(),
                "override_redirect":s.is_override_redirect(),
                "geometry": {"x":s.geometry().loc.x,"y":s.geometry().loc.y,
                    "width":s.geometry().size.w,"height":s.geometry().size.h},
                "surface":s.wl_surface().map(|s|format!("{:?}",s.id()))
            })).collect::<Vec<_>>()
        })
    }
    pub fn send_callbacks(&self, time: u32) {
        let toplevels = self.xdg.toplevel_surfaces();
        let roots = toplevels
            .iter()
            .map(|s| s.wl_surface().clone())
            .chain(self.x11_windows.iter().filter_map(|w| w.wl_surface()));
        for surface in roots {
            // Wine places Vulkan content in a child of its Win32 toplevel.
            with_surface_tree_downward(
                &surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |_, states, _| {
                    for callback in states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .drain(..)
                    {
                        callback.done(time);
                    }
                },
                |_, _, _| true,
            );
        }
    }
}
delegate_viewporter!(State);
impl CompositorHandler for State {
    fn new_surface(&mut self, surface: &WlSurface) {
        if self.syncobj.is_some() {
            smithay::wayland::compositor::add_pre_commit_hook::<Self, _>(
                surface,
                |state, _, surface| {
                    if state.acquire_watches.len() >= 1024 {
                        state.error = Some("too many pending acquire timelines".into());
                        return;
                    }
                    match crate::syncobj::Watch::pending(surface) {
                        Ok(Some(watch)) => state.acquire_watches.push(watch),
                        Ok(None) => {}
                        Err(error) => state.error = Some(error.to_string()),
                    }
                },
            );
        }
    }

    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &data.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("owned listener installs client state")
            .compositor
    }
    fn commit(&mut self, surface: &WlSurface) {
        if matches!(&self.cursor_image, smithay::input::pointer::CursorImageStatus::Surface(cursor) if cursor == surface)
        {
            self.cursor_generation = self.cursor_generation.wrapping_add(1);
            return;
        }
        if self.gpu_only {
            let diagnose = self.diagnose_surfaces;
            let explicit_sync = self.syncobj.is_some();
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            // An unmapped surface may become a cursor after its first commit.
            // Retain that buffer until the client assigns a role.
            if smithay::wayland::compositor::get_role(&root).is_none() {
                return;
            }
            let selected = self.accepts_root(&root);
            let mut buffers = Vec::new();
            // A synchronized child's buffer becomes current on parent commit.
            with_surface_tree_downward(
                &root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, states, _| {
                    if let Some(BufferAssignment::NewBuffer(buffer)) = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer
                        .take()
                    {
                        let sync = if explicit_sync {
                            crate::syncobj::take_release(states)
                        } else {
                            Ok(None)
                        };
                        buffers.push((buffer, diagnose.then(|| surface.clone()), sync));
                    }
                },
                |_, _, _| true,
            );
            for (buffer, surface, sync) in buffers {
                match sync {
                    Ok(sync) if selected => self.accept_buffer(buffer, surface, sync),
                    Ok(sync) => self.discard_inactive(buffer, sync),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            return;
        }
        let buffer = with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer
                .take()
        });
        if let Some(BufferAssignment::NewBuffer(buffer)) = buffer {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if self.accepts_root(&root) {
                self.accept_buffer(
                    buffer,
                    self.diagnose_surfaces.then(|| surface.clone()),
                    None,
                );
            } else {
                self.discard_inactive(buffer, None);
            }
        }
    }
}
impl BufferHandler for State {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        if self.renderer.is_none() || self.track_buffers {
            self.destroyed_buffers.push(buffer.clone());
        }
    }
}
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }
    fn dmabuf_imported(&mut self, _: &DmabufGlobal, dma: Dmabuf, notifier: ImportNotifier) {
        let Some(renderer) = self.renderer.as_mut() else {
            self.pending_imports.push((dma, notifier));
            return;
        };
        match renderer.import_dmabuf(&dma, None) {
            Ok(_) => {
                self.imports += 1;
                let _ = notifier.successful::<Self>();
            }
            Err(error) => {
                self.error = Some(error.to_string());
                notifier.failed();
            }
        }
    }
}
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self._output.enter(surface.wl_surface());
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.size = Some((self.width, self.height).into());
        });
        surface.send_configure();
        self.focus_window(crate::xwayland::Focus::Wayland(
            surface.wl_surface().clone(),
        ));
    }
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        let _ = surface.send_configure();
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if self.presentation_root.as_ref() != Some(surface.wl_surface()) {
            return;
        }
        let replacement = self
            .xdg
            .toplevel_surfaces()
            .iter()
            .rev()
            .find(|candidate| candidate.wl_surface() != surface.wl_surface())
            .map(|candidate| candidate.wl_surface().clone());
        if let Some(replacement) = replacement {
            self.focus_window(crate::xwayland::Focus::Wayland(replacement));
        } else {
            self.clear_window_selection();
        }
    }
    fn grab(&mut self, _: PopupSurface, _: WlSeat, _: Serial) {}
    fn reposition_request(&mut self, surface: PopupSurface, _: PositionerState, token: u32) {
        surface.send_repositioned(token);
    }
}
impl SeatHandler for State {
    type KeyboardFocus = crate::xwayland::Focus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }
    fn cursor_image(&mut self, _: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.ordered_input.cursor_visible = Some(!matches!(
            image,
            smithay::input::pointer::CursorImageStatus::Hidden
        ));
        self.cursor_image = image;
        self.cursor_generation = self.cursor_generation.wrapping_add(1);
    }
}
impl OutputHandler for State {}
impl SelectionHandler for State {
    type SelectionUserData = ();
}
impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data
    }
}
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {
    fn send(&mut self, _: String, _: OwnedFd, _: Seat<Self>) {}
}
#[derive(Default)]
pub struct ClientState {
    compositor: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}
delegate_compositor!(State);
delegate_shm!(State);
delegate_dmabuf!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_data_device!(State);

impl smithay::wayland::drm_syncobj::DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut smithay::wayland::drm_syncobj::DrmSyncobjState> {
        self.syncobj.as_mut()
    }
}
smithay::delegate_drm_syncobj!(State);
