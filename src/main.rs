mod k8s;

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use kube::Client;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const MAX_BUFFER_LINES: usize = 20_000;

#[derive(Parser, Debug)]
#[command(
    name = "remote-log-tui",
    about = "Local TUI: Kubernetes pod logs (kube-rs) with tabbed pod picker"
)]
struct Cli {
    /// Kubernetes namespace
    #[arg(short = 'n', long = "namespace", default_value = "default")]
    namespace: String,

    /// Path to kubeconfig (default: infer from $KUBECONFIG / ~/.kube/config)
    #[arg(long = "kubeconfig", value_name = "PATH")]
    kubeconfig: Option<PathBuf>,

    /// kubeconfig context name
    #[arg(long = "kube-context", value_name = "NAME")]
    kube_context: Option<String>,

    /// Container name when a pod has multiple containers
    #[arg(long = "container", value_name = "NAME")]
    container: Option<String>,

    /// Lines of log history before following the stream
    #[arg(long, default_value_t = 200)]
    tail_lines: u32,
}

struct App {
    lines: VecDeque<String>,
    follow_tail: bool,
    scroll_from_bottom: usize,
    status: String,
}

impl App {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            follow_tail: true,
            scroll_from_bottom: 0,
            status: String::new(),
        }
    }

    fn push_line(&mut self, line: String) {
        if self.lines.len() >= MAX_BUFFER_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn scroll_up(&mut self, n: usize) {
        self.follow_tail = false;
        let max = self.lines.len().saturating_sub(1);
        self.scroll_from_bottom = (self.scroll_from_bottom + n).min(max);
    }

    fn scroll_down(&mut self, n: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(n);
        if self.scroll_from_bottom == 0 {
            self.follow_tail = true;
        }
    }

    fn page_height(&self, area: Rect) -> usize {
        area.height.saturating_sub(2) as usize
    }

    fn visible_slice(&self, area: Rect) -> (usize, usize) {
        let page = self.page_height(area).max(1);
        let total = self.lines.len();
        if total == 0 {
            return (0, 0);
        }
        let end_exclusive = if self.follow_tail {
            total
        } else {
            total.saturating_sub(self.scroll_from_bottom)
        };
        let start = end_exclusive.saturating_sub(page);
        (start, end_exclusive)
    }
}

fn draw_log_body(f: &mut Frame<'_>, app: &App, area: Rect, title: &str) {
    let (start, end) = app.visible_slice(area);
    let page = app.page_height(area).max(1);
    let lines: Vec<Line> = app
        .lines
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .enumerate()
        .map(|(i, text)| {
            let global = start + i;
            let is_recent = global >= app.lines.len().saturating_sub(page);
            let base = if app.follow_tail && is_recent {
                Style::default().fg(Color::LightGreen)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(vec![
                Span::styled(
                    format!("{:>6} │ ", global + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(text.clone(), base),
            ])
        })
        .collect();
    let log = Paragraph::new(lines)
        .block(
            Block::default()
                .title(format!(" {title} "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(log, area);
}

fn draw_log_status(f: &mut Frame<'_>, app: &App, area: Rect) {
    let status = Paragraph::new(app.status.clone()).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::ITALIC),
    );
    f.render_widget(status, area);
}

fn tab_title(name: &str, max_chars: usize) -> Line<'static> {
    let s: String = if name.chars().count() <= max_chars {
        format!(" {name} ")
    } else {
        let mut out: String = name.chars().take(max_chars.saturating_sub(2)).collect();
        out.push('…');
        format!(" {out} ")
    };
    Line::from(s)
}

fn draw_k8s_view(
    f: &mut Frame<'_>,
    namespace: &str,
    pods: &[String],
    selected: usize,
    log_app: &App,
    banner: &str,
) {
    let area = f.area();
    let tab_titles: Vec<Line<'static>> = if pods.is_empty() {
        vec![Line::from(" (no pods in namespace) ")]
    } else {
        pods.iter().map(|p| tab_title(p, 14)).collect()
    };
    let tab_index = if pods.is_empty() {
        0
    } else {
        selected.min(pods.len() - 1)
    };
    let tabs = Tabs::new(tab_titles)
        .select(tab_index)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::raw("|"));
    let tab_block = Block::default()
        .title(format!(" Pods · ns/{namespace} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let tab_widget = tabs.block(tab_block);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(area);
    f.render_widget(tab_widget, chunks[0]);
    let log_title = if pods.is_empty() {
        "no pods".to_string()
    } else {
        format!("logs · {}", pods[tab_index])
    };
    draw_log_body(f, log_app, chunks[1], &log_title);
    let status_line = format!("{banner}   ←/→ pod · ↑/↓ log · g tail · r reload pods · q quit");
    let mut status_app = App::new();
    status_app.status = status_line;
    draw_log_status(f, &status_app, chunks[2]);
}

fn restart_k8s_log_reader(
    log_handle: &mut Option<JoinHandle<()>>,
    log_app: &mut App,
    client: &Client,
    namespace: &str,
    pod: &str,
    container: &Option<String>,
    tail_lines: i64,
    line_tx: &mpsc::UnboundedSender<String>,
) {
    if let Some(h) = log_handle.take() {
        h.abort();
    }
    log_app.lines.clear();
    log_app.follow_tail = true;
    log_app.scroll_from_bottom = 0;
    let h = k8s::spawn_pod_log_reader(
        client.clone(),
        namespace.to_string(),
        pod.to_string(),
        container.clone(),
        tail_lines,
        line_tx.clone(),
    );
    *log_handle = Some(h);
}

async fn run_tui(cli: &Cli) -> io::Result<()> {
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<event::KeyEvent>();
    std::thread::spawn(move || loop {
        if let Ok(Event::Key(key)) = event::read() {
            if key.kind == KeyEventKind::Press {
                let _ = key_tx.send(key);
            }
        }
    });
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let client = match k8s::connect(cli.kubeconfig.as_deref(), cli.kube_context.as_deref()).await {
        Ok(c) => c,
        Err(err) => {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    Some(key) = key_rx.recv() => {
                        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                            break;
                        }
                    }
                    _ = tick.tick() => {}
                }
                terminal.draw(|f| {
                    let p = Paragraph::new(format!("Kubernetes client error:\n\n{err}\n\nPress q to exit."))
                        .block(Block::default().title(" k8s ").borders(Borders::ALL));
                    f.render_widget(p, f.area());
                })?;
            }
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            terminal.show_cursor()?;
            return Ok(());
        }
    };
    let namespace = cli.namespace.clone();
    let mut pods = match k8s::list_pod_names(&client, &namespace).await {
        Ok(p) => p,
        Err(err) => {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    Some(key) = key_rx.recv() => {
                        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                            break;
                        }
                    }
                    _ = tick.tick() => {}
                }
                terminal.draw(|f| {
                    let p = Paragraph::new(format!("List pods failed:\n\n{err}\n\nPress q to exit."))
                        .block(Block::default().title(" k8s ").borders(Borders::ALL));
                    f.render_widget(p, f.area());
                })?;
            }
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
            terminal.show_cursor()?;
            return Ok(());
        }
    };
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    let (reload_tx, mut reload_rx) = mpsc::unbounded_channel::<Result<Vec<String>, String>>();
    let mut selected: usize = 0;
    let mut log_app = App::new();
    let mut log_handle: Option<JoinHandle<()>> = None;
    let tail_i64 = i64::try_from(cli.tail_lines).unwrap_or(200);
    if let Some(pod) = pods.get(selected) {
        restart_k8s_log_reader(
            &mut log_handle,
            &mut log_app,
            &client,
            &namespace,
            pod,
            &cli.container,
            tail_i64,
            &line_tx,
        );
    }
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut banner = format!("local kube · ns/{namespace} · {} pod(s)", pods.len());
    let result = loop {
        tokio::select! {
            Some(line) = line_rx.recv() => {
                log_app.push_line(line);
            }
            Some(res) = reload_rx.recv() => {
                match res {
                    Ok(mut new_pods) => {
                        new_pods.sort();
                        pods = new_pods;
                        if pods.is_empty() {
                            if let Some(h) = log_handle.take() {
                                h.abort();
                            }
                            log_app.lines.clear();
                            selected = 0;
                        } else {
                            if selected >= pods.len() {
                                selected = pods.len() - 1;
                            }
                            let pod = pods[selected].clone();
                            restart_k8s_log_reader(
                                &mut log_handle,
                                &mut log_app,
                                &client,
                                &namespace,
                                &pod,
                                &cli.container,
                                tail_i64,
                                &line_tx,
                            );
                        }
                        banner = format!("local kube · ns/{namespace} · {} pod(s)", pods.len());
                    }
                    Err(err) => {
                        banner = format!("reload failed: {err}");
                    }
                }
            }
            Some(key) = key_rx.recv() => {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break Ok(());
                    }
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('g') => {
                        log_app.follow_tail = true;
                        log_app.scroll_from_bottom = 0;
                    }
                    KeyCode::Left => {
                        if !pods.is_empty() {
                            selected = selected.saturating_sub(1);
                            let pod = pods[selected].clone();
                            restart_k8s_log_reader(
                                &mut log_handle,
                                &mut log_app,
                                &client,
                                &namespace,
                                &pod,
                                &cli.container,
                                tail_i64,
                                &line_tx,
                            );
                            banner = format!("local kube · ns/{namespace} · {} pod(s)", pods.len());
                        }
                    }
                    KeyCode::Right => {
                        if !pods.is_empty() && selected + 1 < pods.len() {
                            selected += 1;
                            let pod = pods[selected].clone();
                            restart_k8s_log_reader(
                                &mut log_handle,
                                &mut log_app,
                                &client,
                                &namespace,
                                &pod,
                                &cli.container,
                                tail_i64,
                                &line_tx,
                            );
                            banner = format!("local kube · ns/{namespace} · {} pod(s)", pods.len());
                        }
                    }
                    KeyCode::Char('r') => {
                        let c = client.clone();
                        let ns = namespace.clone();
                        let tx = reload_tx.clone();
                        tokio::spawn(async move {
                            let r = k8s::list_pod_names(&c, &ns).await;
                            let _ = tx.send(r);
                        });
                    }
                    KeyCode::Up => log_app.scroll_up(1),
                    KeyCode::Down => log_app.scroll_down(1),
                    KeyCode::PageUp => {
                        let h = terminal
                            .size()?
                            .height
                            .saturating_sub(6) as usize;
                        log_app.scroll_up(h.max(1));
                    }
                    KeyCode::PageDown => {
                        let h = terminal
                            .size()?
                            .height
                            .saturating_sub(6) as usize;
                        log_app.scroll_down(h.max(1));
                    }
                    KeyCode::Home => {
                        log_app.follow_tail = false;
                        log_app.scroll_from_bottom = log_app.lines.len().saturating_sub(1);
                    }
                    KeyCode::End => {
                        log_app.follow_tail = true;
                        log_app.scroll_from_bottom = 0;
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {}
        }
        terminal.draw(|f| {
            draw_k8s_view(
                f,
                &namespace,
                &pods,
                selected,
                &log_app,
                &banner,
            );
        })?;
    };
    if let Some(h) = log_handle.take() {
        h.abort();
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();
    run_tui(&cli).await
}
