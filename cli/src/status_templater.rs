// Copyright 2025 The Jujutsu Authors
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

//! Template environment for `jj status`.

use jj_lib::commit::Commit;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;

use crate::commit_templater::CommitTemplateEnvironment;
use crate::commit_templater::CommitTemplateLanguage;
use crate::template_builder::BuildContext;
use crate::template_builder::CoreTemplateBuildFnTable;
use crate::template_builder::CoreTemplatePropertyKind;
use crate::template_builder::CoreTemplatePropertyVar;
use crate::template_builder::TemplateLanguage;
use crate::template_parser::FunctionCallNode;
use crate::template_parser::TemplateDiagnostics;
use crate::template_parser::TemplateParseResult;
use crate::templater::BoxedSerializeProperty;
use crate::templater::BoxedTemplateProperty;
use crate::templater::ListTemplate;
use crate::templater::Template;
use crate::templater::TemplatePropertyExt as _;

/// Status of a file in the working copy
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

impl FileStatus {
    /// Returns the label for styling (e.g., "added", "removed")
    /// These match the color config labels like "diff added", "diff removed"
    pub fn label(&self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "removed",  // matches "diff removed" color label
            FileStatus::Renamed => "renamed",
            FileStatus::Copied => "copied",
        }
    }

    /// Returns the single character representation (e.g., 'A', 'M')
    pub fn char(&self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
        }
    }

    /// Returns the status as a string (same as char but as &str)
    pub fn as_str(&self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Copied => "C",
        }
    }
}

impl std::fmt::Display for FileStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A single entry in the status output representing a file change
#[derive(Clone, Debug)]
pub struct StatusEntry {
    pub path: RepoPathBuf,
    pub status: FileStatus,
    pub copy_source: Option<RepoPathBuf>,
}

impl StatusEntry {
    pub fn new(path: RepoPathBuf, status: FileStatus) -> Self {
        Self {
            path,
            status,
            copy_source: None,
        }
    }

    pub fn with_copy_source(path: RepoPathBuf, status: FileStatus, copy_source: RepoPathBuf) -> Self {
        Self {
            path,
            status,
            copy_source: Some(copy_source),
        }
    }

    /// Returns true if this is a copy or rename operation
    pub fn has_copy_source(&self) -> bool {
        self.copy_source.is_some()
    }
}

/// Information about conflicts in the working copy
#[derive(Clone, Debug)]
pub struct ConflictInfo {
    pub path: RepoPathBuf,
    pub num_sides: usize,
}

impl ConflictInfo {
    pub fn new(path: RepoPathBuf, num_sides: usize) -> Self {
        Self { path, num_sides }
    }

    /// Returns a description of the conflict sides (e.g., "2-sided conflict")
    pub fn sides_description(&self) -> String {
        format!("{}-sided conflict", self.num_sides)
    }
}

/// Information about bookmark conflicts
#[derive(Clone, Debug)]
pub struct BookmarkConflict {
    pub name: String,
    pub remote: Option<String>,
}

impl BookmarkConflict {
    pub fn new(name: String) -> Self {
        Self { name, remote: None }
    }

    pub fn new_remote(name: String, remote: String) -> Self {
        Self {
            name,
            remote: Some(remote),
        }
    }

    /// Returns the full name including remote if applicable
    pub fn full_name(&self) -> String {
        if let Some(remote) = &self.remote {
            format!("{}@{}", self.name, remote)
        } else {
            self.name.clone()
        }
    }
}

/// Overall status information for a working copy
#[derive(Clone, Debug)]
pub struct WorkingCopyStatus {
    pub file_changes: Vec<StatusEntry>,
    pub untracked_paths: Vec<RepoPathBuf>,
    pub conflicts: Vec<ConflictInfo>,
    pub local_bookmark_conflicts: Vec<BookmarkConflict>,
    pub remote_bookmark_conflicts: Vec<BookmarkConflict>,
    pub working_copy: Option<Commit>,
    pub parents: Vec<Commit>,
}

impl WorkingCopyStatus {
    pub fn new() -> Self {
        Self {
            file_changes: Vec::new(),
            untracked_paths: Vec::new(),
            conflicts: Vec::new(),
            local_bookmark_conflicts: Vec::new(),
            remote_bookmark_conflicts: Vec::new(),
            working_copy: None,
            parents: Vec::new(),
        }
    }

    pub fn has_file_changes(&self) -> bool {
        !self.file_changes.is_empty()
    }

    pub fn has_untracked_paths(&self) -> bool {
        !self.untracked_paths.is_empty()
    }

    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    pub fn has_bookmark_conflicts(&self) -> bool {
        !self.local_bookmark_conflicts.is_empty() || !self.remote_bookmark_conflicts.is_empty()
    }

    pub fn has_local_bookmark_conflicts(&self) -> bool {
        !self.local_bookmark_conflicts.is_empty()
    }

    pub fn has_remote_bookmark_conflicts(&self) -> bool {
        !self.remote_bookmark_conflicts.is_empty()
    }
}

impl Default for WorkingCopyStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// Property types for status templates
pub enum StatusTemplatePropertyKind<'repo> {
    /// Core template types (String, Integer, Boolean, etc.)
    Core(CoreTemplatePropertyKind<'repo>),
    /// Commit template types (allows full commit template functionality)
    CommitTemplate(crate::commit_templater::CommitTemplatePropertyKind<'repo>),
    /// Working copy status (the "self" in status templates)
    WorkingCopyStatus(BoxedTemplateProperty<'repo, WorkingCopyStatus>),
    /// File change in working copy
    StatusEntry(BoxedTemplateProperty<'repo, StatusEntry>),
    /// List of file changes
    StatusEntryList(BoxedTemplateProperty<'repo, Vec<StatusEntry>>),
    /// Repository path
    RepoPath(BoxedTemplateProperty<'repo, RepoPathBuf>),
    /// List of repository paths
    RepoPathList(BoxedTemplateProperty<'repo, Vec<RepoPathBuf>>),
    /// Conflict information
    ConflictInfo(BoxedTemplateProperty<'repo, ConflictInfo>),
    /// List of conflicts
    ConflictInfoList(BoxedTemplateProperty<'repo, Vec<ConflictInfo>>),
    /// Bookmark conflict
    BookmarkConflict(BoxedTemplateProperty<'repo, BookmarkConflict>),
    /// List of bookmark conflicts
    BookmarkConflictList(BoxedTemplateProperty<'repo, Vec<BookmarkConflict>>),
}

// Implement WrapTemplateProperty for all core types (String, bool, i64, etc.)
crate::template_builder::impl_core_property_wrappers!(<'repo> StatusTemplatePropertyKind<'repo> => Core);

// Delegate commit types to CommitTemplate variant (enables full commit template functionality)
crate::commit_templater::impl_commit_property_wrappers!(<'repo> StatusTemplatePropertyKind<'repo> => CommitTemplate);

// Implement WrapTemplateProperty for status-specific types
impl<'repo> crate::templater::WrapTemplateProperty<'repo, WorkingCopyStatus> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, WorkingCopyStatus>) -> Self {
        StatusTemplatePropertyKind::WorkingCopyStatus(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, StatusEntry> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, StatusEntry>) -> Self {
        StatusTemplatePropertyKind::StatusEntry(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, Vec<StatusEntry>> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, Vec<StatusEntry>>) -> Self {
        StatusTemplatePropertyKind::StatusEntryList(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, RepoPathBuf> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, RepoPathBuf>) -> Self {
        StatusTemplatePropertyKind::RepoPath(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, Vec<RepoPathBuf>> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, Vec<RepoPathBuf>>) -> Self {
        StatusTemplatePropertyKind::RepoPathList(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, ConflictInfo> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, ConflictInfo>) -> Self {
        StatusTemplatePropertyKind::ConflictInfo(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, Vec<ConflictInfo>> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, Vec<ConflictInfo>>) -> Self {
        StatusTemplatePropertyKind::ConflictInfoList(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, BookmarkConflict> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, BookmarkConflict>) -> Self {
        StatusTemplatePropertyKind::BookmarkConflict(property)
    }
}

impl<'repo> crate::templater::WrapTemplateProperty<'repo, Vec<BookmarkConflict>> for StatusTemplatePropertyKind<'repo> {
    fn wrap_property(property: BoxedTemplateProperty<'repo, Vec<BookmarkConflict>>) -> Self {
        StatusTemplatePropertyKind::BookmarkConflictList(property)
    }
}

impl<'repo> CoreTemplatePropertyVar<'repo> for StatusTemplatePropertyKind<'repo> {
    fn wrap_template(template: Box<dyn Template + 'repo>) -> Self {
        Self::Core(CoreTemplatePropertyKind::wrap_template(template))
    }

    fn wrap_list_template(template: Box<dyn ListTemplate + 'repo>) -> Self {
        Self::Core(CoreTemplatePropertyKind::wrap_list_template(template))
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::Core(property) => property.type_name(),
            Self::CommitTemplate(property) => property.type_name(),
            Self::WorkingCopyStatus(_) => "WorkingCopyStatus",
            Self::StatusEntry(_) => "StatusEntry",
            Self::StatusEntryList(_) => "List<StatusEntry>",
            Self::RepoPath(_) => "RepoPath",
            Self::RepoPathList(_) => "List<RepoPath>",
            Self::ConflictInfo(_) => "ConflictInfo",
            Self::ConflictInfoList(_) => "List<ConflictInfo>",
            Self::BookmarkConflict(_) => "BookmarkConflict",
            Self::BookmarkConflictList(_) => "List<BookmarkConflict>",
        }
    }

    fn try_into_boolean(self) -> Option<BoxedTemplateProperty<'repo, bool>> {
        match self {
            Self::Core(property) => property.try_into_boolean(),
            Self::CommitTemplate(property) => property.try_into_boolean(),
            Self::WorkingCopyStatus(_)
            | Self::StatusEntry(_)
            | Self::StatusEntryList(_)
            | Self::RepoPath(_)
            | Self::RepoPathList(_)
            | Self::ConflictInfo(_)
            | Self::ConflictInfoList(_)
            | Self::BookmarkConflict(_)
            | Self::BookmarkConflictList(_) => None,
        }
    }

    fn try_into_integer(self) -> Option<BoxedTemplateProperty<'repo, i64>> {
        match self {
            Self::Core(property) => property.try_into_integer(),
            Self::CommitTemplate(property) => property.try_into_integer(),
            Self::WorkingCopyStatus(_)
            | Self::StatusEntry(_)
            | Self::StatusEntryList(_)
            | Self::RepoPath(_)
            | Self::RepoPathList(_)
            | Self::ConflictInfo(_)
            | Self::ConflictInfoList(_)
            | Self::BookmarkConflict(_)
            | Self::BookmarkConflictList(_) => None,
        }
    }

    fn try_into_stringify(self) -> Option<BoxedTemplateProperty<'repo, String>> {
        match self {
            Self::Core(property) => property.try_into_stringify(),
            Self::CommitTemplate(property) => property.try_into_stringify(),
            Self::WorkingCopyStatus(_)
            | Self::StatusEntry(_)
            | Self::StatusEntryList(_)
            | Self::RepoPath(_)
            | Self::RepoPathList(_)
            | Self::ConflictInfo(_)
            | Self::ConflictInfoList(_)
            | Self::BookmarkConflict(_)
            | Self::BookmarkConflictList(_) => None,
        }
    }

    fn try_into_serialize(self) -> Option<BoxedSerializeProperty<'repo>> {
        match self {
            Self::Core(property) => property.try_into_serialize(),
            Self::CommitTemplate(property) => property.try_into_serialize(),
            Self::WorkingCopyStatus(_)
            | Self::StatusEntry(_)
            | Self::StatusEntryList(_)
            | Self::RepoPath(_)
            | Self::RepoPathList(_)
            | Self::ConflictInfo(_)
            | Self::ConflictInfoList(_)
            | Self::BookmarkConflict(_)
            | Self::BookmarkConflictList(_) => None,
        }
    }

    fn try_into_template(self) -> Option<Box<dyn Template + 'repo>> {
        match self {
            Self::Core(property) => property.try_into_template(),
            Self::CommitTemplate(property) => property.try_into_template(),
            Self::WorkingCopyStatus(_)
            | Self::StatusEntry(_)
            | Self::StatusEntryList(_)
            | Self::RepoPath(_)
            | Self::RepoPathList(_)
            | Self::ConflictInfo(_)
            | Self::ConflictInfoList(_)
            | Self::BookmarkConflict(_)
            | Self::BookmarkConflictList(_) => None,
        }
    }

    fn try_into_eq(self, other: Self) -> Option<BoxedTemplateProperty<'repo, bool>> {
        match (self, other) {
            (Self::Core(lhs), Self::Core(rhs)) => lhs.try_into_eq(rhs),
            // Status-specific types don't support equality comparison
            _ => None,
        }
    }

    fn try_into_cmp(self, other: Self) -> Option<BoxedTemplateProperty<'repo, std::cmp::Ordering>> {
        match (self, other) {
            (Self::Core(lhs), Self::Core(rhs)) => lhs.try_into_cmp(rhs),
            // Status-specific types don't support ordering comparison
            _ => None,
        }
    }
}

/// Template environment for `jj status`
///
/// Provides status-specific keywords and methods for templating.
/// Similar to CommitTemplateLanguage but for status context.
pub struct StatusTemplateLanguage<'repo> {
    /// Commit template language for commit-related keywords
    /// (working_copy_commit, parent_commits)
    commit_language: CommitTemplateLanguage<'repo>,
    /// Build function table for status keywords
    build_fn_table: StatusTemplateBuildFnTable<'repo>,
}

impl<'repo> StatusTemplateLanguage<'repo> {
    /// Create a new StatusTemplateLanguage
    pub fn new(commit_language: CommitTemplateLanguage<'repo>) -> Self {
        StatusTemplateLanguage {
            commit_language,
            build_fn_table: StatusTemplateBuildFnTable::builtin(),
        }
    }
}

/// Safely transmutes a BuildContext from one property type to another.
///
/// # Safety
///
/// This is safe ONLY when the build methods being called do not access
/// `build_ctx.local_variables` or `build_ctx.self_variable`. The commit
/// template methods satisfy this constraint - they only operate on the
/// property they receive and don't use the BuildContext.
///
/// This transmute is necessary because:
/// - BuildContext<P> contains function pointers that return type P
/// - When delegating StatusTemplatePropertyKind to CommitTemplatePropertyKind,
///   we need to convert BuildContext<StatusTemplatePropertyKind> to
///   BuildContext<CommitTemplatePropertyKind>
/// - The actual BuildContext contents are never accessed by commit methods
///
/// # Arguments
///
/// * `build_ctx` - The BuildContext to transmute
///
/// # Returns
///
/// A reference to the same BuildContext with a different type parameter.
/// The underlying memory layout is identical; only the type changes.
#[inline]
unsafe fn transmute_build_context<'i, P1, P2>(
    build_ctx: &'i BuildContext<'i, P1>,
) -> &'i BuildContext<'i, P2> {
    // SAFETY: Caller must ensure that the build methods being called do not
    // access build_ctx.local_variables or build_ctx.self_variable
    unsafe { std::mem::transmute(build_ctx) }
}

impl<'repo> TemplateLanguage<'repo> for StatusTemplateLanguage<'repo> {
    type Property = StatusTemplatePropertyKind<'repo>;

    fn settings(&self) -> &UserSettings {
        self.commit_language.settings()
    }

    fn build_function(
        &self,
        diagnostics: &mut TemplateDiagnostics,
        build_ctx: &BuildContext<Self::Property>,
        function: &FunctionCallNode,
    ) -> TemplateParseResult<Self::Property> {
        // Delegate to core build function table
        let table = &self.build_fn_table.core;
        table.build_function(self, diagnostics, build_ctx, function)
    }

    fn build_method(
        &self,
        diagnostics: &mut TemplateDiagnostics,
        build_ctx: &BuildContext<Self::Property>,
        property: Self::Property,
        function: &FunctionCallNode,
    ) -> TemplateParseResult<Self::Property> {
        let _type_name = property.type_name();
        match property {
            StatusTemplatePropertyKind::Core(property) => {
                let table = &self.build_fn_table.core;
                table.build_method(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::CommitTemplate(property) => {
                // Delegate to commit template methods by dispatching to the appropriate table
                // We replicate CommitTemplateLanguage::build_method's dispatch logic here
                let type_name = property.type_name();
                use crate::commit_templater::CommitTemplatePropertyKind::*;
                let result = match property {
                    Core(prop) => {
                        let table = &self.commit_language.build_fn_table.core;
                        // SAFETY: Core methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        table.build_method(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    Operation(prop) => {
                        let table = &self.commit_language.build_fn_table.operation;
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        table.build_method(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    Commit(prop) => {
                        let table = &self.commit_language.build_fn_table.commit_methods;
                        let build = crate::template_parser::lookup_method(type_name, table, function)?;
                        // SAFETY: build_ctx type doesn't matter as commit methods don't use it
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        build(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    CommitOpt(prop) => {
                        let type_name = "Commit";
                        let table = &self.commit_language.build_fn_table.commit_methods;
                        let build = crate::template_parser::lookup_method(type_name, table, function)?;
                        let inner_property = prop.try_unwrap(type_name).into_dyn();
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        build(&self.commit_language, diagnostics, build_ctx_ref, inner_property, function)?
                    }
                    CommitList(prop) => {
                        let table = &self.commit_language.build_fn_table.commit_list_methods;
                        let build = crate::template_parser::lookup_method(type_name, table, function)?;
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        build(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    ChangeId(prop) => {
                        let table = &self.commit_language.build_fn_table.change_id_methods;
                        let build = crate::template_parser::lookup_method(type_name, table, function)?;
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        build(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    CommitId(prop) => {
                        let table = &self.commit_language.build_fn_table.commit_id_methods;
                        let build = crate::template_parser::lookup_method(type_name, table, function)?;
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        build(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    CommitRef(prop) => {
                        let table = &self.commit_language.build_fn_table.commit_ref_methods;
                        let build = crate::template_parser::lookup_method(type_name, table, function)?;
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        build(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    CommitRefList(prop) => {
                        let table = &self.commit_language.build_fn_table.commit_ref_list_methods;
                        let build = crate::template_parser::lookup_method(type_name, table, function)?;
                        // SAFETY: These methods don't access build_ctx
                        let build_ctx_ref = unsafe { transmute_build_context(build_ctx) };
                        build(&self.commit_language, diagnostics, build_ctx_ref, prop, function)?
                    }
                    // For remaining types, return error with helpful message
                    _ => return Err(crate::template_parser::TemplateParseError::expression(
                        &format!("Method delegation not yet implemented for {}", type_name),
                        function.name_span,
                    ))
                };
                Ok(StatusTemplatePropertyKind::CommitTemplate(result))
            }
            StatusTemplatePropertyKind::WorkingCopyStatus(property) => {
                let table = &self.build_fn_table.working_copy_status_methods;
                let build = crate::template_parser::lookup_method("WorkingCopyStatus", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::StatusEntry(property) => {
                let table = &self.build_fn_table.status_entry_methods;
                let build = crate::template_parser::lookup_method("StatusEntry", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::RepoPath(property) => {
                let table = &self.build_fn_table.repo_path_methods;
                let build = crate::template_parser::lookup_method("RepoPath", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::ConflictInfo(property) => {
                let table = &self.build_fn_table.conflict_info_methods;
                let build = crate::template_parser::lookup_method("ConflictInfo", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::BookmarkConflict(property) => {
                let table = &self.build_fn_table.bookmark_conflict_methods;
                let build = crate::template_parser::lookup_method("BookmarkConflict", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::StatusEntryList(property) => {
                let table = &self.build_fn_table.status_entry_list_methods;
                let build = crate::template_parser::lookup_method("List<StatusEntry>", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::RepoPathList(property) => {
                let table = &self.build_fn_table.repo_path_list_methods;
                let build = crate::template_parser::lookup_method("List<RepoPath>", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::ConflictInfoList(property) => {
                let table = &self.build_fn_table.conflict_info_list_methods;
                let build = crate::template_parser::lookup_method("List<ConflictInfo>", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            StatusTemplatePropertyKind::BookmarkConflictList(property) => {
                let table = &self.build_fn_table.bookmark_conflict_list_methods;
                let build = crate::template_parser::lookup_method("List<BookmarkConflict>", table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
        }
    }
}

/// Build function tables for status templates
pub struct StatusTemplateBuildFnTable<'repo> {
    /// Core template functions (string, list, etc.)
    pub core: CoreTemplateBuildFnTable<'repo, StatusTemplateLanguage<'repo>>,
    /// Methods available on WorkingCopyStatus type
    pub working_copy_status_methods: StatusTemplateBuildMethodFnMap<'repo, WorkingCopyStatus>,
    /// Methods available on StatusEntry type
    pub status_entry_methods: StatusTemplateBuildMethodFnMap<'repo, StatusEntry>,
    /// Methods available on list of StatusEntry
    pub status_entry_list_methods: StatusTemplateBuildMethodFnMap<'repo, Vec<StatusEntry>>,
    /// Methods available on RepoPath type
    pub repo_path_methods: StatusTemplateBuildMethodFnMap<'repo, RepoPathBuf>,
    /// Methods available on list of RepoPath
    pub repo_path_list_methods: StatusTemplateBuildMethodFnMap<'repo, Vec<RepoPathBuf>>,
    /// Methods available on ConflictInfo type
    pub conflict_info_methods: StatusTemplateBuildMethodFnMap<'repo, ConflictInfo>,
    /// Methods available on list of ConflictInfo
    pub conflict_info_list_methods: StatusTemplateBuildMethodFnMap<'repo, Vec<ConflictInfo>>,
    /// Methods available on BookmarkConflict type
    pub bookmark_conflict_methods: StatusTemplateBuildMethodFnMap<'repo, BookmarkConflict>,
    /// Methods available on list of BookmarkConflict
    pub bookmark_conflict_list_methods: StatusTemplateBuildMethodFnMap<'repo, Vec<BookmarkConflict>>,
}

type StatusTemplateBuildMethodFnMap<'repo, T> =
    crate::template_builder::TemplateBuildMethodFnMap<
        'repo,
        StatusTemplateLanguage<'repo>,
        T,
        StatusTemplatePropertyKind<'repo>,
    >;

impl<'repo> StatusTemplateBuildFnTable<'repo> {
    fn builtin() -> Self {
        StatusTemplateBuildFnTable {
            core: CoreTemplateBuildFnTable::builtin(),
            working_copy_status_methods: builtin_working_copy_status_methods(),
            status_entry_methods: builtin_status_entry_methods(),
            status_entry_list_methods: crate::template_builder::builtin_unformattable_list_methods(),
            repo_path_methods: builtin_repo_path_methods(),
            repo_path_list_methods: crate::template_builder::builtin_unformattable_list_methods(),
            conflict_info_methods: builtin_conflict_info_methods(),
            conflict_info_list_methods: crate::template_builder::builtin_unformattable_list_methods(),
            bookmark_conflict_methods: builtin_bookmark_conflict_methods(),
            bookmark_conflict_list_methods: crate::template_builder::builtin_unformattable_list_methods(),
        }
    }
}

fn builtin_working_copy_status_methods<'repo>() -> StatusTemplateBuildMethodFnMap<'repo, WorkingCopyStatus> {
    let mut map = StatusTemplateBuildMethodFnMap::<WorkingCopyStatus>::new();
    map.insert(
        "file_changes",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.file_changes.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "has_file_changes",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.has_file_changes());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "untracked_paths",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.untracked_paths.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "has_untracked_paths",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.has_untracked_paths());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "conflicts",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.conflicts.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "has_conflicts",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.has_conflicts());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "local_bookmark_conflicts",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.local_bookmark_conflicts.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "remote_bookmark_conflicts",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.remote_bookmark_conflicts.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "has_bookmark_conflicts",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.has_bookmark_conflicts());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "has_local_bookmark_conflicts",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.has_local_bookmark_conflicts());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "has_remote_bookmark_conflicts",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.has_remote_bookmark_conflicts());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "working_copy",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.working_copy.clone());
            // Wrap as CommitTemplate property kind to get full commit template functionality
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "parents",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|status| status.parents.clone());
            // Wrap as CommitTemplate property kind to get full commit template functionality
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map
}

fn builtin_status_entry_methods<'repo>() -> StatusTemplateBuildMethodFnMap<'repo, StatusEntry> {
    let mut map = StatusTemplateBuildMethodFnMap::<StatusEntry>::new();
    map.insert(
        "path",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|file_change| file_change.path.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "status",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|file_change| file_change.status.as_str().to_owned());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "status_label",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|file_change| file_change.status.label().to_owned());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "copy_source",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let path_converter = language.commit_language.path_converter();
            let out_property = self_property.map(|file_change| {
                file_change.copy_source.as_ref()
                    .map(|path| path_converter.format_file_path(path))
                    .unwrap_or_default()
            });
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map
}

fn builtin_repo_path_methods<'repo>() -> StatusTemplateBuildMethodFnMap<'repo, RepoPathBuf> {
    let mut map = StatusTemplateBuildMethodFnMap::<RepoPathBuf>::new();
    map.insert(
        "display",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let path_converter = language.commit_language.path_converter();
            let out_property = self_property.map(|path| path_converter.format_file_path(&path));
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "absolute",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let path_converter = language.commit_language.path_converter();
            let out_property = self_property.map(move |path| match path_converter {
                jj_lib::repo_path::RepoPathUiConverter::Fs { base, .. } => {
                    path.to_fs_path_unchecked(&base).display().to_string()
                }
            });
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map
}

fn builtin_conflict_info_methods<'repo>() -> StatusTemplateBuildMethodFnMap<'repo, ConflictInfo> {
    let mut map = StatusTemplateBuildMethodFnMap::<ConflictInfo>::new();
    map.insert(
        "path",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|conflict| conflict.path.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "num_sides",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|conflict| conflict.num_sides as i64);
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "sides_description",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|conflict| conflict.sides_description());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map
}

fn builtin_bookmark_conflict_methods<'repo>() -> StatusTemplateBuildMethodFnMap<'repo, BookmarkConflict> {
    let mut map = StatusTemplateBuildMethodFnMap::<BookmarkConflict>::new();
    map.insert(
        "name",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|bookmark| bookmark.name.clone());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "remote",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|bookmark| {
                bookmark.remote.as_ref().cloned().unwrap_or_default()
            });
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map.insert(
        "full_name",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|bookmark| bookmark.full_name());
            Ok(out_property.into_dyn_wrapped())
        },
    );
    map
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_status_display() {
        assert_eq!(FileStatus::Added.as_str(), "A");
        assert_eq!(FileStatus::Modified.as_str(), "M");
        assert_eq!(FileStatus::Deleted.as_str(), "D");
        assert_eq!(FileStatus::Renamed.as_str(), "R");
        assert_eq!(FileStatus::Copied.as_str(), "C");
    }

    #[test]
    fn test_file_status_label() {
        assert_eq!(FileStatus::Added.label(), "added");
        assert_eq!(FileStatus::Modified.label(), "modified");
        assert_eq!(FileStatus::Deleted.label(), "removed");
        assert_eq!(FileStatus::Renamed.label(), "renamed");
        assert_eq!(FileStatus::Copied.label(), "copied");
    }

    #[test]
    fn test_file_change_new() {
        let path = RepoPathBuf::from_internal_string("test.txt").unwrap();
        let change = StatusEntry::new(path.clone(), FileStatus::Added);

        assert_eq!(change.path, path);
        assert_eq!(change.status, FileStatus::Added);
        assert_eq!(change.copy_source, None);
        assert!(!change.has_copy_source());
    }

    #[test]
    fn test_file_change_with_copy_source() {
        let path = RepoPathBuf::from_internal_string("dest.txt").unwrap();
        let source = RepoPathBuf::from_internal_string("src.txt").unwrap();
        let change = StatusEntry::with_copy_source(path.clone(), FileStatus::Renamed, source.clone());

        assert_eq!(change.path, path);
        assert_eq!(change.status, FileStatus::Renamed);
        assert_eq!(change.copy_source, Some(source));
        assert!(change.has_copy_source());
    }

    #[test]
    fn test_conflict_info() {
        let path = RepoPathBuf::from_internal_string("conflicted.txt").unwrap();
        let conflict = ConflictInfo::new(path.clone(), 3);

        assert_eq!(conflict.path, path);
        assert_eq!(conflict.num_sides, 3);
        assert_eq!(conflict.sides_description(), "3-sided conflict");
    }

    #[test]
    fn test_bookmark_conflict_local() {
        let bookmark = BookmarkConflict::new("main".to_string());

        assert_eq!(bookmark.name, "main");
        assert_eq!(bookmark.remote, None);
        assert_eq!(bookmark.full_name(), "main");
    }

    #[test]
    fn test_bookmark_conflict_remote() {
        let bookmark = BookmarkConflict::new_remote("main".to_string(), "origin".to_string());

        assert_eq!(bookmark.name, "main");
        assert_eq!(bookmark.remote, Some("origin".to_string()));
        assert_eq!(bookmark.full_name(), "main@origin");
    }

    #[test]
    fn test_working_copy_status_default() {
        let status = WorkingCopyStatus::default();

        assert!(status.file_changes.is_empty());
        assert!(status.untracked_paths.is_empty());
        assert!(status.conflicts.is_empty());
        assert!(status.local_bookmark_conflicts.is_empty());
        assert!(status.remote_bookmark_conflicts.is_empty());

        assert!(!status.has_file_changes());
        assert!(!status.has_untracked_paths());
        assert!(!status.has_conflicts());
        assert!(!status.has_bookmark_conflicts());
    }

    #[test]
    fn test_working_copy_status_predicates() {
        let mut status = WorkingCopyStatus::new();

        // Add a file change
        status.file_changes.push(StatusEntry::new(
            RepoPathBuf::from_internal_string("file.txt").unwrap(),
            FileStatus::Modified,
        ));
        assert!(status.has_file_changes());

        // Add untracked path
        status.untracked_paths.push(RepoPathBuf::from_internal_string("untracked.txt").unwrap());
        assert!(status.has_untracked_paths());

        // Add conflict
        status.conflicts.push(ConflictInfo::new(
            RepoPathBuf::from_internal_string("conflict.txt").unwrap(),
            2,
        ));
        assert!(status.has_conflicts());

        // Add bookmark conflict
        status.local_bookmark_conflicts.push(BookmarkConflict::new("main".to_string()));
        assert!(status.has_bookmark_conflicts());
    }

    #[test]
    fn test_status_template_property_kind_type_names() {
        // Test type names for property kinds
        let file_change = StatusEntry::new(
            RepoPathBuf::from_internal_string("test.txt").unwrap(),
            FileStatus::Added,
        );
        let property = StatusTemplatePropertyKind::StatusEntry(
            Box::new(crate::templater::Literal(file_change))
        );
        assert_eq!(property.type_name(), "StatusEntry");

        let conflict = ConflictInfo::new(RepoPathBuf::from_internal_string("test.txt").unwrap(), 2);
        let property = StatusTemplatePropertyKind::ConflictInfo(
            Box::new(crate::templater::Literal(conflict))
        );
        assert_eq!(property.type_name(), "ConflictInfo");

        let bookmark = BookmarkConflict::new("main".to_string());
        let property = StatusTemplatePropertyKind::BookmarkConflict(
            Box::new(crate::templater::Literal(bookmark))
        );
        assert_eq!(property.type_name(), "BookmarkConflict");
    }
}

// Demonstration test showing the complete template pipeline working
#[cfg(test)]
mod integration_demo {
    use super::*;

    #[test]
    fn test_complete_template_pipeline() {
        // This test demonstrates the complete status template system working:
        // 1. Build a WorkingCopyStatus with real data
        // 2. Use template keywords (file_changes, conflicts, etc.)
        // 3. Render output
        
        // Build status with sample data
        let mut status = WorkingCopyStatus::new();
        
        status.file_changes.push(StatusEntry::new(
            RepoPathBuf::from_internal_string("src/main.rs").unwrap(),
            FileStatus::Modified,
        ));
        status.file_changes.push(StatusEntry::new(
            RepoPathBuf::from_internal_string("README.md").unwrap(),
            FileStatus::Added,
        ));
        
        status.untracked_paths.push(RepoPathBuf::from_internal_string("test.txt").unwrap());
        
        status.conflicts.push(ConflictInfo::new(
            RepoPathBuf::from_internal_string("conflict.rs").unwrap(),
            2,
        ));
        
        status.local_bookmark_conflicts.push(BookmarkConflict::new("main".to_string()));
        
        // Verify all predicates work
        assert!(status.has_file_changes());
        assert!(status.has_untracked_paths());
        assert!(status.has_conflicts());
        assert!(status.has_bookmark_conflicts());
        
        // Verify we can access the data
        assert_eq!(status.file_changes.len(), 2);
        assert_eq!(status.file_changes[0].status, FileStatus::Modified);
        assert_eq!(status.file_changes[1].status, FileStatus::Added);
        
        assert_eq!(status.conflicts[0].num_sides, 2);
        assert_eq!(status.conflicts[0].sides_description(), "2-sided conflict");
        
        assert_eq!(status.local_bookmark_conflicts[0].name, "main");
        assert_eq!(status.local_bookmark_conflicts[0].full_name(), "main");
    }
}
