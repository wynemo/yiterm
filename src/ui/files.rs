use super::*;
impl TerminalUi {
    pub(super) fn sidebar_surface(&self) -> Rgba {
        if self.app.theme == ThemeMode::Light {
            rgb(0xFAF9F6)
        } else {
            rgb(0x202228)
        }
    }
    pub(super) fn sidebar_header(&self, cx: &ViewContext<Self>) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .px(px(12.))
            .pt(px(8.))
            .pb(px(2.))
            .child(self.sidebar_tabs(cx))
            .into_any_element()
    }
    pub(super) fn files_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        if !self.files_focus.is_focused(window) {
            return;
        }
        if self.app.files_settings.mode == settings::FilesPanelMode::Git {
            self.git_key(event, window, cx);
            return;
        }
        let state = &mut self.app.tabs[self.app.active].files;
        match event.keystroke.key.as_str() {
            "escape" => {
                if state.preview_path.is_some() {
                    state.back();
                } else {
                    self.focus.focus(window);
                }
            }
            "up" | "down" if state.preview_path.is_none() => {
                if !state.rows.is_empty() {
                    let index = match state.selected {
                        Some(i) if event.keystroke.key == "up" => i.saturating_sub(1),
                        Some(i) => (i + 1).min(state.rows.len() - 1),
                        None => 0,
                    };
                    state.selected = Some(index);
                    state
                        .scroll
                        .scroll_to_item(index, gpui::ScrollStrategy::Center);
                }
            }
            "enter" | "right" if state.preview_path.is_none() => {
                if let Some(index) = state.selected {
                    if event.keystroke.key == "enter"
                        || !state.expanded.contains(&state.rows[index].entry.path)
                    {
                        state.activate(index);
                    }
                }
            }
            "left" if state.preview_path.is_none() => {
                if let Some(index) = state.selected {
                    if state.expanded.contains(&state.rows[index].entry.path) {
                        state.activate(index);
                    }
                }
            }
            "up" | "down" | "pageup" | "pagedown" if state.preview.is_some() => {
                // UniformList uses pixel offsets; keep preview navigation independent of the PTY.
                let delta = if matches!(event.keystroke.key.as_str(), "up" | "pageup") {
                    1.
                } else {
                    -1.
                };
                let amount = if event.keystroke.key.starts_with("page") {
                    400.
                } else {
                    20.
                };
                let offset = state.preview_scroll.0.borrow().base_handle.offset();
                state
                    .preview_scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(offset.x, offset.y + px(delta * amount)));
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }
    pub(super) fn files_panel(
        &mut self,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) -> gpui::AnyElement {
        if self.app.files_settings.mode == settings::FilesPanelMode::Git {
            return self.git_panel(window, cx);
        }
        let theme = self.app.theme;
        let preview = self.app.tabs[self.app.active].files.preview_path.is_some();
        let root = self.app.tabs[self.app.active]
            .files
            .root
            .to_string_lossy()
            .into_owned();
        let header = self.sidebar_header(cx);
        let state = &self.app.tabs[self.app.active].files;
        let notice = state.notice();
        let label = if let Some(preview) = &state.preview {
            preview.path.to_string_lossy().into_owned()
        } else if preview {
            state.selected_path().to_string_lossy().into_owned()
        } else {
            root
        };
        let list = if preview {
            gpui::uniform_list(
                "file-preview",
                state.preview.as_ref().map_or(0, |p| p.lines.len()),
                cx.processor(|this, range: Range<usize>, _, _| {
                    let state = &this.app.tabs[this.app.active].files;
                    range
                        .map(|index| {
                            div()
                                .h(px(20.))
                                .px(px(6.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font_family("Lilex")
                                .child(
                                    state
                                        .preview
                                        .as_ref()
                                        .and_then(|preview| preview.lines.get(index))
                                        .cloned()
                                        .unwrap_or_default(),
                                )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(state.preview_scroll.clone())
            .flex_1()
            .min_h_0()
            .into_any_element()
        } else {
            gpui::uniform_list(
                "file-tree",
                state.rows.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    let state = &this.app.tabs[this.app.active].files;
                    range
                        .filter_map(|index| {
                            let row = state.rows.get(index)?;
                            let expanded = state.expanded.contains(&row.entry.path);
                            let icon = if row.entry.directory {
                                IconName::Folder
                            } else if row.entry.regular {
                                IconName::File
                            } else {
                                IconName::ExternalLink
                            };
                            let badge = this.app.git.badge(&row.entry.path);
                            let name: String = row
                                .entry
                                .name
                                .chars()
                                .map(|c| if c.is_control() { ' ' } else { c })
                                .collect();
                            Some(
                                div()
                                    .id(("file-row", index))
                                    .w_full()
                                    .min_w_0()
                                    .h(px(30.))
                                    .pl(px(26. + row.depth.min(40) as f32 * 18.))
                                    .pr(px(12.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.))
                                    .rounded(px(4.))
                                    .hover(|d| d.bg(gpui::Hsla::from(theme.border()).opacity(0.25)))
                                    .when(state.selected == Some(index), |d| {
                                        d.bg(gpui::Hsla::from(theme.selection()).opacity(0.6))
                                    })
                                    .when(badge.is_some(), |d| {
                                        d.text_color(theme.palette(
                                            if badge == Some("!") || badge == Some("D") {
                                                1
                                            } else if badge == Some("A") || badge == Some("U") {
                                                2
                                            } else {
                                                3
                                            },
                                        ))
                                    })
                                    .child(
                                        div()
                                            .w(px(12.))
                                            .flex_shrink_0()
                                            .text_color(theme.palette(8))
                                            .when(row.entry.directory, |d| {
                                                d.child(
                                                    Icon::new(if expanded {
                                                        IconName::ChevronDown
                                                    } else {
                                                        IconName::ChevronRight
                                                    })
                                                    .size(px(12.)),
                                                )
                                            }),
                                    )
                                    .child(
                                        Icon::new(icon).size(px(16.)).text_color(theme.palette(8)),
                                    )
                                    .child(div().flex_1().min_w_0().truncate().child(name))
                                    .when_some(badge, |d, badge| {
                                        d.child(div().flex_shrink_0().pr(px(12.)).child(badge))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.app.tabs[this.app.active].files.activate(index);
                                        cx.notify();
                                    })),
                            )
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(state.scroll.clone())
            .flex_1()
            .min_h_0()
            .into_any_element()
        };
        div()
            .id("files-panel")
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(0.))
            .bg(self.sidebar_surface())
            .border_l_1()
            .border_color(theme.border())
            .track_focus(&self.files_focus)
            .on_key_down(cx.listener(Self::files_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.files_focus.focus(window);
                    this.session().ime_preedit.clear();
                    cx.stop_propagation();
                }),
            )
            .child(header)
            .child(
                div()
                    .id("files-root")
                    .flex_shrink_0()
                    .mx(px(12.))
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .when(preview, |d| {
                        d.child(
                            div()
                                .id("files-preview-back")
                                .size(px(24.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(5.))
                                .cursor_pointer()
                                .text_color(theme.palette(8))
                                .hover(|d| d.bg(gpui::Hsla::from(theme.border()).opacity(0.4)))
                                .child(Icon::new(IconName::ArrowLeft).size(px(16.)))
                                .tooltip(|window, cx| {
                                    Tooltip::new("Back to files (Esc)").build(window, cx)
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.app.tabs[this.app.active].files.back();
                                    this.files_focus.focus(window);
                                    cx.stop_propagation();
                                    cx.notify();
                                })),
                        )
                    })
                    .when(!preview, |d| {
                        d.child(
                            Icon::new(IconName::ChevronDown)
                                .size(px(12.))
                                .text_color(theme.palette(8)),
                        )
                    })
                    .child(
                        Icon::new(if preview {
                            IconName::File
                        } else {
                            IconName::FolderOpen
                        })
                        .size(px(16.))
                        .text_color(theme.palette(8)),
                    )
                    .child(
                        div().flex_1().min_w_0().truncate().child(
                            std::path::Path::new(&label)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| label.clone()),
                        ),
                    )
                    .when_some(self.app.git.badge(&state.root), |d, badge| {
                        d.child(
                            div()
                                .text_color(theme.palette(if matches!(badge, "U" | "A") {
                                    2
                                } else {
                                    3
                                }))
                                .child(badge),
                        )
                    })
                    .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx)),
            )
            .child(list)
            .when(!notice.is_empty(), |d| {
                d.child(div().p(px(6.)).text_size(px(12.)).child(notice))
            })
            .into_any_element()
    }
}
