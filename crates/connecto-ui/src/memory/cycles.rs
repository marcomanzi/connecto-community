//! The two cycles, configured apart (spec §8.1, §8.2): provider, model and
//! effort from the providers' capabilities, then each cycle's limits. Every
//! number is clamped to the contract's range before it is sent.

use connecto_core::memory::{
    DevelopmentSettings, DreamSettings, MemorySettings, ModelChoice, Provider, ProviderCapability,
    Weekday,
};
use egui::{RichText, Ui};

use super::{Cx, MemoryAction, MemoryPanel, number, row, text, time};
use crate::forms::{self, Out, heading, muted, warning};
use crate::theme;

/// Keys of one cycle's model controls.
struct Keys {
    provider: &'static str,
    model: &'static str,
    effort: &'static str,
}

const DEV: Keys = Keys {
    provider: "mem-dev-provider",
    model: "mem-dev-model",
    effort: "mem-dev-effort",
};

const DREAM: Keys = Keys {
    provider: "mem-dream-provider",
    model: "mem-dream-model",
    effort: "mem-dream-effort",
};

pub(super) fn show(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let mut next: Option<MemorySettings> = None;
    development(ui, panel, cx, &mut next, out);
    forms::separator(ui);
    dream(ui, panel, cx, &mut next, out);
    if let Some(settings) = next {
        out.actions.push(MemoryAction::Settings(settings.clamped()));
    }
}

/// Settings to change: the change already made this pass, or the shown ones.
fn base(cx: &Cx<'_>, next: &Option<MemorySettings>) -> MemorySettings {
    next.clone().unwrap_or_else(|| cx.settings.clone())
}

fn development(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    cx: &Cx<'_>,
    next: &mut Option<MemorySettings>,
    out: &mut Out<MemoryAction>,
) {
    let dev = &cx.settings.development;
    out.say(ui, heading("DAYTIME CYCLE"));
    out.say(
        ui,
        muted("While you work, a cheap model reads new events in small batches and writes short notes with their sources."),
    );
    if let Some(choice) = model_choice(ui, cx, &dev.choice, &DEV, out) {
        let mut s = base(cx, next);
        s.development.choice = choice;
        *next = Some(s);
    }
    let fields: [(
        &str,
        &'static str,
        u32,
        &std::ops::RangeInclusive<u32>,
        &str,
    ); 4] = [
        (
            "Grouping window",
            "mem-dev-group",
            dev.group_seconds,
            &DevelopmentSettings::GROUP_SECONDS,
            "seconds from the first event (0: at once)",
        ),
        (
            "Runs per hour, at most",
            "mem-dev-runs",
            dev.max_runs_per_hour,
            &DevelopmentSettings::MAX_RUNS_PER_HOUR,
            "beyond it the queue waits",
        ),
        (
            "Input limit",
            "mem-dev-input",
            dev.max_input_chars,
            &DevelopmentSettings::MAX_INPUT_CHARS,
            "characters per run; a larger batch is split, never cut",
        ),
        (
            "Attempts",
            "mem-dev-attempts",
            dev.max_attempts,
            &DevelopmentSettings::MAX_ATTEMPTS,
            "before a batch counts as failed",
        ),
    ];
    for (label, key, value, range, hint) in fields {
        let changed = number_row(ui, panel, out, label, key, value, range, hint);
        if let Some(n) = changed {
            let mut s = base(cx, next);
            let d = &mut s.development;
            match key {
                "mem-dev-group" => d.group_seconds = n,
                "mem-dev-runs" => d.max_runs_per_hour = n,
                "mem-dev-input" => d.max_input_chars = n,
                _ => d.max_attempts = n,
            }
            *next = Some(s);
        }
    }
}

fn dream(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    cx: &Cx<'_>,
    next: &mut Option<MemorySettings>,
    out: &mut Out<MemoryAction>,
) {
    let dream = &cx.settings.dream;
    out.say(ui, heading("DREAM CYCLE"));
    out.say(
        ui,
        muted("At the time you choose, a model consolidates notes and evidence into memories. Connecto checks what it proposes and publishes it as one version you can undo. It also builds the memory from the project, unless you pick otherwise."),
    );
    if let Some(choice) = model_choice(ui, cx, &dream.choice, &DREAM, out) {
        let mut s = base(cx, next);
        s.dream.choice = choice;
        *next = Some(s);
    }
    row(ui, out, "Days", |ui, out| {
        for day in Weekday::ALL {
            let on = dream.days.contains(day);
            let short = day.label().get(..3).unwrap_or(day.label());
            let key = day_key(*day);
            if out.choice(ui, key, on, short) {
                let mut s = base(cx, next);
                if on {
                    s.dream.days.remove(day);
                } else {
                    s.dream.days.insert(*day);
                }
                *next = Some(s);
            }
        }
    });
    if dream.days.is_empty() {
        out.say(
            ui,
            muted("No days chosen: it never runs on schedule. Dream now still works."),
        );
    }
    let mut time_result = None;
    row(ui, out, "Time", |ui, out| {
        time_result = time(ui, panel, out, "mem-dream-time", dream.time);
        out.say(
            ui,
            RichText::new("local time · nothing runs while Connecto is closed; a missed date runs once when it opens again")
                .size(11.5)
                .color(theme::AMBER),
        );
    });
    match time_result {
        Some(Ok(t)) => {
            let mut s = base(cx, next);
            s.dream.time = t;
            *next = Some(s);
        }
        Some(Err(why)) => panel.error = Some(why),
        None => {}
    }
    let fields: [(
        &str,
        &'static str,
        u32,
        &std::ops::RangeInclusive<u32>,
        &str,
    ); 3] = [
        (
            "Time limit",
            "mem-dream-minutes",
            dream.max_minutes,
            &DreamSettings::MAX_MINUTES,
            "minutes",
        ),
        (
            "Input limit",
            "mem-dream-input",
            dream.max_input_chars,
            &DreamSettings::MAX_INPUT_CHARS,
            "characters per run; more is split by room",
        ),
        (
            "Minimum new items",
            "mem-dream-min-new",
            dream.min_new_items,
            &DreamSettings::MIN_NEW_ITEMS,
            "events or notes since the last dream, or it skips",
        ),
    ];
    for (label, key, value, range, hint) in fields {
        if let Some(n) = number_row(ui, panel, out, label, key, value, range, hint) {
            let mut s = base(cx, next);
            match key {
                "mem-dream-minutes" => s.dream.max_minutes = n,
                "mem-dream-input" => s.dream.max_input_chars = n,
                _ => s.dream.min_new_items = n,
            }
            *next = Some(s);
        }
    }
}

const fn day_key(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "mem-dream-day:mon",
        Weekday::Tue => "mem-dream-day:tue",
        Weekday::Wed => "mem-dream-day:wed",
        Weekday::Thu => "mem-dream-day:thu",
        Weekday::Fri => "mem-dream-day:fri",
        Weekday::Sat => "mem-dream-day:sat",
        Weekday::Sun => "mem-dream-day:sun",
    }
}

/// A labelled number field with its range. Returns the new value when one
/// was typed; a bad one shows at the top of the section.
#[allow(clippy::too_many_arguments)]
fn number_row(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    out: &mut Out<MemoryAction>,
    label: &str,
    key: &'static str,
    value: u32,
    range: &std::ops::RangeInclusive<u32>,
    hint: &str,
) -> Option<u32> {
    let wide = u64::from(*range.start())..=u64::from(*range.end());
    let mut result = None;
    row(ui, out, label, |ui, out| {
        result = number(ui, panel, out, key, u64::from(value), &wide);
        out.say(
            ui,
            muted(format!(
                "{hint} ({}–{})",
                text::count(*wide.start()),
                text::count(*wide.end())
            )),
        );
    });
    match result {
        Some(Ok(n)) => u32::try_from(n).ok(),
        Some(Err(why)) => {
            panel.error = Some(why);
            None
        }
        None => None,
    }
}

/// Provider, model and effort. An unavailable provider is shown with its
/// reason and cannot be picked. Returns the new choice when one was made.
fn model_choice(
    ui: &mut Ui,
    cx: &Cx<'_>,
    choice: &ModelChoice,
    keys: &Keys,
    out: &mut Out<MemoryAction>,
) -> Option<ModelChoice> {
    let mut picked = None;
    let mut reasons = Vec::new();
    row(ui, out, "Provider", |ui, out| {
        for provider in Provider::ALL {
            let cap = cx.view.capability(*provider);
            let selected = choice.provider == *provider;
            let key = format!("{}:{}", keys.provider, provider.word());
            match availability(cap) {
                Ok(cap) => {
                    if out.choice(ui, &key, selected, provider.label()) && !selected {
                        picked = Some(ModelChoice {
                            provider: *provider,
                            model: first_or(&cap.models, &choice.model),
                            effort: first_or(&cap.efforts, &choice.effort),
                        });
                    }
                }
                Err(reason) => {
                    let response = ui
                        .add_enabled_ui(false, |ui| ui.selectable_label(selected, provider.label()))
                        .inner
                        .on_disabled_hover_text(&reason);
                    out.mark(&key, response.rect);
                    reasons.push((*provider, reason));
                }
            }
        }
    });
    for (provider, reason) in &reasons {
        out.say(
            ui,
            muted(format!("{} is unavailable: {reason}", provider.label())),
        );
    }
    if let Some((_, reason)) = reasons.iter().find(|(p, _)| *p == choice.provider) {
        out.say(
            ui,
            warning(format!(
                "The chosen provider is unavailable ({reason}): this cycle does not run until you pick an available one."
            )),
        );
    }
    let cap = cx
        .view
        .capability(choice.provider)
        .filter(|c| c.capability.is_available());
    let models = cap.map(|c| c.models.as_slice()).unwrap_or_default();
    let efforts = cap.map(|c| c.efforts.as_slice()).unwrap_or_default();
    if let Some(model) = words(ui, out, "Model", keys.model, models, &choice.model) {
        let mut c = picked.clone().unwrap_or_else(|| choice.clone());
        c.model = model;
        picked = Some(c);
    }
    if let Some(effort) = words(ui, out, "Effort", keys.effort, efforts, &choice.effort) {
        let mut c = picked.clone().unwrap_or_else(|| choice.clone());
        c.effort = effort;
        picked = Some(c);
    }
    picked
}

fn availability(cap: Option<&ProviderCapability>) -> Result<&ProviderCapability, String> {
    match cap {
        Some(c) if c.capability.is_available() => Ok(c),
        Some(c) => Err(c.capability.reason().unwrap_or("unavailable").to_owned()),
        None => Err("not reported by the memory engine yet".to_owned()),
    }
}

/// `current` when the list has it, or the list's first word, or `current`.
fn first_or(list: &[String], current: &str) -> String {
    if list.iter().any(|w| w == current) {
        current.to_owned()
    } else {
        list.first().cloned().unwrap_or_else(|| current.to_owned())
    }
}

/// One row of word choices; the current word is shown even when the list
/// lacks it. Returns the word clicked, if new.
fn words(
    ui: &mut Ui,
    out: &mut Out<MemoryAction>,
    label: &str,
    key: &str,
    list: &[String],
    current: &str,
) -> Option<String> {
    let mut picked = None;
    row(ui, out, label, |ui, out| {
        if !list.iter().any(|w| w == current) {
            out.choice(ui, &format!("{key}:{current}"), true, current);
        }
        for word in list {
            let on = word == current;
            if out.choice(ui, &format!("{key}:{word}"), on, word) && !on {
                picked = Some(word.clone());
            }
        }
    });
    picked
}
