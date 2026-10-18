//! GPUI window, terminal canvas, input routing, and application controls.
use super::*;
use gpui::{
    App, Application, Bounds, ContentMask, Context as ViewContext, ElementInputHandler,
    EntityInputHandler, FocusHandle, Focusable, KeyDownEvent, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point as UiPoint, ScrollDelta,
    ScrollWheelEvent, SharedString, TextRun, UTF16Selection, Window, WindowBounds, WindowOptions,
    canvas, div, fill, point, prelude::*, px, size,
};
use gpui_component::{
    IndexPath, Root, Selectable, Sizable,
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState, NumberInput, NumberInputEvent, StepAction},
    select::{SearchableVec, Select, SelectEvent, SelectState},
    switch::Switch,
};
use std::{borrow::Cow, ops::Range};

type Rect = Bounds<Pixels>;
const HEADER: f32 = 24.;
const DIVIDER: f32 = 5.;

gpui::actions!(terminal, [Complete, CompletePrevious]);

fn terminal_key_bindings() -> [gpui::KeyBinding; 2] {
    [
        gpui::KeyBinding::new("tab", Complete, Some("Terminal")),
        gpui::KeyBinding::new("shift-tab", CompletePrevious, Some("Terminal")),
    ]
}

#[derive(Clone)]
struct PaneGeometry {
    index: usize,
    outer: Rect,
    terminal: Rect,
}
#[derive(Clone)]
struct Divider {
    path: Vec<bool>,
    rect: Rect,
    parent: Rect,
    direction: SplitDirection,
}

impl PaneLayout {
    fn geometry(
        &self,
        rect: Rect,
        path: &mut Vec<bool>,
        panes: &mut Vec<PaneGeometry>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Self::Leaf(index) => panes.push(PaneGeometry {
                index: *index,
                outer: rect,
                terminal: Bounds::new(
                    rect.origin + point(px(2.), px(HEADER + 2.)),
                    size(
                        (rect.size.width - px(4.)).max(px(1.)),
                        (rect.size.height - px(HEADER + 4.)).max(px(1.)),
                    ),
                ),
            }),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let horizontal = *direction == SplitDirection::Right;
                let length = if horizontal {
                    rect.size.width
                } else {
                    rect.size.height
                };
                let available = (f32::from(length) - DIVIDER).max(0.);
                let offset = px(available * ratio.clamp(0.1, 0.9));
                let (a, gap, b) = if horizontal {
                    (
                        Bounds::new(rect.origin, size(offset, rect.size.height)),
                        Bounds::new(
                            rect.origin + point(offset, px(0.)),
                            size(px(DIVIDER), rect.size.height),
                        ),
                        Bounds::new(
                            rect.origin + point(offset + px(DIVIDER), px(0.)),
                            size(px(available) - offset, rect.size.height),
                        ),
                    )
                } else {
                    (
                        Bounds::new(rect.origin, size(rect.size.width, offset)),
                        Bounds::new(
                            rect.origin + point(px(0.), offset),
                            size(rect.size.width, px(DIVIDER)),
                        ),
                        Bounds::new(
                            rect.origin + point(px(0.), offset + px(DIVIDER)),
                            size(rect.size.width, px(available) - offset),
                        ),
                    )
                };
                dividers.push(Divider {
                    path: path.clone(),
                    rect: gap,
                    parent: rect,
                    direction: *direction,
                });
                path.push(false);
                first.geometry(a, path, panes, dividers);
                path.pop();
                path.push(true);
                second.geometry(b, path, panes, dividers);
                path.pop();
            }
        }
    }
    fn set_ratio(&mut self, path: &[bool], ratio: f32) {
        if let Self::Split {
            first,
            second,
            ratio: current,
            ..
        } = self
        {
            if let Some((right, rest)) = path.split_first() {
                if *right {
                    second.set_ratio(rest, ratio)
                } else {
                    first.set_ratio(rest, ratio)
                }
            } else {
                *current = ratio.clamp(0.1, 0.9);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Pane,
    Tools,
}

pub(super) struct TerminalUi {
    app: TerminalApp,
    focus: FocusHandle,
    panes: Vec<PaneGeometry>,
    dividers: Vec<Divider>,
    dragging: Option<Divider>,
    cell: gpui::Size<Pixels>,
    last_blink: Instant,
    profile: Option<profiling::Profile>,
    focused: bool,
    hover: Option<(usize, TerminalLink)>,
    context_menu: bool,
    font_select: gpui::Entity<SelectState<SearchableVec<String>>>,
    font_size: gpui::Entity<InputState>,
    _settings_subscriptions: Vec<gpui::Subscription>,
    menu: Option<Menu>,
    context_anchor: UiPoint<Pixels>,
    ime_selection: Range<usize>,
    #[cfg(windows)]
    clipboard_owner: std::result::Result<isize, String>,
    _redraw: gpui::Subscription,
    _activation: gpui::Subscription,
    _poll: gpui::Task<()>,
}

impl TerminalUi {
    fn new(app: TerminalApp, window: &mut Window, cx: &mut ViewContext<Self>) -> Self {
        let mut app = app;
        for tab in &mut app.tabs {
            for pane in &mut tab.panes {
                pane.poll();
            }
        }
        let focus = cx.focus_handle();
        focus.focus(window);
        // Canvas callbacks update this entity outside the normal view render pass.
        // Keep a window subscription even when a modal removes the input handler.
        let redraw = cx.observe_in(&cx.entity(), window, |_, _, window, _| window.refresh());
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.focused = window.is_window_active();
            if !this.app.tabs.is_empty() {
                let pane = this.session();
                pane.ime_preedit.clear();
                pane.selecting = false;
                pane.mouse_button = None;
                pane.cursor_blink_started = Instant::now();
            }
            cx.notify();
        });
        let task = cx.spawn(async move |this, cx| {
            loop {
                futures_lite::future::race(
                    async {
                        let _ = UI_WAKE.1.recv().await;
                    },
                    async {
                        gpui::Timer::after(Duration::from_millis(500)).await;
                    },
                )
                .await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        });
        let names = available_font_families(cx, &app.font_settings.family);
        let selected = names
            .iter()
            .position(|name| *name == app.font_settings.family)
            .map(|row| IndexPath::default().row(row));
        let font_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(names), selected, window, cx).searchable(true)
        });
        let font_size = cx.new(|cx| {
            InputState::new(window, cx).default_value(app.font_settings.size.to_string())
        });
        let settings_subscriptions = vec![
            cx.subscribe_in(&font_select, window, Self::on_font_selected),
            cx.subscribe_in(&font_size, window, |this, input, event, window, cx| {
                let text = input.read(cx).value();
                let parsed = text.parse::<f32>().ok().filter(|v| v.is_finite());
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    let size = parsed.unwrap_or(this.app.font_settings.size).clamp(8., 48.);
                    input.update(cx, |input, cx| {
                        input.set_value(size.to_string(), window, cx)
                    });
                    if this.app.font_settings.size != size {
                        this.app.font_settings.size = size;
                        this.apply_settings();
                    }
                } else if matches!(event, InputEvent::Change) {
                    if let Some(size) = parsed.filter(|v| (8.0..=48.0).contains(v)) {
                        if this.app.font_settings.size != size {
                            this.app.font_settings.size = size;
                            this.apply_settings();
                        }
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(
                &font_size,
                window,
                |this, input, event: &NumberInputEvent, window, cx| {
                    let NumberInputEvent::Step(action) = event;
                    let delta = match action {
                        StepAction::Increment => 1.,
                        StepAction::Decrement => -1.,
                    };
                    let size = input
                        .read(cx)
                        .value()
                        .parse::<f32>()
                        .ok()
                        .filter(|v| v.is_finite())
                        .unwrap_or(this.app.font_settings.size);
                    let size = (size + delta).clamp(8., 48.);
                    input.update(cx, |input, cx| {
                        input.set_value(size.to_string(), window, cx)
                    });
                    this.app.font_settings.size = size;
                    this.apply_settings();
                    cx.notify();
                },
            ),
        ];
        Self {
            app,
            focus,
            panes: vec![],
            dividers: vec![],
            dragging: None,
            cell: size(px(9.6), px(TERMINAL_FONT_SIZE * TERMINAL_LINE_HEIGHT)),
            last_blink: Instant::now(),
            profile: profiling::Profile::new(),
            focused: true,
            hover: None,
            context_menu: false,
            font_select,
            font_size,
            _settings_subscriptions: settings_subscriptions,
            menu: None,
            context_anchor: point(px(0.), px(0.)),
            ime_selection: 0..0,
            #[cfg(windows)]
            clipboard_owner: windows_clipboard_owner(window).map_err(|e| format!("{e:#}")),
            _redraw: redraw,
            _activation: activation,
            _poll: task,
        }
    }
    fn tick(&mut self, cx: &mut ViewContext<Self>) {
        let started = Instant::now();
        let mut changed = false;
        for tab in &mut self.app.tabs {
            for pane in &mut tab.panes {
                changed |= pane.repaint.swap(false, Ordering::AcqRel) || pane.paste_busy;
            }
        }
        for index in (0..self.app.tabs.len()).rev() {
            if !self.app.tabs[index].poll() {
                self.app.close_tab(index);
                changed = true;
            }
        }
        for tab in &mut self.app.tabs {
            for pane in &mut tab.panes {
                changed |= pane.pending_clipboard.is_some();
                #[cfg(windows)]
                pane.flush_clipboard_with(|text| {
                    let owner = self.clipboard_owner.clone().map_err(anyhow::Error::msg)?;
                    write_windows_clipboard(text, owner)
                });
                #[cfg(not(windows))]
                pane.flush_clipboard_with(|text| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                    if cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .as_deref()
                        != Some(text.as_str())
                    {
                        anyhow::bail!("Host clipboard read-back did not match the requested text");
                    }
                    Ok(())
                });
            }
        }
        let poll = started.elapsed();
        if self.focused
            && !self.blocked()
            && self.app.cursor_settings.blinking
            && self.last_blink.elapsed() >= Duration::from_millis(500)
        {
            changed = true;
            self.last_blink = Instant::now();
        }
        if changed || self.app.tabs.is_empty() {
            cx.notify();
        }
        if let Some(profile) = &mut self.profile {
            profile.sample(self.focused, started.elapsed(), poll);
        }
    }
    fn blocked(&self) -> bool {
        self.app.settings_open || self.app.about_open || self.context_menu || self.menu.is_some()
    }
    fn session(&mut self) -> &mut TerminalSession {
        self.app.tabs[self.app.active].focused_mut()
    }
    fn apply_settings(&mut self) {
        let result = self
            .app
            .args
            .config
            .clone()
            .map(Ok)
            .unwrap_or_else(logging::config_path)
            .and_then(|path| {
                settings::save_with_font(
                    &path,
                    self.app.cursor_settings,
                    self.app.theme,
                    &self.app.font_settings,
                )
            });
        if let Err(error) = result {
            self.app.error = Some(format!("Cannot save settings: {error:#}"));
        }
    }
    fn on_font_selected(
        &mut self,
        _: &gpui::Entity<SelectState<SearchableVec<String>>>,
        event: &SelectEvent<SearchableVec<String>>,
        _: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        if let SelectEvent::Confirm(Some(family)) = event {
            self.app.font_settings.family = family.clone();
            self.apply_settings();
            cx.notify();
        }
    }
    fn reset_settings(&mut self, window: &mut Window, cx: &mut ViewContext<Self>) {
        self.app.cursor_settings = CursorSettings::default();
        self.app.theme = ThemeMode::default();
        self.app.font_settings = settings::FontSettings::default();
        apply_component_theme(self.app.theme, Some(window), cx);
        let names = available_font_families(cx, &self.app.font_settings.family);
        let selected = names
            .iter()
            .position(|name| name == "Lilex")
            .map(|row| IndexPath::default().row(row));
        self.font_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(names), selected, window, cx).searchable(true)
        });
        self._settings_subscriptions[0] =
            cx.subscribe_in(&self.font_select, window, Self::on_font_selected);
        self.font_size
            .update(cx, |input, cx| input.set_value("16", window, cx));
        self.apply_settings();
        cx.notify();
    }
    fn complete(&mut self, previous: bool, cx: &mut ViewContext<Self>) {
        let session = self.session();
        if session.ime_preedit.is_empty() {
            session.term.scroll_display(Scroll::Bottom);
            session.write(if previous { b"\x1b[Z" } else { b"\t" });
            session.cursor_blink_started = Instant::now();
            cx.notify();
        }
        cx.stop_propagation();
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut ViewContext<Self>) {
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if self.blocked() {
            if key == "escape" {
                self.app.settings_open = false;
                self.app.about_open = false;
                self.context_menu = false;
                self.menu = None;
                self.focus.focus(window);
                cx.notify();
                cx.stop_propagation();
            }
            // Text inputs in settings need unhandled keystrokes to reach the
            // platform input handler. The terminal input handler is disabled
            // while a modal is open.
            return;
        }
        if !self.session().ime_preedit.is_empty() {
            return;
        }
        let command = if cfg!(target_os = "macos") {
            m.platform
        } else {
            m.control && m.shift
        };
        if command && matches!(key, "t" | "w" | "v" | "c") {
            match key {
                "t" => self.app.new_tab(),
                "w" => self.app.close_tab(self.app.active),
                "v" => self.session().paste(),
                "c" => self.session().copy_selection(),
                _ => {}
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "tab" && m.control && self.app.tabs.len() > 1 {
            let n = self.app.tabs.len();
            let delta = if m.shift { n - 1 } else { 1 };
            self.app.select((self.app.active + delta) % n);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "insert" && m.control {
            self.session().copy_selection();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if key == "insert" && m.shift {
            self.session().paste();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if let Some(bytes) = key_bytes(
            key,
            m,
            self.session().term.mode().contains(TermMode::APP_CURSOR),
        ) {
            let session = self.session();
            session.term.scroll_display(Scroll::Bottom);
            session.write(&bytes);
            session.cursor_blink_started = Instant::now();
            cx.stop_propagation();
            cx.notify();
        }
        let _ = window;
    }
    fn geometry(&mut self, bounds: Rect, window: &Window) {
        self.panes.clear();
        self.dividers.clear();
        self.cell.height = px(self.app.font_settings.size * TERMINAL_LINE_HEIGHT);
        let font = window
            .text_system()
            .resolve_font(&gpui::font(self.app.font_settings.family.clone()));
        if let Ok(advance) =
            window
                .text_system()
                .advance(font, px(self.app.font_settings.size), 'M')
        {
            self.cell.width = advance.width;
        }
        self.app.tabs[self.app.active].layout.geometry(
            bounds,
            &mut vec![],
            &mut self.panes,
            &mut self.dividers,
        );
        for geo in &self.panes {
            let pane = &mut self.app.tabs[self.app.active].panes[geo.index];
            let cols =
                ((geo.terminal.size.width / self.cell.width) as usize).clamp(2, u16::MAX as usize);
            let rows = ((geo.terminal.size.height / self.cell.height) as usize)
                .clamp(1, u16::MAX as usize);
            if cols != pane.size.cols || rows != pane.size.rows {
                if let Err(error) = pane.master.resize(PtySize {
                    cols: cols as u16,
                    rows: rows as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    pane.status = format!("PTY resize failed: {error}");
                }
                pane.size = Size { cols, rows };
                pane.term.resize(pane.size);
            }
        }
    }
    fn hit(&self, pos: UiPoint<Pixels>) -> Option<PaneGeometry> {
        self.panes.iter().find(|p| p.outer.contains(&pos)).cloned()
    }
    fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        if self.blocked() {
            return;
        }
        if event.button == MouseButton::Left
            && let Some(divider) = self
                .dividers
                .iter()
                .find(|d| d.rect.contains(&event.position))
        {
            self.dragging = Some(divider.clone());
            return;
        }
        let Some(geo) = self.hit(event.position) else {
            return;
        };
        self.focus.focus(window);
        self.app.tabs[self.app.active].select(geo.index);
        if event.button == MouseButton::Left
            && event.position.y < geo.outer.top() + px(HEADER)
            && event.position.x < geo.outer.left() + px(18.)
        {
            self.close_pane();
            cx.notify();
            return;
        }
        if !geo.terminal.contains(&event.position) {
            cx.notify();
            return;
        }
        let cell = self.cell;
        let pane = self.session();
        let (pos, side) = selection_point(
            geo.terminal,
            cell,
            pane.size,
            pane.term.grid().display_offset(),
            event.position,
        );
        let report = pane.term.mode().intersects(TermMode::MOUSE_MODE)
            && !event.modifiers.shift
            && pane.term.grid().display_offset() == 0;
        if event.button == MouseButton::Left
            && (if cfg!(target_os = "macos") {
                event.modifiers.platform
            } else {
                event.modifiers.control
            })
            && let Some(link) = terminal_link(&pane.term, pos)
        {
            cx.open_url(&link);
            cx.notify();
            return;
        }
        let button = button_code(event.button);
        pane.mouse_button = button;
        if report {
            if let Some(button) = button
                && let Some(bytes) = mouse_bytes(
                    pane.mouse_modes.protocol(*pane.term.mode()),
                    geo.terminal,
                    cell,
                    pane.size,
                    event.position,
                    (button, true),
                    event.modifiers,
                )
            {
                pane.write_mouse(&bytes);
            }
        } else if event.button == MouseButton::Left {
            pane.selecting = true;
            let kind = match event.click_count {
                2 => SelectionType::Semantic,
                3.. => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            pane.term.selection = Some(Selection::new(kind, pos, side));
        } else if event.button == MouseButton::Right {
            if cfg!(windows)
                && pane
                    .term
                    .selection_to_string()
                    .is_some_and(|s| !s.is_empty())
            {
                pane.copy_selection();
            } else {
                self.context_menu = true;
                self.context_anchor = event.position;
            }
        }
        cx.notify();
    }
    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut ViewContext<Self>) {
        if self.blocked() {
            return;
        }
        if let Some(divider) = &self.dragging {
            if !event.dragging() {
                self.dragging = None;
                return;
            }
            let horizontal = divider.direction == SplitDirection::Right;
            let position = if horizontal {
                event.position.x - divider.parent.left()
            } else {
                event.position.y - divider.parent.top()
            };
            let length = if horizontal {
                divider.parent.size.width
            } else {
                divider.parent.size.height
            };
            self.app.tabs[self.app.active].layout.set_ratio(
                &divider.path,
                f32::from(position) / (f32::from(length) - DIVIDER).max(1.),
            );
            cx.notify();
            return;
        }
        let active = self.app.tabs[self.app.active].active;
        let selecting = self.app.tabs[self.app.active].focused().selecting;
        let geo = if selecting {
            self.panes.iter().find(|g| g.index == active).cloned()
        } else {
            self.hit(event.position)
        };
        let Some(geo) = geo else {
            if self.hover.take().is_some() {
                cx.notify();
            }
            return;
        };
        let cell = self.cell;
        let pane = &mut self.app.tabs[self.app.active].panes[geo.index];
        let (pos, side) = selection_point(
            geo.terminal,
            cell,
            pane.size,
            pane.term.grid().display_offset(),
            event.position,
        );
        if pane.selecting {
            if let Some(selection) = &mut pane.term.selection {
                selection.update(pos, side);
            }
            cx.notify();
        } else {
            let mode = *pane.term.mode();
            if !event.modifiers.shift
                && pane.term.grid().display_offset() == 0
                && (mode.contains(TermMode::MOUSE_MOTION)
                    || (mode.contains(TermMode::MOUSE_DRAG) && event.pressed_button.is_some()))
            {
                let button = event.pressed_button.and_then(button_code).unwrap_or(3) + 32;
                if let Some(bytes) = mouse_bytes(
                    pane.mouse_modes.protocol(mode),
                    geo.terminal,
                    cell,
                    pane.size,
                    event.position,
                    (button, true),
                    event.modifiers,
                ) {
                    pane.write_mouse(&bytes);
                }
            }
            let link = terminal_link_details(&pane.term, pos);
            let changed = self.hover.as_ref().map(|(i, l)| (*i, l.url.as_str()))
                != link.as_ref().map(|l| (geo.index, l.url.as_str()));
            self.hover = link.map(|l| (geo.index, l));
            if changed {
                cx.notify();
            }
        }
    }
    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut ViewContext<Self>) {
        self.dragging = None;
        if self.blocked() {
            return;
        }
        let index = self.app.tabs[self.app.active].active;
        let Some(geo) = self.panes.iter().find(|g| g.index == index).cloned() else {
            return;
        };
        let cell = self.cell;
        let pane = self.session();
        if !pane.selecting
            && pane.term.mode().intersects(TermMode::MOUSE_MODE)
            && !event.modifiers.shift
            && pane.term.grid().display_offset() == 0
            && let Some(button) = pane.mouse_button.take()
            && let Some(bytes) = mouse_bytes(
                pane.mouse_modes.protocol(*pane.term.mode()),
                geo.terminal,
                cell,
                pane.size,
                event.position,
                (button, false),
                event.modifiers,
            )
        {
            pane.write_mouse(&bytes);
        }
        pane.selecting = false;
        pane.mouse_button = None;
        cx.notify();
    }
    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut ViewContext<Self>) {
        if self.blocked() {
            return;
        }
        let Some(geo) = self.hit(event.position) else {
            return;
        };
        let cell = self.cell;
        let pane = &mut self.app.tabs[self.app.active].panes[geo.index];
        let (delta, scale) = match event.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.y), f32::from(cell.height)),
            ScrollDelta::Lines(p) => (p.y, 1.),
        };
        let steps = pane.mouse_wheel.add(delta, scale);
        if pane.term.mode().intersects(TermMode::MOUSE_MODE)
            && !event.modifiers.shift
            && pane.term.grid().display_offset() == 0
        {
            for _ in 0..steps.unsigned_abs().min(128) {
                if let Some(bytes) = mouse_bytes(
                    pane.mouse_modes.protocol(*pane.term.mode()),
                    geo.terminal,
                    cell,
                    pane.size,
                    event.position,
                    (if steps > 0 { 64 } else { 65 }, true),
                    event.modifiers,
                ) {
                    pane.write_mouse(&bytes);
                }
            }
        } else if pane.term.mode().contains(TermMode::ALT_SCREEN)
            && pane.term.mode().contains(TermMode::ALTERNATE_SCROLL)
            && !event.modifiers.shift
        {
            for _ in 0..steps.unsigned_abs().min(128) {
                pane.write(if steps > 0 { b"\x1bOA" } else { b"\x1bOB" });
            }
        } else {
            pane.term.scroll_display(Scroll::Delta(steps));
        }
        cx.stop_propagation();
        cx.notify();
    }
    fn paint(&mut self, window: &mut Window, cx: &mut ViewContext<Self>) {
        if let Some(profile) = &mut self.profile {
            profile.rendered();
        }
        let theme = self.app.theme;
        self.focused = self.focus.is_focused(window) && window.is_window_active();
        let tab = &mut self.app.tabs[self.app.active];
        for geo in &self.panes {
            let focused = geo.index == tab.active;
            window.paint_quad(fill(geo.outer, theme.background()));
            let header = Bounds::new(geo.outer.origin, size(geo.outer.size.width, px(HEADER)));
            window.with_content_mask(Some(ContentMask { bounds: header }), |window| {
                window.paint_quad(fill(
                    Bounds::new(
                        geo.outer.origin + point(px(1.), px(2.)),
                        size(px(14.), px(16.)),
                    ),
                    if theme == ThemeMode::Dark {
                        rgb(0x333333)
                    } else {
                        rgb(0xE8E8E8)
                    },
                ));
                paint_ui_text(
                    "×",
                    geo.outer.origin + point(px(3.), px(1.)),
                    px(20.),
                    theme.foreground(),
                    window,
                    cx,
                );
                paint_ui_text(
                    &tab.panes[geo.index].title,
                    geo.outer.origin + point(px(20.), px(1.)),
                    px(20.),
                    theme.foreground(),
                    window,
                    cx,
                );
            });
            let pane = &mut tab.panes[geo.index];
            pane.cursor_settings = self.app.cursor_settings;
            pane.font_settings = self.app.font_settings.clone();
            window.with_content_mask(
                Some(ContentMask {
                    bounds: geo.terminal,
                }),
                |window| {
                    paint_terminal(
                        pane,
                        geo.terminal,
                        self.cell,
                        focused && self.focused,
                        theme,
                        self.hover
                            .as_ref()
                            .filter(|(i, _)| *i == geo.index)
                            .map(|(_, l)| l),
                        window,
                        cx,
                    );
                },
            );
        }
        for geo in &self.panes {
            window.paint_quad(gpui::outline(
                geo.outer,
                if geo.index == tab.active {
                    theme.accent()
                } else {
                    theme.border()
                },
                gpui::BorderStyle::Solid,
            ));
        }
        for divider in &self.dividers {
            window.paint_quad(fill(divider.rect, theme.border()));
        }
    }
    fn button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        cx: &ViewContext<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut ViewContext<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px(px(5.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .bg(if self.app.theme == ThemeMode::Dark {
                rgb(0x333333)
            } else {
                rgb(0xE8E8E8)
            })
            .rounded(px(2.))
            .cursor_pointer()
            .hover(|d| d.bg(self.app.theme.border()))
            .child(label.into())
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.notify();
            }))
    }
}

fn text_run(text: &str, color: Rgba) -> TextRun {
    TextRun {
        len: text.len(),
        font: gpui::font("Lilex"),
        color: color.into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}
fn paint_ui_text(
    text: &str,
    origin: UiPoint<Pixels>,
    height: Pixels,
    color: Rgba,
    window: &mut Window,
    cx: &mut App,
) {
    let run = TextRun {
        font: gpui::Font {
            weight: gpui::FontWeight::LIGHT,
            ..gpui::font("Ubuntu")
        },
        ..text_run(text, color)
    };
    let line = window
        .text_system()
        .shape_line(text.to_owned().into(), px(13.), &[run], None);
    if let Err(error) = line.paint(origin, height, window, cx) {
        tracing::warn!(%error,"UI text painting failed");
    }
}
fn paint_text(
    font: &settings::FontSettings,
    text: &str,
    origin: UiPoint<Pixels>,
    height: Pixels,
    color: Rgba,
    max_width: Option<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let runs = [TextRun {
        font: gpui::font(font.family.clone()),
        ..text_run(text, color)
    }];
    let mut line =
        window
            .text_system()
            .shape_line(text.to_owned().into(), px(font.size), &runs, None);
    // Fallback symbol fonts can have a wider advance than the selected mono font.
    // Fit them to their terminal columns before the cell mask clips the glyph.
    if let Some(width) = max_width.filter(|width| *width > px(0.) && line.width > *width) {
        let fitted_size = font.size * f32::from(width) / f32::from(line.width);
        line =
            window
                .text_system()
                .shape_line(text.to_owned().into(), px(fitted_size), &runs, None);
    }
    if let Err(error) = line.paint(origin, height, window, cx) {
        tracing::warn!(%error,"Text painting failed");
    }
}

// Draw common terminal status circles at a stable size regardless of font fallback.
fn paint_status_circle(text: &str, rect: Rect, color: Rgba, window: &mut Window) -> bool {
    if !matches!(text, "○" | "●" | "◐" | "◑" | "◒" | "◓") {
        return false;
    }
    let diameter = rect.size.width.min(rect.size.height) * 0.86;
    let thickness = (diameter * 0.08).max(px(1. / window.scale_factor()));
    let radius = (diameter - thickness) / 2.;
    let center = rect.center();
    let circle = |filled| {
        let mut path = if filled {
            gpui::PathBuilder::fill()
        } else {
            gpui::PathBuilder::stroke(thickness)
        };
        for step in 0..64 {
            let angle = step as f32 * std::f32::consts::TAU / 64.;
            let p = center + point(radius * angle.cos(), radius * angle.sin());
            if step == 0 {
                path.move_to(p);
            } else {
                path.line_to(p);
            }
        }
        path.close();
        path.build()
    };
    if let Ok(path) = circle(text == "●") {
        window.paint_path(path, color);
    }
    if matches!(text, "◐" | "◑" | "◒" | "◓") {
        let mut half = rect;
        match text {
            "◐" => half.size.width /= 2.,
            "◑" => {
                half.origin.x = center.x;
                half.size.width /= 2.;
            }
            "◒" => {
                half.origin.y = center.y;
                half.size.height /= 2.;
            }
            "◓" => half.size.height /= 2.,
            _ => unreachable!(),
        }
        window.with_content_mask(Some(ContentMask { bounds: half }), |window| {
            if let Ok(path) = circle(true) {
                window.paint_path(path, color);
            }
        });
    }
    true
}

// Box-drawing glyphs must reach the cell edges, including the extra leading
// between terminal rows. Font glyph bounds do not guarantee that connection.
fn box_drawing_rects(text: &str, rect: Rect, scale: f32) -> Option<Vec<Rect>> {
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    // TUIs also build scrollbars from fractional block glyphs. These need
    // their full cell height just like vertical box-drawing strokes.
    let block = match text {
        "█" => Some((0., 0., 8., 8.)),
        "▏" => Some((0., 0., 1., 8.)),
        "▎" => Some((0., 0., 2., 8.)),
        "▍" => Some((0., 0., 3., 8.)),
        "▌" => Some((0., 0., 4., 8.)),
        "▋" => Some((0., 0., 5., 8.)),
        "▊" => Some((0., 0., 6., 8.)),
        "▉" => Some((0., 0., 7., 8.)),
        "▐" => Some((4., 0., 8., 8.)),
        "▕" => Some((7., 0., 8., 8.)),
        "▀" => Some((0., 0., 8., 4.)),
        "▁" => Some((0., 7., 8., 8.)),
        "▂" => Some((0., 6., 8., 8.)),
        "▃" => Some((0., 5., 8., 8.)),
        "▄" => Some((0., 4., 8., 8.)),
        "▅" => Some((0., 3., 8., 8.)),
        "▆" => Some((0., 2., 8., 8.)),
        "▇" => Some((0., 1., 8., 8.)),
        _ => None,
    };
    if let Some((left, top, right, bottom)) = block {
        let left = snap(rect.left() + rect.size.width * (left / 8.));
        let top = snap(rect.top() + rect.size.height * (top / 8.));
        let right = snap(rect.left() + rect.size.width * (right / 8.));
        let bottom = snap(rect.top() + rect.size.height * (bottom / 8.));
        return Some(vec![Bounds::new(
            point(left, top),
            size(right - left, bottom - top),
        )]);
    }
    let (up, down, left, right) = match text {
        "│" => (true, true, false, false),
        "─" => (false, false, true, true),
        "┌" => (false, true, false, true),
        "┐" => (false, true, true, false),
        "└" => (true, false, false, true),
        "┘" => (true, false, true, false),
        "├" => (true, true, false, true),
        "┤" => (true, true, true, false),
        "┬" => (false, true, true, true),
        "┴" => (true, false, true, true),
        "┼" => (true, true, true, true),
        _ => return None,
    };
    let thickness = px(scale.round().max(1.) / scale);
    let x = snap(rect.center().x - thickness / 2.);
    let y = snap(rect.center().y - thickness / 2.);
    let mut lines = Vec::with_capacity(2);
    if up || down {
        let top = if up { snap(rect.top()) } else { y };
        let bottom = if down {
            snap(rect.bottom())
        } else {
            y + thickness
        };
        lines.push(Bounds::new(point(x, top), size(thickness, bottom - top)));
    }
    if left || right {
        let start = if left { snap(rect.left()) } else { x };
        let end = if right {
            snap(rect.right())
        } else {
            x + thickness
        };
        lines.push(Bounds::new(point(start, y), size(end - start, thickness)));
    }
    Some(lines)
}

fn paint_box_drawing(text: &str, rect: Rect, color: Rgba, window: &mut Window) -> bool {
    let Some(lines) = box_drawing_rects(text, rect, window.scale_factor()) else {
        return false;
    };
    for line in lines {
        window.paint_quad(fill(line, color));
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn paint_terminal(
    pane: &TerminalSession,
    rect: Rect,
    cell: gpui::Size<Pixels>,
    focused: bool,
    theme: ThemeMode,
    hover: Option<&TerminalLink>,
    window: &mut Window,
    cx: &mut App,
) {
    let content = pane.term.renderable_content();
    let offset = content.display_offset;
    let selection = content.selection;
    let cells: Vec<_> = content.display_iter.collect();
    // Paint backgrounds before glyphs so wide characters survive their spacer cells.
    for indexed in &cells {
        let row = indexed.point.line.0 + offset as i32;
        if row < 0 {
            continue;
        }
        let selected = selection.is_some_and(|s| s.contains(indexed.point));
        let bg = if selected {
            theme.selection()
        } else if indexed.cell.flags.contains(Flags::INVERSE) {
            color(indexed.cell.fg, true, theme)
        } else {
            color(indexed.cell.bg, false, theme)
        };
        if bg != theme.background() {
            window.paint_quad(fill(
                cell_rect(rect.origin, cell, indexed.point.column.0, row as usize, 1),
                bg,
            ));
        }
    }
    for indexed in &cells {
        let row = indexed.point.line.0 + offset as i32;
        if row < 0 {
            continue;
        }
        let flags = indexed.cell.flags;
        if flags
            .intersects(Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let selected = selection.is_some_and(|s| s.contains(indexed.point));
        let fg = if selected {
            theme.selection_foreground()
        } else if flags.contains(Flags::INVERSE) {
            color(indexed.cell.bg, false, theme)
        } else {
            color(indexed.cell.fg, true, theme)
        };
        let bounds = cell_rect(
            rect.origin,
            cell,
            indexed.point.column.0,
            row as usize,
            if flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            },
        );
        let text = terminal_cell_text(indexed.cell);
        if !text.is_empty()
            && text != " "
            && !paint_status_circle(&text, bounds, fg, window)
            && !paint_box_drawing(&text, bounds, fg, window)
        {
            // Keep font fallback and shaping inside the columns owned by this cell.
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_text(
                    &pane.font_settings,
                    &text,
                    bounds.origin,
                    cell.height,
                    fg,
                    Some(bounds.size.width),
                    window,
                    cx,
                )
            });
        }
        if flags.contains(Flags::UNDERLINE)
            || hover.is_some_and(|l| l.points.contains(&indexed.point))
        {
            window.paint_quad(fill(
                Bounds::new(
                    point(bounds.left(), bounds.bottom() - px(2.)),
                    size(bounds.size.width, px(1.)),
                ),
                fg,
            ));
        }
    }
    if focused && offset == 0 && pane.term.mode().contains(TermMode::SHOW_CURSOR) {
        let cursor = content.cursor.point;
        let mut bounds = cell_rect(
            rect.origin,
            cell,
            cursor.column.0,
            cursor.line.0.max(0) as usize,
            if pane.term.grid()[cursor].flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            },
        );
        if !pane.ime_preedit.is_empty() {
            paint_text(
                &pane.font_settings,
                &pane.ime_preedit,
                bounds.origin,
                cell.height,
                theme.foreground(),
                None,
                window,
                cx,
            );
        }
        if !pane.cursor_settings.blinking
            || pane.cursor_blink_started.elapsed().as_millis() % 1000 < 500
        {
            match pane.cursor_settings.shape {
                CursorShape::Beam => bounds.size.width = px(2.),
                CursorShape::Underline => {
                    bounds.origin.y = bounds.bottom() - px(2.);
                    bounds.size.height = px(2.);
                }
                CursorShape::Block => {}
            }
            window.paint_quad(fill(bounds, theme.cursor()));
            if pane.cursor_settings.shape == CursorShape::Block {
                let text = terminal_cell_text(&pane.term.grid()[cursor]);
                if !paint_status_circle(&text, bounds, theme.background(), window)
                    && !paint_box_drawing(&text, bounds, theme.background(), window)
                {
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        paint_text(
                            &pane.font_settings,
                            &text,
                            bounds.origin,
                            cell.height,
                            theme.background(),
                            Some(bounds.size.width),
                            window,
                            cx,
                        )
                    });
                }
            }
        }
    }
}
fn cell_rect(
    origin: UiPoint<Pixels>,
    cell: gpui::Size<Pixels>,
    col: usize,
    row: usize,
    width: usize,
) -> Rect {
    Bounds::from_corners(
        origin + point(cell.width * col as f32, cell.height * row as f32),
        origin
            + point(
                cell.width * (col + width) as f32,
                cell.height * (row + 1) as f32,
            ),
    )
}
fn selection_point(
    rect: Rect,
    cell: gpui::Size<Pixels>,
    size: Size,
    offset: usize,
    pos: UiPoint<Pixels>,
) -> (Point, Side) {
    let x = ((pos.x - rect.left()) / cell.width).clamp(0., size.cols as f32);
    let column = (x.floor() as usize).min(size.cols - 1);
    let row = (((pos.y - rect.top()) / cell.height).floor() as i32).clamp(0, size.rows as i32 - 1);
    (
        Point::new(Line(row - offset as i32), Column(column)),
        if x - (column as f32) < 0.5 {
            Side::Left
        } else {
            Side::Right
        },
    )
}
fn button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}
fn mouse_bytes(
    protocol: mouse::Protocol,
    rect: Rect,
    cell: gpui::Size<Pixels>,
    size: Size,
    pos: UiPoint<Pixels>,
    action: (u8, bool),
    m: Modifiers,
) -> Option<Vec<u8>> {
    let col =
        (((pos.x - rect.left()) / cell.width).floor().max(0.) as usize).min(size.cols - 1) + 1;
    let row =
        (((pos.y - rect.top()) / cell.height).floor().max(0.) as usize).min(size.rows - 1) + 1;
    let (button, pressed) = action;
    let modifiers = u8::from(m.shift) * 4 + u8::from(m.alt) * 8 + u8::from(m.control) * 16;
    if protocol.mode.contains(TermMode::SGR_MOUSE) {
        Some(
            format!(
                "\x1b[<{};{col};{row}{}",
                button + modifiers,
                if pressed { 'M' } else { 'm' }
            )
            .into_bytes(),
        )
    } else if protocol.urxvt {
        Some(
            format!(
                "\x1b[{};{col};{row}M",
                32 + if pressed { button } else { 3 } + modifiers
            )
            .into_bytes(),
        )
    } else if col > 223 || row > 223 {
        None
    } else {
        Some(vec![
            27,
            b'[',
            b'M',
            32 + if pressed { button } else { 3 } + modifiers,
            32 + col as u8,
            32 + row as u8,
        ])
    }
}

pub(super) fn key_bytes(key: &str, m: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    if m.control && key.len() == 1 {
        let c = key.as_bytes()[0].to_ascii_uppercase();
        if (b'@'..=b'_').contains(&c) {
            return Some(vec![c & 0x1f]);
        }
        if key == " " {
            return Some(vec![0]);
        }
    }
    let arrow = if app_cursor { "\x1bO" } else { "\x1b[" };
    let text = match key {
        "enter" => "\r".into(),
        "tab" if m.shift => "\x1b[Z".into(),
        "tab" => "\t".into(),
        "backspace" => "\x7f".into(),
        "escape" => "\x1b".into(),
        "up" => format!("{arrow}A"),
        "down" => format!("{arrow}B"),
        "right" => format!("{arrow}C"),
        "left" => format!("{arrow}D"),
        "home" => "\x1b[H".into(),
        "end" => "\x1b[F".into(),
        "insert" => "\x1b[2~".into(),
        "delete" => "\x1b[3~".into(),
        "pageup" => "\x1b[5~".into(),
        "pagedown" => "\x1b[6~".into(),
        "f1" => "\x1bOP".into(),
        "f2" => "\x1bOQ".into(),
        "f3" => "\x1bOR".into(),
        "f4" => "\x1bOS".into(),
        "f5" => "\x1b[15~".into(),
        "f6" => "\x1b[17~".into(),
        "f7" => "\x1b[18~".into(),
        "f8" => "\x1b[19~".into(),
        "f9" => "\x1b[20~".into(),
        "f10" => "\x1b[21~".into(),
        "f11" => "\x1b[23~".into(),
        "f12" => "\x1b[24~".into(),
        _ if m.alt && !m.platform && !m.control && key.chars().count() == 1 => format!("\x1b{key}"),
        _ => return None,
    };
    Some(text.into_bytes())
}

impl Focusable for TerminalUi {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for TerminalUi {
    fn render(&mut self, window: &mut Window, cx: &mut ViewContext<Self>) -> impl IntoElement {
        if self.app.tabs.is_empty() {
            window.remove_window();
            return div().into_any_element();
        }
        let title = self.app.tabs[self.app.active].focused().title.clone();
        if title != self.app.window_title {
            window.set_window_title(&title);
            self.app.window_title = title;
        }
        let theme = self.app.theme;
        let button_bg = if theme == ThemeMode::Dark {
            rgb(0x333333)
        } else {
            rgb(0xE8E8E8)
        };
        let menu = div()
            .flex()
            .items_center()
            .h(px(23.))
            .gap(px(4.))
            .px(px(4.))
            .child(self.menu_button("settings", "Settings", cx, |this, _, _| {
                this.app.settings_open = true;

                this.menu = None;
            }))
            .child(self.menu_button("pane-menu", "Pane", cx, |this, _, _| {
                this.menu = if this.menu == Some(Menu::Pane) {
                    None
                } else {
                    Some(Menu::Pane)
                };
            }))
            .child(self.menu_button("tools-menu", "Tools", cx, |this, _, _| {
                this.menu = if this.menu == Some(Menu::Tools) {
                    None
                } else {
                    Some(Menu::Tools)
                };
            }))
            .child(self.menu_button("about", "About", cx, |this, _, _| {
                this.app.about_open = true;
                this.app.about_licenses = false;
                this.menu = None;
            }));
        let mut tabs = div()
            .id("tabs")
            .flex()
            .items_center()
            .h(px(24.))
            .gap(px(8.))
            .px(px(3.))
            .overflow_x_scroll()
            .flex_shrink_0();
        for (index, tab) in self.app.tabs.iter().enumerate() {
            tabs = tabs.child(
                div()
                    .id(("tab", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .id(("select-tab", index))
                            .w(px(180.))
                            .h(px(20.))
                            .px(px(6.))
                            .flex()
                            .items_center()
                            .rounded(px(2.))
                            .bg(if index == self.app.active {
                                theme.selection()
                            } else {
                                button_bg
                            })
                            .cursor_pointer()
                            .child(div().overflow_hidden().text_ellipsis().child(format!(
                                "{}: {}",
                                index + 1,
                                tab.focused().title
                            )))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app.select(index);
                                this.hover = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id(("close-tab", index))
                            .h(px(18.))
                            .w(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(2.))
                            .bg(button_bg)
                            .cursor_pointer()
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app.close_tab(index);
                                this.hover = None;
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    ),
            );
        }
        tabs = tabs.child(self.button("new-tab", "+", cx, |this, _, _| this.app.new_tab()));
        let mut top = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(theme.surface())
            .border_b_1()
            .border_color(theme.border())
            .child(menu)
            .child(tabs);
        if let Some(error) = &self.app.error {
            top = top.child(
                div()
                    .px(px(4.))
                    .text_color(theme.palette(1))
                    .child(error.clone()),
            );
        }
        let entity = cx.entity();
        let painting = entity.clone();
        let blocked = self.blocked();
        let terminal = div()
            .id("terminal-area")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .cursor(gpui::CursorStyle::IBeam)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        entity.update(cx, |this, _| {
                            this.geometry(bounds, window);
                            this.panes
                                .iter()
                                .map(|geo| {
                                    let pane = &this.app.tabs[this.app.active].panes[geo.index];
                                    let cursor = if this
                                        .hover
                                        .as_ref()
                                        .is_some_and(|(index, _)| *index == geo.index)
                                    {
                                        gpui::CursorStyle::PointingHand
                                    } else if pane.term.mode().intersects(TermMode::MOUSE_MODE) {
                                        gpui::CursorStyle::Arrow
                                    } else {
                                        gpui::CursorStyle::IBeam
                                    };
                                    (
                                        window.insert_hitbox(
                                            geo.terminal,
                                            gpui::HitboxBehavior::Normal,
                                        ),
                                        cursor,
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                    },
                    move |bounds, cursors, window, cx| {
                        for (hitbox, cursor) in cursors {
                            window.set_cursor_style(cursor, &hitbox);
                        }
                        if !blocked {
                            let focus = painting.read(cx).focus.clone();
                            window.handle_input(
                                &focus,
                                ElementInputHandler::new(bounds, painting.clone()),
                                cx,
                            );
                        }
                        painting.update(cx, |this, cx| this.paint(window, cx));
                    },
                )
                .size_full(),
            );

        let status = self.app.tabs[self.app.active].focused().status.clone();
        let footer = div()
            .flex()
            .items_center()
            .h(px(24.))
            .px(px(3.))
            .gap(px(8.))
            .flex_shrink_0()
            .bg(theme.surface())
            .border_t_1()
            .border_color(theme.border())
            .child(self.button("paste", "Paste", cx, |this, _, _| this.session().paste()))
            .child(div().overflow_hidden().text_ellipsis().child(status));
        let mut root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background())
            .text_color(theme.foreground())
            .font_family("Ubuntu")
            .font_weight(gpui::FontWeight::LIGHT)
            .text_size(px(13.))
            .line_height(px(18.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            // Override Root's Tab focus navigation only while the terminal is active.
            .when(!self.blocked(), |root| {
                root.key_context("Terminal")
                    .on_action(cx.listener(|this, _: &Complete, _, cx| {
                        this.complete(false, cx);
                    }))
                    .on_action(cx.listener(|this, _: &CompletePrevious, _, cx| {
                        this.complete(true, cx);
                    }))
            })
            .child(top)
            .child(terminal)
            .child(footer);
        if let Some(menu) = self.menu {
            let mut items = div()
                .id("popup-menu")
                .occlude()
                .absolute()
                .left(px(if menu == Menu::Pane { 60. } else { 100. }))
                .top(px(23.))
                .min_w(px(130.))
                .p(px(4.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .bg(theme.surface())
                .border_1()
                .border_color(theme.border())
                .rounded(px(4.))
                .shadow_md()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
            if menu == Menu::Pane {
                items = items
                    .child(
                        self.menu_button("split-right", "Split Right", cx, |this, _, _| {
                            this.menu = None;
                            this.app.split_pane(SplitDirection::Right);
                        }),
                    )
                    .child(
                        self.menu_button("split-down", "Split Down", cx, |this, _, _| {
                            this.menu = None;
                            this.app.split_pane(SplitDirection::Down);
                        }),
                    )
                    .child(
                        self.menu_button("close-pane", "Close Pane", cx, |this, _, _| {
                            this.menu = None;
                            this.close_pane();
                        }),
                    );
            } else {
                items = items.child(self.menu_button(
                    "open-log-file",
                    "Open Log File",
                    cx,
                    |this, _, _| {
                        this.menu = None;
                        if let Err(error) = logging::open_current_file(&this.app.log_directory) {
                            this.app.error = Some(format!("{error:#}"));
                        }
                    },
                ));
            }
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            cx.notify();
                        }),
                    )
                    .child(items),
            );
        }
        if self.context_menu {
            let items = div()
                .id("context-menu")
                .occlude()
                .absolute()
                .left(self.context_anchor.x)
                .top(self.context_anchor.y)
                .min_w(px(100.))
                .p(px(4.))
                .flex()
                .flex_col()
                .bg(theme.surface())
                .border_1()
                .border_color(theme.border())
                .rounded(px(4.))
                .shadow_md()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(self.menu_button("menu-copy", "Copy", cx, |this, _, _| {
                    this.session().copy_selection();
                    this.context_menu = false;
                }))
                .child(self.menu_button("menu-paste", "Paste", cx, |this, _, _| {
                    this.session().paste();
                    this.context_menu = false;
                }));
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.context_menu = false;
                            cx.notify();
                        }),
                    )
                    .child(items),
            );
        }
        if self.app.settings_open || self.app.about_open {
            let panel = if self.app.settings_open {
                self.settings_panel(cx)
            } else {
                self.about_panel(cx)
            };
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(8.))
                    .bg(gpui::rgba(0x00000066))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.app.settings_open = false;
                            this.app.about_open = false;
                            cx.notify();
                        }),
                    )
                    .child(panel),
            );
        }
        root.into_any_element()
    }
}

impl TerminalUi {
    fn close_pane(&mut self) {
        let tab = &mut self.app.tabs[self.app.active];
        tab.close_pane(tab.active);
        if tab.panes.is_empty() {
            self.app.close_tab(self.app.active);
        }
        self.hover = None;
    }
    fn menu_button(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &ViewContext<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut ViewContext<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px(px(4.))
            .h(px(20.))
            .flex()
            .items_center()
            .rounded(px(2.))
            .cursor_pointer()
            .hover(|d| d.bg(self.app.theme.border()))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                action(this, window, cx);
                cx.notify();
            }))
    }
    fn settings_panel(&self, cx: &ViewContext<Self>) -> gpui::AnyElement {
        let theme = self.app.theme;
        let description = |title: &'static str, detail: &'static str| {
            div()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(14.)).child(title))
                .child(
                    div()
                        .mt(px(4.))
                        .text_size(px(12.))
                        .opacity(0.65)
                        .child(detail),
                )
        };
        let row = || {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(20.))
                .py(px(16.))
                .flex_shrink_0()
        };
        let divider = || div().h(px(1.)).bg(theme.border());
        let mut themes = div().flex().gap(px(4.));
        for (value, label) in [(ThemeMode::Dark, "Dark"), (ThemeMode::Light, "Light")] {
            themes = themes.child(
                Button::new(label)
                    .label(label)
                    .small()
                    .selected(theme == value)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.app.theme = value;
                        apply_component_theme(value, Some(window), cx);
                        this.apply_settings();
                        cx.notify();
                    })),
            );
        }
        let mut shapes = div().flex().gap(px(4.));
        for (shape, label) in [
            (CursorShape::Block, "Block"),
            (CursorShape::Underline, "Underline"),
            (CursorShape::Beam, "Bar"),
        ] {
            shapes = shapes.child(
                Button::new(label)
                    .label(label)
                    .small()
                    .selected(self.app.cursor_settings.shape == shape)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.app.cursor_settings.shape = shape;
                        this.apply_settings();
                        cx.notify();
                    })),
            );
        }
        div()
            .id("settings-dialog")
            .occlude()
            .w(px(600.))
            .max_w_full()
            .max_h(gpui::relative(0.95))
            .overflow_y_scroll()
            .p(px(24.))
            .text_size(px(13.))
            .bg(theme.surface())
            .border_1()
            .border_color(theme.border())
            .rounded(px(10.))
            .shadow_lg()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_size(px(20.))
                    .line_height(px(28.))
                    .child("Settings"),
            )
            .child(div().mt(px(20.)).opacity(0.6).child("APPEARANCE"))
            .child(
                row()
                    .child(description(
                        "Theme",
                        "Choose the window and terminal appearance.",
                    ))
                    .child(themes),
            )
            .child(divider())
            .child(
                row()
                    .child(description(
                        "Font Size",
                        "Terminal text size in pixels, from 8 to 48.",
                    ))
                    .child(
                        div()
                            .w(px(148.))
                            .flex_shrink_0()
                            .child(NumberInput::new(&self.font_size).small()),
                    ),
            )
            .child(divider())
            .child(
                row()
                    .child(description(
                        "Font Family",
                        "Use a monospace font to keep characters aligned.",
                    ))
                    .child(
                        div().w(px(220.)).flex_shrink_0().child(
                            Select::new(&self.font_select)
                                .small()
                                .search_placeholder("Search fonts…")
                                .w_full(),
                        ),
                    ),
            )
            .child(
                div()
                    .mt(px(4.))
                    .p(px(14.))
                    .h(px(self.app.font_settings.size * 1.3 + 28.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .bg(theme.background())
                    .overflow_hidden()
                    .font_family(self.app.font_settings.family.clone())
                    .text_size(px(self.app.font_settings.size))
                    .line_height(px(self.app.font_settings.size * 1.3))
                    .child("Aa Bb 0123  ~/projects"),
            )
            .child(div().mt(px(24.)).opacity(0.6).child("CURSOR"))
            .child(
                row()
                    .child(description(
                        "Cursor Style",
                        "Choose the terminal cursor shape.",
                    ))
                    .child(shapes),
            )
            .child(divider())
            .child(
                row()
                    .child(description(
                        "Cursor Blinking",
                        "Blink the cursor while the terminal is focused.",
                    ))
                    .child(
                        Switch::new("cursor-blinking")
                            .checked(self.app.cursor_settings.blinking)
                            .small()
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.app.cursor_settings.blinking = *checked;
                                this.apply_settings();
                                cx.notify();
                            })),
                    ),
            )
            .child(divider())
            .child(
                div()
                    .mt(px(16.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.))
                            .opacity(0.6)
                            .child("Changes are saved automatically."),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("settings-reset")
                                    .label("Reset Defaults")
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_settings(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("settings-done")
                                    .label("Done")
                                    .primary()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.app.settings_open = false;
                                        this.focus.focus(window);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
    fn about_panel(&self, cx: &ViewContext<Self>) -> gpui::AnyElement {
        let theme = self.app.theme;
        let mut panel = div()
            .id("about-dialog")
            .occlude()
            .w(px(376.))
            .max_w_full()
            .max_h(gpui::relative(0.95))
            .overflow_y_scroll()
            .p(px(8.))
            .bg(theme.surface())
            .border_1()
            .border_color(theme.border())
            .rounded(px(6.))
            .shadow_lg()
            .flex()
            .flex_col()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .mt(px(18.))
                    .size(px(72.))
                    .rounded(px(18.))
                    .bg(theme.selection())
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family("Lilex")
                    .text_size(px(30.))
                    .text_color(theme.accent())
                    .child(">_"),
            )
            .child(
                div()
                    .mt(px(12.))
                    .text_size(px(28.))
                    .line_height(px(36.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("YiTerm"),
            )
            .child(
                div()
                    .mt(px(8.))
                    .child("A lightweight terminal for macOS and Windows."),
            )
            .child("Tabs, split panes and image paste over SSH.")
            .child(
                div()
                    .mt(px(20.))
                    .child(format!("Version  {}", env!("CARGO_PKG_VERSION"))),
            );
        if let Some(revision) = option_env!("YITERM_BUILD_REV") {
            let short = revision.get(..8).unwrap_or(revision);
            panel = panel.child(
                div()
                    .id("commit-link")
                    .cursor_pointer()
                    .text_color(theme.accent())
                    .child(format!("Commit  {short}"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&format!(
                            "https://github.com/wynemo/yiterm/commit/{revision}"
                        ))
                    })),
            );
        } else {
            panel = panel.child(div().opacity(0.65).child("Local development build"));
        }
        panel = panel.child(
            div()
                .mt(px(20.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(self.menu_button("docs", "Docs", cx, |_, _, cx| {
                    cx.open_url("https://github.com/wynemo/yiterm/blob/main/TECHNICAL.md")
                }))
                .child("|")
                .child(self.menu_button("github", "GitHub", cx, |_, _, cx| {
                    cx.open_url("https://github.com/wynemo/yiterm")
                }))
                .child("|")
                .child(self.button("licenses", "Licenses", cx, |this, _, _| {
                    this.app.about_licenses = !this.app.about_licenses
                })),
        );
        if self.app.about_licenses {
            panel = panel.child(
                div()
                    .id("license-text")
                    .w_full()
                    .h(px(220.))
                    .mt(px(12.))
                    .overflow_y_scroll()
                    .child("Lilex · SIL Open Font License 1.1")
                    .child(include_str!("../assets/fonts/lilex/OFL.txt"))
                    .child("GPUI · Apache-2.0")
                    .child(include_str!("../licenses/gpui-LICENSE-APACHE")),
            );
        }
        panel
            .child(div().mt(px(12.)).child(self.button(
                "close-about",
                "Close",
                cx,
                |this, _, _| this.app.about_open = false,
            )))
            .into_any_element()
    }
}

// The platform input handler owns committed text. KeyDown handles only shortcuts
// and control sequences; printable text must never be sent through both paths.
impl EntityInputHandler for TerminalUi {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut ViewContext<Self>,
    ) -> Option<String> {
        let text = &self.session().ime_preedit;
        let len = text.encode_utf16().count();
        let start = range.start.min(len);
        let end = range.end.min(len).max(start);
        let a = utf16_offset(text, start);
        let b = utf16_offset(text, end);
        *actual = Some(start..end);
        Some(text[a..b].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut ViewContext<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.ime_selection.clone(),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut ViewContext<Self>) -> Option<Range<usize>> {
        let text = &self.app.tabs[self.app.active].focused().ime_preedit;
        (!text.is_empty()).then(|| 0..text.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut ViewContext<Self>) {
        self.session().ime_preedit.clear();
        self.ime_selection = 0..0;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        if self.blocked() {
            return;
        }
        let pane = self.session();
        pane.ime_preedit.clear();
        pane.term.scroll_display(Scroll::Bottom);
        pane.write(text.as_bytes());
        pane.cursor_blink_started = Instant::now();
        self.ime_selection = 0..0;
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut ViewContext<Self>,
    ) {
        if self.blocked() {
            return;
        }
        let pane = self.session();
        let old = &pane.ime_preedit;
        let range = range.unwrap_or(0..old.encode_utf16().count());
        let a = utf16_offset(old, range.start);
        let b = utf16_offset(old, range.end).max(a);
        pane.ime_preedit = format!("{}{}{}", &old[..a], text, &old[b..]);
        let len = pane.ime_preedit.encode_utf16().count();
        self.ime_selection = selected.unwrap_or(len..len);
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Rect,
        _: &mut Window,
        _: &mut ViewContext<Self>,
    ) -> Option<Rect> {
        let tab = &self.app.tabs[self.app.active];
        let geo = self.panes.iter().find(|g| g.index == tab.active)?;
        let p = tab.focused().term.grid().cursor.point;
        Some(cell_rect(
            geo.terminal.origin,
            self.cell,
            p.column.0,
            p.line.0.max(0) as usize,
            1,
        ))
    }
    fn character_index_for_point(
        &mut self,
        _: UiPoint<Pixels>,
        _: &mut Window,
        _: &mut ViewContext<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}
fn utf16_offset(text: &str, index: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= index {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

fn available_font_families(cx: &App, current: &str) -> Vec<String> {
    let mut names = cx.text_system().all_font_names();
    names.retain(|name| !name.starts_with('.'));
    names.push("Lilex".into());
    names.push(current.to_owned());
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    names
}

fn component_theme(theme: ThemeMode) -> gpui_component::ThemeMode {
    match theme {
        ThemeMode::Dark => gpui_component::ThemeMode::Dark,
        ThemeMode::Light => gpui_component::ThemeMode::Light,
    }
}

fn apply_component_theme(theme: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    gpui_component::Theme::change(component_theme(theme), window, cx);
    gpui_component::Theme::global_mut(cx).ring = gpui::rgb(0x3B82F6).into();
}

pub(super) fn run(app: TerminalApp) -> Result<()> {
    let result = std::rc::Rc::new(std::cell::RefCell::new(Ok(())));
    let outcome = result.clone();
    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            cx.bind_keys(terminal_key_bindings());
            apply_component_theme(app.theme, None, cx);
            if let Err(error) = cx.text_system().add_fonts(vec![
                Cow::Borrowed(include_bytes!("../assets/fonts/lilex/Lilex-Regular.ttf")),
                Cow::Borrowed(include_bytes!("../assets/fonts/ubuntu/Ubuntu-Light.ttf")),
            ]) {
                *outcome.borrow_mut() = Err(error);
                cx.quit();
                return;
            }
            let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(420.), px(260.))),
                    ..Default::default()
                },
                move |window, cx| {
                    let view = cx.new(|cx| TerminalUi::new(app, window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            );
            if let Err(error) = opened {
                *outcome.borrow_mut() = Err(error);
                cx.quit();
                return;
            }
            tracing::info!(renderer = "GPUI", "GPUI window initialized");
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
    std::mem::replace(&mut *result.borrow_mut(), Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_tab_bindings_override_root_focus_navigation() {
        let mut keymap = gpui::Keymap::default();
        // Model the component Root's focus-navigation bindings.
        keymap.add_bindings([
            gpui::KeyBinding::new("tab", gpui::NoAction, Some("Root")),
            gpui::KeyBinding::new("shift-tab", gpui::NoAction, Some("Root")),
        ]);
        keymap.add_bindings(terminal_key_bindings());
        let contexts = [
            gpui::KeyContext::parse("Root").unwrap(),
            gpui::KeyContext::parse("Terminal").unwrap(),
        ];
        for (key, previous) in [("tab", false), ("shift-tab", true)] {
            let input = [gpui::Keystroke::parse(key).unwrap()];
            let (bindings, pending) = keymap.bindings_for_input(&input, &contexts);
            assert!(!pending);
            let action = bindings.first().expect("terminal handles Tab").action();
            if previous {
                assert!(action.as_any().is::<CompletePrevious>());
            } else {
                assert!(action.as_any().is::<Complete>());
            }
            // With a modal open the terminal context is absent.
            let (bindings, _) = keymap.bindings_for_input(&input, &contexts[..1]);
            assert!(bindings.is_empty());
        }
    }

    #[test]
    fn scrollbar_blocks_connect_across_rows_and_track_thumb_changes() {
        for scale in [1., 1.25, 1.5, 2.] {
            let cell = size(px(9.6), px(20.8));
            let origin = point(px(2.3), px(26.7));
            for text in ["█", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "▐", "▕"] {
                let first =
                    box_drawing_rects(text, cell_rect(origin, cell, 0, 0, 1), scale).unwrap()[0];
                let next =
                    box_drawing_rects(text, cell_rect(origin, cell, 0, 1, 1), scale).unwrap()[0];
                assert_eq!(first.bottom(), next.top(), "{text} at {scale}");
                assert_eq!(first.left(), next.left());
                assert_eq!(first.right(), next.right());
            }
            let track = box_drawing_rects("▕", cell_rect(origin, cell, 0, 0, 1), scale).unwrap()[0];
            let thumb = box_drawing_rects("▐", cell_rect(origin, cell, 0, 1, 1), scale).unwrap()[0];
            assert_eq!(track.bottom(), thumb.top());
            assert_eq!(track.right(), thumb.right());
            assert!(thumb.left() < track.left());
        }
    }

    #[test]
    fn box_drawing_connects_across_fractional_terminal_cells() {
        for scale in [1., 1.25, 1.5, 2.] {
            for font_size in [12., 16., 19., 24.] {
                let cell = size(px(font_size * 0.6), px(font_size * 1.3));
                let origin = point(px(2.3), px(26.7));
                // A corner, vertical run and bottom corner must share exact
                // physical-pixel endpoints, even with fractional row heights.
                let top = box_drawing_rects("┌", cell_rect(origin, cell, 0, 0, 1), scale).unwrap();
                let middle =
                    box_drawing_rects("│", cell_rect(origin, cell, 0, 1, 1), scale).unwrap();
                let bottom =
                    box_drawing_rects("└", cell_rect(origin, cell, 0, 2, 1), scale).unwrap();
                assert_eq!(top[0].bottom(), middle[0].top());
                assert_eq!(middle[0].bottom(), bottom[0].top());
                assert_eq!(top[0].left(), middle[0].left());
                let horizontal =
                    box_drawing_rects("─", cell_rect(origin, cell, 1, 0, 1), scale).unwrap();
                assert_eq!(top[1].right(), horizontal[0].left());
                assert_eq!(top[1].top(), horizontal[0].top());
            }
        }
        let rect = Bounds::new(point(px(0.), px(0.)), size(px(10.), px(20.)));
        assert!(box_drawing_rects("|", rect, 1.).is_none());
        assert!(box_drawing_rects("A", rect, 1.).is_none());
        assert!(box_drawing_rects("│\u{301}", rect, 1.).is_none());
    }
    #[test]
    fn nested_splits_partition_space_and_collapse_after_close() {
        let mut layout = PaneLayout::Leaf(0);
        layout.split(0, 1, SplitDirection::Right);
        layout.split(1, 2, SplitDirection::Down);
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(600.)));
        let mut panes = vec![];
        let mut dividers = vec![];
        layout.geometry(bounds, &mut vec![], &mut panes, &mut dividers);
        assert_eq!(panes.len(), 3);
        assert_eq!(dividers.len(), 2);
        for (i, a) in panes.iter().enumerate() {
            for b in &panes[i + 1..] {
                assert!(!a.outer.intersects(&b.outer));
            }
        }
        layout.set_ratio(&[], 0.7);
        panes.clear();
        dividers.clear();
        layout.geometry(bounds, &mut vec![], &mut panes, &mut dividers);
        assert_eq!(panes[0].outer.size.width, px((800. - DIVIDER) * 0.7));
        let layout = layout.remove(1).unwrap().remove(1).unwrap();
        assert_eq!(layout, PaneLayout::Leaf(0));
    }
    #[test]
    fn selection_coordinates_include_scrollback_and_clamp_drag() {
        let rect = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(60.)));
        let cell = size(px(10.), px(20.));
        let size = Size { cols: 10, rows: 3 };
        assert_eq!(
            selection_point(rect, cell, size, 5, point(px(11.), px(21.))),
            (Point::new(Line(-5), Column(0)), Side::Left)
        );
        assert_eq!(
            selection_point(rect, cell, size, 5, point(px(500.), px(500.))),
            (Point::new(Line(-3), Column(9)), Side::Right)
        );
    }
    #[test]
    fn mouse_protocols_preserve_release_buttons_modifiers_and_large_coordinates() {
        let rect = Bounds::new(point(px(0.), px(0.)), size(px(4000.), px(500.)));
        let cell = size(px(10.), px(20.));
        let size = Size {
            cols: 400,
            rows: 25,
        };
        let pos = point(px(2995.), px(10.));
        let modifiers = Modifiers {
            shift: true,
            control: true,
            ..Default::default()
        };
        assert_eq!(
            mouse_bytes(
                TermMode::SGR_MOUSE.into(),
                rect,
                cell,
                size,
                pos,
                (2, false),
                modifiers
            )
            .unwrap(),
            b"\x1b[<22;300;1m"
        );
        assert_eq!(
            mouse_bytes(
                mouse::Protocol {
                    mode: TermMode::empty(),
                    urxvt: true
                },
                rect,
                cell,
                size,
                pos,
                (2, false),
                Modifiers::default()
            )
            .unwrap(),
            b"\x1b[35;300;1M"
        );
        assert!(
            mouse_bytes(
                TermMode::empty().into(),
                rect,
                cell,
                size,
                pos,
                (0, true),
                Modifiers::default()
            )
            .is_none()
        );
    }
    #[test]
    fn keys_distinguish_text_control_and_application_cursor() {
        assert_eq!(
            key_bytes(
                "c",
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false
            ),
            Some(vec![3])
        );
        assert_eq!(
            key_bytes("up", Modifiers::default(), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(
                "tab",
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                false
            ),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(key_bytes("a", Modifiers::default(), false), None);
        assert_eq!(key_bytes("中", Modifiers::default(), false), None);
        assert_eq!(
            key_bytes(
                "x",
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                false
            ),
            Some(b"\x1bx".to_vec())
        );
    }
    #[test]
    fn ime_utf16_offsets_preserve_surrogate_pairs() {
        let text = "a😀中";
        assert_eq!(utf16_offset(text, 0), 0);
        assert_eq!(utf16_offset(text, 1), 1);
        assert_eq!(utf16_offset(text, 3), 5);
        assert_eq!(utf16_offset(text, 4), 8);
        assert!(text.is_char_boundary(utf16_offset(text, 2)));
    }
    #[test]
    fn terminal_unicode_shaping_keeps_variation_and_combining_sequences() {
        let mut cell = alacritty_terminal::term::cell::Cell {
            c: '❤',
            ..Default::default()
        };
        cell.push_zerowidth('\u{fe0f}');
        assert_eq!(terminal_cell_text(&cell), "❤️");
    }
}
