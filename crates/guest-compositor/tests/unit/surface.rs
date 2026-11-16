//! Real private Wayland socket coverage; no desktop or GPU is opened.
use crate::state::{ClientState, State};
use smithay::{
    backend::allocator::{Format, Fourcc, Modifier},
    reexports::wayland_server::{Display, Resource},
};
use std::{
    os::fd::AsFd,
    os::unix::net::UnixStream,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat,
        wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
    },
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{zwp_locked_pointer_v1, zwp_pointer_constraints_v1},
    relative_pointer::zv1::client::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    callbacks: u8,
    count: usize,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    focused: Option<wl_surface::WlSurface>,
    pointer: Option<wl_pointer::WlPointer>,
    keys: Vec<(u32, u32)>,
    enters: Vec<Vec<u32>>,
    buttons: Vec<(u32, u32)>,
    motions: Vec<(f64, f64)>,
    axes: Vec<f64>,
    relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    relative_events: Vec<(f64, f64, f64, f64)>,
    locks: usize,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_subcompositor" => state.subcompositor = Some(registry.bind(name, 1, qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.wm = Some(registry.bind(name, version.min(3), qh, ())),
                "wl_seat" => {
                    registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ());
                }
                "zwp_relative_pointer_manager_v1" => {
                    state.relative_manager = Some(registry.bind(name, 1, qh, ()))
                }
                "zwp_pointer_constraints_v1" => {
                    state.constraints = Some(registry.bind(name, 1, qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event
            && let wayland_client::WEnum::Value(capabilities) = capabilities
        {
            if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, keys, .. } => {
                state.focused = Some(surface);
                state.enters.push(
                    keys.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|v| u32::from_ne_bytes(*v))
                        .collect(),
                );
            }
            wl_keyboard::Event::Leave { .. } => state.focused = None,
            wl_keyboard::Event::Key {
                key,
                state: wayland_client::WEnum::Value(value),
                ..
            } => {
                state.keys.push((key, value as u32));
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.motions.push((surface_x, surface_y)),
            wl_pointer::Event::Button {
                button,
                state: wayland_client::WEnum::Value(value),
                ..
            } => state.buttons.push((button, value as u32)),
            wl_pointer::Event::Axis { value, .. } => state.axes.push(value),
            _ => {}
        }
    }
}
impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            state.relative_events.push((dx, dy, dx_unaccel, dy_unaccel));
        }
    }
}
impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_locked_pointer_v1::Event::Locked = event {
            state.locks += 1;
        }
    }
}
delegate_noop!(Client:ignore zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
delegate_noop!(Client:ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
impl Dispatch<wl_callback::WlCallback, u8> for Client {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        tag: &u8,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.callbacks |= *tag;
            state.count += 1;
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        _: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}
delegate_noop!(Client:ignore wl_compositor::WlCompositor);
delegate_noop!(Client:ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Client:ignore wl_subsurface::WlSubsurface);
delegate_noop!(Client:ignore wl_surface::WlSurface);
delegate_noop!(Client:ignore wl_shm::WlShm);
delegate_noop!(Client:ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client:ignore wl_buffer::WlBuffer);
delegate_noop!(Client:ignore xdg_toplevel::XdgToplevel);

fn client(socket: UnixStream) -> anyhow::Result<(u32, Connection)> {
    let connection = Connection::from_socket(socket)?;
    let mut queue = connection.new_event_queue::<Client>();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut state = Client::default();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let compositor = state
        .compositor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no compositor"))?;
    let subs = state
        .subcompositor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no subcompositor"))?;
    let older = compositor.create_surface(&qh, ());
    let older_shell = state
        .wm
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no xdg-shell"))?
        .get_xdg_surface(&older, &qh, ());
    let _older_top = older_shell.get_toplevel(&qh, ());
    older.commit();
    let root = compositor.create_surface(&qh, ());
    let child = compositor.create_surface(&qh, ());
    let grandchild = compositor.create_surface(&qh, ());
    let shell = state
        .wm
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no xdg-shell"))?
        .get_xdg_surface(&root, &qh, ());
    let _top = shell.get_toplevel(&qh, ());
    root.commit();
    let _sub = subs.get_subsurface(&child, &root, &qh, ());
    let _grand = subs.get_subsurface(&grandchild, &child, &qh, ());
    let file = tempfile::tempfile()?;
    file.set_len(4 * 64 * 64 * 4)?;
    let pool = state
        .shm
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no SHM"))?
        .create_pool(file.as_fd(), 4 * 64 * 64 * 4, &qh, ());
    let mut selected_buffer = 0;
    for (index, surface) in [&root, &child, &grandchild].into_iter().enumerate() {
        let buffer = pool.create_buffer(
            index as i32 * 64 * 64 * 4,
            64,
            64,
            64 * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        if index == 0 {
            selected_buffer = buffer.id().protocol_id();
        }
        surface.attach(Some(&buffer), 0, 0);
        surface.frame(&qh, 1_u8 << index);
    }
    grandchild.commit();
    child.commit();
    root.commit();
    // The older splash repaints after the game's complete surface tree.
    let old_buffer = pool.create_buffer(
        3 * 64 * 64 * 4,
        64,
        64,
        64 * 4,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );
    older.attach(Some(&old_buffer), 0, 0);
    older.frame(&qh, 16);
    older.commit();
    queue.roundtrip(&mut state)?;
    while state.callbacks != 23 {
        queue.blocking_dispatch(&mut state)?;
    }
    queue.roundtrip(&mut state)?;
    anyhow::ensure!(state.count == 4, "callback delivered more than once");
    anyhow::ensure!(
        state.focused.as_ref() == Some(&root),
        "new toplevel did not receive keyboard focus before host input"
    );
    // Drain several idle server iterations, then request a new callback without a new buffer.
    for _ in 0..4 {
        queue.roundtrip(&mut state)?;
    }
    root.frame(&qh, 8);
    queue.roundtrip(&mut state)?;
    anyhow::ensure!(state.count == 4, "uncommitted callback completed early");
    root.commit();
    while state.callbacks != 31 {
        queue.blocking_dispatch(&mut state)?;
    }
    queue.roundtrip(&mut state)?;
    anyhow::ensure!(
        state.count == 5,
        "callback after idle missing or duplicated"
    );
    Ok((selected_buffer, connection))
}

#[test]
#[cfg_attr(
    miri,
    ignore = "real private Wayland sockets, XKB and SHM descriptors require the native test"
)]
fn synchronized_children_receive_callbacks_and_placeholder_buffers_do_not_enter_gpu_mailbox() {
    exercise_window_selection(true);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "real private Wayland sockets, XKB and SHM descriptors require the native test"
)]
fn inactive_splash_repaint_does_not_replace_selected_game_buffer() {
    exercise_window_selection(false);
}

fn exercise_window_selection(gpu_only: bool) {
    let mut display = Display::<State>::new().expect("private display");
    let mut handle = display.handle();
    let mut state = State::forwarding(
        &handle,
        vec![Format {
            code: Fourcc::Abgr8888,
            modifier: Modifier::Linear,
        }],
        1,
        64,
        64,
    )
    .expect("GPU-free state");
    state.gpu_only = gpu_only;
    let (server, socket) = UnixStream::pair().expect("private socket pair");
    let shutdown = socket.try_clone().expect("private shutdown handle");
    handle
        .insert_client(server, Arc::new(ClientState::default()))
        .expect("owned client");
    let (send, receive) = mpsc::sync_channel(1);
    let (finish, finished) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = client(socket).map_err(|e| e.to_string());
        // Retain the client connection until the server has inspected its mailbox; disconnect teardown must not race the selection assertion.
        let _ = send.send(
            result
                .as_ref()
                .map(|(buffer, _)| *buffer)
                .map_err(Clone::clone),
        );
        let _ = finished.recv_timeout(Duration::from_secs(5));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        display
            .dispatch_clients(&mut state)
            .expect("private dispatch");
        state.send_callbacks(1);
        display.flush_clients().expect("private flush");
        if let Ok(result) = receive.try_recv() {
            break result;
        }
        if Instant::now() > deadline {
            break Err("subsurface callback timeout".to_owned());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    use smithay::wayland::{
        compositor::{TraversalAction, with_surface_tree_downward},
        shell::xdg::XdgShellHandler,
    };
    let roots = state
        .xdg_shell_state()
        .toplevel_surfaces()
        .iter()
        .map(|surface| surface.wl_surface().clone())
        .collect::<Vec<_>>();
    let mut checked = 0;
    for root in roots {
        with_surface_tree_downward(
            &root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_, states, _| {
                assert!(
                    crate::syncobj::take_release(states)
                        .expect("locked tree timeline state")
                        .is_none()
                );
                checked += 1;
            },
            |_, _, _| true,
        );
    }
    assert!(checked >= 3);
    let _ = finish.send(());
    let _ = shutdown.shutdown(std::net::Shutdown::Both);
    worker.join().expect("owned client thread");
    let selected_buffer = result.expect("surface tree callbacks");
    assert_eq!(state.source_commits, 4);
    assert_eq!(state.inactive_surface_buffers, 1);
    assert_eq!(
        state.source_commits,
        state.superseded
            + state.shm_skipped
            + state.inactive_surface_buffers
            + state.discarded_on_selection_change
            + u64::from(state.pending.is_some())
    );
    if gpu_only {
        assert_eq!(state.shm_skipped, 3);
        assert!(state.pending.is_none());
    } else {
        assert_eq!(state.shm_skipped, 0);
        assert_eq!(
            state
                .pending
                .as_ref()
                .expect("selected game buffer")
                .id()
                .protocol_id(),
            selected_buffer
        );
    }
    state.clear_window_selection();
    assert!(state.pending.is_none());
    assert_eq!(
        state.source_commits,
        state.superseded
            + state.shm_skipped
            + state.inactive_surface_buffers
            + state.discarded_on_selection_change
    );
}

/// Exercise wire decoding all the way to a real client.
#[test]
#[cfg_attr(
    miri,
    ignore = "private Wayland sockets and XKB require native execution"
)]
fn ordered_input_delivers_short_presses_mouse_and_focus_restoration() {
    exercise_input(false);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "private Wayland sockets and XKB require native execution"
)]
fn relative_input_and_persistent_pointer_lock_survive_keyboard_refocus() {
    exercise_input(true);
}

fn exercise_input(locked: bool) {
    use legion_display_channel::input_events::{BYTES, Batch, Event, TimedEvent};
    let mut display = Display::<State>::new().expect("display");
    let mut handle = display.handle();
    let mut state = State::forwarding(
        &handle,
        vec![Format {
            code: Fourcc::Abgr8888,
            modifier: Modifier::Linear,
        }],
        1,
        1920,
        1080,
    )
    .expect("state");
    // Local forwarding states must advertise relative input themselves; the
    // KMS game path never gets a separate enable call.
    let (server, socket) = UnixStream::pair().expect("socket");
    let shutdown = socket.try_clone().expect("shutdown");
    handle
        .insert_client(server, Arc::new(ClientState::default()))
        .expect("client");
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (advance_tx, advance_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (finish_tx, finish_rx) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<_> {
            let connection = Connection::from_socket(socket)?;
            let mut queue = connection.new_event_queue::<Client>();
            let qh = queue.handle();
            connection.display().get_registry(&qh, ());
            let mut client = Client::default();
            queue.roundtrip(&mut client)?;
            queue.roundtrip(&mut client)?;
            let surface = client
                .compositor
                .as_ref()
                .expect("compositor")
                .create_surface(&qh, ());
            let shell = client
                .wm
                .as_ref()
                .expect("shell")
                .get_xdg_surface(&surface, &qh, ());
            let _top = shell.get_toplevel(&qh, ());
            surface.commit();
            queue.roundtrip(&mut client)?;
            let file = tempfile::tempfile()?;
            file.set_len(640 * 360 * 4)?;
            let pool =
                client
                    .shm
                    .as_ref()
                    .expect("shm")
                    .create_pool(file.as_fd(), 640 * 360 * 4, &qh, ());
            let buffer =
                pool.create_buffer(0, 640, 360, 640 * 4, wl_shm::Format::Argb8888, &qh, ());
            surface.attach(Some(&buffer), 0, 0);
            surface.commit();
            queue.roundtrip(&mut client)?;
            let pointer = client.pointer.as_ref().expect("pointer");
            let _relative = client
                .relative_manager
                .as_ref()
                .expect("relative input")
                .get_relative_pointer(pointer, &qh, ());
            let _lock = locked.then(|| {
                client
                    .constraints
                    .as_ref()
                    .expect("constraints")
                    .lock_pointer(
                        &surface,
                        pointer,
                        None,
                        zwp_pointer_constraints_v1::Lifetime::Persistent,
                        &qh,
                        (),
                    )
            });
            queue.roundtrip(&mut client)?;
            ready_tx.send(())?;
            advance_rx.recv_timeout(Duration::from_secs(5))?;
            queue.roundtrip(&mut client)?;
            // Keep the connection alive until the server has checked results.
            result_tx.send((
                client.keys,
                client.enters,
                client.buttons,
                client.motions,
                client.axes,
                client.relative_events,
                client.locks,
            ))?;
            let _ = finish_rx.recv_timeout(Duration::from_secs(5));
            Ok(())
        })();
        result.map_err(|e| e.to_string())
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut injected = false;
    let observed = loop {
        display.dispatch_clients(&mut state).expect("dispatch");
        if !injected && ready_rx.try_recv().is_ok() {
            let events = [
                Event::Focus {
                    keyboard: true,
                    focused: true,
                },
                Event::Absolute { x: 0.5, y: 0.25 },
                Event::Focus {
                    keyboard: false,
                    focused: true,
                },
                Event::Relative {
                    dx: 2.0,
                    dy: -3.0,
                    raw_dx: 4.0,
                    raw_dy: -6.0,
                },
                Event::Key {
                    code: 30,
                    down: true,
                },
                Event::Key {
                    code: 30,
                    down: false,
                },
                Event::Button {
                    code: 0x110,
                    down: true,
                },
                Event::Button {
                    code: 0x110,
                    down: false,
                },
                Event::Axis {
                    axis: 0,
                    value: -10.0,
                    v120: -120,
                    stop: false,
                    source: 0,
                },
                Event::Key {
                    code: 32,
                    down: true,
                },
                Event::Button {
                    code: 0x111,
                    down: true,
                },
                Event::Focus {
                    keyboard: true,
                    focused: false,
                },
                Event::KeyboardRestore,
                Event::HeldKey { code: 42 },
                Event::Focus {
                    keyboard: true,
                    focused: true,
                },
                Event::Key {
                    code: 42,
                    down: false,
                },
            ];
            let batch = Batch {
                width: 640,
                height: 360,
                events: events
                    .into_iter()
                    .enumerate()
                    .map(|(i, event)| TimedEvent {
                        time_us: i as u64 * 100,
                        event,
                    })
                    .collect(),
                ..Batch::default()
            };
            let mut bytes = [0; BYTES];
            batch.encode(&mut bytes).expect("encode");
            state.ordered_input_batch(Batch::decode(&bytes).expect("decode"), 1_000);
            assert_eq!(
                (state.width, state.height),
                (1920, 1080),
                "splash coordinates cannot resize the virtual output"
            );
            advance_tx.send(()).expect("advance");
            injected = true;
        }
        display.flush_clients().expect("flush");
        if let Ok(result) = result_rx.try_recv() {
            break result;
        }
        assert!(Instant::now() < deadline, "input client timed out");
        std::thread::sleep(Duration::from_millis(1));
    };
    let _ = finish_tx.send(());
    let _ = shutdown.shutdown(std::net::Shutdown::Both);
    worker.join().expect("worker").expect("client result");
    let (keys, enters, buttons, motions, axes, relative, locks) = observed;
    assert_eq!(keys, vec![(30, 1), (30, 0), (32, 1), (32, 0), (42, 0)]);
    assert!(
        enters.contains(&vec![42]),
        "held key must be present on enter without a ghost key-down: {enters:?}"
    );
    assert_eq!(
        buttons,
        vec![(0x110, 1), (0x110, 0), (0x111, 1), (0x111, 0)]
    );
    assert!(
        motions.contains(&(320.0, 90.0)),
        "scaled pointer coordinate: {motions:?}"
    );
    assert_eq!(axes, vec![-10.0]);
    assert_eq!(relative, vec![(2.0, -3.0, 4.0, -6.0)]);
    assert_eq!(
        locks,
        if locked { 2 } else { 0 },
        "persistent lock should reactivate after focus returns"
    );
}
