use std::collections::HashMap;

pub(crate) enum NamedTaskOutcome<T> {
    Completed { alias: String, value: T },
    Failed { alias: String, detail: String },
}

impl<T> NamedTaskOutcome<T> {
    fn alias(&self) -> &str {
        match self {
            Self::Completed { alias, .. } | Self::Failed { alias, .. } => alias,
        }
    }
}

pub(crate) fn join_failure_outcome(error: &tokio::task::JoinError) -> &'static str {
    if error.is_panic() {
        "panicked"
    } else if error.is_cancelled() {
        "was cancelled"
    } else {
        "failed unexpectedly"
    }
}

pub(crate) fn join_failure_detail(error: &tokio::task::JoinError) -> String {
    format!(
        "worker {} before its result could be collected",
        join_failure_outcome(error)
    )
}

/// Drain every registered worker, even after one worker fails to join.
///
/// Usage workers can rotate a single-use refresh token. Dropping their
/// `JoinSet` on the first failure could discard a later worker's replacement
/// credential before it is persisted.
pub(crate) async fn drain_named_tasks<T: 'static>(
    tasks: &mut tokio::task::JoinSet<T>,
    task_aliases: &mut HashMap<tokio::task::Id, String>,
    mut on_joined: impl FnMut(&str),
) -> Vec<NamedTaskOutcome<T>> {
    let mut outcomes = Vec::with_capacity(tasks.len());
    while let Some(joined) = tasks.join_next_with_id().await {
        let task_id = match &joined {
            Ok((task_id, _)) => *task_id,
            Err(error) => error.id(),
        };
        let Some(alias) = task_aliases.remove(&task_id) else {
            outcomes.push(NamedTaskOutcome::Failed {
                alias: "<unregistered worker>".to_string(),
                detail: "batch bookkeeping lost a completed worker identity".to_string(),
            });
            continue;
        };
        on_joined(&alias);
        match joined {
            Ok((_, value)) => outcomes.push(NamedTaskOutcome::Completed { alias, value }),
            Err(error) => outcomes.push(NamedTaskOutcome::Failed {
                alias,
                detail: join_failure_detail(&error),
            }),
        }
    }

    for (_, alias) in task_aliases.drain() {
        outcomes.push(NamedTaskOutcome::Failed {
            alias,
            detail: "batch ended before the registered worker returned".to_string(),
        });
    }
    outcomes.sort_by(|a, b| a.alias().cmp(b.alias()));
    outcomes
}

pub(crate) fn batch_failure_error(
    context: &str,
    mut failures: Vec<(String, String)>,
) -> anyhow::Error {
    failures.sort_by(|(a_alias, a_detail), (b_alias, b_detail)| {
        a_alias.cmp(b_alias).then_with(|| a_detail.cmp(b_detail))
    });
    let details = failures
        .into_iter()
        .map(|(alias, detail)| format!("[{alias}] {detail}"))
        .collect::<Vec<_>>()
        .join("; ");
    anyhow::anyhow!("{context}: {details}")
}

#[cfg(test)]
mod tests {
    use super::{NamedTaskOutcome, drain_named_tasks};
    use std::collections::HashMap;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_failure_is_reported_only_after_later_persistence_finishes() {
        let temp = crate::fs_ops::create_direct_tempdir().unwrap();
        let persisted = temp.path().join("persisted");
        let delayed_path = persisted.clone();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let panic_barrier = barrier.clone();
        let delayed_barrier = barrier.clone();
        let mut tasks = tokio::task::JoinSet::new();
        let mut aliases = HashMap::new();

        let panic_task = tasks.spawn(async move {
            panic_barrier.wait().await;
            panic!("private panic payload");
        });
        aliases.insert(panic_task.id(), "panic".to_string());

        let delayed_task = tasks.spawn(async move {
            delayed_barrier.wait().await;
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            std::fs::write(delayed_path, b"saved").unwrap();
        });
        aliases.insert(delayed_task.id(), "persist".to_string());

        let outcomes = drain_named_tasks(&mut tasks, &mut aliases, |_| {}).await;

        assert_eq!(std::fs::read(persisted).unwrap(), b"saved");
        let failure = outcomes
            .iter()
            .find_map(|outcome| match outcome {
                NamedTaskOutcome::Failed { alias, detail } if alias == "panic" => Some(detail),
                _ => None,
            })
            .expect("panicking worker must be reported");
        assert!(failure.contains("worker panicked"), "{failure}");
        assert!(!failure.contains("private panic payload"), "{failure}");
    }

    #[tokio::test]
    async fn cancellation_keeps_its_registered_alias() {
        let mut tasks = tokio::task::JoinSet::new();
        let mut aliases = HashMap::new();
        let task = tasks.spawn(std::future::pending::<()>());
        aliases.insert(task.id(), "cancelled-account".to_string());
        task.abort();

        let outcomes = drain_named_tasks(&mut tasks, &mut aliases, |_| {}).await;
        assert!(matches!(
            outcomes.as_slice(),
            [NamedTaskOutcome::Failed { alias, detail }]
                if alias == "cancelled-account" && detail.contains("was cancelled")
        ));
    }
}
