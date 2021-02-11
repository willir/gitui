use anyhow::Result;
use asyncgit::{
    sync::{self, limit_str, CommitInfo},
    AsyncLog, AsyncNotification, CWD,
};
use bitflags::bitflags;
use crossbeam_channel::{Sender, TryRecvError};
use parking_lot::Mutex;
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

const FILTER_SLEEP_DURATION: Duration = Duration::from_millis(10);
const FILTER_SLEEP_DURATION_FAILED_LOCK: Duration =
    Duration::from_millis(500);
const SLICE_SIZE: usize = 1200;

bitflags! {
    pub struct FilterBy: u32 {
        const SHA = 0b0000_0001;
        const AUTHOR = 0b0000_0010;
        const MESSAGE = 0b0000_0100;
    }
}

#[derive(PartialEq)]
pub enum FilterStatus {
    Filtering,
    Finished,
}

pub struct AsyncCommitFilterer {
    git_log: AsyncLog,
    filter_strings: Vec<Vec<(String, FilterBy)>>,
    filtered_commits: Arc<Mutex<Vec<CommitInfo>>>,
    filter_count: Arc<AtomicUsize>,
    filter_finished: Arc<AtomicBool>,
    is_pending_local: RefCell<bool>,
    filter_thread_sender: Option<Sender<bool>>,
    filter_thread_mutex: Arc<Mutex<()>>,
    sender: Sender<AsyncNotification>,
}

impl AsyncCommitFilterer {
    pub fn new(
        git_log: AsyncLog,
        sender: &Sender<AsyncNotification>,
    ) -> Self {
        Self {
            filter_strings: Vec::new(),
            git_log: git_log,
            filtered_commits: Arc::new(Mutex::new(Vec::new())),
            filter_count: Arc::new(AtomicUsize::new(0)),
            filter_finished: Arc::new(AtomicBool::new(false)),
            filter_thread_mutex: Arc::new(Mutex::new(())),
            is_pending_local: RefCell::new(false),
            filter_thread_sender: None,
            sender: sender.clone(),
        }
    }

    pub fn is_pending(&self) -> bool {
        let mut b = self.is_pending_local.borrow_mut();
        if *b {
            *b = self.fetch() == FilterStatus::Filtering;
            *b
        } else {
            false
        }
    }

    pub fn clear(&mut self) -> Result<()> {
        self.filtered_commits.lock().clear();
        Ok(())
    }

    pub fn filter(
        mut vec_commit_info: Vec<CommitInfo>,
        filter_strings: &Vec<Vec<(String, FilterBy)>>,
    ) -> Vec<CommitInfo> {
        vec_commit_info
            .drain(..)
            .filter(|commit| {
                for to_and in filter_strings {
                    let mut is_and = true;
                    for (s, filter) in to_and {
                        let b = false
                            || if filter.contains(FilterBy::SHA) {
                                if commit
                                    .id
                                    .to_string()
                                    .to_lowercase()
                                    .contains(&s.to_lowercase())
                                {
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                            || if filter.contains(FilterBy::AUTHOR) {
                                if commit
                                    .author
                                    .to_lowercase()
                                    .contains(&s.to_lowercase())
                                {
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                            || if filter.contains(FilterBy::MESSAGE) {
                                if commit
                                    .message
                                    .to_lowercase()
                                    .contains(&s.to_lowercase())
                                {
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                        is_and = is_and && b;
                    }
                    if is_and {
                        return true;
                    }
                }
                false
            })
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    pub fn start_filter(
        &mut self,
        filter_strings: Vec<Vec<(String, FilterBy)>>,
    ) -> Result<()> {
        self.stop_filter();

        self.clear().expect("Can't fail unless app crashes");
        self.filter_strings = filter_strings.clone();
        self.filter_count.store(0, Ordering::Relaxed);

        let filtered_commits = Arc::clone(&self.filtered_commits);
        let filter_count = Arc::clone(&self.filter_count);
        let async_log = self.git_log.clone();
        let filter_finished = Arc::clone(&self.filter_finished);

        let (tx, rx) = crossbeam_channel::unbounded();

        self.filter_thread_sender = Some(tx);
        let async_app_sender = self.sender.clone();

        let prev_thread_mutex = Arc::clone(&self.filter_thread_mutex);
        self.filter_thread_mutex = Arc::new(Mutex::new(()));

        let cur_thread_mutex = Arc::clone(&self.filter_thread_mutex);
        self.is_pending_local.replace(true);

        rayon_core::spawn(move || {
            // Only 1 thread can filter at a time
            let _c = cur_thread_mutex.lock();
            let _p = prev_thread_mutex.lock();
            filter_finished.store(false, Ordering::Relaxed);
            filter_count.store(0, Ordering::Relaxed);
            filtered_commits.lock().clear();
            let mut cur_index: usize = 0;
            loop {
                // Get the git_log and start filtering through it
                match async_log.get_slice(cur_index, SLICE_SIZE) {
                    Ok(ids) => {
                        match sync::get_commits_info(
                            CWD,
                            &ids,
                            usize::MAX,
                        ) {
                            Ok(v) => {
                                match rx.try_recv() {
                                    Ok(_)
                                    | Err(
                                        TryRecvError::Disconnected,
                                    ) => {
                                        break;
                                    }
                                    _ => {}
                                }
                                if v.len() == 0
                                    && !async_log.is_pending()
                                {
                                    // Assume finished if log not pending and 0 recieved
                                    filter_finished.store(
                                        true,
                                        Ordering::Relaxed,
                                    );
                                    break;
                                }

                                let mut filtered =
                                    Self::filter(v, &filter_strings);
                                filter_count.fetch_add(
                                    filtered.len(),
                                    Ordering::Relaxed,
                                );
                                let mut fc = filtered_commits.lock();
                                fc.append(&mut filtered);
                                drop(fc);
                                cur_index += SLICE_SIZE;
                                async_app_sender
                                    .send(AsyncNotification::Log)
                                    .expect("error sending");
                                thread::sleep(FILTER_SLEEP_DURATION);
                            }
                            Err(_) => {
                                // Failed to get commit info
                                thread::sleep(
                                    FILTER_SLEEP_DURATION_FAILED_LOCK,
                                );
                            }
                        }
                    }
                    Err(_) => {
                        // Failed to get slice
                        thread::sleep(
                            FILTER_SLEEP_DURATION_FAILED_LOCK,
                        );
                    }
                }
            }
        });
        Ok(())
    }

    /// Stop the filter if one was running, otherwise does nothing.
    /// Is it possible to restart from this stage by calling restart
    pub fn stop_filter(&self) {
        // Any error this gives can be safely ignored,
        // it will send if reciever exists, otherwise does nothing
        if let Some(sender) = &self.filter_thread_sender {
            match sender.try_send(true) {
                Ok(_) | Err(_) => {}
            };
        }
        self.is_pending_local.replace(false);
        self.filter_finished.store(true, Ordering::Relaxed);
    }

    /// Use if the next item to be filtered is a substring of the previous item.
    /// This then only searches through the previous list
    //pub fn continue_filter(&mut self, _s: String) -> Result<()> {
    //   Ok(())
    //}

    pub fn get_filter_items(
        &mut self,
        start: usize,
        amount: usize,
        message_length_limit: usize,
    ) -> Result<Vec<CommitInfo>> {
        let fc = self.filtered_commits.lock();
        let len = fc.len();
        let min = start.min(len);
        let max = min + amount;
        let max = max.min(len);
        let mut commits_requested = fc[min..max].to_vec();
        for c in &mut commits_requested {
            c.message = limit_str(&c.message, message_length_limit)
                .to_string();
        }
        Ok(commits_requested)
    }

    pub fn count(&self) -> usize {
        self.filter_count.load(Ordering::Relaxed)
    }

    pub fn fetch(&self) -> FilterStatus {
        if self.filter_finished.load(Ordering::Relaxed) {
            FilterStatus::Finished
        } else {
            FilterStatus::Filtering
        }
    }
}
