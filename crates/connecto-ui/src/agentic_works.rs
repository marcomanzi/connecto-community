//! The Agentic Works sheet (Terminal > Agentic Works…): links a project to
//! PostgreSQL queues, each under a name such as dev, staging or production
//! (a connection string, or Google Cloud SQL), and publishes the work types
//! agents take from them.

use connecto_core::{
    ToolKind,
    agentic_works::{
        AgentStage, CloudSql, DEFAULT_DATABASE, Database, GoogleCredential, InstanceName, IpType,
        JobAgent, LinkedDatabase, MAX_PARALLEL, SetupStep, WorksAction, WorksControl, WorksView,
        cloudsql_setup_steps, database_name,
    },
};
use egui::{Context, Id, Rect, RichText, TextEdit, Ui};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::forms::{self, Out, card, heading, muted, problem, warning};
use crate::theme;

/// The kind of database the form is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbKind {
    ConnectionString,
    CloudSql,
}

pub(crate) struct Sheet {
    project: String,
    name: String,
    /// The name of the database the form links or changes.
    db_name: String,
    db_kind: DbKind,
    credential: String,
    /// The Cloud SQL form. Its `credential` key name is the chosen key.
    sql: CloudSql,
    /// A password typed and not yet kept in the vault.
    password: String,
    /// The key file just imported, until it is moved to the Trash or kept.
    imported_file: Option<PathBuf>,
    /// The instance and user the typed password was typed for: it is
    /// dropped when either changes, never offered to another target.
    password_for: (String, String),
    kind: String,
    instructions: String,
    schema: String,
    setup: String,
    prepare: String,
    cli: ToolKind,
    /// The CLIs an authoring session can run on here: Claude and Codex, as
    /// far as the project's Tools setting allows them.
    authors: Vec<ToolKind>,
    /// The databases the type in the form is published for: None for every
    /// one. Loaded from the published type when the work type changes.
    publish_to: Option<Vec<String>>,
    /// The work type `publish_to` was loaded for.
    publish_kind: String,
    error: String,
    view: WorksView,
    refreshed: Instant,
}

/// A one-line field drawn like the other settings fields.
fn field(ui: &mut Ui, id: &str, text: &mut String, hint: &str) {
    ui.add(
        TextEdit::singleline(text)
            .id(Id::unique(("works-field", id)))
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    );
}

/// A multi-line field in the code font.
fn code_field(ui: &mut Ui, id: &str, text: &mut String, hint: &str, rows: usize) {
    ui.add(
        TextEdit::multiline(text)
            .id(Id::unique(("works-field", id)))
            .hint_text(hint)
            .code_editor()
            .desired_rows(rows)
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    );
}

/// The CLIs an authoring session can run on.
const AUTHORS: [ToolKind; 2] = [ToolKind::Claude, ToolKind::Codex];

impl Sheet {
    /// Only the authoring CLIs the project's Tools setting allows (`allowed`)
    /// are offered.
    #[must_use]
    pub fn allowing(mut self, allowed: &[ToolKind]) -> Self {
        self.authors = AUTHORS
            .into_iter()
            .filter(|cli| allowed.contains(cli))
            .collect();
        if let Some(first) = self.authors.first()
            && !self.authors.contains(&self.cli)
        {
            self.cli = *first;
        }
        self
    }

    pub fn new(project: &str, name: &str, control: &dyn WorksControl) -> Self {
        let view = control.view(project);
        let mut sheet = Self {
            project: project.into(),
            name: name.into(),
            db_name: if view.linked() {
                String::new()
            } else {
                DEFAULT_DATABASE.into()
            },
            db_kind: DbKind::ConnectionString,
            credential: String::new(),
            sql: CloudSql::default(),
            password: String::new(),
            imported_file: None,
            password_for: (String::new(), String::new()),
            kind: String::new(),
            instructions: String::new(),
            schema: String::new(),
            setup: String::new(),
            prepare: String::new(),
            cli: ToolKind::Claude,
            authors: AUTHORS.to_vec(),
            publish_to: None,
            publish_kind: String::new(),
            error: String::new(),
            view,
            refreshed: Instant::now(),
        };
        // A single database is in the form, ready to change.
        if let [only] = sheet.view.databases.as_slice() {
            let only = only.clone();
            sheet.edit(&only);
        }
        sheet.default_key();
        sheet
    }

    /// Put a linked database in the form, to change it.
    fn edit(&mut self, linked: &LinkedDatabase) {
        self.db_name.clone_from(&linked.name);
        self.password.clear();
        match &linked.database {
            Database::CloudSql(sql) => {
                self.db_kind = DbKind::CloudSql;
                self.sql = sql.clone();
                self.credential.clear();
            }
            Database::ConnectionString(name) => {
                self.db_kind = DbKind::ConnectionString;
                self.credential.clone_from(name);
                self.sql = CloudSql::default();
            }
        }
    }

    /// With no key chosen, the project's only imported key.
    fn default_key(&mut self) {
        if self.sql.credential == GoogleCredential::Key(String::new())
            && let [only] = self.view.google_keys.as_slice()
        {
            self.sql.credential = GoogleCredential::Key(only.clone());
        }
    }

    pub fn draw(&mut self, ctx: &Context, area: Rect, control: &dyn WorksControl) -> bool {
        if self.refreshed.elapsed() >= Duration::from_secs(1) {
            self.view = control.view(&self.project);
            self.refreshed = Instant::now();
            self.default_key();
        }
        ctx.request_repaint_after(Duration::from_secs(1));
        let mut leave = false;
        let title = format!("Agentic Works — {}", self.name);
        let closed = crate::sheet::show(ctx, area, "agentic-works", &title, |ui, rect| {
            let mut out = Out::<()>::default();
            forms::column(ui, rect, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !self.error.is_empty() {
                            out.say(ui, problem(&self.error));
                        }
                        if !self.view.notice.is_empty() {
                            out.say(ui, muted(&self.view.notice));
                        }
                        self.database_section(ui, control, &mut out);
                        forms::separator(ui);
                        leave = self.types_section(ui, control, &mut out);
                        forms::separator(ui);
                        self.queue_section(ui, &mut out);
                    });
            });
        });
        closed || leave || ctx.input(|i| i.key_pressed(egui::Key::Escape))
    }

    /// How many jobs run at once, with − and +, and the jobs running now.
    fn parallel_row(&mut self, ui: &mut Ui, control: &dyn WorksControl, out: &mut Out<()>) {
        let jobs = self.view.parallel.max(1);
        let mut set = None;
        ui.horizontal_wrapped(|ui| {
            out.say(ui, muted("Jobs at once:"));
            if out.button(ui, "works-parallel-less", "−", jobs > 1) {
                set = Some(jobs - 1);
            }
            out.say(ui, RichText::new(jobs.to_string()).size(13.0).strong());
            if out.button(ui, "works-parallel-more", "+", jobs < MAX_PARALLEL) {
                set = Some(jobs + 1);
            }
            out.say(
                ui,
                muted(match self.view.running.len() {
                    0 => "none running".to_owned(),
                    n => format!("{n} running"),
                }),
            );
        });
        if let Some(jobs) = set {
            // Shown at once; the next refresh reads it back.
            self.view.parallel = jobs;
            control.submit(self.project.clone(), WorksAction::Parallel { jobs });
        }
        if jobs > 1 {
            out.say(
                ui,
                muted("Jobs share the project folder: each gets its own folder (CONNECTO_JOB_DIR), and its prepare command must write only there."),
            );
        }
        for r in &self.view.running {
            out.say(
                ui,
                muted(if r.agent.is_empty() {
                    format!(
                        "Running job {} of {} ({}) since {}",
                        r.id, r.database, r.kind, r.since
                    )
                } else {
                    let agent = match r.agent_stage {
                        AgentStage::Running => format!("on {}", r.agent),
                        AgentStage::Dispatched => format!("dispatched to {}, starting", r.agent),
                        AgentStage::Selected => format!("selected {}, not started yet", r.agent),
                    };
                    format!(
                        "Running job {} of {} ({}) since {}; {agent}",
                        r.id, r.database, r.kind, r.since
                    )
                }),
            );
        }
    }

    /// The agent the jobs that name none run on: Automatic, or one CLI the
    /// project's Tools allow with one of its models or its default model.
    fn job_agent_row(&mut self, ui: &mut Ui, control: &dyn WorksControl, out: &mut Out<()>) {
        let current = self.view.job_agent.clone();
        let mut set = None;
        ui.horizontal_wrapped(|ui| {
            out.say(ui, muted("Agent for jobs:"));
            if out.choice(
                ui,
                "works-agent:auto",
                current == JobAgent::Automatic,
                "Automatic",
            ) && current != JobAgent::Automatic
            {
                set = Some(JobAgent::Automatic);
            }
            for c in &self.view.job_agent_choices {
                let on = matches!(&current, JobAgent::Cli { tool, .. } if *tool == c.tool);
                if out.choice(
                    ui,
                    &format!("works-agent:{}", c.tool.name()),
                    on,
                    c.tool.label(),
                ) && !on
                {
                    set = Some(JobAgent::Cli {
                        tool: c.tool,
                        model: None,
                    });
                }
            }
        });
        if let JobAgent::Cli { tool, model } = &current {
            let offered = self
                .view
                .job_agent_choices
                .iter()
                .find(|c| c.tool == *tool)
                .map(|c| c.models.clone());
            ui.horizontal_wrapped(|ui| {
                out.say(ui, muted("Model:"));
                if out.choice(ui, "works-agent-model:", model.is_none(), "Its default")
                    && model.is_some()
                {
                    set = Some(JobAgent::Cli {
                        tool: *tool,
                        model: None,
                    });
                }
                let mut models = offered.clone().unwrap_or_default();
                // A model set with `connecto works agent --model` and not
                // in the list is still shown, chosen.
                if let Some(m) = model
                    && !models.contains(m)
                {
                    models.push(m.clone());
                }
                for m in models {
                    let on = model.as_deref() == Some(m.as_str());
                    if out.choice(ui, &format!("works-agent-model:{m}"), on, &m) && !on {
                        set = Some(JobAgent::Cli {
                            tool: *tool,
                            model: Some(m.clone()),
                        });
                    }
                }
            });
            if offered.is_none() {
                out.say(
                    ui,
                    warning(format!(
                        "{} is not one of this project's Tools: its jobs that name no agent fail until it is allowed again, or another agent is chosen.",
                        tool.label()
                    )),
                );
            }
        }
        out.say(
            ui,
            muted(match &current {
                JobAgent::Automatic => "Jobs whose type names no agent run on Antigravity, else the first usable of Codex (GPT Terra), Claude (Haiku), Pi and OpenCode. A type that names its agent keeps it.".to_owned(),
                JobAgent::Cli { .. } => format!(
                    "Jobs whose type names no agent run on {}; when it cannot run one, the job fails saying why. A type that names its agent keeps it, and its own model.",
                    current.label()
                ),
            }),
        );
        if let Some(agent) = set {
            // Shown at once; the next refresh reads it back.
            self.view.job_agent = agent.clone();
            control.submit(self.project.clone(), WorksAction::JobAgent { agent });
        }
    }

    fn database_section(&mut self, ui: &mut Ui, control: &dyn WorksControl, out: &mut Out<()>) {
        out.say(ui, heading("DATABASES"));
        if self.view.linked() {
            out.say(
                ui,
                muted(if self.view.parallel > 1 {
                    "Connecto takes jobs from each of these, several at once: the database running the fewest gets the next place. Work types are published once, for all of them."
                } else {
                    "Connecto takes jobs from each of these, one job at a time, every database in turn. Work types are published once, for all of them."
                }),
            );
            self.parallel_row(ui, control, out);
        } else {
            out.say(ui, warning("Not linked."));
        }
        // Before the first link too: jobs already waiting in a queue being
        // linked are claimed at once, on the agent chosen here.
        self.job_agent_row(ui, control, out);
        for linked in self.view.databases.clone() {
            ui.horizontal_wrapped(|ui| {
                out.pill(ui, &linked.name, theme::CYAN);
                out.say(ui, muted(linked.database.label()));
                let status = &linked.status;
                match status.ok {
                    None => out.say(ui, muted("Not looked at yet")),
                    Some(true) => out.say(
                        ui,
                        RichText::new(format!("✓ {} ({})", status.message, status.at))
                            .size(11.5)
                            .color(theme::GREEN),
                    ),
                    Some(false) => {
                        out.say(ui, problem(format!("✗ {} ({})", status.message, status.at)));
                    }
                }
                if out.button(ui, &format!("works-edit:{}", linked.name), "Edit", true) {
                    self.edit(&linked);
                    self.default_key();
                }
                if out.button(ui, &format!("works-remove:{}", linked.name), "Remove", true) {
                    control.submit(
                        self.project.clone(),
                        WorksAction::Unlink {
                            name: linked.name.clone(),
                            force: false,
                        },
                    );
                }
            });
        }
        ui.add_space(4.0);
        out.say(
            ui,
            muted("Add a database, or change one: its name, such as dev, staging or production"),
        );
        field(
            ui,
            "db-name",
            &mut self.db_name,
            "default, dev, staging, production",
        );
        let name = database_name(&self.db_name);
        if let Err(why) = &name
            && !self.db_name.trim().is_empty()
        {
            out.say(ui, problem(why));
        }
        let exists = name.as_ref().is_ok_and(|n| self.view.database(n).is_some());
        ui.horizontal(|ui| {
            for (kind, label) in [
                (DbKind::ConnectionString, "PostgreSQL connection string"),
                (DbKind::CloudSql, "Google Cloud SQL"),
            ] {
                if out.choice(
                    ui,
                    &format!("works-db:{label}"),
                    self.db_kind == kind,
                    label,
                ) {
                    self.db_kind = kind;
                }
            }
        });
        let ready = match self.db_kind {
            DbKind::ConnectionString => {
                out.say(
                    ui,
                    muted("In Settings → Secrets, save a PostgreSQL connection string under this project and choose Agentic Works database as its use. Then enter the secret's name here."),
                );
                field(
                    ui,
                    "credential",
                    &mut self.credential,
                    "Project secret name",
                );
                if self.credential.trim().is_empty() {
                    Err("Enter the secret's name first.".to_owned())
                } else {
                    Ok(())
                }
            }
            DbKind::CloudSql => self.cloudsql_form(ui, control, out),
        }
        .and_then(|()| name.clone().map(drop).map_err(|_| "Enter the database's name first.".to_owned()));
        ui.horizontal(|ui| {
            let why = ready.as_ref().err().cloned().unwrap_or_default();
            if self.db_kind == DbKind::CloudSql
                && out.button_hint(ui, "works-test", "Test connection", ready.is_ok(), &why)
            {
                control.submit(
                    self.project.clone(),
                    WorksAction::Test {
                        database: self.database(),
                        password: self.typed_password(),
                    },
                );
            }
            for (key, label, create) in [
                ("works-create", "Create queue and link", true),
                ("works-validate", "Validate existing queue", false),
            ] {
                let enabled = ready.is_ok() && (!create || !exists);
                let why = if ready.is_err() {
                    why.clone()
                } else {
                    format!(
                        "This project already has a database named {}.",
                        self.db_name.trim()
                    )
                };
                if out.button_hint(ui, key, label, enabled, &why) {
                    control.submit(
                        self.project.clone(),
                        WorksAction::Link {
                            name: self.db_name.trim().into(),
                            database: self.database(),
                            create,
                            password: self.typed_password(),
                        },
                    );
                    self.password.clear();
                }
            }
        });
        if self.db_kind == DbKind::CloudSql {
            self.setup_panel(ui, out);
        }
    }

    /// The database the form says. The saved password goes with it only
    /// while the instance and user are the linked ones.
    fn database(&self) -> Database {
        match self.db_kind {
            DbKind::ConnectionString => Database::ConnectionString(self.credential.trim().into()),
            DbKind::CloudSql => {
                let mut sql = self.sql.clone();
                sql.connection_name = sql.connection_name.trim().into();
                sql.database = sql.database.trim().into();
                sql.user = sql.user.trim().into();
                sql.password_secret = self.saved_password().unwrap_or_default();
                if sql.iam_auth {
                    sql.password_secret.clear();
                }
                Database::CloudSql(sql)
            }
        }
    }

    /// A linked database's saved password, when the form is for that
    /// instance and user.
    fn saved_password(&self) -> Option<String> {
        self.view
            .databases
            .iter()
            .find_map(|linked| match &linked.database {
                Database::CloudSql(linked)
                    if !linked.password_secret.is_empty()
                        && linked.connection_name.trim() == self.sql.connection_name.trim()
                        && linked.user.trim() == self.sql.user.trim() =>
                {
                    Some(linked.password_secret.clone())
                }
                _ => None,
            })
    }

    fn typed_password(&self) -> Option<String> {
        (self.db_kind == DbKind::CloudSql && !self.sql.iam_auth && !self.password.is_empty())
            .then(|| self.password.clone())
    }

    /// The Google Cloud SQL fields. Ok when the form can be used.
    fn cloudsql_form(
        &mut self,
        ui: &mut Ui,
        control: &dyn WorksControl,
        out: &mut Out<()>,
    ) -> Result<(), String> {
        out.say(
            ui,
            muted("Connecto connects as Google's Cloud SQL connectors do: your Google identity authorises it, so this Mac's IP address does not matter."),
        );
        out.say(
            ui,
            muted("Instance connection name (on the instance's Overview page)"),
        );
        field(
            ui,
            "sql-name",
            &mut self.sql.connection_name,
            "my-project:europe-west2:my-instance",
        );
        let name = InstanceName::parse(&self.sql.connection_name);
        if let Err(why) = &name
            && !self.sql.connection_name.trim().is_empty()
        {
            out.say(ui, problem(why));
        }
        out.say(ui, muted("Database"));
        field(ui, "sql-db", &mut self.sql.database, "app");
        out.say(ui, muted("Database user"));
        let user_hint = if self.sql.iam_auth {
            "Empty: the service account's IAM user"
        } else {
            "connecto_works"
        };
        field(ui, "sql-user", &mut self.sql.user, user_hint);
        let target = (
            self.sql.connection_name.trim().to_owned(),
            self.sql.user.trim().to_owned(),
        );
        if !self.password.is_empty() && self.password_for != target {
            self.password.clear();
        }
        if !self.sql.iam_auth {
            let saved = self.saved_password().is_some();
            out.say(ui, muted("Password"));
            let typed = ui.add(
                TextEdit::singleline(&mut self.password)
                    .id(Id::unique(("works-field", "sql-password")))
                    .password(true)
                    .hint_text(if !saved {
                        "Kept in the vault, never shown again"
                    } else {
                        "Saved in the vault; type to replace"
                    })
                    .desired_width(f32::INFINITY)
                    .margin(theme::FIELD_MARGIN),
            );
            if typed.changed() {
                self.password_for = target;
            }
        }
        out.say(ui, muted("Google credential"));
        ui.horizontal(|ui| {
            let key = matches!(self.sql.credential, GoogleCredential::Key(_));
            if out.choice(ui, "works-cred:key", key, "Service account key") && !key {
                self.sql.credential = GoogleCredential::Key(String::new());
                self.default_key();
            }
            if out.choice(ui, "works-cred:adc", !key, "My gcloud login") {
                self.sql.credential = GoogleCredential::Adc;
            }
        });
        if let GoogleCredential::Key(chosen) = &mut self.sql.credential {
            if self.view.google_keys.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    for key in &self.view.google_keys {
                        if out.choice(ui, &format!("works-key:{key}"), chosen == key, key) {
                            chosen.clone_from(key);
                        }
                    }
                });
            } else if !chosen.is_empty() {
                out.say(
                    ui,
                    RichText::new(format!("Key kept in the vault as {chosen}"))
                        .size(11.5)
                        .color(theme::GREEN),
                );
            }
            if out.button(ui, "works-import-key", "Import key file…", true)
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Service account key", &["json"])
                    .pick_file()
            {
                match control.import_key(&self.project, &path) {
                    Ok(imported) => {
                        *chosen = imported.secret;
                        self.imported_file = Some(path);
                        self.error.clear();
                    }
                    Err(e) => self.error = e,
                }
            }
        }
        if let Some(path) = self.imported_file.clone() {
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.say(
                ui,
                muted(format!("The key is in Connecto's encrypted vault. You can remove the downloaded copy, {file}.")),
            );
            ui.horizontal(|ui| {
                if out.button(ui, "works-trash-key", TRASH_LABEL, true) {
                    match trash(&path) {
                        Ok(()) => self.imported_file = None,
                        Err(e) => self.error = e,
                    }
                }
                if out.button(ui, "works-keep-key", "Keep it", true) {
                    self.imported_file = None;
                }
            });
        }
        out.say(ui, muted("IP type"));
        ui.horizontal(|ui| {
            for (ip, label) in [
                (IpType::Public, "Public"),
                (IpType::Private, "Private"),
                (IpType::Psc, "Private Service Connect"),
            ] {
                if out.choice(
                    ui,
                    &format!("works-ip:{label}"),
                    self.sql.ip_type == ip,
                    label,
                ) {
                    self.sql.ip_type = ip;
                }
            }
        });
        if self.sql.ip_type != IpType::Public {
            out.say(
                ui,
                muted("Private addresses work only while this Mac is on the instance's network (a VPN)."),
            );
        }
        let iam = self.sql.iam_auth;
        if out.choice(
            ui,
            "works-iam",
            iam,
            if iam {
                "☑ Sign in to the database with IAM"
            } else {
                "☐ Sign in to the database with IAM"
            },
        ) {
            self.sql.iam_auth = !iam;
        }
        name?;
        if self.sql.database.trim().is_empty() {
            return Err("Enter the database.".into());
        }
        if self.sql.credential == GoogleCredential::Key(String::new()) {
            return Err("Import a service account key, or choose My gcloud login.".into());
        }
        if !self.sql.iam_auth {
            if self.sql.user.trim().is_empty() {
                return Err("Enter the database user.".into());
            }
            if self.password.is_empty() && self.saved_password().is_none() {
                return Err("Enter the database password.".into());
            }
        }
        Ok(())
    }

    /// "How to set this up": the commands, filled in with what was typed.
    fn setup_panel(&self, ui: &mut Ui, out: &mut Out<()>) {
        egui::CollapsingHeader::new("How to set this up")
            .id_salt("works-cloudsql-setup")
            .default_open(!self.view.linked())
            .show(ui, |ui| {
                for (i, step) in setup_steps(&self.sql).iter().enumerate() {
                    out.say(ui, RichText::new(&step.title).color(theme::TEXT));
                    if let Some(code) = &step.code {
                        card(ui, |ui| {
                            ui.label(
                                RichText::new(code)
                                    .monospace()
                                    .size(12.0)
                                    .color(theme::TEXT_ROW),
                            );
                            if out.button(ui, &format!("works-copy:{i}"), "Copy", true) {
                                ui.ctx().copy_text(code.clone());
                            }
                        });
                    }
                    if !step.after.is_empty() {
                        out.say(ui, muted(&step.after));
                    }
                }
            });
    }

    /// Returns whether the sheet should close (an authoring terminal opened).
    fn types_section(
        &mut self,
        ui: &mut Ui,
        control: &dyn WorksControl,
        out: &mut Out<()>,
    ) -> bool {
        let mut leave = false;
        let typed = self.kind.trim().to_owned();
        if typed != self.publish_kind {
            self.publish_to = self
                .view
                .types
                .iter()
                .find(|t| t.name == typed)
                .and_then(|t| t.databases.clone());
            self.publish_kind = typed;
        }
        out.say(ui, heading("WORK TYPES"));
        if self.view.types.is_empty() {
            out.say(ui, muted("No work type is published yet."));
        } else {
            for kind in &self.view.types {
                ui.horizontal_wrapped(|ui| {
                    out.pill(ui, &kind.name, theme::CYAN);
                    match &kind.databases {
                        None if self.view.databases.len() > 1 => {
                            out.say(ui, muted("every database"));
                        }
                        None => {}
                        Some(list) => out.say(ui, muted(list.join(", "))),
                    }
                    if !kind.setup {
                        return;
                    }
                    match &kind.setup_ok_at {
                        Some(at) => out.say(ui, muted(format!("Setup succeeded {at}"))),
                        None => out.say(ui, warning("Setup has not succeeded.")),
                    }
                    if out.button(
                        ui,
                        &format!("works-setup:{}", kind.name),
                        "Re-run setup",
                        true,
                    ) {
                        control.submit(
                            self.project.clone(),
                            WorksAction::Setup {
                                kind: kind.name.clone(),
                            },
                        );
                    }
                });
            }
        }
        ui.add_space(4.0);
        card(ui, |ui| {
            out.say(ui, RichText::new("1. Discuss").color(theme::TEXT));
            out.say(
                ui,
                muted("Open an authoring terminal and describe the work with the agent."),
            );
            field(ui, "kind", &mut self.kind, "Work type, such as summarize");
            if self.authors.is_empty() {
                out.say(
                    ui,
                    muted("An authoring session runs in Claude or Codex, and this project's Tools setting leaves both out. Turn one on in the project's settings (Tools)."),
                );
            }
            ui.horizontal(|ui| {
                for cli in self.authors.clone() {
                    let label = cli.label();
                    if out.choice(ui, &format!("works-cli:{label}"), self.cli == cli, label) {
                        self.cli = cli;
                    }
                }
                let enabled = !self.kind.trim().is_empty() && self.authors.contains(&self.cli);
                if out.button_hint(
                    ui,
                    "works-author",
                    "Open authoring session",
                    enabled,
                    if self.authors.is_empty() {
                        "No authoring CLI is allowed in this project."
                    } else {
                        "Enter a work type first."
                    },
                ) {
                    control.submit(
                        self.project.clone(),
                        WorksAction::Author {
                            kind: self.kind.trim().into(),
                            cli: self.cli,
                        },
                    );
                    leave = true;
                }
            });
        });
        card(ui, |ui| {
            out.say(ui, RichText::new("2. Review").color(theme::TEXT));
            out.say(
                ui,
                muted("Come back here with the same work type and import the agent's reply."),
            );
            let enabled = !self.kind.trim().is_empty();
            if out.button_hint(
                ui,
                "works-import",
                "Import author reply",
                enabled,
                "Enter a work type first.",
            ) {
                match control.draft(&self.project, self.kind.trim()) {
                    Ok(draft) => {
                        self.instructions = draft.instructions;
                        self.schema = draft.schema;
                        self.setup = draft.setup;
                        self.prepare = draft.prepare;
                        // The databases the author asked for, shown below.
                        self.publish_to = draft.databases;
                        self.publish_kind = self.kind.trim().to_owned();
                        self.error.clear();
                    }
                    Err(e) => self.error = e,
                }
            }
            ui.add_space(4.0);
            out.say(ui, muted("Instructions (include {{prompt}})"));
            code_field(ui, "instructions", &mut self.instructions, "", 6);
            out.say(ui, muted("Expected JSON Schema"));
            code_field(ui, "schema", &mut self.schema, "", 6);
            out.say(
                ui,
                muted("Setup command (optional): runs when you publish, in a terminal of the agentic jobs project where you can answer it"),
            );
            code_field(
                ui,
                "setup",
                &mut self.setup,
                r#"{"argv": ["tool/setup.sh"]}"#,
                3,
            );
            out.say(
                ui,
                muted("Prepare command (optional): runs before each job with the prompt on stdin; its JSON output replaces {{prepared}}"),
            );
            code_field(
                ui,
                "prepare",
                &mut self.prepare,
                r#"{"argv": ["tool/prepare.py"], "timeout_s": 900}"#,
                3,
            );
            if !self.setup.trim().is_empty() || !self.prepare.trim().is_empty() {
                out.say(
                    ui,
                    warning("Connecto runs these commands with your rights, outside the agent's permissions. Read every argument before publishing."),
                );
            }
        });
        card(ui, |ui| {
            out.say(ui, RichText::new("3. Publish").color(theme::TEXT));
            let kind = self.kind.trim();
            let replacing = self.view.types.iter().any(|t| t.name == kind);
            let label = if replacing {
                "Replace published definition"
            } else {
                "Publish reviewed definition"
            };
            if self.view.databases.len() > 1 || self.publish_to.is_some() {
                if let Some(list) = &self.publish_to
                    && let Some(gone) = list.iter().find(|n| self.view.database(n).is_none())
                {
                    out.say(
                        ui,
                        problem(format!("This project has no database named {gone}.")),
                    );
                }
                out.say(ui, muted("Run its jobs from"));
                ui.horizontal_wrapped(|ui| {
                    if out.choice(
                        ui,
                        "works-publish-to:*",
                        self.publish_to.is_none(),
                        "Every database",
                    ) {
                        self.publish_to = None;
                    }
                    for linked in &self.view.databases {
                        let on = self
                            .publish_to
                            .as_ref()
                            .is_some_and(|list| list.contains(&linked.name));
                        if out.choice(
                            ui,
                            &format!("works-publish-to:{}", linked.name),
                            on,
                            &linked.name,
                        ) {
                            let list = self.publish_to.get_or_insert_with(Vec::new);
                            if on {
                                list.retain(|n| n != &linked.name);
                            } else {
                                list.push(linked.name.clone());
                            }
                        }
                    }
                });
            }
            let (ready, why) = if kind.is_empty() || self.instructions.trim().is_empty() {
                (false, "Enter a work type and its instructions first.")
            } else if self.publish_to.as_ref().is_some_and(Vec::is_empty) {
                (false, "Choose at least one database, or every database.")
            } else {
                (true, "")
            };
            if out.button_hint(ui, "works-publish", label, ready, why) {
                control.submit(
                    self.project.clone(),
                    WorksAction::Publish {
                        kind: kind.into(),
                        instructions: self.instructions.clone(),
                        schema: self.schema.clone(),
                        setup: Some(self.setup.clone()).filter(|c| !c.trim().is_empty()),
                        prepare: Some(self.prepare.clone()).filter(|c| !c.trim().is_empty()),
                        databases: self.publish_to.clone(),
                    },
                );
                self.error.clear();
            }
        });
        leave
    }

    fn queue_section(&self, ui: &mut Ui, out: &mut Out<()>) {
        out.say(ui, heading("QUEUE A JOB"));
        out.say(
            ui,
            warning("Queue writers can start a project agent with its configured permissions, including Bypass when selected."),
        );
        let project = self.name.replace('\'', "''");
        let kind = match self.kind.trim() {
            "" => "<work type>".to_owned(),
            kind => kind.replace('\'', "''"),
        };
        card(ui, |ui| {
            ui.label(
                RichText::new(format!(
                    "INSERT INTO connecto_works.connecto_jobs\n  (project_name, work_type, prompt)\nVALUES ('{project}', '{kind}', 'Summarize the README');"
                ))
                .monospace()
                .size(12.0)
                .color(theme::TEXT_ROW),
            );
        });
    }
}

/// The setup steps for what is typed in the form.
fn setup_steps(sql: &CloudSql) -> Vec<SetupStep> {
    cloudsql_setup_steps(
        sql,
        "Then click Import key file… and choose that file. Connecto keeps the key in its encrypted vault, and you can delete the downloaded copy.",
    )
}

#[cfg(target_os = "macos")]
const TRASH_LABEL: &str = "Move it to the Trash";
#[cfg(not(target_os = "macos"))]
const TRASH_LABEL: &str = "Delete it";

/// Move the downloaded key to the Trash (as Connecto's updater does), or
/// delete it where there is no Trash folder to move it to.
fn trash(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").ok_or("Cannot find the Trash.")?;
        let bin = PathBuf::from(home).join(".Trash");
        let name = path.file_name().ok_or("Cannot move that file.")?;
        let mut target = bin.join(name);
        if target.exists() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            target = bin.join(format!("{} {stamp}", name.to_string_lossy()));
        }
        std::fs::rename(path, &target)
            .map_err(|e| format!("Cannot move the key file to the Trash: {e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::remove_file(path).map_err(|e| format!("Cannot delete the key file: {e}"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn text(steps: &[SetupStep]) -> String {
        steps
            .iter()
            .map(|s| {
                format!(
                    "{}\n{}\n{}",
                    s.title,
                    s.code.clone().unwrap_or_default(),
                    s.after
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A view with Claude (two models) and Pi (none listed) offered, and
    /// what was submitted.
    #[derive(Default)]
    struct Control {
        view: std::sync::Mutex<WorksView>,
        submitted: std::sync::Mutex<Vec<JobAgent>>,
    }
    impl WorksControl for Control {
        fn view(&self, _: &str) -> WorksView {
            self.view.lock().unwrap().clone()
        }
        fn submit(&self, _: String, action: WorksAction) {
            if let WorksAction::JobAgent { agent } = action {
                self.submitted.lock().unwrap().push(agent.clone());
                self.view.lock().unwrap().job_agent = agent;
            }
        }
        fn draft(
            &self,
            _: &str,
            _: &str,
        ) -> Result<connecto_core::agentic_works::WorksDraft, String> {
            Err(String::new())
        }
        fn import_key(
            &self,
            _: &str,
            _: &std::path::Path,
        ) -> Result<connecto_core::agentic_works::ImportedKey, String> {
            Err(String::new())
        }
    }

    /// Draws the databases section (with the agent row) once, clicking `at`
    /// first when given: the
    /// controls drawn and the texts said.
    fn agent_row(
        ctx: &Context,
        sheet: &mut Sheet,
        control: &Control,
        at: Option<egui::Pos2>,
    ) -> forms::Marks {
        let mut marks = forms::Marks::default();
        let mut pass = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 2400.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let mut out = Out::<()>::default();
                // The whole section, as the sheet draws it: the view has no
                // database linked (review finding 3).
                sheet.database_section(ui, control, &mut out);
                marks = std::mem::take(&mut out.marks);
            });
            output.textures_delta.clear();
        };
        if let Some(pos) = at {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            };
            pass(vec![egui::Event::PointerMoved(pos), button(true)]);
            pass(vec![button(false)]);
        }
        pass(vec![]);
        marks
    }

    #[test]
    fn the_agent_for_jobs_is_chosen_with_its_model_in_the_sheet() {
        use connecto_core::agentic_works::JobAgentChoice;
        let control = Control::default();
        *control.view.lock().unwrap() = WorksView {
            job_agent_choices: vec![
                JobAgentChoice {
                    tool: ToolKind::Claude,
                    models: vec!["opus".into(), "sonnet".into()],
                },
                JobAgentChoice {
                    tool: ToolKind::Pi,
                    models: Vec::new(),
                },
            ],
            ..WorksView::default()
        };
        let mut sheet = Sheet::new("p", "p", &control);
        let ctx = Context::default();
        let marks = agent_row(&ctx, &mut sheet, &control, None);
        assert!(marks.rects.contains_key("works-agent:auto"));
        assert!(marks.rects.contains_key("works-agent:pi"));
        assert!(
            !marks.rects.contains_key("works-agent:codex"),
            "not in the project's Tools"
        );
        assert!(
            !marks.rects.contains_key("works-agent-model:"),
            "no model for Automatic"
        );
        let claude = marks.rects["works-agent:claude"].center();
        let marks = agent_row(&ctx, &mut sheet, &control, Some(claude));
        let sonnet = marks.rects["works-agent-model:sonnet"].center();
        let marks = agent_row(&ctx, &mut sheet, &control, Some(sonnet));
        let sonnet = JobAgent::Cli {
            tool: ToolKind::Claude,
            model: Some("sonnet".into()),
        };
        assert_eq!(
            *control.submitted.lock().unwrap(),
            [
                JobAgent::Cli {
                    tool: ToolKind::Claude,
                    model: None
                },
                sonnet.clone()
            ]
        );
        assert_eq!(sheet.view.job_agent, sonnet);
        assert!(
            marks
                .texts
                .iter()
                .any(|t| t.contains("run on Claude, model sonnet")),
            "{:?}",
            marks.texts
        );
        // Back to Automatic.
        let auto = marks.rects["works-agent:auto"].center();
        agent_row(&ctx, &mut sheet, &control, Some(auto));
        assert_eq!(
            control.submitted.lock().unwrap().last(),
            Some(&JobAgent::Automatic)
        );
    }

    /// The authoring choices drawn once: their marks and texts.
    fn authoring(sheet: &mut Sheet, control: &Control) -> forms::Marks {
        let ctx = Context::default();
        let mut marks = forms::Marks::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 2400.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let mut out = Out::<()>::default();
            sheet.types_section(ui, control, &mut out);
            marks = std::mem::take(&mut out.marks);
        });
        output.textures_delta.clear();
        marks
    }

    /// An authoring session is offered only on the CLIs the project's Tools
    /// setting allows; with neither Claude nor Codex, the sheet says why.
    #[test]
    fn authoring_is_offered_only_on_the_projects_allowed_clis() {
        let control = Control::default();
        let mut sheet = Sheet::new("p", "p", &control).allowing(&[ToolKind::Codex, ToolKind::Pi]);
        let marks = authoring(&mut sheet, &control);
        assert!(marks.rects.contains_key("works-cli:Codex"));
        assert!(!marks.rects.contains_key("works-cli:Claude"), "left out");
        assert_eq!(sheet.cli, ToolKind::Codex, "the allowed one is chosen");
        let mut sheet = Sheet::new("p", "p", &control).allowing(&[ToolKind::Pi]);
        let marks = authoring(&mut sheet, &control);
        assert!(!marks.rects.keys().any(|k| k.starts_with("works-cli:")));
        assert!(
            marks.texts.iter().any(|t| t.contains("leaves both out")),
            "{:?}",
            marks.texts
        );
    }

    #[test]
    fn setup_steps_use_placeholders_until_typed() {
        let all = text(&setup_steps(&CloudSql::default()));
        for p in ["<PROJECT>", "<DB>", "<USER>"] {
            assert!(all.contains(p), "{p}");
        }
        assert!(all.contains("roles/cloudsql.client"));
        assert!(!all.contains("instanceUser"));
    }

    #[test]
    fn setup_steps_fill_in_what_was_typed() {
        let sql = CloudSql {
            connection_name: "acme:europe-west2:main".into(),
            database: "app".into(),
            user: "connecto_works".into(),
            ..CloudSql::default()
        };
        let all = text(&setup_steps(&sql));
        assert!(!all.contains("<PROJECT>") && !all.contains("<DB>") && !all.contains("<USER>"));
        assert!(all.contains("--project=acme"));
        assert!(all.contains("connecto-agentic-works@acme.iam.gserviceaccount.com"));
        assert!(all.contains("~/Downloads/connecto-acme.json"));
        assert!(all.contains("GRANT CREATE ON DATABASE \"app\" TO \"connecto_works\";"));
        let reserved = text(&setup_steps(&CloudSql {
            database: "select".into(),
            user: "<x>".into(),
            ..sql.clone()
        }));
        assert!(reserved.contains("DATABASE \"select\" TO \"<x>\""));
        let literal = text(&setup_steps(&CloudSql {
            database: "<DB>".into(),
            user: "<USER>".into(),
            ..sql.clone()
        }));
        assert!(literal.contains("DATABASE \"<DB>\" TO \"<USER>\""));
        let iam = text(&setup_steps(&CloudSql {
            iam_auth: true,
            user: String::new(),
            ..sql.clone()
        }));
        assert!(iam.contains("roles/cloudsql.instanceUser"));
        assert!(iam.contains("gcloud sql instances patch main --project=acme"));
        assert!(iam.contains("--type=cloud_iam_service_account"));
        assert!(iam.contains("\"connecto-agentic-works@acme.iam\""));
        assert!(iam.contains("replaces the instance's flags"));
        let quoted = text(&setup_steps(&CloudSql {
            database: "app-prod".into(),
            user: "Works\"x".into(),
            ..CloudSql::default()
        }));
        assert!(quoted.contains("GRANT CREATE ON DATABASE \"app-prod\" TO \"Works\"\"x\";"));
        let adc = setup_steps(&CloudSql {
            credential: GoogleCredential::Adc,
            ..sql
        });
        assert_eq!(adc.len(), 1);
        assert_eq!(
            adc[0].code.as_deref(),
            Some("gcloud auth application-default login")
        );
    }
}
