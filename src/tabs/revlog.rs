use crate::{
    components::{
        async_commit_filter::{
            AsyncCommitFilterer, FilterBy, FilterStatus,
        },
        visibility_blocking, CommandBlocking, CommandInfo,
        CommitDetailsComponent, CommitList, Component,
        DrawableComponent, EventState, FindCommitComponent,
    },
    keys::SharedKeyConfig,
    queue::{InternalEvent, Queue},
    strings,
    ui::style::SharedTheme,
};
use anyhow::Result;
use asyncgit::{
    cached,
    sync::{self, CommitId},
    AsyncLog, AsyncNotification, AsyncTags, FetchStatus, CWD,
};
use crossbeam_channel::Sender;
use crossterm::event::Event;
use std::convert::TryFrom;
use std::time::Duration;
use sync::CommitTags;
use tui::{
    backend::Backend,
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

const SLICE_SIZE: usize = 1200;

///
pub struct Revlog {
    commit_details: CommitDetailsComponent,
    list: CommitList,
    find_commit: FindCommitComponent,
    async_filter: AsyncCommitFilterer,
    git_log: AsyncLog,
    git_tags: AsyncTags,
    queue: Queue,
    visible: bool,
    branch_name: cached::BranchName,
    key_config: SharedKeyConfig,
    is_filtering: bool,
    filter_string: String,
}

impl Revlog {
    ///
    pub fn new(
        queue: &Queue,
        sender: &Sender<AsyncNotification>,
        theme: SharedTheme,
        key_config: SharedKeyConfig,
    ) -> Self {
        let log = AsyncLog::new(sender);
        let tags = AsyncTags::new(sender);
        Self {
            queue: queue.clone(),
            commit_details: CommitDetailsComponent::new(
                queue,
                sender,
                theme.clone(),
                key_config.clone(),
            ),
            list: CommitList::new(
                &strings::log_title(&key_config),
                theme.clone(),
                key_config.clone(),
            ),
            find_commit: FindCommitComponent::new(
                queue.clone(),
                theme,
                key_config.clone(),
            ),
            async_filter: AsyncCommitFilterer::new(
                log.clone(),
                tags.clone(),
                sender,
            ),
            git_log: log,
            git_tags: tags,
            visible: false,
            branch_name: cached::BranchName::new(CWD),
            key_config,
            is_filtering: false,
            filter_string: "".to_string(),
        }
    }

    ///
    pub fn any_work_pending(&self) -> bool {
        self.git_log.is_pending()
            || self.git_tags.is_pending()
            || self.async_filter.is_pending()
            || self.commit_details.any_work_pending()
    }

    ///
    pub fn update(&mut self) -> Result<()> {
        if self.is_visible() {
            let log_changed = if self.is_filtering {
                self.list.set_total_count(self.async_filter.count());
                self.async_filter.fetch() == FilterStatus::Filtering
            } else {
                self.list.set_total_count(self.git_log.count()?);
                self.git_log.fetch()? == FetchStatus::Started
            };

            let selection = self.list.selection();
            let selection_max = self.list.selection_max();
            if self.list.items().needs_data(selection, selection_max)
                || log_changed
            {
                self.fetch_commits()?;
            }

            self.git_tags.request(Duration::from_secs(3), false)?;

            self.list.set_branch(
                self.branch_name.lookup().map(Some).unwrap_or(None),
            );

            if self.commit_details.is_visible() {
                let commit = self.selected_commit();
                let tags = self.selected_commit_tags(&commit);

                self.commit_details.set_commit(commit, tags)?;
            }
        }

        Ok(())
    }

    ///
    pub fn update_git(
        &mut self,
        ev: AsyncNotification,
    ) -> Result<()> {
        if self.visible {
            match ev {
                AsyncNotification::CommitFiles
                | AsyncNotification::Log => self.update()?,
                AsyncNotification::Tags => {
                    if let Some(tags) = self.git_tags.last()? {
                        self.list.set_tags(tags);
                        self.update()?;
                    }
                }
                _ => (),
            }
        }

        Ok(())
    }

    fn fetch_commits(&mut self) -> Result<()> {
        let want_min =
            self.list.selection().saturating_sub(SLICE_SIZE / 2);

        let commits = if self.is_filtering {
            self.async_filter
                .get_filter_items(
                    want_min,
                    SLICE_SIZE,
                    self.list.current_size().0.into(),
                )
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        } else {
            sync::get_commits_info(
                CWD,
                &self.git_log.get_slice(want_min, SLICE_SIZE)?,
                self.list.current_size().0.into(),
            )
            .map_err(|e| anyhow::anyhow!(e.to_string()))
        };

        if let Ok(commits) = commits {
            self.list.items().set_items(want_min, commits);
        }

        Ok(())
    }

    fn selected_commit(&self) -> Option<CommitId> {
        self.list.selected_entry().map(|e| e.id)
    }

    fn copy_commit_hash(&self) -> Result<()> {
        self.list.copy_entry_hash()?;
        Ok(())
    }

    fn selected_commit_tags(
        &self,
        commit: &Option<CommitId>,
    ) -> Option<CommitTags> {
        let tags = self.list.tags();

        commit.and_then(|commit| {
            tags.and_then(|tags| tags.get(&commit).cloned())
        })
    }

    /// Parses search string into individual sub-searches.
    /// Each sub-search is a tuple of (string-to-search, flags-where-to-search)
    ///
    /// Returns vec of vec of sub-searches.
    /// Where search results:
    ///   1. from outer vec should be combined via 'disjunction' (or);
    ///   2. from inter vec should be combined via 'conjunction' (and).
    ///
    /// Currently parentheses in the `filter_by_str` are not supported.
    /// They should be removed by `Self::pre_process_string`.
    fn get_what_to_filter_by(
        filter_by_str: &str,
    ) -> Vec<Vec<(String, FilterBy)>> {
        let mut search_vec = Vec::new();
        let mut and_vec = Vec::new();
        for or in filter_by_str.split("||") {
            for split_sub in or.split("&&").map(str::trim) {
                if !split_sub.starts_with(':') {
                    and_vec.push((
                        split_sub.to_string(),
                        FilterBy::everywhere(),
                    ));
                    continue;
                }

                let mut split_str = split_sub.splitn(2, ' ');
                let first = split_str
                    .next()
                    .expect("Split must return at least one element");
                let mut to_filter_by = first.chars().skip(1).fold(
                    FilterBy::empty(),
                    |acc, ch| {
                        acc | FilterBy::try_from(ch)
                            .unwrap_or_else(|_| FilterBy::empty())
                    },
                );

                if to_filter_by.exclude_modifiers().is_empty() {
                    to_filter_by |= FilterBy::everywhere();
                }

                and_vec.push((
                    split_str
                        .next()
                        .unwrap_or("")
                        .trim_start()
                        .to_string(),
                    to_filter_by,
                ));
            }
            search_vec.push(and_vec.clone());
            and_vec.clear();
        }
        search_vec
    }

    pub fn filter(&mut self, filter_by: &str) -> Result<()> {
        if filter_by != self.filter_string {
            self.filter_string = filter_by.to_string();
            let pre_processed_string =
                Self::pre_process_string(filter_by.to_string());
            let trimmed_string =
                pre_processed_string.trim().to_string();
            if filter_by.is_empty() {
                self.async_filter.stop_filter();
                self.is_filtering = false;
            } else {
                let filter_strings =
                    Self::get_what_to_filter_by(&trimmed_string);
                self.async_filter
                    .start_filter(filter_strings)
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                self.is_filtering = true;
            }
            return self.update();
        }
        Ok(())
    }

    /// pre process string to remove any brackets
    pub fn pre_process_string(mut s: String) -> String {
        while s.contains("&&(") {
            let before = s.clone();
            s = Self::remove_out_brackets(&s);
            if s == before {
                break;
            }
        }
        s
    }

    /// Remove the brakcets, replacing them with the unbracketed 'full' expression
    pub fn remove_out_brackets(s: &str) -> String {
        if let Some(first_bracket) = s.find("&&(") {
            let (first, rest_of_string) =
                s.split_at(first_bracket + 3);
            if let Some(last_bracket) =
                Self::get_ending_bracket(rest_of_string)
            {
                let mut v = vec![];
                let (second, third) =
                    rest_of_string.split_at(last_bracket);
                if let Some((first, third)) = first
                    .strip_suffix('(')
                    .zip(third.strip_prefix(')'))
                {
                    for inside_bracket_item in second.split("||") {
                        // Append first, prepend third onto bracket element
                        v.push(format!(
                            "{}{}{}",
                            first, inside_bracket_item, third
                        ));
                    }
                    return v.join("||");
                }
            }
        }
        s.to_string()
    }

    /// Get outer matching brakets in a string
    pub fn get_ending_bracket(s: &str) -> Option<usize> {
        let mut brack_count = 0;
        let mut ending_brakcet_pos = None;
        for (i, c) in s.chars().enumerate() {
            if c == '(' {
                brack_count += 1;
            } else if c == ')' {
                if brack_count == 0 {
                    // Found
                    ending_brakcet_pos = Some(i);
                    break;
                }
                brack_count -= 1;
            }
        }
        ending_brakcet_pos
    }
}

impl DrawableComponent for Revlog {
    fn draw<B: Backend>(
        &self,
        f: &mut Frame<B>,
        area: Rect,
    ) -> Result<()> {
        if self.commit_details.is_visible() {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(
                    [
                        Constraint::Percentage(60),
                        Constraint::Percentage(40),
                    ]
                    .as_ref(),
                )
                .split(area);

            if self.find_commit.is_visible() {
                let log_find_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(
                        [
                            Constraint::Percentage(90),
                            Constraint::Percentage(20),
                        ]
                        .as_ref(),
                    )
                    .split(chunks[0]);
                self.list.draw(f, log_find_chunks[0])?;
                self.find_commit.draw(f, log_find_chunks[1])?;
                self.commit_details.draw(f, chunks[1])?;
            } else {
                self.list.draw(f, chunks[0])?;
                self.commit_details.draw(f, chunks[1])?;
            }
        } else if self.find_commit.is_visible() {
            let log_find_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(
                    [
                        Constraint::Percentage(90),
                        Constraint::Percentage(20),
                    ]
                    .as_ref(),
                )
                .split(area);
            self.list.draw(f, log_find_chunks[0])?;
            self.find_commit.draw(f, log_find_chunks[1])?;
        } else {
            self.list.draw(f, area)?;
        }

        Ok(())
    }
}

impl Component for Revlog {
    fn event(&mut self, ev: Event) -> Result<EventState> {
        if self.visible {
            let mut event_used = self.find_commit.event(ev)?;
            if !event_used.is_consumed() {
                event_used = self.list.event(ev)?;
            }

            if event_used.is_consumed() {
                self.update()?;
                return Ok(EventState::Consumed);
            } else if let Event::Key(k) = ev {
                if k == self.key_config.enter {
                    self.commit_details.toggle_visible()?;
                    self.update()?;
                    return Ok(EventState::Consumed);
                } else if k == self.key_config.copy {
                    self.copy_commit_hash()?;
                    return Ok(EventState::Consumed);
                } else if k == self.key_config.push {
                    self.queue
                        .borrow_mut()
                        .push_back(InternalEvent::PushTags);
                    return Ok(EventState::Consumed);
                } else if k == self.key_config.log_tag_commit {
                    return self.selected_commit().map_or(
                        Ok(EventState::NotConsumed),
                        |id| {
                            self.queue.borrow_mut().push_back(
                                InternalEvent::TagCommit(id),
                            );
                            Ok(EventState::Consumed)
                        },
                    );
                } else if k == self.key_config.focus_right
                    && self.commit_details.is_visible()
                {
                    return self.selected_commit().map_or(
                        Ok(EventState::NotConsumed),
                        |id| {
                            self.queue.borrow_mut().push_back(
                                InternalEvent::InspectCommit(
                                    id,
                                    self.selected_commit_tags(&Some(
                                        id,
                                    )),
                                ),
                            );
                            Ok(EventState::Consumed)
                        },
                    );
                } else if k == self.key_config.select_branch {
                    self.queue
                        .borrow_mut()
                        .push_back(InternalEvent::SelectBranch);
                    return Ok(EventState::Consumed);
                } else if k
                    == self.key_config.show_find_commit_text_input
                {
                    self.find_commit.toggle_visible()?;
                    self.find_commit.focus(true);
                    return Ok(EventState::Consumed);
                } else if k == self.key_config.exit_popup {
                    self.filter("")?;
                    self.find_commit.clear_input();
                    self.update()?;
                } else if k == self.key_config.open_file_tree {
                    return self.selected_commit().map_or(
                        Ok(EventState::NotConsumed),
                        |id| {
                            self.queue.borrow_mut().push_back(
                                InternalEvent::OpenFileTree(id),
                            );
                            Ok(EventState::Consumed)
                        },
                    );
                }
            }
        }

        Ok(EventState::NotConsumed)
    }

    fn commands(
        &self,
        out: &mut Vec<CommandInfo>,
        force_all: bool,
    ) -> CommandBlocking {
        if self.visible || force_all {
            self.list.commands(out, force_all);
        }

        out.push(CommandInfo::new(
            strings::commands::log_details_toggle(&self.key_config),
            true,
            self.visible,
        ));

        out.push(CommandInfo::new(
            strings::commands::log_details_open(&self.key_config),
            true,
            (self.visible && self.commit_details.is_visible())
                || force_all,
        ));

        out.push(CommandInfo::new(
            strings::commands::log_tag_commit(&self.key_config),
            self.selected_commit().is_some(),
            self.visible || force_all,
        ));

        out.push(CommandInfo::new(
            strings::commands::open_branch_select_popup(
                &self.key_config,
            ),
            true,
            self.visible || force_all,
        ));

        out.push(CommandInfo::new(
            strings::commands::copy_hash(&self.key_config),
            self.selected_commit().is_some(),
            self.visible || force_all,
        ));

        out.push(CommandInfo::new(
            strings::commands::push_tags(&self.key_config),
            true,
            self.visible || force_all,
        ));

        out.push(CommandInfo::new(
            strings::commands::find_commit(&self.key_config),
            true,
            self.visible || force_all,
        ));

        out.push(CommandInfo::new(
            strings::commands::inspect_file_tree(&self.key_config),
            self.selected_commit().is_some(),
            self.visible || force_all,
        ));

        visibility_blocking(self)
    }

    fn is_visible(&self) -> bool {
        self.visible
    }

    fn hide(&mut self) {
        self.visible = false;
        self.git_log.set_background();
    }

    fn show(&mut self) -> Result<()> {
        self.visible = true;
        self.list.clear();
        self.update()?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::Revlog;
    use crate::components::async_commit_filter::FilterBy;

    #[test]
    fn test_get_what_to_filter_by_flags() {
        assert_eq!(
            Revlog::get_what_to_filter_by("foo"),
            vec![vec![("foo".to_owned(), FilterBy::everywhere())]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":s foo"),
            vec![vec![("foo".to_owned(), FilterBy::SHA)]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":sm foo"),
            vec![vec![(
                "foo".to_owned(),
                FilterBy::SHA | FilterBy::MESSAGE
            )]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":samt foo"),
            vec![vec![("foo".to_owned(), FilterBy::everywhere())]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":!csamt foo"),
            vec![vec![("foo".to_owned(), FilterBy::all())]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":!c foo"),
            vec![vec![("foo".to_owned(), FilterBy::all())]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":! foo"),
            vec![vec![(
                "foo".to_owned(),
                FilterBy::everywhere() | FilterBy::NOT
            )]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":c foo"),
            vec![vec![(
                "foo".to_owned(),
                FilterBy::everywhere() | FilterBy::CASE_SENSITIVE
            )]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by(":!m foo"),
            vec![vec![(
                "foo".to_owned(),
                FilterBy::MESSAGE | FilterBy::NOT
            )]]
        );
    }

    #[test]
    fn test_get_what_to_filter_by_log_op() {
        assert_eq!(
            Revlog::get_what_to_filter_by("foo && bar"),
            vec![vec![
                ("foo".to_owned(), FilterBy::everywhere()),
                ("bar".to_owned(), FilterBy::everywhere())
            ]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by("foo || bar"),
            vec![
                vec![("foo".to_owned(), FilterBy::everywhere())],
                vec![("bar".to_owned(), FilterBy::everywhere())]
            ]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by("foo && bar || :m baz"),
            vec![
                vec![
                    ("foo".to_owned(), FilterBy::everywhere()),
                    ("bar".to_owned(), FilterBy::everywhere())
                ],
                vec![("baz".to_owned(), FilterBy::MESSAGE)]
            ]
        );
    }

    #[test]
    fn test_get_what_to_filter_by_spaces() {
        assert_eq!(
            Revlog::get_what_to_filter_by("foo&&bar"),
            vec![vec![
                ("foo".to_owned(), FilterBy::everywhere()),
                ("bar".to_owned(), FilterBy::everywhere())
            ]]
        );
        assert_eq!(
            Revlog::get_what_to_filter_by("  foo  &&  bar  "),
            vec![vec![
                ("foo".to_owned(), FilterBy::everywhere()),
                ("bar".to_owned(), FilterBy::everywhere())
            ]]
        );

        assert_eq!(
            Revlog::get_what_to_filter_by("  foo  bar   baz "),
            vec![vec![(
                "foo  bar   baz".to_owned(),
                FilterBy::everywhere()
            )]]
        );
        assert_eq!(
            Revlog::get_what_to_filter_by(" :m  foo  bar   baz "),
            vec![vec![(
                "foo  bar   baz".to_owned(),
                FilterBy::MESSAGE
            )]]
        );
        assert_eq!(
            Revlog::get_what_to_filter_by(
                " :m  foo  bar   baz && qwe   t "
            ),
            vec![vec![
                ("foo  bar   baz".to_owned(), FilterBy::MESSAGE),
                ("qwe   t".to_owned(), FilterBy::everywhere())
            ]]
        );
    }

    #[test]
    fn test_get_what_to_filter_by_invalid_flags_ignored() {
        assert_eq!(
            Revlog::get_what_to_filter_by(":q foo"),
            vec![vec![("foo".to_owned(), FilterBy::everywhere())]]
        );
        assert_eq!(
            Revlog::get_what_to_filter_by(":mq foo"),
            vec![vec![("foo".to_owned(), FilterBy::MESSAGE)]]
        );
    }
}
