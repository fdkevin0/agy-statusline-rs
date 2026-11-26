//! Full-screen historical conversation browser powered by `tui-input` and `nucleo-matcher`.

use std::cmp::Ordering;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::{
    compute_header_hits, render_footer_bar, render_table_with_scrollbar, right_cell,
    sort_header_cell, ClickRegions, HeaderHits, Palette, SpanBar, TableChrome, TableHit,
};
use crate::stats::metric::{ConversationSort, ConversationSortField, SortDir};
use crate::stats::query::{filter_and_rank_conversations, SourceFilter};
use crate::stats::render::format_conversation_display_title;
use crate::stats::ConversationListItem;
use crate::sys::fmt::{format_tokens_string as fmt_tok, truncate_to_width};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PickerAction {
    Continue,
    Reload,
    Select(String),
    Back,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerClick {
    StartSearch,
    ExitSearch,
    ClearQuery,
    CycleSource,
    OpenSelected,
    CycleSortField,
    ToggleSubagents,
    Reload,
    Back,
    Quit,
}

fn cmp_opt_str_ci(a: Option<&str>, b: Option<&str>, dir: SortDir) -> Ordering {
    match (a, b) {
        (Some(va), Some(vb)) => dir.apply(va.to_ascii_lowercase().cmp(&vb.to_ascii_lowercase())),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_conversations(
    a: &ConversationListItem,
    b: &ConversationListItem,
    sort: ConversationSort,
) -> Ordering {
    let ord = match sort.field {
        ConversationSortField::Modified => sort
            .dir
            .apply(a.last_modified_secs.cmp(&b.last_modified_secs)),
        ConversationSortField::Id => sort.dir.apply(a.conversation_id.cmp(&b.conversation_id)),
        ConversationSortField::Title => {
            cmp_opt_str_ci(a.title.as_deref(), b.title.as_deref(), sort.dir)
        }
        ConversationSortField::Workspace => {
            cmp_opt_str_ci(a.workspace.as_deref(), b.workspace.as_deref(), sort.dir)
        }
        ConversationSortField::Steps => match (a.step_count, b.step_count) {
            (Some(sa), Some(sb)) => sort.dir.apply(sa.cmp(&sb)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => sort.dir.apply(a.db_size_bytes.cmp(&b.db_size_bytes)),
        },
        ConversationSortField::Subagents => sort.dir.apply(a.subagent_count.cmp(&b.subagent_count)),
        ConversationSortField::Source => sort.dir.apply(a.source.cmp(&b.source)),
    };
    ord.then_with(|| b.last_modified_secs.cmp(&a.last_modified_secs))
        .then_with(|| a.conversation_id.cmp(&b.conversation_id))
}

pub(super) struct PickerScreen {
    items: Vec<ConversationListItem>,
    filtered: Vec<usize>,
    selected: usize,
    input: Input,
    searching: bool,
    show_subagents: bool,
    source_filter: SourceFilter,
    sort: ConversationSort,
    has_cli: bool,
    has_desktop: bool,
    active_id: Option<String>,
    status_msg: Option<String>,
    table_state: TableState,
    search_bar: Rect,
    search_hits: ClickRegions<PickerClick>,
    footer_hits: ClickRegions<PickerClick>,
    table_hit: TableHit,
    header_hits: HeaderHits<ConversationSortField>,
}

impl PickerScreen {
    pub(super) fn new(
        items: Vec<ConversationListItem>,
        active_id: Option<&str>,
        initial_query: Option<&str>,
        show_subagents: bool,
        sort: ConversationSort,
    ) -> Self {
        let mut screen = Self {
            items: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            input: Input::new(initial_query.unwrap_or("").trim().to_string()),
            searching: false,
            show_subagents,
            source_filter: SourceFilter::All,
            sort,
            has_cli: false,
            has_desktop: false,
            active_id: active_id.map(str::to_ascii_lowercase),
            status_msg: None,
            table_state: TableState::default(),
            search_bar: Rect::default(),
            search_hits: ClickRegions::default(),
            footer_hits: ClickRegions::default(),
            table_hit: TableHit::default(),
            header_hits: HeaderHits::default(),
        };
        screen.replace_items(items, active_id);
        screen
    }

    pub(super) fn active_id(&self) -> Option<&str> {
        self.active_id.as_deref()
    }

    pub(super) fn sort(&self) -> ConversationSort {
        self.sort
    }

    pub(super) fn set_active_id(&mut self, active_id: Option<&str>) {
        self.active_id = active_id.map(str::to_ascii_lowercase);
    }

    pub(super) fn set_status_msg(&mut self, msg: impl Into<String>) {
        self.status_msg = Some(msg.into());
    }

    pub(super) fn replace_items(
        &mut self,
        items: Vec<ConversationListItem>,
        active_id: Option<&str>,
    ) {
        self.has_cli = items.iter().any(|it| it.source == "cli");
        self.has_desktop = items.iter().any(|it| it.source == "desktop");
        self.active_id = active_id.map(str::to_ascii_lowercase);
        self.items = items;
        if let Some(active) = self.active_id.as_deref() {
            if self
                .items
                .iter()
                .any(|it| it.conversation_id == active && it.parent_conversation_id.is_some())
            {
                self.show_subagents = true;
            }
        }
        self.recompute_filtered();
        if let Some(active) = self.active_id.as_deref() {
            if let Some(pos) = self
                .filtered
                .iter()
                .position(|&idx| self.items[idx].conversation_id == active)
            {
                self.selected = pos;
            }
        }
    }

    /// Updates the conversation list on a live watch tick without interrupting search
    /// input or losing the currently highlighted conversation.
    pub(super) fn refresh_items_live(&mut self, items: Vec<ConversationListItem>) {
        let selected_id = self
            .filtered
            .get(self.selected)
            .map(|&idx| self.items[idx].conversation_id.clone());
        self.has_cli = items.iter().any(|it| it.source == "cli");
        self.has_desktop = items.iter().any(|it| it.source == "desktop");
        self.items = items;
        self.recompute_filtered();
        if let Some(sel_id) = selected_id.as_deref() {
            if let Some(pos) = self
                .filtered
                .iter()
                .position(|&idx| self.items[idx].conversation_id == sel_id)
            {
                self.selected = pos;
            }
        }
    }

    fn recompute_filtered(&mut self) {
        self.filtered = filter_and_rank_conversations(
            &self.items,
            self.input.value(),
            self.show_subagents,
            self.source_filter,
        );
        if self.input.value().trim().is_empty() || self.sort != ConversationSort::default() {
            self.filtered
                .sort_by(|&a, &b| cmp_conversations(&self.items[a], &self.items[b], self.sort));
        }
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }

    fn cycle_sort_field(&mut self) {
        let fields: Vec<ConversationSortField> = if !self.header_hits.cols.is_empty() {
            self.header_hits.cols.iter().map(|c| c.col).collect()
        } else {
            let mut all = vec![
                ConversationSortField::Modified,
                ConversationSortField::Id,
                ConversationSortField::Title,
                ConversationSortField::Workspace,
                ConversationSortField::Steps,
                ConversationSortField::Subagents,
            ];
            if self.has_cli && self.has_desktop {
                all.push(ConversationSortField::Source);
            }
            all
        };
        let next_field = fields
            .iter()
            .position(|&f| f == self.sort.field)
            .map_or(ConversationSortField::Modified, |idx| {
                fields[(idx + 1) % fields.len()]
            });
        self.sort = ConversationSort {
            field: next_field,
            dir: next_field.default_dir(),
        };
        self.selected = 0;
        self.recompute_filtered();
    }

    fn click_sort_field(&mut self, field: ConversationSortField) {
        self.sort = self.sort.click(field);
        self.selected = 0;
        self.recompute_filtered();
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        self.status_msg = None;
        let page = self.table_hit.data_rows.max(5);
        let len = self.filtered.len();

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c' | 'C') => return PickerAction::Quit,
                KeyCode::Char('n' | 'j') if len > 0 => {
                    self.selected = (self.selected + 1).min(len - 1);
                    return PickerAction::Continue;
                }
                KeyCode::Char('p' | 'k') => {
                    self.selected = self.selected.saturating_sub(1);
                    return PickerAction::Continue;
                }
                _ => {}
            }
        }

        if self.searching {
            match key.code {
                KeyCode::Esc => self.searching = false,
                KeyCode::Enter => {
                    if let Some(&idx) = self.filtered.get(self.selected) {
                        return PickerAction::Select(self.items[idx].conversation_id.clone());
                    }
                    self.searching = false;
                }
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down if len > 0 => self.selected = (self.selected + 1).min(len - 1),
                KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
                KeyCode::PageDown if len > 0 => self.selected = (self.selected + page).min(len - 1),
                _ => {
                    if let Some(changed) = self.input.handle_event(&Event::Key(key)) {
                        if changed.value {
                            self.selected = 0;
                            self.recompute_filtered();
                        }
                    }
                }
            }
            return PickerAction::Continue;
        }

        match key.code {
            KeyCode::Char('q') => return PickerAction::Quit,
            KeyCode::Char('b') => return PickerAction::Back,
            KeyCode::Esc => {
                if !self.input.value().is_empty() || self.source_filter != SourceFilter::All {
                    self.input.reset();
                    self.source_filter = SourceFilter::All;
                    self.selected = 0;
                    self.recompute_filtered();
                } else {
                    return PickerAction::Back;
                }
            }
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('a') => {
                self.show_subagents = !self.show_subagents;
                self.selected = 0;
                self.recompute_filtered();
            }
            KeyCode::Char('f') if self.has_cli && self.has_desktop => {
                self.source_filter = self.source_filter.next();
                self.selected = 0;
                self.recompute_filtered();
            }
            KeyCode::Char('s') => self.cycle_sort_field(),
            KeyCode::Char('S') => {
                self.sort = self.sort.toggle_dir();
                self.selected = 0;
                self.recompute_filtered();
            }
            KeyCode::Char('r') => return PickerAction::Reload,
            KeyCode::Enter => {
                if let Some(&idx) = self.filtered.get(self.selected) {
                    return PickerAction::Select(self.items[idx].conversation_id.clone());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if len > 0 => {
                self.selected = (self.selected + 1).min(len - 1);
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown if len > 0 => self.selected = (self.selected + page).min(len - 1),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') if len > 0 => self.selected = len - 1,
            _ => {}
        }
        PickerAction::Continue
    }

    fn apply_click(&mut self, click: PickerClick, has_back: bool) -> PickerAction {
        match click {
            PickerClick::StartSearch => self.searching = true,
            PickerClick::ExitSearch => self.searching = false,
            PickerClick::ClearQuery => {
                self.input.reset();
                self.searching = false;
                self.selected = 0;
                self.recompute_filtered();
            }
            PickerClick::CycleSource => {
                if self.has_cli && self.has_desktop || self.source_filter != SourceFilter::All {
                    self.source_filter = self.source_filter.next();
                    self.searching = false;
                    self.selected = 0;
                    self.recompute_filtered();
                }
            }
            PickerClick::OpenSelected => {
                if let Some(&idx) = self.filtered.get(self.selected) {
                    return PickerAction::Select(self.items[idx].conversation_id.clone());
                }
            }
            PickerClick::CycleSortField => {
                self.searching = false;
                self.cycle_sort_field();
            }
            PickerClick::ToggleSubagents => {
                self.searching = false;
                self.show_subagents = !self.show_subagents;
                self.selected = 0;
                self.recompute_filtered();
            }
            PickerClick::Reload => {
                self.searching = false;
                return PickerAction::Reload;
            }
            PickerClick::Back if has_back => return PickerAction::Back,
            PickerClick::Back => {}
            PickerClick::Quit => return PickerAction::Quit,
        }
        PickerAction::Continue
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent, has_back: bool) -> PickerAction {
        let pos = Position::new(mouse.column, mouse.row);
        let len = self.filtered.len();
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.status_msg = None;
                self.selected = self.selected.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if len > 0 => {
                self.status_msg = None;
                self.selected = (self.selected + 3).min(len - 1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.status_msg = None;
                if let Some(click) = self
                    .search_hits
                    .hit(pos)
                    .or_else(|| self.footer_hits.hit(pos))
                {
                    return self.apply_click(click, has_back);
                }
                if self.search_bar.contains(pos) {
                    self.searching = true;
                    return PickerAction::Continue;
                }
                if let Some(col) = self.header_hits.col_at(pos) {
                    self.searching = false;
                    self.click_sort_field(col);
                    return PickerAction::Continue;
                }
                if let Some(target) = self.table_hit.row_at(pos, self.table_state.offset(), len) {
                    if self.selected == target && !self.searching {
                        return PickerAction::Select(
                            self.items[self.filtered[target]].conversation_id.clone(),
                        );
                    }
                    self.selected = target;
                    self.searching = false;
                } else {
                    self.searching = false;
                }
            }
            _ => {}
        }
        PickerAction::Continue
    }

    pub(super) fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        timezone: &str,
        has_back: bool,
        pal: &Palette,
    ) {
        let sep = pal.sep();
        let sub_tag = if self.show_subagents {
            format!(" {sep} +Sub")
        } else {
            String::new()
        };
        let live_mark = if pal.ascii { "* Live" } else { "● Live" };
        let title = format!(
            " agy-statusline conversations {sep} {}/{}{sub_tag} {sep} {} {sep} {timezone} {sep} {live_mark} ",
            self.filtered.len(),
            self.items.len(),
            self.sort.label(pal.ascii),
        );
        let block = pal.pane_block(title, true);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 || inner.width < 16 {
            self.search_bar = Rect::default();
            self.search_hits = ClickRegions::default();
            self.footer_hits = ClickRegions::default();
            self.table_hit = TableHit::default();
            self.header_hits = HeaderHits::default();
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        self.draw_search_bar(frame, chunks[0], pal);
        self.draw_list(frame, chunks[1], area, pal);
        self.footer_hits = self.draw_footer(frame, chunks[2], has_back, pal);
    }

    fn draw_search_bar(&mut self, frame: &mut Frame<'_>, area: Rect, pal: &Palette) {
        self.search_bar = area;
        self.search_hits = ClickRegions::default();
        let query_val = self.input.value();
        if self.searching {
            let prefix_w = 3u16.min(area.width);
            let input_area = Rect {
                x: area.x.saturating_add(prefix_w),
                y: area.y,
                width: area.width.saturating_sub(prefix_w),
                height: area.height,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(" / ", pal.bold(Color::Cyan))),
                Rect {
                    width: prefix_w,
                    ..area
                },
            );
            let scroll = self
                .input
                .visual_scroll(usize::from(input_area.width.saturating_sub(1)).max(1));
            frame.render_widget(
                Paragraph::new(Span::styled(query_val.to_string(), pal.bold(Color::White)))
                    .scroll((0, scroll as u16)),
                input_area,
            );
            let cursor_x = input_area
                .x
                .saturating_add(self.input.visual_cursor().saturating_sub(scroll) as u16)
                .min(area.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(cursor_x, area.y));
            return;
        }
        let mut bar = SpanBar::new(area);
        let src_badge = (self.source_filter != SourceFilter::All)
            .then(|| format!("[Src:{}]", self.source_filter.label()));
        if !query_val.is_empty() {
            let ellipsis = pal.ellipsis();
            let clear_label = "(Esc clear)";
            let reserved =
                3 + 2 + clear_label.len() + src_badge.as_ref().map_or(0, |s| 2 + s.len());
            let total_w = usize::from(area.width);
            let query_budget = total_w
                .checked_sub(reserved)
                .filter(|&b| b > 0)
                .unwrap_or_else(|| total_w.saturating_sub(3));
            let shown_query = truncate_to_width(query_val, query_budget, ellipsis);
            let _ = bar.push_click(" / ", pal.bold(Color::Yellow), PickerClick::StartSearch);
            let _ = bar.push_click(
                shown_query,
                pal.bold(Color::Yellow),
                PickerClick::StartSearch,
            );
            let _ = bar.push("  ", Style::default());
            let _ = bar.push_click(clear_label, pal.dim(), PickerClick::ClearQuery);
        } else {
            let _ = bar.push_click(
                " Press / to fuzzy-search by title, workspace, or ID",
                pal.dim(),
                PickerClick::StartSearch,
            );
        }
        if let Some(badge) = src_badge {
            let _ = bar.push("  ", Style::default());
            let _ = bar.push_click(badge, pal.bold(Color::Yellow), PickerClick::CycleSource);
        }
        let (line, hits) = bar.finish();
        self.search_hits = hits;
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_list(
        &mut self,
        frame: &mut Frame<'_>,
        list_area: Rect,
        outer_area: Rect,
        pal: &Palette,
    ) {
        if self.filtered.is_empty() {
            let msg = if self.items.is_empty() {
                "  No local conversation databases found."
            } else {
                "  No conversations match the current filter (press Esc to clear)."
            };
            frame.render_widget(Paragraph::new(Line::styled(msg, pal.dim())), list_area);
            self.table_hit = TableHit {
                pane: list_area,
                ..Default::default()
            };
            self.header_hits = HeaderHits::default();
            return;
        }

        let inner_w = usize::from(list_area.width);
        let show_full_time = inner_w >= 82;
        let show_ws = inner_w >= 72;
        let show_info = inner_w >= 58;
        let show_sub_col = inner_w >= 92;
        let show_src_col = self.has_cli && self.has_desktop && inner_w >= 98;
        let ellipsis = pal.ellipsis();
        let active_mark = if pal.ascii { "*" } else { "●" };

        let mut cols = vec![
            ConversationSortField::Modified,
            ConversationSortField::Id,
            ConversationSortField::Title,
        ];
        let mut widths = vec![
            Constraint::Length(if show_full_time { 16 } else { 11 }),
            Constraint::Length(10),
            Constraint::Min(14),
        ];
        if show_ws {
            cols.push(ConversationSortField::Workspace);
            widths.push(Constraint::Length(16));
        }
        if show_info {
            cols.push(ConversationSortField::Steps);
            widths.push(Constraint::Length(7));
        }
        if show_sub_col {
            cols.push(ConversationSortField::Subagents);
            widths.push(Constraint::Length(5));
        }
        if show_src_col {
            cols.push(ConversationSortField::Source);
            widths.push(Constraint::Length(5));
        }
        let hdr_cells: Vec<Cell<'static>> = cols
            .iter()
            .map(|&col| {
                sort_header_cell(
                    col.header_label(),
                    self.sort.field == col,
                    self.sort.dir,
                    matches!(
                        col,
                        ConversationSortField::Steps | ConversationSortField::Subagents
                    ),
                    pal,
                )
            })
            .collect();
        let chrome = TableChrome {
            highlight: true,
            header: true,
            footer: false,
            border: false,
        };
        self.header_hits = compute_header_hits(list_area, &cols, &widths, chrome, 2);

        let rows: Vec<Row<'static>> = self
            .filtered
            .iter()
            .map(|&idx| {
                let it = &self.items[idx];
                let is_active = self.active_id.as_deref() == Some(it.conversation_id.as_str());
                let is_sub = it.parent_conversation_id.is_some();
                let time_str = if it.last_modified_local.is_empty() {
                    "-".to_string()
                } else if show_full_time {
                    it.last_modified_local.clone()
                } else {
                    it.last_modified_local
                        .get(5..16)
                        .unwrap_or(&it.last_modified_local)
                        .to_string()
                };
                let short_id: String = it.conversation_id.chars().take(8).collect();
                let id_cell = if is_active {
                    Cell::from(format!("{active_mark} {short_id}")).style(pal.bold(Color::Cyan))
                } else {
                    Cell::from(format!("  {short_id}")).style(pal.dim())
                };

                let title_style = if is_active {
                    pal.bold(Color::White)
                } else if is_sub {
                    pal.fg(Color::Magenta)
                } else if it.title.is_none() {
                    pal.dim()
                } else {
                    Style::default()
                };

                let mut cells = vec![
                    Cell::from(time_str).style(pal.dim()),
                    id_cell,
                    Cell::from(format_conversation_display_title(it)).style(title_style),
                ];
                if show_ws {
                    let ws = it
                        .workspace
                        .as_deref()
                        .map(|w| truncate_to_width(w, 16, ellipsis))
                        .unwrap_or_else(|| "-".to_string());
                    cells.push(Cell::from(ws).style(pal.fg(Color::Blue)));
                }
                if show_info {
                    let info = match it.step_count {
                        Some(steps) => format!("{steps}st"),
                        None => fmt_tok(it.db_size_bytes),
                    };
                    cells.push(right_cell(info, pal.dim()));
                }
                if show_sub_col {
                    let sub_str = if it.subagent_count > 0 {
                        format!("+{}", it.subagent_count)
                    } else {
                        "-".to_string()
                    };
                    let sub_style = if it.subagent_count > 0 {
                        pal.fg(Color::Magenta)
                    } else {
                        pal.dim()
                    };
                    cells.push(right_cell(sub_str, sub_style));
                }
                if show_src_col {
                    let tag = if it.source == "desktop" { "app" } else { "cli" };
                    cells.push(Cell::from(tag).style(pal.dim()));
                }
                Row::new(cells)
            })
            .collect();

        let table = Table::new(rows, widths)
            .header(Row::new(hdr_cells).style(pal.dim()))
            .column_spacing(1)
            .row_highlight_style(pal.selected_row(true))
            .highlight_symbol(pal.highlight_symbol());

        let sb_area = Rect::new(
            outer_area.x,
            list_area.y,
            outer_area.width,
            list_area.height,
        );
        self.table_hit = render_table_with_scrollbar(
            frame,
            table,
            (list_area, sb_area),
            &mut self.table_state,
            self.selected,
            self.filtered.len(),
            chrome,
        );
    }

    fn draw_footer(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        has_back: bool,
        pal: &Palette,
    ) -> ClickRegions<PickerClick> {
        render_footer_bar(
            frame,
            area,
            self.status_msg.as_deref(),
            pal,
            |bar, sep, dim| {
                if self.searching {
                    let nav = if pal.ascii { "Up/Dn" } else { "↑↓" };
                    let _ = bar.push(format!("Type to search{sep}{nav} Select"), dim);
                    let _ =
                        bar.push_sep_click(sep, dim, "Enter Open", dim, PickerClick::OpenSelected);
                    let _ = bar.push_sep_click(sep, dim, "Esc Done", dim, PickerClick::ExitSearch);
                } else {
                    let nav = if pal.ascii { "j/k" } else { "↑↓/jk" };
                    let _ = bar.push(format!("{nav} Select"), dim);
                    let _ = bar.push_sep_click(sep, dim, "/ Search", dim, PickerClick::StartSearch);
                    let _ =
                        bar.push_sep_click(sep, dim, "Enter Open", dim, PickerClick::OpenSelected);
                    let _ =
                        bar.push_sep_click(sep, dim, "s/S Sort", dim, PickerClick::CycleSortField);
                    let _ = bar.push_sep_click(
                        sep,
                        dim,
                        "a Subagents",
                        dim,
                        PickerClick::ToggleSubagents,
                    );
                    if self.has_cli && self.has_desktop {
                        let _ =
                            bar.push_sep_click(sep, dim, "f Src", dim, PickerClick::CycleSource);
                    }
                    let _ = bar.push_sep_click(sep, dim, "r Reload", dim, PickerClick::Reload);
                    if has_back {
                        let _ = bar.push_sep_click(sep, dim, "b Back", dim, PickerClick::Back);
                    }
                    let _ = bar.push_sep_click(sep, dim, "q Quit", dim, PickerClick::Quit);
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn item(
        id: &str,
        src: &str,
        title: &str,
        ws: Option<&str>,
        parent: Option<&str>,
        agent: Option<&str>,
        ts: i64,
    ) -> ConversationListItem {
        ConversationListItem {
            conversation_id: id.to_string(),
            source: src.to_string(),
            title: Some(title.to_string()),
            workspace: ws.map(str::to_string),
            agent_name: agent.map(str::to_string),
            parent_conversation_id: parent.map(str::to_string),
            subagent_count: usize::from(parent.is_none() && id.starts_with("8d03")),
            step_count: Some(20),
            last_modified_secs: ts,
            last_modified_local: "2026-11-24 18:00".to_string(),
            db_size_bytes: 32_000,
        }
    }

    fn sample_items() -> Vec<ConversationListItem> {
        vec![
            item(
                "8d0371e1-5a05-423b-a8bc-ed75995f9a8b",
                "cli",
                "Cache Hit Investigation",
                Some("agy-statusline"),
                None,
                None,
                1_795_500_000,
            ),
            item(
                "9a1482f2-6b16-434c-99cd-fe86aa60ab9c",
                "cli",
                "Subagent Research",
                None,
                Some("8d0371e1-5a05-423b-a8bc-ed75995f9a8b"),
                Some("research"),
                1_795_499_900,
            ),
            item(
                "deadbeef-1111-4111-8111-111111111111",
                "desktop",
                "Desktop UI Polish",
                Some("frontend-app"),
                None,
                None,
                1_795_490_000,
            ),
        ]
    }

    #[test]
    fn picker_supports_fuzzy_search_tui_input_editing_and_mouse_actions() {
        let mut p = PickerScreen::new(
            sample_items(),
            Some("8d0371e1-5a05-423b-a8bc-ed75995f9a8b"),
            None,
            false,
            ConversationSort::default(),
        );
        // Subagents hidden by default -> 2 top-level rows.
        assert_eq!(p.filtered.len(), 2);

        // Toggle subagents ('a') and cycle source ('f').
        p.handle_key(key(KeyCode::Char('a')));
        assert_eq!(p.filtered.len(), 3);
        p.handle_key(key(KeyCode::Char('f')));
        assert_eq!(p.filtered.len(), 2);
        p.handle_key(key(KeyCode::Char('f')));
        assert_eq!(p.filtered.len(), 1);
        p.handle_key(key(KeyCode::Char('f')));
        assert_eq!(p.filtered.len(), 3);

        // Fuzzy query via tui-input: "frnt pol" matches "frontend-app" + "Desktop UI Polish".
        p.handle_key(key(KeyCode::Char('/')));
        assert!(p.searching);
        p.handle_key(key(KeyCode::Char('f')));
        p.handle_key(key(KeyCode::Char('r')));
        p.handle_key(key(KeyCode::Char('n')));
        p.handle_key(key(KeyCode::Char('t')));
        p.handle_key(key(KeyCode::Char(' ')));
        p.handle_key(key(KeyCode::Char('p')));
        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('l')));
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            PickerAction::Select("deadbeef-1111-4111-8111-111111111111".to_string())
        );

        // tui-input cursor editing: Left + Backspace + Esc chain.
        p.handle_key(key(KeyCode::Left));
        p.handle_key(key(KeyCode::Backspace));
        assert_eq!(p.input.value(), "frnt pl");
        assert_eq!(p.handle_key(key(KeyCode::Esc)), PickerAction::Continue);
        assert!(!p.searching);
        assert_eq!(p.handle_key(key(KeyCode::Esc)), PickerAction::Continue);
        assert!(p.input.value().is_empty());
        assert_eq!(p.handle_key(key(KeyCode::Esc)), PickerAction::Back);

        // Render and verify mouse wheel + click-to-select + click-selected-to-open.
        let pal = Palette {
            no_color: true,
            ascii: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", true, &pal))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            rendered.contains(
                "agy-statusline conversations · 3/3 · +Sub · Modified ↓ · UTC+08:00 · ● Live"
            ),
            "{rendered}"
        );
        assert!(rendered.contains("b Back"));

        let m = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        p.handle_mouse(m(MouseEventKind::ScrollDown, 20, 10), true);
        assert_eq!(p.selected, 2);
        p.handle_mouse(m(MouseEventKind::ScrollUp, 20, 10), true);
        assert_eq!(p.selected, 0);

        let row1_y = p.table_hit.data_y0 + 1;
        let list_x = p.table_hit.pane.x + 2;
        assert_eq!(
            p.handle_mouse(
                m(MouseEventKind::Down(MouseButton::Left), list_x, row1_y),
                true
            ),
            PickerAction::Continue
        );
        assert_eq!(p.selected, 1);
        assert_eq!(
            p.handle_mouse(
                m(MouseEventKind::Down(MouseButton::Left), list_x, row1_y),
                true
            ),
            PickerAction::Select("9a1482f2-6b16-434c-99cd-fe86aa60ab9c".to_string())
        );

        // Clicking Title header sorts alphabetically by title and toggles asc/desc on repeat click.
        let title_hit = p
            .header_hits
            .cols
            .iter()
            .find(|c| c.col == ConversationSortField::Title)
            .copied()
            .unwrap();
        let title_hdr: String = (title_hit.x0..title_hit.x1)
            .map(|x| terminal.backend().buffer()[(x, p.header_hits.y)].symbol())
            .collect();
        assert!(title_hdr.contains("Title"), "{title_hdr:?}");
        p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                title_hit.x0 + 1,
                p.header_hits.y,
            ),
            true,
        );
        assert_eq!(
            p.sort,
            ConversationSort {
                field: ConversationSortField::Title,
                dir: SortDir::Asc,
            }
        );
        assert_eq!(
            p.items[p.filtered[0]].title.as_deref(),
            Some("Cache Hit Investigation")
        );
        p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                title_hit.x0 + 1,
                p.header_hits.y,
            ),
            true,
        );
        assert_eq!(
            p.sort,
            ConversationSort {
                field: ConversationSortField::Title,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            p.items[p.filtered[0]].title.as_deref(),
            Some("Subagent Research")
        );

        // 's' cycles sort columns across visible headers (Title -> Workspace), then click restores Title ↓.
        p.handle_key(key(KeyCode::Char('s')));
        assert_eq!(
            p.sort,
            ConversationSort {
                field: ConversationSortField::Workspace,
                dir: SortDir::Asc,
            }
        );

        // Live refresh preserves highlighted conversation and active search mode.
        p.selected = 1;
        let pinned_id = p.items[p.filtered[1]].conversation_id.clone();
        p.handle_key(key(KeyCode::Char('/')));
        let mut expanded = sample_items();
        expanded.push(item(
            "aaaa0000-1111-4111-8111-111111111111",
            "cli",
            "Zebra Live Session",
            Some("ws"),
            None,
            None,
            1_795_600_000,
        ));
        p.refresh_items_live(expanded);
        assert!(p.searching);
        assert_eq!(p.items[p.filtered[p.selected]].conversation_id, pinned_id);

        // Search-mode footer uses pure ASCII labels under --ascii.
        let ascii_pal = Palette {
            no_color: true,
            ascii: true,
        };
        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", true, &ascii_pal))
            .unwrap();
        let footer_row: String = (1..119)
            .map(|x| terminal.backend().buffer()[(x, 30)].symbol())
            .collect();
        assert!(footer_row.contains("Up/Dn Select"), "{footer_row}");
        assert!(footer_row.is_ascii(), "{footer_row}");
    }

    #[test]
    fn picker_mouse_clicks_search_badges_and_footer_actions() {
        let m = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let rect_text = |buf: &ratatui::buffer::Buffer, r: Rect| -> String {
            (r.x..r.right()).map(|x| buf[(x, r.y)].symbol()).collect()
        };
        let mut p = PickerScreen::new(
            sample_items(),
            None,
            Some("Cache"),
            false,
            ConversationSort::default(),
        );
        let pal = Palette {
            no_color: true,
            ascii: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", true, &pal))
            .unwrap();
        assert_eq!(p.filtered.len(), 1);

        // Clicking "(Esc clear)" in search bar clears the active query.
        let clear_rect = p
            .search_hits
            .find(|c| *c == PickerClick::ClearQuery)
            .unwrap();
        assert_eq!(
            rect_text(terminal.backend().buffer(), clear_rect),
            "(Esc clear)"
        );
        assert_eq!(
            p.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    clear_rect.x + 1,
                    clear_rect.y
                ),
                true,
            ),
            PickerAction::Continue
        );
        assert_eq!(p.input.value(), "");
        assert_eq!(p.filtered.len(), 2);

        // Footer toggles: Subagents and Source filter.
        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", true, &pal))
            .unwrap();
        let sub_btn = p
            .footer_hits
            .find(|c| *c == PickerClick::ToggleSubagents)
            .unwrap();
        p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                sub_btn.x + 1,
                sub_btn.y,
            ),
            true,
        );
        assert!(p.show_subagents);
        assert_eq!(p.filtered.len(), 3);

        let src_btn = p
            .footer_hits
            .find(|c| *c == PickerClick::CycleSource)
            .unwrap();
        p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                src_btn.x + 1,
                src_btn.y,
            ),
            true,
        );
        assert_eq!(p.source_filter, SourceFilter::Cli);
        assert_eq!(p.filtered.len(), 2);

        // Clicking "[Src:CLI]" badge in search bar cycles to Desktop.
        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", true, &pal))
            .unwrap();
        let src_badge = p
            .search_hits
            .find(|c| *c == PickerClick::CycleSource)
            .unwrap();
        assert_eq!(
            rect_text(terminal.backend().buffer(), src_badge),
            "[Src:CLI]"
        );
        p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                src_badge.x + 1,
                src_badge.y,
            ),
            true,
        );
        assert_eq!(p.source_filter, SourceFilter::Desktop);
        assert_eq!(p.filtered.len(), 1);

        // Footer "Enter Open" opens selected Desktop conversation.
        let open_btn = p
            .footer_hits
            .find(|c| *c == PickerClick::OpenSelected)
            .unwrap();
        assert_eq!(
            p.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    open_btn.x + 1,
                    open_btn.y
                ),
                true,
            ),
            PickerAction::Select("deadbeef-1111-4111-8111-111111111111".to_string())
        );

        // Footer "/ Search" enters search mode; "Esc Done" exits search mode.
        let search_btn = p
            .footer_hits
            .find(|c| *c == PickerClick::StartSearch)
            .unwrap();
        p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                search_btn.x + 1,
                search_btn.y,
            ),
            true,
        );
        assert!(p.searching);

        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", true, &pal))
            .unwrap();
        let done_btn = p
            .footer_hits
            .find(|c| *c == PickerClick::ExitSearch)
            .unwrap();
        assert_eq!(rect_text(terminal.backend().buffer(), done_btn), "Esc Done");
        p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                done_btn.x + 1,
                done_btn.y,
            ),
            true,
        );
        assert!(!p.searching);

        // Footer Reload, Back, Quit; and Back hidden when has_back is false.
        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", true, &pal))
            .unwrap();
        let reload_btn = p.footer_hits.find(|c| *c == PickerClick::Reload).unwrap();
        assert_eq!(
            p.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    reload_btn.x + 1,
                    reload_btn.y,
                ),
                true,
            ),
            PickerAction::Reload
        );
        let back_btn = p.footer_hits.find(|c| *c == PickerClick::Back).unwrap();
        assert_eq!(rect_text(terminal.backend().buffer(), back_btn), "b Back");
        assert_eq!(
            p.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    back_btn.x + 1,
                    back_btn.y
                ),
                true,
            ),
            PickerAction::Back
        );
        let quit_btn = p.footer_hits.find(|c| *c == PickerClick::Quit).unwrap();
        assert_eq!(
            p.handle_mouse(
                m(
                    MouseEventKind::Down(MouseButton::Left),
                    quit_btn.x + 1,
                    quit_btn.y
                ),
                true,
            ),
            PickerAction::Quit
        );

        terminal
            .draw(|frame| p.draw(frame, frame.area(), "UTC+08:00", false, &pal))
            .unwrap();
        assert!(p.footer_hits.find(|c| *c == PickerClick::Back).is_none());

        // Long query on a narrow viewport truncates cleanly so both query and "(Esc clear)" stay visible/clickable.
        let mut narrow_p = PickerScreen::new(
            sample_items(),
            None,
            Some("super-long-search-query-that-exceeds-narrow-terminal-width-by-far"),
            false,
            ConversationSort::default(),
        );
        let mut narrow_term = Terminal::new(TestBackend::new(40, 16)).unwrap();
        narrow_term
            .draw(|frame| narrow_p.draw(frame, frame.area(), "UTC+08:00", false, &pal))
            .unwrap();
        let search_line = rect_text(narrow_term.backend().buffer(), narrow_p.search_bar);
        assert!(search_line.contains("super-long"), "{search_line}");
        assert!(search_line.contains("(Esc clear)"), "{search_line}");
        let narrow_clear = narrow_p
            .search_hits
            .find(|c| *c == PickerClick::ClearQuery)
            .unwrap();
        narrow_p.handle_mouse(
            m(
                MouseEventKind::Down(MouseButton::Left),
                narrow_clear.x + 1,
                narrow_clear.y,
            ),
            false,
        );
        assert_eq!(narrow_p.input.value(), "");
    }
}
