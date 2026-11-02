use std::{
    future::Future,
    time::{Duration, Instant},
};

use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext as _, BorrowAppContext as _, SharedString, Task};

use crate::github_pull_requests::{
    GitHubPullRequestService, GitHubRepository, PullRequestDetail, PullRequestFilter,
    PullRequestSummary,
};

const FRESHNESS: Duration = Duration::from_secs(60);
const CACHE_CAPACITY: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) type Response<T> = Shared<Task<Result<T, SharedString>>>;

#[derive(Clone, Copy)]
pub(super) enum CachePolicy {
    PreferCache,
    Reload,
}

struct Entry<T: Clone + 'static> {
    response: Response<T>,
    snapshot: Option<T>,
    requested_at: Instant,
    _poller: Task<()>,
}

struct Cache<T: Clone + 'static> {
    entries: collections::HashMap<String, Entry<T>>,
}

impl<T: Clone + 'static> Default for Cache<T> {
    fn default() -> Self {
        Self {
            entries: Default::default(),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Cache<T> {
    fn request(
        &mut self,
        key: String,
        policy: CachePolicy,
        load: impl Future<Output = anyhow::Result<T>> + Send + 'static,
        cx: &App,
    ) -> (Option<T>, Response<T>) {
        let now = cx.background_executor().now();
        let entry = self.entries.get(&key);
        let snapshot = entry.and_then(|entry| match entry.response.peek() {
            Some(Ok(value)) => Some(value.clone()),
            Some(Err(_)) => None,
            None => entry.snapshot.clone(),
        });
        if let Some(entry) = entry {
            let pending = entry.response.peek().is_none();
            let fresh = snapshot.is_some() && now.duration_since(entry.requested_at) < FRESHNESS;
            if pending || (fresh && matches!(policy, CachePolicy::PreferCache)) {
                return (snapshot, entry.response.clone());
            }
        }
        if !self.entries.contains_key(&key) && self.entries.len() >= CACHE_CAPACITY {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.requested_at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let timeout = cx.background_executor().timer(REQUEST_TIMEOUT);
        let response = cx
            .background_spawn(async move {
                let load = load.fuse();
                let timeout = timeout.fuse();
                futures::pin_mut!(load, timeout);
                futures::select_biased! {
                    result = load => result.map_err(|error| SharedString::from(format!("{error:#}"))),
                    () = timeout => Err("GitHub request timed out; check connectivity and retry".into()),
                }
            })
            .shared();
        // Drive completion even if a tab switches away and drops its subscriber.
        // Evicting the entry cancels this owner, so abandoned requests remain bounded.
        let poller = cx.background_spawn({
            let response = response.clone();
            async move {
                if let Err(error) = response.await {
                    log::debug!("PR cache request failed: {error}");
                }
            }
        });
        self.entries.insert(
            key,
            Entry {
                response: response.clone(),
                snapshot: snapshot.clone(),
                requested_at: now,
                _poller: poller,
            },
        );
        (snapshot, response)
    }
}

#[derive(Default)]
pub(super) struct PullRequestCache {
    lists: Cache<Vec<PullRequestSummary>>,
    details: Cache<PullRequestDetail>,
}

impl gpui::Global for PullRequestCache {}

impl PullRequestCache {
    pub(super) fn list(
        scope: &str,
        target: GitHubRepository,
        filter: PullRequestFilter,
        policy: CachePolicy,
        cx: &mut App,
    ) -> (
        Option<Vec<PullRequestSummary>>,
        Response<Vec<PullRequestSummary>>,
    ) {
        let key = format!("{scope}:{}:{}", target.target(), filter.label());
        cx.update_default_global::<Self, _>(|cache, cx| {
            cache.lists.request(
                key,
                policy,
                async move { GitHubPullRequestService::list(&target, filter).await },
                cx,
            )
        })
    }

    pub(super) fn detail(
        scope: &str,
        target: GitHubRepository,
        number: u64,
        policy: CachePolicy,
        cx: &mut App,
    ) -> (Option<PullRequestDetail>, Response<PullRequestDetail>) {
        let key = format!("{scope}:{}:{number}", target.target());
        cx.update_default_global::<Self, _>(|cache, cx| {
            cache.details.request(
                key,
                policy,
                async move { GitHubPullRequestService::detail(&target, number).await },
                cx,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[gpui::test(iterations = 5)]
    async fn test_cache_single_flight(cx: &mut gpui::TestAppContext) {
        let count = Arc::new(AtomicUsize::new(0));
        let mut cache = Cache::default();
        let load = |value| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(value)
            }
        };
        let (_, first) =
            cx.update(|cx| cache.request("repo".into(), CachePolicy::PreferCache, load(1), cx));
        let (_, second) =
            cx.update(|cx| cache.request("repo".into(), CachePolicy::PreferCache, load(2), cx));
        let (first, second) = futures::join!(first, second);
        assert_eq!(first.expect("first result"), 1);
        assert_eq!(second.expect("shared result"), 1);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let (snapshot, fresh) =
            cx.update(|cx| cache.request("repo".into(), CachePolicy::PreferCache, load(3), cx));
        assert_eq!(snapshot, Some(1));
        assert_eq!(fresh.await.expect("fresh snapshot"), 1);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let (snapshot, reload) =
            cx.update(|cx| cache.request("repo".into(), CachePolicy::Reload, load(4), cx));
        assert_eq!(snapshot, Some(1));
        assert_eq!(reload.await.expect("forced reload"), 4);
        cx.executor().advance_clock(FRESHNESS);
        let (snapshot, expired) =
            cx.update(|cx| cache.request("repo".into(), CachePolicy::PreferCache, load(5), cx));
        assert_eq!(snapshot, Some(4));
        let (pending_snapshot, shared_expired) =
            cx.update(|cx| cache.request("repo".into(), CachePolicy::PreferCache, load(6), cx));
        assert_eq!(
            pending_snapshot,
            Some(4),
            "retain stale content during shared revalidation"
        );
        assert_eq!(expired.await.expect("stale revalidation"), 5);
        assert_eq!(shared_expired.await.expect("shared revalidation"), 5);
    }

    #[gpui::test(iterations = 5)]
    async fn test_cache_completion(cx: &mut gpui::TestAppContext) {
        let mut cache = Cache::default();
        let (_, response) = cx.update(|cx| {
            cache.request("repo".into(), CachePolicy::PreferCache, async { Ok(9) }, cx)
        });
        drop(response);
        cx.run_until_parked();
        let (snapshot, response) = cx.update(|cx| {
            cache.request(
                "repo".into(),
                CachePolicy::PreferCache,
                async { Ok(10) },
                cx,
            )
        });
        assert_eq!(
            snapshot,
            Some(9),
            "finish prefetch even without a subscriber"
        );
        assert_eq!(response.await.expect("completed prefetch"), 9);
        let (_, response) = cx.update(|cx| {
            cache.request(
                "timeout".into(),
                CachePolicy::PreferCache,
                std::future::pending::<anyhow::Result<usize>>(),
                cx,
            )
        });
        cx.run_until_parked();
        cx.executor().advance_clock(REQUEST_TIMEOUT);
        assert!(
            response
                .await
                .expect_err("bounded request")
                .contains("timed out")
        );
    }

    #[gpui::test(iterations = 5)]
    async fn test_cache_isolation(cx: &mut gpui::TestAppContext) {
        let mut cache = Cache::default();
        for index in 0..=CACHE_CAPACITY {
            let (_, response) = cx.update(|cx| {
                cache.request(
                    index.to_string(),
                    CachePolicy::PreferCache,
                    async move { Ok(index) },
                    cx,
                )
            });
            assert_eq!(response.await.expect("isolated result"), index);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        let (_, failure) = cx.update(|cx| {
            cache.request(
                "failure".into(),
                CachePolicy::PreferCache,
                async { anyhow::bail!("expired credentials") },
                cx,
            )
        });
        assert!(failure.await.is_err());
        let (snapshot, retry) = cx.update(|cx| {
            cache.request(
                "failure".into(),
                CachePolicy::PreferCache,
                async { Ok(42) },
                cx,
            )
        });
        assert_eq!(snapshot, None);
        assert_eq!(retry.await.expect("retry error"), 42);
    }
}
