//! The live dashboard. A background thread polls the node (on an interval or on
//! demand) and streams updates to the UI thread over a channel; the UI thread
//! owns rendering and input.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::model::{GuestIps, Vm};
use crate::node::{CreateSpec, Node};

const REFRESH: Duration = Duration::from_secs(3);

/// A lifecycle action that can be taken on a VM.
#[derive(Clone, Copy, PartialEq)]
enum ActionKind {
    Start,
    Stop,
    Destroy,
}

impl ActionKind {
    /// Lower-case verb, e.g. for the confirmation prompt.
    fn verb(self) -> &'static str {
        match self {
            ActionKind::Start => "start",
            ActionKind::Stop => "stop",
            ActionKind::Destroy => "destroy",
        }
    }

    /// Present-progressive label shown while the command runs.
    fn gerund(self) -> &'static str {
        match self {
            ActionKind::Start => "starting",
            ActionKind::Stop => "stopping",
            ActionKind::Destroy => "destroying",
        }
    }
}

/// A pending confirmation modal for a lifecycle action on one VM.
struct Confirm {
    kind: ActionKind,
    vmid: u32,
    name: String,
    /// True once the user confirmed and the command was dispatched.
    running: bool,
}

/// How a create-form field is edited.
#[derive(PartialEq)]
enum FieldKind {
    /// Free text.
    Text,
    /// Digits only.
    Uint,
    /// A yes/no toggle (`value` is "yes" or "no").
    Bool,
}

/// One labelled field in the create wizard.
struct Field {
    label: &'static str,
    value: String,
    kind: FieldKind,
    /// Hint shown when the value is empty (e.g. "(next available)").
    placeholder: &'static str,
}

impl Field {
    fn text(label: &'static str, value: impl Into<String>, placeholder: &'static str) -> Field {
        Field {
            label,
            value: value.into(),
            kind: FieldKind::Text,
            placeholder,
        }
    }
    fn uint(label: &'static str, value: impl Into<String>) -> Field {
        Field {
            label,
            value: value.into(),
            kind: FieldKind::Uint,
            placeholder: "",
        }
    }
    fn boolean(label: &'static str, on: bool) -> Field {
        Field {
            label,
            value: if on { "yes" } else { "no" }.into(),
            kind: FieldKind::Bool,
            placeholder: "",
        }
    }
    fn is_on(&self) -> bool {
        self.value == "yes"
    }
}

/// Where the create wizard is in its lifecycle.
enum FormState {
    /// Filling in fields; `Some` holds a validation message.
    Editing(Option<String>),
    /// Dispatched to the node; awaiting the result.
    Submitting,
    /// Finished — showing the outcome summary.
    Done(CreateOutcome),
}

/// The create wizard overlay.
struct CreateForm {
    fields: Vec<Field>,
    focus: usize,
    state: FormState,
    /// The next free VM id, resolved async to hint the (blank) VM ID field.
    next_id: Option<u32>,
}

/// The result of a successful create, shown on the wizard's final screen.
struct CreateOutcome {
    vmid: u32,
    name: String,
    user: String,
    started: bool,
    ips: GuestIps,
    /// The `~/.ssh/config` alias written for a Tailscale VM, if any.
    ssh_alias: Option<String>,
}

/// A request from the UI to the background fetcher.
enum Req {
    /// Re-fetch the full VM list.
    Refresh,
    /// Resolve the guest-agent IPs for one VM.
    Ips(u32),
    /// Run a lifecycle action, then refresh.
    Action { vmid: u32, kind: ActionKind },
    /// Clone + configure a new VM, then (if started) wait for its IPs.
    Create(CreateSpec),
    /// Resolve the next free VM id, to hint the create form.
    NextId,
}

/// A message from the background fetcher to the UI.
enum Update {
    Vms(Vec<Vm>),
    Error(String),
    Ips {
        vmid: u32,
        result: std::result::Result<GuestIps, String>,
    },
    ActionDone {
        kind: ActionKind,
        result: std::result::Result<(), String>,
    },
    CreateDone(std::result::Result<CreateOutcome, String>),
    NextId(u32),
}

/// State of the guest-agent IP lookup for the open detail view.
enum IpState {
    Loading,
    NotRunning,
    Unavailable(String),
    Loaded(GuestIps),
}

/// The open detail overlay: pinned to a VM id, with its IP lookup progressing
/// asynchronously via the background fetcher.
struct DetailView {
    vmid: u32,
    ips: IpState,
}

struct App {
    cfg: Config,
    vms: Vec<Vm>,
    state: TableState,
    error: Option<String>,
    last_update: Option<Instant>,
    loading: bool,
    detail: Option<DetailView>,
    confirm: Option<Confirm>,
    form: Option<CreateForm>,
    req_tx: Sender<Req>,
}

impl App {
    fn selected_vm(&self) -> Option<&Vm> {
        self.state.selected().and_then(|i| self.vms.get(i))
    }

    /// Open the detail overlay for the selected VM, kicking off an async IP
    /// lookup when it's running (the guest agent is unreachable otherwise).
    fn open_detail(&mut self) {
        let Some(vm) = self.selected_vm() else {
            return;
        };
        let vmid = vm.vmid;
        let ips = if vm.is_running() {
            let _ = self.req_tx.send(Req::Ips(vmid));
            IpState::Loading
        } else {
            IpState::NotRunning
        };
        self.detail = Some(DetailView { vmid, ips });
    }

    /// Look up the VM backing the open detail view by id, so its cpu/mem/status
    /// stay live as background refreshes replace the VM list.
    fn detail_vm(&self) -> Option<&Vm> {
        let d = self.detail.as_ref()?;
        self.vms.iter().find(|v| v.vmid == d.vmid)
    }

    /// Open a confirmation modal for a lifecycle action on the selected VM.
    /// Start/stop are meaningless on templates, so only destroy is offered there.
    fn begin_action(&mut self, kind: ActionKind) {
        let Some(vm) = self.selected_vm() else {
            return;
        };
        if vm.is_template && kind != ActionKind::Destroy {
            return;
        }
        self.confirm = Some(Confirm {
            kind,
            vmid: vm.vmid,
            name: vm.name.clone(),
            running: false,
        });
    }

    /// Dispatch the confirmed action to the fetcher thread; the modal stays up
    /// showing progress until an `ActionDone` arrives.
    fn confirm_action(&mut self) {
        if let Some(c) = &mut self.confirm {
            if c.running {
                return; // already dispatched
            }
            c.running = true;
            let _ = self.req_tx.send(Req::Action {
                vmid: c.vmid,
                kind: c.kind,
            });
        }
    }

    /// Open the create wizard, seeding fields from the config defaults.
    fn open_form(&mut self) {
        let c = &self.cfg;
        self.form = Some(CreateForm {
            fields: vec![
                Field::text("Name", "", "required"),
                Field::text("VM ID", "", "(next available)"),
                Field::uint("Cores", c.default_cores.to_string()),
                Field::uint("Memory (MB)", c.default_memory.to_string()),
                Field::uint("Disk (GB)", c.default_disk.to_string()),
                Field::text("IP", "dhcp", "dhcp or CIDR"),
                Field::text("Gateway", "", "(static IP only)"),
                Field::text("Nameserver", "", "(optional)"),
                Field::text("User", c.ci_user.clone(), "cloud-init user"),
                Field::text("Tags", "", "(optional, ;-separated)"),
                Field::boolean("Full clone", c.default_full_clone),
                Field::boolean("Start after create", true),
                Field::boolean("Join Tailscale", c.tailscale_default),
            ],
            focus: 0,
            state: FormState::Editing(None),
            next_id: None,
        });
        // Resolve the next free id in the background to hint the VM ID field.
        let _ = self.req_tx.send(Req::NextId);
    }

    fn form_move(&mut self, delta: isize) {
        if let Some(f) = &mut self.form {
            let n = f.fields.len() as isize;
            f.focus = ((f.focus as isize + delta).rem_euclid(n)) as usize;
        }
    }

    /// Feed a typed character to the focused field.
    fn form_char(&mut self, ch: char) {
        if let Some(f) = &mut self.form {
            let field = &mut f.fields[f.focus];
            match field.kind {
                FieldKind::Text => field.value.push(ch),
                FieldKind::Uint if ch.is_ascii_digit() => field.value.push(ch),
                FieldKind::Bool if matches!(ch, 'y' | 'n' | ' ') => {
                    field.value = if ch == 'y' {
                        "yes"
                    } else if ch == 'n' {
                        "no"
                    } else if field.value == "yes" {
                        "no"
                    } else {
                        "yes"
                    }
                    .into();
                }
                _ => {}
            }
        }
    }

    fn form_backspace(&mut self) {
        if let Some(f) = &mut self.form {
            let field = &mut f.fields[f.focus];
            if field.kind != FieldKind::Bool {
                field.value.pop();
            }
        }
    }

    /// Look up a field's trimmed value by label.
    fn field_value(form: &CreateForm, label: &str) -> String {
        form.fields
            .iter()
            .find(|f| f.label == label)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default()
    }

    fn field_on(form: &CreateForm, label: &str) -> bool {
        form.fields.iter().find(|f| f.label == label).is_some_and(Field::is_on)
    }

    /// Validate the form and, if good, dispatch a `Req::Create` and move to the
    /// Submitting state. On failure, stash a validation message in the form.
    fn submit_form(&mut self) {
        let Some(form) = &self.form else { return };
        let spec = match self.build_spec(form) {
            Ok(s) => s,
            Err(msg) => {
                if let Some(f) = &mut self.form {
                    f.state = FormState::Editing(Some(msg));
                }
                return;
            }
        };
        let _ = self.req_tx.send(Req::Create(spec));
        if let Some(f) = &mut self.form {
            f.state = FormState::Submitting;
        }
    }

    /// Turn the form's fields into a `CreateSpec`, reading the local SSH public
    /// key (`<identity_file>.pub`) to install into the VM.
    fn build_spec(&self, form: &CreateForm) -> std::result::Result<CreateSpec, String> {
        let name = Self::field_value(form, "Name");
        if name.is_empty() {
            return Err("Name is required".into());
        }
        let vmid = {
            let raw = Self::field_value(form, "VM ID");
            if raw.is_empty() {
                None
            } else {
                Some(raw.parse::<u32>().map_err(|_| "VM ID must be a number".to_string())?)
            }
        };
        let parse_u32 = |label: &str| -> std::result::Result<u32, String> {
            Self::field_value(form, label)
                .parse::<u32>()
                .map_err(|_| format!("{label} must be a number"))
        };
        let cores = parse_u32("Cores")?;
        let memory = parse_u32("Memory (MB)")?;
        let disk = parse_u32("Disk (GB)")?;
        let ip = Self::field_value(form, "IP");
        if ip.is_empty() {
            return Err("IP is required (dhcp or a CIDR)".into());
        }
        let opt = |label: &str| {
            let v = Self::field_value(form, label);
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        };
        let user = Self::field_value(form, "User");
        let tailscale = Self::field_on(form, "Join Tailscale");
        if tailscale && self.cfg.tailscale_authkey.is_none() {
            return Err("Join Tailscale needs PVE_TAILSCALE_AUTHKEY in the config".into());
        }
        let ssh_pubkey = self.read_pubkey()?;

        Ok(CreateSpec {
            name,
            vmid,
            cores,
            memory,
            disk,
            ip,
            gateway: opt("Gateway"),
            nameserver: opt("Nameserver"),
            ci_user: if user.is_empty() { self.cfg.ci_user.clone() } else { user },
            ssh_pubkey,
            full_clone: Self::field_on(form, "Full clone"),
            start: Self::field_on(form, "Start after create"),
            tags: opt("Tags"),
            tailscale,
        })
    }

    /// Read `<identity_file>.pub` — the public key installed into new VMs.
    fn read_pubkey(&self) -> std::result::Result<String, String> {
        let identity = self
            .cfg
            .identity_file
            .as_ref()
            .ok_or("no SSH identity configured (set PVE_IDENTITY_FILE)")?;
        let pub_path = format!("{}.pub", identity.display());
        std::fs::read_to_string(&pub_path)
            .map_err(|e| format!("cannot read SSH public key {pub_path}: {e}"))
    }

    fn next(&mut self) {
        if self.vms.is_empty() {
            return;
        }
        let i = self.state.selected().map_or(0, |i| (i + 1) % self.vms.len());
        self.state.select(Some(i));
    }

    fn prev(&mut self) {
        if self.vms.is_empty() {
            return;
        }
        let i = self
            .state
            .selected()
            .map_or(0, |i| if i == 0 { self.vms.len() - 1 } else { i - 1 });
        self.state.select(Some(i));
    }

    fn request_refresh(&mut self) {
        self.loading = true;
        let _ = self.req_tx.send(Req::Refresh);
        // If a running VM's detail is open, refresh its IPs too.
        if let Some(d) = &mut self.detail {
            if matches!(d.ips, IpState::Loaded(_) | IpState::Unavailable(_)) {
                d.ips = IpState::Loading;
                let _ = self.req_tx.send(Req::Ips(d.vmid));
            }
        }
    }
}

pub fn run(cfg: Config) -> Result<()> {
    let cfg_for_app = cfg.clone();
    let (data_tx, data_rx) = mpsc::channel::<Update>();
    let (req_tx, req_rx) = mpsc::channel::<Req>();

    // Background fetcher: an initial fetch, then serve refreshes (on request or
    // the interval) and on-demand guest-IP lookups without forcing a full poll.
    thread::spawn(move || {
        let node_ip_timeout = cfg.ip_timeout;
        let node = Node::new(cfg);
        let initial = match node.vms() {
            Ok(vms) => Update::Vms(vms),
            Err(e) => Update::Error(e.to_string()),
        };
        if data_tx.send(initial).is_err() {
            return; // UI gone
        }
        loop {
            match req_rx.recv_timeout(REFRESH) {
                Ok(Req::Refresh) | Err(RecvTimeoutError::Timeout) => {
                    let msg = match node.vms() {
                        Ok(vms) => Update::Vms(vms),
                        Err(e) => Update::Error(e.to_string()),
                    };
                    if data_tx.send(msg).is_err() {
                        break;
                    }
                }
                Ok(Req::Ips(vmid)) => {
                    let result = node.guest_ips(vmid).map_err(|e| e.to_string());
                    if data_tx.send(Update::Ips { vmid, result }).is_err() {
                        break;
                    }
                }
                Ok(Req::NextId) => {
                    // Best-effort hint; silently skip if it fails.
                    if let Ok(id) = node.next_id() {
                        if data_tx.send(Update::NextId(id)).is_err() {
                            break;
                        }
                    }
                }
                Ok(Req::Action { vmid, kind }) => {
                    let result = match kind {
                        ActionKind::Start => node.start_vm(vmid),
                        ActionKind::Stop => node.stop_vm(vmid),
                        ActionKind::Destroy => node.destroy_vm(vmid),
                    }
                    .map_err(|e| e.to_string());
                    if data_tx.send(Update::ActionDone { kind, result }).is_err() {
                        break;
                    }
                    // Reflect the new state immediately.
                    let refreshed = match node.vms() {
                        Ok(vms) => Update::Vms(vms),
                        Err(e) => Update::Error(e.to_string()),
                    };
                    if data_tx.send(refreshed).is_err() {
                        break;
                    }
                }
                Ok(Req::Create(spec)) => {
                    let want_ts = spec.tailscale;
                    let start = spec.start;
                    let name = spec.name.clone();
                    let user = spec.ci_user.clone();
                    let done = match node.create(&spec) {
                        Ok(vmid) => {
                            // Show the new VM in the list right away.
                            if let Ok(vms) = node.vms() {
                                let _ = data_tx.send(Update::Vms(vms));
                            }
                            let ips = if start {
                                node.wait_for_ips(
                                    vmid,
                                    want_ts,
                                    Duration::from_secs(node_ip_timeout),
                                )
                            } else {
                                GuestIps::default()
                            };
                            // Write the local ssh-config alias for a started
                            // Tailscale VM so `ssh <name>` works.
                            let ssh_alias = if start && want_ts {
                                node.write_vm_ssh_config(&name)
                            } else {
                                None
                            };
                            Ok(CreateOutcome {
                                vmid,
                                name,
                                user,
                                started: start,
                                ips,
                                ssh_alias,
                            })
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    if data_tx.send(Update::CreateDone(done)).is_err() {
                        break;
                    }
                    // Final refresh so status/IP settle.
                    if let Ok(vms) = node.vms() {
                        let _ = data_tx.send(Update::Vms(vms));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    let mut app = App {
        cfg: cfg_for_app,
        vms: Vec::new(),
        state: TableState::default(),
        error: None,
        last_update: None,
        loading: true,
        detail: None,
        confirm: None,
        form: None,
        req_tx,
    };

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &data_rx);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    data_rx: &Receiver<Update>,
) -> Result<()> {
    loop {
        while let Ok(update) = data_rx.try_recv() {
            match update {
                Update::Vms(vms) => {
                    app.vms = vms;
                    app.error = None;
                    app.loading = false;
                    app.last_update = Some(Instant::now());
                    if app.state.selected().is_none() && !app.vms.is_empty() {
                        app.state.select(Some(0));
                    }
                    if let Some(sel) = app.state.selected() {
                        if sel >= app.vms.len() {
                            let last = app.vms.len().checked_sub(1);
                            app.state.select(last);
                        }
                    }
                }
                Update::Error(e) => {
                    app.error = Some(e);
                    app.loading = false;
                }
                Update::Ips { vmid, result } => {
                    // Only apply if the detail view is still on this VM.
                    if let Some(d) = &mut app.detail {
                        if d.vmid == vmid {
                            d.ips = match result {
                                Ok(ips) => IpState::Loaded(ips),
                                Err(e) => IpState::Unavailable(short_reason(&e)),
                            };
                        }
                    }
                }
                Update::ActionDone { kind, result } => {
                    // Close the modal; surface any failure in the footer.
                    app.confirm = None;
                    if let Err(e) = result {
                        app.error = Some(format!("{} failed: {}", kind.verb(), short_reason(&e)));
                    } else {
                        app.error = None;
                    }
                }
                Update::NextId(id) => {
                    if let Some(f) = &mut app.form {
                        f.next_id = Some(id);
                    }
                }
                Update::CreateDone(result) => match result {
                    Ok(outcome) => {
                        if let Some(f) = &mut app.form {
                            f.state = FormState::Done(outcome);
                        }
                        app.error = None;
                    }
                    Err(e) => {
                        // Drop back to editing with the failure shown.
                        if let Some(f) = &mut app.form {
                            f.state = FormState::Editing(Some(short_reason(&e)));
                        }
                    }
                },
            }
        }

        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Modal precedence: create form > confirmation > detail > list.
                if app.form.is_some() {
                    handle_form_key(app, key.code);
                } else if let Some(confirm) = &app.confirm {
                    // Ignore input while the command is in flight.
                    if !confirm.running {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Enter => app.confirm_action(),
                            KeyCode::Char('n') | KeyCode::Esc => app.confirm = None,
                            _ => {}
                        }
                    }
                } else if app.detail.is_some() {
                    match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Esc | KeyCode::Enter => app.detail = None,
                        KeyCode::Char('r') => app.request_refresh(),
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Enter => app.open_detail(),
                        KeyCode::Char('j') | KeyCode::Down => app.next(),
                        KeyCode::Char('k') | KeyCode::Up => app.prev(),
                        KeyCode::Char('s') => app.begin_action(ActionKind::Start),
                        KeyCode::Char('x') => app.begin_action(ActionKind::Stop),
                        KeyCode::Char('d') => app.begin_action(ActionKind::Destroy),
                        KeyCode::Char('c') => app.open_form(),
                        KeyCode::Char('r') => app.request_refresh(),
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Route a key to the create wizard based on its current phase.
fn handle_form_key(app: &mut App, code: KeyCode) {
    enum Phase {
        Editing,
        Submitting,
        Done,
    }
    let phase = match &app.form {
        Some(f) => match f.state {
            FormState::Editing(_) => Phase::Editing,
            FormState::Submitting => Phase::Submitting,
            FormState::Done(_) => Phase::Done,
        },
        None => return,
    };
    match phase {
        // In flight — ignore input until CreateDone arrives.
        Phase::Submitting => {}
        // Any key dismisses the outcome summary.
        Phase::Done => app.form = None,
        Phase::Editing => match code {
            KeyCode::Esc => app.form = None,
            KeyCode::Enter => app.submit_form(),
            KeyCode::Up | KeyCode::BackTab => app.form_move(-1),
            KeyCode::Down | KeyCode::Tab => app.form_move(1),
            KeyCode::Backspace => app.form_backspace(),
            KeyCode::Left | KeyCode::Right => {
                let is_bool = app
                    .form
                    .as_ref()
                    .map(|f| f.fields[f.focus].kind == FieldKind::Bool)
                    .unwrap_or(false);
                if is_bool {
                    app.form_char(' ');
                }
            }
            KeyCode::Char(c) => app.form_char(c),
            _ => {}
        },
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());

    render_header(f, app, areas[0]);
    render_table(f, app, areas[1]);
    render_footer(f, app, areas[2]);

    // Overlay precedence: create form > confirmation > detail.
    if app.form.is_some() {
        render_form(f, app, areas[1]);
    } else if app.confirm.is_some() {
        render_confirm(f, app, areas[1]);
    } else if app.detail.is_some() {
        render_detail(f, app, areas[1]);
    }
}

fn render_header(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let count = app.vms.iter().filter(|v| !v.is_template).count();
    let status = if app.loading {
        "↻ refreshing".to_string()
    } else if let Some(t) = app.last_update {
        format!("updated {}s ago", t.elapsed().as_secs())
    } else {
        "—".to_string()
    };
    let header = Line::from(vec![
        Span::styled(
            " ☾ mox ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  node {}   ", app.cfg.host)),
        Span::styled(format!("{count} VMs"), Style::new().fg(Color::Gray)),
        Span::raw("  ·  "),
        Span::styled(status, Style::new().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(header), area);
}

fn render_table(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let header_row = Row::new(["ID", "NAME", "STATUS", "CPU", "MEM", "UPTIME"])
        .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = app.vms.iter().map(vm_row).collect();
    let widths = [
        Constraint::Length(5),
        Constraint::Min(14),
        Constraint::Length(12),
        Constraint::Length(15),
        Constraint::Length(15),
        Constraint::Length(8),
    ];
    let table = Table::new(rows, widths)
        .header(header_row)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" virtual machines "),
        )
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(table, area, &mut app.state);
}

fn render_footer(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let footer = if let Some(err) = &app.error {
        Line::from(Span::styled(
            format!(" error: {err} "),
            Style::new().fg(Color::White).bg(Color::Red),
        ))
    } else {
        let key = |k| Span::styled(k, Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        if let Some(form) = &app.form {
            match form.state {
                FormState::Editing(_) => Line::from(vec![
                    Span::raw(" "),
                    key("↑/↓"),
                    Span::raw(" field  "),
                    key("Enter"),
                    Span::raw(" create  "),
                    key("Esc"),
                    Span::raw(" cancel"),
                ]),
                FormState::Submitting => Line::from(Span::styled(
                    " creating…",
                    Style::new().fg(Color::Yellow),
                )),
                FormState::Done(_) => Line::from(vec![
                    Span::raw(" "),
                    key("any key"),
                    Span::raw(" close"),
                ]),
            }
        } else if app.confirm.is_some() {
            Line::from(vec![
                Span::raw(" "),
                key("y"),
                Span::raw(" confirm   "),
                key("n"),
                Span::raw(" cancel"),
            ])
        } else if app.detail.is_some() {
            Line::from(vec![
                Span::raw(" "),
                key("Esc"),
                Span::raw(" back   "),
                key("r"),
                Span::raw(" refresh   "),
                key("q"),
                Span::raw(" quit"),
            ])
        } else {
            Line::from(vec![
                Span::raw(" "),
                key("↑/↓"),
                Span::raw(" move  "),
                key("Enter"),
                Span::raw(" details  "),
                key("s"),
                Span::raw("/"),
                key("x"),
                Span::raw("/"),
                key("d"),
                Span::raw(" start/stop/destroy  "),
                key("c"),
                Span::raw(" new  "),
                key("r"),
                Span::raw(" refresh  "),
                key("q"),
                Span::raw(" quit"),
            ])
        }
    };
    f.render_widget(Paragraph::new(footer), area);
}

/// The colored status glyph and color for a VM status string.
fn status_dot(status: &str) -> (&'static str, Color) {
    match status {
        "running" => ("●", Color::Green),
        "stopped" => ("○", Color::DarkGray),
        "template" => ("◆", Color::Blue),
        _ => ("○", Color::Yellow),
    }
}

fn vm_row(vm: &Vm) -> Row<'static> {
    let (dot, color) = status_dot(&vm.status);
    let status_cell = Cell::from(Line::from(vec![
        Span::styled(dot, Style::new().fg(color)),
        Span::raw(format!(" {}", vm.status)),
    ]));
    Row::new(vec![
        Cell::from(vm.vmid.to_string()),
        Cell::from(vm.name.clone()),
        status_cell,
        gauge_cell(vm.cpu_pct(), vm.is_running()),
        gauge_cell(vm.mem_pct(), vm.is_running()),
        Cell::from(vm.uptime_human()),
    ])
}

/// A compact `████░░░░  37%` gauge, colored by load. Dashed when inactive.
fn gauge_cell(pct: f64, active: bool) -> Cell<'static> {
    if !active {
        return Cell::from(Span::styled(
            "       —",
            Style::new().fg(Color::DarkGray),
        ));
    }
    let width = 8usize;
    let p = pct.clamp(0.0, 100.0);
    let filled = (((p / 100.0) * width as f64).round() as usize).min(width);
    let bar: String = "█".repeat(filled) + &"░".repeat(width - filled);
    let color = if p < 60.0 {
        Color::Green
    } else if p < 85.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    Cell::from(Line::from(vec![
        Span::styled(bar, Style::new().fg(color)),
        Span::raw(format!(" {p:>3.0}%")),
    ]))
}

/// A `label      value` line for the detail overlay.
fn kv(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::new().fg(Color::Gray)),
        Span::raw(value),
    ])
}

/// A dimmed placeholder line for the detail overlay (e.g. `LAN IP     —`).
fn kv_dim(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::new().fg(Color::Gray)),
        Span::styled(value, Style::new().fg(Color::DarkGray)),
    ])
}

/// Render the detail overlay as a centered popup over the table area.
fn render_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(detail) = app.detail.as_ref() else {
        return;
    };
    let vm = app.detail_vm();

    let title = match vm {
        Some(v) => format!(" {} ({}) ", v.name, v.vmid),
        None => format!(" vm {} ", detail.vmid),
    };

    let mut lines: Vec<Line> = Vec::new();
    if let Some(v) = vm {
        let (dot, color) = status_dot(&v.status);
        lines.push(Line::from(vec![
            Span::styled(dot, Style::new().fg(color)),
            Span::raw(format!(" {}", v.status)),
        ]));
        lines.push(Line::raw(""));
        lines.push(kv("CPU", format!("{} vCPU  ·  {:.0}% used", v.maxcpu as u64, v.cpu_pct())));
        lines.push(kv(
            "Memory",
            format!("{} / {}  ({:.0}%)", v.mem_used_human(), v.mem_total_human(), v.mem_pct()),
        ));
        lines.push(kv("Disk", v.disk_human()));
        lines.push(kv("Uptime", v.uptime_human()));
    } else {
        lines.push(kv_dim("", "(vm no longer present)".to_string()));
    }

    lines.push(Line::raw(""));
    match &detail.ips {
        IpState::Loading => {
            lines.push(kv_dim("LAN IP", "resolving…".to_string()));
            lines.push(kv_dim("Tailscale", "resolving…".to_string()));
        }
        IpState::NotRunning => {
            lines.push(kv_dim("LAN IP", "— (VM not running)".to_string()));
            lines.push(kv_dim("Tailscale", "—".to_string()));
        }
        IpState::Unavailable(reason) => {
            lines.push(kv_dim("LAN IP", format!("— ({reason})")));
            lines.push(kv_dim("Tailscale", "—".to_string()));
        }
        IpState::Loaded(ips) => {
            match &ips.lan {
                Some(ip) => lines.push(kv("LAN IP", ip.clone())),
                None => lines.push(kv_dim("LAN IP", "—".to_string())),
            }
            match &ips.tailscale {
                Some(ip) => lines.push(kv("Tailscale", ip.clone())),
                None => lines.push(kv_dim("Tailscale", "—".to_string())),
            }
        }
    }

    let height = lines.len() as u16 + 2; // + top/bottom borders
    let popup = centered_rect(area, 48, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Render the confirmation modal for a pending lifecycle action.
fn render_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some(c) = app.confirm.as_ref() else {
        return;
    };
    // Destroy is destructive — color it red; start/stop are routine (cyan).
    let accent = if c.kind == ActionKind::Destroy {
        Color::Red
    } else {
        Color::Cyan
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    if c.running {
        lines.push(Line::from(Span::styled(
            format!("  {} {} ({})…", c.kind.gerund(), c.name, c.vmid),
            Style::new().fg(accent),
        )));
    } else {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(format!("{} ", c.kind.verb())),
            Span::styled(
                format!("{} ({})", c.name, c.vmid),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::raw("?"),
        ]));
        if c.kind == ActionKind::Destroy {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  This deletes the VM and purges it. Irreversible.",
                Style::new().fg(Color::Red),
            )));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("y", Style::new().fg(accent).add_modifier(Modifier::BOLD)),
            Span::styled(" confirm    ", Style::new().fg(Color::Gray)),
            Span::styled("n", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::new().fg(Color::Gray)),
        ]));
    }

    let height = lines.len() as u16 + 2;
    let popup = centered_rect(area, 52, height);
    let title = if c.kind == ActionKind::Destroy {
        " confirm destroy "
    } else {
        " confirm "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(accent))
        .title(Span::styled(
            title,
            Style::new().fg(accent).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Render the create wizard: the field list, an in-flight spinner, or the
/// final outcome summary.
fn render_form(f: &mut Frame, app: &App, area: Rect) {
    let Some(form) = app.form.as_ref() else {
        return;
    };
    let mut lines: Vec<Line> = Vec::new();

    match &form.state {
        FormState::Submitting => {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  creating VM…",
                Style::new().fg(Color::Yellow),
            )));
            lines.push(Line::raw(""));
        }
        FormState::Done(o) => {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                format!("  ✓ created {} ({})", o.name, o.vmid),
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            lines.push(kv("Status", if o.started { "started" } else { "created (not started)" }.to_string()));
            if o.started {
                match &o.ips.lan {
                    Some(ip) => lines.push(kv("LAN IP", ip.clone())),
                    None => lines.push(kv_dim("LAN IP", "not yet available".to_string())),
                }
                if let Some(ts) = &o.ips.tailscale {
                    lines.push(kv("Tailscale", ts.clone()));
                }
                if o.ips.lan.is_some() || o.ssh_alias.is_some() {
                    lines.push(Line::raw(""));
                }
                if let Some(ip) = &o.ips.lan {
                    lines.push(Line::from(Span::styled(
                        format!("  ssh {}@{}", o.user, ip),
                        Style::new().fg(Color::Cyan),
                    )));
                }
                // For a Tailscale VM, the ~/.ssh/config alias makes `ssh <name>` work.
                if o.ssh_alias.is_some() {
                    lines.push(Line::from(Span::styled(
                        format!("  ssh {}", o.name),
                        Style::new().fg(Color::Cyan),
                    )));
                }
            }
            lines.push(Line::raw(""));
        }
        FormState::Editing(err) => {
            for (i, field) in form.fields.iter().enumerate() {
                let focused = i == form.focus;
                let marker = if focused { "▶ " } else { "  " };
                let label_style = if focused {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::Gray)
                };
                // The VM ID field's placeholder shows the resolved next free id.
                let placeholder: String = if field.label == "VM ID" {
                    match form.next_id {
                        Some(id) => format!("(next available: {id})"),
                        None => field.placeholder.to_string(),
                    }
                } else if field.placeholder.is_empty() {
                    "—".to_string()
                } else {
                    field.placeholder.to_string()
                };
                let value_span = if field.value.is_empty() && !focused {
                    Span::styled(placeholder, Style::new().fg(Color::DarkGray))
                } else {
                    let mut v = field.value.clone();
                    if focused && field.kind != FieldKind::Bool {
                        v.push('▏'); // text cursor
                    }
                    Span::raw(v)
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::new().fg(Color::Cyan)),
                    Span::styled(format!("{:<19}", field.label), label_style),
                    value_span,
                ]));
            }
            if let Some(msg) = err {
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled(
                    format!("  ⚠ {msg}"),
                    Style::new().fg(Color::Red),
                )));
            }
        }
    }

    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = centered_rect(area, 56, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            " create vm ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// A rect of the given size, centered within `area`.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let [h] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(h);
    v
}

/// Trim an error into a short, single-line reason for the detail overlay.
fn short_reason(e: &str) -> String {
    let first = e.lines().next().unwrap_or(e).trim();
    let s = first
        .strip_prefix("remote command failed: ")
        .unwrap_or(first)
        .trim();
    if s.chars().count() > 30 {
        s.chars().take(29).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_config() -> Config {
        Config {
            host: "10.0.0.1".into(),
            user: "root".into(),
            port: 22,
            identity_file: None,
            template_id: 9000,
            bridge: "vmbr0".into(),
            template_name: "ubuntu-2404-cloudinit".into(),
            image_url: "https://example.com/img.img".into(),
            storage: "local-lvm".into(),
            cpu_type: "host".into(),
            template_packages: String::new(),
            default_cores: 2,
            default_memory: 4096,
            default_disk: 20,
            default_full_clone: false,
            ci_user: "ubuntu".into(),
            tailscale_default: false,
            tailscale_authkey: None,
            tailnet_domain: None,
            ip_timeout: 120,
            vm_subnet: None,
        }
    }

    fn sample_vm(id: u32, name: &str, status: &str, cpu: f64, mem: f64, up: u64, tmpl: bool) -> Vm {
        Vm {
            vmid: id,
            name: name.to_string(),
            status: status.to_string(),
            is_template: tmpl,
            mem: mem as u64,
            maxmem: 100,
            maxdisk: 20 * 1024 * 1024 * 1024,
            cpu: cpu / 100.0,
            maxcpu: 2.0,
            uptime: up,
        }
    }

    #[test]
    fn renders_dashboard() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App {
            cfg: test_config(),
            vms: vec![
                sample_vm(100, "web-01", "running", 5.0, 37.0, 5940, false),
                sample_vm(101, "db-01", "running", 1.0, 36.0, 1260, false),
                sample_vm(9000, "ubuntu-2404-cloudinit", "template", 0.0, 0.0, 0, true),
            ],
            state: TableState::default().with_selected(Some(0)),
            error: None,
            last_update: Some(Instant::now()),
            loading: false,
            detail: None,
            confirm: None,
            form: None,
            req_tx: tx,
        };

        let mut terminal = Terminal::new(TestBackend::new(78, 10)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        println!("\n{out}");

        assert!(out.contains("mox"));
        assert!(out.contains("web-01"));
        assert!(out.contains("template"));
        assert!(out.contains('%'));
    }

    fn buffer_text(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_detail_overlay() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App {
            cfg: test_config(),
            vms: vec![sample_vm(100, "web-01", "running", 5.0, 37.0, 5940, false)],
            state: TableState::default().with_selected(Some(0)),
            error: None,
            last_update: Some(Instant::now()),
            loading: false,
            detail: Some(DetailView {
                vmid: 100,
                ips: IpState::Loaded(GuestIps {
                    lan: Some("192.168.3.42".into()),
                    tailscale: Some("100.101.102.103".into()),
                }),
            }),
            confirm: None,
            form: None,
            req_tx: tx,
        };

        let out = buffer_text(&mut app, 78, 16);
        println!("\n{out}");
        assert!(out.contains("web-01 (100)"));
        assert!(out.contains("CPU"));
        assert!(out.contains("Disk"));
        assert!(out.contains("192.168.3.42"), "LAN IP should show");
        assert!(out.contains("100.101.102.103"), "Tailscale IP should show");
        assert!(out.contains("Esc"), "footer should offer back");
    }

    #[test]
    fn renders_destroy_confirm() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App {
            cfg: test_config(),
            vms: vec![sample_vm(101, "db-01", "running", 1.0, 36.0, 1260, false)],
            state: TableState::default().with_selected(Some(0)),
            error: None,
            last_update: Some(Instant::now()),
            loading: false,
            detail: None,
            confirm: Some(Confirm {
                kind: ActionKind::Destroy,
                vmid: 101,
                name: "db-01".into(),
                running: false,
            }),
            form: None,
            req_tx: tx,
        };

        let out = buffer_text(&mut app, 78, 16);
        println!("\n{out}");
        assert!(out.contains("destroy db-01 (101)"), "prompt should name the action + VM");
        assert!(out.contains("Irreversible"), "destroy should warn");
        assert!(out.contains("confirm"), "footer/body should offer confirm");
    }

    fn app_with_form() -> App {
        let (tx, _rx) = mpsc::channel();
        let mut app = App {
            cfg: test_config(),
            vms: vec![],
            state: TableState::default(),
            error: None,
            last_update: Some(Instant::now()),
            loading: false,
            detail: None,
            confirm: None,
            form: None,
            req_tx: tx,
        };
        app.open_form();
        app
    }

    #[test]
    fn renders_create_form() {
        let mut app = app_with_form();
        let out = buffer_text(&mut app, 78, 20);
        println!("\n{out}");
        assert!(out.contains("create vm"));
        assert!(out.contains("Name"));
        assert!(out.contains("Cores"));
        assert!(out.contains("Tailscale"));
        assert!(out.contains("create"), "footer should offer create");
    }

    #[test]
    fn form_shows_next_id_hint() {
        let mut app = app_with_form();
        app.form.as_mut().unwrap().next_id = Some(142);
        let out = buffer_text(&mut app, 78, 20);
        assert!(
            out.contains("next available: 142"),
            "VM ID field should hint the resolved next id"
        );
    }

    #[test]
    fn form_editing_rules() {
        let mut app = app_with_form();
        // Focus "Cores" (index 2): digits accepted, letters ignored.
        app.form.as_mut().unwrap().focus = 2;
        app.form_char('9');
        app.form_char('a');
        assert_eq!(App::field_value(app.form.as_ref().unwrap(), "Cores"), "29");
        // Focus the Tailscale toggle (last field) and flip it on.
        let last = app.form.as_ref().unwrap().fields.len() - 1;
        app.form.as_mut().unwrap().focus = last;
        assert!(!App::field_on(app.form.as_ref().unwrap(), "Join Tailscale"));
        app.form_char(' ');
        assert!(App::field_on(app.form.as_ref().unwrap(), "Join Tailscale"));
        // Navigation wraps.
        app.form.as_mut().unwrap().focus = 0;
        app.form_move(-1);
        assert_eq!(app.form.as_ref().unwrap().focus, last);
    }

    #[test]
    fn submit_requires_name() {
        let mut app = app_with_form();
        // Name is empty by default → submit should refuse with a message.
        app.submit_form();
        match &app.form.as_ref().unwrap().state {
            FormState::Editing(Some(msg)) => assert!(msg.contains("Name")),
            _ => panic!("expected a validation error about Name"),
        }
    }

    #[test]
    fn begin_action_blocks_start_on_template() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App {
            cfg: test_config(),
            vms: vec![sample_vm(9000, "tmpl", "template", 0.0, 0.0, 0, true)],
            state: TableState::default().with_selected(Some(0)),
            error: None,
            last_update: Some(Instant::now()),
            loading: false,
            detail: None,
            confirm: None,
            form: None,
            req_tx: tx,
        };
        app.begin_action(ActionKind::Start);
        assert!(app.confirm.is_none(), "start must be refused on a template");
        app.begin_action(ActionKind::Destroy);
        assert!(app.confirm.is_some(), "destroy is allowed on a template");
    }
}
