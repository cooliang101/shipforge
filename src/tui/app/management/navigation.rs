//! Bounded, non-recursive return snapshots. A preview is never a reusable receipt.

use super::{Arc, ManagementPage, ManagementRequest, ManagementScope, ManagementScreen, viewport};

const MAX_RETURN_DEPTH: usize = 8;

#[derive(Clone, Debug)]
pub(super) struct ManagementReturn {
    scope: Arc<ManagementScope>,
    page: ManagementPage,
    view: viewport::Viewport,
    notice: Option<String>,
}

impl ManagementScreen {
    pub(super) fn remember_page(&mut self) {
        if self.back.len() == MAX_RETURN_DEPTH {
            self.back.remove(0);
        }
        self.back.push(ManagementReturn {
            scope: Arc::clone(&self.scope),
            page: self.page.clone(),
            view: self.view.clone(),
            notice: self.notice.clone(),
        });
    }

    pub(super) fn push_page(&mut self, page: ManagementPage) {
        self.remember_page();
        self.page = page;
        self.view = viewport::Viewport::default();
        self.notice = None;
    }

    pub(super) fn go_back(&mut self) {
        if let Some(previous) = self.back.pop() {
            self.scope = previous.scope;
            self.page = previous.page;
            self.view = previous.view;
            self.notice = previous.notice;
        } else {
            self.page = ManagementPage::Home;
            self.view = viewport::Viewport::default();
            self.notice = None;
        }
    }

    pub(super) fn consume_rollback_navigation(&mut self) {
        // Retain read-only source navigation, not any selection, preview, failure
        // retry or prior execution page that could carry a consumed plan.
        self.back.retain(|previous| {
            matches!(
                previous.page,
                ManagementPage::Home
                    | ManagementPage::Environments { .. }
                    | ManagementPage::History { .. }
                    | ManagementPage::Detail(_)
                    | ManagementPage::Reports { .. }
                    | ManagementPage::Report { .. }
            )
        });
        self.page = ManagementPage::Home;
        self.view = viewport::Viewport::default();
        self.notice = None;
    }

    pub(super) fn accept_result(&mut self, request: &ManagementRequest, mut page: ManagementPage) {
        if matches!(request, ManagementRequest::Execute(_)) {
            self.page = page;
            self.view = viewport::Viewport::default();
            self.notice = None;
        } else if let Some(refresh) = refresh_cursor(&self.page, &mut page) {
            self.page = page;
            if !refresh {
                self.view = viewport::Viewport::default();
            }
            self.notice = None;
        } else {
            self.push_page(page);
        }
    }

    pub(super) fn show_failure(&mut self, request: ManagementRequest, message: String) {
        let label = request.label();
        let executing = matches!(request, ManagementRequest::Execute(_));
        let retry = request.can_retry().then_some(request);
        let page = ManagementPage::Failed {
            label,
            message,
            retry,
        };
        if executing {
            self.page = page;
            self.view = viewport::Viewport::default();
        } else {
            self.notice = Some(format!(
                "Previous page retained after a failed request ({label}); it is not a new check or refreshed result."
            ));
            self.push_page(page);
        }
    }
}

impl ManagementRequest {
    pub(super) const fn discards_cancelled_result(&self) -> bool {
        !matches!(self, Self::Inspect { .. } | Self::Execute(_))
    }

    pub(super) const fn can_retry(&self) -> bool {
        // Inspection can already have produced a local report before an error.
        // Only an explicit new inspection from its selection page may repeat it.
        !matches!(self, Self::Inspect { .. } | Self::Execute(_))
    }
}

fn refresh_cursor(previous: &ManagementPage, next: &mut ManagementPage) -> Option<bool> {
    match (previous, next) {
        (
            ManagementPage::History {
                page: old,
                offset,
                cursor,
            },
            ManagementPage::History {
                page: new,
                offset: next_offset,
                cursor: next_cursor,
            },
        ) => {
            if offset == next_offset {
                *next_cursor =
                    stable_cursor(&old.items, &new.items, *cursor, |item| &item.deployment);
            }
            Some(offset == next_offset)
        }
        (
            ManagementPage::Reports {
                page: old,
                offset,
                cursor,
            },
            ManagementPage::Reports {
                page: new,
                offset: next_offset,
                cursor: next_cursor,
            },
        ) => {
            if offset == next_offset {
                *next_cursor = stable_cursor(&old.items, &new.items, *cursor, |item| &item.id);
            }
            Some(offset == next_offset)
        }
        (
            ManagementPage::Environments {
                page: old,
                offset,
                cursor,
            },
            ManagementPage::Environments {
                page: new,
                offset: next_offset,
                cursor: next_cursor,
            },
        ) => {
            if offset == next_offset {
                *next_cursor = stable_cursor(&old.items, &new.items, *cursor, |item| item);
            }
            Some(offset == next_offset)
        }
        _ => None,
    }
}

fn stable_cursor<T, K: PartialEq>(
    previous: &[T],
    next: &[T],
    cursor: usize,
    key: impl Fn(&T) -> &K,
) -> usize {
    previous
        .get(cursor)
        .and_then(|selected| next.iter().position(|item| key(item) == key(selected)))
        .unwrap_or_else(|| cursor.min(next.len().saturating_sub(1)))
}
