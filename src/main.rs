use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
    Terminal,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    fs::OpenOptions,
    io,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const USBIPD: &str = "usbipd.exe";
const AUTO_REFRESH: Duration = Duration::from_secs(5);
const AUTO_SCAN_REFRESH: Duration = Duration::from_millis(500);
const AUTO_RETRY_DELAY: Duration = Duration::from_secs(30);
const ELEVATION_SIDECAR: &[u8] = include_bytes!(env!("USBIPD_TUI_SIDECAR_PATH"));

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
struct Device {
    bus_id: Option<String>,
    #[serde(rename = "ClientIPAddress")]
    client_ip_address: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    instance_id: String,
    #[serde(default)]
    is_forced: bool,
    persisted_guid: Option<String>,
    stub_instance_id: Option<String>,
}

impl Device {
    fn vid_pid(&self) -> String {
        let id = self.instance_id.to_ascii_uppercase();
        let Some(vid_start) = id.find("VID_") else {
            return "—".into();
        };
        let vid = &id[vid_start + 4..];
        let Some(pid_start) = vid.find("&PID_") else {
            return "—".into();
        };
        let pid = &vid[pid_start + 5..];
        let vid = vid[..pid_start].split(['\\', '&']).next().unwrap_or("");
        let pid = pid.split(['\\', '&']).next().unwrap_or("");
        if vid.is_empty() || pid.is_empty() {
            "—".into()
        } else {
            format!("{vid}:{pid}")
        }
    }
    fn state(&self) -> &'static str {
        if self.client_ip_address.is_some() {
            "Attached"
        } else if self.persisted_guid.is_some() && self.bus_id.is_some() {
            "Bound"
        } else if self.persisted_guid.is_some() {
            "Persisted"
        } else {
            "Not Shared"
        }
    }
    fn row_key(&self) -> String {
        self.bus_id
            .clone()
            .or_else(|| self.persisted_guid.clone())
            .unwrap_or_else(|| self.instance_id.clone())
    }
    fn can_bind(&self) -> bool {
        self.state() == "Not Shared"
    }
    fn can_unbind(&self) -> bool {
        ["Bound", "Persisted", "Attached"].contains(&self.state())
    }
    fn can_attach(&self) -> bool {
        self.state() == "Bound"
    }
    fn can_detach(&self) -> bool {
        self.state() == "Attached"
    }
}

fn config_file() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("~/.config"));
    base.join("wsl2_usb").join("auto_attach_busids.json")
}

fn load_auto_ids(path: &PathBuf) -> HashSet<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect()
}
fn save_auto_ids(path: &PathBuf, ids: &HashSet<String>) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    fs::create_dir_all(dir)?;
    let mut sorted: Vec<_> = ids.iter().cloned().collect();
    sorted.sort();
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&sorted).unwrap()),
    )
}
fn fetch_devices() -> Result<Vec<Device>, String> {
    let out = Command::new(USBIPD)
        .arg("state")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not run {USBIPD} state: {error}"))?;
    #[derive(Deserialize)]
    struct State {
        #[serde(rename = "Devices")]
        #[serde(default)]
        devices: Vec<Device>,
    }
    serde_json::from_slice::<State>(&out.stdout)
        .map(|state| state.devices)
        .map_err(|error| {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            if stderr.is_empty() {
                format!("Could not parse {USBIPD} state JSON: {error}")
            } else {
                stderr
            }
        })
}
fn run_usbipd(args: &[String]) -> (i32, String, String) {
    if args.first().is_some_and(|a| a == "bind" || a == "unbind") {
        return run_elevated(args);
    }
    match Command::new(USBIPD).args(args).output() {
        Ok(out) => output_result(out),
        Err(e) => (1, String::new(), e.to_string()),
    }
}
fn output_result(out: Output) -> (i32, String, String) {
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).trim().into(),
        String::from_utf8_lossy(&out.stderr).trim().into(),
    )
}
fn clean_terminal_text(input: &str) -> String {
    let mut chars = input.chars().peekable();
    let mut cleaned = String::with_capacity(input.len());
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for code in chars.by_ref() {
                        if ('@'..='~').contains(&code) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(code) = chars.next() {
                        if code == '\u{7}' {
                            break;
                        }
                        if code == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if ch == '\r' {
            continue;
        } else if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        } else {
            cleaned.push(ch);
        }
    }
    cleaned
}
fn admin_effect_applied(args: &[String]) -> bool {
    let devices = fetch_devices().unwrap_or_default();
    if devices.is_empty() {
        return false;
    }
    match args.first().map(String::as_str) {
        Some("bind") => {
            if let Some(i) = args.iter().position(|a| a == "--busid") {
                return args.get(i + 1).is_some_and(|bus| {
                    devices
                        .iter()
                        .any(|d| d.bus_id.as_ref() == Some(bus) && d.persisted_guid.is_some())
                });
            }
            if let Some(i) = args.iter().position(|a| a == "--hardware-id") {
                return args.get(i + 1).is_some_and(|id| {
                    devices
                        .iter()
                        .any(|d| d.vid_pid() == *id && d.persisted_guid.is_some())
                });
            }
        }
        Some("unbind") => {
            if args.iter().any(|a| a == "--all") {
                return devices.iter().all(|d| d.persisted_guid.is_none());
            }
            if let Some(i) = args.iter().position(|a| a == "--busid") {
                return args.get(i + 1).is_some_and(|bus| {
                    devices
                        .iter()
                        .all(|d| d.bus_id.as_ref() != Some(bus) || d.persisted_guid.is_none())
                });
            }
            if let Some(i) = args.iter().position(|a| a == "--guid") {
                return args.get(i + 1).is_some_and(|guid| {
                    devices
                        .iter()
                        .all(|d| d.persisted_guid.as_ref() != Some(guid))
                });
            }
        }
        _ => {}
    }
    false
}
#[derive(Serialize)]
struct ElevationRequest<'a> {
    args: &'a [String],
}
#[derive(Deserialize)]
struct ElevationResponse {
    code: i32,
    stdout: String,
    stderr: String,
}

fn elevation_sidecar(base: &str) -> Result<(PathBuf, bool), String> {
    if let Some(path) = env::var_os("USBIPD_TUI_ELEVATE_SIDECAR") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok((path, false));
        }
    }
    let extracted = PathBuf::from("/mnt/c/Windows/Temp").join(format!("{base}.exe"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&extracted)
        .map_err(|error| format!("Could not extract Windows elevation helper: {error}"))?;
    use io::Write;
    if let Err(error) = file.write_all(ELEVATION_SIDECAR) {
        let _ = fs::remove_file(&extracted);
        return Err(format!("Could not write Windows elevation helper: {error}"));
    }
    Ok((extracted, true))
}

fn run_elevated(args: &[String]) -> (i32, String, String) {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let base = format!("wsl2_usb_{id}");
    let wsl_tmp = PathBuf::from("/mnt/c/Windows/Temp");
    let (sidecar, remove_sidecar) = match elevation_sidecar(&base) {
        Ok(sidecar) => sidecar,
        Err(error) => return (1, String::new(), error),
    };
    let request_path = wsl_tmp.join(format!("{base}.request.json"));
    let response_path = wsl_tmp.join(format!("{base}.response.json"));
    let request_win = format!("C:\\Windows\\Temp\\{base}.request.json");
    let response_win = format!("C:\\Windows\\Temp\\{base}.response.json");

    let request = match serde_json::to_vec(&ElevationRequest { args }) {
        Ok(request) => request,
        Err(error) => {
            if remove_sidecar {
                let _ = fs::remove_file(&sidecar);
            }
            return (
                1,
                String::new(),
                format!("Could not encode elevation request: {error}"),
            );
        }
    };
    if let Err(error) = fs::write(&request_path, request) {
        if remove_sidecar {
            let _ = fs::remove_file(&sidecar);
        }
        return (
            1,
            String::new(),
            format!("Could not write elevation request: {error}"),
        );
    }

    let launch = Command::new(&sidecar)
        .arg(&request_win)
        .arg(&response_win)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match launch {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&request_path);
            if remove_sidecar {
                let _ = fs::remove_file(&sidecar);
            }
            return (
                1,
                String::new(),
                format!("Could not start Windows elevation helper: {error}"),
            );
        }
    };

    let deadline = Instant::now() + Duration::from_secs(180);
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(200)),
            Ok(None) => {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&request_path);
                if remove_sidecar {
                    let _ = fs::remove_file(&sidecar);
                }
                return (
                    1,
                    String::new(),
                    format!("Could not wait for elevation helper: {error}"),
                );
            }
        }
    }

    let result = fs::read(&response_path)
        .ok()
        .and_then(|data| serde_json::from_slice::<ElevationResponse>(&data).ok());
    let _ = fs::remove_file(&request_path);
    let _ = fs::remove_file(&response_path);
    if remove_sidecar {
        let _ = fs::remove_file(&sidecar);
    }
    if let Some(result) = result {
        return (result.code, result.stdout, result.stderr);
    }
    if timed_out {
        return (
            1,
            String::new(),
            "Timed out waiting for UAC approval or usbipd.".into(),
        );
    }
    if admin_effect_applied(args) {
        return (0, String::new(), String::new());
    }
    (
        1,
        String::new(),
        "Elevation helper did not return a result. UAC may have been cancelled.".into(),
    )
}

enum Message {
    Devices(Result<Vec<Device>, String>),
    Command(String, i32, String, String),
    Auto(String, Vec<(String, i32, String, String)>),
}
struct App {
    devices: Vec<Device>,
    selected: usize,
    show_persisted: bool,
    auto_ids: HashSet<String>,
    config: PathBuf,
    logs: VecDeque<String>,
    status: String,
    last_refresh: Instant,
    last_scan: Instant,
    pending_fetch: bool,
    auto_inflight: HashSet<String>,
    last_attempt: HashMap<String, Instant>,
    tx: Sender<Message>,
    rx: Receiver<Message>,
}
impl App {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let config = config_file();
        let auto_ids = load_auto_ids(&config);
        let mut app = Self {
            devices: vec![],
            selected: 0,
            show_persisted: true,
            auto_ids,
            config,
            logs: VecDeque::new(),
            status: "Loading devices...".into(),
            last_refresh: Instant::now() - AUTO_REFRESH,
            last_scan: Instant::now(),
            pending_fetch: false,
            auto_inflight: HashSet::new(),
            last_attempt: HashMap::new(),
            tx,
            rx,
        };
        app.log("Loading devices…");
        app.refresh();
        app
    }
    fn log(&mut self, msg: impl Into<String>) {
        let cleaned = clean_terminal_text(&msg.into());
        for line in cleaned.lines().filter(|line| !line.trim().is_empty()) {
            self.logs.push_back(line.trim().to_owned());
            while self.logs.len() > 100 {
                self.logs.pop_front();
            }
        }
    }
    fn refresh(&mut self) {
        if self.pending_fetch {
            return;
        }
        self.pending_fetch = true;
        self.status = "Refreshing device list…".into();
        self.last_refresh = Instant::now();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Message::Devices(fetch_devices()));
        });
    }
    fn all_visible(&self) -> Vec<&Device> {
        self.devices
            .iter()
            .filter(|d| d.bus_id.is_some() || self.show_persisted)
            .collect()
    }
    fn selected(&self) -> Option<&Device> {
        self.all_visible().get(self.selected).copied()
    }
    fn act(&mut self, name: &str) {
        let dev = self.selected().cloned();
        let mut args: Vec<String> = vec![];
        match name {
            "bind" => {
                if let Some(d) = dev.filter(Device::can_bind) {
                    args = if let Some(bus) = d.bus_id {
                        vec!["bind".into(), "--busid".into(), bus]
                    } else {
                        vec!["bind".into(), "--hardware-id".into(), d.vid_pid()]
                    };
                }
            }
            "unbind" => {
                if let Some(d) = dev.filter(Device::can_unbind) {
                    args = if let Some(bus) = d.bus_id {
                        vec!["unbind".into(), "--busid".into(), bus]
                    } else if let Some(guid) = d.persisted_guid {
                        vec!["unbind".into(), "--guid".into(), guid]
                    } else {
                        vec![]
                    };
                }
            }
            "attach" => {
                if let Some(d) = dev.filter(Device::can_attach) {
                    if let Some(bus) = d.bus_id {
                        args = vec!["attach".into(), "--busid".into(), bus, "--wsl".into()];
                    }
                }
            }
            "detach" => {
                if let Some(d) = dev.filter(Device::can_detach) {
                    if let Some(bus) = d.bus_id {
                        args = vec!["detach".into(), "--busid".into(), bus];
                    }
                }
            }
            "detach-all" => args = vec!["detach".into(), "--all".into()],
            "unbind-all" => args = vec!["unbind".into(), "--all".into()],
            _ => {}
        }
        if args.is_empty() {
            return;
        }
        self.start_command(args);
    }
    fn start_command(&mut self, args: Vec<String>) {
        let desc = format!("$ {} {}", USBIPD, args.join(" "));
        self.log(desc);
        if matches!(args.first().map(String::as_str), Some("bind" | "unbind")) {
            self.log("Requires elevation — Windows UAC prompt will appear");
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let (rc, out, err) = run_usbipd(&args);
            let _ = tx.send(Message::Command(args.join(" "), rc, out, err));
        });
    }
    fn toggle_auto(&mut self) {
        let Some(bus) = self.selected().and_then(|d| d.bus_id.clone()) else {
            return;
        };
        if !self.auto_ids.remove(&bus) {
            self.auto_ids.insert(bus.clone());
            self.last_attempt.remove(&bus);
            self.log(format!("Auto bus-id enabled: {bus}"));
        } else {
            self.auto_inflight.remove(&bus);
            self.log(format!("Auto bus-id disabled: {bus}"));
        }
        if let Err(e) = save_auto_ids(&self.config, &self.auto_ids) {
            self.log(format!("Could not save auto bus-id rules: {e}"));
        }
    }
    fn toggle_persisted(&mut self) {
        self.show_persisted = !self.show_persisted;
        self.selected = 0;
        self.log(format!(
            "Persisted (disconnected) devices: {}",
            if self.show_persisted {
                "shown"
            } else {
                "hidden"
            }
        ));
    }
    fn handle_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Message::Devices(result) => {
                    self.pending_fetch = false;
                    self.last_refresh = Instant::now();
                    let d = match result {
                        Ok(devices) => devices,
                        Err(error) => {
                            let status = format!("Could not read USB devices: {error}");
                            if self.status != status {
                                self.log(status.clone());
                            }
                            self.status = status;
                            continue;
                        }
                    };
                    let selected_key = self.selected().map(Device::row_key);
                    self.devices = d;
                    if let Some(key) = selected_key {
                        if let Some(index) = self
                            .all_visible()
                            .iter()
                            .position(|device| device.row_key() == key)
                        {
                            self.selected = index;
                        }
                    }
                    self.selected = self
                        .selected
                        .min(self.all_visible().len().saturating_sub(1));
                    let connected = self.devices.iter().filter(|x| x.bus_id.is_some()).count();
                    self.status = if self.devices.is_empty() {
                        "No USB devices found.".into()
                    } else if connected == 0 {
                        "Only persisted devices were found. Press p to show them.".into()
                    } else {
                        format!(
                            "{} device(s) found · refreshes every 5 seconds",
                            self.devices.len()
                        )
                    };
                }
                Message::Command(args, rc, out, err) => {
                    if !out.is_empty() {
                        self.log(out);
                    }
                    if !err.is_empty() {
                        self.log(if rc == 0 {
                            format!("INFO: {err}")
                        } else {
                            format!("ERROR: {err}")
                        });
                    }
                    self.log(if rc == 0 {
                        "✓ OK".into()
                    } else {
                        format!("✗ Failed (exit {rc})")
                    });
                    self.log(format!("Completed: {args}"));
                    self.status = if rc == 0 {
                        format!("Completed: {args}")
                    } else {
                        format!("Command failed with exit code {rc}")
                    };
                    self.refresh();
                }
                Message::Auto(bus, results) => {
                    self.auto_inflight.remove(&bus);
                    for (action, rc, out, err) in results {
                        if !out.is_empty() {
                            self.log(out);
                        }
                        if !err.is_empty() {
                            self.log(if rc == 0 {
                                format!("INFO: {err}")
                            } else {
                                format!("ERROR: {err}")
                            });
                        }
                        self.log(if rc == 0 {
                            format!("Auto bus-id {bus}: {action}")
                        } else {
                            format!("Auto bus-id {bus}: {action} failed (exit {rc})")
                        });
                    }
                    self.refresh();
                }
            }
        }
    }
    fn maybe_auto_attach(&mut self) {
        let now = Instant::now();
        let candidates: Vec<(String, String)> = self
            .devices
            .iter()
            .filter_map(|d| {
                let bus = d.bus_id.clone()?;
                if !self.auto_ids.contains(&bus)
                    || d.client_ip_address.is_some()
                    || self.auto_inflight.contains(&bus)
                    || self
                        .last_attempt
                        .get(&bus)
                        .is_some_and(|t| now.duration_since(*t) < AUTO_RETRY_DELAY)
                {
                    return None;
                }
                Some((bus, d.state().to_owned()))
            })
            .collect();
        for (bus, state) in candidates {
            self.auto_inflight.insert(bus.clone());
            self.last_attempt.insert(bus.clone(), now);
            self.log(format!("Auto bus-id {bus}: detected {state} device"));
            let tx = self.tx.clone();
            thread::spawn(move || {
                let mut results = vec![];
                if state == "Not Shared" {
                    let (rc, out, err) =
                        run_usbipd(&["bind".into(), "--busid".into(), bus.clone()]);
                    results.push(("bind".into(), rc, out, err));
                    if rc != 0 {
                        let _ = tx.send(Message::Auto(bus, results));
                        return;
                    }
                }
                let (rc, out, err) = run_usbipd(&[
                    "attach".into(),
                    "--busid".into(),
                    bus.clone(),
                    "--wsl".into(),
                ]);
                results.push(("attached".into(), rc, out, err));
                let _ = tx.send(Message::Auto(bus, results));
            });
        }
    }
}

fn state_style(state: &str) -> Style {
    match state {
        "Attached" => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        "Bound" => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        "Persisted" => Style::default().fg(Color::Gray),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn card<'a>(title: &'a str, value: String, color: Color) -> Paragraph<'a> {
    Paragraph::new(Line::from(Span::styled(
        format!(" {value}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(format!(" {title} ")),
    )
}

fn render<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &App) -> io::Result<()> {
    terminal.draw(|frame| {
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(5),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(frame.area());

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    " WSL2 USB ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "DEVICE MANAGER",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  usbipd-win", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(Span::styled(&app.status, Style::default().fg(Color::Gray))),
        ])
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(header, root[0]);

        let connected = app
            .devices
            .iter()
            .filter(|device| device.bus_id.is_some())
            .count();
        let persisted = app.devices.len().saturating_sub(connected);
        let attached = app
            .devices
            .iter()
            .filter(|device| device.state() == "Attached")
            .count();
        let auto_count = app.auto_ids.len();
        let cards = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(root[1]);
        frame.render_widget(
            card("CONNECTED", connected.to_string(), Color::Cyan),
            cards[0],
        );
        frame.render_widget(
            card("ATTACHED TO WSL", attached.to_string(), Color::Green),
            cards[1],
        );
        frame.render_widget(
            card("PERSISTED", persisted.to_string(), Color::Yellow),
            cards[2],
        );
        frame.render_widget(
            card("AUTO-ATTACH RULES", auto_count.to_string(), Color::Magenta),
            cards[3],
        );

        let wide = frame.area().width >= 108;
        let (device_area, detail_area) = if wide {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
                .split(root[2]);
            (split[0], split[1])
        } else {
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(6), Constraint::Length(4)])
                .split(root[2]);
            (split[0], split[1])
        };

        let visible = app.all_visible();
        if visible.is_empty() {
            let message = if app.status.starts_with("Could not read USB devices:") {
                "Could not load USB device state\n\nCheck that usbipd-win is installed and usbipd.exe is available in WSL."
            } else if app.devices.is_empty() {
                "No USB devices found\n\nConnect a device to Windows, then press [r] to refresh."
            } else {
                "Only persisted devices are available\n\nPress [p] to show disconnected devices."
            };
            frame.render_widget(
                Paragraph::new(message)
                    .alignment(ratatui::layout::Alignment::Center)
                    .block(
                        Block::default()
                            .title(" DEVICES ")
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded),
                    ),
                device_area,
            );
        } else {
            let compact = device_area.width < 82;
            let rows: Vec<Row> = visible
                .iter()
                .map(|device| {
                    let mut cells = vec![
                        Cell::from(device.bus_id.as_deref().unwrap_or("—")),
                        Cell::from(device.vid_pid()),
                        Cell::from(truncate(&device.description, if compact { 18 } else { 42 })),
                        Cell::from(device.state()).style(state_style(device.state())),
                    ];
                    if !compact {
                        cells.push(Cell::from(
                            device.client_ip_address.as_deref().unwrap_or("—"),
                        ));
                        cells.push(Cell::from(if device.is_forced { "F" } else { "" }));
                        cells.push(Cell::from(
                            if device
                                .bus_id
                                .as_ref()
                                .is_some_and(|bus| app.auto_ids.contains(bus))
                            {
                                "●"
                            } else {
                                ""
                            },
                        ));
                    }
                    Row::new(cells).height(1)
                })
                .collect();
            let header_cells = if compact {
                vec!["BUS ID", "VID:PID", "DEVICE", "STATE"]
            } else {
                vec![
                    "BUS ID", "VID:PID", "DEVICE", "STATE", "CLIENT", "F", "AUTO",
                ]
            };
            let widths = if compact {
                vec![
                    Constraint::Length(9),
                    Constraint::Length(11),
                    Constraint::Min(14),
                    Constraint::Length(12),
                ]
            } else {
                vec![
                    Constraint::Length(9),
                    Constraint::Length(11),
                    Constraint::Min(18),
                    Constraint::Length(12),
                    Constraint::Length(16),
                    Constraint::Length(2),
                    Constraint::Length(5),
                ]
            };
            let table = Table::new(rows, widths)
                .header(
                    Row::new(header_cells).style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .row_highlight_style(
                    Style::default()
                        .bg(Color::Rgb(35, 48, 66))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("› ")
                .block(
                    Block::default()
                        .title(" DEVICES ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray)),
                );
            let mut table_state = TableState::default();
            table_state.select(Some(app.selected.min(visible.len().saturating_sub(1))));
            frame.render_stateful_widget(table, device_area, &mut table_state);
        }

        let detail_lines = if let Some(device) = app.selected() {
            let auto = device
                .bus_id
                .as_ref()
                .is_some_and(|bus| app.auto_ids.contains(bus));
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("State  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(device.state(), state_style(device.state())),
                ]),
                Line::from(vec![
                    Span::styled("VID:PID  ", Style::default().fg(Color::DarkGray)),
                    Span::raw(device.vid_pid()),
                    Span::raw("    "),
                    Span::styled("Bus ID  ", Style::default().fg(Color::DarkGray)),
                    Span::raw(device.bus_id.as_deref().unwrap_or("—")),
                ]),
                Line::from(vec![
                    Span::styled("Client  ", Style::default().fg(Color::DarkGray)),
                    Span::raw(
                        device
                            .client_ip_address
                            .as_deref()
                            .unwrap_or("Not attached"),
                    ),
                    Span::raw("    "),
                    Span::styled("Auto  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if auto { "ON" } else { "OFF" },
                        Style::default().fg(if auto { Color::Green } else { Color::DarkGray }),
                    ),
                ]),
            ];
            if device.description.is_empty() {
                lines.insert(
                    0,
                    Line::from(Span::styled(
                        "No description",
                        Style::default().fg(Color::Gray),
                    )),
                );
            } else {
                lines.insert(
                    0,
                    Line::from(Span::styled(
                        truncate(&device.description, 70),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                );
            }
            lines
        } else {
            vec![Line::from(Span::styled(
                "Select a device to see details.",
                Style::default().fg(Color::Gray),
            ))]
        };
        frame.render_widget(
            Paragraph::new(detail_lines)
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .title(" SELECTED DEVICE ")
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray)),
                ),
            detail_area,
        );

        let logs: Vec<Line> = app
            .logs
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|entry| {
                let style = if entry.contains("ERROR")
                    || entry.contains("failed")
                    || entry.contains("Failed")
                {
                    Style::default().fg(Color::Red)
                } else if entry.contains("OK") || entry.contains("attached") {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Line::from(Span::styled(entry.as_str(), style))
            })
            .collect();
        frame.render_widget(
            Paragraph::new(logs).wrap(Wrap { trim: true }).block(
                Block::default()
                    .title(" ACTIVITY ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            root[3],
        );

        let actions = app
            .selected()
            .map(|device| {
                let mut available = Vec::new();
                if device.can_bind() {
                    available.push("[b] Bind");
                }
                if device.can_attach() {
                    available.push("[a] Attach");
                }
                if device.can_detach() {
                    available.push("[d] Detach");
                }
                if device.can_unbind() {
                    available.push("[u] Unbind");
                }
                if device.bus_id.is_some() {
                    available.push("[x] Auto-attach");
                }
                available.join("   ")
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "No actions available for this selection".into());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ACTIONS  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    actions,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            root[4],
        );
        frame.render_widget(
            Paragraph::new(
                "↑/↓ select  r refresh  D detach all  U unbind all  p persisted  q quit",
            )
            .style(Style::default().fg(Color::DarkGray)),
            root[5],
        );
    })?;
    Ok(())
}
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.into()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}
fn key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Up | KeyCode::Char('k') => app.selected = app.selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            let n = app.all_visible().len();
            if n > 0 {
                app.selected = (app.selected + 1).min(n - 1);
            }
        }
        KeyCode::Char('r') => {
            app.log("Refreshing…");
            app.refresh();
        }
        KeyCode::Char('b') => app.act("bind"),
        KeyCode::Char('a') => app.act("attach"),
        KeyCode::Char('d') => app.act("detach"),
        KeyCode::Char('u') => app.act("unbind"),
        KeyCode::Char('D') => app.act("detach-all"),
        KeyCode::Char('U') => app.act("unbind-all"),
        KeyCode::Char('x') => app.toggle_auto(),
        KeyCode::Char('p') => app.toggle_persisted(),
        _ => {}
    }
    false
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("usbipd-tui {VERSION}");
        return Ok(());
    }
    if !args.is_empty() {
        eprintln!("Usage: usbipd-tui [--version]");
        std::process::exit(2);
    }
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        let mut quit = false;
        while !quit {
            app.handle_messages();
            let now = Instant::now();
            if now.duration_since(app.last_refresh) >= AUTO_REFRESH {
                app.refresh();
            }
            if !app.auto_ids.is_empty() && now.duration_since(app.last_scan) >= AUTO_SCAN_REFRESH {
                app.last_scan = now;
                app.refresh();
            }
            app.maybe_auto_attach();
            render(&mut terminal, &app)?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(k) = event::read()? {
                    quit = key(&mut app, k);
                }
            }
        }
        Ok(())
    })();
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()?;
    result
}
fn main() {
    if let Err(e) = run() {
        let _ = crossterm::terminal::disable_raw_mode();
        eprintln!("usbipd-tui: {e}");
        std::process::exit(1);
    }
}
