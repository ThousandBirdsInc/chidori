use std::{error::Error, io};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{prelude::*, widgets::*};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use prompt_graph_core::proto2::execution_runtime_client::ExecutionRuntimeClient;
use prompt_graph_core::proto2::{ChangeValue, Path, RequestOnlyId, SerializedValue};
use prompt_graph_core::utils::serialized_value_to_string;

async fn get_client(url: String) -> Result<ExecutionRuntimeClient<tonic::transport::Channel>, tonic::transport::Error> {
    ExecutionRuntimeClient::connect(url.clone()).await
}

enum InputMode {
    Normal,
    Editing,
}

struct App {
    input: Input,
    input_mode: InputMode,
    changes_state: TableState,
    node_will_execs_state: TableState,
    node_will_execs: Arc<Mutex<Vec<Vec<String>>>>,
    seen_node_will_execs: Arc<Mutex<HashSet<u64>>>,
    changes: Arc<Mutex<Vec<Vec<String>>>>,
    seen_changes: Arc<Mutex<HashSet<u64>>>
}

impl App {
    fn new() -> App {
        App {
            input: Input::default(),
            input_mode: InputMode::Normal,
            changes_state: TableState::default(),
            node_will_execs_state: TableState::default(),
            node_will_execs: Arc::new(Mutex::new(vec![])),
            seen_node_will_execs: Arc::new(Mutex::new(HashSet::new())),
            changes: Arc::new(Mutex::new(vec![])),
            seen_changes: Arc::new(Mutex::new(HashSet::new()))
        }
    }
    pub fn next(&mut self) {
        let i = match self.changes_state.selected() {
            Some(i) => {
                if self.changes.lock().unwrap().len() == 0 {
                    0
                } else {
                    if i >= self.changes.lock().unwrap().len() - 1 {
                        0
                    } else {
                        i + 1
                    }
                }
            }
            None => 0,
        };
        self.changes_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.changes_state.selected() {
            Some(i) => {
                if i == 0 {
                    if self.changes.lock().unwrap().len() == 0 {
                        0
                    } else {
                        self.changes.lock().unwrap().len() - 1
                    }
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.changes_state.select(Some(i));
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // create app and run it
    let mut app = App::new();

    let mut changes = app.changes.clone();
    let mut seen_changes = app.seen_changes.clone();
    let mut node_will_execs = app.node_will_execs.clone();
    let mut seen_node_will_execs = app.seen_node_will_execs.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(mut client) = get_client("http://localhost:9800".to_string()).await {
                if let Ok(resp) = client.list_change_events(RequestOnlyId {
                    id: "0".to_string(),
                    branch: 0,
                }).await {
                    let mut stream = resp.into_inner();
                    while let Some(x) = stream.next().await {
                        if let Ok(x) = x {
                            let counter = x.monotonic_counter;
                            if seen_changes.lock().unwrap().contains(&counter) {
                                continue;
                            }
                            seen_changes.lock().unwrap().insert(counter);
                            for fv in x.filled_values.iter() {
                                let ChangeValue { path, value , ..} = fv.clone();
                                let path = path.unwrap_or(Path { address: vec![]}).address.join(":");
                                let value = serialized_value_to_string(&value.unwrap_or(SerializedValue { val: None, }));
                                changes.lock().unwrap().push(vec![x.source_node.clone(), path, value]);
                            }
                        }
                    };
                }
            } else {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                continue;
            }
        }
    });
    tokio::spawn(async move {
        loop {
            if let Ok(mut client) = get_client("http://localhost:9800".to_string()).await {
                if let Ok(resp) = client.list_node_will_execute_events(RequestOnlyId {
                    id: "0".to_string(),
                    branch: 0,
                }).await {
                    let mut stream = resp.into_inner();
                    // Clear the node_will_execs
                    let mut replace_node_will_execs = vec![];
                    while let Some(x) = stream.next().await {
                        if let Ok(x) = x {
                            let counter = x.counter;
                            if seen_node_will_execs.lock().unwrap().contains(&counter) {
                                continue;
                            }
                            seen_node_will_execs.lock().unwrap().insert(counter);
                            let n = x.node.as_ref().unwrap();
                            let source_node = &n.source_node;
                            let mut paths = vec![];
                            for wcv in n.change_values_used_in_execution.iter() {
                                let path = wcv.change_value.as_ref().unwrap().path.as_ref().unwrap().address.join(":");
                                paths.push(path);
                            }
                            replace_node_will_execs.push(vec![source_node.clone(), paths.join(", ")]);
                        }
                    };
                    let mut node_will_execs = node_will_execs.lock().unwrap();
                    node_will_execs.clear();
                    node_will_execs.extend(replace_node_will_execs);
                }
            } else {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                continue;
            }
        }
    });

    let res = run_app(&mut terminal, app).await;

    // restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;


    if let Err(err) = res {
        println!("{err:?}");
    }

    Ok(())
}

async fn run_app<B: Backend>(terminal: &mut Terminal<B>, mut app: App) -> anyhow::Result<()> {

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match app.input_mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char('e') => {
                            app.input_mode = InputMode::Editing;
                        }
                        KeyCode::Char('r') => {
                            app.changes.lock().unwrap().clear();
                            app.seen_changes.lock().unwrap().clear();
                            app.node_will_execs.lock().unwrap().clear();
                            app.seen_node_will_execs.lock().unwrap().clear();
                        }
                        KeyCode::Char('q') => {
                            return Ok(());
                        }
                        KeyCode::Down => app.next(),
                        KeyCode::Up => app.previous(),
                        _ => {}
                    },
                    InputMode::Editing => match key.code {
                        KeyCode::Enter => {
                            // app.items.push(app.input.value().into());
                            app.input.reset();
                        }
                        KeyCode::Esc => {
                            app.input_mode = InputMode::Normal;
                        }
                        _ => {
                            app.input.handle_event(&Event::Key(key));
                        }
                    },
                }
            }
        } else {

        }

    }
}

fn insert_newlines(input: &str, max_length: usize) -> String {
    let mut result = String::new();
    let mut line_length = 0;

    for line in input.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for word in words {
            let word_length = word.len();

            if line_length + word_length > max_length {
                result.push('\n');
                line_length = 0;
            } else if line_length > 0 {
                result.push(' ');
                line_length += 1;
            }

            result.push_str(word);
            line_length += word_length;
        }
        result.push('\n');
        line_length = 0;
    }

    result
}

fn truncate_string(input: &str) -> String {
    let lines: Vec<&str> = input.lines().take(20).collect();
    lines.join("\n")
}


fn ui<B: Backend>(f: &mut Frame<B>, app: &mut App) {
    let rects = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints(
            [
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(20),
                Constraint::Min(1),
            ]
                .as_ref(),
        )
        .split(f.size());

    let (msg, style) = match app.input_mode {
        InputMode::Normal => (
            vec![
                Span::raw("Press "),
                Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" to exit, "),
                Span::styled("e", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" to start editing."),
            ],
            Style::default().add_modifier(Modifier::RAPID_BLINK),
        ),
        InputMode::Editing => (
            vec![
                Span::raw("Press "),
                Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" to stop editing, "),
                Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" to record the message"),
            ],
            Style::default(),
        ),
    };
    let mut text = Text::from(Line::from(msg));
    text.patch_style(style);
    let help_message = Paragraph::new(text);
    f.render_widget(help_message, rects[0]);
    let width = rects[0].width.max(3) - 3; // keep 2 for borders and 1 for cursor


    let scroll = app.input.visual_scroll(width as usize);
    let input = Paragraph::new(app.input.value())
        .style(match app.input_mode {
            InputMode::Normal => Style::default(),
            InputMode::Editing => Style::default().fg(Color::Yellow),
        })
        .scroll((0, scroll as u16))
        .block(Block::default().borders(Borders::ALL).title("Input"));
    f.render_widget(input, rects[1]);
    match app.input_mode {
        InputMode::Normal =>
        // Hide the cursor. `Frame` does this by default, so we don't need to do anything here
            {}

        InputMode::Editing => {
            // Make the cursor visible and ask tui-rs to put it at the specified coordinates after rendering
            f.set_cursor(
                // Put cursor past the end of the input text
                rects[1].x
                    + ((app.input.visual_cursor()).max(scroll) - scroll) as u16
                    + 1,
                // Move one line down, from the border to the input line
                rects[1].y + 1,
            )
        }
    }

    let node_will_execs = app.node_will_execs.try_lock().unwrap();
    let changes = app.changes.try_lock().unwrap();

    // Table widget nodes
    let selected_style = Style::default().add_modifier(Modifier::REVERSED);
    let normal_style = Style::default();
    let header_cells = ["Node", "Paths"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::REVERSED)));
    let header = Row::new(header_cells)
        .style(normal_style)
        .height(1);
    let rows = node_will_execs.iter().map(|item| {
        let height = item
            .iter()
            .map(|content| truncate_string(&insert_newlines(&content.clone(), 100)).chars().filter(|c| *c == '\n').count())
            .max()
            .min(Some(10))
            .unwrap_or(0)
            + 1;
        let cells = item.iter().map(|c| Cell::from(insert_newlines(&c.clone(), 100)));
        Row::new(cells).height(height as u16)
    });

    let t = Table::new(rows)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Nodes Currently Executing"))
        // .highlight_style(selected_style)
        // .highlight_symbol(">> ")
        .widths(&[
            Constraint::Min(20),
            Constraint::Percentage(20),
            Constraint::Percentage(80),
        ]);
    f.render_stateful_widget(t, rects[2], &mut app.node_will_execs_state);


    // Table widget for query results
    let selected_style = Style::default().add_modifier(Modifier::REVERSED);
    let normal_style = Style::default();
    let header_cells = ["Node", "Path", "Value"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::REVERSED)));
    let header = Row::new(header_cells)
        .style(normal_style)
        .height(1);
    let mut changes = changes.clone();
    changes.reverse();
    let rows = changes.iter().map(|item| {
        let height = item
            .iter()
            .map(|content| truncate_string(&insert_newlines(&content.clone(), 100)).chars().filter(|c| *c == '\n').count())
            .max()
            .min(Some(10))
            .unwrap_or(0)
            + 1;
        let cells = item.iter().map(|c| Cell::from(insert_newlines(&c.clone(), 100)));
        Row::new(cells).height(height as u16).bottom_margin(1)
    });


    let t = Table::new(rows)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Changes Emitted"))
        .highlight_style(selected_style)
        .highlight_symbol(">> ")
        .widths(&[
            Constraint::Min(20),
            Constraint::Percentage(20),
            Constraint::Percentage(80),
        ]);
    f.render_stateful_widget(t, rects[3], &mut app.changes_state);
}