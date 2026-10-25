use super::*;
impl TerminalUi {
    fn browse_files(&mut self, window: &mut Window, cx: &mut ViewContext<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Folder".into()),
        });
        // Keep the picker attached to its original tab, even if tabs change while it is open.
        let tab_index = self.app.active;
        let identity = self.app.tabs[tab_index].files.identity();
        cx.spawn_in(window, async move |this, cx| {
            let result = paths.await;
            let _ = this.update_in(cx, |this, _, cx| {
                let mut selected = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            if let Some(tab) = this
                                .app
                                .tabs
                                .iter_mut()
                                .find(|tab| tab.files.identity() == identity)
                            {
                                tab.files.set_root(path.clone());
                                selected = true;
                            }
                        }
                    }
                    Ok(Ok(None)) => {}
                    other => this.app.error = Some(format!("Cannot select folder: {other:?}")),
                }
                if selected {
                    this.app.set_files_follow(false);
                }
                cx.notify();
            });
        })
        .detach();
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
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .p(px(4.))
            .child(self.sidebar_tabs(cx))
            .child(
                div()
                    .flex()
                    .gap(px(3.))
                    .child(
                        self.button("files-select", "Folder…", cx, |this, window, cx| {
                            this.browse_files(window, cx)
                        }),
                    )
                    .child(self.button("files-close", "×", cx, |this, window, cx| {
                        this.app.toggle_files();
                        this.focus.focus(window);
                        cx.notify();
                    })),
            );
        let toolbar = div()
            .flex()
            .gap(px(3.))
            .px(px(4.))
            .child(self.button("files-up", "↑", cx, |this, _, cx| {
                this.app.set_files_follow(false);
                let state = &mut this.app.tabs[this.app.active].files;
                if let Some(path) = state.root.parent().map(|p| p.to_owned()) {
                    state.set_root(path);
                }
                cx.notify();
            }))
            .child(self.button("files-refresh", "Refresh", cx, |this, _, cx| {
                this.app.refresh_sidebar();
                cx.notify();
            }))
            .child(self.button(
                "files-hidden",
                if self.app.tabs[self.app.active].files.hidden {
                    "Hide dotfiles"
                } else {
                    "Dotfiles"
                },
                cx,
                |this, _, cx| {
                    let state = &mut this.app.tabs[this.app.active].files;
                    state.hidden = !state.hidden;
                    state.refresh();
                    cx.notify();
                },
            ));
        let follow = self.app.files_settings.follow_terminal;
        let follow_control = self.button(
            "files-follow",
            if follow { "Follow ✓" } else { "Follow" },
            cx,
            |this, _, cx| {
                this.app
                    .set_files_follow(!this.app.files_settings.follow_terminal);
                this.app.poll_files(true);
                cx.notify();
            },
        );
        let actions = div()
            .flex()
            .gap(px(3.))
            .px(px(4.))
            .when(preview, |d| {
                d.child(self.button("files-back", "Back", cx, |this, _, cx| {
                    this.app.tabs[this.app.active].files.back();
                    cx.notify();
                }))
            })
            .child(self.button("files-copy", "Copy Path", cx, |this, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                    this.app.tabs[this.app.active]
                        .files
                        .selected_path()
                        .to_string_lossy()
                        .into_owned(),
                ));
            }))
            .child(self.button("files-open", "Open", cx, |this, _, cx| {
                this.app.tabs[this.app.active].files.open();
                cx.notify();
            }));
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
                            let marker = if row.entry.directory {
                                if state.expanded.contains(&row.entry.path) {
                                    "▾"
                                } else {
                                    "▸"
                                }
                            } else if row.entry.regular {
                                "·"
                            } else {
                                "↗"
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
                                    .h(px(24.))
                                    .pl(px(6. + row.depth.min(40) as f32 * 14.))
                                    .pr(px(4.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .when(state.selected == Some(index), |d| d.bg(theme.border()))
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
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(format!("{marker} {name}")),
                                    )
                                    .when_some(badge, |d, badge| {
                                        d.child(div().flex_shrink_0().pr(px(4.)).child(badge))
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
            .gap(px(4.))
            .bg(theme.surface())
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
            .child(toolbar.child(follow_control))
            .child(div().px(px(6.)).truncate().child(label))
            .child(actions)
            .child(list)
            .when(!notice.is_empty(), |d| {
                d.child(div().p(px(6.)).text_size(px(12.)).child(notice))
            })
            .into_any_element()
    }
}
