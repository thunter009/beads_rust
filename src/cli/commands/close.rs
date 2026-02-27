//! Close command implementation.

use crate::cli::CloseArgs as CliCloseArgs;
use crate::cli::commands::{
    auto_import_storage_ctx_if_stale, finalize_batched_blocked_cache_refresh,
    preserve_blocked_cache_on_error, resolve_issue_ids, update_issue_with_recovery,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{IssueType, Status};
use crate::output::OutputContext;
use crate::storage::IssueUpdate;
use crate::util::id::{IdResolver, ResolverConfig};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// Internal arguments for the close command.
#[derive(Debug, Clone, Default)]
pub struct CloseArgs {
    /// Issue IDs to close
    pub ids: Vec<String>,
    /// Close reason
    pub reason: Option<String>,
    /// Force close even if blocked
    pub force: bool,
    /// Session ID for `closed_by_session` field
    pub session: Option<String>,
    /// Return newly unblocked issues (single ID only)
    pub suggest_next: bool,
}

impl From<&CliCloseArgs> for CloseArgs {
    fn from(cli: &CliCloseArgs) -> Self {
        Self {
            ids: cli.ids.clone(),
            reason: cli.reason.clone(),
            force: cli.force,
            session: cli.session.clone(),
            suggest_next: cli.suggest_next,
        }
    }
}

/// Execute the close command from CLI args.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute_cli(
    cli_args: &CliCloseArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let args = CloseArgs::from(cli_args);
    execute_with_args(&args, json, cli, ctx)
}

/// Result of a close operation for JSON output.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
}

/// Result of closing with suggest-next.
#[derive(Debug, Serialize, Deserialize)]
pub struct CloseWithSuggestResult {
    pub closed: Vec<ClosedIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped: Vec<SkippedIssue>,
    pub unblocked: Vec<UnblockedIssue>,
}

/// An issue that became unblocked after closing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnblockedIssue {
    pub id: String,
    pub title: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    pub closed_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub close_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedIssue {
    pub id: String,
    pub reason: String,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
struct CloseExecution {
    closed: Vec<ClosedIssue>,
    skipped: Vec<SkippedIssue>,
    unblocked: Vec<UnblockedIssue>,
    ordered_outcomes: Vec<CloseOutcome>,
}

#[derive(Debug, Clone)]
enum CloseOutcome {
    Closed(ClosedIssue),
    Skipped(SkippedIssue),
}

fn build_close_json_payload(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
) -> Result<String> {
    let json = if args.suggest_next {
        // suggest_next is br-only, so always use the wrapped machine format.
        let result = CloseWithSuggestResult {
            closed: closed_issues,
            skipped: skipped_issues,
            unblocked: unblocked_issues,
        };
        serde_json::to_string_pretty(&result)?
    } else if skipped_issues.is_empty() {
        // Preserve bd-compatible array output for pure-success closes.
        serde_json::to_string_pretty(&closed_issues)?
    } else {
        // Once skips are present, a bare array loses machine-readable reasons.
        let result = CloseResult {
            closed: closed_issues,
            skipped: skipped_issues,
        };
        serde_json::to_string_pretty(&result)?
    };

    Ok(json)
}

fn render_close_json(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
) -> Result<()> {
    let json = build_close_json_payload(args, closed_issues, skipped_issues, unblocked_issues)?;
    println!("{json}");
    Ok(())
}

fn emit_close_structured_output(
    args: &CloseArgs,
    closed_issues: Vec<ClosedIssue>,
    skipped_issues: Vec<SkippedIssue>,
    unblocked_issues: Vec<UnblockedIssue>,
    ctx: &OutputContext,
) -> Result<()> {
    if args.suggest_next {
        let result = CloseWithSuggestResult {
            closed: closed_issues,
            skipped: skipped_issues,
            unblocked: unblocked_issues,
        };
        if ctx.is_toon() {
            ctx.toon(&result);
        } else if ctx.is_json() {
            ctx.json_pretty(&result);
        } else {
            let json_ctx = OutputContext::from_flags(true, false, true);
            json_ctx.json_pretty(&result);
        }
        return Ok(());
    }

    if skipped_issues.is_empty() {
        if ctx.is_toon() {
            ctx.toon(&closed_issues);
        } else if ctx.is_json() {
            ctx.json_pretty(&closed_issues);
        } else {
            render_close_json(args, closed_issues, skipped_issues, unblocked_issues)?;
        }
        return Ok(());
    }

    let result = CloseResult {
        closed: closed_issues,
        skipped: skipped_issues,
    };
    if ctx.is_toon() {
        ctx.toon(&result);
    } else if ctx.is_json() {
        ctx.json_pretty(&result);
    } else {
        let json_ctx = OutputContext::from_flags(true, false, true);
        json_ctx.json_pretty(&result);
    }
    Ok(())
}

fn reorder_routed_items_by_requested_inputs<T>(
    requested_inputs: &[String],
    routed_items: Vec<(Vec<String>, Vec<T>)>,
    context: &str,
) -> Result<Vec<T>> {
    let mut positions_by_input: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (index, input) in requested_inputs.iter().enumerate() {
        positions_by_input
            .entry(input.as_str())
            .or_default()
            .push_back(index);
    }

    let mut ordered_items: Vec<Option<T>> = (0..requested_inputs.len()).map(|_| None).collect();
    for (batch_inputs, batch_items) in routed_items {
        if batch_inputs.len() != batch_items.len() {
            return Err(BeadsError::Config(format!(
                "{context} produced mismatched issue/result counts"
            )));
        }

        for (input, item) in batch_inputs.into_iter().zip(batch_items) {
            let Some(index) = positions_by_input
                .get_mut(input.as_str())
                .and_then(VecDeque::pop_front)
            else {
                return Err(BeadsError::Config(format!(
                    "{context} returned unexpected issue input {input}"
                )));
            };
            ordered_items[index] = Some(item);
        }
    }

    ordered_items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            item.ok_or_else(|| {
                BeadsError::Config(format!(
                    "{context} did not produce a result for {}",
                    requested_inputs[index]
                ))
            })
        })
        .collect()
}

fn compute_batch_closable_ids(
    active_issue_ids: &HashSet<String>,
    internal_blockers_by_id: &HashMap<String, Vec<String>>,
    external_blockers_by_id: &HashMap<String, Vec<String>>,
) -> HashSet<String> {
    let mut closable: HashSet<String> = active_issue_ids
        .iter()
        .filter(|id| {
            external_blockers_by_id
                .get(*id)
                .is_none_or(std::vec::Vec::is_empty)
        })
        .cloned()
        .collect();

    loop {
        let to_remove: Vec<String> = closable
            .iter()
            .filter(|id| {
                internal_blockers_by_id
                    .get(*id)
                    .into_iter()
                    .flatten()
                    .any(|blocker_id| !closable.contains(blocker_id))
            })
            .cloned()
            .collect();

        if to_remove.is_empty() {
            break;
        }

        for id in to_remove {
            closable.remove(&id);
        }
    }

    closable
}

/// Execute the close command.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
pub fn execute(
    ids: Vec<String>,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let args = CloseArgs {
        ids,
        reason: None,
        force: false,
        session: None,
        suggest_next: false,
    };

    execute_with_args(&args, json, cli, ctx)
}

/// Execute the close command with full arguments.
///
/// # Errors
///
/// Returns an error if database operations fail or IDs cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn execute_with_args(
    args: &CloseArgs,
    use_json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    tracing::info!("Executing close command");
    let use_structured_output = use_json || ctx.is_json() || ctx.is_toon();

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut target_inputs = args.ids.clone();
    if target_inputs.is_empty() {
        let last_touched = crate::util::get_last_touched_id(&beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        target_inputs.push(last_touched);
    }

    if args.suggest_next && target_inputs.len() > 1 {
        return Err(BeadsError::validation(
            "suggest-next",
            "--suggest-next only works with a single issue ID",
        ));
    }
    let routed_batches = config::routing::group_issue_inputs_by_route(&target_inputs, &beads_dir)?;

    let mut closed_issues = Vec::new();
    let mut skipped_issues = Vec::new();
    let mut unblocked_issues = Vec::new();

    if routed_batches.iter().any(|batch| batch.is_external) {
        let normalized_local_beads_dir =
            dunce::canonicalize(&beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let mut routed_outcomes = Vec::new();

        for batch in routed_batches {
            let mut batch_args = args.clone();
            batch_args.ids.clone_from(&batch.issue_inputs);

            let normalized_batch_beads_dir =
                dunce::canonicalize(&batch.beads_dir).unwrap_or_else(|_| batch.beads_dir.clone());
            let mut batch_cli = cli.clone();
            batch_cli.db = if normalized_batch_beads_dir == normalized_local_beads_dir {
                cli.db.clone()
            } else {
                None
            };

            let execution =
                execute_route(&batch_args, &batch_cli, &batch.beads_dir, batch.is_external)?;
            let CloseExecution {
                unblocked,
                ordered_outcomes,
                ..
            } = execution;
            routed_outcomes.push((batch.issue_inputs, ordered_outcomes));
            unblocked_issues.extend(unblocked);
        }

        let ordered_outcomes = reorder_routed_items_by_requested_inputs(
            &target_inputs,
            routed_outcomes,
            "close routing",
        )?;
        for outcome in ordered_outcomes {
            match outcome {
                CloseOutcome::Closed(issue) => closed_issues.push(issue),
                CloseOutcome::Skipped(issue) => skipped_issues.push(issue),
            }
        }
    } else {
        let mut local_args = args.clone();
        local_args.ids = target_inputs;
        let execution = execute_route(&local_args, cli, &beads_dir, false)?;
        closed_issues = execution.closed;
        skipped_issues = execution.skipped;
        unblocked_issues = execution.unblocked;
    }

    let closed_count = closed_issues.len();
    let skipped_count = skipped_issues.len();

    if let Some(last_closed) = closed_issues.last() {
        crate::util::set_last_touched_id(&beads_dir, &last_closed.id);
    }

    if use_structured_output {
        emit_close_structured_output(args, closed_issues, skipped_issues, unblocked_issues, ctx)?;
    } else if closed_issues.is_empty() && skipped_issues.is_empty() {
        ctx.info("No issues to close.");
    } else {
        for closed in &closed_issues {
            let mut msg = format!("Closed {}: {}", closed.id, closed.title);
            if let Some(reason) = &closed.close_reason {
                msg.push_str(&format!(" ({reason})"));
            }
            ctx.success(&msg);
        }
        for skipped in &skipped_issues {
            ctx.warning(&format!("Skipped {}: {}", skipped.id, skipped.reason));
        }
        if !unblocked_issues.is_empty() {
            ctx.newline();
            ctx.info(&format!("Unblocked {} issue(s):", unblocked_issues.len()));
            for issue in &unblocked_issues {
                ctx.print_line(&format!("  {}: {}", issue.id, issue.title));
            }
        }
    }

    if closed_count == 0 && skipped_count > 0 {
        return Err(BeadsError::NothingToDo {
            reason: format!("all {skipped_count} issue(s) skipped"),
        });
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_route(
    args: &CloseArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    auto_flush_external: bool,
) -> Result<CloseExecution> {
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let resolved_ids = resolve_issue_ids(&storage_ctx.storage, &resolver, &args.ids)?;

    let epic_counts = storage_ctx.storage.get_epic_counts()?;
    let blocked_before: Vec<String> = if args.suggest_next {
        storage_ctx
            .storage
            .get_blocked_issues()?
            .into_iter()
            .map(|(i, _)| i.id)
            .collect()
    } else {
        Vec::new()
    };

    let requested_ids: HashSet<String> = resolved_ids.iter().cloned().collect();
    let mut open_issues: HashMap<String, crate::model::Issue> = HashMap::new();
    let mut internal_blockers_by_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut external_blockers_by_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut closed_issues: Vec<ClosedIssue> = Vec::new();
    let mut skipped_issues: Vec<SkippedIssue> = Vec::new();
    let mut ordered_outcomes = Vec::with_capacity(resolved_ids.len());
    let mut cache_dirty = false;

    for id in &resolved_ids {
        tracing::info!(id = %id, "Closing issue");

        let issue_result = storage_ctx.storage.get_issue(id);
        let Some(issue) = preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            issue_result,
        )?
        else {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: "issue not found".to_string(),
            };
            ordered_outcomes.push(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        };

        if issue.status.is_terminal() {
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!("already {}", issue.status.as_str()),
            };
            ordered_outcomes.push(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        if !args.force
            && let Some(&(total, closed)) = epic_counts.get(id)
            && closed < total
        {
            let label = if issue.issue_type == IssueType::Epic {
                "epic"
            } else {
                "parent issue"
            };
            let skipped = SkippedIssue {
                id: id.clone(),
                reason: format!(
                    "{label} has {}/{} open children (use --force to close anyway)",
                    total - closed,
                    total
                ),
            };
            ordered_outcomes.push(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        if args.force {
            open_issues.insert(id.clone(), issue);
            continue;
        }

        let is_blocked_result = storage_ctx.storage.is_blocked(id);
        let mut blocker_ids = if preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            is_blocked_result,
        )? {
            let blockers_result = storage_ctx.storage.get_blockers(id);
            preserve_blocked_cache_on_error(
                &mut storage_ctx.storage,
                cache_dirty,
                "close",
                blockers_result,
            )?
        } else {
            Vec::new()
        };
        blocker_ids.sort();
        blocker_ids.dedup();
        let (internal_blockers, external_blockers): (Vec<String>, Vec<String>) = blocker_ids
            .into_iter()
            .partition(|blocker_id| requested_ids.contains(blocker_id));
        internal_blockers_by_id.insert(id.clone(), internal_blockers);
        external_blockers_by_id.insert(id.clone(), external_blockers);
        open_issues.insert(id.clone(), issue);
    }

    let active_issue_ids: HashSet<String> = open_issues.keys().cloned().collect();
    let batch_closable_ids = if args.force {
        active_issue_ids
    } else {
        compute_batch_closable_ids(
            &active_issue_ids,
            &internal_blockers_by_id,
            &external_blockers_by_id,
        )
    };

    for id in &resolved_ids {
        let Some(issue) = open_issues.get(id) else {
            continue;
        };

        if !args.force && !batch_closable_ids.contains(id) {
            let mut blocker_ids = external_blockers_by_id.get(id).cloned().unwrap_or_default();
            if let Some(internal_blockers) = internal_blockers_by_id.get(id) {
                blocker_ids.extend(
                    internal_blockers
                        .iter()
                        .filter(|blocker_id| !batch_closable_ids.contains(*blocker_id))
                        .cloned(),
                );
            }
            blocker_ids.sort();
            blocker_ids.dedup();
            tracing::debug!(blocked_by = ?blocker_ids, "Issue remains blocked in batch close");
            let reason = if blocker_ids.is_empty() {
                "blocked by dependencies".to_string()
            } else {
                format!("blocked by: {}", blocker_ids.join(", "))
            };
            let skipped = SkippedIssue {
                id: id.clone(),
                reason,
            };
            ordered_outcomes.push(CloseOutcome::Skipped(skipped.clone()));
            skipped_issues.push(skipped);
            continue;
        }

        // Dot-notation child guard (fork-specific; supplements upstream's
        // epic_counts check, which only sees formally declared parent-child deps).
        if !args.force {
            let open_children = storage_ctx.storage.get_open_child_ids(id)?;
            if !open_children.is_empty() {
                let reason = format!(
                    "has {} open child issue(s): {}",
                    open_children.len(),
                    open_children.join(", ")
                );
                tracing::info!(id = %id, %reason, "Skipping close — open children");
                skipped_issues.push(SkippedIssue {
                    id: id.clone(),
                    reason,
                });
                continue;
            }
        }

        let now = Utc::now();
        let close_reason = args.reason.clone().unwrap_or_else(|| "done".to_string());
        let update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(now)),
            close_reason: Some(Some(close_reason.clone())),
            closed_by_session: args.session.clone().map(Some),
            skip_cache_rebuild: true,
            ..Default::default()
        };

        let update_result = update_issue_with_recovery(
            &mut storage_ctx,
            !cache_dirty,
            "close",
            id,
            &update,
            &actor,
        );
        preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            update_result,
        )?;
        cache_dirty = true;
        tracing::info!(id = %id, reason = ?args.reason, "Issue closed");

        let closed = ClosedIssue {
            id: id.clone(),
            title: issue.title.clone(),
            status: "closed".to_string(),
            closed_at: now.to_rfc3339(),
            close_reason: Some(close_reason),
        };
        ordered_outcomes.push(CloseOutcome::Closed(closed.clone()));
        closed_issues.push(closed);
    }

    if cache_dirty {
        tracing::info!(
            "Rebuilding blocked cache after closing {} issues",
            closed_issues.len()
        );
        finalize_batched_blocked_cache_refresh(&mut storage_ctx.storage, cache_dirty, "close")?;
    }

    let unblocked_issues: Vec<UnblockedIssue> = if args.suggest_next && !closed_issues.is_empty() {
        let blocked_after_result = storage_ctx.storage.get_blocked_issues();
        let blocked_after = match preserve_blocked_cache_on_error(
            &mut storage_ctx.storage,
            cache_dirty,
            "close",
            blocked_after_result,
        ) {
            Ok(blocked_after) => Some(
                blocked_after
                    .into_iter()
                    .map(|(issue, _)| issue.id)
                    .collect::<Vec<_>>(),
            ),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Skipping suggest-next calculation after committed close because blocked-cache lookup failed"
                );
                None
            }
        };

        let Some(blocked_after) = blocked_after else {
            storage_ctx.flush_no_db_if_dirty()?;
            return Ok(CloseExecution {
                closed: closed_issues,
                skipped: skipped_issues,
                unblocked: Vec::new(),
                ordered_outcomes,
            });
        };

        let newly_unblocked: Vec<String> = blocked_before
            .into_iter()
            .filter(|id| !blocked_after.contains(id))
            .collect();

        tracing::debug!(unblocked = ?newly_unblocked, "Issues unblocked by close");

        let mut unblocked = Vec::new();
        for uid in newly_unblocked {
            let issue_result = storage_ctx.storage.get_issue(&uid);
            match preserve_blocked_cache_on_error(
                &mut storage_ctx.storage,
                cache_dirty,
                "close",
                issue_result,
            ) {
                Ok(Some(issue)) if issue.status.is_active() => {
                    unblocked.push(UnblockedIssue {
                        id: issue.id,
                        title: issue.title,
                        priority: issue.priority.0,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        issue_id = %uid,
                        error = %error,
                        "Skipping suggest-next candidate after committed close because issue lookup failed"
                    );
                }
            }
        }
        unblocked
    } else {
        Vec::new()
    };

    storage_ctx.flush_no_db_if_dirty()?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        tracing::debug!(
            beads_dir = %storage_ctx.paths.beads_dir.display(),
            error = %error,
            "Routed auto-flush failed (non-fatal)"
        );
    }

    Ok(CloseExecution {
        closed: closed_issues,
        skipped: skipped_issues,
        unblocked: unblocked_issues,
        ordered_outcomes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands;
    use crate::config::CliOverrides;
    use crate::model::{DependencyType, Issue, IssueType, Priority, Status};
    use crate::output::OutputContext;
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use std::env;
    use std::path::PathBuf;

    use tempfile::TempDir;

    struct DirGuard {
        previous: PathBuf,
    }

    impl DirGuard {
        fn new(target: &std::path::Path) -> Self {
            let previous = env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
            env::set_current_dir(target).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    fn make_issue(id: &str, title: &str) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            ..Issue::default()
        }
    }

    // =========================================================================
    // CloseArgs tests
    // =========================================================================

    #[test]
    fn test_close_args_default() {
        let args = CloseArgs::default();
        assert!(args.ids.is_empty());
        assert!(args.reason.is_none());
        assert!(!args.force);
        assert!(args.session.is_none());
        assert!(!args.suggest_next);
    }

    #[test]
    fn test_close_args_with_all_fields() {
        let args = CloseArgs {
            ids: vec!["bd-abc".to_string(), "bd-xyz".to_string()],
            reason: Some("Fixed in PR #123".to_string()),
            force: true,
            session: Some("session-456".to_string()),
            suggest_next: true,
        };
        assert_eq!(args.ids.len(), 2);
        assert_eq!(args.ids[0], "bd-abc");
        assert_eq!(args.reason.as_deref(), Some("Fixed in PR #123"));
        assert!(args.force);
        assert_eq!(args.session.as_deref(), Some("session-456"));
        assert!(args.suggest_next);
    }

    // =========================================================================
    // CloseResult serialization tests
    // =========================================================================

    #[test]
    fn test_close_result_serialization_empty_skipped_omitted() {
        let result = CloseResult {
            closed: vec![ClosedIssue {
                id: "bd-123".to_string(),
                title: "Test issue".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: None,
            }],
            skipped: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        // Empty skipped should be omitted due to skip_serializing_if
        assert!(!json.contains("\"skipped\""));
        assert!(json.contains("\"closed\""));
    }

    #[test]
    fn test_close_result_serialization_with_skipped() {
        let result = CloseResult {
            closed: vec![],
            skipped: vec![SkippedIssue {
                id: "bd-456".to_string(),
                reason: "already closed".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"skipped\""));
        assert!(json.contains("\"reason\":\"already closed\""));
    }

    #[test]
    fn test_close_result_roundtrip() {
        let result = CloseResult {
            closed: vec![
                ClosedIssue {
                    id: "bd-a".to_string(),
                    title: "First".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:00Z".to_string(),
                    close_reason: Some("Done".to_string()),
                },
                ClosedIssue {
                    id: "bd-b".to_string(),
                    title: "Second".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-02T00:00:00Z".to_string(),
                    close_reason: None,
                },
            ],
            skipped: vec![SkippedIssue {
                id: "bd-c".to_string(),
                reason: "blocked by: bd-d".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.closed.len(), 2);
        assert_eq!(parsed.skipped.len(), 1);
        assert_eq!(parsed.closed[0].id, "bd-a");
        assert_eq!(parsed.closed[0].close_reason.as_deref(), Some("Done"));
        assert!(parsed.closed[1].close_reason.is_none());
    }

    // =========================================================================
    // CloseWithSuggestResult serialization tests
    // =========================================================================

    #[test]
    fn test_close_with_suggest_result_serialization() {
        let result = CloseWithSuggestResult {
            closed: vec![ClosedIssue {
                id: "bd-parent".to_string(),
                title: "Parent task".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-15T10:00:00Z".to_string(),
                close_reason: Some("Completed".to_string()),
            }],
            skipped: vec![],
            unblocked: vec![
                UnblockedIssue {
                    id: "bd-child1".to_string(),
                    title: "Child task 1".to_string(),
                    priority: 1,
                },
                UnblockedIssue {
                    id: "bd-child2".to_string(),
                    title: "Child task 2".to_string(),
                    priority: 2,
                },
            ],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"unblocked\""));
        assert!(json.contains("\"bd-child1\""));
        assert!(json.contains("\"bd-child2\""));
        assert!(json.contains("\"priority\":1"));
        assert!(json.contains("\"priority\":2"));
        // Empty skipped should be omitted
        assert!(!json.contains("\"skipped\""));
    }

    #[test]
    fn test_close_with_suggest_result_empty_unblocked() {
        let result = CloseWithSuggestResult {
            closed: vec![],
            skipped: vec![SkippedIssue {
                id: "bd-x".to_string(),
                reason: "not found".to_string(),
            }],
            unblocked: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        // unblocked is not marked skip_serializing_if, so it should appear as empty array
        assert!(json.contains("\"unblocked\":[]"));
        assert!(json.contains("\"skipped\""));
    }

    // =========================================================================
    // ClosedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_closed_issue_serialization_with_reason() {
        let issue = ClosedIssue {
            id: "bd-test".to_string(),
            title: "Test issue".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-01-17T08:00:00Z".to_string(),
            close_reason: Some("Fixed in commit abc123".to_string()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        assert!(json.contains("\"close_reason\":\"Fixed in commit abc123\""));
    }

    #[test]
    fn test_closed_issue_serialization_without_reason() {
        let issue = ClosedIssue {
            id: "bd-test".to_string(),
            title: "Test issue".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-01-17T08:00:00Z".to_string(),
            close_reason: None,
        };
        let json = serde_json::to_string(&issue).unwrap();
        // close_reason should be omitted due to skip_serializing_if
        assert!(!json.contains("close_reason"));
    }

    #[test]
    fn test_closed_issue_all_fields() {
        let issue = ClosedIssue {
            id: "beads_rust-xyz".to_string(),
            title: "Multi-word title with special chars: <>&".to_string(),
            status: "closed".to_string(),
            closed_at: "2026-12-31T23:59:59Z".to_string(),
            close_reason: Some("End of year cleanup".to_string()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let parsed: ClosedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "beads_rust-xyz");
        assert!(parsed.title.contains("<>&"));
        assert_eq!(parsed.status, "closed");
        assert!(parsed.closed_at.contains("2026-12-31"));
    }

    // =========================================================================
    // SkippedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_skipped_issue_serialization() {
        let skipped = SkippedIssue {
            id: "bd-skip".to_string(),
            reason: "already closed".to_string(),
        };
        let json = serde_json::to_string(&skipped).unwrap();
        assert!(json.contains("\"id\":\"bd-skip\""));
        assert!(json.contains("\"reason\":\"already closed\""));
    }

    #[test]
    fn test_skipped_issue_blocked_reason() {
        let skipped = SkippedIssue {
            id: "bd-blocked".to_string(),
            reason: "blocked by: bd-dep1, bd-dep2".to_string(),
        };
        let json = serde_json::to_string(&skipped).unwrap();
        let parsed: SkippedIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "bd-blocked");
        assert!(parsed.reason.contains("bd-dep1"));
        assert!(parsed.reason.contains("bd-dep2"));
    }

    // =========================================================================
    // UnblockedIssue serialization tests
    // =========================================================================

    #[test]
    fn test_unblocked_issue_serialization() {
        let unblocked = UnblockedIssue {
            id: "bd-next".to_string(),
            title: "Next task".to_string(),
            priority: 1,
        };
        let json = serde_json::to_string(&unblocked).unwrap();
        assert!(json.contains("\"id\":\"bd-next\""));
        assert!(json.contains("\"title\":\"Next task\""));
        assert!(json.contains("\"priority\":1"));
    }

    #[test]
    fn test_unblocked_issue_priority_boundaries() {
        for priority in [0, 1, 2, 3, 4] {
            let unblocked = UnblockedIssue {
                id: format!("bd-p{priority}"),
                title: format!("Priority {priority} task"),
                priority,
            };
            let json = serde_json::to_string(&unblocked).unwrap();
            let parsed: UnblockedIssue = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.priority, priority);
        }
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn test_close_result_multiple_closed_multiple_skipped() {
        let result = CloseResult {
            closed: vec![
                ClosedIssue {
                    id: "bd-1".to_string(),
                    title: "Task 1".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:00Z".to_string(),
                    close_reason: None,
                },
                ClosedIssue {
                    id: "bd-2".to_string(),
                    title: "Task 2".to_string(),
                    status: "closed".to_string(),
                    closed_at: "2026-01-01T00:00:01Z".to_string(),
                    close_reason: Some("Batch close".to_string()),
                },
            ],
            skipped: vec![
                SkippedIssue {
                    id: "bd-3".to_string(),
                    reason: "issue not found".to_string(),
                },
                SkippedIssue {
                    id: "bd-4".to_string(),
                    reason: "already tombstone".to_string(),
                },
            ],
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        let parsed: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.closed.len(), 2);
        assert_eq!(parsed.skipped.len(), 2);
    }

    #[test]
    fn test_render_close_json_preserves_bare_array_for_pure_success() {
        let json = build_close_json_payload(
            &CloseArgs::default(),
            vec![ClosedIssue {
                id: "bd-1".to_string(),
                title: "Task 1".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: Some("done".to_string()),
            }],
            vec![],
            vec![],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
    }

    #[test]
    fn test_close_result_shape_with_skipped_is_wrapped() {
        let json = build_close_json_payload(
            &CloseArgs::default(),
            vec![ClosedIssue {
                id: "bd-1".to_string(),
                title: "Task 1".to_string(),
                status: "closed".to_string(),
                closed_at: "2026-01-01T00:00:00Z".to_string(),
                close_reason: Some("done".to_string()),
            }],
            vec![SkippedIssue {
                id: "bd-2".to_string(),
                reason: "blocked by: bd-3".to_string(),
            }],
            vec![],
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["closed"][0]["id"], "bd-1");
        assert_eq!(parsed["skipped"][0]["id"], "bd-2");
    }

    #[test]
    fn test_close_args_clone() {
        let args = CloseArgs {
            ids: vec!["bd-clone".to_string()],
            reason: Some("Clone test".to_string()),
            force: true,
            session: Some("sess".to_string()),
            suggest_next: true,
        };
        let cloned = args.clone();
        assert_eq!(cloned.ids, args.ids);
        assert_eq!(cloned.reason, args.reason);
        assert_eq!(cloned.force, args.force);
        assert_eq!(cloned.session, args.session);
        assert_eq!(cloned.suggest_next, args.suggest_next);
    }

    #[test]
    fn test_close_args_debug_impl() {
        let args = CloseArgs::default();
        let debug_str = format!("{args:?}");
        assert!(debug_str.contains("CloseArgs"));
        assert!(debug_str.contains("ids"));
        assert!(debug_str.contains("reason"));
    }

    #[test]
    fn execute_with_args_closes_requested_blocker_chain_in_one_batch() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        storage
            .create_issue(&make_issue("bd-blocker", "Batch blocker"), "tester")
            .expect("create blocker");
        storage
            .create_issue(&make_issue("bd-blocked", "Batch blocked"), "tester")
            .expect("create blocked");
        storage
            .add_dependency(
                "bd-blocked",
                "bd-blocker",
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .expect("add dependency");
        storage.rebuild_blocked_cache(true).expect("rebuild cache");
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-blocked".to_string(), "bd-blocker".to_string()],
            ..CloseArgs::default()
        };
        execute_with_args(&args, false, &CliOverrides::default(), &ctx).expect("close batch");

        let storage = SqliteStorage::open(&db_path).expect("reopen storage");
        let blocker = storage
            .get_issue("bd-blocker")
            .expect("get blocker")
            .expect("blocker exists");
        let blocked_issue = storage
            .get_issue("bd-blocked")
            .expect("get blocked")
            .expect("blocked exists");

        assert_eq!(blocker.status, Status::Closed);
        assert_eq!(blocked_issue.status, Status::Closed);
    }

    #[test]
    fn execute_with_args_returns_nothing_to_do_when_all_requested_issues_are_skipped() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().expect("tempdir");
        let ctx = OutputContext::from_flags(false, false, true);
        commands::init::execute(None, false, Some(temp.path()), &ctx).expect("init");

        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("storage");
        let mut issue = make_issue("bd-closed", "Already closed");
        issue.status = Status::Closed;
        issue.closed_at = Some(Utc::now());
        storage
            .create_issue(&issue, "tester")
            .expect("create closed issue");

        let _guard = DirGuard::new(temp.path());
        let args = CloseArgs {
            ids: vec!["bd-closed".to_string()],
            ..CloseArgs::default()
        };

        let err = execute_with_args(&args, true, &CliOverrides::default(), &ctx)
            .expect_err("all-skipped close should fail");
        assert!(matches!(err, BeadsError::NothingToDo { .. }));
    }
}
