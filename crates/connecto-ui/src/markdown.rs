//! Markdown drawn as a document, for the file preview: headings, paragraphs
//! with bold, italic, struck and inline code, links, lists (nested, numbered,
//! task lists), quotes, code blocks, rules and tables. Images show as their
//! alt text. Nothing is fetched or run.

use egui::text::LayoutJob;
use egui::{Color32, FontFamily, FontId, Frame, Margin, RichText, Stroke, TextFormat, Ui};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::theme;

const BODY: f32 = 14.0;
const CODE: f32 = 13.0;
const INDENT: f32 = 18.0;

/// One block of the document, ready to draw.
#[derive(Debug, Clone)]
enum Block {
    Heading(u8, LayoutJob),
    Paragraph(LayoutJob),
    /// A list item: its depth, its marker and its first paragraph.
    Item {
        depth: usize,
        marker: String,
        text: LayoutJob,
    },
    Quote(usize, LayoutJob),
    Code {
        lang: String,
        text: String,
    },
    Rule,
    Table {
        aligns: Vec<Alignment>,
        rows: Vec<Vec<LayoutJob>>,
    },
}

/// Inline style in force.
#[derive(Default, Clone, Copy)]
struct Style {
    strong: bool,
    emphasis: bool,
    strike: bool,
    link: bool,
    heading: Option<u8>,
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 24.0,
        2 => 20.0,
        3 => 17.0,
        _ => 15.0,
    }
}

fn format(style: Style, code: bool) -> TextFormat {
    let size = style.heading.map_or(BODY, heading_size);
    let mut format = TextFormat {
        font_id: if code {
            FontId::monospace(CODE)
        } else {
            FontId::proportional(size)
        },
        color: if style.heading.is_some() || style.strong {
            Color32::WHITE
        } else {
            theme::TEXT_ROW
        },
        italics: style.emphasis,
        line_height: Some(size * 1.45),
        ..TextFormat::default()
    };
    if code {
        format.background = theme::RAISED_BG;
        format.color = theme::AMBER;
    }
    if style.link {
        format.color = theme::CYAN;
        format.underline = Stroke::new(1.0, theme::CYAN);
    }
    if style.strike {
        format.strikethrough = Stroke::new(1.0, theme::TEXT_MUTED);
    }
    format
}

/// Turn the Markdown into blocks.
fn parse(source: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut blocks = Vec::new();
    let mut job = LayoutJob::default();
    let mut style = Style::default();
    // Each open list: the next number, or `None` for bullets.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut item_marker: Option<String> = None;
    let mut quote = 0usize;
    let mut code: Option<(String, String)> = None;
    let mut table: Option<(Vec<Alignment>, Vec<Vec<LayoutJob>>)> = None;
    let mut metadata = false;

    let flush = |job: &mut LayoutJob,
                 blocks: &mut Vec<Block>,
                 item_marker: &mut Option<String>,
                 lists: &[Option<u64>],
                 quote: usize,
                 heading: Option<u8>| {
        let text = std::mem::take(job);
        if text.text.trim().is_empty() && item_marker.is_none() {
            return;
        }
        if let Some(level) = heading {
            blocks.push(Block::Heading(level, text));
        } else if let Some(marker) = item_marker.take() {
            blocks.push(Block::Item {
                depth: lists.len().saturating_sub(1),
                marker,
                text,
            });
        } else if !lists.is_empty() {
            // A later paragraph of a list item, under its text.
            blocks.push(Block::Item {
                depth: lists.len().saturating_sub(1),
                marker: String::new(),
                text,
            });
        } else if quote > 0 {
            blocks.push(Block::Quote(quote, text));
        } else {
            blocks.push(Block::Paragraph(text));
        }
    };

    for event in Parser::new_ext(source, options) {
        if metadata {
            if matches!(event, Event::End(TagEnd::MetadataBlock(_))) {
                metadata = false;
            }
            continue;
        }
        if let Some((_, text)) = &mut code {
            match event {
                Event::Text(t) => text.push_str(&t),
                Event::End(TagEnd::CodeBlock) => {
                    if let Some((lang, text)) = code.take() {
                        blocks.push(Block::Code {
                            lang,
                            text: text.trim_end_matches('\n').to_owned(),
                        });
                    }
                }
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::MetadataBlock(_) => metadata = true,
                Tag::Heading { level, .. } => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                    style.heading = Some(match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    });
                }
                Tag::Paragraph => {}
                Tag::BlockQuote(_) => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                    quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                    let lang = match kind {
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                    lists.push(start);
                }
                Tag::Item => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                    let marker = match lists.last_mut() {
                        Some(Some(number)) => {
                            let marker = format!("{number}.");
                            *number += 1;
                            marker
                        }
                        _ => if lists.len() % 2 == 1 { "•" } else { "◦" }.to_owned(),
                    };
                    item_marker = Some(marker);
                }
                Tag::Table(aligns) => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                    table = Some((aligns, Vec::new()));
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some((_, rows)) = &mut table {
                        rows.push(Vec::new());
                    }
                }
                Tag::TableCell => job = LayoutJob::default(),
                Tag::Emphasis => style.emphasis = true,
                Tag::Strong => style.strong = true,
                Tag::Strikethrough => style.strike = true,
                Tag::Link { .. } => style.link = true,
                Tag::Image { .. } => job.append("[image: ", 0.0, format(style, false)),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    let heading = style.heading.take();
                    flush(
                        &mut job,
                        &mut blocks,
                        &mut item_marker,
                        &lists,
                        quote,
                        heading,
                    );
                }
                TagEnd::Paragraph => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                }
                TagEnd::BlockQuote(_) => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                    quote = quote.saturating_sub(1);
                }
                TagEnd::Item => {
                    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                }
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::TableCell => {
                    if let Some((_, rows)) = &mut table
                        && let Some(row) = rows.last_mut()
                    {
                        row.push(std::mem::take(&mut job));
                    }
                }
                TagEnd::TableHead => style.strong = false,
                TagEnd::Table => {
                    if let Some((aligns, rows)) = table.take() {
                        blocks.push(Block::Table { aligns, rows });
                    }
                }
                TagEnd::Emphasis => style.emphasis = false,
                TagEnd::Strong => style.strong = false,
                TagEnd::Strikethrough => style.strike = false,
                TagEnd::Link => style.link = false,
                TagEnd::Image => job.append("]", 0.0, format(style, false)),
                _ => {}
            },
            Event::Text(text) => {
                // A table's header row is bold.
                let mut current = style;
                if table.as_ref().is_some_and(|(_, rows)| rows.len() == 1) {
                    current.strong = true;
                }
                job.append(&text, 0.0, format(current, false));
            }
            Event::Code(text) => job.append(&text, 0.0, format(style, true)),
            Event::SoftBreak => job.append(" ", 0.0, format(style, false)),
            Event::HardBreak => job.append("\n", 0.0, format(style, false)),
            Event::Rule => {
                flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
                blocks.push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                item_marker = Some(if done { "☑" } else { "☐" }.to_owned());
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                job.append(&html, 0.0, format(style, true));
            }
            Event::FootnoteReference(name) => {
                job.append(&format!("[{name}]"), 0.0, format(style, false));
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                job.append(&text, 0.0, format(style, true));
            }
        }
    }
    flush(&mut job, &mut blocks, &mut item_marker, &lists, quote, None);
    blocks
}

fn wrapped(mut job: LayoutJob, width: f32) -> LayoutJob {
    job.wrap.max_width = width;
    job
}

/// Draw `source` as a document, as wide as `ui` allows.
pub(crate) fn show(ui: &mut Ui, source: &str) {
    let blocks = parse(source);
    let width = ui.available_width();
    ui.spacing_mut().item_spacing.y = 6.0;
    for block in blocks {
        match block {
            Block::Heading(level, job) => {
                ui.add_space(if level <= 2 { 14.0 } else { 8.0 });
                ui.label(wrapped(job, width));
                if level <= 2 {
                    let rect = ui.available_rect_before_wrap();
                    ui.painter()
                        .hline(rect.x_range(), rect.min.y, Stroke::new(1.0, theme::BORDER));
                    ui.add_space(6.0);
                }
            }
            Block::Paragraph(job) => {
                ui.label(wrapped(job, width));
                ui.add_space(4.0);
            }
            Block::Item {
                depth,
                marker,
                text,
            } => {
                let left = INDENT * depth as f32;
                ui.horizontal_top(|ui| {
                    ui.add_space(left);
                    let (marker_rect, _) = ui
                        .allocate_exact_size(egui::vec2(INDENT + 6.0, BODY), egui::Sense::hover());
                    ui.painter().text(
                        marker_rect.right_top() + egui::vec2(-6.0, 3.0),
                        egui::Align2::RIGHT_TOP,
                        marker,
                        FontId::proportional(BODY),
                        theme::TEXT_MUTED,
                    );
                    let rest = (width - left - INDENT - 12.0).max(80.0);
                    ui.label(wrapped(text, rest));
                });
            }
            Block::Quote(depth, job) => {
                let left = 14.0 * depth as f32;
                let response = ui
                    .horizontal_top(|ui| {
                        ui.add_space(left);
                        let mut job = job;
                        for section in &mut job.sections {
                            section.format.color = theme::TEXT_MUTED;
                        }
                        ui.label(wrapped(job, (width - left - 8.0).max(80.0)));
                    })
                    .response;
                let rect = response.rect;
                ui.painter().vline(
                    rect.min.x + left - 8.0,
                    rect.y_range(),
                    Stroke::new(3.0, theme::TEAL),
                );
            }
            Block::Code { lang, text } => {
                Frame::NONE
                    .fill(theme::TERM_BG)
                    .stroke(Stroke::new(1.0, theme::HAIR))
                    .corner_radius(6.0)
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        if !lang.is_empty() {
                            ui.label(RichText::new(lang).size(11.0).color(theme::FAINT));
                        }
                        egui::ScrollArea::horizontal()
                            .id_salt(ui.next_auto_id())
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(text)
                                            .family(FontFamily::Monospace)
                                            .size(CODE)
                                            .color(theme::TEXT),
                                    )
                                    .extend(),
                                );
                            });
                    });
                ui.add_space(4.0);
            }
            Block::Rule => {
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
            }
            Block::Table { aligns, rows } => {
                egui::ScrollArea::horizontal()
                    .id_salt(ui.next_auto_id())
                    .show(ui, |ui| {
                        egui::Grid::new(ui.next_auto_id())
                            .striped(true)
                            .spacing(egui::vec2(16.0, 6.0))
                            .show(ui, |ui| {
                                for row in rows {
                                    for (index, cell) in row.into_iter().enumerate() {
                                        let layout = match aligns.get(index) {
                                            Some(Alignment::Right) => {
                                                egui::Layout::right_to_left(egui::Align::Center)
                                            }
                                            Some(Alignment::Center) => {
                                                egui::Layout::top_down(egui::Align::Center)
                                            }
                                            _ => egui::Layout::left_to_right(egui::Align::Center),
                                        };
                                        ui.with_layout(layout, |ui| {
                                            ui.label(wrapped(cell, 360.0));
                                        });
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                ui.add_space(6.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<String> {
        parse(source)
            .into_iter()
            .map(|block| match block {
                Block::Heading(level, job) => format!("h{level}:{}", job.text),
                Block::Paragraph(job) => format!("p:{}", job.text),
                Block::Item {
                    depth,
                    marker,
                    text,
                } => format!("li{depth}{marker}:{}", text.text),
                Block::Quote(depth, job) => format!("q{depth}:{}", job.text),
                Block::Code { lang, text } => format!("code({lang}):{text}"),
                Block::Rule => "hr".to_owned(),
                Block::Table { rows, .. } => format!(
                    "table:{}",
                    rows.iter()
                        .map(|row| row
                            .iter()
                            .map(|c| c.text.clone())
                            .collect::<Vec<_>>()
                            .join("|"))
                        .collect::<Vec<_>>()
                        .join("/")
                ),
            })
            .collect()
    }

    #[test]
    fn a_document_becomes_its_blocks() {
        let source = "---\ntitle: x\n---\n# Title\n\nSome **bold** and `code`\nwrapped.\n\n- one\n- two\n  1. nested\n- [x] done\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n---\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n";
        assert_eq!(
            kinds(source),
            [
                "h1:Title",
                "p:Some bold and code wrapped.",
                "li0•:one",
                "li0•:two",
                "li11.:nested",
                "li0☑:done",
                "q1:quoted",
                "code(rust):fn main() {}",
                "hr",
                "table:a|b/1|2",
            ]
        );
    }
}
