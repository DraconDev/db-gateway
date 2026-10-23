//! The terminal operator view.
//!
//! One screen of four tabs — databases, service, backups and keys — each
//! given the whole body of the screen rather than a tile, with the strip
//! always naming every tab so a closed one is still discoverable. The
//! operator acts on what the open tab shows.
//!
//! Three rules hold this module together:
//!
//! * **The facts come from the shared view layer** ([`crate::views`]),
//!   the same functions the CLI draws from, so the two surfaces cannot
//!   tell different stories about the same gateway.
//! * **The requests come from the shared action catalogue**
//!   ([`actions`]), so the view cannot invent a call the CLI would not
//!   make, and the destructive ones are confirmed by the same rule.
//! * **The terminal is always given back** ([`screen`]), including when
//!   the view panics.

pub mod actions;
pub mod draw;
pub mod screen;
pub mod theme;

use crate::{
    client,
    error::{Error, Result},
    render, transfer, views,
};
use actions::{Action, ActionKind, Invocation};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use screen::Screen;
use serde_json::Value;
use std::time::{Duration, Instant};

/// How the view was started.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// How often the service is read again, in seconds.
    pub refresh_secs: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self { refresh_secs: 2 }
    }
}

/// What the view is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The dashboard.
    Normal,
    /// The list of actions.
    Palette,
    /// The arguments of the chosen action.
    Prompt { kind: ActionKind },
    /// The phrase that confirms a destructive act.
    Confirm {
        invocation: Invocation,
        phrase: String,
    },
}

/// Which tab has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The database list.
    Databases,
    /// The service dashboard.
    Service,
    /// The backup freshness.
    Backups,
    /// The credentials.
    Keys,
}

impl Focus {
    /// The next tab.
    pub fn next(self) -> Self {
        match self {
            Self::Databases => Self::Service,
            Self::Service => Self::Backups,
            Self::Backups => Self::Keys,
            Self::Keys => Self::Databases,
        }
    }

    /// The previous tab, so `shift-tab` goes back the way it came.
    pub fn previous(self) -> Self {
        match self {
            Self::Databases => Self::Keys,
            Self::Service => Self::Databases,
            Self::Backups => Self::Service,
            Self::Keys => Self::Backups,
        }
    }

    /// What the tab is called.
    pub fn title(self) -> &'static str {
        match self {
            Self::Databases => "databases",
            Self::Service => "service",
            Self::Backups => "backups",
            Self::Keys => "keys",
        }
    }
}

/// What an action did.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// What happened, in one line.
    pub headline: String,
    /// The facts it produced, as a person reads them.
    pub facts: Vec<(String, String)>,
    /// The rows it produced, when it produced rows.
    pub table: Option<render::Table>,
    /// Why it failed, when it failed.
    pub error: Option<String>,
}

/// How far each tab is scrolled.
///
/// A tab that silently cuts its list is a visibility lie: the operator
/// cannot tell a database that does not exist from one that is below
/// the fold. So the tabs scroll, and they say how much they hold.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scroll {
    /// The database list.
    pub databases: u16,
    /// The service dashboard.
    pub service: u16,
    /// The backup freshness.
    pub backups: u16,
    /// The credentials.
    pub keys: u16,
}

impl Scroll {
    /// The offset of the open tab.
    pub fn of(&mut self, focus: Focus) -> &mut u16 {
        match focus {
            Focus::Databases => &mut self.databases,
            Focus::Service => &mut self.service,
            Focus::Backups => &mut self.backups,
            Focus::Keys => &mut self.keys,
        }
    }
}

/// The view's state.
pub struct App {
    /// The gateway being watched.
    pub server: String,
    /// The credential the view uses. Never drawn.
    key: String,
    /// The gateway's answer about the service.
    pub status: Option<Value>,
    /// The gateway's answer about the backups.
    pub backups: Option<Value>,
    /// The gateway's answer about the credentials.
    pub keys: Option<Value>,
    /// The gateway's answer about the databases.
    pub list: Option<Value>,
    /// When the answers last arrived.
    pub refreshed: Option<Instant>,
    /// Whether the gateway answered the last refresh.
    pub connected: bool,
    /// The last thing that went wrong.
    pub error: Option<String>,
    /// What the view is doing.
    pub mode: Mode,
    /// The palette's filter.
    pub filter: String,
    /// The palette's highlighted entry.
    pub highlighted: usize,
    /// The line the operator is typing.
    pub input: String,
    /// The last action's outcome.
    pub outcome: Option<Outcome>,
    /// Which tab has the keyboard.
    pub focus: Focus,
    /// How far each tab is scrolled.
    pub scroll: Scroll,
    /// The action waiting to run.
    pending: Option<Invocation>,
    /// Whether the view should end.
    pub quit: bool,
}

impl App {
    /// A view of one gateway.
    pub fn new(server: String, key: String) -> Self {
        Self {
            server,
            key,
            status: None,
            backups: None,
            keys: None,
            list: None,
            refreshed: None,
            connected: false,
            error: None,
            mode: Mode::Normal,
            filter: String::new(),
            highlighted: 0,
            input: String::new(),
            outcome: None,
            focus: Focus::Databases,
            scroll: Scroll::default(),
            pending: None,
            quit: false,
        }
    }

    /// The actions the palette shows, filtered by what was typed.
    pub fn filtered_actions(&self) -> Vec<&'static Action> {
        let needle = self.filter.to_lowercase();
        actions::CATALOG
            .iter()
            .filter(|action| {
                needle.is_empty()
                    || action.title.to_lowercase().contains(&needle)
                    || action.hint.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// The action waiting to run, taken.
    pub fn take_pending(&mut self) -> Option<Invocation> {
        self.pending.take()
    }

    /// Handle one key.
    pub fn on_key(&mut self, key: KeyEvent) {
        if key
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            // The way out of a program that has taken the terminal.
            self.quit = true;
            return;
        }
        match self.mode.clone() {
            Mode::Normal => self.on_key_normal(key),
            Mode::Palette => self.on_key_palette(key),
            Mode::Prompt { .. } | Mode::Confirm { .. } => self.on_key_input(key),
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('r') => self.pending = Some(Invocation::Refresh),
            KeyCode::Char(':') => {
                self.mode = Mode::Palette;
                self.filter.clear();
                self.highlighted = 0;
            }
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.previous(),
            // The tab strip prints these numbers, so they have to work.
            // A numbered hint that does not do anything is worse than no
            // hint: it teaches the operator a key that fails.
            KeyCode::Char(digit @ '1'..='4') => {
                let index = digit as usize - '1' as usize;
                self.focus = [
                    Focus::Databases,
                    Focus::Service,
                    Focus::Backups,
                    Focus::Keys,
                ][index];
            }
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Home => *self.scroll.of(self.focus) = 0,
            _ => {}
        }
    }

    /// Scroll the open tab.
    fn scroll_by(&mut self, lines: i32) {
        let offset = self.scroll.of(self.focus);
        let moved = (*offset as i32 + lines).max(0) as u16;
        *offset = moved;
    }

    fn on_key_palette(&mut self, key: KeyEvent) {
        let count = self.filtered_actions().len();
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.filter.clear();
            }
            KeyCode::Up => self.highlighted = self.highlighted.saturating_sub(1),
            KeyCode::Down => {
                if count > 0 {
                    self.highlighted = (self.highlighted + 1).min(count - 1);
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.highlighted = 0;
            }
            KeyCode::Char(character) => {
                self.filter.push(character);
                self.highlighted = 0;
            }
            KeyCode::Enter => self.choose(),
            _ => {}
        }
    }

    fn on_key_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character) => self.input.push(character),
            KeyCode::Enter => self.submit(),
            _ => {}
        }
    }

    /// The operator chose an action from the palette.
    fn choose(&mut self) {
        let Some(action) = self.filtered_actions().get(self.highlighted).copied() else {
            return;
        };
        if action.arguments {
            self.mode = Mode::Prompt { kind: action.kind };
            self.input.clear();
            return;
        }
        if action.confirmation {
            // An action whose confirmation is its whole input: the
            // owner key has no arguments, only a word.
            let Ok(invocation) = action.kind.parse("") else {
                return;
            };
            let Some(phrase) = invocation.confirmation() else {
                return;
            };
            self.mode = Mode::Confirm { invocation, phrase };
            self.input.clear();
            return;
        }
        self.mode = Mode::Normal;
        match action.kind {
            ActionKind::Quit => self.quit = true,
            ActionKind::Refresh => self.pending = Some(Invocation::Refresh),
            _ => {}
        }
    }

    /// The operator answered a prompt.
    fn submit(&mut self) {
        let Mode::Prompt { kind } = self.mode.clone() else {
            // A confirmation: the phrase has to be exact. The rule is
            // the shared one, the same rule the CLI's `--confirm` flag
            // enforces.
            let Mode::Confirm { invocation, phrase } = self.mode.clone() else {
                return;
            };
            if crate::confirmation::confirm_matches(Some(&self.input), &phrase).is_err() {
                // The line is cleared, so the operator types the name
                // again rather than editing a wrong one in place.
                self.input.clear();
                self.outcome = Some(Outcome {
                    headline: "not confirmed".to_string(),
                    facts: Vec::new(),
                    table: None,
                    error: Some(format!("type {phrase} exactly to go ahead")),
                });
                return;
            }
            self.mode = Mode::Normal;
            self.input.clear();
            self.pending = Some(invocation);
            return;
        };
        let arguments = std::mem::take(&mut self.input);
        match kind.parse(&arguments) {
            Ok(invocation) => {
                if let Some(phrase) = invocation.confirmation() {
                    self.mode = Mode::Confirm { invocation, phrase };
                    self.input.clear();
                    return;
                }
                self.mode = Mode::Normal;
                self.pending = Some(invocation);
            }
            Err(error) => {
                self.mode = Mode::Normal;
                self.outcome = Some(Outcome {
                    headline: "the action was not understood".to_string(),
                    facts: Vec::new(),
                    table: None,
                    error: Some(error.message.to_string()),
                });
            }
        }
    }

    /// Read the service again.
    pub async fn refresh(&mut self) {
        let status = self.fetch("/v1/status").await;
        let backups = self.fetch("/v1/backups").await;
        let keys = self.fetch("/v1/keys").await;
        let list = self.fetch("/v1/databases").await;
        let mut failures: Vec<String> = Vec::new();
        match status {
            Ok(value) => self.status = Some(value),
            Err(error) => failures.push(error.message.to_string()),
        }
        match backups {
            Ok(value) => self.backups = Some(value),
            Err(error) => failures.push(error.message.to_string()),
        }
        match keys {
            Ok(value) => self.keys = Some(value),
            Err(error) => failures.push(error.message.to_string()),
        }
        match list {
            Ok(value) => self.list = Some(value),
            Err(error) => failures.push(error.message.to_string()),
        }
        // The gateway is reachable when it answered anything at all: a
        // refused backup list does not mean the service is down.
        self.connected = failures.len() < 4;
        self.error = (!failures.is_empty()).then(|| failures.join("; "));
        self.refreshed = Some(Instant::now());
    }

    /// One read of the gateway.
    async fn fetch(&self, path: &str) -> Result<Value> {
        let value = client::request(
            &self.server,
            &self.key,
            reqwest::Method::GET,
            path,
            None,
            None,
        )
        .await?;
        if let Some(error) = value.get("error") {
            let code = error["code"].as_str().unwrap_or("error");
            let message = error["message"].as_str().unwrap_or("refused");
            return Err(Error::with_message(
                400,
                "api_error",
                format!("{code}: {message}"),
            ));
        }
        Ok(value)
    }

    /// Run the action the operator chose.
    pub async fn run_invocation(&mut self, invocation: Invocation) {
        let outcome = match &invocation {
            Invocation::Quit => {
                self.quit = true;
                return;
            }
            Invocation::Refresh => {
                self.refresh().await;
                return;
            }
            Invocation::ExportDatabase { name, out } => {
                match transfer::export(&self.server, &self.key, name, out).await {
                    Ok(report) => Outcome {
                        headline: format!("exported {name}"),
                        facts: vec![
                            ("file".to_string(), report.out.display().to_string()),
                            ("size".to_string(), render::human_bytes(report.size)),
                            ("sha256".to_string(), report.sha256),
                        ],
                        table: None,
                        error: None,
                    },
                    Err(failure) => Outcome {
                        headline: "the export failed".to_string(),
                        facts: Vec::new(),
                        table: None,
                        error: Some(failure.error.message.to_string()),
                    },
                }
            }
            Invocation::ImportDatabase { name, from } => {
                match transfer::import(&self.server, &self.key, name, from).await {
                    Ok(report) => Outcome {
                        headline: format!("imported {name}"),
                        facts: vec![
                            ("file".to_string(), report.from.display().to_string()),
                            ("size".to_string(), render::human_bytes(report.size)),
                        ],
                        table: None,
                        error: None,
                    },
                    Err(failure) => Outcome {
                        headline: "the import failed".to_string(),
                        facts: Vec::new(),
                        table: None,
                        error: Some(failure.error.message.to_string()),
                    },
                }
            }
            _ => {
                let request = match invocation.request() {
                    Ok(request) => request,
                    Err(error) => {
                        // A refusal the client made: nothing was sent.
                        self.outcome = Some(Outcome {
                            headline: "nothing was sent".to_string(),
                            facts: Vec::new(),
                            table: None,
                            error: Some(error.message.to_string()),
                        });
                        return;
                    }
                };
                // The statements the request carried, read from the
                // request itself: the view describes what was sent, not
                // what the file says now.
                let statements: Vec<String> = request
                    .body
                    .as_ref()
                    .and_then(|body| body["statements"].as_array().cloned())
                    .map(|list| {
                        list.iter()
                            .filter_map(|statement| statement["sql"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                // The request says what it can commit, and the view
                // says what the failure means: a read-only query that
                // cannot reach the gateway is an unavailable gateway,
                // never a write that may have committed.
                let semantics = request.semantics;
                let response = match client::request_with_semantics(
                    &self.server,
                    &self.key,
                    request.method,
                    &request.path,
                    request.body,
                    None,
                    semantics,
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        self.outcome = Some(Outcome {
                            headline: "the gateway did not answer".to_string(),
                            facts: Vec::new(),
                            table: None,
                            error: Some(error.message.to_string()),
                        });
                        return;
                    }
                };
                outcome_of(&invocation, &statements, &response)
            }
        };
        self.outcome = Some(outcome);
        self.refresh().await;
    }

    /// The database list with each database's traffic.
    pub fn databases(&self) -> Option<render::Table> {
        let list = self.list.as_ref()?;
        let status = self.status.clone().unwrap_or(Value::Null);
        Some(views::databases(list, &status))
    }

    /// The credential list.
    ///
    /// The tab strip counts what is in each tab, so it reads the same table
    /// the tab itself draws -- one source, so the count and the contents
    /// cannot disagree.
    pub fn keys_table(&self) -> Option<render::Table> {
        self.keys.as_ref().map(views::keys)
    }
}

/// Read the answer to an action.
///
/// The reading is the shared one: a batch's outcome comes from
/// [`views::batch_outcomes`], so the view says the same thing about a
/// failed batch that the CLI says.
fn outcome_of(invocation: &Invocation, statements: &[String], response: &Value) -> Outcome {
    if !statements.is_empty() {
        let outcome = views::batch_outcomes(statements, response);
        let reason = outcome.reason();
        return Outcome {
            headline: if outcome.ran() {
                "the statements ran".to_string()
            } else {
                "the batch failed".to_string()
            },
            facts: Vec::new(),
            table: Some(outcome.table),
            error: reason,
        };
    }
    if let Some(columns) = response["columns"].as_array() {
        let rows: Vec<Vec<Value>> = response["rows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|row| row.as_array().cloned().unwrap_or_default())
            .collect();
        let names: Vec<String> = columns
            .iter()
            .map(|column| column.as_str().unwrap_or("").to_string())
            .collect();
        return Outcome {
            headline: format!("{} rows", rows.len()),
            facts: Vec::new(),
            table: Some(render::Table::from_json(&names, &rows)),
            error: None,
        };
    }
    if let Some(error) = response.get("error") {
        let code = error["code"].as_str().unwrap_or("error");
        let message = error["message"].as_str().unwrap_or("refused");
        return Outcome {
            headline: "the gateway refused".to_string(),
            facts: Vec::new(),
            table: None,
            error: Some(format!("{code}: {message}")),
        };
    }
    let facts = match invocation {
        Invocation::CreateDatabase { name } => vec![("name".to_string(), name.clone())],
        Invocation::DeleteDatabase { name } => {
            let purge = response["purge_after_seconds"]
                .as_u64()
                .map(render::human_duration)
                .unwrap_or_else(|| "unknown".to_string());
            vec![
                ("name".to_string(), name.clone()),
                ("purges in".to_string(), purge),
            ]
        }
        Invocation::CloneDatabase {
            source,
            destination,
        } => vec![
            ("from".to_string(), source.clone()),
            ("to".to_string(), destination.clone()),
        ],
        Invocation::IssueKey {
            name,
            role,
            databases,
        } => vec![
            // The key is shown once and never again, so it is on the
            // screen, with the scope it is safe with.
            (
                "key".to_string(),
                response["key"].as_str().unwrap_or("").to_string(),
            ),
            ("name".to_string(), name.clone()),
            ("role".to_string(), role.clone()),
            ("scope".to_string(), databases.join(", ")),
        ],
        Invocation::RevokeKey { name } => vec![("name".to_string(), name.clone())],
        Invocation::RotateOwner => vec![
            (
                "key".to_string(),
                response["key"].as_str().unwrap_or("").to_string(),
            ),
            ("old key".to_string(), "dead immediately".to_string()),
        ],
        _ => Vec::new(),
    };
    Outcome {
        headline: match invocation {
            Invocation::CreateDatabase { name } => format!("created {name}"),
            Invocation::DeleteDatabase { name } => format!("tombstoned {name}"),
            Invocation::CloneDatabase {
                source,
                destination,
            } => format!("cloned {source} to {destination}"),
            Invocation::IssueKey { name, .. } => format!("issued {name}"),
            Invocation::RevokeKey { name } => format!("revoked {name}"),
            Invocation::RotateOwner => "rotated the owner credential".to_string(),
            _ => "done".to_string(),
        },
        facts,
        table: None,
        error: None,
    }
}

/// Run the view until the operator leaves it.
pub async fn run(server: &str, key: &str, options: Options) -> Result<()> {
    let mut app = App::new(server.to_string(), key.to_string());
    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    // crossterm's reader blocks, so it lives on its own thread and the
    // loop waits for its events beside the refresh.
    std::thread::spawn(move || {
        while let Ok(event) = ratatui::crossterm::event::read() {
            // The channel is closed when the view has left; the send
            // failing is the thread's signal to stop.
            if events.send(event).is_err() {
                break;
            }
        }
    });
    let mut screen = screen::CrosstermScreen::new().map_err(|_| Error::internal())?;
    screen.enter().map_err(|_| Error::internal())?;
    // The guard gives the terminal back however this ends: a normal
    // exit, an error, or a panic in a draw function.
    let mut guard = screen::Guard::new(screen);
    // The first tick is a full period away: the view reads the service
    // once when it opens, and the ticker takes it from there. A ticker
    // that fired at once would read everything twice.
    let period = Duration::from_secs(options.refresh_secs.clamp(1, 3600));
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first read happens now, so the view opens with facts rather
    // than with an empty screen.
    app.refresh().await;
    loop {
        if guard.draw(|frame| draw::draw(frame, &app)).is_err() {
            return Err(Error::internal());
        }
        tokio::select! {
            event = inbox.recv() => {
                match event {
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        app.on_key(key);
                        if app.quit {
                            break;
                        }
                        if let Some(invocation) = app.take_pending() {
                            app.run_invocation(invocation).await;
                        }
                    }
                    Some(_) => {}
                    // The reader thread is gone: there is nothing left
                    // to wait for.
                    None => break,
                }
            }
            _ = ticker.tick() => {
                app.refresh().await;
            }
        }
    }
    guard.release().map_err(|_| Error::internal())?;
    Ok(())
}
