use super::*;
use crate::git::{ListRow, Section};
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
    fn git_row_button(
        &self,
        id: impl Into<gpui::ElementId>,
        icon: IconName,
        tooltip: &'static str,
        enabled: bool,
        cx: &ViewContext<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut ViewContext<Self>) + 'static,
    ) -> gpui::AnyElement {
        Button::new(id)
            .icon(icon)
            .tooltip(tooltip)
            .small()
            .w(px(26.))
            .h(px(26.))
            .flex_shrink_0()
            .ghost()
            .disabled(!enabled)
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.stop_propagation();
                cx.notify();
            }))
            .into_any_element()
    }
    fn confirm_git_discard(
        &mut self,
        change: crate::git::Change,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        if !self.app.git.can_act() {
            return;
        }
        let Some(snapshot) = self.app.git.snapshot.clone() else {
            return;
        };
        let title = format!("Discard {}?", crate::git::shown(&change.path));
        let detail = if change.section == Section::Untracked {
            "This deletes the untracked file or directory and its untracked contents. This cannot be undone. Ignored files and nested repositories are preserved."
        } else if matches!(
            change.mark,
            crate::git::Mark::Added | crate::git::Mark::Renamed
        ) {
            "This deletes the added or renamed file and removes its index entry. This cannot be undone."
        } else {
            "This restores the working file from the index. Its unstaged changes will be lost. This cannot be undone."
        };
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &title,
            Some(detail),
            &[
                gpui::PromptButton::cancel("Cancel"),
                gpui::PromptButton::ok("Discard"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() == Some(1) {
                let _ = this.update_in(cx, |this, _, cx| {
                    this.app.git.discard(&snapshot, change);
                    cx.notify();
                });
            }
        })
        .detach();
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
            .map(|s| s.branch.clone())
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
            .child(
                div()
                    .id("git-branch")
                    .h(px(34.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        gpui::svg()
                            .path("icons/git-branch.svg")
                            .size(px(18.))
                            .text_color(theme.palette(8)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(branch))
                    .tooltip(move |window, cx| Tooltip::new(directory.clone()).build(window, cx)),
            );
        if let Some(snapshot) = &self.app.git.snapshot {
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
                                            .w_full()
                                            .h(px(34.))
                                            .px(px(10.))
                                            .flex()
                                            .items_center()
                                            .gap(px(4.))
                                            .overflow_hidden()
                                            .text_size(px(12.))
                                            .text_color(theme.palette(8))
                                            .child(div().flex_1().min_w_0().truncate().child(
                                                if section == Section::Unstaged {
                                                    "Changes"
                                                } else {
                                                    section.label()
                                                },
                                            ))
                                            .child(div().flex_shrink_0().child(count.to_string()))
                                            .when(section != Section::Conflicted, |d| {
                                                d.child(this.git_row_button(
                                                    ("git-section-action", index),
                                                    if section == Section::Staged {
                                                        IconName::Minus
                                                    } else {
                                                        IconName::Plus
                                                    },
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
                                        let diff_change = change.clone();
                                        let discard_change = change.clone();
                                        let section = change.section;
                                        let name = change
                                            .path
                                            .file_name()
                                            .map(|n| crate::git::shown(std::path::Path::new(n)))
                                            .unwrap_or_else(|| crate::git::shown(&change.path));
                                        let parent = change
                                            .path
                                            .parent()
                                            .filter(|p| !p.as_os_str().is_empty())
                                            .map(crate::git::shown);
                                        let color = theme.palette(match change.mark {
                                            crate::git::Mark::Added
                                            | crate::git::Mark::Untracked => 2,
                                            crate::git::Mark::Deleted
                                            | crate::git::Mark::Conflicted => 1,
                                            _ => 3,
                                        });
                                        div()
                                            .id(("git-change", index))
                                            .w_full()
                                            .h(px(34.))
                                            .px(px(8.))
                                            .border_1()
                                            .border_color(gpui::transparent_black())
                                            .rounded(px(4.))
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
                                                d.border_color(theme.accent())
                                            })
                                            .child(
                                                div()
                                                    .size(px(22.))
                                                    .flex_shrink_0()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(3.))
                                                    .text_size(px(11.))
                                                    .text_color(color)
                                                    .bg(gpui::Hsla::from(color).opacity(0.1))
                                                    .child(change.mark.label()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(7.))
                                                    .child(
                                                        div()
                                                            .min_w_0()
                                                            .max_w(px(
                                                                if this.app.files_settings.width
                                                                    < 400.
                                                                {
                                                                    130.
                                                                } else {
                                                                    300.
                                                                },
                                                            ))
                                                            .truncate()
                                                            .child(name),
                                                    )
                                                    .when_some(parent, |d, parent| {
                                                        d.child(
                                                            div()
                                                                .min_w_0()
                                                                .truncate()
                                                                .text_color(theme.palette(8))
                                                                .child(parent),
                                                        )
                                                    }),
                                            )
                                            .child(this.git_row_button(
                                                ("git-row-diff", index),
                                                IconName::File,
                                                "View diff",
                                                true,
                                                cx,
                                                move |this, _, cx| {
                                                    this.app.git.cursor = Some(index);
                                                    this.app.git.open(diff_change.clone());
                                                    this.app.git.poll();
                                                    cx.notify();
                                                },
                                            ))
                                            .child(this.git_row_button(
                                                ("git-row-discard", index),
                                                IconName::Undo2,
                                                "Discard changes",
                                                this.app.git.can_act()
                                                    && matches!(
                                                        section,
                                                        Section::Unstaged | Section::Untracked
                                                    ),
                                                cx,
                                                move |this, window, cx| {
                                                    this.confirm_git_discard(
                                                        discard_change.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            ))
                                            .child(this.git_row_button(
                                                ("git-row-action", index),
                                                if section == Section::Staged {
                                                    IconName::Minus
                                                } else {
                                                    IconName::Plus
                                                },
                                                if section == Section::Staged {
                                                    "Unstage"
                                                } else {
                                                    "Stage"
                                                },
                                                this.app.git.can_act()
                                                    && section != Section::Conflicted,
                                                cx,
                                                move |this, _, _| {
                                                    this.app.git.act_on(stage_change.clone())
                                                },
                                            ))
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
                .w_full()
                .px(px(8.))
                .flex_1()
                .min_h_0();
                content = content.child(list);
            }
        }
        if self.app.git.snapshot.is_some() {
            content = content.child(self.git_commit_form(window, cx));
            content = content.child(self.git_history(cx));
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
