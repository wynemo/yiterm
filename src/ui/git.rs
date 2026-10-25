use super::*;
use crate::git::{LineKind, ListRow, Section};
use gpui_component::Disableable;
impl TerminalUi {
    pub(super) fn sidebar_tabs(&self, cx: &ViewContext<Self>) -> gpui::AnyElement {
        let active = self.app.files_settings.mode;
        let theme = self.app.theme;
        let mut tabs = div()
            .flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(9.))
            .border_1()
            .border_color(theme.border());
        for (id, label, mode) in [
            ("sidebar-git", "Git", settings::FilesPanelMode::Git),
            ("sidebar-files", "Files", settings::FilesPanelMode::Files),
        ] {
            tabs = tabs.child(
                div()
                    .id(id)
                    .px(px(14.))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_color(theme.palette(8))
                    .hover(|d| d.text_color(theme.foreground()))
                    .when(active == mode, |d| {
                        d.bg(theme.background())
                            .text_color(theme.foreground())
                            .shadow_sm()
                    })
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.app.set_sidebar_mode(mode);
                        this.app.poll_files(true);
                        cx.notify();
                    })),
            );
        }
        tabs.into_any_element()
    }
    pub(super) fn git_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => {
                if self.app.git.selected.is_some() {
                    self.app.git.back();
                } else {
                    self.focus.focus(window);
                }
            }
            "up" | "down" if self.app.git.selected.is_none() => {
                if let Some(snapshot) = &self.app.git.snapshot {
                    let rows = &snapshot.rows;
                    let next = if event.keystroke.key == "up" {
                        (0..self.app.git.cursor.unwrap_or(rows.len()))
                            .rev()
                            .find(|i| matches!(rows[*i], ListRow::Change(_)))
                    } else {
                        (self.app.git.cursor.map_or(0, |i| i + 1)..rows.len())
                            .find(|i| matches!(rows[*i], ListRow::Change(_)))
                    };
                    if let Some(index) = next {
                        self.app.git.cursor = Some(index);
                        self.app
                            .git
                            .scroll
                            .scroll_to_item(index, gpui::ScrollStrategy::Center);
                    }
                }
            }
            "enter" if self.app.git.selected.is_none() => {
                let change =
                    self.app
                        .git
                        .snapshot
                        .as_ref()
                        .and_then(|s| {
                            self.app.git.cursor.and_then(|i| s.rows.get(i)).and_then(
                                |row| match row {
                                    ListRow::Change(i) => s.changes.get(*i),
                                    _ => None,
                                },
                            )
                        })
                        .cloned();
                if let Some(change) = change {
                    self.app.git.open(change);
                    self.app.git.poll();
                }
            }
            "up" | "down" | "pageup" | "pagedown" if self.app.git.selected.is_some() => {
                let step = if event.keystroke.key.starts_with("page") {
                    400.
                } else {
                    20.
                };
                let sign = if matches!(event.keystroke.key.as_str(), "up" | "pageup") {
                    1.
                } else {
                    -1.
                };
                let handle = &self.app.git.diff_scroll.0.borrow().base_handle;
                let offset = handle.offset();
                handle.set_offset(point(offset.x, offset.y + px(step * sign)));
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }
    fn git_action_button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        enabled: bool,
        cx: &ViewContext<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut ViewContext<Self>) + 'static,
    ) -> gpui::AnyElement {
        Button::new(id)
            .label(label)
            .small()
            .when(label == "Commit staged", |b| b.primary())
            .when(label != "Commit staged", |b| b.ghost())
            .disabled(!enabled)
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.stop_propagation();
                cx.notify();
            }))
            .into_any_element()
    }
    fn git_commit_form(
        &mut self,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) -> gpui::AnyElement {
        let key = self.app.git.draft_key().cloned();
        let message = self.app.git.commit_message.clone();
        if self.git_message_repo != key
            || self.git_message.read(cx).value().as_ref() != message.as_str()
        {
            self.git_message_repo = key;
            self.git_message
                .update(cx, |input, cx| input.set_value(message, window, cx));
        }
        let staged = self
            .app
            .git
            .snapshot
            .as_ref()
            .map_or(0, |s| s.count(Section::Staged));
        let status = self
            .app
            .git
            .action_label()
            .map(|label| format!("{label}…"))
            .unwrap_or_else(|| format!("Commit {staged} staged file(s)"));
        let error = self.app.git.action_error().map(str::to_owned);
        div()
            .w_full()
            .flex_shrink_0()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(12.))
            .border_t_1()
            .border_color(self.app.theme.border())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.files_focus.focus(window);
                    cx.stop_propagation();
                }
            }))
            .child(status)
            .child(Input::new(&self.git_message).small().h(px(72.)))
            .child(self.git_action_button(
                "git-commit",
                "Commit staged",
                self.app.git.can_commit(),
                cx,
                |this, _, _| this.app.git.commit(),
            ))
            .when_some(error, |d, error| {
                d.child(
                    div()
                        .id("git-action-error")
                        .w_full()
                        .max_h(px(140.))
                        .overflow_y_scroll()
                        .text_color(self.app.theme.palette(1))
                        .child(error),
                )
            })
            .into_any_element()
    }
    fn git_history(&self, cx: &mut ViewContext<Self>) -> gpui::AnyElement {
        let theme = self.app.theme;
        let snapshot = self.app.git.snapshot.as_ref().unwrap();
        let mut panel = div()
            .w_full()
            .min_w_0()
            .min_h_0()
            .flex_1()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border())
            .child(div().px(px(6.)).py(px(4.)).child("Commits"));
        if let Some(error) = &snapshot.history_error {
            panel = panel.child(
                div()
                    .p(px(6.))
                    .child(format!("History unavailable: {error}")),
            );
        } else if snapshot.commits.is_empty() {
            panel = panel.child(div().p(px(6.)).child("No commits yet"));
        } else {
            panel = panel.child(
                gpui::uniform_list(
                    "git-commits",
                    snapshot.commits.len(),
                    cx.processor(move |this, range: Range<usize>, _, _| {
                        let Some(snapshot) = &this.app.git.snapshot else {
                            return vec![];
                        };
                        let wide = this.app.files_settings.width >= 480.;
                        range
                            .filter_map(|index| {
                                let commit = snapshot.commits.get(index)?;
                                let details = commit.details();
                                Some(
                                    div()
                                        .id(("git-commit-row", index))
                                        .h(px(30.))
                                        .px(px(6.))
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .w(px(20.))
                                                .h(px(20.))
                                                .rounded(px(10.))
                                                .text_center()
                                                .bg(theme.border())
                                                .child(
                                                    commit
                                                        .author
                                                        .chars()
                                                        .next()
                                                        .unwrap_or('?')
                                                        .to_uppercase()
                                                        .to_string(),
                                                ),
                                        )
                                        .when(!commit.refs.is_empty(), |d| {
                                            d.child(
                                                div()
                                                    .max_w(px(if wide { 130. } else { 80. }))
                                                    .truncate()
                                                    .px(px(4.))
                                                    .rounded(px(3.))
                                                    .bg(theme.border())
                                                    .text_color(theme.palette(4))
                                                    .child(commit.refs.join(", ")),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(commit.subject.clone()),
                                        )
                                        .when(wide, |d| {
                                            d.child(
                                                div()
                                                    .w(px(80.))
                                                    .flex_shrink_0()
                                                    .truncate()
                                                    .child(commit.author.clone()),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .font_family("Lilex")
                                                .child(commit.short_id().to_owned()),
                                        )
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(details.clone())
                                                .max_w(px(600.))
                                                .whitespace_normal()
                                                .build(window, cx)
                                        }),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.app.git.history_scroll.clone())
                .flex_1()
                .min_h_0(),
            );
        }
        panel.into_any_element()
    }
    pub(super) fn git_panel(
        &mut self,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) -> gpui::AnyElement {
        let theme = self.app.theme;
        let header = self.sidebar_header(cx);
        let message = self.app.git.message();
        let branch = self
            .app
            .git
            .snapshot
            .as_ref()
            .map(|s| match &s.upstream {
                Some(upstream) => format!("{} → {upstream}", s.branch),
                None => s.branch.clone(),
            })
            .unwrap_or_else(|| "Git · Local".into());
        let directory = self
            .app
            .git
            .snapshot
            .as_ref()
            .map(|s| crate::git::shown(&s.root))
            .unwrap_or_else(|| crate::git::shown(&self.app.tabs[self.app.active].files.root));
        let mut content = div()
            .id("git-panel")
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.))
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
            .child(div().px(px(16.)).py(px(6.)).truncate().child(branch))
            .child(
                div()
                    .px(px(16.))
                    .text_size(px(11.))
                    .text_color(theme.palette(8))
                    .truncate()
                    .child(directory),
            );
        if let Some(selected) = &self.app.git.selected {
            let label = format!(
                "{} · {}",
                selected.section.label(),
                crate::git::shown(&selected.path)
            );
            let change = selected.clone();
            let action_label = match change.section {
                Section::Staged => "Unstage",
                _ => "Stage",
            };
            let action_enabled = self.app.git.can_act() && change.section != Section::Conflicted;
            content = content
                .child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .px(px(4.))
                        .child(self.git_action_button(
                            "git-diff-action",
                            action_label,
                            action_enabled,
                            cx,
                            move |this, _, _| this.app.git.act_on(change.clone()),
                        ))
                        .child(self.button("git-back", "Back", cx, |this, _, cx| {
                            this.app.git.back();
                            cx.notify();
                        }))
                        .child(
                            self.button("git-copy-path", "Copy Path", cx, |this, _, cx| {
                                if let (Some(snapshot), Some(change)) =
                                    (&this.app.git.snapshot, &this.app.git.selected)
                                {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        snapshot
                                            .root
                                            .join(&change.path)
                                            .to_string_lossy()
                                            .into_owned(),
                                    ));
                                }
                            }),
                        ),
                )
                .child(div().px(px(6.)).truncate().child(label));
            match &self.app.git.diff {
                Some(Ok(diff)) => {
                    let notice = diff.notice.clone();
                    let list = gpui::uniform_list(
                        "git-diff",
                        diff.lines.len(),
                        cx.processor(move |this, range: Range<usize>, _, _| {
                            range
                                .filter_map(|index| {
                                    let line = this
                                        .app
                                        .git
                                        .diff
                                        .as_ref()?
                                        .as_ref()
                                        .ok()?
                                        .lines
                                        .get(index)?;
                                    let color = match line.kind {
                                        LineKind::Addition => theme.palette(2),
                                        LineKind::Deletion => theme.palette(1),
                                        LineKind::Hunk => theme.palette(4),
                                        LineKind::Header => theme.palette(3),
                                        LineKind::Context => theme.foreground(),
                                    };
                                    Some(
                                        div()
                                            .h(px(20.))
                                            .px(px(6.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .font_family("Lilex")
                                            .text_color(color)
                                            .child(line.text.clone()),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(self.app.git.diff_scroll.clone())
                    .flex_1()
                    .min_h_0();
                    content = content.child(list);
                    if let Some(notice) = notice {
                        content = content.child(div().p(px(6.)).child(notice));
                    }
                }
                Some(Err(error)) => {
                    content = content.child(div().flex_1().min_h_0().p(px(6.)).child(error.clone()))
                }
                None => {
                    content =
                        content.child(div().flex_1().min_h_0().p(px(6.)).child("Reading diff…"))
                }
            }
        } else if let Some(snapshot) = &self.app.git.snapshot {
            if snapshot.rows.is_empty() {
                content = content.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .p(px(6.))
                        .child("Working tree clean"),
                );
            } else {
                let list = gpui::uniform_list(
                    "git-changes",
                    snapshot.rows.len(),
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        let Some(snapshot) = &this.app.git.snapshot else {
                            return vec![];
                        };
                        range
                            .filter_map(|index| {
                                let row = snapshot.rows.get(index)?;
                                Some(match row {
                                    ListRow::Header(section, count) => {
                                        let section = *section;
                                        let label = if section == Section::Staged {
                                            "Unstage all"
                                        } else {
                                            "Stage all"
                                        };
                                        div()
                                            .id(("git-section", index))
                                            .h(px(32.))
                                            .px(px(6.))
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .overflow_hidden()
                                            .child(
                                                div().flex_1().min_w_0().truncate().child(format!(
                                                    "{} ({count})",
                                                    section.label()
                                                )),
                                            )
                                            .when(section != Section::Conflicted, |d| {
                                                d.child(this.git_action_button(
                                                    ("git-section-action", index),
                                                    label,
                                                    this.app.git.can_act(),
                                                    cx,
                                                    move |this, _, _| {
                                                        this.app.git.act_section(section)
                                                    },
                                                ))
                                            })
                                    }
                                    ListRow::Change(change_index) => {
                                        let change = snapshot.changes[*change_index].clone();
                                        let stage_change = change.clone();
                                        let section = change.section;
                                        let label = format!(
                                            "{}  {}",
                                            change.mark.label(),
                                            crate::git::shown(&change.path)
                                        );
                                        div()
                                            .id(("git-change", index))
                                            .h(px(32.))
                                            .px(px(8.))
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .cursor_pointer()
                                            .hover(|d| {
                                                d.bg(gpui::Hsla::from(theme.border()).opacity(0.25))
                                            })
                                            .when(this.app.git.cursor == Some(index), |d| {
                                                d.bg(theme.selection())
                                            })
                                            .child(div().flex_1().min_w_0().truncate().child(label))
                                            .when(section != Section::Conflicted, |d| {
                                                d.child(this.git_action_button(
                                                    ("git-row-action", index),
                                                    if section == Section::Staged {
                                                        "Unstage"
                                                    } else {
                                                        "Stage"
                                                    },
                                                    this.app.git.can_act(),
                                                    cx,
                                                    move |this, _, _| {
                                                        this.app.git.act_on(stage_change.clone())
                                                    },
                                                ))
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.app.git.cursor = Some(index);
                                                this.app.git.open(change.clone());
                                                this.app.git.poll();
                                                cx.notify();
                                            }))
                                    }
                                })
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(self.app.git.scroll.clone())
                .flex_1()
                .min_h_0();
                content = content.child(list);
            }
        }
        if self.app.git.snapshot.is_some() {
            content = content.child(self.git_commit_form(window, cx));
            if self.app.git.selected.is_none() {
                content = content.child(self.git_history(cx));
            }
        } else {
            if let Some(label) = self.app.git.action_label() {
                content = content.child(div().p(px(6.)).child(format!("{label}…")));
            }
        }
        content
            .when(!message.is_empty(), |d| {
                d.child(div().p(px(6.)).text_size(px(12.)).child(message))
            })
            .into_any_element()
    }
}
