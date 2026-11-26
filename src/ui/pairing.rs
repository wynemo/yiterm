use super::*;
use crate::direct_share::pairing::{Pairing, Status};

impl TerminalUi {
    pub(super) fn pairing_panel(&self, cx: &mut ViewContext<Self>) -> Option<gpui::AnyElement> {
        let host = self.share_host.as_ref()?;
        if !host.pairing_visible {
            return None;
        }
        let theme = self.app.theme;
        let mut panel = div()
            .w(px(420.))
            .max_w_full()
            .p(px(24.))
            .rounded(px(12.))
            .bg(theme.background())
            .text_color(theme.foreground())
            .border_1()
            .border_color(theme.border())
            .flex()
            .flex_col()
            .items_center()
            .gap(px(12.))
            .child(div().text_size(px(20.)).child("Connect with Tenimon"));
        match host.pairing.status() {
            Status::Starting => panel = panel.child("Preparing encrypted connection…"),
            Status::Failed => panel = panel.child("Connection code unavailable. Check your network and that yiterm-tailcat is installed beside YiTerm."),
            Status::Ready(invitation) => {
                let link = invitation.url.clone();
                // Render at native integer module scale so camera recognition stays sharp.
                panel = panel.child(gpui::img(invitation.render.clone())
                    .w(px(invitation.pixels.width() as f32)).h(px(invitation.pixels.height() as f32)))
                    .child("Scan with Tenimon to connect to this desktop.")
                    .child(Button::new("copy-sharing-link").label("Copy connection link").on_click(
                        cx.listener(move |_, _, _, cx| { cx.write_to_clipboard(gpui::ClipboardItem::new_string(link.clone())); })))
                    .child(div().text_size(px(12.)).child("Keep this code private. Stopping sharing revokes it."));
            }
        }
        panel = panel.child(
            div()
                .flex()
                .gap(px(8.))
                .child(
                    Button::new("regenerate-sharing-code")
                        .label("New code")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(host) = &mut this.share_host {
                                host.pairing = Pairing::start(host.address.port());
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("stop-code-sharing")
                        .label("Stop sharing")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.share_host = None;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("close-sharing-code")
                        .label("Done")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(host) = &mut this.share_host {
                                host.pairing_visible = false;
                            }
                            cx.notify();
                        })),
                ),
        );
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(panel)
                .into_any_element(),
        )
    }
}
