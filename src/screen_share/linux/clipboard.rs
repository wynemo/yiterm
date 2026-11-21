use anyhow::Result;
use x11rb::{
    connection::Connection,
    protocol::{Event, xproto::*},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
use zeroize::Zeroizing;

x11rb::atom_manager! {
    ClipboardAtoms: ClipboardAtomCookies {
        CLIPBOARD,
        TARGETS,
        UTF8_STRING,
        TEXT_UTF8: b"text/plain;charset=utf-8",
    }
}

pub(super) struct Clipboard {
    window: Window,
    atoms: ClipboardAtoms,
    text: Option<Zeroizing<String>>,
}

impl Clipboard {
    pub(super) fn new(connection: &RustConnection, root: Window) -> Result<Self> {
        let window = connection.generate_id()?;
        let atoms = ClipboardAtoms::new(connection)?;
        connection
            .create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )?
            .check()?;
        Ok(Self {
            window,
            atoms: atoms.reply()?,
            text: None,
        })
    }

    pub(super) fn offer(&mut self, connection: &RustConnection, text: String) -> Result<bool> {
        let text = Zeroizing::new(text);
        connection
            .set_selection_owner(self.window, self.atoms.CLIPBOARD, x11rb::CURRENT_TIME)?
            .check()?;
        let owns_selection = self.owns_selection(connection)?;
        self.text = owns_selection.then_some(text);
        Ok(owns_selection)
    }

    pub(super) fn has_text(&self) -> bool {
        self.text.is_some()
    }

    fn owns_selection(&self, connection: &RustConnection) -> Result<bool> {
        Ok(connection
            .get_selection_owner(self.atoms.CLIPBOARD)?
            .reply()?
            .owner
            == self.window)
    }

    pub(super) fn dispatch(
        &mut self,
        connection: &RustConnection,
        focus: Option<Window>,
    ) -> Result<bool> {
        let client_bits = !connection.setup().resource_id_mask;
        let mut delivered = false;
        for _ in 0..32 {
            match connection.poll_for_event()? {
                Some(Event::SelectionRequest(request)) if request.owner == self.window => {
                    let text_sent = self.respond(connection, request)?;
                    delivered |= text_sent
                        && focus.is_some_and(|window| {
                            // Toolkits often use a separate window for selection requests.
                            window & client_bits == request.requestor & client_bits
                        });
                }
                Some(Event::SelectionClear(event))
                    if event.owner == self.window && event.selection == self.atoms.CLIPBOARD =>
                {
                    if !self.owns_selection(connection)? {
                        self.text = None;
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        connection.flush()?;
        Ok(delivered)
    }

    fn respond(&self, connection: &RustConnection, request: SelectionRequestEvent) -> Result<bool> {
        let property = if request.property == x11rb::NONE {
            request.target
        } else {
            request.property
        };
        let mut text_sent = false;
        let mut converted = false;
        if let Some(text) = &self.text
            && request.selection == self.atoms.CLIPBOARD
        {
            if request.target == self.atoms.TARGETS {
                converted = connection
                    .change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::ATOM,
                        &[
                            self.atoms.TARGETS,
                            self.atoms.UTF8_STRING,
                            self.atoms.TEXT_UTF8,
                        ],
                    )?
                    .check()
                    .is_ok();
            } else if [self.atoms.UTF8_STRING, self.atoms.TEXT_UTF8].contains(&request.target) {
                converted = true;
                // Stay below the minimum X11 request size even without BIG-REQUESTS.
                for (index, bytes) in text.as_bytes().chunks(16_000).enumerate() {
                    let mode = if index == 0 {
                        PropMode::REPLACE
                    } else {
                        PropMode::APPEND
                    };
                    if connection
                        .change_property8(mode, request.requestor, property, request.target, bytes)?
                        .check()
                        .is_err()
                    {
                        converted = false;
                        break;
                    }
                }
                text_sent = converted;
            }
        }
        connection
            .send_event(
                false,
                request.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: request.time,
                    requestor: request.requestor,
                    selection: request.selection,
                    target: request.target,
                    property: if converted { property } else { x11rb::NONE },
                },
            )?
            .ignore_error();
        Ok(text_sent)
    }
}
