use chrono::{Local, TimeZone};
use iced::widget::{button, checkbox, column, container, responsive, row, scrollable, text, text_input, Column, Row};
use iced::{Center, Element, Fill, Length};

use crate::dedupe;
use crate::formatting::human_size;

use super::style;
use super::{App, Message, PendingAction, Phase, ResultsView, Tab};

pub fn root(app: &App) -> Element<'_, Message> {
    let content = column![header(app), tab_bar(app), error_banner(app), body(app)].spacing(0).width(Fill).height(Fill);

    container(content).width(Fill).height(Fill).into()
}

fn header(_app: &App) -> Element<'_, Message> {
    let title = column![text("File Sorter").size(28), text("Find, review, and clear out duplicate files -- safely.").size(13).color(style::MUTED)]
        .spacing(2);

    container(title).padding([20, 24]).width(Fill).into()
}

fn tab_bar(app: &App) -> Element<'_, Message> {
    let tab_button = |label: &'static str, tab: Tab, active: Tab| {
        let is_active = tab == active;
        button(text(label).size(14))
            .padding([8, 18])
            .style(if is_active { style::tab_active } else { style::tab_inactive })
            .on_press(Message::TabSelected(tab))
    };

    let bar = row![tab_button("Scan", Tab::Scan, app.tab), tab_button("History", Tab::History, app.tab)].spacing(8);

    container(bar).padding(pad4(0.0, 24.0, 16.0, 24.0)).into()
}

fn error_banner(app: &App) -> Element<'_, Message> {
    let Some(err) = &app.error else { return column![].into() };
    let bar = row![text(err.as_str()).size(13), horizontal_space(), button(text("Dismiss").size(12)).style(style::ghost_button).on_press(Message::DismissError)]
        .align_y(Center)
        .spacing(12);
    container(bar).padding(14).style(style::pill_danger_bg).width(Fill).into()
}

fn body<'a>(app: &'a App) -> Element<'a, Message> {
    match app.tab {
        Tab::Scan => scan_tab(app),
        Tab::History => history_tab(app),
    }
}

// ---------------------------------------------------------------- Scan tab

fn scan_tab(app: &App) -> Element<'_, Message> {
    let layout = row![
        container(setup_panel(app)).width(Length::FillPortion(2)),
        container(status_panel(app)).width(Length::FillPortion(3)),
    ]
    .spacing(16)
    .padding(pad4(0.0, 24.0, 24.0, 24.0));

    scrollable(layout).height(Fill).into()
}

fn setup_panel(app: &App) -> Element<'_, Message> {
    let content = column![
        section_title("Directories"),
        directory_input(app),
        directory_list(app),
        section_title("Options"),
        options_form(app),
        section_title("File types"),
        file_type_grid(app),
        scan_controls(app),
    ]
    .spacing(14);

    card(content)
}

fn directory_input(app: &App) -> Element<'_, Message> {
    let input = text_input("Paste or type a folder path...", &app.dir_input)
        .on_input(Message::DirInputChanged)
        .on_submit(Message::AddTypedDirectory)
        .padding(8);

    row![
        input,
        button(text("Add").size(13)).padding([8, 12]).on_press(Message::AddTypedDirectory),
        button(text("Browse...").size(13)).padding([8, 12]).style(button::secondary).on_press(Message::PickDirectory),
    ]
    .spacing(8)
    .into()
}

fn directory_list(app: &App) -> Element<'_, Message> {
    if app.directories.is_empty() {
        return container(text("No directories added yet.").size(13).color(style::MUTED)).padding(10).into();
    }

    let mut list = Column::new().spacing(6);
    for (idx, dir) in app.directories.iter().enumerate() {
        let row = row![
            text(dir.to_string_lossy().to_string()).size(13).width(Fill),
            button(text("Remove").size(11)).style(style::ghost_button).padding([4, 8]).on_press(Message::RemoveDirectory(idx)),
        ]
        .align_y(Center)
        .spacing(8);
        list = list.push(container(row).padding(8).style(if idx % 2 == 0 { style::row_even } else { style::row_odd }).width(Fill));
    }

    container(scrollable(list).height(Length::Fixed(130.0))).style(style::well).padding(4).into()
}

fn labeled_input<'a>(label: &'a str, suffix: &'a str, value: &'a str, on_change: impl Fn(String) -> Message + 'a) -> Element<'a, Message> {
    let input = text_input("0", value).on_input(on_change).padding(6).width(Length::Fixed(90.0));
    row![text(label).size(13).width(Length::Fixed(140.0)), input, text(suffix).size(12).color(style::MUTED)].align_y(Center).spacing(8).into()
}

fn options_form(app: &App) -> Element<'_, Message> {
    let numeric_row = column![
        labeled_input("Minimum size", "MB (0 = no minimum)", &app.min_size_mb, Message::MinSizeChanged),
        labeled_input("Large-file threshold", "MB (defer full hash above this)", &app.large_threshold_mb, Message::LargeThresholdChanged),
        labeled_input("Worker threads", "0 = one per core", &app.threads, Message::ThreadsChanged),
    ]
    .spacing(8);

    let toggles = column![
        checkbox(app.use_default_excludes)
            .label("Skip common build/cache folders (node_modules, .venv, __pycache__, ...)")
            .on_toggle(Message::ToggleDefaultExcludes)
            .size(16)
            .text_size(13),
        checkbox(app.exclude_temp_files)
            .label("Skip temp/backup files (.tmp, .bak, Thumbs.db, ...)")
            .on_toggle(Message::ToggleExcludeTempFiles)
            .size(16)
            .text_size(13),
        checkbox(app.dry_run).label("Dry run -- preview actions without touching any files").on_toggle(Message::ToggleDryRun).size(16).text_size(13),
    ]
    .spacing(8);

    let exclude_input = row![
        text_input("Extra folder name to skip...", &app.exclude_input)
            .on_input(Message::ExcludeInputChanged)
            .on_submit(Message::AddExclude)
            .padding(6),
        button(text("Add").size(13)).padding([6, 10]).on_press(Message::AddExclude),
    ]
    .spacing(8);

    let mut exclude_chips = Row::new().spacing(6);
    for (idx, ex) in app.excludes.iter().enumerate() {
        exclude_chips = exclude_chips.push(
            container(
                row![text(ex.as_str()).size(12), button(text("x").size(11)).style(style::ghost_button).padding(2).on_press(Message::RemoveExclude(idx))]
                    .spacing(6)
                    .align_y(Center),
            )
            .style(style::pill_muted)
            .padding([4, 8]),
        );
    }

    column![numeric_row, toggles, exclude_input, exclude_chips].spacing(12).into()
}

fn file_type_grid(app: &App) -> Element<'_, Message> {
    let checkboxes: Vec<(&'static str, bool)> =
        dedupe::valid_file_types().into_iter().map(|name| (name, app.file_type_filter.contains(name))).collect();

    // The type-filter list wraps onto as many rows as the panel's actual
    // width allows, instead of overflowing into a horizontal scrollbar.
    let grid = responsive(move |size| {
        const SPACING: f32 = 10.0;
        let item_width = |name: &str| 34.0 + name.chars().count() as f32 * 7.5;

        let mut rows = Column::new().spacing(8);
        let mut current_row = Row::new().spacing(SPACING);
        let mut current_width = 0.0_f32;

        for (name, checked) in checkboxes.iter().copied() {
            let width = item_width(name);
            if current_width > 0.0 && current_width + SPACING + width > size.width {
                rows = rows.push(current_row);
                current_row = Row::new().spacing(SPACING);
                current_width = 0.0;
            }
            current_width += if current_width > 0.0 { SPACING + width } else { width };

            let name_owned = name.to_string();
            current_row = current_row
                .push(checkbox(checked).label(name).on_toggle(move |v| Message::ToggleFileType(name_owned.clone(), v)).size(15).text_size(13));
        }

        rows.push(current_row).into()
    })
    .height(Length::Shrink);

    let hint = text("Leave all unchecked to scan every file type.").size(11).color(style::MUTED);
    column![grid, hint].spacing(6).into()
}

fn scan_controls(app: &App) -> Element<'_, Message> {
    let mut controls = Row::new().spacing(10);

    match app.phase {
        Phase::Scanning => {
            controls = controls.push(button(text("Scanning...").size(14)).padding([10, 20]));
            controls = controls.push(button(text("Cancel").size(14)).style(button::danger).padding([10, 20]).on_press(Message::CancelScan));
        }
        Phase::Acting => {
            controls = controls.push(button(text("Working...").size(14)).padding([10, 20]));
        }
        _ => {
            controls = controls.push(button(text("Start Scan").size(14)).padding([10, 20]).on_press(Message::StartScan));
            if app.resumable_run.is_some() {
                controls =
                    controls.push(button(text("Resume last scan").size(14)).style(button::secondary).padding([10, 20]).on_press(Message::ResumeScan));
            }
        }
    }

    container(controls).padding(pad4(6.0, 0.0, 0.0, 0.0)).into()
}

fn status_panel(app: &App) -> Element<'_, Message> {
    match app.phase {
        Phase::Scanning => card(scanning_view(app)),
        _ => match &app.last_summary {
            Some(_) => card(results_view(app)),
            None => card(idle_view()),
        },
    }
}

fn idle_view<'a>() -> Element<'a, Message> {
    container(
        column![
            text("Ready when you are.").size(18),
            text("Add one or more folders on the left, tune the options if you like, then hit Start Scan.").size(13).color(style::MUTED),
        ]
        .spacing(8),
    )
    .padding(30)
    .into()
}

fn scanning_view(app: &App) -> Element<'_, Message> {
    let elapsed = app.scan_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let dirs = app.directories.iter().map(|d| d.to_string_lossy().to_string()).collect::<Vec<_>>().join(", ");
    let dirs = if dirs.is_empty() { "the resumed run's directories".to_string() } else { dirs };

    column![
        text("Scanning...").size(20),
        text(format!("{:.1}s elapsed", elapsed)).size(14).color(style::MUTED),
        text(format!("Looking through: {}", dirs)).size(13).color(style::MUTED),
        text("Hashing and comparing files -- this can take a while for large folders.").size(12).color(style::MUTED),
    ]
    .spacing(10)
    .into()
}

fn results_view<'a>(app: &'a App) -> Element<'a, Message> {
    let Some(summary) = &app.last_summary else { return idle_view() };

    let mut sections = column![summary_cards(summary), results_tabs(app)].spacing(14);

    if summary.cancelled {
        let msg = if summary.resumable {
            "Scan was cancelled -- results so far are shown below. Use \"Resume last scan\" to pick up where it left off."
        } else {
            "Scan was cancelled."
        };
        sections = sections.push(container(text(msg).size(12)).padding(10).style(style::pill_warning_bg).width(Fill));
    }
    if summary.reused {
        sections = sections.push(
            container(text("Nothing changed since the last scan of these directories -- reused those results instantly.").size(12))
                .padding(10)
                .style(style::pill_accent_bg)
                .width(Fill),
        );
    }

    match app.results_view {
        ResultsView::Files => sections = sections.push(selection_bar(app, true)).push(file_groups(app)),
        ResultsView::Folders => sections = sections.push(selection_bar(app, false)).push(folder_groups(app)),
    }

    if let Some(PendingAction::Trash) = app.confirm {
        sections = sections.push(confirm_bar());
    }

    if let Some(report) = &app.last_action_report {
        sections = sections.push(action_report_view(report));
    }

    sections.into()
}

fn summary_cards(summary: &super::ScanSummary) -> Element<'_, Message> {
    let card = |label: &'static str, value: String| {
        container(column![text(value).size(20), text(label).size(11).color(style::MUTED)].spacing(4))
            .padding(12)
            .style(style::well)
            .width(Fill)
    };

    row![
        card("duplicate file groups", summary.groups.len().to_string()),
        card("duplicate folder groups", summary.folder_groups.len().to_string()),
        card("reclaimable", human_size(summary.reclaimable_bytes)),
        card("skipped (unreadable)", summary.skipped.to_string()),
        card("scan time", format!("{:.1}s", summary.duration_seconds)),
    ]
    .spacing(10)
    .into()
}

fn results_tabs(app: &App) -> Element<'_, Message> {
    let choice = |label: &'static str, view: ResultsView| {
        let active = app.results_view == view;
        button(text(label).size(13))
            .padding([6, 14])
            .style(if active { style::tab_active } else { style::tab_inactive })
            .on_press(Message::ResultsViewChanged(view))
    };
    row![choice("Files", ResultsView::Files), choice("Folders", ResultsView::Folders)].spacing(8).into()
}

fn selection_bar(app: &App, files: bool) -> Element<'_, Message> {
    let (count, size) = selected_totals(app, files);
    let label = if files { "files" } else { "folders" };
    let (all_msg, none_msg) = if files { (Message::SelectAllFiles, Message::SelectNoneFiles) } else { (Message::SelectAllFolders, Message::SelectNoneFolders) };

    row![
        text(format!("{} {} selected -- {} to reclaim", count, label, human_size(size))).size(13).color(style::MUTED),
        horizontal_space(),
        button(text("Select all").size(12)).style(style::ghost_button).padding([6, 10]).on_press(all_msg),
        button(text("Select none").size(12)).style(style::ghost_button).padding([6, 10]).on_press(none_msg),
        button(text("Move to Trash").size(13)).style(button::danger).padding([8, 14]).on_press(Message::RequestTrash),
        button(text("Move to folder...").size(13)).style(button::secondary).padding([8, 14]).on_press(Message::RequestMoveTo),
    ]
    .align_y(Center)
    .spacing(10)
    .into()
}

fn selected_totals(app: &App, files: bool) -> (usize, u64) {
    let Some(summary) = &app.last_summary else { return (0, 0) };
    if files {
        let count = app.file_removed.len();
        let size = summary
            .groups
            .iter()
            .enumerate()
            .map(|(gi, g)| app.file_removed.iter().filter(|(fgi, _)| *fgi == gi).count() as u64 * g.size)
            .sum();
        (count, size)
    } else {
        let count = app.folder_removed.len();
        let size = summary
            .folder_groups
            .iter()
            .enumerate()
            .map(|(gi, g)| app.folder_removed.iter().filter(|(fgi, _)| *fgi == gi).count() as u64 * g.size)
            .sum();
        (count, size)
    }
}

fn file_groups(app: &App) -> Element<'_, Message> {
    let Some(summary) = &app.last_summary else { return column![].into() };
    if summary.groups.is_empty() {
        return empty_note("No duplicate files found.");
    }

    let mut list = Column::new().spacing(10);
    for (gi, g) in summary.groups.iter().enumerate() {
        let mut card_body = Column::new().spacing(6);
        card_body = card_body.push(
            row![
                pill(human_size(g.size), style::pill_accent_bg),
                pill(format!("{} copies", g.paths.len()), style::pill_muted),
                pill(if g.confirmed { "verified".to_string() } else { "unconfirmed".to_string() }, if g.confirmed { style::pill_success_bg } else { style::pill_warning_bg }),
            ]
            .spacing(6),
        );
        for (pi, path) in g.paths.iter().enumerate() {
            card_body = card_body.push(path_row_file(gi, pi, path, app.file_removed.contains(&(gi, pi))));
        }
        list = list.push(container(card_body).padding(12).style(style::well));
    }

    container(scrollable(list).height(Length::Fixed(420.0))).into()
}

fn folder_groups(app: &App) -> Element<'_, Message> {
    let Some(summary) = &app.last_summary else { return column![].into() };
    if summary.folder_groups.is_empty() {
        return empty_note("No duplicate folders found.");
    }

    let mut list = Column::new().spacing(10);
    for (gi, g) in summary.folder_groups.iter().enumerate() {
        let mut card_body = Column::new().spacing(6);
        card_body = card_body.push(
            row![
                pill(human_size(g.size), style::pill_accent_bg),
                pill(format!("{} copies, {} files each", g.paths.len(), g.file_count), style::pill_muted),
                pill(if g.confirmed { "verified".to_string() } else { "unconfirmed".to_string() }, if g.confirmed { style::pill_success_bg } else { style::pill_warning_bg }),
            ]
            .spacing(6),
        );
        if !g.confirmed {
            card_body = card_body.push(
                text("Contains deferred large-file matches -- handled per-file in the Files tab instead of as a whole folder.")
                    .size(11)
                    .color(style::MUTED),
            );
        }
        for (pi, path) in g.paths.iter().enumerate() {
            card_body = card_body.push(path_row_folder(gi, pi, path, app.folder_removed.contains(&(gi, pi)), g.confirmed));
        }
        list = list.push(container(card_body).padding(12).style(style::well));
    }

    container(scrollable(list).height(Length::Fixed(420.0))).into()
}

fn path_row_file<'a>(gi: usize, pi: usize, path: &'a std::path::Path, checked: bool) -> Element<'a, Message> {
    let label = path.to_string_lossy().to_string();
    if pi == 0 {
        row![pill("KEEP".to_string(), style::pill_success_bg), text(label).size(12)].spacing(8).align_y(Center).into()
    } else {
        row![checkbox(checked).on_toggle(move |v| Message::ToggleFileRemoved(gi, pi, v)).size(15), text(label).size(12)]
            .spacing(8)
            .align_y(Center)
            .into()
    }
}

fn path_row_folder<'a>(gi: usize, pi: usize, path: &'a std::path::Path, checked: bool, confirmed: bool) -> Element<'a, Message> {
    let label = path.to_string_lossy().to_string();
    if pi == 0 {
        row![pill("KEEP".to_string(), style::pill_success_bg), text(label).size(12)].spacing(8).align_y(Center).into()
    } else if confirmed {
        row![checkbox(checked).on_toggle(move |v| Message::ToggleFolderRemoved(gi, pi, v)).size(15), text(label).size(12)]
            .spacing(8)
            .align_y(Center)
            .into()
    } else {
        row![pill("skip".to_string(), style::pill_muted), text(label).size(12).color(style::MUTED)].spacing(8).align_y(Center).into()
    }
}

fn confirm_bar<'a>() -> Element<'a, Message> {
    container(
        row![
            text("Move every checked item to the Trash?").size(13),
            horizontal_space(),
            button(text("Cancel").size(13)).style(style::ghost_button).padding([8, 14]).on_press(Message::CancelConfirm),
            button(text("Yes, move to Trash").size(13)).style(button::danger).padding([8, 14]).on_press(Message::ConfirmAction),
        ]
        .align_y(Center)
        .spacing(10),
    )
    .padding(12)
    .style(style::pill_warning_bg)
    .width(Fill)
    .into()
}

fn action_report_view(report: &super::ActionReport) -> Element<'_, Message> {
    let verb = if report.dry_run { "Would move" } else { "Moved" };
    let dest = report.dest.as_ref().map(|d| format!(" to {}", d.display())).unwrap_or_else(|| " to Trash".to_string());
    let mut body = column![text(format!("{} {} file(s) and {} folder(s){}.", verb, report.moved_files, report.moved_folders, dest)).size(13)].spacing(6);

    if !report.failures.is_empty() {
        body = body.push(text(format!("{} item(s) were not touched:", report.failures.len())).size(12).color(style::WARNING));
        let mut fails = Column::new().spacing(2);
        for f in report.failures.iter().take(20) {
            fails = fails.push(text(f.as_str()).size(11).color(style::MUTED));
        }
        body = body.push(fails);
    }

    container(body).padding(12).style(style::well).width(Fill).into()
}

// ------------------------------------------------------------- History tab

fn history_tab(app: &App) -> Element<'_, Message> {
    let content = if let Some(err) = &app.history_error {
        column![text(err.as_str()).size(13).color(style::DANGER)]
    } else if app.history.is_empty() {
        column![text("No scans yet -- run one from the Scan tab.").size(13).color(style::MUTED)]
    } else {
        let mut list = Column::new().spacing(8);
        for (idx, record) in app.history.iter().enumerate() {
            list = list.push(history_row(idx, record, &app.resumable_ids));
        }
        column![list]
    };

    container(card(content)).padding(pad4(0.0, 24.0, 24.0, 24.0)).into()
}

fn history_row<'a>(idx: usize, record: &'a crate::store::RunRecord, resumable: &std::collections::HashSet<String>) -> Element<'a, Message> {
    let run_id = format!("{}", record.timestamp);
    let status = if !record.cancelled {
        ("done", style::pill_success_bg as fn(&iced::Theme) -> container::Style)
    } else if resumable.contains(&run_id) {
        ("cancelled, resumable", style::pill_warning_bg as fn(&iced::Theme) -> container::Style)
    } else {
        ("cancelled", style::pill_muted as fn(&iced::Theme) -> container::Style)
    };

    let when = format_timestamp(record.timestamp);
    let dirs = record.directories.join(", ");

    let line1 = row![
        text(when).size(13),
        pill(status.0.to_string(), status.1),
        text(dirs).size(12).color(style::MUTED).width(Fill),
        button(text("View").size(12)).padding([6, 12]).on_press(Message::LoadHistoryRun(idx)),
    ]
    .spacing(10)
    .align_y(Center);

    let line2 = text(format!(
        "{} duplicate group(s) -- {} reclaimable -- {} skipped -- {:.1}s",
        record.groups,
        human_size(record.reclaimable_bytes.max(0) as u64),
        record.skipped,
        record.duration_seconds
    ))
    .size(11)
    .color(style::MUTED);

    container(column![line1, line2].spacing(4)).padding(10).style(style::well).width(Fill).into()
}

fn format_timestamp(ts: f64) -> String {
    match Local.timestamp_opt(ts as i64, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => "?".to_string(),
    }
}

// ------------------------------------------------------------------ helpers

fn section_title<'a>(label: &'a str) -> Element<'a, Message> {
    text(label).size(13).color(style::MUTED).into()
}

fn card<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content).padding(18).style(style::card).width(Fill).into()
}

fn pill<'a>(label: String, style_fn: fn(&iced::Theme) -> container::Style) -> Element<'a, Message> {
    container(text(label).size(11)).padding([3, 10]).style(style_fn).into()
}

fn empty_note<'a>(msg: &'a str) -> Element<'a, Message> {
    container(text(msg).size(13).color(style::MUTED)).padding(20).into()
}

fn horizontal_space<'a>() -> Element<'a, Message> {
    iced::widget::space::horizontal().into()
}

fn pad4(top: f32, right: f32, bottom: f32, left: f32) -> iced::Padding {
    iced::Padding { top, right, bottom, left }
}
