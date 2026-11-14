//! The instructions file written into each package's folder, which teaches the
//! authoring agent what to produce. Managed in a marked block, like the
//! automations guide, so a user's own text in the file is kept.

/// Bump whenever the text changes: a resumed session is told to re-read it.
pub const VERSION: u32 = 3;

/// The record of how a package was made, kept by the authoring agent at the
/// package root: never installed, and not part of the content hash.
pub const ORIGIN: &str = "ORIGIN.md";

const BEGIN: &str = "<!-- connecto-packages:begin v";
const END: &str = "<!-- connecto-packages:end -->";

fn wrap(body: &str) -> String {
    format!("{BEGIN}{VERSION} -->\n{}\n{END}\n", body.trim_end())
}

/// What the file should hold so that it carries the current guide, or `None`
/// when it already does: no file gets the block; a marked block is replaced
/// (only if it differs); any other text is the user's and is kept, with the
/// block appended.
#[must_use]
pub fn apply(existing: Option<&str>, body: &str) -> Option<String> {
    let block = wrap(body);
    let Some(text) = existing else {
        return Some(block);
    };
    if let (Some(begin), Some(end_at)) = (text.find(BEGIN), text.find(END))
        && begin < end_at
    {
        let mut end = end_at + END.len();
        if text[end..].starts_with('\n') {
            end += 1;
        }
        return (text[begin..end] != block)
            .then(|| format!("{}{}{}", &text[..begin], block, &text[end..]));
    }
    if text.trim().is_empty() {
        return Some(block);
    }
    Some(format!("{}\n\n{}", text.trim_end(), block))
}

#[must_use]
pub const fn file_name(claude: bool) -> &'static str {
    if claude { "CLAUDE.md" } else { "AGENTS.md" }
}

/// The guide for one package.
#[must_use]
pub fn text(name: &str, id: &str) -> String {
    TEMPLATE.replace("__NAME__", name).replace("__ID__", id)
}

const TEMPLATE: &str = r##"# Connecto package: __NAME__

You are helping the user build ONE package for Connecto, a terminal app that
runs Claude Code and Codex. A package is a set of skills, subagents, commands,
hooks and plugin settings that Connecto copies into the user's projects, so every
project has the same tooling. This folder is the package; you are running in
Connecto Home, the folder above `packages/`.

## Layout

```
package.json          name, description (and version, extends)
ORIGIN.md             how the package was made (see below)
.claude/              for Claude Code
  skills/<name>/SKILL.md      (front matter: name, description)
  agents/<name>.md            (front matter: name, description)
  commands/<name>.md
  hooks/...                   scripts a hook may call
  settings.json               ONLY what the package must set: enabledPlugins,
                              extraKnownMarketplaces, statusLine, hooks ...
.codex/               for Codex
  agents/<name>.toml
.agents/              for Codex
  skills/<name>/SKILL.md
```

Everything under `.claude/`, `.codex/` and `.agents/` is copied into the same
place in each project. Nothing else in this folder is installed.

## package.json

```json
{ "name": "Team tools", "description": "What it gives a project, in one line", "version": 1 }
```

- `name` and `description` are yours to write. Keep the description short.
- NEVER edit `version`. Connecto raises it by one by itself whenever the files
  change, and then updates every project that uses the package.
- `extends` (optional) names another package id (a folder in `packages/`, or
  `connecto` for the built-in package). Installing this package installs that
  one first, then this one's files on top. List only what differs.

## ORIGIN.md

`ORIGIN.md` is the package's record of how it was made, in plain Markdown, so
whoever changes it later (you, in another session) knows where it came from.
It is never installed, and editing it alone does not change the package's
version. Keep these sections:

- **Request**: what the user asked for, in their terms.
- **Sources**: each source with what is needed to fetch it again: a
  repository URL and the ref or commit (or the date it was fetched), a local
  path, or "written here". Say which paths were taken from each.
- **Choices**: what was adapted, renamed or left out, and why.
- **Refresh**: how to bring the package up to date from its sources.

Write only what you know: if a source or ref is unknown, say so; never invent
one.

## settings.json

`.claude/settings.json` is MERGED into each project's own file: keys the user
set are never replaced, and Connecto takes back exactly what it added when the
package changes. Put only what the package needs (for example `enabledPlugins`
and `extraKnownMarketplaces` for a plugin). Never put secrets or personal
paths in it. `.claude/settings.local.json` is never installed.

## What to do

1. Ask the user what the package is for, and which skills, agents, commands and
   plugins to include. Offer what you can find:
   - the user's own `~/.claude` and `~/.codex` (skills, agents, commands, and
     the plugins enabled in `~/.claude/settings.json`);
   - the skills and agents of their Connecto projects (the paths are in your
     first message);
   - the built-in `connecto` package (its files are listed in your first
     message), which a package can extend instead of copying.
2. Copy or write what they choose into the layout above. For plugins, set
   `enabledPlugins` (and `extraKnownMarketplaces` if the plugin comes from a
   marketplace) in `.claude/settings.json`; do not copy a plugin's cache.
3. A Claude skill and a Codex skill are different folders: `.claude/skills` is
   for Claude Code, `.agents/skills` for Codex. Ask before porting one to the
   other tool.
4. Run `connecto package check __ID__` after every edit and fix what it reports.
5. Write `ORIGIN.md` (above) before you finish. When changing an existing
   package, read it first; if the work changes what it says (a new source, a
   newer ref, different choices), bring it up to date. If it is missing, write
   it from what the package holds and what the user tells you.
6. Show the user what the package now contains and ask whether to change
   anything. They install it from Connecto's settings.

Do not edit anything outside this package's folder. Do not copy secrets,
tokens or credentials into a package.
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_file_gets_the_block_and_a_current_one_is_left_alone() {
        let body = text("Team", "team");
        let first = apply(None, &body).unwrap_or_default();
        assert!(first.starts_with(&format!("<!-- connecto-packages:begin v{VERSION} -->")));
        assert!(body.contains(ORIGIN), "the guide asks for the record");
        assert_eq!(apply(Some(&first), &body), None);
    }

    #[test]
    fn the_users_text_is_kept_and_an_old_block_is_replaced() {
        let appended = apply(Some("mine"), "guide").unwrap_or_default();
        assert!(appended.starts_with("mine\n\n") && appended.contains("guide"));
        let old = "mine\n\n<!-- connecto-packages:begin v0 -->\nold\n<!-- connecto-packages:end -->\ntail\n";
        let new = apply(Some(old), "new").unwrap_or_default();
        assert!(new.starts_with("mine\n\n") && new.contains("new") && !new.contains("old"));
        assert!(new.ends_with("tail\n"));
    }
}
