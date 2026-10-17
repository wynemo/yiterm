use super::*;
use portable_pty::CommandBuilder;
#[test]
fn ansi_white_text_is_readable_in_light_theme_without_changing_backgrounds() {
    for (named, index) in [(NamedColor::White, 7), (NamedColor::BrightWhite, 15)] {
        for value in [Color::Named(named), Color::Indexed(index)] {
            assert_eq!(
                color(value, true, ThemeMode::Light),
                ThemeMode::Light.foreground()
            );
            assert_eq!(
                color(value, false, ThemeMode::Light),
                ThemeMode::Light.palette(index as usize)
            );
            assert_eq!(
                color(value, true, ThemeMode::Dark),
                ThemeMode::Dark.palette(index as usize)
            );
        }
    }
}

#[test]
fn theme_colors_preserve_explicit_terminal_colors() {
    use alacritty_terminal::vte::ansi::Rgb;
    let rgb = Color::Spec(Rgb {
        r: 12,
        g: 34,
        b: 56,
    });
    for theme in [ThemeMode::Dark, ThemeMode::Light] {
        assert_eq!(
            color(Color::Named(NamedColor::Background), false, theme),
            theme.background()
        );
        assert_eq!(
            color(Color::Named(NamedColor::Foreground), true, theme),
            theme.foreground()
        );
        assert_eq!(color(rgb, true, theme), rgb_bytes(12, 34, 56));
        for index in 16..=255 {
            assert_eq!(
                color(Color::Indexed(index), true, theme),
                color(Color::Indexed(index), true, ThemeMode::Dark)
            );
        }
    }
    assert_ne!(
        color(Color::Indexed(1), true, ThemeMode::Dark),
        color(Color::Indexed(1), true, ThemeMode::Light)
    );
}

#[test]
fn terminal_unicode_keeps_grid_positions_and_original_selection() {
    for (input, _display, columns) in [
        ("⚠", "⚠", 1),
        ("⚠\u{FE0F}", "⚠", 2),
        ("❤\u{FE0F}", "❤", 2),
        ("❤\u{FE0E}", "❤", 1),
        ("✓\u{FE0F}", "✓", 1),
        ("中\u{E0100}", "中", 2),
        ("ᠠ\u{180B}", "ᠠ", 1),
        ("a\u{034F}\u{0301}", "a\u{0301}", 1),
        ("a\u{200D}\u{2060}\u{FEFF}", "a", 1),
        ("🏴\u{E0067}\u{E0062}\u{E007F}", "🏴", 2),
        ("1\u{FE0F}\u{20E3}", "1\u{20E3}", 2),
    ] {
        let (tx, _rx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &Size { cols: 20, rows: 2 }, Listener(tx));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, format!("{input}X").as_bytes());
        let first = &term.grid()[Point::new(Line(0), Column(0))];
        assert_eq!(terminal_cell_text(first), input, "{input:?}");
        assert_eq!(first.flags.contains(Flags::WIDE_CHAR), columns == 2);
        assert_eq!(term.grid()[Point::new(Line(0), Column(columns))].c, 'X');
        assert_eq!(term.grid().cursor.point.column.0, columns + 1);
        term.selection = Some(Selection::new(
            SelectionType::Lines,
            Point::new(Line(0), Column(0)),
            Side::Left,
        ));
        assert_eq!(
            term.selection_to_string().unwrap().trim_end(),
            format!("{input}X")
        );
    }
}

#[test]
fn terminal_unicode_pi_footer_survives_streaming_and_cursor_updates() {
    let prefix = "· Mimo cache 43/44·2.06M/2.25M 91.5% ";
    let footer = format!("{prefix}⚠️ compat");
    for split in 0..=footer.len() {
        let (tx, _rx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &Size { cols: 80, rows: 3 }, Listener(tx));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, &footer.as_bytes()[..split]);
        parser.advance(&mut term, &footer.as_bytes()[split..]);
        let warning_column = prefix.chars().count();
        let warning = &term.grid()[Point::new(Line(0), Column(warning_column))];
        assert_eq!(warning.c, '⚠');
        assert!(warning.flags.contains(Flags::WIDE_CHAR), "split {split}");
        assert!(
            term.grid()[Point::new(Line(0), Column(warning_column + 1))]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
        );
        assert_eq!(term.grid().cursor.point.column.0, warning_column + 9);
        // Pi's next absolute cursor update must land after the same two
        // columns the terminal consumed, without leaving stale text.
        parser.advance(
            &mut term,
            format!("\x1b[1;{}Hready\x1b[K", warning_column + 4).as_bytes(),
        );
        for (i, ch) in "ready".chars().enumerate() {
            assert_eq!(
                term.grid()[Point::new(Line(0), Column(warning_column + 3 + i))].c,
                ch
            );
        }
        term.selection = Some(Selection::new(
            SelectionType::Lines,
            Point::new(Line(0), Column(0)),
            Side::Left,
        ));
        assert_eq!(
            term.selection_to_string().unwrap().trim_end(),
            format!("{prefix}⚠️ ready")
        );
    }
}

#[test]
fn terminal_unicode_emoji_handles_row_edges_scrolling_insert_and_overwrite() {
    for (input, row, col) in [
        ("abcd⚠️X", 0, 4),
        ("abcde⚠️X", 1, 0),
        ("\r\nabcde⚠️X", 1, 0),
        ("abcdef\x1b[1;2H\x1b[4h⚠️", 0, 1),
        ("ab中XY\x1b[1;2H⚠️", 0, 1),
        ("⚠️\u{FE0F}", 0, 0),
    ] {
        let (tx, _rx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &Size { cols: 6, rows: 2 }, Listener(tx));
        let mut parser: Processor = Processor::new();
        // Split every UTF-8 byte to exercise decoding and late selectors.
        for byte in input.as_bytes() {
            parser.advance(&mut term, std::slice::from_ref(byte));
        }
        let base = &term.grid()[Point::new(Line(row), Column(col))];
        assert_eq!(base.c, '⚠', "{input:?}");
        assert!(base.flags.contains(Flags::WIDE_CHAR), "{input:?}");
        let spacer = &term.grid()[Point::new(Line(row), Column(col + 1))];
        assert_eq!(spacer.c, ' ');
        assert!(spacer.flags.contains(Flags::WIDE_CHAR_SPACER));
        assert!(spacer.zerowidth().unwrap_or_default().is_empty());
        if input.contains("abcde⚠") {
            let placeholder = &term.grid()[Point::new(Line(0), Column(5))];
            assert_eq!(placeholder.c, ' ');
            assert!(
                placeholder
                    .flags
                    .contains(Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE)
            );
        }
        if input.contains("[4h") {
            assert_eq!(term.grid()[Point::new(Line(0), Column(3))].c, 'b');
        }
        if input.contains('中') {
            assert!(
                !term.grid()[Point::new(Line(0), Column(3))]
                    .flags
                    .contains(Flags::WIDE_CHAR_SPACER)
            );
        }
    }
    let (tx, _rx) = mpsc::channel();
    let mut term = Term::new(Config::default(), &Size { cols: 6, rows: 2 }, Listener(tx));
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, "\x1b[?7labcde⚠️".as_bytes());
    assert_eq!(term.grid().cursor.point, Point::new(Line(0), Column(5)));
    assert!(
        !term.grid()[Point::new(Line(0), Column(5))]
            .flags
            .contains(Flags::WIDE_CHAR)
    );
}

#[test]
fn closing_tabs_preserves_selection_or_selects_a_neighbor() {
    assert_eq!(active_after_close(2, 0, 3), 1);
    assert_eq!(active_after_close(1, 3, 3), 1);
    assert_eq!(active_after_close(1, 1, 3), 1);
    assert_eq!(active_after_close(3, 3, 3), 2);
    assert_eq!(active_after_close(0, 0, 0), 0);
}

#[test]
#[cfg(unix)]
fn background_sessions_process_output_and_survive_another_tab_closing() {
    let args = Args {
        ssh: None,
        config: None,
        log_filter: None,
        shell: Some("/bin/sh".into()),
    };
    let first = TerminalSession::new(&args, None).unwrap();
    let second = TerminalSession::new(&args, None).unwrap();
    let mut app = TerminalApp {
        window_title: String::new(),
        tabs: vec![TerminalTab::new(first), TerminalTab::new(second)],
        active: 0,
        args,
        log_directory: std::env::temp_dir(),
        warning: None,
        error: None,
        about_open: false,
        about_licenses: false,
        settings_open: false,
        theme: ThemeMode::Dark,
        cursor_settings: CursorSettings::default(),
    };
    // Set different titles through each real PTY; the background session
    // must parse output too, without changing the foreground session title.
    app.tabs[0]
        .focused_mut()
        .write(b"printf '\\033]0;first-session\\007'\n");
    app.tabs[1]
        .focused_mut()
        .write(b"printf '\\033]0;background-session\\007'\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(app.tabs[0].poll());
        assert!(app.tabs[1].poll());
        if app.tabs[0].focused().title == "first-session"
            && app.tabs[1].focused().title == "background-session"
        {
            break;
        }
        assert!(Instant::now() < deadline, "PTY titles were not processed");
        std::thread::sleep(Duration::from_millis(10));
    }
    app.close_tab(0);
    assert_eq!(app.active, 0);
    assert_eq!(app.tabs[0].focused().title, "background-session");
    assert!(
        app.tabs[0]
            .focused_mut()
            .child
            .try_wait()
            .unwrap()
            .is_none()
    );
    app.new_tab();
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active, 1);
    app.args.shell = Some("/yiterm-nonexistent-shell".into());
    app.new_tab();
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.active, 1);
    assert!(app.error.is_some());
}

#[test]
fn terminal_links_exclude_following_prose() {
    let url = "https://platform.worldquantbrain.com/alpha/xA1MlEam";
    for suffix in [
        ") (TO 14.73%",
        ")(TO 14.73%",
        ")（TO 14.73%",
        "）作为备选",
        "].",
    ] {
        let (tx, _rx) = mpsc::channel();
        let mut term = Term::new(Config::default(), &Size { cols: 20, rows: 8 }, Listener(tx));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, format!("({url}{suffix}").as_bytes());
        let link = terminal_link_details(&term, Point::new(Line(0), Column(5))).unwrap();
        assert_eq!(link.url, url, "suffix: {suffix}");
        assert_eq!(link.points.len(), url.len());
        let end = url.len() + 1;
        assert!(
            terminal_link(&term, Point::new(Line((end / 20) as i32), Column(end % 20))).is_none()
        );
    }
    assert_eq!(
        link_text("https://example.com/a(b)?x=[1] rest"),
        "https://example.com/a(b)?x=[1]"
    );
    assert_eq!(
        link_text("https://example.com/a(b))."),
        "https://example.com/a(b)"
    );
}

#[test]
fn terminal_links_handle_wrap_punctuation_and_osc8() {
    let (tx, _rx) = mpsc::channel();
    let mut term = Term::new(Config::default(), &Size { cols: 20, rows: 6 }, Listener(tx));
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, b"(https://example.com/a(b)).");
    assert_eq!(
        terminal_link(&term, Point::new(Line(1), Column(1))).as_deref(),
        Some("https://example.com/a(b)")
    );
    let link = terminal_link_details(&term, Point::new(Line(1), Column(1))).unwrap();
    assert_eq!(link.points.first(), Some(&Point::new(Line(0), Column(1))));
    assert_eq!(link.points.last(), Some(&Point::new(Line(1), Column(4))));
    assert_eq!(link.points.len(), "https://example.com/a(b)".len());
    assert!(terminal_link(&term, Point::new(Line(0), Column(0))).is_none());
    assert!(terminal_link(&term, Point::new(Line(1), Column(5))).is_none());
    parser.advance(
        &mut term,
        b"\r\n\x1b]8;;https://example.org\x1b\\label\x1b]8;;\x1b\\",
    );
    assert_eq!(
        terminal_link(&term, Point::new(Line(2), Column(0))).as_deref(),
        Some("https://example.org")
    );
    let link = terminal_link_details(&term, Point::new(Line(2), Column(2))).unwrap();
    assert_eq!(link.points.len(), 5);
    assert_eq!(link.points.first(), Some(&Point::new(Line(2), Column(0))));
    assert_eq!(link.points.last(), Some(&Point::new(Line(2), Column(4))));
    assert!(browser_url("javascript:alert(1)").is_none());
    assert!(browser_url("file:///tmp/test").is_none());
}

#[test]
fn terminal_parses_output_and_answers_queries() {
    let (tx, rx) = mpsc::channel();
    let mut term = Term::new(
        Config::default(),
        &Size { cols: 80, rows: 24 },
        Listener(tx),
    );
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, b"hello\x1b[6n");
    assert_eq!(
        term.grid()[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)].c,
        'h'
    );
    assert!(
        rx.try_iter()
            .any(|e| matches!(e, Event::PtyWrite(text) if text == "\x1b[1;6R"))
    );
}

#[test]
fn real_pty_runs_shell_and_receives_output() {
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = if cfg!(windows) {
        let mut command = CommandBuilder::new("powershell.exe");
        command.args(["-NoProfile", "-Command", "Write-Output yiterm-pty-ok"]);
        command
    } else {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "printf yiterm-pty-ok"]);
        command
    };
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => output.extend_from_slice(&buffer[..n]),
            }
            // ConPTY may keep its output pipe open after the child exits.
            if String::from_utf8_lossy(&output).contains("yiterm-pty-ok") {
                break;
            }
        }
        let _ = tx.send(output);
    });
    let output = match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(output) => output,
        Err(error) => {
            let _ = child.kill();
            panic!("PTY output timed out: {error}");
        }
    };
    assert!(child.wait().unwrap().success());
    assert!(String::from_utf8_lossy(&output).contains("yiterm-pty-ok"));
}

#[test]
fn osc52_copy_request_reaches_the_host_clipboard() {
    let args = Args {
        ssh: None,
        config: None,
        log_filter: None,
        shell: None,
    };
    let mut session = TerminalSession::new(&args, None).unwrap();
    session
        .parser
        .advance(&mut session.term, b"\x1b]52;c;SGVsbG8=\x07");
    assert!(session.poll());

    let mut copied = None;
    session.flush_clipboard_with(|text| {
        copied = Some(text);
        Ok(())
    });
    assert_eq!(copied.as_deref(), Some("Hello"));
    assert_eq!(session.status, "Copied 5 chars to host clipboard");

    session.pending_clipboard = Some("again".into());
    session.flush_clipboard_with(|_| Err(anyhow::anyhow!("clipboard unavailable")));
    assert_eq!(
        session.status,
        "Host clipboard write failed: clipboard unavailable"
    );
}
