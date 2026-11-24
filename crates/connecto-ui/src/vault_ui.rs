//! Settings > Secrets: the person's own secrets, for terminals to use without
//! reading them. A value typed here is sent once and never shown again; the
//! list shows names, scopes and how each may be used.

use connecto_core::{
    AskAnswer, AskKind, Binding, GeneratedKey, ProviderScope, SecretAsk, SecretEntry, VaultView,
};
use egui::{Id, RichText, TextEdit, Ui};

use crate::forms::{self, Out, heading, muted, problem, warning};
use crate::theme;

/// What the section holds while Settings is open. Its debug form hides the
/// value being typed.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct VaultForm {
    /// Where a new secret goes.
    pub(crate) scope: ProviderScope,
    pub(crate) name: String,
    pub(crate) description: String,
    /// The value being typed. Never filled from a saved one.
    pub(crate) value: String,
    /// Why the last change was not made.
    pub(crate) error: Option<String>,
    /// How one secret may be used, while it is being changed.
    pub(crate) uses: Option<UsesForm>,
    /// A key pair just generated for the value, shown until it is saved.
    pub(crate) generated: Option<GeneratedKey>,
}

impl std::fmt::Debug for VaultForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultForm")
            .field("scope", &self.scope)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("value", &connecto_core::vault::HIDDEN)
            .field("error", &self.error)
            .field("uses", &self.uses)
            .field("generated", &self.generated)
            .finish()
    }
}

/// The field a value is typed in, in Settings > Secrets.
fn settings_value_id() -> Id {
    Id::unique(("vault-field", "value"))
}

/// The fields a value is typed or pasted in, in the request dialog.
fn ask_value_ids() -> [Id; 2] {
    [
        Id::unique(("vault-field", "ask-value")),
        Id::unique("vault-ask-key"),
    ]
}

fn wipe_history(ctx: &egui::Context, ids: &[Id]) {
    for id in ids {
        if let Some(mut state) = TextEdit::load_state(ctx, *id) {
            state.clear_undoer();
            TextEdit::store_state(ctx, *id, state);
        }
    }
}

/// Forget what egui keeps of the value fields besides their text: the undo
/// history holds every earlier text, a secret included.
pub(crate) fn wipe_value_state(ctx: &egui::Context) {
    wipe_settings_value_state(ctx);
    wipe_history(ctx, &ask_value_ids());
}

/// The same for Settings' value field only: a request's draft, minimized
/// meanwhile, keeps its own.
pub(crate) fn wipe_settings_value_state(ctx: &egui::Context) {
    wipe_history(ctx, &[settings_value_id()]);
}

impl VaultForm {
    /// Let go of the value, a key made for it, and the field's history.
    fn forget_value(&mut self, ctx: &egui::Context) {
        self.value.clear();
        self.generated = None;
        wipe_value_state(ctx);
    }

    /// Where a new secret goes. A key made here is let go of with its
    /// value when the place changes, and the field's history always is.
    fn set_scope(&mut self, ctx: &egui::Context, scope: ProviderScope) {
        if scope != self.scope {
            if self.generated.is_some() {
                self.forget_value(ctx);
            }
            wipe_value_state(ctx);
        }
        self.scope = scope;
    }
}

/// Which kind of use is being set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum UseKind {
    /// Terminals cannot use it.
    #[default]
    None,
    Http,
    Command,
    Ssh,
    AgenticWorks,
    GoogleServiceAccount,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsesForm {
    pub(crate) scope: ProviderScope,
    pub(crate) name: String,
    pub(crate) kind: UseKind,
    pub(crate) base_url: String,
    pub(crate) header: String,
    pub(crate) prefix: String,
    /// One full path per line.
    pub(crate) programs: String,
    pub(crate) env: String,
}

impl UsesForm {
    /// The form for `entry`, filled from its first binding.
    fn of(entry: &SecretEntry) -> Self {
        Self::from_bindings(&entry.scope, &entry.name, &entry.bindings)
    }

    /// The form for a secret with `bindings`, filled from the first.
    pub(crate) fn from_bindings(scope: &ProviderScope, name: &str, bindings: &[Binding]) -> Self {
        let mut form = Self {
            scope: scope.clone(),
            name: name.to_owned(),
            header: "Authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            ..Self::default()
        };
        match bindings.first() {
            Some(Binding::Http {
                base_url,
                header,
                prefix,
            }) => {
                form.kind = UseKind::Http;
                form.base_url.clone_from(base_url);
                form.header.clone_from(header);
                form.prefix.clone_from(prefix);
            }
            Some(Binding::Command { programs, env }) => {
                form.kind = UseKind::Command;
                form.programs = programs.join("\n");
                form.env.clone_from(env);
            }
            Some(Binding::AgenticWorks) => form.kind = UseKind::AgenticWorks,
            Some(Binding::GoogleServiceAccount) => form.kind = UseKind::GoogleServiceAccount,
            Some(Binding::Ssh) => form.kind = UseKind::Ssh,
            None => {}
        }
        form
    }

    /// The bindings the form says.
    pub(crate) fn bindings(&self) -> Vec<Binding> {
        match self.kind {
            UseKind::None => Vec::new(),
            UseKind::Http => vec![Binding::Http {
                base_url: self.base_url.trim().trim_end_matches('/').to_owned(),
                header: self.header.trim().to_owned(),
                prefix: self.prefix.clone(),
            }],
            UseKind::Command => vec![Binding::Command {
                programs: self
                    .programs
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect(),
                env: self.env.trim().to_owned(),
            }],
            UseKind::AgenticWorks => vec![Binding::AgenticWorks],
            UseKind::GoogleServiceAccount => vec![Binding::GoogleServiceAccount],
            UseKind::Ssh => vec![Binding::Ssh],
        }
    }
}

/// A change to secrets. Its debug form hides a value.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum VaultAction {
    Put {
        scope: ProviderScope,
        name: String,
        description: String,
        value: String,
    },
    SetBindings {
        scope: ProviderScope,
        name: String,
        bindings: Vec<Binding>,
    },
    Delete {
        scope: ProviderScope,
        name: String,
    },
    /// Make a new SSH key pair for the value being typed.
    Generate {
        comment: String,
    },
    /// Make a new SSH key pair where the secrets are kept (another
    /// machine) and keep it as `name`: its private half never comes here.
    GenerateKept {
        scope: ProviderScope,
        name: String,
        description: String,
    },
    /// Show `scope`'s secrets after the name `from` (`None`: from the
    /// first), for a list shown in part.
    ShowFrom {
        scope: ProviderScope,
        from: Option<String>,
    },
}

impl std::fmt::Debug for VaultAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Put {
                scope,
                name,
                description,
                ..
            } => f
                .debug_struct("Put")
                .field("scope", scope)
                .field("name", name)
                .field("description", description)
                .field("value", &connecto_core::vault::HIDDEN)
                .finish(),
            Self::SetBindings {
                scope,
                name,
                bindings,
            } => f
                .debug_struct("SetBindings")
                .field("scope", scope)
                .field("name", name)
                .field("bindings", bindings)
                .finish(),
            Self::Delete { scope, name } => f
                .debug_struct("Delete")
                .field("scope", scope)
                .field("name", name)
                .finish(),
            Self::Generate { comment } => f
                .debug_struct("Generate")
                .field("comment", comment)
                .finish(),
            Self::GenerateKept {
                scope,
                name,
                description,
            } => f
                .debug_struct("GenerateKept")
                .field("scope", scope)
                .field("name", name)
                .field("description", description)
                .finish(),
            Self::ShowFrom { scope, from } => f
                .debug_struct("ShowFrom")
                .field("scope", scope)
                .field("from", from)
                .finish(),
        }
    }
}

/// A project as the section names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectName {
    pub(crate) id: String,
    pub(crate) name: String,
    /// The machine that keeps the project's secrets, when it is not this
    /// one (a paired host's project).
    pub(crate) elsewhere: Option<String>,
}

/// Whether `scope` is a paired host's project's, kept on that machine.
pub(crate) fn is_remote_scope(scope: &ProviderScope) -> bool {
    scope.project().is_some_and(crate::app::is_remote_id)
}

/// The machine that keeps `scope`'s secrets, when it is not this one.
pub(crate) fn kept_elsewhere<'a>(
    scope: &ProviderScope,
    projects: &'a [ProjectName],
) -> Option<&'a str> {
    let id = scope.project()?;
    projects
        .iter()
        .find(|p| p.id == id)
        .and_then(|p| p.elsewhere.as_deref())
}

fn scope_words(scope: &ProviderScope, projects: &[ProjectName]) -> String {
    match scope {
        ProviderScope::Global => "App-wide".to_owned(),
        ProviderScope::Project(id) => projects.iter().find(|p| &p.id == id).map_or_else(
            || "A project that is not open".to_owned(),
            |p| p.name.clone(),
        ),
    }
}

fn field(ui: &mut Ui, id: &str, text: &mut String, hint: &str, password: bool) -> egui::Response {
    ui.add(
        TextEdit::singleline(text)
            .id(Id::unique(("vault-field", id)))
            .hint_text(hint)
            .password(password)
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    )
}

/// The section. `projects`: the open projects, for a project's own secrets.
pub(crate) fn section<A: From<VaultAction>>(
    ui: &mut Ui,
    form: &mut VaultForm,
    view: &VaultView,
    projects: &[ProjectName],
    out: &mut Out<A>,
) {
    out.say(ui, heading("SECRETS"));
    out.say(
        ui,
        muted("Keep API keys and tokens here for terminals to use without reading them. A terminal sees a secret's name, never its value: Connecto itself adds the value to a request, or runs the one program you allow with it. Wherever a terminal's text goes (connecto read, history, memory), the value is hidden."),
    );
    if !view.protection.is_empty() {
        out.say(ui, muted(&view.protection));
    }
    if let Some(why) = &view.problem {
        out.say(ui, problem(why));
    }
    if let Some(why) = &form.error {
        out.say(ui, problem(why));
    }

    if view.entries.is_empty() {
        out.say(ui, muted("No secrets yet."));
    }
    for entry in &view.entries {
        entry_card(ui, form, entry, projects, out);
    }
    // A list shown in part: where it begins, and the way on and back.
    for page in &view.pages {
        let key = page.scope.project().unwrap_or("app");
        let place = scope_words(&page.scope, projects);
        out.say(
            ui,
            muted(match &page.from {
                Some(after) => format!("{place}: the secrets after {after}."),
                None => format!("{place}: the first secrets."),
            }),
        );
        ui.horizontal(|ui| {
            if page.from.is_some()
                && out.button(ui, &format!("vault-first:{key}"), "First secrets", true)
            {
                out.actions.push(A::from(VaultAction::ShowFrom {
                    scope: page.scope.clone(),
                    from: None,
                }));
            }
            if let Some(next) = &page.next
                && out.button(ui, &format!("vault-next:{key}"), "Next secrets", true)
            {
                out.actions.push(A::from(VaultAction::ShowFrom {
                    scope: page.scope.clone(),
                    from: Some(next.clone()),
                }));
            }
        });
    }

    forms::separator(ui);
    add_form(ui, form, view, projects, out);
}

fn entry_card<A: From<VaultAction>>(
    ui: &mut Ui,
    form: &mut VaultForm,
    entry: &SecretEntry,
    projects: &[ProjectName],
    out: &mut Out<A>,
) {
    let key = format!("{}:{}", entry.scope.project().unwrap_or("app"), entry.name);
    forms::card(ui, |ui| {
        out.say(
            ui,
            RichText::new(&entry.name).monospace().color(theme::TEXT),
        );
        out.say(ui, muted(scope_words(&entry.scope, projects)));
        if !entry.description.is_empty() {
            out.say(ui, muted(&entry.description));
        }
        let uses = entry
            .bindings
            .iter()
            .map(|binding| match binding {
                Binding::Http {
                    base_url, header, ..
                } => {
                    format!("Sent as {header} to {base_url}")
                }
                Binding::Command { programs, env } => {
                    format!("Given as {env} to {}", programs.join(", "))
                }
                Binding::AgenticWorks => "Links this project's Agentic Works queue".to_owned(),
                Binding::GoogleServiceAccount => {
                    "Authorises this project's Agentic Works Cloud SQL connections".to_owned()
                }
                Binding::Ssh => "Signs SSH logins through Connecto's ssh-agent".to_owned(),
            })
            .collect::<Vec<_>>();
        if let Some(public) = &entry.public_key {
            out.say(
                ui,
                RichText::new("Public key (give this to the server)").color(theme::TEXT),
            );
            out.say(ui, RichText::new(public).monospace().size(11.0));
            if out.button(
                ui,
                &format!("vault-copy-public:{key}"),
                "Copy public key",
                true,
            ) {
                ui.ctx().copy_text(public.clone());
            }
        }
        if uses.is_empty() {
            out.say(ui, warning("Terminals cannot use it yet: set how below."));
        } else {
            for line in uses {
                out.say(ui, RichText::new(line).size(11.5).color(theme::GREEN));
            }
        }
        let editing = form
            .uses
            .as_ref()
            .is_some_and(|u| u.scope == entry.scope && u.name == entry.name);
        ui.horizontal(|ui| {
            if out.button(
                ui,
                &format!("vault-uses:{key}"),
                "How it is used…",
                !editing,
            ) {
                form.uses = Some(UsesForm::of(entry));
                form.error = None;
            }
            if out.button(ui, &format!("vault-replace:{key}"), "Replace value…", true) {
                form.set_scope(ui.ctx(), entry.scope.clone());
                form.forget_value(ui.ctx());
                form.name.clone_from(&entry.name);
                form.description.clone_from(&entry.description);
                form.error = None;
            }
            if out.button(ui, &format!("vault-remove:{key}"), "Remove", true) {
                form.error = None;
                if editing {
                    form.uses = None;
                }
                out.actions.push(A::from(VaultAction::Delete {
                    scope: entry.scope.clone(),
                    name: entry.name.clone(),
                }));
            }
        });
        if editing && let Some(uses) = &mut form.uses {
            if let Some(action) = uses_editor(ui, uses, &key, out) {
                if matches!(action, VaultAction::SetBindings { .. }) {
                    form.error = None;
                }
                out.actions.push(A::from(action));
                form.uses = None;
            } else if uses.kind == UseKind::None && uses.name.is_empty() {
                form.uses = None;
            }
        }
    });
}

/// The editor for how one secret is used. Returns the change on Save.
fn uses_editor<A>(
    ui: &mut Ui,
    uses: &mut UsesForm,
    key: &str,
    out: &mut Out<A>,
) -> Option<VaultAction> {
    forms::separator(ui);
    let _ = uses_fields(ui, uses, key, out);
    let mut action = None;
    ui.horizontal(|ui| {
        if out.button(ui, &format!("vault-uses-save:{key}"), "Save", true) {
            action = Some(VaultAction::SetBindings {
                scope: uses.scope.clone(),
                name: uses.name.clone(),
                bindings: uses.bindings(),
            });
        }
        if out.button(ui, &format!("vault-uses-cancel:{key}"), "Cancel", true) {
            uses.kind = UseKind::None;
            uses.name.clear();
        }
    });
    action
}

/// The kind of use and its fields.
/// Returns whether the person chose a kind of use.
fn uses_fields<A>(ui: &mut Ui, uses: &mut UsesForm, key: &str, out: &mut Out<A>) -> bool {
    let mut chosen = false;
    ui.horizontal(|ui| {
        for (kind, label) in [
            (UseKind::None, "Not usable"),
            (UseKind::Http, "Web requests"),
            (UseKind::Command, "One program"),
            (UseKind::Ssh, "SSH key"),
            (UseKind::AgenticWorks, "Agentic Works database"),
            (
                UseKind::GoogleServiceAccount,
                "Google Cloud service account",
            ),
        ] {
            if out.choice(
                ui,
                &format!("vault-kind:{key}:{label}"),
                uses.kind == kind,
                label,
            ) {
                uses.kind = kind;
                chosen = true;
            }
        }
    });
    match uses.kind {
        UseKind::AgenticWorks => {
            out.say(
                ui,
                muted("The secret is a PostgreSQL connection string, or a Cloud SQL database password, for this project's Agentic Works queue. Only Connecto uses it; no terminal sees it."),
            );
        }
        UseKind::GoogleServiceAccount => {
            out.say(
                ui,
                muted("The secret is a Google Cloud service account key (its JSON file) that authorises this project's Agentic Works Cloud SQL connections. Only Connecto uses it; no terminal sees it. Import it from Terminal → Agentic Works…"),
            );
        }
        UseKind::None => {
            out.say(ui, muted("No terminal can use this secret."));
        }
        UseKind::Ssh => {
            out.say(
                ui,
                muted("The secret is an SSH private key. Terminals get its public key and an ssh-agent that signs with it; the key itself never leaves Connecto."),
            );
        }
        UseKind::Http => {
            out.say(
                ui,
                muted("Terminals call the address Connecto gives them; Connecto sends the request on to this address with the secret in the header. The secret goes nowhere else."),
            );
            field(
                ui,
                &format!("{key}:url"),
                &mut uses.base_url,
                "https://api.example.com",
                false,
            );
            ui.horizontal(|ui| {
                field(
                    ui,
                    &format!("{key}:header"),
                    &mut uses.header,
                    "Authorization",
                    false,
                );
            });
            field(
                ui,
                &format!("{key}:prefix"),
                &mut uses.prefix,
                "Before the secret, such as `Bearer `",
                false,
            );
        }
        UseKind::Command => {
            out.say(
                ui,
                warning("Connecto runs only these programs with the secret, and shows the terminal their output with the secret hidden. A program that prints the secret in another form can still reveal it: allow only programs you trust with it."),
            );
            ui.add(
                TextEdit::multiline(&mut uses.programs)
                    .id(Id::unique(("vault-field", key, "programs")))
                    .hint_text("Full path of each program, one per line: /opt/homebrew/bin/gh")
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .margin(theme::FIELD_MARGIN),
            );
            field(
                ui,
                &format!("{key}:env"),
                &mut uses.env,
                "Environment variable, such as GH_TOKEN",
                false,
            );
        }
    }
    chosen
}

fn add_form<A: From<VaultAction>>(
    ui: &mut Ui,
    form: &mut VaultForm,
    view: &VaultView,
    projects: &[ProjectName],
    out: &mut Out<A>,
) {
    let replacing = view
        .entries
        .iter()
        .any(|e| e.scope == form.scope && e.name == form.name.trim());
    out.say(
        ui,
        RichText::new(if replacing {
            "Replace a secret's value"
        } else {
            "Add a secret"
        })
        .color(theme::TEXT),
    );
    ui.horizontal_wrapped(|ui| {
        let mut scopes = vec![ProviderScope::Global];
        scopes.extend(
            projects
                .iter()
                .map(|p| ProviderScope::Project(p.id.clone())),
        );
        for scope in scopes {
            let label = scope_words(&scope, projects);
            let key = format!("vault-scope:{}", scope.project().unwrap_or("app"));
            if out.choice(ui, &key, form.scope == scope, &label) {
                // A key made here never goes elsewhere: its private half is
                // let go of.
                form.set_scope(ui.ctx(), scope);
            }
        }
    });
    let name = field(
        ui,
        "name",
        &mut form.name,
        "Name, such as GITHUB_TOKEN",
        false,
    );
    out.mark("vault-name", name.rect);
    let description = field(
        ui,
        "description",
        &mut form.description,
        "What it is for (optional)",
        false,
    );
    out.mark("vault-description", description.rect);
    let value = field(
        ui,
        "value",
        &mut form.value,
        if replacing {
            "Paste the new value"
        } else {
            "Paste the value"
        },
        true,
    );
    out.mark("vault-value", value.rect);
    if is_remote_scope(&form.scope) {
        // Made and kept there: this machine sees its public half only. A
        // machine that is not connected now makes none, and none is made
        // here instead.
        let host = kept_elsewhere(&form.scope, projects);
        if host.is_none() {
            out.say(ui, problem("That machine is not connected now."));
        }
        if out.button(
            ui,
            "vault-generate",
            &format!("Generate an SSH key on {}", host.unwrap_or("that machine")),
            host.is_some() && !form.name.trim().is_empty(),
        ) {
            form.error = None;
            out.actions.push(A::from(VaultAction::GenerateKept {
                scope: form.scope.clone(),
                name: form.name.trim().to_owned(),
                description: std::mem::take(&mut form.description),
            }));
            form.name.clear();
            form.forget_value(ui.ctx());
        }
    } else if out.button(ui, "vault-generate", "Generate an SSH key", true) {
        form.error = None;
        out.actions.push(A::from(VaultAction::Generate {
            comment: form.name.trim().to_owned(),
        }));
    }
    if let Some(made) = &form.generated {
        show_key(ui, made, "vault-generated", out);
    }
    let ready = !form.name.trim().is_empty() && !form.value.is_empty();
    if out.button(
        ui,
        "vault-save",
        if replacing {
            "Replace value"
        } else {
            "Add secret"
        },
        ready,
    ) {
        form.error = None;
        out.actions.push(A::from(VaultAction::Put {
            scope: form.scope.clone(),
            name: form.name.trim().to_owned(),
            description: std::mem::take(&mut form.description),
            value: std::mem::take(&mut form.value),
        }));
        form.name.clear();
        form.forget_value(ui.ctx());
    }
}

/// A generated key pair, both halves readable and copyable, for the person
/// to look at before saving it.
fn show_key<A>(ui: &mut Ui, made: &GeneratedKey, key: &str, out: &mut Out<A>) {
    out.say(
        ui,
        RichText::new("Public key (give this to the server)").color(theme::TEXT),
    );
    let mut public = made.public_openssh.clone();
    ui.add(
        TextEdit::multiline(&mut public)
            .id(Id::unique((key, "public")))
            .font(egui::TextStyle::Monospace)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .interactive(true),
    );
    if out.button(ui, &format!("{key}-copy-public"), "Copy public key", true) {
        ui.ctx().copy_text(made.public_openssh.clone());
    }
    out.say(
        ui,
        RichText::new("Private key (kept by Connecto; terminals never get it)").color(theme::TEXT),
    );
    let mut private = made.private_openssh.clone();
    ui.add(
        TextEdit::multiline(&mut private)
            .id(Id::unique((key, "private")))
            .font(egui::TextStyle::Monospace)
            .desired_rows(7)
            .desired_width(f32::INFINITY)
            .interactive(true),
    );
}

/// The dialog for a terminal's request, while it is open. Its debug form
/// hides the value being typed.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AskForm {
    pub(crate) id: u64,
    /// The machine whose terminal asks, when it is not this one.
    pub(crate) host: Option<String>,
    /// The answer went to that machine, which has not taken or refused it
    /// yet.
    pub(crate) sent: bool,
    /// The person chose a use here: needed before approving a request
    /// whose proposed use could not be sent ([`SecretAsk::proposed_omitted`]).
    pub(crate) use_chosen: bool,
    pub(crate) scope: ProviderScope,
    pub(crate) uses: UsesForm,
    /// The value being typed or generated. Never filled from a saved one.
    pub(crate) value: String,
    pub(crate) generated: Option<GeneratedKey>,
    pub(crate) error: Option<String>,
}

impl std::fmt::Debug for AskForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskForm")
            .field("id", &self.id)
            .field("host", &self.host)
            .field("sent", &self.sent)
            .field("scope", &self.scope)
            .field("uses", &self.uses)
            .field("value", &connecto_core::vault::HIDDEN)
            .field("generated", &self.generated)
            .field("error", &self.error)
            .finish()
    }
}

impl AskForm {
    /// The form for `ask` (of `host`'s terminal, when not this machine's):
    /// its project's own by default, used as proposed.
    pub(crate) fn new(ask: &SecretAsk, host: Option<&str>) -> Self {
        let scope = ask
            .project
            .clone()
            .map_or(ProviderScope::Global, ProviderScope::Project);
        Self {
            id: ask.id,
            host: host.map(str::to_owned),
            sent: false,
            use_chosen: false,
            uses: UsesForm::from_bindings(&scope, &ask.name, &ask.proposed),
            scope,
            value: String::new(),
            generated: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AskAction {
    Answer {
        id: u64,
        answer: AskAnswer,
    },
    /// Make a key pair to look at; the window fills the form with it.
    Generate {
        comment: String,
    },
    /// Shrink it to a card in the corner; what is typed is kept.
    Minimize,
}

/// Who asks for a secret, as the dialog names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Asker {
    /// The terminal's id, as agents address it (`claude-3`).
    pub(crate) terminal: String,
    /// The name the person or an agent gave it, when it has one.
    pub(crate) name: Option<String>,
    /// The kind of agent in it, when known.
    pub(crate) tool: Option<connecto_core::ToolKind>,
}

impl Asker {
    /// The terminal `id`, named and of the kind its entry says; with no
    /// entry (another machine's terminal), the kind its id begins with.
    pub(crate) fn new(id: &str, name: Option<&str>, tool: Option<connecto_core::ToolKind>) -> Self {
        let tool = tool.or_else(|| {
            let (word, number) = id.rsplit_once('-')?;
            number.parse::<u32>().ok()?;
            connecto_core::ToolKind::from_name(word)
        });
        Self {
            terminal: id.to_owned(),
            name: name
                .map(str::trim)
                .filter(|n| !n.is_empty() && *n != id)
                .map(str::to_owned),
            tool,
        }
    }

    /// What Connecto knows of the terminal itself: "Terminal claude-3
    /// (Claude)". Its name is not part of it ([`Asker::label`]).
    pub(crate) fn words(&self) -> String {
        let id = connecto_core::vault::display_line(&self.terminal, 120);
        match self.tool {
            Some(tool) => format!("Terminal {id} ({})", tool.label()),
            None => format!("Terminal {id}"),
        }
    }

    /// The name it was given, which the person or an agent chose, on one
    /// line that cannot break out of its quotes.
    pub(crate) fn label(&self) -> Option<String> {
        self.name
            .as_deref()
            .map(|name| connecto_core::vault::display_line(name, 80))
            .filter(|name| !name.is_empty())
    }
}

/// Where the secret will go, one sentence per use of `chosen` (exactly what
/// Approve grants), then how that compares with what the terminal proposed.
pub(crate) fn use_lines(chosen: &[Binding], proposed: &[Binding], omitted: bool) -> Vec<String> {
    let mut lines: Vec<String> = chosen.iter().map(Binding::plain_use).collect();
    if lines.is_empty() {
        lines.push("Nowhere: approved like this, no terminal can use it.".to_owned());
    }
    // The terminal's proposal as the form would write it (an address
    // without its last `/`), one use at a time.
    let as_proposed: Vec<Binding> = proposed
        .iter()
        .flat_map(|b| {
            UsesForm::from_bindings(&ProviderScope::Global, "", std::slice::from_ref(b)).bindings()
        })
        .collect();
    if omitted {
        lines.push(
            "The terminal's proposed use was too long to send here: Approve grants only what is above."
                .to_owned(),
        );
    } else if chosen == as_proposed.as_slice() {
        lines.push(
            "Approve grants exactly this, as the terminal proposed. You can change it below."
                .to_owned(),
        );
    } else if proposed.is_empty() {
        lines.push("The terminal proposed no use. Approve grants only what is above.".to_owned());
    } else {
        let theirs: Vec<String> = proposed.iter().map(Binding::plain_use).collect();
        lines.push(format!(
            "The terminal proposed: {} Approve grants only what is above.",
            theirs.join(" ")
        ));
    }
    lines
}

/// A terminal asks for a secret: what, why, where it goes, how it may be
/// used, and the value, typed or generated. Approve keeps it and tells the
/// terminal how to use it; Decline tells it no. Neither tells it the value.
pub(crate) fn ask_dialog(
    ctx: &egui::Context,
    form: &mut AskForm,
    ask: &SecretAsk,
    asker: &Asker,
    waiting: usize,
    projects: &[ProjectName],
    out: &mut Out<AskAction>,
) {
    let elsewhere = form.host.clone();
    crate::dialog::card(ctx, "vault-ask", |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            if crate::dialog::heading_with_minimize(
                ui,
                out,
                "A TERMINAL ASKS FOR A SECRET",
                "vault-ask-minimize",
            ) {
                out.actions.push(AskAction::Minimize);
            }
            let place = ask
                .project
                .as_ref()
                .map(|id| scope_words(&ProviderScope::Project(id.clone()), projects))
                .unwrap_or_else(|| "no project".to_owned());
            out.say(
                ui,
                RichText::new(format!(
                    "{} of project {place} asks for {} named {}.",
                    asker.words(),
                    ask.kind.words(),
                    ask.name
                ))
                .size(14.0)
                .strong()
                .color(theme::TEXT),
            );
            if let Some(label) = asker.label() {
                out.say(
                    ui,
                    muted(format!(
                        "Its tab is named \u{201c}{label}\u{201d} (set by you or an agent; not checked)."
                    )),
                );
            }
            forms::separator(ui);
            out.say(ui, heading("WHAT IT IS FOR"));
            out.say(
                ui,
                RichText::new(format!(
                    "\u{201c}{}\u{201d}",
                    connecto_core::vault::display_line(&ask.reason, 300)
                ))
                    .size(14.0)
                    .color(theme::TEXT),
            );
            out.say(
                ui,
                muted("The agent's own words: Connecto cannot check them. Decline if they do not match what you asked it to do."),
            );
            forms::separator(ui);
            out.say(ui, heading("WHERE IT WILL BE USED"));
            for line in use_lines(&form.uses.bindings(), &ask.proposed, ask.proposed_omitted) {
                out.say(ui, RichText::new(line).color(theme::TEXT));
            }
            out.say(
                ui,
                muted(
                    "The terminal will never see the value: only its name and how it may use it.",
                ),
            );
            if let Some(host) = &elsewhere {
                out.say(
                    ui,
                    muted(format!(
                        "The terminal runs on {host}. What you give is sent there, kept in that project's secrets, and never shown here again."
                    )),
                );
            }
            if waiting > 1 {
                out.say(
                    ui,
                    muted(format!("{} more requests are waiting.", waiting - 1)),
                );
            }
            if let Some(why) = &form.error {
                out.say(ui, problem(why));
            }
            forms::separator(ui);
            out.say(ui, RichText::new("Keep it for").color(theme::TEXT));
            ui.horizontal_wrapped(|ui| {
                // Another machine's request is kept in its project only.
                let mut scopes = if elsewhere.is_some() {
                    Vec::new()
                } else {
                    vec![ProviderScope::Global]
                };
                if let Some(id) = &ask.project {
                    scopes.push(ProviderScope::Project(id.clone()));
                }
                for scope in scopes {
                    let label = scope_words(&scope, projects);
                    let key = format!("vault-ask-scope:{}", scope.project().unwrap_or("app"));
                    if out.choice(ui, &key, form.scope == scope, &label) {
                        if form.scope != scope {
                            wipe_value_state(ui.ctx());
                        }
                        form.uses.scope = scope.clone();
                        form.scope = scope;
                    }
                }
            });
            forms::separator(ui);
            out.say(ui, RichText::new("How it may be used").color(theme::TEXT));
            if ask.proposed_omitted {
                out.say(
                    ui,
                    warning("The terminal proposed a use too long to send to this machine. Choose how the secret may be used before approving."),
                );
            }
            if uses_fields(ui, &mut form.uses, "vault-ask", out) {
                form.use_chosen = true;
            }
            forms::separator(ui);
            out.say(ui, RichText::new("The value").color(theme::TEXT));
            if let (Some(host), true) = (
                &elsewhere,
                ask.kind == AskKind::SshKey || form.uses.kind == UseKind::Ssh,
            ) {
                // Made and kept there: its private half never comes here.
                // It is kept as an SSH key only, so it is offered only when
                // that is the use chosen above, which the summary describes.
                if form.uses.kind == UseKind::Ssh {
                    if out.button(
                        ui,
                        "vault-ask-generate",
                        &format!("Generate a new SSH key on {host} and approve"),
                        !form.sent,
                    ) {
                        form.error = None;
                        out.actions.push(AskAction::Answer {
                            id: ask.id,
                            answer: AskAnswer::GenerateSshKey {
                                scope: form.scope.clone(),
                            },
                        });
                    }
                } else {
                    out.say(
                        ui,
                        muted(format!(
                            "To have {host} generate a key, choose \u{201c}SSH key\u{201d} above."
                        )),
                    );
                }
                out.say(
                    ui,
                    muted("Or paste a private key (OpenSSH, without a passphrase):"),
                );
                let pasted = ui.add(
                    TextEdit::multiline(&mut form.value)
                        .id(Id::unique("vault-ask-key"))
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .margin(theme::FIELD_MARGIN),
                );
                out.mark("vault-ask-value", pasted.rect);
            } else if ask.kind == AskKind::SshKey || form.uses.kind == UseKind::Ssh {
                if out.button(ui, "vault-ask-generate", "Generate a new SSH key", true) {
                    form.error = None;
                    out.actions.push(AskAction::Generate {
                        comment: format!("connecto {}", ask.name),
                    });
                }
                if let Some(made) = &form.generated {
                    show_key(ui, made, "vault-ask-generated", out);
                } else {
                    out.say(
                        ui,
                        muted("Or paste a private key (OpenSSH, without a passphrase):"),
                    );
                    let pasted = ui.add(
                        TextEdit::multiline(&mut form.value)
                            .id(Id::unique("vault-ask-key"))
                            .font(egui::TextStyle::Monospace)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .margin(theme::FIELD_MARGIN),
                    );
                    out.mark("vault-ask-value", pasted.rect);
                }
            } else {
                let typed = field(ui, "ask-value", &mut form.value, "Paste the value", true);
                out.mark("vault-ask-value", typed.rect);
            }
            forms::separator(ui);
            if let (Some(host), true) = (&elsewhere, form.sent) {
                out.say(ui, muted(format!("Sent to {host}; waiting for it to answer…")));
            }
            let open = !form.sent;
            ui.horizontal(|ui| {
                if out.button(
                    ui,
                    "vault-ask-approve",
                    "Approve",
                    open && !form.value.trim().is_empty()
                        && (form.use_chosen || !ask.proposed_omitted),
                ) {
                    out.actions.push(AskAction::Answer {
                        id: ask.id,
                        answer: AskAnswer::Provide {
                            scope: form.scope.clone(),
                            value: form.value.clone(),
                            bindings: form.uses.bindings(),
                        },
                    });
                }
                if out.button(ui, "vault-ask-decline", "Decline", open) {
                    out.actions.push(AskAction::Answer {
                        id: ask.id,
                        answer: AskAnswer::Decline,
                    });
                }
            });
        });
    });
}

/// Carry out a change.
///
/// # Errors
/// Why not, for the section to show; never a value.
pub(crate) fn apply(
    vault: &dyn connecto_core::VaultControl,
    action: VaultAction,
) -> Result<(), String> {
    match action {
        VaultAction::Put {
            scope,
            name,
            description,
            value,
        } => vault.put(&scope, &name, &description, &value),
        VaultAction::SetBindings {
            scope,
            name,
            bindings,
        } => vault.set_bindings(&scope, &name, bindings),
        VaultAction::Delete { scope, name } => vault.delete(&scope, &name),
        VaultAction::GenerateKept {
            scope,
            name,
            description,
        } => vault.put_generated_key(&scope, &name, &description),
        VaultAction::ShowFrom { scope, from } => {
            vault.show_secrets_from(&scope, from);
            Ok(())
        }
        // Carried out by the window, which shows the key it gets.
        VaultAction::Generate { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uses_form_reads_and_writes_a_binding() {
        let entry = SecretEntry {
            name: "GH".into(),
            scope: ProviderScope::Global,
            description: String::new(),
            bindings: vec![Binding::Command {
                programs: vec!["/opt/homebrew/bin/gh".into()],
                env: "GH_TOKEN".into(),
            }],
            updated_unix: 0,
            last_used_unix: None,
            public_key: None,
        };
        let mut form = UsesForm::of(&entry);
        assert_eq!(form.kind, UseKind::Command);
        assert_eq!(form.bindings(), entry.bindings);
        form.kind = UseKind::Http;
        form.base_url = " https://api.github.com/ ".into();
        assert_eq!(
            form.bindings(),
            [Binding::Http {
                base_url: "https://api.github.com".into(),
                header: "Authorization".into(),
                prefix: "Bearer ".into(),
            }]
        );
        form.kind = UseKind::None;
        assert!(form.bindings().is_empty());
    }
}
