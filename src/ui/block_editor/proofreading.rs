//! Spelling and grammar in the block editor.
//!
//! Two independent layers, both optional: the offline speller
//! ([`crate::ui::spellcheck`]) and LanguageTool, whose results come back from
//! the runtime. `LanguageToolCoverage` decides whether the grammar layer also
//! replaces the speller, so both schedulers below consult the settings before
//! running. Issues become highlight runs on each block input, and the fix-menu
//! entries are the actions handled here.

use super::*;
pub(super) struct SpellingResult {
    source: String,
    issues: Vec<spellcheck::Issue>,
}

pub(super) struct LanguageToolResult {
    source: String,
    issues: Vec<ProofreadingIssue>,
}

pub(super) static LANGUAGE_ISSUE_CACHE: OnceLock<
    Mutex<std::collections::HashMap<String, Vec<ProofreadingIssue>>>,
> = OnceLock::new();
pub(super) static LANGUAGE_SUPPRESSED_HUNSPELL: OnceLock<
    RwLock<std::collections::HashSet<String>>,
> = OnceLock::new();
pub(super) static IGNORED_LANGUAGE_RULES: OnceLock<RwLock<std::collections::HashSet<String>>> =
    OnceLock::new();

pub(super) fn ignored_language_rules() -> &'static RwLock<std::collections::HashSet<String>> {
    IGNORED_LANGUAGE_RULES.get_or_init(Default::default)
}

pub(super) fn cache_language_issues(source: &str, issues: &[ProofreadingIssue]) {
    let cache = LANGUAGE_ISSUE_CACHE.get_or_init(Default::default);
    let Ok(mut cache) = cache.lock() else { return };
    if cache.len() >= 512 {
        cache.clear();
    }
    cache.insert(source.to_string(), issues.to_vec());
}

pub(super) fn set_hunspell_suppressed(source: &str, suppressed: bool) {
    let cache = LANGUAGE_SUPPRESSED_HUNSPELL.get_or_init(Default::default);
    let Ok(mut cache) = cache.write() else { return };
    if suppressed {
        if cache.len() >= 512 {
            cache.clear();
        }
        cache.insert(source.to_string());
    } else {
        cache.remove(source);
    }
}

pub(super) fn hunspell_is_suppressed(source: &str) -> bool {
    LANGUAGE_SUPPRESSED_HUNSPELL
        .get_or_init(Default::default)
        .read()
        .is_ok_and(|sources| sources.contains(source))
}

pub(super) fn language_issue_at(value: &str, offset: usize) -> Option<ProofreadingIssue> {
    let cache = LANGUAGE_ISSUE_CACHE
        .get_or_init(Default::default)
        .lock()
        .ok()?;
    let ignored = ignored_language_rules().read().ok();
    cache.get(value)?.iter().find_map(|issue| {
        let contains = issue.range.contains(&offset)
            || (offset == issue.range.end && issue.range.start < issue.range.end);
        let ignored = ignored
            .as_ref()
            .is_some_and(|rules| rules.contains(&issue.rule_id));
        let accepted = issue.is_spelling()
            && value
                .get(issue.range.clone())
                .is_some_and(spellcheck::word_is_accepted);
        (contains && !ignored && !accepted).then(|| issue.clone())
    })
}

pub(super) fn should_use_hunspell(
    settings: &LanguageToolSettings,
    has_current_result: bool,
) -> bool {
    settings.mode == LanguageToolMode::Disabled
        || settings.coverage == LanguageToolCoverage::GrammarOnly
        || !has_current_result
}

pub(super) fn languagetool_result_is_current(
    expected_revision: Option<u64>,
    revision: u64,
    current_source: &str,
    response_source: &str,
) -> bool {
    expected_revision == Some(revision) && current_source == response_source
}

impl BlockEditor {
    /// Connects the editor to the background runtime. Required by grammar
    /// checks and by the download of pasted remote images; an editor without it
    /// keeps working, minus those two.
    pub(crate) fn with_runtime(mut self, tx: mpsc::UnboundedSender<Cmd>) -> Self {
        self.runtime_tx = Some(tx);
        self
    }

    pub(crate) fn with_proofreading(mut self, settings: LanguageToolSettings) -> Self {
        self.languagetool_settings = settings;
        self
    }

    pub(crate) fn update_proofreading_settings(
        &mut self,
        settings: LanguageToolSettings,
        cx: &mut Context<Self>,
    ) {
        self.languagetool_settings = settings;
        if let Ok(mut sources) = LANGUAGE_SUPPRESSED_HUNSPELL
            .get_or_init(Default::default)
            .write()
        {
            sources.clear();
        }
        self.languagetool_results.clear();
        self.languagetool_tasks.clear();
        self.languagetool_failures.clear();
        for revision in self.languagetool_revisions.values_mut() {
            *revision = revision.saturating_add(1);
        }
        if self.languagetool_settings.automatic_check {
            self.check_all_now(cx);
        } else {
            self.refresh_spellchecks(cx);
        }
    }

    pub(crate) fn check_all_now(&mut self, cx: &mut Context<Self>) {
        self.refresh_spellchecks(cx);
        for (block_id, input) in self.proofreading_inputs() {
            self.schedule_languagetool(block_id, input, std::time::Duration::ZERO, cx);
        }
    }

    pub(crate) fn apply_languagetool_result(
        &mut self,
        editor_id: &str,
        block_id: u64,
        revision: u64,
        source: String,
        mut issues: Vec<ProofreadingIssue>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.scope != editor_id {
            return false;
        }
        if self.languagetool_settings.mode == LanguageToolMode::Disabled {
            return true;
        }
        let Some((_, input)) = self
            .proofreading_inputs()
            .into_iter()
            .find(|(candidate, _)| *candidate == block_id)
        else {
            return true;
        };
        let input_id = input.entity_id();
        if !languagetool_result_is_current(
            self.languagetool_revisions.get(&input_id).copied(),
            revision,
            input.read(cx).value().as_ref(),
            &source,
        ) {
            return true;
        }
        let ignored = ignored_language_rules().read().ok();
        issues.retain(|issue| {
            if ignored
                .as_ref()
                .is_some_and(|rules| rules.contains(&issue.rule_id))
            {
                return false;
            }
            if !issue.is_spelling() {
                return true;
            }
            source
                .get(issue.range.clone())
                .is_none_or(|word| !spellcheck::word_is_accepted(word))
        });
        cache_language_issues(&source, &issues);
        set_hunspell_suppressed(
            &source,
            self.languagetool_settings.coverage == LanguageToolCoverage::SpellingAndGrammar,
        );
        self.languagetool_results
            .insert(input_id, LanguageToolResult { source, issues });
        self.languagetool_tasks.remove(&input_id);
        self.languagetool_failures.remove(&input_id);
        self.apply_input_highlights(&input, cx);
        cx.notify();
        true
    }

    pub(crate) fn apply_languagetool_failure(
        &mut self,
        editor_id: &str,
        block_id: u64,
        revision: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.scope != editor_id {
            return false;
        }
        if let Some((_, input)) = self
            .proofreading_inputs()
            .into_iter()
            .find(|(candidate, _)| *candidate == block_id)
        {
            let input_id = input.entity_id();
            if self.languagetool_revisions.get(&input_id).copied() == Some(revision) {
                let source = input.read(cx).value().to_string();
                set_hunspell_suppressed(&source, false);
                self.languagetool_results.remove(&input_id);
                self.languagetool_tasks.remove(&input_id);
                self.languagetool_failures.insert(input_id, source);
                self.apply_input_highlights(&input, cx);
            }
        }
        true
    }

    pub(crate) fn retry_languagetool_failures(&mut self, cx: &mut Context<Self>) {
        if self.languagetool_failures.is_empty() {
            return;
        }
        self.languagetool_failures.clear();
        if self.languagetool_settings.automatic_check {
            self.check_all_now(cx);
        }
    }

    pub(super) fn on_apply_spelling_suggestion(
        &mut self,
        action: &ApplySpellingSuggestion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let Some(input) = self
            .all_inputs()
            .into_iter()
            .find(|input| input.entity_id() == action.input_id)
        else {
            return;
        };
        let mut value = input.read(cx).value().to_string();
        if action.range.end > value.len()
            || !value.is_char_boundary(action.range.start)
            || !value.is_char_boundary(action.range.end)
            || value.get(action.range.clone()) != Some(action.original.as_str())
        {
            return;
        }
        self.push_undo(cx);
        value.replace_range(action.range.clone(), &action.replacement);
        let cursor = action.range.start + action.replacement.len();
        input.update(cx, |state, cx| {
            state.set_value(value, window, cx);
            state.set_cursor_offset(cursor, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn on_ignore_spelling(
        &mut self,
        action: &IgnoreSpelling,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        spellcheck::ignore_for_session(&action.word);
        self.refresh_spellchecks(cx);
    }

    pub(super) fn on_add_spelling_to_dictionary(
        &mut self,
        action: &AddSpellingToDictionary,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        spellcheck::add_to_personal_dictionary(&action.word);
        self.refresh_spellchecks(cx);
    }

    pub(super) fn on_ignore_proofreading_rule(
        &mut self,
        action: &IgnoreProofreadingRule,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if let Ok(mut rules) = ignored_language_rules().write() {
            rules.insert(action.rule_id.clone());
        }
        for result in self.languagetool_results.values_mut() {
            result
                .issues
                .retain(|issue| issue.rule_id != action.rule_id);
            cache_language_issues(&result.source, &result.issues);
        }
        for (_, input) in self.proofreading_inputs() {
            self.apply_input_highlights(&input, cx);
        }
        cx.notify();
    }

    pub(super) fn proofreading_inputs(&self) -> Vec<(u64, Entity<InputState>)> {
        let mut inputs = Vec::new();
        for block in &self.blocks {
            match &block.kind {
                EbKind::Text(text) if text.style != TextStyle::Code => {
                    inputs.push((block.id << 32, text.input.clone()));
                }
                EbKind::List(list) => {
                    inputs.extend(list.rows.iter().enumerate().map(|(index, row)| {
                        ((block.id << 32) | (index as u64 + 1), row.input.clone())
                    }));
                }
                EbKind::Table(table) => {
                    inputs.extend(table.rows.iter().enumerate().flat_map(|(row, cells)| {
                        cells.iter().enumerate().map(move |(column, cell)| {
                            (
                                (block.id << 32) | 0x10_0000 | ((row as u64) << 10) | column as u64,
                                cell.input.clone(),
                            )
                        })
                    }));
                }
                EbKind::Text(_)
                | EbKind::Image { .. }
                | EbKind::Divider
                | EbKind::Original { .. } => {}
            }
        }
        inputs
    }

    pub(super) fn spellcheck_inputs(&self) -> Vec<Entity<InputState>> {
        self.proofreading_inputs()
            .into_iter()
            .map(|(_, input)| input)
            .collect()
    }

    pub(super) fn input_highlights(
        &self,
        input: &Entity<InputState>,
        cx: &App,
    ) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
        let value = input.read(cx).value();
        let mut highlights = inline_format_highlights(&value, InlineColors::from_theme(cx.theme()));
        if let Some(address_book) = &self.mention_address_book {
            highlights.extend(
                address_book
                    .mention_ranges(&value)
                    .into_iter()
                    .map(|range| {
                        (
                            range,
                            HighlightStyle {
                                color: Some(cx.theme().accent),
                                font_weight: Some(FontWeight::SEMIBOLD),
                                ..Default::default()
                            },
                        )
                    }),
            );
        }
        let language_result = self
            .languagetool_results
            .get(&input.entity_id())
            .filter(|result| result.source == value.as_ref());
        let use_hunspell =
            should_use_hunspell(&self.languagetool_settings, language_result.is_some());
        let spelling_style = HighlightStyle {
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(gpui::rgb(0xdc_26_26).into()),
                wavy: true,
            }),
            ..Default::default()
        };
        if use_hunspell {
            if let Some(result) = self
                .spelling
                .get(&input.entity_id())
                .filter(|result| result.source == value.as_ref())
            {
                highlights.extend(
                    result
                        .issues
                        .iter()
                        .map(|issue| (issue.range.clone(), spelling_style)),
                );
            }
        }
        if let Some(result) = language_result {
            let ignored = ignored_language_rules().read().ok();
            highlights.extend(result.issues.iter().filter_map(|issue| {
                if ignored
                    .as_ref()
                    .is_some_and(|rules| rules.contains(&issue.rule_id))
                    || (issue.is_spelling()
                        && value
                            .get(issue.range.clone())
                            .is_some_and(spellcheck::word_is_accepted))
                {
                    return None;
                }
                let color = match issue.category {
                    ProofreadingCategory::Spelling => gpui::rgb(0xdc_26_26),
                    ProofreadingCategory::Grammar => gpui::rgb(0x25_63_eb),
                    ProofreadingCategory::Typography | ProofreadingCategory::Style => {
                        gpui::rgb(0xd9_77_06)
                    }
                };
                Some((
                    issue.range.clone(),
                    HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(color.into()),
                            wavy: true,
                        }),
                        ..Default::default()
                    },
                ))
            }));
        }
        highlights
    }

    pub(super) fn apply_input_highlights(
        &self,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) {
        let highlights = self.input_highlights(input, cx);
        // Folds travel with the highlights: both are derived from the same text,
        // and refreshing one without the other would leave the input hiding
        // ranges that no longer match what it holds.
        let folds = super::links::foldable_ranges(input.read(cx).text());
        input.update(cx, |state, cx| {
            state.set_text_highlights(highlights, cx);
            state.set_foldable_ranges(folds, cx);
        });
    }

    pub(super) fn schedule_spellcheck(
        &mut self,
        input: Entity<InputState>,
        delay: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        let input_id = input.entity_id();
        let source = input.read(cx).value().to_string();
        self.spelling.remove(&input_id);
        self.apply_input_highlights(&input, cx);
        let task_source = source.clone();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let check_source = task_source.clone();
            let issues = cx
                .background_spawn(async move { spellcheck::check_text(&check_source) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if input.read(cx).value().as_ref() != task_source {
                    return;
                }
                this.spelling.insert(
                    input_id,
                    SpellingResult {
                        source: task_source,
                        issues,
                    },
                );
                this.apply_input_highlights(&input, cx);
            });
        });
        self.spelling_tasks.insert(input_id, task);
    }

    pub(super) fn schedule_languagetool(
        &mut self,
        block_id: u64,
        input: Entity<InputState>,
        delay: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        if self.languagetool_settings.mode == LanguageToolMode::Disabled {
            return;
        }
        let Some(tx) = self.runtime_tx.clone() else {
            return;
        };
        let input_id = input.entity_id();
        let source = input.read(cx).value().to_string();
        let revision = self
            .languagetool_revisions
            .entry(input_id)
            .and_modify(|revision| *revision = revision.saturating_add(1))
            .or_insert(1)
            .to_owned();
        self.languagetool_results.remove(&input_id);
        self.languagetool_failures.remove(&input_id);
        self.apply_input_highlights(&input, cx);
        let editor_id = self.scope.clone();
        let task = cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = tx.send(Cmd::CheckLanguageTool {
                editor_id,
                block_id,
                revision,
                text: source,
                ui_language: rust_i18n::locale().to_string(),
            });
        });
        self.languagetool_tasks.insert(input_id, task);
    }

    pub(super) fn ensure_spellchecks(&mut self, cx: &mut Context<Self>) {
        let inputs = self.spellcheck_inputs();
        let live_ids: std::collections::HashSet<_> = inputs.iter().map(Entity::entity_id).collect();
        self.spelling.retain(|id, _| live_ids.contains(id));
        self.spelling_tasks.retain(|id, _| live_ids.contains(id));
        self.languagetool_results
            .retain(|id, _| live_ids.contains(id));
        self.languagetool_tasks
            .retain(|id, _| live_ids.contains(id));
        self.languagetool_revisions
            .retain(|id, _| live_ids.contains(id));
        self.languagetool_failures
            .retain(|id, _| live_ids.contains(id));
        for input in inputs {
            let input_id = input.entity_id();
            let source = input.read(cx).value();
            let current = self
                .spelling
                .get(&input_id)
                .is_some_and(|result| result.source == source.as_ref());
            if !current && !self.spelling_tasks.contains_key(&input_id) {
                self.schedule_spellcheck(input, std::time::Duration::ZERO, cx);
            }
        }
        if self.languagetool_settings.automatic_check
            && self.languagetool_settings.mode != LanguageToolMode::Disabled
        {
            for (block_id, input) in self.proofreading_inputs() {
                let input_id = input.entity_id();
                let source = input.read(cx).value();
                let current = self
                    .languagetool_results
                    .get(&input_id)
                    .is_some_and(|result| result.source == source.as_ref());
                let failed = self
                    .languagetool_failures
                    .get(&input_id)
                    .is_some_and(|failed_source| failed_source == source.as_ref());
                if !current && !failed && !self.languagetool_tasks.contains_key(&input_id) {
                    self.schedule_languagetool(
                        block_id,
                        input,
                        std::time::Duration::from_millis(700),
                        cx,
                    );
                }
            }
        }
    }

    pub(super) fn refresh_spellchecks(&mut self, cx: &mut Context<Self>) {
        self.spelling.clear();
        for input in self.spellcheck_inputs() {
            self.schedule_spellcheck(input, std::time::Duration::ZERO, cx);
        }
    }
}
