use anyhow::{bail, Error};
use chrono::{DateTime, Utc};
use log::debug;
use mio::{Events, Interest, Poll, Waker};
use private_poker::{
    game::GameView,
    net::{
        messages::{ClientMessage, ServerResponse},
        server::{DEFAULT_POLL_TIMEOUT, SERVER, WAKER},
    },
};
use ratatui::{
    self,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Margin, Position},
    style::{Style, Stylize},
    symbols::scrollbar,
    text::{Line, Span, Text},
    widgets::{
        Block, List, ListDirection, ListItem, ListState, Paragraph, ScrollDirection, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
    DefaultTerminal, Frame,
};
use std::{
    collections::VecDeque,
    io,
    net::TcpStream,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
};

pub const MAX_LOG_RECORDS: usize = 1024;

#[derive(Clone)]
enum RecordSource {
    System,
    User,
}

#[derive(Clone)]
struct Record {
    datetime: DateTime<Utc>,
    source: RecordSource,
    content: String,
}

impl Record {
    fn new(source: RecordSource, content: String) -> Self {
        Self {
            datetime: Utc::now(),
            source,
            content,
        }
    }
}

impl From<Record> for Vec<Span<'_>> {
    fn from(val: Record) -> Self {
        let source = match val.source {
            RecordSource::System => format!("{:6}", "SYSTEM").yellow(),
            RecordSource::User => format!("{:6}", "USER").light_green(),
        };

        vec![
            format!("  [{} ", val.datetime.format("%Y-%m-%d %H:%M:%S")).into(),
            source,
            format!("]: {}", val.content).into(),
        ]
    }
}

struct LogHandle {
    records: VecDeque<Record>,
    list_state: ListState,
    scroll_state: ScrollbarState,
}

impl LogHandle {
    pub fn clear(&mut self) {
        self.jump_to_last();
        self.scroll_state = self.scroll_state.content_length(0);
        self.records.clear();
    }

    pub fn jump_to_first(&mut self) {
        self.list_state.scroll_down_by(MAX_LOG_RECORDS as u16);
        self.scroll_state.first();
    }

    pub fn jump_to_last(&mut self) {
        self.list_state.scroll_up_by(MAX_LOG_RECORDS as u16);
        self.scroll_state.last();
    }

    pub fn move_down(&mut self) {
        self.list_state.scroll_up_by(1);
        if self.list_state.selected().is_some() {
            self.scroll_state.scroll(ScrollDirection::Forward);
        }
    }

    pub fn move_up(&mut self) {
        self.list_state.scroll_down_by(1);
        if self.list_state.selected().is_some() {
            self.scroll_state.scroll(ScrollDirection::Backward);
        }
    }

    pub fn new() -> Self {
        Self {
            records: VecDeque::with_capacity(MAX_LOG_RECORDS),
            list_state: ListState::default(),
            scroll_state: ScrollbarState::new(0),
        }
    }

    pub fn push(&mut self, source: RecordSource, content: String) {
        let record = Record::new(source, content);
        if self.records.len() == MAX_LOG_RECORDS {
            self.records.pop_back();
        }
        self.records.push_front(record);
        self.scroll_state = self.scroll_state.content_length(self.records.len());
        self.move_down();
    }
}

struct UserInput {
    /// Position of cursor in the input box.
    char_idx: usize,
    /// Current value of the input box.
    value: String,
}

impl UserInput {
    pub fn backspace(&mut self) {
        // Method "remove" is not used on the saved text for deleting the selected char.
        // Reason: Using remove on String works on bytes instead of the chars.
        // Using remove would require special care because of char boundaries.
        if self.char_idx != 0 {
            // Getting all characters before the selected character.
            let before_char_to_delete = self.value.chars().take(self.char_idx - 1);
            // Getting all characters after selected character.
            let after_char_to_delete = self.value.chars().skip(self.char_idx);

            // Put all characters together except the selected one.
            // By leaving the selected one out, it is forgotten and therefore deleted.
            self.value = before_char_to_delete.chain(after_char_to_delete).collect();
            self.move_left();
        }
    }

    /// Returns the byte index based on the character position.
    ///
    /// Since each character in a string can be contain multiple bytes, it's necessary to calculate
    /// the byte index based on the index of the character.
    fn byte_idx(&self) -> usize {
        self.value
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.char_idx)
            .unwrap_or(self.value.len())
    }

    fn clamp_cursor(&self, new_cursor_pos: usize) -> usize {
        new_cursor_pos.clamp(0, self.value.chars().count())
    }

    pub fn delete(&mut self) {
        // Method "remove" is not used on the saved text for deleting the selected char.
        // Reason: Using remove on String works on bytes instead of the chars.
        // Using remove would require special care because of char boundaries.
        if self.char_idx != self.value.len() {
            // Getting all characters before the selected character.
            let before_char_to_delete = self.value.chars().take(self.char_idx);
            // Getting all characters after selected character.
            let after_char_to_delete = self.value.chars().skip(self.char_idx + 1);

            // Put all characters together except the selected one.
            // By leaving the selected one out, it is forgotten and therefore deleted.
            self.value = before_char_to_delete.chain(after_char_to_delete).collect();
        }
    }

    pub fn input(&mut self, new_char: char) {
        let idx = self.byte_idx();
        self.value.insert(idx, new_char);
        self.move_right();
    }

    pub fn jump_to_first(&mut self) {
        self.char_idx = 0;
    }

    pub fn jump_to_last(&mut self) {
        self.char_idx = self.value.len();
    }

    pub fn move_left(&mut self) {
        let cursor_moved_left = self.char_idx.saturating_sub(1);
        self.char_idx = self.clamp_cursor(cursor_moved_left);
    }

    pub fn move_right(&mut self) {
        let cursor_moved_right = self.char_idx.saturating_add(1);
        self.char_idx = self.clamp_cursor(cursor_moved_right);
    }

    pub fn new() -> Self {
        Self {
            char_idx: 0,
            value: String::new(),
        }
    }

    pub fn submit(&mut self) -> String {
        let input = self.value.clone();
        self.char_idx = 0;
        self.value.clear();
        input
    }
}

enum ViewRequest {
    Board,
    Pots,
    Players,
    Turn,
}

enum WorkerMessage {
    Client(ViewRequest),
    Server(Box<ServerResponse>),
}

/// App holds the state of the application
pub struct App {
    username: String,
    addr: String,
    /// History of recorded messages
    log_handle: Arc<Mutex<LogHandle>>,
    /// Current value of the input box
    user_input: UserInput,
}

impl App {
    pub fn new(username: String, addr: String) -> Self {
        Self {
            username,
            addr,
            log_handle: Arc::new(Mutex::new(LogHandle::new())),
            user_input: UserInput::new(),
        }
    }

    pub fn run(
        mut self,
        stream: TcpStream,
        view: GameView,
        mut terminal: DefaultTerminal,
    ) -> Result<(), Error> {
        let (tx_client, rx_client): (Sender<ClientMessage>, Receiver<ClientMessage>) = channel();
        let (tx_worker, rx_worker): (Sender<WorkerMessage>, Receiver<WorkerMessage>) = channel();

        let mut poll = Poll::new()?;
        let waker = Waker::new(poll.registry(), WAKER)?;

        // This thread is where the actual client-server networking happens for
        // non-blocking IO. Some non-blocking IO between client threads is also
        // managed by this thread. The UI thread sends client command messages
        // to this thread; those messages are eventually written to the server.
        thread::spawn(move || -> Result<(), Error> {
            let mut events = Events::with_capacity(64);
            let mut messages_to_write: VecDeque<ClientMessage> = VecDeque::new();
            stream.set_nonblocking(true)?;
            let mut stream = mio::net::TcpStream::from_std(stream);
            poll.registry()
                .register(&mut stream, SERVER, Interest::READABLE)?;

            debug!("Polling for network events.");
            loop {
                if let Err(error) = poll.poll(&mut events, Some(DEFAULT_POLL_TIMEOUT)) {
                    match error.kind() {
                        io::ErrorKind::Interrupted => continue,
                        _ => bail!(error),
                    }
                }

                for event in events.iter() {
                    match event.token() {
                        SERVER => {}
                        WAKER => {}
                        _ => {}
                    }
                }
            }
        });

        // This thread helps manage the log handle, writing updates from the
        // server to it. This helps prevent some kind of receiver call or loop
        // within the main (UI) thread.
        let log_handle = self.log_handle.clone();
        thread::spawn(move || -> Result<(), Error> {
            let mut view = view;
            loop {
                let msg = rx_worker.recv()?;
                match msg {
                    WorkerMessage::Client(view_request) => {
                        let repr = match view_request {
                            ViewRequest::Board => view.board_as_string(),
                            ViewRequest::Players => view.players_as_string(),
                            ViewRequest::Pots => view.pot_as_string(),
                            ViewRequest::Turn => view.turn_as_string(),
                        };
                        let mut log_handle = log_handle.lock().expect("Locking on view request.");
                        log_handle.push(RecordSource::System, repr);
                    }
                    WorkerMessage::Server(server_response) => {
                        match *server_response {
                            ServerResponse::GameView(new_view) => view = new_view,
                            other => {
                                let repr = other.to_string();
                                let mut log_handle = log_handle.lock().expect("Locking on server response.");
                                log_handle.push(RecordSource::System, repr)
                            }
                        }
                    }
                }
            }
        });

        loop {
            terminal.draw(|frame| self.draw(frame))?;

            if let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = event::read()?
            {
                let mut log_handle = self.log_handle.lock().expect("Locking on key event.");
                if kind == KeyEventKind::Press {
                    match modifiers {
                        KeyModifiers::CONTROL => match code {
                            KeyCode::Char('c') => return Ok(()),
                            KeyCode::Home => log_handle.jump_to_first(),
                            KeyCode::End => log_handle.jump_to_last(),
                            _ => {}
                        },
                        KeyModifiers::NONE => match code {
                            KeyCode::Enter => {
                                let content = self.user_input.submit();
                                log_handle.push(RecordSource::User, content.clone());
                                match content.as_str() {
                                    "clear" => log_handle.clear(),
                                    "exit" => return Ok(()),
                                    "view players" => {
                                        let msg = WorkerMessage::Client(ViewRequest::Players);
                                        tx_worker.send(msg)?;
                                    }
                                    _ => {}
                                }
                            }
                            KeyCode::Char(to_insert) => self.user_input.input(to_insert),
                            KeyCode::Backspace => self.user_input.backspace(),
                            KeyCode::Delete => self.user_input.delete(),
                            KeyCode::Left => self.user_input.move_left(),
                            KeyCode::Right => self.user_input.move_right(),
                            KeyCode::Up => log_handle.move_up(),
                            KeyCode::Down => log_handle.move_down(),
                            KeyCode::Home => self.user_input.jump_to_first(),
                            KeyCode::End => self.user_input.jump_to_last(),
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let vertical = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ]);
        let [log_area, user_input_area, help_area] = vertical.areas(frame.area());

        // Render log window.
        let mut log_handle = self.log_handle.lock().expect("Locking on render.");
        let log_records: VecDeque<ListItem> = log_handle
            .records
            .iter()
            .map(|r| {
                let message: Vec<Span<'_>> = (*r).clone().into();
                let content = Line::from(message);
                ListItem::new(content)
            })
            .collect();
        let log_records = List::new(log_records)
            .direction(ListDirection::BottomToTop)
            .block(Block::bordered().title("Log"));
        frame.render_stateful_widget(log_records, log_area, &mut log_handle.list_state);

        // Render log window scrollbar.
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalLeft)
                .symbols(scrollbar::VERTICAL)
                .begin_symbol(None)
                .end_symbol(None),
            log_area.inner(Margin {
                vertical: 1,
                horizontal: 1,
            }),
            &mut log_handle.scroll_state,
        );

        // Render user input area.
        let username = self.username.clone();
        let addr = self.addr.clone();
        let user_input = Paragraph::new(self.user_input.value.as_str())
            .style(Style::default())
            .block(Block::bordered().title(format!("{username}@{addr}").light_green()));
        frame.render_widget(user_input, user_input_area);
        frame.set_cursor_position(Position::new(
            // Draw the cursor at the current position in the input field.
            // This position is can be controlled via the left and right arrow key
            user_input_area.x + self.user_input.char_idx as u16 + 1,
            // Move one line down, from the border to the input line
            user_input_area.y + 1,
        ));

        // Render user input help message.
        let (help_message, help_style) = (
            vec![
                "Press ".into(),
                "Enter".bold(),
                " to record a command, enter ".into(),
                "help".bold(),
                " to view commands,".into(),
                " or press ".into(),
                "CTRL+C".bold(),
                " to exit.".into(),
            ],
            Style::default(),
        );
        let help_message = Text::from(Line::from(help_message)).patch_style(help_style);
        let help_message = Paragraph::new(help_message);
        frame.render_widget(help_message, help_area);
    }
}
