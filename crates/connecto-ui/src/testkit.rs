//! Where the last pass drew each control, for tests that click them.

use egui::Rect;

use crate::automations::AutomationsHits;
use crate::chooser::ChooserHits;
use crate::forms::Marks;
use crate::header::HeadOut;
use crate::history::HistoryHits;
use crate::tree::Hit;
use crate::utility::UtilityHits;

pub(crate) struct Hits {
    pub(crate) tree: Vec<(Hit, Rect)>,
    pub(crate) head: HeadOut,
    pub(crate) chooser: ChooserHits,
    pub(crate) inline: ChooserHits,
    pub(crate) history: HistoryHits,
    pub(crate) utility: UtilityHits,
    pub(crate) utility_banner: Option<Rect>,
    pub(crate) update_strip: Marks,
    pub(crate) offer_strip: Marks,
    pub(crate) automations: AutomationsHits,
    pub(crate) automations_banner: AutomationsHits,
    pub(crate) skills: Marks,
    pub(crate) settings: Marks,
    pub(crate) host_sheet: Marks,
    pub(crate) host_rows: crate::hosts::RowRects,
    pub(crate) project_settings: Marks,
    pub(crate) pairing: Marks,
    pub(crate) confirm: Marks,
    pub(crate) new_project: Marks,
    pub(crate) delete_project: Marks,
    pub(crate) notice: Marks,
    pub(crate) secret_ask: Marks,
    pub(crate) job_question: Marks,
    /// The minimized requests' cards in the corner.
    pub(crate) pending: Marks,
    pub(crate) content: Rect,
    pub(crate) sidebar: Rect,
    pub(crate) main: Rect,
    pub(crate) notice_height: f32,
    pub(crate) open_button: Option<Rect>,
    pub(crate) recent_rows: Vec<Rect>,
}

impl Default for Hits {
    fn default() -> Self {
        Self {
            tree: Vec::new(),
            head: HeadOut::default(),
            chooser: ChooserHits::default(),
            inline: ChooserHits::default(),
            history: HistoryHits::default(),
            utility: UtilityHits::default(),
            utility_banner: None,
            update_strip: Marks::default(),
            offer_strip: Marks::default(),
            automations: AutomationsHits::default(),
            automations_banner: AutomationsHits::default(),
            skills: Marks::default(),
            settings: Marks::default(),
            host_sheet: Marks::default(),
            host_rows: Vec::new(),
            project_settings: Marks::default(),
            pairing: Marks::default(),
            confirm: Marks::default(),
            new_project: Marks::default(),
            delete_project: Marks::default(),
            notice: Marks::default(),
            secret_ask: Marks::default(),
            job_question: Marks::default(),
            pending: Marks::default(),
            content: Rect::NOTHING,
            sidebar: Rect::NOTHING,
            main: Rect::NOTHING,
            notice_height: 0.0,
            open_button: None,
            recent_rows: Vec::new(),
        }
    }
}
