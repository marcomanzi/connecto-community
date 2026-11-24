use crate::ToolKind;

/// One published work type, as the menu lists it.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct WorkType {
    pub name: String,
    /// The type has a `setup` command.
    pub setup: bool,
    /// When `setup` last succeeded (local time, RFC 3339). None when it never
    /// ran or its last run failed.
    pub setup_ok_at: Option<String>,
    /// The linked databases whose jobs of this type are run; None for every
    /// one, including those linked later.
    pub databases: Option<Vec<String>>,
}

/// The name a project's database gets when none is given, and the one a
/// link from before named databases became.
pub const DEFAULT_DATABASE: &str = "default";

/// The longest database name.
pub const MAX_DATABASE_NAME: usize = 32;

/// The word that means every linked database in a list (`--db all`): never
/// a database's name.
pub const ALL_DATABASES: &str = "all";

/// A linked database's name, such as `dev`, `staging` or `production`:
/// letters, digits, `-` and `_`, at most [`MAX_DATABASE_NAME`] long, and
/// never [`ALL_DATABASES`].
///
/// # Errors
/// What is wrong with it, for the person.
pub fn database_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_DATABASE_NAME
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "A database name is up to {MAX_DATABASE_NAME} letters, digits, - and _, such as dev, staging or production, not {name:?}."
        ));
    }
    if name.eq_ignore_ascii_case(ALL_DATABASES) {
        return Err(format!(
            "{name:?} means every database: give the database another name."
        ));
    }
    Ok(name.to_owned())
}

/// How a linked database answered the last time Connecto looked for jobs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DbStatus {
    /// None until Connecto has looked; then whether it answered.
    pub ok: Option<bool>,
    /// What happened: the job taken, or why the database did not answer.
    pub message: String,
    /// When (local time, RFC 3339).
    pub at: String,
}

/// One database a project takes jobs from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedDatabase {
    pub name: String,
    pub database: Database,
    pub status: DbStatus,
}

/// Which addresses of a Cloud SQL instance Connecto connects to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IpType {
    /// The instance's public IP (`PRIMARY`).
    #[default]
    Public,
    /// Its private IP: only from inside that VPC (a VPN).
    Private,
    /// Private Service Connect: its DNS name, from inside that VPC.
    Psc,
}

impl IpType {
    /// The word stored in a link.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Public => "PUBLIC",
            Self::Private => "PRIVATE",
            Self::Psc => "PSC",
        }
    }
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "PUBLIC" => Some(Self::Public),
            "PRIVATE" => Some(Self::Private),
            "PSC" => Some(Self::Psc),
            _ => None,
        }
    }
}

/// The Google identity that authorises a Cloud SQL connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoogleCredential {
    /// A service account key kept in the vault under this project secret.
    Key(String),
    /// The person's own `gcloud auth application-default login`.
    Adc,
}

impl Default for GoogleCredential {
    fn default() -> Self {
        Self::Key(String::new())
    }
}

/// A Google Cloud SQL for PostgreSQL database. Secrets are only named here:
/// their values stay in the vault.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudSql {
    /// `project:region:instance`.
    pub connection_name: String,
    pub database: String,
    /// The database user. With `iam_auth` and left empty, the identity's
    /// IAM database user.
    pub user: String,
    pub ip_type: IpType,
    /// Sign in to the database with an OAuth token instead of a password.
    pub iam_auth: bool,
    pub credential: GoogleCredential,
    /// The project secret holding the database password; empty with
    /// `iam_auth`.
    pub password_secret: String,
}

/// Where a project's queue lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Database {
    /// A PostgreSQL connection string kept in the vault under this name.
    ConnectionString(String),
    CloudSql(CloudSql),
}

impl Database {
    /// How the sheet names it ("Linked through …").
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::ConnectionString(name) => name.clone(),
            Self::CloudSql(sql) => format!("Google Cloud SQL {}", sql.connection_name),
        }
    }
}

/// A parsed instance connection name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceName {
    /// The Google project, with its domain for a domain-scoped project
    /// (`example.com:project`).
    pub project: String,
    pub region: String,
    pub instance: String,
}

impl InstanceName {
    /// `project:region:instance`, or `domain:project:region:instance` for a
    /// domain-scoped project.
    ///
    /// # Errors
    /// What is wrong with it, for the person.
    pub fn parse(name: &str) -> Result<Self, String> {
        let bad = || {
            Err(format!(
                "Enter the instance connection name as project:region:instance (shown on the instance's Overview page), not {name:?}."
            ))
        };
        let name = name.trim();
        let parts: Vec<&str> = name.split(':').collect();
        let (project, region, instance) = match parts.as_slice() {
            [p, r, i] => ((*p).to_owned(), *r, *i),
            [d, p, r, i] if d.contains('.') => (format!("{d}:{p}"), *r, *i),
            _ => return bad(),
        };
        let plain = |s: &str, extra: &[char]| {
            !s.is_empty()
                && s.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || extra.contains(&c)
                })
        };
        let project_ok = project.rsplit(':').next().is_some_and(|p| plain(p, &[]))
            && project.split(':').next().is_some_and(|d| plain(d, &['.']));
        if !project_ok || !plain(region, &[]) || !plain(instance, &[]) {
            return bad();
        }
        Ok(Self {
            project,
            region: region.to_owned(),
            instance: instance.to_owned(),
        })
    }

    /// The common name of the instance's server certificate.
    #[must_use]
    pub fn server_cn(&self) -> String {
        format!("{}:{}", self.project, self.instance)
    }
}

/// A service account key imported into the vault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedKey {
    /// The project secret it is kept under.
    pub secret: String,
    /// The service account's email.
    pub email: String,
}

/// The most jobs one project may run at once.
pub const MAX_PARALLEL: u32 = 32;

/// The agent a project's jobs run on when their work type names none
/// (session CLI blank, `agy` or `antigravity`): "Agent for jobs" (ADR 0041).
/// A job that names another agent keeps it, with its own model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum JobAgent {
    /// Antigravity, else the first usable stand-in (ADR 0041).
    #[default]
    Automatic,
    /// Always this CLI, with this model (`None`: the CLI's default model).
    /// When it cannot run a job, the job fails saying why: no stand-in.
    Cli {
        tool: ToolKind,
        model: Option<String>,
    },
}

impl JobAgent {
    /// "Automatic", "Claude, model sonnet", "Pi, its default model".
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Automatic => "Automatic (Antigravity, else the first usable stand-in)".into(),
            Self::Cli {
                tool,
                model: Some(model),
            } => format!("{}, model {model}", tool.label()),
            Self::Cli { tool, model: None } => format!("{}, its default model", tool.label()),
        }
    }
}

/// A CLI a project's jobs may be put on, and the models offered for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobAgentChoice {
    pub tool: ToolKind,
    /// Newest first; the CLI's default model is always offered besides.
    pub models: Vec<String>,
}

/// A job running now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningJob {
    /// The linked database it came from.
    pub database: String,
    /// Its id in that database's queue.
    pub id: i64,
    pub kind: String,
    /// When it was taken (local time, RFC 3339).
    pub since: String,
    /// The agent chosen for it and its model ("Codex, model gpt-6.1-terra,
    /// standing in for Antigravity"); empty until it is chosen.
    pub agent: String,
    /// How far that agent has come.
    pub agent_stage: AgentStage,
}

/// How far a running job's agent has come (ADR 0041).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentStage {
    /// Chosen; its terminal is not made yet (prepare or setup may run).
    #[default]
    Selected,
    /// Its terminal is made and starting: the program's checks, its login.
    Dispatched,
    /// The agent's program runs.
    Running,
}

/// A running job's question to the user (`connecto works ask`), waiting
/// for an answer. ADR 0040.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobQuestion {
    /// Connecto's id for the question, unique while it runs.
    pub id: u64,
    /// The project's id, and its name as the window shows it.
    pub project: String,
    pub project_name: String,
    /// The linked database the job came from, its id there and its type.
    pub database: String,
    pub job: i64,
    pub kind: String,
    /// The job's terminal.
    pub terminal: String,
    /// The agent's own words.
    pub text: String,
    /// When it was asked, and by when an answer must come (local time,
    /// RFC 3339): after that the job is told no answer came.
    pub asked_at: String,
    pub answer_by: String,
}

#[derive(Clone, Default)]
pub struct WorksView {
    /// The linked databases, by name.
    pub databases: Vec<LinkedDatabase>,
    /// How many jobs the project runs at once (1 to [`MAX_PARALLEL`]); the
    /// linked databases share them.
    pub parallel: u32,
    /// The agent its jobs that name none run on.
    pub job_agent: JobAgent,
    /// The CLIs "Agent for jobs" offers: those the project's Tools allow,
    /// each with its models.
    pub job_agent_choices: Vec<JobAgentChoice>,
    /// The jobs running now, oldest first.
    pub running: Vec<RunningJob>,
    /// This project's vault secrets that hold a Google service account key.
    pub google_keys: Vec<String>,
    pub types: Vec<WorkType>,
    pub notice: String,
    /// The project's jobs' questions waiting for an answer, oldest first.
    pub questions: Vec<JobQuestion>,
}

impl WorksView {
    /// Whether any database is linked.
    #[must_use]
    pub fn linked(&self) -> bool {
        !self.databases.is_empty()
    }

    /// The linked database named `name`.
    #[must_use]
    pub fn database(&self, name: &str) -> Option<&LinkedDatabase> {
        self.databases.iter().find(|d| d.name == name)
    }
}

/// An author's reply, each field ready for review. `setup` and `prepare` are
/// empty when the author proposed none.
#[derive(Clone, Default, Debug)]
pub struct WorksDraft {
    pub instructions: String,
    pub schema: String,
    pub setup: String,
    pub prepare: String,
    /// The databases the author asked the type for; None for every one.
    pub databases: Option<Vec<String>>,
}

pub enum WorksAction {
    /// Link `database` under `name`, or change the link of that name (its
    /// queue must stay the same). `password` is a Cloud SQL database
    /// password just typed (None keeps the saved one). Connecto keeps it in
    /// the vault once linking works.
    Link {
        name: String,
        database: Database,
        create: bool,
        password: Option<String>,
    },
    /// Connect and report each step, without changing anything: a typed
    /// `password` is used, never kept.
    Test {
        database: Database,
        password: Option<String>,
    },
    /// Stop taking jobs from the database `name` and forget its link. With
    /// an outcome still to be written back to it, only when `force`.
    Unlink {
        name: String,
        force: bool,
    },
    Author {
        kind: String,
        cli: ToolKind,
    },
    /// `setup` and `prepare` are reviewed JSON commands
    /// (`{"argv": [...], "timeout_s": n}`) Connecto runs in the project root.
    /// `databases` names the linked databases whose jobs of this type run;
    /// None for every one.
    Publish {
        kind: String,
        instructions: String,
        schema: String,
        setup: Option<String>,
        prepare: Option<String>,
        databases: Option<Vec<String>>,
    },
    /// Run a published type's `setup` command again.
    Setup {
        kind: String,
    },
    /// Run up to `jobs` jobs at once (1 to [`MAX_PARALLEL`]), shared by the
    /// linked databases. Running jobs are not stopped when it is lowered.
    Parallel {
        jobs: u32,
    },
    /// Run the jobs that name no agent on `agent` from the next claim on.
    /// Running jobs keep their agent.
    JobAgent {
        agent: JobAgent,
    },
}

pub trait WorksControl: Send + Sync {
    fn view(&self, project: &str) -> WorksView;
    fn submit(&self, project: String, action: WorksAction);
    /// Read the exact final reply of this work type's authoring terminal.
    fn draft(&self, project: &str, kind: &str) -> Result<WorksDraft, String>;
    /// Read a service account key file, check it and keep it in the vault
    /// for this project. The file itself is left where it is.
    fn import_key(&self, project: &str, path: &std::path::Path) -> Result<ImportedKey, String>;
    /// Every project's jobs' questions waiting for an answer, oldest first.
    fn questions(&self) -> Vec<JobQuestion> {
        Vec::new()
    }
    /// Answer the question `id` with `answer`, or (None) say no answer
    /// will come: the job is told, and ends with its JSON.
    ///
    /// # Errors
    /// When the question is no longer waiting (its job ended, or its time
    /// to answer is over).
    fn answer(&self, id: u64, answer: Option<String>) -> Result<(), String> {
        let _ = (id, answer);
        Err("Agentic Works is not running in this Connecto.".to_owned())
    }
}

/// Who runs `connecto works`: the terminal the bridge found from the process
/// tree, its project, and how its agent was launched. Never taken from an
/// argument or the environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorksCaller {
    pub terminal: String,
    /// The relay's project id.
    pub project_id: String,
    /// The permission mode the caller's agent was started with, from the
    /// launch itself; None for a shell or a terminal without an agent.
    pub launched: Option<crate::AgentMode>,
    /// The terminal runs an Agentic Works job, or was started by one (at
    /// any depth): recorded by Connecto when the terminal was created, and
    /// kept through closes, restarts and new parents.
    pub job: bool,
    /// The terminal's launch, as the bridge found the caller in it: a
    /// restart in place keeps the terminal's id but is another attempt. A
    /// job's question is bound to the attempt that asked it.
    pub attempt: u64,
}

/// `connecto works` on the bridge: an agent sets up its own project's
/// Agentic Works. Answers are text for the agent, never a secret's value.
pub trait WorksBridge: Send + Sync {
    /// One `connecto works <verb> [args]`. `text` is what the command read
    /// for it: a definition, a prompt or a key file. `notify` tells the
    /// terminal later when the user answers a request Connecto made for it.
    ///
    /// # Errors
    /// Why it was refused, for the agent.
    fn command(
        &self,
        caller: &WorksCaller,
        verb: &str,
        args: &[String],
        text: Option<&str>,
        notify: crate::AskReply,
    ) -> Result<String, String>;
}

/// The bridge used when Agentic Works is not running.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoWorksBridge;

impl WorksBridge for NoWorksBridge {
    fn command(
        &self,
        _caller: &WorksCaller,
        _verb: &str,
        _args: &[String],
        _text: Option<&str>,
        _notify: crate::AskReply,
    ) -> Result<String, String> {
        Err("Agentic Works is not running in this Connecto.".to_owned())
    }
}

/// One step of the Cloud SQL setup: what to do, the commands, and a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStep {
    pub title: String,
    pub code: Option<String>,
    pub after: String,
}

/// The service account Connecto's setup creates.
pub const SERVICE_ACCOUNT: &str = "connecto-agentic-works";

/// The setup steps for a Cloud SQL link, `<PROJECT>`, `<INSTANCE>`, `<DB>`
/// and `<USER>` standing for what is not known yet. `import` says how the
/// key file is imported (the sheet's button, or the command).
#[must_use]
pub fn cloudsql_setup_steps(sql: &CloudSql, import: &str) -> Vec<SetupStep> {
    let (project, instance) = InstanceName::parse(&sql.connection_name)
        .map_or(("<PROJECT>".to_owned(), "<INSTANCE>".to_owned()), |n| {
            (n.project, n.instance)
        });
    // Each name as shown in prose and as SQL; a placeholder stays bare.
    let (db, qdb) = match sql.database.trim() {
        "" => ("<DB>".to_owned(), "<DB>".to_owned()),
        db => (db.to_owned(), ident(db)),
    };
    let sa = format!("{SERVICE_ACCOUNT}@{project}.iam.gserviceaccount.com");
    let quser = match sql.user.trim() {
        "" if sql.iam_auth => ident(&format!("{SERVICE_ACCOUNT}@{project}.iam")),
        "" => "<USER>".to_owned(),
        user => ident(user),
    };
    let step = |title: &str, code: Option<String>, after: &str| SetupStep {
        title: title.to_owned(),
        code,
        after: after.to_owned(),
    };
    if sql.credential == GoogleCredential::Adc {
        return vec![step(
            "Sign in with gcloud on this Mac:",
            Some("gcloud auth application-default login".to_owned()),
            &format!(
                "Your own Google account needs roles/cloudsql.client on {project}. Connecto uses that login while it is valid. A service account key keeps working without it."
            ),
        )];
    }
    let role = if sql.iam_auth {
        "roles/cloudsql.instanceUser"
    } else {
        "roles/cloudsql.client"
    };
    let mut first = format!(
        "gcloud iam service-accounts create {SERVICE_ACCOUNT} \\\n  --project={project} --display-name=\"Connecto Agentic Works\"\ngcloud projects add-iam-policy-binding {project} \\\n  --member=\"serviceAccount:{sa}\" \\\n  --role=\"roles/cloudsql.client\""
    );
    if sql.iam_auth {
        first.push_str(&format!(
            "\ngcloud projects add-iam-policy-binding {project} \\\n  --member=\"serviceAccount:{sa}\" \\\n  --role=\"{role}\"\ngcloud sql instances patch {instance} --project={project} \\\n  --database-flags=cloudsql.iam_authentication=on   # keep the instance's other flags!\ngcloud sql users create {SERVICE_ACCOUNT}@{project}.iam \\\n  --instance={instance} --project={project} --type=cloud_iam_service_account"
        ));
    }
    let mut steps = vec![step(
        "1. Create a service account for Connecto (once per Google project):",
        Some(first),
        &if sql.iam_auth {
            format!(
                "--database-flags replaces the instance's flags: list the existing ones too. See them with: gcloud sql instances describe {instance} --project={project} --format='value(settings.databaseFlags)'"
            )
        } else {
            String::new()
        },
    )];
    steps.push(step(
        "2. Create its key and import it:",
        Some(format!(
            "gcloud iam service-accounts keys create ~/Downloads/connecto-{project}.json \\\n  --iam-account={sa}"
        )),
        import,
    ));
    let sql_text = if sql.iam_auth {
        format!(
            "GRANT CREATE ON DATABASE {qdb} TO {quser};\n-- Let your application write jobs without sharing its own credentials:\nGRANT {quser} TO <your application's database role>;"
        )
    } else {
        format!(
            "CREATE ROLE {quser} LOGIN PASSWORD '<a long password>';\nGRANT CREATE ON DATABASE {qdb} TO {quser};\n-- Let your application write jobs without sharing its own credentials:\nGRANT {quser} TO <your application's database role>;"
        )
    };
    steps.push(step(
        &format!("3. Create the database role that owns the queue. Run this once, connected as an administrator of {db} (Cloud SQL Studio in the console works):"),
        Some(sql_text),
        &format!(
            "{}Connecto accepts a queue only when the role it connects with owns it and no other role has write grants on it. Role membership is not a grant, so the application can still write jobs through it.",
            if sql.iam_auth {
                format!("The database user is {SERVICE_ACCOUNT}@{project}.iam. ")
            } else {
                String::new()
            }
        ),
    ));
    steps.push(step(
        "4. Make sure the Cloud SQL Admin API is enabled (it usually already is):",
        Some(format!(
            "gcloud services enable sqladmin.googleapis.com --project={project}"
        )),
        "You do not need to add this computer's IP to Authorized networks: access is granted by the service account.",
    ));
    steps
}

/// A PostgreSQL identifier as SQL, always double-quoted (so reserved words
/// and any case are safe).
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn instance_connection_names() {
        let n = InstanceName::parse("my-project:europe-west2:my-instance").unwrap();
        assert_eq!(
            (n.project.as_str(), n.region.as_str(), n.instance.as_str()),
            ("my-project", "europe-west2", "my-instance")
        );
        assert_eq!(n.server_cn(), "my-project:my-instance");
        let d = InstanceName::parse(" example.com:proj:us-central1:db ").unwrap();
        assert_eq!(d.project, "example.com:proj");
        assert_eq!(d.server_cn(), "example.com:proj:db");
        for bad in [
            "",
            "proj",
            "proj:inst",
            "proj::inst",
            "a:b:c:d",
            "Proj:region:inst",
            "proj:region:inst/x",
            "proj:region:inst:extra",
            "proj:region:in st",
        ] {
            assert!(InstanceName::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn database_names() {
        for good in [
            "default",
            "dev",
            "staging",
            "production",
            "eu-west_2",
            " dev ",
        ] {
            assert_eq!(database_name(good).unwrap(), good.trim());
        }
        for bad in [
            "",
            " ",
            "my db",
            "prod/eu",
            "é",
            "a,b",
            "all",
            "ALL",
            &"x".repeat(33),
        ] {
            assert!(database_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ip_type_words_round_trip() {
        for t in [IpType::Public, IpType::Private, IpType::Psc] {
            assert_eq!(IpType::from_word(t.word()), Some(t));
        }
        assert_eq!(IpType::from_word("public"), None);
    }
}
