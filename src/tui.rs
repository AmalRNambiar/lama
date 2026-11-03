//! Terminal lifecycle and the central event loop.
//!
//! A single-task UI loop with `tick_animation` for marching-ant edge animation,
//! over one session ([`run`]) or several on a shared plane ([`run_plane`]).
//! Installs mouse capture and a panic hook that also disables mouse capture
//! (ratatui's default hook restores the screen but not mouse capture). Draws
//! every iteration so animation never freezes; drains both channels after the
//! `select!` so input never lags.

use std::io::stdout;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::handler;
use crate::state::App;
use crate::state::plane::{Chip, Group, Plane};
use crate::tailer::{TailRequest, UiEvent};
use crate::ui;

/// Frame/tick cadence. 16 ms ≈ 60 fps so marching-ant edges stay smooth.
const TICK: Duration = Duration::from_millis(16);

/// Interval for re-deriving time-based agent status (see `App::status_tick`).
const STATUS_TICK: Duration = Duration::from_secs(1);

/// One session's feed: its app, the request sender that keeps its tailer
/// alive, and the batches that tailer sends back.
pub struct Pane {
    pub app: App,
    // Held for the run only to keep the request channel open: the tailer treats a
    // closed channel as "exit", so dropping this sender would kill tailing. No
    // input path sends on it (auto-switch is internal to the tailer).
    pub tail_tx: mpsc::Sender<TailRequest>,
    pub ui_rx: mpsc::Receiver<UiEvent>,
}

/// Crossterm event reader → unbounded channel (input must never block the
/// tailer's bounded channel, and bursts of mouse motion must not be dropped).
fn spawn_input() -> mpsc::UnboundedReceiver<crossterm::event::Event> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut reader = crossterm::event::EventStream::new();
        while let Some(Ok(event)) = reader.next().await {
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });
    event_rx
}

/// A session the ask watcher checks for a permission dialog.
#[derive(Clone)]
struct Watch {
    sid: String,
    provider: crate::provider::Provider,
    cwd: Option<std::path::PathBuf>,
    born: Option<std::time::SystemTime>,
    /// Check its screen for a permission dialog (no console open: an open
    /// console watches its own).
    ask: bool,
}

/// Checks sessions without an open console for a permission dialog, off the
/// UI thread (finding a window and reading its screen run `kitten`).
struct AskWatcher {
    jobs: std::sync::Arc<std::sync::Mutex<Vec<Watch>>>,
    asking: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Each watched session's latest turn: working or done.
    turns: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, crate::provider::conversation::TurnState>,
        >,
    >,
    /// Each watched session's context fill and cost so far.
    usage: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, crate::state::usage::UsageSummary>>,
    >,
}

impl AskWatcher {
    fn start() -> AskWatcher {
        let w = AskWatcher {
            jobs: Default::default(),
            asking: Default::default(),
            turns: Default::default(),
            usage: Default::default(),
        };
        let (jobs, asking, turns, usage) = (
            w.jobs.clone(),
            w.asking.clone(),
            w.turns.clone(),
            w.usage.clone(),
        );
        std::thread::spawn(move || {
            // Transcript paths, found once per session.
            let mut paths: std::collections::HashMap<String, std::path::PathBuf> =
                Default::default();
            // Context and cost, read incrementally per transcript.
            let mut meters: std::collections::HashMap<String, crate::provider::usage::UsageMeter> =
                Default::default();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let list = jobs.lock().map(|j| j.clone()).unwrap_or_default();
                let found: std::collections::HashSet<String> = list
                    .iter()
                    .filter(|j| j.ask)
                    .filter(|j| {
                        crate::kitty::asking(j.provider, &j.sid, j.cwd.as_deref(), j.born).is_some()
                    })
                    .map(|j| j.sid.clone())
                    .collect();
                let mut states = std::collections::HashMap::new();
                let mut summaries = std::collections::HashMap::new();
                for j in &list {
                    if !paths.contains_key(&j.sid)
                        && let Ok(s) = crate::provider::open(
                            &crate::provider::Target::Id(j.sid.clone()),
                            Some(j.provider),
                        )
                    {
                        paths.insert(j.sid.clone(), s.root.path);
                    }
                    if let Some(state) = paths
                        .get(&j.sid)
                        .and_then(|p| crate::provider::conversation::turn_state(j.provider, p))
                    {
                        states.insert(j.sid.clone(), state);
                    }
                    if let Some(path) = paths.get(&j.sid) {
                        let meter = meters.entry(j.sid.clone()).or_insert_with(|| {
                            crate::provider::usage::UsageMeter::new(j.provider, path.clone())
                        });
                        summaries.insert(j.sid.clone(), meter.refresh());
                    }
                }
                if let Ok(mut a) = asking.lock() {
                    *a = found;
                }
                if let Ok(mut t) = turns.lock() {
                    *t = states;
                }
                // Merged, not replaced: a session scrolled out of view still
                // counts toward today's total.
                if let Ok(mut u) = usage.lock() {
                    u.extend(summaries);
                }
            }
        });
        w
    }

    /// Hand over the sessions to watch: every shown one without a console
    /// (an open console watches its own screen).
    fn set(&self, plane: &Plane, apps: &[App]) {
        let list: Vec<Watch> = plane
            .shown(apps)
            .into_iter()
            .filter_map(|i| {
                let app = &apps[i];
                let born = app
                    .session
                    .spawn_order()
                    .next()
                    .and_then(|r| app.session.agent(r))
                    .and_then(|a| a.first_ts)
                    .map(std::time::SystemTime::from);
                Some(Watch {
                    sid: app.current_session_id.clone(),
                    provider: app.session.provider()?,
                    cwd: plane.session_cwd(i).map(|p| p.to_path_buf()),
                    born,
                    ask: !plane.consoles.contains_key(&app.current_session_id),
                })
            })
            .collect();
        if let Ok(mut j) = self.jobs.lock() {
            *j = list;
        }
    }

    fn asking(&self) -> std::collections::HashSet<String> {
        self.asking.lock().map(|a| a.clone()).unwrap_or_default()
    }

    fn turns(&self) -> std::collections::HashMap<String, crate::provider::conversation::TurnState> {
        self.turns.lock().map(|t| t.clone()).unwrap_or_default()
    }

    fn usage(&self) -> std::collections::HashMap<String, crate::state::usage::UsageSummary> {
        self.usage.lock().map(|u| u.clone()).unwrap_or_default()
    }
}

/// What the universe's sweep reports to the plane.
pub enum Found {
    /// A session not seen before, with its project.
    Session(Box<Pane>, Group),
    /// Every session whose agent process is running, by session id.
    Alive(std::collections::HashSet<String>),
}

/// Run the TUI to completion over a single session.
///
/// Owns the terminal, spawns the crossterm `EventStream` reader into an
/// unbounded channel, ticks at 16 ms, and routes UI events / input each
/// iteration. Returns when the user quits.
pub async fn run(
    mut app: App,
    _tail_tx: mpsc::Sender<TailRequest>,
    mut ui_rx: mpsc::Receiver<UiEvent>,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    install_panic_hook();
    let mut event_rx = spawn_input();

    let mut tick = tokio::time::interval(TICK);
    let mut last_tick = Instant::now();
    let mut last_status_tick = Instant::now();

    // Recording-only scripted pointer (see crate::autopilot). Off unless
    // LAMA_DEMO=1, so this is inert for every real user. Armed at startup
    // but only fired by the trigger key, so the tape picks the moment — the
    // waypoints are read off the laid-out graph at that instant.
    let demo = crate::autopilot::requested();
    let mut pilot: Option<crate::autopilot::Autopilot> = None;

    let result = loop {
        // Advance animation/auto-pan EVERY iteration before drawing — otherwise
        // marching-ant edges freeze (the tick_auto_pan return is ignored).
        let now = Instant::now();
        let elapsed = now - last_tick;
        let _ = app.flow.tick_auto_pan(elapsed);
        app.flow.tick_animation(elapsed);
        app.tick_camera(elapsed);
        // Advance the replay playhead (paces replay; no-op while following a
        // live edge, where folding happens as Batches arrive).
        app.tick_timeline(elapsed);
        last_tick = now;

        // Drive the scripted pointer through the REAL event path, so hit
        // testing, the drag threshold, and the scrubber intercept all run.
        if let Some(p) = pilot.as_mut() {
            for ev in p.tick(elapsed) {
                handler::handle_event(&ev, &mut app);
            }
        }

        // Interactive liveness (main/forks) is time-derived: a quiet session
        // produces no batches, so running→idle transitions need their own
        // clock. ~1s granularity is plenty for a minutes-scale idle window.
        if now - last_status_tick >= STATUS_TICK {
            app.status_tick();
            last_status_tick = now;
        }

        // Draw at the top of the loop, every iteration, so animation is smooth
        // and state changes from the previous iteration are reflected.
        let cursor = pilot.as_ref().map(|p| p.cell());
        if let Err(e) = terminal.draw(|frame| {
            ui::draw(frame, &mut app);
            // Painted after the UI so the pointer sits above what it points at.
            if let (Some(p), Some(_)) = (pilot.as_ref(), cursor) {
                p.draw(frame.buffer_mut());
            }
        }) {
            break Err(e.into());
        }

        tokio::select! {
            _ = tick.tick() => {}
            Some(ev) = ui_rx.recv() => app.handle_ui_event(ev),
            Some(ev) = event_rx.recv() => {
                if route(&ev, &mut app, &mut pilot, demo) {
                    break Ok(());
                }
            }
        }

        // Drain both channels after the select so neither input nor tailer
        // batches lag behind a single per-frame wakeup.
        let mut quit = false;
        while let Ok(ev) = event_rx.try_recv() {
            if route(&ev, &mut app, &mut pilot, demo) {
                quit = true;
                break;
            }
        }
        while let Ok(ev) = ui_rx.try_recv() {
            app.handle_ui_event(ev);
        }

        if quit || app.should_quit {
            break Ok(());
        }
    };

    // Clean restore regardless of how the loop ended.
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Run the shared plane: several live sessions on one canvas under a
/// project hub (see [`crate::state::plane`]).
///
/// Each session's app folds its own tailer's batches as when it runs alone;
/// the plane re-projects them into the host flow whenever one changes, and
/// once a second so cards keep ticking. All input goes to the host, so the
/// camera, selection and keys act on the combined graph.
///
/// `incoming` carries what the universe's sweep finds after launch: new
/// sessions, each joining the plane under its project, and which sessions
/// have their agent process running right now.
pub async fn run_plane(
    mut plane: Plane,
    panes: Vec<(Pane, Group)>,
    mut incoming: Option<mpsc::UnboundedReceiver<Found>>,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    // Lets Shift+Enter reach the intake as its own key (kitty's keyboard
    // protocol); terminals without it ignore the request.
    let _ = execute!(
        stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );
    install_panic_hook();
    crate::registry::spawn_observer();
    let mut event_rx = spawn_input();

    // Fan every tailer's batches into one channel tagged with the session's
    // index, so the loop selects over a single receiver.
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<(usize, UiEvent)>();
    let mut apps: Vec<App> = Vec::with_capacity(panes.len());
    let mut _tail_txs = Vec::with_capacity(panes.len());
    // Join a session: its batches are forwarded tagged with its index.
    let mut join = |pane: Pane, group: Group, apps: &mut Vec<App>, plane: &mut Plane| {
        let i = apps.len();
        let Pane {
            app,
            tail_tx,
            mut ui_rx,
        } = pane;
        let ui_tx = ui_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = ui_rx.recv().await {
                if ui_tx.send((i, ev)).is_err() {
                    break;
                }
            }
        });
        apps.push(app);
        _tail_txs.push(tail_tx);
        plane.add_session(group);
    };
    for (pane, group) in panes {
        join(pane, group, &mut apps, &mut plane);
    }
    let (pins, consoles, placed) = load_layout();
    if plane.window.is_some() {
        plane.pins = pins;
        plane.placed = placed;
    }
    plane.consoles = consoles;

    let mut tick = tokio::time::interval(TICK);
    let mut last_tick = Instant::now();
    let mut last_status_tick = Instant::now();
    let mut dirty = true;
    let mut input = PlaneInput {
        news: crate::news::Dropper::new(),
        ..Default::default()
    };
    // Tasks still under way when lama last closed pick back up.
    if plane.window.is_some() {
        for store in crate::task::run::recent() {
            input.tasks.extend(crate::task::run::Runner::open(
                store,
                known_projects(&plane),
            ));
        }
    }
    // The lamaverse always shows the intake, ready for the next task.
    if plane.window.is_some() && input.intake.is_none() {
        input.intake = Some(Default::default());
    }
    let watcher = AskWatcher::start();
    // Which agents there are, found while the first frames draw.
    crate::agents::warm();
    // The theme you switched to last time (`p`), unless LAMA_THEME pins one.
    if let Some(skin) = crate::config::get("theme").and_then(|t| crate::ui::fx::Skin::named(&t)) {
        crate::ui::fx::set_skin(skin);
    }

    // The skin the canvases were last coloured for: each flow keeps its own
    // palette, so a switch (or the saved one, loaded above) re-themes them.
    let mut themed: Option<crate::ui::fx::Skin> = None;

    let result = loop {
        let skin = crate::ui::fx::skin();
        if themed != Some(skin) {
            let theme = rataflow::Theme::Custom(crate::ui::fx::palette());
            plane.host.flow.theme = theme;
            for app in &mut apps {
                app.flow.theme = theme;
            }
            themed = Some(skin);
        }
        let now = Instant::now();
        let elapsed = now - last_tick;
        for app in &mut apps {
            app.tick_timeline(elapsed);
        }
        let host = &mut plane.host;
        let _ = host.flow.tick_auto_pan(elapsed);
        host.flow.tick_animation(elapsed);
        host.tick_camera(elapsed);
        plane.tick_motion(elapsed);
        last_tick = now;

        if now - last_status_tick >= STATUS_TICK {
            for app in &mut apps {
                app.status_tick();
            }
            // An agent waiting on you gets its console, in place.
            watcher.set(&plane, &apps);
            plane.usage = watcher.usage();
            input.expand_asking(&watcher.asking(), &mut plane, &apps);
            let finished = input.track_turns(&watcher.turns(), &mut plane);
            input.track_asking(&watcher.asking());
            // The lama goes to see: the one waiting on you, the ones done.
            plane.asking = input.waiting.clone();
            plane.mascot.cheer(finished);
            plane.sync(&apps, true);
            dirty = false;
            last_status_tick = now;
        }
        if dirty {
            plane.sync(&apps, false);
            dirty = false;
        }
        plane.follow(&apps);
        input.adopt_launched(&mut plane, &apps);
        input.task_frame(&mut plane, &apps);
        input.sync_consoles(&plane, &apps);
        for c in input.consoles.values_mut() {
            c.tick();
        }
        if let Some(d) = input.drawer.as_mut() {
            d.refresh();
        }
        let quiet = input.waiting.is_empty()
            && input.paper.is_none()
            && input.composer.is_none()
            && input.drawer.is_none()
            && !input.consoles.values().any(|c| c.needs_approval());
        input.news.tick(quiet);

        let drawn = terminal.draw(|frame| {
            let mut overlay =
                |f: &mut ratatui::Frame, canvas: ratatui::layout::Rect, plane: &Plane| {
                    input.draw(f, canvas, plane);
                    if let Some(c) = input.composer.as_ref() {
                        c.render(f.buffer_mut(), canvas);
                    }
                    if let Some(d) = input.drawer.as_ref() {
                        d.render(f.buffer_mut(), canvas);
                    }
                    // The page covers the canvas; the timeline stays below.
                    if let Some(p) = input.paper.as_ref() {
                        p.render(f.buffer_mut(), canvas);
                    }
                };
            ui::plane::draw(frame, &mut plane, &mut apps, Some(&mut overlay));
        });
        if let Err(e) = drawn {
            break Err(e.into());
        }
        if let Some((sid, text)) = input.to_copy.take() {
            let note = match crate::clipboard::copy(&text) {
                Ok(()) => (format!("✓ copied {} chars", text.chars().count()), true),
                Err(e) => (format!("✗ copy failed: {e}"), false),
            };
            if let Some(c) = input.consoles.get_mut(&sid) {
                c.say(note.0, note.1);
            }
        }

        tokio::select! {
            _ = tick.tick() => {}
            Some((i, ev)) = ui_rx.recv() => {
                apps[i].handle_ui_event(ev);
                dirty = true;
            }
            Some(ev) = event_rx.recv() => {
                if route_plane(&ev, &mut plane, &mut input, &apps) {
                    break Ok(());
                }
                dirty = true;
            }
        }
        if let Some(rx) = incoming.as_mut() {
            while let Ok(found) = rx.try_recv() {
                match found {
                    Found::Session(pane, group) => join(*pane, group, &mut apps, &mut plane),
                    Found::Alive(ids) => plane.alive = ids,
                }
                dirty = true;
            }
        }

        // Drain both channels after the select so neither input nor tailer
        // batches lag behind a single per-frame wakeup.
        let mut quit = false;
        while let Ok(ev) = event_rx.try_recv() {
            if route_plane(&ev, &mut plane, &mut input, &apps) {
                quit = true;
                break;
            }
            dirty = true;
        }
        while let Ok((i, ev)) = ui_rx.try_recv() {
            apps[i].handle_ui_event(ev);
            dirty = true;
        }

        if plane.pins_dirty {
            plane.pins_dirty = false;
            save_layout(&plane.pins, &plane.consoles, &plane.placed);
        }

        if quit || plane.host.should_quit {
            break Ok(());
        }
    };

    let _ = execute!(
        stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    result
}

/// Route one input event on the plane, returning whether to quit. The plane's
/// own keys (project jumps, expand) come first; the rest is the host's.
fn route_plane(
    event: &crossterm::event::Event,
    plane: &mut Plane,
    input: &mut PlaneInput,
    apps: &[App],
) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
    if matches!(event, Event::Key(_))
        || matches!(event, Event::Mouse(m) if m.kind != MouseEventKind::Moved)
    {
        input.news.touched();
    }
    // An open newspaper page takes every key and click until it closes.
    if let Some(paper) = input.paper.as_mut() {
        match event {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                if k.code == KeyCode::Char('c')
                    && k.modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                {
                    return true;
                }
                if let crate::news::PaperAction::Close = paper.key(k) {
                    input.paper = None;
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollDown => paper.turn(true),
                MouseEventKind::ScrollUp => paper.turn(false),
                MouseEventKind::Down(MouseButton::Left) => {
                    let r = crate::news::Paper::rect(input.canvas);
                    let inside = m.column >= r.x
                        && m.column < r.right()
                        && m.row >= r.y
                        && m.row < r.bottom();
                    if inside {
                        paper.turn(true);
                    } else {
                        input.paper = None;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        return false;
    }
    let Event::Mouse(m) = event else {
        return route_plane_inner(event, plane, input, apps);
    };
    // Caught a falling envelope: its story opens.
    if m.kind == MouseEventKind::Down(MouseButton::Left)
        && input.console_at(m.column, m.row).is_none()
        && let Some(story) = input.news.catch(input.canvas, m.column, m.row)
    {
        input.paper = Some(crate::news::Paper::new(story));
        return false;
    }
    if m.kind == MouseEventKind::Down(MouseButton::Left) && input.task_click(m.column, m.row, plane)
    {
        return false;
    }
    // The wheel over the intake scrolls its text.
    if matches!(
        m.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) && plane.task_hits.intake.is_some_and(|r| {
        m.column >= r.x && m.column < r.right() && m.row >= r.y && m.row < r.bottom()
    }) && let Some(i) = input.intake.as_mut()
    {
        i.scroll = if m.kind == MouseEventKind::ScrollUp {
            i.scroll + 1
        } else {
            i.scroll.saturating_sub(1)
        };
        return false;
    }
    let on_rail = plane
        .strip_area
        .is_some_and(|r| m.row >= r.y && m.row < r.bottom());
    let on_console = input.console_at(m.column, m.row).is_some();
    // The wheel over the canvas zooms, eased, around the pointer (over a
    // console it scrolls the console, below).
    if matches!(
        m.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) && !on_rail
        && !on_console
    {
        plane.wheel_zoom(m.kind == MouseEventKind::ScrollUp, m.column, m.row);
        return false;
    }
    if m.kind == MouseEventKind::Down(MouseButton::Left) {
        plane.halt();
        // A press anywhere lets go of the last selection.
        input.selection = None;
        // A press on a finished session — its card or its console — is
        // looking at it: the green blink stops.
        let sid = input.console_at(m.column, m.row).or_else(|| {
            let id = plane.node_under(m.column, m.row)?;
            let (i, _) = Plane::resolve(&id)?;
            apps.get(i).map(|a| a.current_session_id.clone())
        });
        if let Some(sid) = sid {
            plane.acknowledge(&sid);
        }
    }
    if input.touch(m, plane) {
        return false;
    }
    let v = plane.host.flow.viewport;
    let quit = route_plane_inner(event, plane, input, apps);
    match m.kind {
        MouseEventKind::Drag(MouseButton::Left) => {
            plane.smooth_pan((v.x, v.y));
            plane.track_pan();
        }
        MouseEventKind::Up(MouseButton::Left) => plane.release_pan(),
        _ => {}
    }
    quit
}

fn route_plane_inner(
    event: &crossterm::event::Event,
    plane: &mut Plane,
    input: &mut PlaneInput,
    apps: &[App],
) -> bool {
    use crossterm::event::{
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    };
    // Ctrl-C over a selection copies it (again), rather than quitting.
    if let Event::Key(key) = event
        && key.kind != KeyEventKind::Release
        && key.code == KeyCode::Char('c')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && let Some(sel) = input.selection.take()
        && let Some(text) = sel.text
    {
        input.to_copy = Some((sel.session, text));
        return false;
    }
    // A terminal paste (ctrl+shift+v) lands in the focused console's prompt.
    if let Event::Paste(text) = event
        && input.intake.as_ref().is_none_or(|i| !i.focused)
        && let Some(console) = input.focused_mut()
    {
        console.paste(text);
        return false;
    }
    // An open composer owns the keyboard, then a focused console. Ctrl-C
    // still quits.
    if let Event::Key(key) = event
        && input.composer.is_some()
    {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        input.composer_key(key, plane);
        return false;
    }
    // So does the sessions drawer.
    if let Event::Key(key) = event
        && let Some(d) = input.drawer.as_mut()
    {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match d.key(key) {
            crate::sessions::Action::None => {}
            crate::sessions::Action::Close => input.drawer = None,
            crate::sessions::Action::Show(sid) => {
                if let Some(i) = apps.iter().position(|a| a.current_session_id == sid) {
                    input.drawer = None;
                    input.open_console(i, plane, apps);
                }
            }
        }
        return false;
    }
    // A focused intake owns the keyboard (and pastes), like a console.
    if input.intake.as_ref().is_some_and(|i| i.focused) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return true;
                }
                input.intake_key(key, plane, apps);
                return false;
            }
            Event::Paste(text) => {
                if let Some(i) = input.intake.as_mut() {
                    // Keep its lines: a pasted ticket or thread reads as written.
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    i.draft.push_str(&text.replace('\t', "    "));
                    i.scroll = 0;
                }
                return false;
            }
            _ => {}
        }
    }
    if let Event::Key(key) = event
        && input.focus.is_none()
        && key.kind != KeyEventKind::Release
        && plane.window.is_some()
        && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
    {
        input.intake.get_or_insert_with(Default::default).focused = true;
        return false;
    }
    if let Event::Key(key) = event
        && input.focus.is_none()
        && key.kind != KeyEventKind::Release
        && matches!(key.code, KeyCode::Char('h') | KeyCode::Char('H'))
    {
        input.drawer = Some(crate::sessions::Drawer::new());
        return false;
    }
    if let Event::Key(key) = event
        && input.focus.is_none()
        && key.kind != KeyEventKind::Release
        && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
    {
        input.open_composer(plane);
        return false;
    }
    if let Event::Mouse(m) = event
        && m.kind == MouseEventKind::Down(MouseButton::Left)
        && plane
            .new_button
            .is_some_and(|b| m.column >= b.x && m.column < b.right() && m.row == b.y)
    {
        input.open_composer(plane);
        return false;
    }
    if let Event::Key(key) = event
        && let Some(console) = input.focused_mut()
    {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if let crate::console::Action::Blur = console.key(key) {
            input.focus = None;
        }
        if let Some(sid) = input.focus.clone() {
            plane.acknowledge(&sid);
        }
        return false;
    }
    // The mouse over the console is the console's: the wheel scrolls its
    // history, and clicks don't fall through to the canvas behind it.
    // A console being resized by its grip takes the drag wherever it goes.
    if let Event::Mouse(m) = event
        && let Some(r) = input.resizing.clone()
    {
        match m.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                let zoom = plane.host.flow.viewport.zoom.max(0.05);
                let size = (
                    r.size.0 + (m.column as f64 - r.from.0 as f64) / zoom,
                    r.size.1 + (m.row as f64 - r.from.1 as f64) / zoom,
                );
                plane.resize_console(r.session, size);
            }
            MouseEventKind::Up(_) => {
                input.resizing = None;
                plane.capture_console_sizes(apps);
            }
            _ => {}
        }
        return false;
    }
    if let Event::Mouse(m) = event
        && let Some(sid) = input.console_at(m.column, m.row)
    {
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some(c) = input.consoles.get_mut(&sid) {
                    c.wheel(m.kind == MouseEventKind::ScrollUp);
                }
                return false;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let close = input
                    .consoles
                    .get(&sid)
                    .is_some_and(|c| c.close_hit(m.column, m.row));
                if close {
                    if let Some(c) = input.consoles.remove(&sid) {
                        if c.needs_approval() {
                            input.dismissed.insert(sid.clone());
                        }
                        plane.close_console(c.session, apps);
                    }
                    if input.focus.as_deref() == Some(sid.as_str()) {
                        input.focus = None;
                    }
                    return false;
                }
                let grip = input
                    .consoles
                    .get(&sid)
                    .filter(|c| c.grip_hit(m.column, m.row));
                if let Some(c) = grip
                    && let Some(size) = plane.console_size(c.session)
                {
                    input.resizing = Some(Resizing {
                        session: c.session,
                        from: (m.column, m.row),
                        size,
                    });
                    input.focus = Some(sid);
                    return false;
                }
                // A press on a console gives it the keyboard; the press
                // itself still moves its project below.
                input.focus = Some(sid);
            }
            _ => {}
        }
    }
    if plane.window.is_some() {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                match key.code {
                    KeyCode::Char(']') => {
                        plane.step_project(true);
                        plane.sync(apps, false);
                    }
                    KeyCode::Char('[') => {
                        plane.step_project(false);
                        plane.sync(apps, false);
                    }
                    KeyCode::Enter => {
                        if !input.open_selected(plane, apps)
                            && let Some(item) = plane.rail.get(plane.cursor)
                        {
                            plane.open_project(item.group);
                        }
                    }
                    KeyCode::Char('o') | KeyCode::Char('O') => plane.go_home(),
                    KeyCode::Char('e') | KeyCode::Char('E') => plane.expand_all = !plane.expand_all,
                    // The host's own relayout would stack every session in
                    // one tree; the universe tidies them by project instead.
                    KeyCode::Char('r') | KeyCode::Char('R') => plane.reset_layout(),
                    KeyCode::Char('l') | KeyCode::Char('L') => {
                        plane.mascot.hidden = !plane.mascot.hidden
                    }
                    KeyCode::Char('p') | KeyCode::Char('P') => switch_theme(),
                    _ => return route_host(event, plane, input, apps),
                }
                return false;
            }
            Event::Mouse(m) => {
                if let MouseEventKind::Down(MouseButton::Left) = m.kind
                    && let Some(chip) = plane.chip_hit(m.column, m.row)
                {
                    match chip {
                        Chip::Project(g) => plane.toggle_spot(g),
                        Chip::Quiet => plane.expand_all = !plane.expand_all,
                        Chip::Theme(skin) => set_theme(skin),
                    }
                    plane.sync(apps, false);
                    return false;
                }
                if plane
                    .strip_area
                    .is_some_and(|r| m.row >= r.y && m.row < r.bottom())
                {
                    return false;
                }
                let quit = handler::handle_event(event, &mut plane.host);
                input.double_click(event, plane, apps);
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => plane.press(m.column, m.row),
                    MouseEventKind::Drag(MouseButton::Left) => plane.dragged(),
                    MouseEventKind::Up(MouseButton::Left) => {
                        plane.release();
                        plane.capture_console_sizes(apps);
                    }
                    _ => {}
                }
                plane.skip_structure(event);
                return quit;
            }
            _ => {}
        }
    }
    route_host(event, plane, input, apps)
}

/// Hand an event to the host app, keeping plane structure out of selection.
fn route_host(
    event: &crossterm::event::Event,
    plane: &mut Plane,
    input: &mut PlaneInput,
    apps: &[App],
) -> bool {
    // `r` tidies with the plane's own layout, which keeps an open console's
    // grown card in place; the host's would space every card by the largest.
    if let crossterm::event::Event::Key(k) = event
        && k.code == crossterm::event::KeyCode::Char('r')
        && k.kind != crossterm::event::KeyEventKind::Release
    {
        plane.repack = true;
        return false;
    }
    // ⏎ on a selected card (tab to it) opens its console, like a double-click.
    if let crossterm::event::Event::Key(k) = event
        && k.code == crossterm::event::KeyCode::Enter
        && k.kind != crossterm::event::KeyEventKind::Release
        && input.open_selected(plane, apps)
    {
        return false;
    }
    if handler::handle_event(event, &mut plane.host) {
        return true;
    }
    input.double_click(event, plane, apps);
    if let crossterm::event::Event::Mouse(m) = event
        && let crossterm::event::MouseEventKind::Up(_) = m.kind
    {
        plane.capture_console_sizes(apps);
    }
    plane.skip_structure(event);
    false
}

/// How close two presses must be to count as a double-click.
// Wide enough for a finger's double-tap, which runs slower than a mouse's.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// The plane's own input state: the consoles open on the canvas, which one
/// takes the keyboard, and the last press, for telling a double-click.
#[derive(Default)]
struct PlaneInput {
    /// Open consoles by session id.
    consoles: std::collections::HashMap<String, crate::console::Console>,
    /// The console typing goes to; `None` hands keys to the canvas.
    focus: Option<String>,
    last_press: Option<(String, Instant)>,
    /// A console grip drag in progress.
    resizing: Option<Resizing>,
    /// Consoles closed while their agent was asking: not reopened for that
    /// same wait.
    dismissed: std::collections::HashSet<String>,
    /// The intake is asking which agent conducts tasks (both are installed
    /// and none was picked yet); its draft is the task that waits on it.
    picking_conductor: bool,
    /// Each session's turn state when last seen, to catch working → done.
    turns_seen: std::collections::HashMap<String, crate::provider::conversation::TurnState>,
    /// Text being selected (or selected) in a console's history.
    selection: Option<Selection>,
    /// Text a finished selection picked up last frame, to copy out.
    to_copy: Option<(String, String)>,
    /// The new-session composer, while open.
    composer: Option<crate::composer::Composer>,
    /// The sessions drawer, while open.
    drawer: Option<crate::sessions::Drawer>,
    /// Sessions started from the canvas, waiting to show up so their console
    /// can open.
    launched: Vec<Launched>,
    /// The task this window runs, once one is started.
    tasks: Vec<crate::task::run::Runner>,
    /// Sessions waiting on a permission prompt, for the TASKS box.
    waiting: std::collections::HashSet<String>,
    /// The task picked in the TASKS box; `None` follows the newest.
    selected: Option<String>,
    /// The conductor question last shown, so each one takes the keyboard
    /// once.
    asked: Option<String>,
    /// The clock every task's animation times are put on.
    clock: Option<Instant>,
    /// The intake: open (`Some`) shows the task panel; its draft, focus
    /// and last notice.
    intake: Option<crate::state::task::IntakeView>,
    /// The message log drawer.
    log_open: bool,
    /// When each task agent's card first showed up (task clock), for its
    /// arrival animation.
    joined: std::collections::HashMap<String, f64>,
    /// When the task last read its directory and delivered messages.
    task_ticked: Option<Instant>,
    /// The news envelopes that fall while you're idle (`LAMA_NEWS=on`).
    news: crate::news::Dropper,
    /// A caught story, open as a newspaper page.
    paper: Option<crate::news::Paper>,
    /// Where the canvas was drawn last frame, for catching envelopes.
    canvas: ratatui::layout::Rect,
}

/// The sessions on the plane, for matching launched Codex agents.
fn seen_sessions(plane: &Plane, apps: &[App]) -> Vec<crate::task::run::Seen> {
    (0..apps.len())
        .filter_map(|i| {
            let app = &apps[i];
            let provider = app.session.provider()?;
            let cwd = plane.session_cwd(i)?.to_path_buf();
            let born = app
                .session
                .spawn_order()
                .next()
                .and_then(|a| app.session.agent(a))
                .and_then(|a| a.first_ts)?;
            let transcript = crate::provider::open(
                &crate::provider::Target::Id(app.current_session_id.clone()),
                Some(provider),
            )
            .ok()?
            .root
            .path;
            Some(crate::task::run::Seen {
                session: app.current_session_id.clone(),
                provider,
                cwd,
                born: Some(born),
                transcript,
            })
        })
        .collect()
}

/// The projects task agents may run in: the ones the lamaverse shows plus
/// every repo you have session history in, never a task's own directory.
fn known_projects(plane: &Plane) -> Vec<std::path::PathBuf> {
    // Read once, in the background: every repo with session history.
    static HISTORY: std::sync::OnceLock<std::sync::Mutex<Vec<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    let history = HISTORY.get_or_init(|| {
        std::thread::spawn(|| {
            let found = crate::task::history_projects();
            if let Some(h) = HISTORY.get()
                && let Ok(mut v) = h.lock()
            {
                *v = found;
            }
        });
        Default::default()
    });
    let tasks = crate::task::root();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut projects: Vec<std::path::PathBuf> = plane
        .groups
        .iter()
        .filter_map(|g| g.cwd.clone())
        .filter(|c| tasks.as_ref().is_none_or(|m| !c.starts_with(m)))
        // Your home or the root is everything, not a project.
        .filter(|c| c.parent().is_some() && Some(c.as_path()) != home.as_deref())
        .map(|c| crate::provider::project_dir(&c).to_path_buf())
        .chain(history.lock().map(|v| v.clone()).unwrap_or_default())
        .collect();
    projects.sort();
    projects.dedup();
    projects
}

/// A session started from the canvas: where, which agent, and when.
struct Launched {
    cwd: std::path::PathBuf,
    provider: crate::provider::Provider,
    at: chrono::DateTime<chrono::Utc>,
}

/// A mouse selection in a console, in cells relative to its top-left corner
/// (so it follows the console as the canvas moves).
struct Selection {
    session: String,
    from: (u16, u16),
    to: (u16, u16),
    /// Whether the pointer moved: a press without a drag is a tap.
    moved: bool,
    /// Released: copy what it covers on the next frame.
    copy: bool,
    /// What it covered when copied, for ctrl+c.
    text: Option<String>,
}

impl Selection {
    /// The covered cells of a `w`×`h` console, row by row: `(row, from, to)`
    /// with `to` exclusive, inside its border.
    fn spans(&self, w: u16, h: u16) -> Vec<(u16, u16, u16)> {
        let key = |p: (u16, u16)| (p.1, p.0);
        let (a, b) = if key(self.from) <= key(self.to) {
            (self.from, self.to)
        } else {
            (self.to, self.from)
        };
        let (left, right) = (1, w.saturating_sub(1));
        (a.1.max(1)..=b.1.min(h.saturating_sub(2)))
            .filter_map(|y| {
                let x0 = if y == a.1 { a.0.max(left) } else { left };
                let x1 = if y == b.1 {
                    (b.0 + 1).min(right)
                } else {
                    right
                };
                (x0 < x1).then_some((y, x0, x1))
            })
            .collect()
    }

    /// The text under the selection in `buf`, one line per row, trailing
    /// blanks dropped.
    fn text(&self, buf: &ratatui::buffer::Buffer) -> String {
        let a = buf.area;
        self.spans(a.width, a.height)
            .iter()
            .map(|&(y, x0, x1)| {
                let mut line = String::new();
                let mut x = x0;
                while x < x1 {
                    let sym = buf[(x, y)].symbol();
                    line.push_str(sym);
                    // A wide glyph's second cell is only its shadow.
                    x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
                }
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A screen cell relative to a console's top-left corner at `origin`.
fn local(origin: (i32, i32), col: u16, row: u16) -> (u16, u16) {
    (
        (col as i32 - origin.0).clamp(0, u16::MAX as i32) as u16,
        (row as i32 - origin.1).clamp(0, u16::MAX as i32) as u16,
    )
}

/// Where a console resize started: the session, the pointer, and the size.
#[derive(Clone)]
struct Resizing {
    session: usize,
    from: (u16, u16),
    size: (f64, f64),
}

impl PlaneInput {
    /// Dragging a console's history scrolls it (a touchscreen has no
    /// wheel, and a trackpad click-drag reads the same way); its header bar
    /// still moves the project. Returns whether the event was taken.
    fn touch(&mut self, m: &crossterm::event::MouseEvent, plane: &mut Plane) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        let (col, row) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(sid) = self.console_at(col, row) else {
                    return false;
                };
                // The header's stop / resume button.
                if self
                    .consoles
                    .get_mut(&sid)
                    .is_some_and(|c| c.click(col, row))
                {
                    self.focus = Some(sid);
                    return true;
                }
                // A click on one of a permission dialog's choices answers it.
                if let Some(n) = self.consoles[&sid].ask_hit(col, row) {
                    if let Some(c) = self.consoles.get_mut(&sid) {
                        c.answer(n);
                    }
                    self.focus = Some(sid);
                    return true;
                }
                let c = &self.consoles[&sid];
                if c.close_hit(col, row) || c.grip_hit(col, row) || c.on_header(col, row) {
                    return false;
                }
                let at = local(c.origin(), col, row);
                self.focus = Some(sid.clone());
                self.selection = Some(Selection {
                    session: sid,
                    from: at,
                    to: at,
                    moved: false,
                    copy: false,
                    text: None,
                });
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(s) = self
                    .selection
                    .as_mut()
                    .filter(|s| !s.copy && s.text.is_none())
                else {
                    return false;
                };
                let Some(c) = self.consoles.get(&s.session) else {
                    return false;
                };
                s.to = local(c.origin(), col, row);
                s.moved |= s.to != s.from;
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(s) = self
                    .selection
                    .as_mut()
                    .filter(|s| !s.copy && s.text.is_none())
                else {
                    return false;
                };
                if s.moved {
                    // Copied as soon as it is let go, like a terminal's own.
                    s.copy = true;
                } else {
                    // A tap, not a selection: a console small on screen
                    // zooms in.
                    let sid = s.session.clone();
                    self.selection = None;
                    if let Some(c) = self.consoles.get(&sid) {
                        plane.tap_console(c.session);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Open the composer for the project the camera is on (or the strip's
    /// highlighted one; the only one on a project plane).
    fn open_composer(&mut self, plane: &Plane) {
        let g = plane
            .focus
            .or_else(|| plane.rail.get(plane.cursor).map(|r| r.group))
            .unwrap_or(0);
        let Some(group) = plane.groups.get(g) else {
            return;
        };
        self.composer = Some(crate::composer::Composer::new(
            g,
            group.name.clone(),
            crate::agents::installed(),
        ));
    }

    fn composer_key(&mut self, key: &crossterm::event::KeyEvent, plane: &Plane) {
        use crate::composer::Action;
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        match c.key(key) {
            Action::None => {}
            Action::Cancel => self.composer = None,
            Action::Launch(prompt) => {
                let Some(cwd) = plane.groups.get(c.group).and_then(|g| g.cwd.clone()) else {
                    c.error = Some("this project's folder isn't known".into());
                    return;
                };
                let name = match c.provider {
                    crate::provider::Provider::Claude => "claude",
                    crate::provider::Provider::Codex => "codex",
                };
                let title = format!("{} · {name}", c.project);
                match crate::kitty::launch_window(&cwd, c.provider, &prompt, &title) {
                    Ok(w) => {
                        crate::registry::record_launch(c.provider, &cwd, &prompt, &title, &w);
                        self.launched.push(Launched {
                            cwd,
                            provider: c.provider,
                            at: chrono::Utc::now(),
                        });
                        self.composer = None;
                    }
                    Err(e) => c.error = Some(format!("✗ {e}")),
                }
            }
        }
    }

    /// A session started from the canvas has shown up: open its console.
    /// Waits up to three minutes for it.
    fn adopt_launched(&mut self, plane: &mut Plane, apps: &[App]) {
        let now = chrono::Utc::now();
        self.launched
            .retain(|l| now - l.at < chrono::Duration::minutes(3));
        let mut found = Vec::new();
        for (k, l) in self.launched.iter().enumerate() {
            let hit = (0..apps.len()).find(|&i| {
                let app = &apps[i];
                let cwd = plane
                    .groups
                    .get(plane.group_of(i))
                    .and_then(|g| g.cwd.as_ref());
                let born = app
                    .session
                    .spawn_order()
                    .next()
                    .and_then(|r| app.session.agent(r))
                    .and_then(|a| a.first_ts);
                cwd == Some(&l.cwd)
                    && app.session.provider() == Some(l.provider)
                    && born.is_some_and(|b| b >= l.at - chrono::Duration::seconds(5))
                    && !plane.consoles.contains_key(&app.current_session_id)
            });
            if let Some(i) = hit {
                found.push((k, i));
            }
        }
        for &(k, i) in found.iter().rev() {
            self.launched.remove(k);
            self.open_console(i, plane, apps);
        }
    }

    /// Sessions the watcher found asking for input: open their consoles where
    /// their cards are (no camera move, no keyboard grab). One you closed
    /// during this wait stays closed until the wait ends.
    fn expand_asking(
        &mut self,
        asking: &std::collections::HashSet<String>,
        plane: &mut Plane,
        apps: &[App],
    ) {
        self.dismissed.retain(|sid| asking.contains(sid));
        for sid in asking {
            if self.consoles.contains_key(sid) || self.dismissed.contains(sid) {
                continue;
            }
            let Some(i) = apps.iter().position(|a| &a.current_session_id == sid) else {
                continue;
            };
            let root = apps[i]
                .session
                .spawn_order()
                .next()
                .unwrap_or("main")
                .to_string();
            self.consoles.insert(
                sid.clone(),
                crate::console::Console::open(i, &root, apps, plane),
            );
            plane.open_console_in_place(i, apps);
        }
    }

    /// A session whose turn went from working to done while lama watched is
    /// finished: its card blinks green until you look (or it starts again).
    /// Sessions already done when first seen don't blink.
    /// Returns the sessions that just finished.
    fn track_turns(
        &mut self,
        turns: &std::collections::HashMap<String, crate::provider::conversation::TurnState>,
        plane: &mut Plane,
    ) -> Vec<String> {
        use crate::provider::conversation::TurnState;
        let mut just_finished = Vec::new();
        for (sid, &now) in turns {
            let before = self.turns_seen.insert(sid.clone(), now);
            match (before, now) {
                (Some(TurnState::Working), TurnState::Done) => {
                    plane.finished.insert(sid.clone());
                    just_finished.push(sid.clone());
                }
                (_, TurnState::Working) => {
                    plane.finished.remove(sid);
                }
                _ => {}
            }
        }
        just_finished
    }

    /// Which sessions are asking for approval now: the watcher's, plus
    /// any open console showing a question.
    fn track_asking(&mut self, watched_asking: &std::collections::HashSet<String>) {
        let mut asking = watched_asking.clone();
        asking.extend(
            self.consoles
                .iter()
                .filter(|(_, c)| c.needs_approval())
                .map(|(sid, _)| sid.clone()),
        );
        if asking != self.waiting {
            crate::registry::note_asking(&asking);
        }
        self.waiting = asking;
    }

    /// A key for the focused intake: type, Enter sends, Esc hands the keys
    /// back to the canvas (and closes an empty intake with no task).
    fn intake_key(&mut self, key: &crossterm::event::KeyEvent, plane: &mut Plane, apps: &[App]) {
        use crate::provider::Provider;
        use crossterm::event::{KeyCode, KeyModifiers};
        if self.picking_conductor {
            match key.code {
                KeyCode::Char('1') => self.pick_conductor(Provider::Claude, plane),
                KeyCode::Char('2') => self.pick_conductor(Provider::Codex, plane),
                KeyCode::Esc => {
                    self.picking_conductor = false;
                    if let Some(i) = self.intake.as_mut() {
                        i.notice = None;
                    }
                }
                _ => {}
            }
            return;
        }
        let empty = self.intake.as_ref().is_some_and(|i| i.draft.is_empty());
        // A dispatched task waiting on you: ↑↓ pick an owner, ←→ or Space
        // switch it between Claude and Codex, 1/2/3 all Claude / all Codex
        // / as planned, Enter launches.
        if empty && let Some(r) = self.tasks.iter_mut().rev().find(|r| r.choosing) {
            match key.code {
                KeyCode::Up => r.choice_move(false),
                KeyCode::Down => r.choice_move(true),
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => r.choice_toggle(),
                KeyCode::Char('1') => r.choice_all(Some(Provider::Claude)),
                KeyCode::Char('2') => r.choice_all(Some(Provider::Codex)),
                KeyCode::Char('3') => r.choice_all(None),
                KeyCode::Enter => r.choice_launch(),
                _ => {}
            }
            if key.code != KeyCode::Esc {
                return;
            }
        }
        // An empty intake steers the TASKS box: ↑↓ pick a task, Enter opens
        // its owner.
        if empty && !self.tasks.is_empty() {
            match key.code {
                KeyCode::Up | KeyCode::Down => {
                    let ids: Vec<String> = self.tasks.iter().map(|r| r.store.id()).collect();
                    let cur = self
                        .selected
                        .as_ref()
                        .and_then(|s| ids.iter().position(|i| i == s))
                        .unwrap_or(ids.len() - 1);
                    let next = if key.code == KeyCode::Up {
                        cur.saturating_sub(1)
                    } else {
                        (cur + 1).min(ids.len() - 1)
                    };
                    self.select_task(ids[next].clone(), plane);
                    return;
                }
                KeyCode::Enter if !self.tasks.iter().any(|r| r.asking.is_some()) => {
                    self.open_owner(plane, apps);
                    return;
                }
                _ => {}
            }
        }
        let Some(i) = self.intake.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                i.focused = false;
                if self.tasks.is_empty() && i.draft.is_empty() {
                    self.intake = None;
                }
            }
            // Shift+Enter (Alt+Enter, Ctrl+J where Shift isn't reported)
            // starts a new line; Enter sends.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                i.draft.push('\n');
                i.scroll = 0;
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                i.draft.push('\n');
                i.scroll = 0;
            }
            KeyCode::Enter => self.submit(plane),
            KeyCode::PageUp => i.scroll += 4,
            KeyCode::PageDown => i.scroll = i.scroll.saturating_sub(4),
            KeyCode::Backspace => {
                i.draft.pop();
                i.scroll = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                i.draft.clear();
                i.scroll = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                i.draft.push(c);
                i.scroll = 0;
            }
            _ => {}
        }
    }

    /// Enter in the intake starts a task — its own conductor plans it while
    /// the others run on — or answers a conductor that asked you something.
    fn submit(&mut self, plane: &Plane) {
        let Some(i) = self.intake.as_mut() else {
            return;
        };
        let text = i.draft.trim().to_string();
        if text.is_empty() {
            return;
        }
        // A conductor waiting on you gets the answer; otherwise it is a
        // new task, conducted by the agent you have — or, with both and no
        // pick yet, the one you pick now.
        use crate::agents::Conductor;
        let result = match self.tasks.iter_mut().rev().find(|r| r.asking.is_some()) {
            Some(r) => r.say(&text),
            None => match crate::agents::conductor() {
                Conductor::Use(p) => self.start_task(&text, plane, p),
                Conductor::Missing => Err("tasks need claude or codex installed".into()),
                Conductor::Ask => {
                    self.picking_conductor = true;
                    i.notice = Some(
                        "who conducts your tasks?  1 claude · 2 codex · esc cancels  \
(kept in ~/.config/lama/config.json)"
                            .into(),
                    );
                    return;
                }
            },
        };
        self.after_submit(result);
    }

    fn start_task(
        &mut self,
        text: &str,
        plane: &Plane,
        conductor: crate::provider::Provider,
    ) -> Result<(), String> {
        let r = crate::task::run::Runner::start(text, known_projects(plane), conductor)?;
        self.tasks.push(r);
        Ok(())
    }

    /// The conductor asked about once picked: kept for later tasks, then
    /// the waiting task starts on it.
    fn pick_conductor(&mut self, p: crate::provider::Provider, plane: &Plane) {
        self.picking_conductor = false;
        let text = self
            .intake
            .as_ref()
            .map(|i| i.draft.trim().to_string())
            .unwrap_or_default();
        let result =
            crate::agents::save_conductor(p).and_then(|()| self.start_task(&text, plane, p));
        self.after_submit(result);
    }

    fn after_submit(&mut self, result: Result<(), String>) {
        if let Some(i) = self.intake.as_mut() {
            match result {
                Ok(()) => {
                    i.draft.clear();
                    i.notice = None;
                }
                Err(e) => i.notice = Some(format!("✗ {e}")),
            }
        }
    }

    /// A click on the task panel or the log chip. Returns whether it was
    /// taken; a click elsewhere hands the keys back to the canvas.
    fn task_click(&mut self, column: u16, row: u16, plane: &mut Plane) -> bool {
        let hit = |r: Option<ratatui::layout::Rect>| {
            r.is_some_and(|r| column >= r.x && column < r.right() && row >= r.y && row < r.bottom())
        };
        let h = &plane.task_hits;
        if hit(h.log_chip) {
            self.log_open = !self.log_open;
            return true;
        }
        if hit(h.intake) || hit(h.prompt) {
            self.focus = None;
            self.intake.get_or_insert_with(Default::default).focused = true;
            return true;
        }
        if let Some(id) = h
            .rows
            .iter()
            .find(|(_, r)| hit(Some(*r)))
            .map(|(id, _)| id.clone())
        {
            self.select_task(id, plane);
            return true;
        }
        if let Some(i) = self.intake.as_mut() {
            i.focused = false;
        }
        hit(h.panel) || hit(h.log)
    }

    /// Pick a task in the TASKS box and fly to its owners.
    fn select_task(&mut self, id: String, plane: &mut Plane) {
        plane.fly_to_task(&id);
        self.selected = Some(id);
    }

    /// Open the selected task's first owner's console.
    fn open_owner(&mut self, plane: &mut Plane, apps: &[App]) {
        let id = self
            .selected
            .clone()
            .or_else(|| self.tasks.last().map(|r| r.store.id()));
        let Some(r) = self.tasks.iter().find(|r| Some(r.store.id()) == id) else {
            return;
        };
        let owner = r.plan.as_ref().and_then(|p| {
            p.agents
                .iter()
                .filter(|a| a.parent.is_none())
                .find_map(|a| r.launched.get(&a.name).and_then(|l| l.session.clone()))
        });
        if let Some(sid) = owner
            && let Some(i) = apps.iter().position(|a| a.current_session_id == sid)
        {
            self.open_console(i, plane, apps);
        }
    }

    /// Each frame: once a second the task reads its directory, starts
    /// requested agents and delivers messages; every frame the panel gets a
    /// fresh view.
    fn task_frame(&mut self, plane: &mut Plane, apps: &[App]) {
        if self.tasks.is_empty() {
            plane.task = self
                .intake
                .clone()
                .map(|intake| crate::state::task::TaskView {
                    intake,
                    log_open: self.log_open,
                    ..Default::default()
                });
            return;
        }
        if self
            .task_ticked
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.task_ticked = Some(Instant::now());
            let projects = known_projects(plane);
            let unadopted = self
                .tasks
                .iter()
                .any(|r| r.launched.values().any(|l| l.session.is_none()));
            // Codex agents' session ids aren't known up front: match them.
            let seen = if unadopted {
                seen_sessions(plane, apps)
            } else {
                Vec::new()
            };
            for r in &mut self.tasks {
                r.set_projects(projects.clone());
                r.tick();
                if !seen.is_empty() {
                    r.adopt(&seen);
                }
            }
        }
        // One clock for every task, so their animations line up.
        let now = self
            .clock
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs_f64();
        let mut view: Option<crate::state::task::TaskView> = None;
        let mut notice = None;
        for r in &self.tasks {
            let mut v = r.view();
            let shift = now - v.now;
            v.now = now;
            for m in &mut v.messages {
                m.shown_at += shift;
            }
            if let Some(p) = v.plan.as_mut() {
                p.planned_at += shift;
            }
            // An agent joins when its card exists: its session is on the plane.
            for a in &mut v.agents {
                if let Some(sid) = &a.session
                    && apps.iter().any(|app| &app.current_session_id == sid)
                {
                    let key = format!("{}/{}", v.id, a.name);
                    a.joined = Some(*self.joined.entry(key).or_insert(now));
                }
            }
            notice = r.last_note(30.0).map(str::to_string).or(notice);
            // The newest task leads the panel; the others' teams and
            // messages join it on the canvas.
            view = Some(match view.take() {
                None => v,
                Some(mut older) => {
                    let mut newer = v;
                    older.tasks.append(&mut newer.tasks);
                    newer.tasks = std::mem::take(&mut older.tasks);
                    let (o, n) = (&older.conductor, &mut newer.conductor);
                    n.dispatching |= o.dispatching;
                    n.reading = n.reading.take().or_else(|| o.reading.clone());
                    n.routed += o.routed;
                    n.owners += o.owners;
                    let mut log = o.log.clone();
                    log.append(&mut n.log);
                    n.log = log;
                    older.agents.append(&mut newer.agents);
                    newer.agents = std::mem::take(&mut older.agents);
                    older.messages.append(&mut newer.messages);
                    newer.messages = std::mem::take(&mut older.messages);
                    if let (Some(op), Some(np)) = (older.plan.take(), newer.plan.as_mut()) {
                        let mut agents = op.agents;
                        agents.append(&mut np.agents);
                        np.agents = agents;
                    }
                    newer
                }
            });
        }
        let Some(mut view) = view else { return };
        // A task with an agent at a permission prompt is waiting on you.
        for (row, r) in view.tasks.iter_mut().zip(&self.tasks) {
            let asking = r
                .launched
                .values()
                .filter_map(|l| l.session.as_ref())
                .any(|s| self.waiting.contains(s));
            if asking && row.status != crate::state::task::RowStatus::Done {
                row.status = crate::state::task::RowStatus::Waiting;
            }
            let sessions: Vec<&String> = r
                .launched
                .values()
                .filter_map(|l| l.session.as_ref())
                .collect();
            row.cost = sessions
                .iter()
                .filter_map(|s| plane.usage.get(*s).and_then(|u| u.cost_usd))
                .reduce(|a, b| a + b);
        }
        view.conductor.spent = self
            .tasks
            .iter()
            .filter_map(|r| {
                plane
                    .usage
                    .get(&r.conductor_session)
                    .and_then(|u| u.cost_usd)
            })
            .reduce(|a, b| a + b);
        if self
            .selected
            .as_ref()
            .is_some_and(|s| !view.tasks.iter().any(|t| &t.id == s))
        {
            self.selected = None;
        }
        view.selected = self
            .selected
            .clone()
            .or_else(|| view.tasks.last().map(|t| t.id.clone()));
        // A new question from a conductor takes the keyboard, unless you are
        // typing in a console.
        let asking = self.tasks.iter().rev().find_map(|r| {
            r.asking
                .clone()
                .or_else(|| r.choosing.then(|| format!("choose {}", r.store.id())))
        });
        if asking.is_some() && asking != self.asked && self.focus.is_none() {
            self.intake.get_or_insert_with(Default::default).focused = true;
        }
        self.asked = asking;
        view.intake = self.intake.clone().unwrap_or_default();
        if view.intake.notice.is_none() {
            view.intake.notice = notice;
        }
        view.log_open = self.log_open;
        let sessions: Vec<&String> = view
            .agents
            .iter()
            .filter_map(|a| a.session.as_ref())
            .chain(std::iter::once(&view.conductor_session))
            .collect();
        view.cost = sessions
            .iter()
            .filter_map(|s| plane.usage.get(*s).and_then(|u| u.cost_usd))
            .reduce(|a, b| a + b);
        // Re-syncs when the team or plan changed, and flies the camera.
        plane.set_task(view, apps);
    }

    fn focused_mut(&mut self) -> Option<&mut crate::console::Console> {
        let sid = self.focus.clone()?;
        self.consoles.get_mut(&sid)
    }

    /// Open (or focus) the console of session `i`, on the canvas.
    fn open_console(&mut self, i: usize, plane: &mut Plane, apps: &[App]) {
        let Some(app) = apps.get(i) else { return };
        let sid = app.current_session_id.clone();
        if !self.consoles.contains_key(&sid) {
            let root = app
                .session
                .spawn_order()
                .next()
                .unwrap_or("main")
                .to_string();
            self.consoles.insert(
                sid.clone(),
                crate::console::Console::open(i, &root, apps, plane),
            );
        }
        plane.open_console(i, apps);
        self.focus = Some(sid);
    }

    /// Keep a console object for every console the plane has open — restored
    /// from the last run, or opened since — once its session has shown up,
    /// and drop the ones the plane closed.
    fn sync_consoles(&mut self, plane: &Plane, apps: &[App]) {
        self.consoles
            .retain(|sid, _| plane.consoles.contains_key(sid));
        for sid in plane.consoles.keys() {
            if self.consoles.contains_key(sid) {
                continue;
            }
            let Some(i) = apps.iter().position(|a| &a.current_session_id == sid) else {
                continue;
            };
            if apps[i].timeline.items.is_empty() {
                continue;
            }
            let root = apps[i]
                .session
                .spawn_order()
                .next()
                .unwrap_or("main")
                .to_string();
            self.consoles.insert(
                sid.clone(),
                crate::console::Console::open(i, &root, apps, plane),
            );
        }
        if self
            .focus
            .as_ref()
            .is_some_and(|f| !self.consoles.contains_key(f))
        {
            self.focus = None;
        }
    }

    /// The console under a terminal cell, if any.
    fn console_at(&self, column: u16, row: u16) -> Option<String> {
        self.consoles
            .iter()
            .find(|(_, c)| c.contains(column, row))
            .map(|(sid, _)| sid.clone())
    }

    /// Draw every console over its place on the canvas, clipped to it.
    fn draw(&mut self, frame: &mut ratatui::Frame, canvas: ratatui::layout::Rect, plane: &Plane) {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        self.canvas = canvas;
        self.news.render(frame.buffer_mut(), canvas);
        for (sid, console) in &self.consoles {
            let Some((x, y, w, h)) = plane.console_rect(console.session) else {
                console.set_screen(None, false);
                continue;
            };
            // Round exactly as the canvas does (down, edge by edge), so the
            // console covers its card cell for cell.
            let (x0, y0) = (x.floor() as i32, y.floor() as i32);
            let (x1, y1) = ((x + w).floor() as i32, (y + h).floor() as i32);
            let (w, h) = (
                (x1 - x0).clamp(0, 600) as u16,
                (y1 - y0).clamp(0, 300) as u16,
            );
            if w == 0 || h == 0 {
                console.set_screen(None, false);
                continue;
            }
            // Render the whole console off-screen, then copy what the
            // canvas shows of it.
            console.set_origin(x0, y0);
            let full = Rect::new(0, 0, w, h);
            let mut scratch = Buffer::empty(full);
            console.render_into(
                &mut scratch,
                full,
                self.focus.as_deref() == Some(sid.as_str()),
                plane.finished.contains(sid),
            );
            if let Some(sel) = self.selection.as_mut().filter(|s| &s.session == sid) {
                if sel.copy {
                    sel.copy = false;
                    let text = sel.text(&scratch);
                    sel.text = Some(text.clone());
                    self.to_copy = Some((sid.clone(), text));
                }
                for (y, x0, x1) in sel.spans(w, h) {
                    for x in x0..x1 {
                        let cell = &mut scratch[(x, y)];
                        cell.set_style(
                            cell.style()
                                .add_modifier(ratatui::style::Modifier::REVERSED),
                        );
                    }
                }
            }
            let (cx0, cy0) = (canvas.x as i32, canvas.y as i32);
            let (cx1, cy1) = (canvas.right() as i32, canvas.bottom() as i32);
            let vx0 = x0.max(cx0);
            let vy0 = y0.max(cy0);
            let vx1 = (x0 + w as i32).min(cx1);
            let vy1 = (y0 + h as i32).min(cy1);
            if vx1 <= vx0 || vy1 <= vy0 {
                console.set_screen(None, false);
                continue;
            }
            let buf = frame.buffer_mut();
            for sy in vy0..vy1 {
                for sx in vx0..vx1 {
                    let cell = scratch[((sx - x0) as u16, (sy - y0) as u16)].clone();
                    buf[(sx as u16, sy as u16)] = cell;
                }
            }
            let whole = vx0 == x0 && vy0 == y0 && vx1 == x0 + w as i32 && vy1 == y0 + h as i32;
            console.set_screen(
                Some(Rect::new(
                    vx0 as u16,
                    vy0 as u16,
                    (vx1 - vx0) as u16,
                    (vy1 - vy0) as u16,
                )),
                whole,
            );
        }
    }

    /// Open the console of the selected agent card, if one is selected.
    fn open_selected(&mut self, plane: &mut Plane, apps: &[App]) -> bool {
        let Some(id) = plane.host.selected_agent_id() else {
            return false;
        };
        let Some((i, _)) = Plane::resolve(&id) else {
            return false;
        };
        if i >= apps.len() {
            return false;
        }
        self.open_console(i, plane, apps);
        true
    }

    /// After a press has been handled: a second press on the same agent card
    /// within [`DOUBLE_CLICK`] opens that session's console on the canvas.
    fn double_click(&mut self, event: &crossterm::event::Event, plane: &mut Plane, apps: &[App]) {
        use crossterm::event::{Event, MouseButton, MouseEventKind};
        let Event::Mouse(m) = event else { return };
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        // The card under the pointer, not the selection: a draggable card is
        // only selected once the button comes back up.
        let Some(id) = plane.node_under(m.column, m.row) else {
            self.last_press = None;
            return;
        };
        let Some((i, _)) = Plane::resolve(&id) else {
            self.last_press = None;
            return;
        };
        let now = Instant::now();
        let again = self
            .last_press
            .as_ref()
            .is_some_and(|(prev, at)| *prev == id && now.duration_since(*at) <= DOUBLE_CLICK);
        if again && i < apps.len() {
            self.open_console(i, plane, apps);
            self.last_press = None;
        } else {
            self.last_press = Some((id, now));
        }
    }
}

/// Switch between the cyberpunk and standard themes (`p`).
fn switch_theme() {
    set_theme(match crate::ui::fx::skin() {
        crate::ui::fx::Skin::Cyberpunk => crate::ui::fx::Skin::Standard,
        _ => crate::ui::fx::Skin::Cyberpunk,
    });
}

/// Use `skin` from now on and keep the choice. A theme pinned by
/// `LAMA_THEME` stays.
fn set_theme(skin: crate::ui::fx::Skin) {
    if crate::ui::fx::skin_pinned() || crate::ui::fx::skin() == skin {
        return;
    }
    crate::ui::fx::set_skin(skin);
    let _ = crate::config::set("theme", skin.name());
}

/// Where the universe keeps your project arrangement:
/// `$XDG_CONFIG_HOME/lama/universe.json`, else `~/.config/…`.
fn layout_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("lama").join("lamaverse.json"))
}

/// Where the layout lived under earlier names (`lama/universe.json`, and
/// `zoetrope/universe.json` before that): read once, when the current file
/// doesn't exist yet, so an arrangement carries over.
fn old_layout_paths() -> Vec<std::path::PathBuf> {
    let Some(new) = layout_path() else {
        return Vec::new();
    };
    let (Some(dir), Some(config)) = (new.parent(), new.parent().and_then(|d| d.parent())) else {
        return Vec::new();
    };
    vec![
        dir.join("universe.json"),
        config.join("zoetrope").join("universe.json"),
    ]
}

type Pins = std::collections::HashMap<String, (f64, f64)>;
type Places = std::collections::HashMap<String, (f64, f64, f64, f64)>;

/// Read the saved layout: `{"sessions": {"<session id>": [x, y]},
/// "consoles": {"<session id>": [w, h]}, "placed": {"<session id>": [x, y,
/// w, h]}}` — where you left sessions, their consoles' sizes, and where
/// every session sat. Where projects were left, from before sessions moved
/// on their own, is dropped; their consoles still read. A missing or
/// unreadable file is simply no layout.
fn load_layout() -> (Pins, Pins, Places) {
    let read = |p: Option<std::path::PathBuf>| p.and_then(|p| std::fs::read_to_string(p).ok());
    let Some(text) =
        read(layout_path()).or_else(|| old_layout_paths().into_iter().find_map(|p| read(Some(p))))
    else {
        return Default::default();
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&text) else {
        return Default::default();
    };
    let pairs = |v: Option<&serde_json::Value>| -> Pins {
        v.and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let xy = v.as_array()?;
                        Some((k.clone(), (xy.first()?.as_f64()?, xy.get(1)?.as_f64()?)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let places: Places = map
        .get("placed")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let r: Vec<f64> = v.as_array()?.iter().filter_map(|n| n.as_f64()).collect();
                    let &[x, y, w, h] = r.as_slice() else {
                        return None;
                    };
                    Some((k.clone(), (x, y, w, h)))
                })
                .collect()
        })
        .unwrap_or_default();
    (
        pairs(map.get("sessions")),
        pairs(map.get("consoles")),
        places,
    )
}

/// Write the layout back. Best-effort: a read-only config dir costs the
/// layout, never the session.
fn save_layout(pins: &Pins, consoles: &Pins, placed: &Places) {
    let Some(path) = layout_path() else { return };
    let obj = |m: &Pins| -> serde_json::Value {
        serde_json::Value::Object(
            m.iter()
                .map(|(k, &(a, b))| (k.clone(), serde_json::json!([a, b])))
                .collect(),
        )
    };
    let places: serde_json::Map<String, serde_json::Value> = placed
        .iter()
        .map(|(k, &(x, y, w, h))| (k.clone(), serde_json::json!([x, y, w, h])))
        .collect();
    let doc = serde_json::json!({
        "sessions": obj(pins),
        "consoles": obj(consoles),
        "placed": places,
    });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(path, text);
    }
}

/// Install a panic hook that disables mouse capture before delegating to
/// ratatui's screen-restoring hook, so a panic leaves the terminal usable.
///
/// Called after [`fn@ratatui::init`] (which installs the screen-restore hook), so
/// our layer wraps it: we disable mouse capture first, then chain to the prior
/// hook which leaves raw mode / the alternate screen.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(
            stdout(),
            crossterm::event::PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        prev(info);
    }));
}

/// Route one input event, returning whether the app should quit.
///
/// Both the `select!` arm and the post-select drain go through here so the
/// demo trigger cannot depend on which path an event arrives by.
fn route(
    event: &crossterm::event::Event,
    app: &mut App,
    pilot: &mut Option<crate::autopilot::Autopilot>,
    demo: bool,
) -> bool {
    // Arm the scripted pointer. Waypoints are read off the CURRENT layout, so
    // the script adapts to whatever the graph settled into.
    if demo && pilot.is_none() && crate::autopilot::is_trigger(event) {
        let start = app
            .scrubber_area
            .map_or((40, 10), |b| (b.x + b.width / 2, b.y.saturating_sub(6)));
        *pilot = Some(crate::autopilot::Autopilot::new(
            start,
            crate::autopilot::tour(app),
        ));
        return false;
    }
    // While the pilot drives, a stray keypress in the recording terminal must
    // not desync the script. Its own synthesized keys bypass this — they go
    // straight to the handler from the tick loop.
    if pilot.is_some() && crate::autopilot::is_key_press(event) {
        return false;
    }
    handler::handle_event(event, app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn sel(from: (u16, u16), to: (u16, u16)) -> Selection {
        Selection {
            session: String::new(),
            from,
            to,
            moved: true,
            copy: false,
            text: None,
        }
    }

    #[test]
    fn a_selection_reads_its_lines_inside_the_border() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 5));
        for (y, line) in [
            "┌────────┐",
            "│hello   │",
            "│wide 世x│",
            "│bye     │",
            "└────────┘",
        ]
        .iter()
        .enumerate()
        {
            buf.set_string(0, y as u16, line, ratatui::style::Style::default());
        }
        // Dragged backwards, from "by|e" up to "|llo": same as forwards.
        assert_eq!(sel((2, 3), (3, 1)).text(&buf), "llo\nwide 世x\nby");
        // Off the edges it stays inside the border.
        assert_eq!(sel((0, 0), (40, 9)).text(&buf), "hello\nwide 世x\nbye");
    }
}
