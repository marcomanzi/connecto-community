//! The "Set up model providers…" dialog, for Claude or Pi: app-wide (in
//! Settings, Agents) or for one project (Project settings, Tools, when that
//! tool's config is Isolated). A key typed here is sent once and never shown again.

use connecto_core::providers::{
    CLAUDE_PROVIDERS_SUPPORTED, TOGETHER_KEYS_PAGE, TOGETHERLINK_DOCS, TOGETHERLINK_INSTALL,
};
use connecto_core::{
    ClaudeProvidersView, PiLocalState, PiProvider, PiProvidersView, ProviderScope,
    TogetherLinkState, ToolKind,
};
use egui::{Id, RichText, TextEdit, Ui};

use crate::forms::{self, Out, heading, muted, problem, warning};
use crate::theme;

/// What the dialog holds while it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvidersDialog {
    /// Whose providers: the app's, or one project's.
    pub(crate) scope: ProviderScope,
    /// Claude or Pi.
    pub(crate) tool: ToolKind,
    /// The keys being typed for Pi's providers, in the order of
    /// [`PiProvider::ALL`]. Never filled from the saved ones.
    pub(crate) pi_keys: [String; 2],
    /// The LM Studio address being typed. `None` until the dialog first
    /// shows the saved one.
    pub(crate) pi_lmstudio: Option<String>,
    /// What is typed to find a model.
    pub(crate) pi_model_filter: String,
    /// The Together AI key being typed for Claude. Never filled from the
    /// saved one.
    pub(crate) together_key: String,
    /// Why a change was not saved.
    pub(crate) error: Option<String>,
}

impl ProvidersDialog {
    pub(crate) fn new(scope: ProviderScope, tool: ToolKind) -> Self {
        Self {
            scope,
            tool,
            pi_keys: Default::default(),
            pi_lmstudio: None,
            pi_model_filter: String::new(),
            together_key: String::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProvidersAction {
    /// Save (`Some`) or remove (`None`) the key Pi is given for a provider.
    SetPiKey {
        scope: ProviderScope,
        provider: PiProvider,
        key: Option<String>,
    },
    /// The address of the local LM Studio server; empty turns it off.
    SetPiLmStudio { scope: ProviderScope, url: String },
    /// The model a new Pi terminal starts on; `None` leaves it to Pi.
    SetPiDefaultModel {
        scope: ProviderScope,
        model: Option<String>,
    },
    /// Ask the local server again.
    CheckPiLocal(ProviderScope),
    /// Save (`Some`) or remove (`None`) the Together AI key Claude is
    /// started with.
    SetClaudeTogetherKey {
        scope: ProviderScope,
        key: Option<String>,
    },
    /// Look again for TogetherLink.
    CheckTogetherLink,
    /// The person agreed to install TogetherLink.
    InstallTogetherLink,
    /// The person declined, for now.
    DeclineTogetherLink,
    /// A project uses providers of its own for `tool`, or the app-wide ones.
    SetOwn {
        project: String,
        tool: ToolKind,
        own: bool,
    },
}

/// Whether Claude's card in Settings > Agents offers its providers: only
/// where TogetherLink runs (not on Windows), and once Claude is installed.
pub(crate) fn claude_setup_offered(supported: bool, installed: bool) -> bool {
    supported && installed
}

/// Whether a project's Tools section offers `tool`'s own providers: Claude
/// (where TogetherLink runs) and Pi, when that tool's config is Isolated.
pub(crate) fn project_setup_offered(
    supported: bool,
    tool: ToolKind,
    mode: connecto_core::ConfigMode,
) -> bool {
    mode == connecto_core::ConfigMode::Isolated
        && match tool {
            ToolKind::Claude => supported,
            ToolKind::Pi => true,
            // OpenCode signs in to its providers itself (`/connect`).
            ToolKind::Codex | ToolKind::Antigravity | ToolKind::OpenCode | ToolKind::Shell => false,
        }
}

/// Carry out a change made in the dialog.
///
/// # Errors
/// Why it was not saved, for the dialog to show; never a key.
pub(crate) fn apply(
    agents: &dyn connecto_core::AgentsControl,
    action: ProvidersAction,
) -> Result<(), String> {
    match action {
        ProvidersAction::SetPiKey {
            scope,
            provider,
            key,
        } => agents.set_pi_key(&scope, provider, key),
        ProvidersAction::SetPiLmStudio { scope, url } => agents.set_pi_lmstudio_url(&scope, url),
        ProvidersAction::SetPiDefaultModel { scope, model } => {
            agents.set_pi_default_model(&scope, model)
        }
        // App-wide, as Check again on the agents does.
        ProvidersAction::CheckPiLocal(ProviderScope::Global) => {
            agents.refresh();
            Ok(())
        }
        ProvidersAction::CheckPiLocal(scope) => {
            agents.check_pi_local(&scope);
            Ok(())
        }
        ProvidersAction::SetClaudeTogetherKey { scope, key } => {
            agents.set_claude_together_key(&scope, key)
        }
        ProvidersAction::CheckTogetherLink => {
            agents.check_togetherlink();
            Ok(())
        }
        ProvidersAction::InstallTogetherLink => {
            agents.install_togetherlink();
            Ok(())
        }
        ProvidersAction::DeclineTogetherLink => {
            agents.decline_togetherlink();
            Ok(())
        }
        ProvidersAction::SetOwn { project, tool, own } => {
            agents.set_own_providers(&project, tool, own)
        }
    }
}

/// For the New terminal chooser of `project`: the agents not offered, and
/// Pi's providers as that project's Pi terminals get them (its own when its
/// Pi config is Isolated and it chose them, else the app-wide ones).
pub(crate) fn for_new_terminal(
    agents: &dyn connecto_core::AgentsControl,
    project: Option<&str>,
) -> (Vec<ToolKind>, PiProvidersView) {
    match project {
        Some(project) => {
            let scope = agents.provider_scope(project, ToolKind::Pi);
            (agents.inactive_for(project), agents.pi_providers(&scope))
        }
        None => (agents.inactive(), agents.view().pi.clone()),
    }
}

/// Claude's providers on this system, for the window.
pub(crate) const CLAUDE_SUPPORTED: bool = CLAUDE_PROVIDERS_SUPPORTED;

/// What Claude reaches, in a line for its card. `None` when it is as usual.
pub(crate) fn claude_providers_line(
    claude: &ClaudeProvidersView,
) -> Option<(String, egui::Color32)> {
    if !claude.together_key {
        return None;
    }
    Some(if claude.blocked() {
        (
            "Model provider: Together AI, but TogetherLink is not installed".to_owned(),
            theme::AMBER,
        )
    } else {
        (
            "Model provider: Together AI, through TogetherLink".to_owned(),
            theme::TEXT_MUTED,
        )
    })
}

/// The dialog's content. `closed` is set when Done is clicked.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dialog<A: From<ProvidersAction>>(
    ui: &mut Ui,
    state: &mut ProvidersDialog,
    claude: &ClaudeProvidersView,
    pi: &PiProvidersView,
    out: &mut Out<A>,
    closed: &mut bool,
) {
    let project = state.scope.project().map(str::to_owned);
    let own = match state.tool {
        ToolKind::Pi => pi.own,
        _ => claude.own,
    };
    let done_key = if state.tool == ToolKind::Pi {
        "pi-setup-done"
    } else {
        "claude-setup-done"
    };
    let title = if state.tool == ToolKind::Pi {
        "PI MODEL PROVIDERS"
    } else {
        "CLAUDE MODEL PROVIDERS"
    };
    if project.is_some() {
        out.say(ui, heading(&format!("{title} FOR THIS PROJECT")));
    } else {
        out.say(ui, heading(title));
    }
    if let Some(project) = &project {
        let mut on = own;
        let label = "Use model providers set for this project only";
        let toggle = ui.checkbox(&mut on, label);
        out.mark("providers-own", toggle.rect);
        if toggle.changed() {
            state.error = None;
            out.actions.push(A::from(ProvidersAction::SetOwn {
                project: project.clone(),
                tool: state.tool,
                own: on,
            }));
        }
        if !own {
            out.say(
                ui,
                muted("Off: this project's terminals use the providers set in Settings > Agents. Turn it on to give this project providers and keys of its own."),
            );
        }
    }
    if project.is_none() || own {
        match state.tool {
            ToolKind::Pi => pi_body(ui, state, pi, out),
            _ => claude_body(ui, state, claude, out),
        }
    }
    if let Some(error) = &state.error {
        out.say(ui, problem(error.clone()));
    }
    ui.add_space(8.0);
    if out.button(ui, done_key, "Done", true) {
        *closed = true;
    }
}

fn input(
    ui: &mut Ui,
    id: &'static str,
    text: &mut String,
    hint: &str,
    password: bool,
) -> egui::Response {
    ui.add(
        TextEdit::singleline(text)
            .id(Id::unique(id))
            .hint_text(hint)
            .password(password)
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    )
}

/// One key: whether it is saved, a field to paste one, Save and Remove.
/// Returns `Some(Some(key))` to save, `Some(None)` to remove.
fn key_card<A>(
    ui: &mut Ui,
    out: &mut Out<A>,
    label: &str,
    saved: bool,
    typed: &mut String,
    keys_page: &str,
    marks: (&'static str, &'static str, &'static str),
) -> Option<Option<String>> {
    let (field, save, remove) = marks;
    let mut change = None;
    forms::card(ui, |ui| {
        out.say(ui, RichText::new(label).color(theme::TEXT));
        let (status, color) = if saved {
            ("Key saved", theme::GREEN)
        } else {
            ("No key", theme::TEXT_MUTED)
        };
        out.say(ui, RichText::new(status).size(11.5).color(color));
        let hint = if saved {
            "key saved: paste another to replace it".to_owned()
        } else {
            format!("API key, from {keys_page}")
        };
        let response = input(ui, field, typed, &hint, true);
        out.mark(field, response.rect);
        ui.horizontal(|ui| {
            if out.button(ui, save, "Save key", !typed.trim().is_empty()) {
                change = Some(Some(std::mem::take(typed)));
            }
            if saved && out.button(ui, remove, "Remove key", true) {
                typed.clear();
                change = Some(None);
            }
        });
    });
    change
}

/// Claude: a Together AI key, and TogetherLink, which a key needs.
fn claude_body<A: From<ProvidersAction>>(
    ui: &mut Ui,
    state: &mut ProvidersDialog,
    claude: &ClaudeProvidersView,
    out: &mut Out<A>,
) {
    out.say(
        ui,
        muted(format!(
            "Claude Code can use Together AI's models instead of your Anthropic account. With a Together AI key saved here, Claude terminals start through TogetherLink, Together's launcher for Claude Code, with everything else as usual. The key is kept in the {} and given to each terminal when it starts. A change applies to terminals started from now on.",
            crate::platform::KEY_STORE
        )),
    );
    if claude.keys_unreadable {
        out.say(
            ui,
            problem(format!(
                "The saved key could not be read from the {}.",
                crate::platform::KEY_STORE
            )),
        );
    }
    if let Some(key) = key_card(
        ui,
        out,
        "Together AI",
        claude.together_key,
        &mut state.together_key,
        TOGETHER_KEYS_PAGE,
        (
            "claude-together-key",
            "claude-together-save",
            "claude-together-remove",
        ),
    ) {
        state.error = None;
        out.actions
            .push(A::from(ProvidersAction::SetClaudeTogetherKey {
                scope: state.scope.clone(),
                key,
            }));
    }
    forms::card(ui, |ui| {
        out.say(ui, RichText::new("TogetherLink").color(theme::TEXT));
        match &claude.togetherlink {
            TogetherLinkState::Checking => out.say(ui, muted("Looking for TogetherLink…")),
            TogetherLinkState::Installing(step) => out.say(ui, muted(step.clone())),
            TogetherLinkState::Installed => {
                out.say(
                    ui,
                    RichText::new("Installed.").size(11.5).color(theme::GREEN),
                );
                if claude.togetherlink_own_key {
                    out.say(
                        ui,
                        warning("TogetherLink has a key of its own, saved with togetherlink configure, and uses it instead of the one here. Remove that one to use this one."),
                    );
                }
            }
            TogetherLinkState::Missing => {
                if claude.together_key {
                    out.say(
                        ui,
                        problem("Not installed, so Claude terminals cannot start with this key. Install it, or remove the key."),
                    );
                } else {
                    out.say(
                        ui,
                        muted("Not installed. It is needed once a key is saved."),
                    );
                }
                let offer = "Connecto runs Together's own installer, from link.together.ai. It puts TogetherLink in ~/.local/bin, installs Bun (which it runs on) if it is missing, and adds ~/.local/bin to your shell's startup file.";
                if claude.install_offered {
                    out.say(ui, warning(format!("Install TogetherLink now? {offer}")));
                } else {
                    out.say(ui, muted(offer));
                }
                ui.horizontal_wrapped(|ui| {
                    if out.button(ui, "togetherlink-install", "Install TogetherLink", true) {
                        state.error = None;
                        out.actions
                            .push(A::from(ProvidersAction::InstallTogetherLink));
                    }
                    if claude.install_offered
                        && out.button(ui, "togetherlink-decline", "Not now", true)
                    {
                        out.actions
                            .push(A::from(ProvidersAction::DeclineTogetherLink));
                    }
                    if out.button(ui, "togetherlink-check", "Check again", true) {
                        out.actions
                            .push(A::from(ProvidersAction::CheckTogetherLink));
                    }
                });
                out.say(
                    ui,
                    muted(format!(
                        "Or install it yourself in a terminal: {TOGETHERLINK_INSTALL} (see {TOGETHERLINK_DOCS})."
                    )),
                );
            }
        }
        if let Some(why) = &claude.install_error {
            out.say(
                ui,
                problem(format!("TogetherLink was not installed: {why}")),
            );
        }
    });
}

/// What Pi can reach, in a line for its card.
pub(crate) fn pi_providers_line(pi: &PiProvidersView) -> String {
    let mut names: Vec<String> = pi.keys_set.iter().map(|p| p.label().to_owned()).collect();
    match pi.lmstudio {
        PiLocalState::Working { models } if models > 0 => {
            names.push(format!("LM Studio ({models} local)"));
        }
        PiLocalState::Checking => names.push("LM Studio (checking…)".to_owned()),
        _ => {}
    }
    if names.is_empty() {
        "No model provider that works yet".to_owned()
    } else {
        format!("Providers: {}", names.join(", "))
    }
}

/// What the check of the local server found, for under its address.
fn lmstudio_status(pi: &PiProvidersView) -> RichText {
    match pi.lmstudio {
        PiLocalState::Off => muted("Not used."),
        PiLocalState::Checking => muted("Checking the server…"),
        PiLocalState::Working { models: 0 } => warning(format!(
            "The server at {} answers, but it has no chat model. Download or load one in LM Studio, then Check again.",
            pi.lmstudio_url
        )),
        PiLocalState::Working { models } => RichText::new(format!(
            "Working: the server at {} answers with {models} chat model{}.",
            pi.lmstudio_url,
            if models == 1 { "" } else { "s" }
        ))
        .size(11.5)
        .color(theme::GREEN),
        PiLocalState::NotAnswering => problem(format!(
            "Nothing answers at {}. Start the server in LM Studio (Developer) or with `lms server start`, check the port, then Check again.",
            pi.lmstudio_url
        )),
    }
}

/// How many models the dialog lists at once; the field narrows them.
const MODELS_SHOWN: usize = 8;

/// The model a new Pi terminal starts on: found by typing, from every model
/// Pi offers with the providers as they are.
fn pi_default_model<A: From<ProvidersAction>>(
    ui: &mut Ui,
    state: &mut ProvidersDialog,
    pi: &PiProvidersView,
    out: &mut Out<A>,
) {
    let scope = state.scope.clone();
    forms::card(ui, |ui| {
        out.say(
            ui,
            RichText::new("Model for new terminals").color(theme::TEXT),
        );
        let current = match &pi.default_model {
            Some(model) if pi.models.is_empty() || pi.models.contains(model) => {
                RichText::new(model.clone()).size(11.5).color(theme::GREEN)
            }
            Some(model) => warning(format!(
                "{model} is not offered by Pi now, so new terminals are left to Pi. Choose another, or bring it back."
            )),
            None => muted("Pi chooses. Pick one here to start every new Pi terminal on it."),
        };
        out.say(ui, current);
        if pi.models.is_empty() {
            out.say(
                ui,
                muted(if pi.models_loading {
                    "Asking Pi for its models…"
                } else {
                    "Pi lists no model yet: set a provider above."
                }),
            );
        } else {
            let response = input(
                ui,
                "pi-model-filter",
                &mut state.pi_model_filter,
                "type to find a model (provider or name)",
                false,
            );
            out.mark("pi-model-filter", response.rect);
            let found = connecto_core::pi::matching_models(
                &pi.models,
                &state.pi_model_filter,
                MODELS_SHOWN,
            );
            for model in &found {
                let on = pi.default_model.as_deref() == Some(*model);
                if out.choice(ui, &format!("pi-model:{model}"), on, model) && !on {
                    state.error = None;
                    out.actions
                        .push(A::from(ProvidersAction::SetPiDefaultModel {
                            scope: scope.clone(),
                            model: Some((*model).to_owned()),
                        }));
                }
            }
            let total = pi.models.len();
            out.say(
                ui,
                muted(if found.is_empty() {
                    format!("None of Pi's {total} models has that in its name.")
                } else if found.len() < total {
                    format!(
                        "{} of {total} models shown. Type to find another.",
                        found.len()
                    )
                } else {
                    format!("{total} models.")
                }),
            );
        }
        if pi.default_model.is_some() && out.button(ui, "pi-model-clear", "Let Pi choose", true) {
            state.error = None;
            out.actions
                .push(A::from(ProvidersAction::SetPiDefaultModel {
                    scope: scope.clone(),
                    model: None,
                }));
        }
    });
}

/// Pi: a key for each hosted provider, and the address of a local LM Studio
/// server.
fn pi_body<A: From<ProvidersAction>>(
    ui: &mut Ui,
    state: &mut ProvidersDialog,
    pi: &PiProvidersView,
    out: &mut Out<A>,
) {
    out.say(
        ui,
        muted(format!(
            "Pi uses the models of the providers set here. A key is kept in {} and given to each Pi terminal when it starts. Pick the model inside Pi with /model. A change applies to terminals started from now on.",
            crate::platform::KEY_STORE
        )),
    );
    if pi.keys_unreadable {
        out.say(
            ui,
            problem(format!(
                "The saved keys could not be read from the {}.",
                crate::platform::KEY_STORE
            )),
        );
    }
    const KEY_FIELDS: [(&str, &str, &str); 2] = [
        (
            "pi-key-openrouter",
            "pi-save-openrouter",
            "pi-remove-openrouter",
        ),
        ("pi-key-together", "pi-save-together", "pi-remove-together"),
    ];
    let scope = state.scope.clone();
    for ((provider, typed), marks) in PiProvider::ALL
        .into_iter()
        .zip(&mut state.pi_keys)
        .zip(KEY_FIELDS)
    {
        if let Some(key) = key_card(
            ui,
            out,
            provider.label(),
            pi.has_key(provider),
            typed,
            provider.keys_page(),
            marks,
        ) {
            state.error = None;
            out.actions.push(A::from(ProvidersAction::SetPiKey {
                scope: scope.clone(),
                provider,
                key,
            }));
        }
    }
    forms::card(ui, |ui| {
        out.say(
            ui,
            RichText::new("LM Studio (local models)").color(theme::TEXT),
        );
        out.say(
            ui,
            muted("The models of LM Studio's local server are offered in Pi under lmstudio/. Start the server in LM Studio (Developer) or with `lms server start`, and set the address it listens on. Leave it empty to use no local server."),
        );
        let url = state
            .pi_lmstudio
            .get_or_insert_with(|| pi.lmstudio_url.clone());
        let response = input(
            ui,
            "pi-lmstudio-url",
            url,
            connecto_core::pi::DEFAULT_LMSTUDIO_URL,
            false,
        );
        out.mark("pi-lmstudio-url", response.rect);
        let changed = url.trim() != pi.lmstudio_url;
        let mut save = false;
        let mut check = false;
        ui.horizontal(|ui| {
            save = out.button(ui, "pi-lmstudio-save", "Save and check", changed);
            check = out.button(
                ui,
                "pi-lmstudio-check",
                "Check again",
                !changed && pi.lmstudio != PiLocalState::Checking,
            );
        });
        if save {
            state.error = None;
            out.actions.push(A::from(ProvidersAction::SetPiLmStudio {
                scope: scope.clone(),
                url: url.clone(),
            }));
            // Shown again as it was saved (cleaned, or where the server was found).
            state.pi_lmstudio = None;
        } else if check {
            out.actions
                .push(A::from(ProvidersAction::CheckPiLocal(scope.clone())));
        } else if !changed {
            // The address the check settled on, when it found the server elsewhere.
            state.pi_lmstudio = None;
        }
        out.say(ui, lmstudio_status(pi));
    });
    pi_default_model(ui, state, pi, out);
    if !pi.usable() && !pi.undecided() {
        out.say(
            ui,
            warning("Pi is not offered for new terminals until one of these works: a saved key, or a local server that answers with a chat model."),
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use connecto_core::ConfigMode;

    #[test]
    fn claudes_providers_are_hidden_where_togetherlink_does_not_run() {
        // Windows: never offered, app-wide or for a project.
        assert!(!claude_setup_offered(false, true));
        assert!(!project_setup_offered(
            false,
            ToolKind::Claude,
            ConfigMode::Isolated
        ));
        // macOS and Linux: once Claude is installed; a project when Isolated.
        assert!(claude_setup_offered(true, true));
        assert!(!claude_setup_offered(true, false));
        assert!(project_setup_offered(
            true,
            ToolKind::Claude,
            ConfigMode::Isolated
        ));
        assert!(!project_setup_offered(
            true,
            ToolKind::Claude,
            ConfigMode::System
        ));
        // Pi's run everywhere, and Codex has none here.
        assert!(project_setup_offered(
            false,
            ToolKind::Pi,
            ConfigMode::Isolated
        ));
        assert!(!project_setup_offered(
            true,
            ToolKind::Pi,
            ConfigMode::System
        ));
        assert!(!project_setup_offered(
            true,
            ToolKind::Codex,
            ConfigMode::Isolated
        ));
        assert_eq!(CLAUDE_SUPPORTED, !cfg!(windows));
    }

    /// Agents where a project with Pi providers of its own (Isolated) has a
    /// key and nothing app-wide works.
    struct ScopedAgents;

    impl connecto_core::AgentsControl for ScopedAgents {
        fn view(&self) -> std::sync::Arc<connecto_core::AgentsView> {
            let mut view = connecto_core::AgentsView::default();
            view.pi.models = vec!["openrouter/app/wide".to_owned()];
            std::sync::Arc::new(view)
        }
        fn inactive(&self) -> Vec<ToolKind> {
            vec![ToolKind::Pi]
        }
        fn inactive_for(&self, project: &str) -> Vec<ToolKind> {
            if project == "own" {
                Vec::new()
            } else {
                vec![ToolKind::Pi]
            }
        }
        fn provider_scope(&self, project: &str, _tool: ToolKind) -> ProviderScope {
            if project == "own" {
                ProviderScope::Project(project.to_owned())
            } else {
                ProviderScope::Global
            }
        }
        fn pi_providers(&self, scope: &ProviderScope) -> PiProvidersView {
            match scope {
                ProviderScope::Global => self.view().pi.clone(),
                ProviderScope::Project(_) => PiProvidersView {
                    keys_set: vec![PiProvider::Together],
                    models: vec!["together/project/only".to_owned()],
                    default_model: Some("together/project/only".to_owned()),
                    ..PiProvidersView::default()
                },
            }
        }
    }

    #[test]
    fn the_chooser_offers_pi_and_its_models_as_the_projects_terminals_get_them() {
        let (off, pi) = for_new_terminal(&ScopedAgents, Some("own"));
        assert!(!off.contains(&ToolKind::Pi));
        assert_eq!(pi.models, ["together/project/only"]);
        assert_eq!(pi.default_model.as_deref(), Some("together/project/only"));
        let (off, pi) = for_new_terminal(&ScopedAgents, Some("other"));
        assert!(off.contains(&ToolKind::Pi));
        assert_eq!(pi.models, ["openrouter/app/wide"]);
        let (off, _) = for_new_terminal(&ScopedAgents, None);
        assert!(off.contains(&ToolKind::Pi));
    }

    #[test]
    fn claudes_card_says_when_togetherlink_is_missing() {
        let mut view = ClaudeProvidersView::default();
        assert_eq!(claude_providers_line(&view), None);
        view.together_key = true;
        view.togetherlink = TogetherLinkState::Missing;
        assert!(
            claude_providers_line(&view)
                .unwrap()
                .0
                .contains("not installed")
        );
        view.togetherlink = TogetherLinkState::Installed;
        assert_eq!(
            claude_providers_line(&view).unwrap().0,
            "Model provider: Together AI, through TogetherLink"
        );
    }
}
