pub(crate) mod x11 {
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail, ensure};
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xfixes::{self, SelectionEventMask};
    use x11rb::protocol::xproto::{
        Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, GetPropertyReply, Property,
        Timestamp, Window, WindowClass,
    };
    use x11rb::rust_connection::RustConnection;
    use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

    x11rb::atom_manager! {
        Atoms: AtomsCookie {
            CLIPBOARD,
            INCR,
            TARGETS,
            PROPERTY: b"WAYLAND_CLIPBOARD_PROPERTY",
        }
    }

    #[derive(Clone, Copy)]
    pub(crate) struct OwnerToken {
        window: Window,
        timestamp: Timestamp,
    }

    impl OwnerToken {
        #[cfg(test)]
        pub(crate) fn new(window: u32, timestamp: u32) -> Self {
            Self { window, timestamp }
        }

        pub(crate) fn id(self) -> u32 {
            self.window
        }
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    pub(crate) struct Target(Atom);

    pub(crate) struct Clipboard {
        connection: RustConnection,
        window: Window,
        atoms: Atoms,
    }

    impl Clipboard {
        pub(crate) fn new(display: &str) -> Result<Self> {
            let (connection, screen) = x11rb::connect(Some(display))?;
            let screen = &connection.setup().roots[screen];
            let window = connection.generate_id()?;
            connection
                .create_window(
                    COPY_DEPTH_FROM_PARENT,
                    window,
                    screen.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    screen.root_visual,
                    &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )?
                .check()?;

            let atoms = Atoms::new(&connection)?.reply()?;
            Ok(Self {
                connection,
                window,
                atoms,
            })
        }

        pub(crate) fn target(&self, name: &str) -> Result<Target> {
            Ok(Target(
                self.connection
                    .intern_atom(false, name.as_bytes())?
                    .reply()?
                    .atom,
            ))
        }

        pub(crate) fn targets(
            &mut self,
            max_bytes: usize,
            timeout: Duration,
            check: impl FnMut() -> Result<()>,
        ) -> Result<Vec<Target>> {
            let data = self
                .read_inner(
                    self.atoms.TARGETS,
                    AtomEnum::ATOM.into(),
                    32,
                    max_bytes,
                    timeout,
                    check,
                )?
                .context("selection owner refused TARGETS")?;
            ensure!(
                data.len().is_multiple_of(4),
                "invalid TARGETS property length"
            );
            Ok(data
                .chunks_exact(4)
                .map(|bytes| Target(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])))
                .collect())
        }

        pub(crate) fn read(
            &mut self,
            target: Target,
            max_bytes: usize,
            timeout: Duration,
            check: impl FnMut() -> Result<()>,
        ) -> Result<Option<Vec<u8>>> {
            self.read_inner(target.0, target.0, 8, max_bytes, timeout, check)
        }

        fn read_inner(
            &mut self,
            target: Atom,
            expected_type: Atom,
            expected_format: u8,
            max_bytes: usize,
            timeout: Duration,
            mut check: impl FnMut() -> Result<()>,
        ) -> Result<Option<Vec<u8>>> {
            check()?;
            self.connection
                .convert_selection(
                    self.window,
                    self.atoms.CLIPBOARD,
                    target,
                    self.atoms.PROPERTY,
                    CURRENT_TIME,
                )?
                .check()?;
            self.connection.flush()?;

            let deadline = Instant::now() + timeout;
            let mut data = Vec::new();
            let mut incremental = false;

            loop {
                if let Err(error) = check() {
                    self.delete_property();
                    return Err(error);
                }
                if Instant::now() >= deadline {
                    self.delete_property();
                    bail!("selection timed out after {} seconds", timeout.as_secs());
                }

                let Some(event) = self.connection.poll_for_event()? else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };

                match event {
                    Event::SelectionNotify(event) => {
                        if event.requestor != self.window
                            || event.selection != self.atoms.CLIPBOARD
                            || event.target != target
                        {
                            continue;
                        }
                        if event.property == x11rb::NONE {
                            return Ok(None);
                        }
                        let reply = self.read_property(true, event.property, max_bytes)?;
                        if reply.type_ == self.atoms.INCR {
                            if let Some(size) = reply.value32().and_then(|mut values| values.next())
                            {
                                let size = size as usize;
                                ensure!(
                                    size <= max_bytes,
                                    "selection exceeds {max_bytes} bytes: {size} bytes requested by INCR"
                                );
                                data.try_reserve(size)
                                    .context("failed to reserve incremental selection buffer")?;
                            }
                            self.connection.flush()?;
                            incremental = true;
                        } else {
                            ensure!(
                                reply.type_ == expected_type && reply.format == expected_format,
                                "unexpected selection type or format"
                            );
                            return Ok(Some(reply.value));
                        }
                    }
                    Event::PropertyNotify(event) => {
                        if !incremental
                            || event.window != self.window
                            || event.atom != self.atoms.PROPERTY
                            || event.state != Property::NEW_VALUE
                        {
                            continue;
                        }
                        let reply = self.read_property(true, self.atoms.PROPERTY, max_bytes)?;
                        ensure!(
                            reply.type_ == expected_type && reply.format == expected_format,
                            "unexpected INCR selection type or format"
                        );
                        if reply.value.is_empty() {
                            return Ok(Some(data));
                        }
                        ensure!(
                            data.len().saturating_add(reply.value.len()) <= max_bytes,
                            "selection exceeds {max_bytes} bytes"
                        );
                        data.extend_from_slice(&reply.value);
                    }
                    _ => {}
                }
            }
        }

        fn read_property(
            &self,
            delete: bool,
            property: Atom,
            max_bytes: usize,
        ) -> Result<GetPropertyReply> {
            let reply = self
                .connection
                .get_property(
                    delete,
                    self.window,
                    property,
                    AtomEnum::NONE,
                    0,
                    u32::try_from(max_bytes / 4).unwrap_or(u32::MAX),
                )?
                .reply()?;
            let size = reply.value.len();
            ensure!(
                reply.bytes_after == 0 && size <= max_bytes,
                "selection exceeds {max_bytes} bytes: {size} bytes"
            );
            Ok(reply)
        }

        fn delete_property(&self) {
            if let Ok(cookie) = self
                .connection
                .delete_property(self.window, self.atoms.PROPERTY)
            {
                let _ = cookie.check();
            }
            let _ = self.connection.flush();
        }

        pub(crate) fn clear_if_owner(&self, owner: OwnerToken) -> Result<bool> {
            let current = self
                .connection
                .get_selection_owner(self.atoms.CLIPBOARD)?
                .reply()?
                .owner;
            if current != owner.window {
                return Ok(false);
            }
            self.connection
                .set_selection_owner(x11rb::NONE, self.atoms.CLIPBOARD, owner.timestamp)?
                .check()?;
            Ok(self
                .connection
                .get_selection_owner(self.atoms.CLIPBOARD)?
                .reply()?
                .owner
                == x11rb::NONE)
        }

        pub(crate) fn is_empty(&self) -> Result<bool> {
            Ok(self
                .connection
                .get_selection_owner(self.atoms.CLIPBOARD)?
                .reply()?
                .owner
                == x11rb::NONE)
        }
    }

    pub(crate) struct Watcher {
        connection: RustConnection,
        clipboard: Atom,
    }

    impl Watcher {
        pub(crate) fn new(display: &str) -> Result<Self> {
            let (connection, screen) = x11rb::connect(Some(display))?;
            let root = connection.setup().roots[screen].root;
            let clipboard = Atoms::new(&connection)?.reply()?.CLIPBOARD;
            xfixes::query_version(&connection, 5, 0)?.reply()?;
            xfixes::select_selection_input(
                &connection,
                root,
                clipboard,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )?
            .check()?;
            connection.flush()?;
            Ok(Self {
                connection,
                clipboard,
            })
        }

        pub(crate) fn run(self, mut on_owner: impl FnMut(OwnerToken)) -> Result<()> {
            loop {
                if let Event::XfixesSelectionNotify(event) = self.connection.wait_for_event()?
                    && event.selection == self.clipboard
                    && event.owner != x11rb::NONE
                {
                    on_owner(OwnerToken {
                        window: event.owner,
                        timestamp: event.selection_timestamp,
                    });
                }
            }
        }
    }
}

pub(crate) mod wayland {
    use std::collections::HashMap;

    use anyhow::{Context, Result, ensure};
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_registry::WlRegistry;
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::ext::data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    };
    use wayland_protocols_wlr::data_control::v1::client::{
        zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
        zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    };
    use wl_clipboard_rs::copy::Options;
    pub(crate) use wl_clipboard_rs::copy::{MimeSource, MimeType, Source};

    pub(crate) fn copy(sources: Vec<MimeSource>) -> Result<()> {
        Options::new()
            .copy_multi(sources)
            .context("failed to publish clipboard to Wayland")
    }

    #[derive(Clone, PartialEq, Eq, Hash)]
    enum Offer {
        Ext(ExtDataControlOfferV1),
        Wlr(ZwlrDataControlOfferV1),
    }

    struct State {
        on_selection: Box<dyn FnMut(Vec<String>) + Send>,
        watching: bool,
        offers: HashMap<Offer, Vec<String>>,
        _seats: Vec<WlSeat>,
        _ext_manager: Option<ExtDataControlManagerV1>,
        _wlr_manager: Option<ZwlrDataControlManagerV1>,
        _ext_devices: Vec<ExtDataControlDeviceV1>,
        _wlr_devices: Vec<ZwlrDataControlDeviceV1>,
    }

    impl State {
        fn selected(&mut self, offer: Option<Offer>) {
            let mime_types = offer
                .as_ref()
                .and_then(|offer| self.offers.remove(offer))
                .unwrap_or_default();
            if self.watching {
                (self.on_selection)(mime_types);
            }
        }
    }

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _state: &mut Self,
            _proxy: &WlRegistry,
            _event: <WlRegistry as Proxy>::Event,
            _data: &GlobalListContents,
            _connection: &Connection,
            _queue: &QueueHandle<Self>,
        ) {
        }
    }

    wayland_client::delegate_noop!(State: ignore WlSeat);
    wayland_client::delegate_noop!(State: ignore ExtDataControlManagerV1);
    wayland_client::delegate_noop!(State: ignore ZwlrDataControlManagerV1);

    impl Dispatch<ExtDataControlOfferV1, ()> for State {
        fn event(
            state: &mut Self,
            offer: &ExtDataControlOfferV1,
            event: ext_data_control_offer_v1::Event,
            _data: &(),
            _connection: &Connection,
            _queue: &QueueHandle<Self>,
        ) {
            if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
                state
                    .offers
                    .entry(Offer::Ext(offer.clone()))
                    .or_default()
                    .push(mime_type);
            }
        }
    }

    impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
        fn event(
            state: &mut Self,
            offer: &ZwlrDataControlOfferV1,
            event: zwlr_data_control_offer_v1::Event,
            _data: &(),
            _connection: &Connection,
            _queue: &QueueHandle<Self>,
        ) {
            if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
                state
                    .offers
                    .entry(Offer::Wlr(offer.clone()))
                    .or_default()
                    .push(mime_type);
            }
        }
    }

    impl Dispatch<ExtDataControlDeviceV1, ()> for State {
        fn event(
            state: &mut Self,
            _device: &ExtDataControlDeviceV1,
            event: ext_data_control_device_v1::Event,
            _data: &(),
            _connection: &Connection,
            _queue: &QueueHandle<Self>,
        ) {
            if let ext_data_control_device_v1::Event::Selection { id } = event {
                state.selected(id.map(Offer::Ext));
            }
        }

        wayland_client::event_created_child!(State, ExtDataControlDeviceV1, [
            ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ())
        ]);
    }

    impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
        fn event(
            state: &mut Self,
            _device: &ZwlrDataControlDeviceV1,
            event: zwlr_data_control_device_v1::Event,
            _data: &(),
            _connection: &Connection,
            _queue: &QueueHandle<Self>,
        ) {
            if let zwlr_data_control_device_v1::Event::Selection { id } = event {
                state.selected(id.map(Offer::Wlr));
            }
        }

        wayland_client::event_created_child!(State, ZwlrDataControlDeviceV1, [
            zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ())
        ]);
    }

    pub(crate) struct Watcher {
        queue: EventQueue<State>,
        state: State,
    }

    impl Watcher {
        pub(crate) fn new(on_selection: impl FnMut(Vec<String>) + Send + 'static) -> Result<Self> {
            let connection = Connection::connect_to_env()
                .context("failed to connect Wayland clipboard watcher")?;
            let (globals, mut queue) = registry_queue_init::<State>(&connection)
                .context("failed to read Wayland globals")?;
            let queue_handle = queue.handle();
            let registry = globals.registry();
            let seats = globals.contents().with_list(|globals| {
                globals
                    .iter()
                    .filter(|global| {
                        global.interface == WlSeat::interface().name && global.version >= 1
                    })
                    .map(|global| {
                        registry.bind(global.name, global.version.min(2), &queue_handle, ())
                    })
                    .collect::<Vec<_>>()
            });
            ensure!(!seats.is_empty(), "Wayland compositor has no seats");

            let ext_manager = globals
                .bind::<ExtDataControlManagerV1, _, _>(&queue_handle, 1..=1, ())
                .ok();
            let wlr_manager = if ext_manager.is_none() {
                globals
                    .bind::<ZwlrDataControlManagerV1, _, _>(&queue_handle, 1..=1, ())
                    .ok()
            } else {
                None
            };
            ensure!(
                ext_manager.is_some() || wlr_manager.is_some(),
                "Wayland compositor supports neither ext-data-control nor wlr-data-control"
            );

            let ext_devices = ext_manager
                .iter()
                .flat_map(|manager| {
                    seats
                        .iter()
                        .map(|seat| manager.get_data_device(seat, &queue_handle, ()))
                })
                .collect();
            let wlr_devices = wlr_manager
                .iter()
                .flat_map(|manager| {
                    seats
                        .iter()
                        .map(|seat| manager.get_data_device(seat, &queue_handle, ()))
                })
                .collect();
            let mut state = State {
                on_selection: Box::new(on_selection),
                watching: false,
                offers: HashMap::new(),
                _seats: seats,
                _ext_manager: ext_manager,
                _wlr_manager: wlr_manager,
                _ext_devices: ext_devices,
                _wlr_devices: wlr_devices,
            };
            queue
                .roundtrip(&mut state)
                .context("failed to initialize Wayland clipboard watcher")?;
            state.watching = true;
            Ok(Self { queue, state })
        }

        pub(crate) fn run(mut self) -> Result<()> {
            loop {
                self.queue
                    .blocking_dispatch(&mut self.state)
                    .context("Wayland clipboard watcher disconnected")?;
            }
        }
    }
}
