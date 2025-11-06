// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use itertools::Itertools as _;
use jj_lib::copies::CopyRecords;
use jj_lib::merge::Diff;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::revset::RevsetExpression;
use jj_lib::revset::RevsetFilterPredicate;
use jj_lib::working_copy::SnapshotStats;
use pollster::FutureExt as _;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::print_conflicted_paths;
use crate::cli_util::print_snapshot_stats;
use crate::command_error::CommandError;
use crate::diff_util::get_copy_records;
use crate::formatter::FormatterExt as _;
use crate::ui::Ui;

/// Show high-level repo status [default alias: st]
///
/// This includes:
///
/// * The working copy commit and its parents, and a summary of the changes in
///   the working copy (compared to the merged parents)
///
/// * Conflicts in the working copy
///
/// * [Conflicted bookmarks]
///
/// [Conflicted bookmarks]:
///     https://jj-vcs.github.io/jj/latest/bookmarks/#conflicts
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct StatusArgs {
    /// Restrict the status display to these paths
    #[arg(value_name = "FILESETS", value_hint = clap::ValueHint::AnyPath)]
    paths: Vec<String>,
    /// Render status using the given template
    ///
    /// For the syntax, see https://jj-vcs.github.io/jj/latest/templates/
    ///
    /// TODO: document this
    #[arg(long, short = 'T')]
    template: Option<String>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_status(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &StatusArgs,
) -> Result<(), CommandError> {
    let (workspace_command, snapshot_stats) = command.workspace_helper_with_stats(ui)?;

    print_snapshot_stats(
        ui,
        &snapshot_stats,
        workspace_command.env().path_converter(),
    )?;
    let repo = workspace_command.repo();
    let maybe_wc_commit = workspace_command
        .get_wc_commit_id()
        .map(|id| repo.store().get_commit(id))
        .transpose()?;
    let fileset_expression = workspace_command.parse_file_patterns(ui, &args.paths)?;
    let matcher = fileset_expression.to_matcher();
    ui.request_pager();
    let mut formatter = ui.stdout_formatter();
    let formatter = formatter.as_mut();

    // Get template from args or config (defaults to templates.status in config)
    let status_template_text = match &args.template {
        Some(value) => value.clone(),
        None => workspace_command.settings().get_string("templates.status")?,
    };

    if let Some(wc_commit) = &maybe_wc_commit {
        let parent_tree = wc_commit.parent_tree(repo.as_ref())?;
        let tree = wc_commit.tree();

        // Render status sections using templates
        let mut copy_records = CopyRecords::default();
        for parent in wc_commit.parent_ids() {
            let records = get_copy_records(repo.store(), parent, wc_commit.id(), &matcher)?;
            copy_records.add_records(records)?;
        }

        // Collect parent commits for template
        let parents: Result<Vec<_>, _> = wc_commit.parents().collect();
        let parents = parents?;

        let status = build_working_copy_status(
            &parent_tree,
            &tree,
            &matcher,
            &copy_records,
            &snapshot_stats,
            repo.as_ref(),
            Some(wc_commit.clone()),
            parents,
        )
        .block_on()?;

        let template = workspace_command.parse_status_template(ui, &status_template_text)?;
        template.format(&status, formatter)?;

        // Commits are now rendered by the template
        // Check for conflicts in working copy
        if args.template.is_none() && wc_commit.has_conflict() {
            // TODO: Conflicts should also be filtered by the `matcher`. See the related
            // TODO on `MergedTree::conflicts()`.
            let conflicts = wc_commit.tree().conflicts().collect_vec();
            writeln!(
                formatter.labeled("warning").with_heading("Warning: "),
                "There are unresolved conflicts at these paths:"
            )?;
            print_conflicted_paths(conflicts, formatter, &workspace_command)?;

            let wc_revset = RevsetExpression::commit(wc_commit.id().clone());

            // Ancestors with conflicts, excluding the current working copy commit.
            let ancestors_conflicts: Vec<_> = workspace_command
                .attach_revset_evaluator(
                    wc_revset
                        .parents()
                        .ancestors()
                        .filtered(RevsetFilterPredicate::HasConflict)
                        .minus(&workspace_command.env().immutable_expression()),
                )
                .evaluate_to_commit_ids()?
                .try_collect()?;

            workspace_command.report_repo_conflicts(formatter, repo, ancestors_conflicts)?;
        } else {
            for parent in wc_commit.parents() {
                let parent = parent?;
                if parent.has_conflict() {
                    writeln!(
                        formatter.labeled("hint").with_heading("Hint: "),
                        "Conflict in parent commit has been resolved in working copy"
                    )?;
                    break;
                }
            }
        }

        // Bookmark conflicts are now rendered by the template
    } else {
        writeln!(formatter, "No working copy")?;
    }

    Ok(())
}

fn diff_entry_to_file_change(
    path: &jj_lib::copies::CopiesTreeDiffEntryPath,
    values: &Diff<jj_lib::merge::MergedTreeValue>,
) -> crate::status_templater::StatusEntry {
    use crate::status_templater::{StatusEntry, FileStatus};
    use jj_lib::copies::CopyOperation;

    let target_path = path.target.clone();

    // Determine status based on copy operation and before/after presence
    let (status, copy_source) = if let Some((source_path, op)) = &path.source {
        let status = match op {
            CopyOperation::Copy => FileStatus::Copied,
            CopyOperation::Rename => FileStatus::Renamed,
        };
        (status, Some(source_path.clone()))
    } else {
        let status = match (values.before.is_present(), values.after.is_present()) {
            (true, true) => FileStatus::Modified,
            (false, true) => FileStatus::Added,
            (true, false) => FileStatus::Deleted,
            (false, false) => panic!("values pair must differ"),
        };
        (status, None)
    };

    if let Some(source) = copy_source {
        StatusEntry::with_copy_source(target_path, status, source)
    } else {
        StatusEntry::new(target_path, status)
    }
}

/// Extracts file changes from tree diff for template rendering
async fn extract_file_changes(
    parent_tree: &MergedTree,
    tree: &MergedTree,
    matcher: &dyn jj_lib::matchers::Matcher,
    copy_records: &CopyRecords,
) -> Result<Vec<crate::status_templater::StatusEntry>, CommandError> {
    use futures::StreamExt;
    use jj_lib::copies::CopiesTreeDiffEntry;

    let mut file_changes = Vec::new();
    let mut diff_stream = parent_tree.diff_stream_with_copies(tree, matcher, copy_records);

    while let Some(CopiesTreeDiffEntry { path, values }) = diff_stream.next().await {
        let values = values?;
        let file_change = diff_entry_to_file_change(&path, &values);
        file_changes.push(file_change);
    }

    Ok(file_changes)
}

/// Extracts conflicts from tree for template rendering
fn extract_conflicts(
    tree: &MergedTree,
) -> Result<Vec<crate::status_templater::ConflictInfo>, CommandError> {
    use crate::status_templater::ConflictInfo;

    let conflicts: Result<Vec<ConflictInfo>, _> = tree
        .conflicts()
        .map(|(path, conflict)| {
            // Propagate errors instead of silently ignoring them
            conflict.map(|conflict| {
                let num_sides = conflict.num_sides();
                ConflictInfo::new(path, num_sides)
            })
        })
        .collect();

    Ok(conflicts?)
}

/// Extracts bookmark conflicts for template rendering
fn extract_bookmark_conflicts(
    repo: &dyn jj_lib::repo::Repo,
) -> (Vec<crate::status_templater::BookmarkConflict>, Vec<crate::status_templater::BookmarkConflict>) {
    use crate::status_templater::BookmarkConflict;

    let local_conflicts: Vec<BookmarkConflict> = repo
        .view()
        .local_bookmarks()
        .filter(|(_, target)| target.has_conflict())
        .map(|(name, _)| BookmarkConflict::new(name.as_str().to_owned()))
        .collect();

    let remote_conflicts: Vec<BookmarkConflict> = repo
        .view()
        .all_remote_bookmarks()
        .filter(|(_, remote_ref)| remote_ref.target.has_conflict())
        .map(|(symbol, _)| {
            BookmarkConflict::new_remote(
                symbol.name.as_str().to_owned(),
                symbol.remote.as_str().to_owned(),
            )
        })
        .collect();

    (local_conflicts, remote_conflicts)
}

/// Extracts collapsed untracked paths for template rendering
async fn extract_untracked_paths(
    untracked_paths: impl IntoIterator<Item = impl AsRef<RepoPath>>,
    tree: &MergedTree,
) -> Result<Vec<RepoPathBuf>, CommandError> {
    let mut paths = Vec::new();

    visit_collapsed_untracked_files(
        untracked_paths,
        tree.clone(),
        |path, _is_dir| {
            paths.push(path.to_owned());
            Ok(())
        },
    )
    .await?;

    Ok(paths)
}

/// Builds a complete WorkingCopyStatus object for template rendering
async fn build_working_copy_status(
    parent_tree: &MergedTree,
    tree: &MergedTree,
    matcher: &dyn jj_lib::matchers::Matcher,
    copy_records: &CopyRecords,
    snapshot_stats: &SnapshotStats,
    repo: &dyn jj_lib::repo::Repo,
    working_copy: Option<jj_lib::commit::Commit>,
    parents: Vec<jj_lib::commit::Commit>,
) -> Result<crate::status_templater::WorkingCopyStatus, CommandError> {
    use crate::status_templater::WorkingCopyStatus;

    // Extract all data using helper functions
    let file_changes = extract_file_changes(parent_tree, tree, matcher, copy_records).await?;
    let untracked_paths = extract_untracked_paths(snapshot_stats.untracked_paths.keys(), tree).await?;
    let conflicts = extract_conflicts(tree)?;
    let (local_bookmark_conflicts, remote_bookmark_conflicts) = extract_bookmark_conflicts(repo);

    Ok(WorkingCopyStatus {
        file_changes,
        untracked_paths,
        conflicts,
        local_bookmark_conflicts,
        remote_bookmark_conflicts,
        working_copy,
        parents,
    })
}

async fn visit_collapsed_untracked_files(
    untracked_paths: impl IntoIterator<Item = impl AsRef<RepoPath>>,
    tree: MergedTree,
    mut on_path: impl FnMut(&RepoPath, bool) -> Result<(), CommandError>,
) -> Result<(), CommandError> {
    let trees = tree.trees()?;
    let mut stack = vec![trees];

    // TODO: This loop can be improved with BTreeMap cursors once that's stable,
    // would remove the need for the whole `skip_prefixed_by` thing and turn it
    // into a B-tree lookup.
    let mut skip_prefixed_by_dir: Option<RepoPathBuf> = None;
    'untracked: for path in untracked_paths {
        let path = path.as_ref();
        if skip_prefixed_by_dir
            .as_ref()
            .is_some_and(|p| path.starts_with(p))
        {
            continue;
        } else {
            skip_prefixed_by_dir = None;
        }

        let mut it = path.components().dropping_back(1);
        let first_mismatch = it.by_ref().enumerate().find(|(i, component)| {
            stack.get(i + 1).is_none_or(|tree| {
                tree.dir()
                    .components()
                    .next_back()
                    .expect("should always have at least one element (the root)")
                    != *component
            })
        });

        if let Some((i, component)) = first_mismatch {
            stack.truncate(i + 1);
            for component in std::iter::once(component).chain(it) {
                let parent = stack
                    .last()
                    .expect("should always have at least one element (the root)");

                if let Some(subtree) = parent.sub_tree(component).await? {
                    stack.push(subtree);
                } else {
                    let dir = parent.dir().join(component);

                    on_path(&dir, true)?;
                    skip_prefixed_by_dir = Some(dir);

                    continue 'untracked;
                }
            }
        }

        on_path(path, false)?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use testutils::TestRepo;
    use testutils::TestTreeBuilder;
    use testutils::repo_path;

    use super::*;

    fn collect_collapsed_untracked_files_string(
        untracked_paths: &[&RepoPath],
        tree: MergedTree,
    ) -> String {
        let mut result = String::new();
        visit_collapsed_untracked_files(untracked_paths, tree, |path, is_dir| {
            result.push_str("? ");
            if is_dir {
                result.push_str(&path.to_internal_dir_string());
            } else {
                result.push_str(path.as_internal_file_string());
            }
            result.push('\n');
            Ok(())
        })
        .block_on()
        .unwrap();
        result
    }

    #[test]
    fn test_collapsed_untracked_files() {
        let repo = TestRepo::init();

        let tracked = {
            let mut builder = TestTreeBuilder::new(repo.repo.store().clone());

            builder.file(repo_path("top_level_file"), "");
            // ? "untracked_top_level_file"
            // ? "dir"
            // ? "dir2/c"
            builder.file(repo_path("dir2/d"), "");
            // ? "dir3/partially_tracked/e"
            builder.file(repo_path("dir3/partially_tracked/f"), "");
            // ? "dir3/fully_untracked/"
            builder.file(repo_path("dir3/j"), "");
            // ? "dir3/k"

            builder.write_merged_tree()
        };
        let untracked = &[
            repo_path("untracked_top_level_file"),
            repo_path("dir/a"),
            repo_path("dir/b"),
            repo_path("dir2/c"),
            repo_path("dir3/partially_tracked/e"),
            repo_path("dir3/fully_untracked/g"),
            repo_path("dir3/fully_untracked/h"),
            repo_path("dir3/k"),
        ];

        insta::assert_snapshot!(
            collect_collapsed_untracked_files_string(untracked, tracked),
            @r"
        ? untracked_top_level_file
        ? dir/
        ? dir2/c
        ? dir3/partially_tracked/e
        ? dir3/fully_untracked/
        ? dir3/k
        "
        );
    }
}
