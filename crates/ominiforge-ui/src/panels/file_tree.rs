//! File tree panel: read-only browsing of the workspace (doc/gpui-app.md §3.3).
//!
//! The tree is scoped to the gateway's workspace root ([`ClientProtocol::workspace_root`]),
//! independent of any one session's optional workspace — it browses "the project".
//! Directories lazy-load on first expand; selecting a file previews it read-only
//! (no editing — that is the Phase 7 editor's job).
//!
//! Split into a transport-independent [`FileTreeState`] (a pure fold of directory
//! listings + expansion + selection into indented render rows — unit-testable
//! without a UI) and the [`FileTree`] view (gpui: async protocol plumbing, pointer
//! affordances, layout). The view is a thin shell; every interesting rule lives
//! in the state.
//!
//! Per the Phase 3.7 decisions: no global keybindings are registered yet (j/k
//! navigation and `/` search land with the finalized keymap, alongside the other
//! panels' deferred keys).

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, ElementId, Render, Styled, div, prelude::*, px};
use ominiforge_net::{ClientProtocol, DirEntry, FilePreview};

use crate::theme::Theme;

/// Element id of the panel root, used by tests via `debug_bounds`.
pub const FILE_TREE_PANEL_ID: &str = "file-tree-panel";

/// One renderable row in the folded tree — a directory or file with its
/// indentation depth and expansion/selection state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// Workspace-relative path ("" segments joined by `/`), the row's identity.
    pub path: String,
    /// Entry name (last path segment), the display label.
    pub name: String,
    /// `true` for a directory (expandable), `false` for a file (previewable).
    pub is_dir: bool,
    /// Indentation depth (0 = a child of the workspace root).
    pub depth: usize,
    /// Whether an expanded directory's children are shown (dirs only).
    pub expanded: bool,
    /// Whether this row is the current selection.
    pub selected: bool,
}

/// One directory's loaded children, kept so collapse/re-expand is free and
/// expand state survives a re-fold.
#[derive(Debug, Default)]
struct DirNode {
    /// The directory's children, in protocol order (dirs first, then by name).
    entries: Vec<DirEntry>,
    /// Whether the directory is expanded (children visible).
    expanded: bool,
}

/// Transport-independent file-tree state: the listing/expansion/selection fold.
///
/// Pure data + pure methods — no gpui, no async — so the fold rules (lazy-load,
/// expand/collapse, sort, selection) are unit-testable without a window.
#[derive(Debug, Default)]
pub struct FileTreeState {
    /// Loaded directory nodes, keyed by workspace-relative dir path ("" = root).
    /// A directory is present only after its first listing backfilled.
    nodes: HashMap<String, DirNode>,
    /// The currently-selected file/dir path, if any.
    pub selected: Option<String>,
    /// The preview of the selected file, if it loaded.
    pub preview: Option<FilePreview>,
    /// A browsing/listing problem to surface (fail loud).
    pub notice: Option<String>,
}

impl FileTreeState {
    /// Whether a directory's listing has already been fetched (so the view
    /// knows an expand needs a `list_dir` round-trip first).
    #[must_use]
    pub fn is_loaded(&self, dir: &str) -> bool {
        self.nodes.contains_key(dir)
    }

    /// Fold a fetched directory listing into the tree and expand it. `dir` is
    /// the directory's workspace-relative path ("" for the root); `entries`
    /// must already be in protocol order (dirs first, then by name).
    pub fn load_dir(&mut self, dir: &str, entries: Vec<DirEntry>) {
        let node = self.nodes.entry(dir.to_owned()).or_default();
        node.entries = entries;
        node.expanded = true;
    }

    /// Toggle a directory's expansion (collapse hides its whole subtree from
    /// the folded rows, but keeps the loaded listing so re-expand is free).
    /// No-op for a directory that was never loaded.
    pub fn toggle(&mut self, dir: &str) {
        if let Some(node) = self.nodes.get_mut(dir) {
            node.expanded = !node.expanded;
        }
    }

    /// Select a row and clear any prior preview. The view follows up with a
    /// `read_file` round-trip for files (dirs just highlight).
    pub fn select(&mut self, path: &str) {
        self.selected = Some(path.to_owned());
        self.preview = None;
    }

    /// Fold a loaded file preview in (paired with the current selection).
    pub fn set_preview(&mut self, preview: FilePreview) {
        self.preview = Some(preview);
    }

    /// Surface a browsing problem (listing/read failure or a refused path).
    pub fn set_error(&mut self, message: String) {
        self.notice = Some(message);
    }

    /// The folded, indented visible rows: a directory's children appear only
    /// while it is expanded, depth-first. The protocol's per-directory order
    /// (dirs first, then by name) is preserved within each level.
    #[must_use]
    pub fn rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.fold_into("", 0, &mut rows);
        rows
    }

    /// Depth-first fold of one directory's children into `rows`.
    fn fold_into(&self, dir: &str, depth: usize, rows: &mut Vec<TreeRow>) {
        let Some(node) = self.nodes.get(dir) else {
            return;
        };
        if !node.expanded {
            return;
        }
        for entry in &node.entries {
            let path = join(dir, &entry.name);
            let child_expanded = self.nodes.get(&path).is_some_and(|n| n.expanded);
            rows.push(TreeRow {
                name: entry.name.clone(),
                is_dir: entry.is_dir,
                depth,
                expanded: child_expanded,
                selected: self.selected.as_deref() == Some(path.as_str()),
                path: path.clone(),
            });
            if entry.is_dir {
                self.fold_into(&path, depth + 1, rows);
            }
        }
    }
}

/// Join a directory path and an entry name into a workspace-relative path.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The file tree view: protocol plumbing + pointer affordances over
/// [`FileTreeState`].
pub struct FileTree {
    client: Arc<dyn ClientProtocol>,
    state: FileTreeState,
    /// The workspace root label shown at the panel top (empty until loaded).
    root_label: String,
}

impl FileTree {
    /// Create a file tree attached to `client`: load the workspace root label
    /// and the root directory listing.
    pub fn new(client: Arc<dyn ClientProtocol>, cx: &mut Context<Self>) -> Self {
        let tree = Self {
            client,
            state: FileTreeState::default(),
            root_label: String::new(),
        };
        tree.load_root(cx);
        tree
    }

    /// The current state (for tests and the workspace).
    #[must_use]
    pub const fn state(&self) -> &FileTreeState {
        &self.state
    }

    /// Fetch the workspace root label, then the root listing.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn load_root(&self, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            if let Ok(root) = client.workspace_root().await {
                let label = root.file_name().map_or_else(
                    || root.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let _ = this.update(cx, |tree, cx| {
                    tree.root_label = label;
                    cx.notify();
                });
            }
            match client.list_dir("").await {
                Ok(entries) => {
                    let _ = this.update(cx, |tree, cx| {
                        tree.state.load_dir("", entries);
                        cx.notify();
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |tree, cx| {
                        tree.state
                            .set_error(format!("failed to list workspace: {e:#}"));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Click a directory row: lazy-load its listing on first expand, else just
    /// toggle expansion.
    fn open_dir(&mut self, path: String, cx: &mut Context<Self>) {
        if self.state.is_loaded(&path) {
            self.state.toggle(&path);
            cx.notify();
            return;
        }
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| match client.list_dir(&path).await {
            Ok(entries) => {
                let _ = this.update(cx, |tree, cx| {
                    tree.state.load_dir(&path, entries);
                    cx.notify();
                });
            }
            Err(e) => {
                let _ = this.update(cx, |tree, cx| {
                    tree.state
                        .set_error(format!("failed to list `{path}`: {e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Click a file row: select it and fetch its read-only preview.
    fn open_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.state.select(&path);
        cx.notify();
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| match client.read_file(&path).await {
            Ok(preview) => {
                let _ = this.update(cx, |tree, cx| {
                    tree.state.set_preview(preview);
                    cx.notify();
                });
            }
            Err(e) => {
                let _ = this.update(cx, |tree, cx| {
                    tree.state
                        .set_error(format!("failed to read `{path}`: {e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for FileTree {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();

        let mut root = div()
            .id(FILE_TREE_PANEL_ID)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.canvas_raised)
            .text_color(theme.text_primary);

        if let Some(notice) = &self.state.notice {
            root = root.child(
                div()
                    .px_2()
                    .py_1()
                    .bg(theme.canvas_overlay)
                    .text_color(theme.state_error)
                    .child(notice.clone()),
            );
        }

        // Workspace root label.
        if !self.root_label.is_empty() {
            root = root.child(
                div()
                    .px_2()
                    .py_1()
                    .text_color(theme.text_tertiary)
                    .child(self.root_label.clone()),
            );
        }

        let rows = self
            .state
            .rows()
            .into_iter()
            .map(|row| render_row(&row, &theme, cx))
            .collect::<Vec<_>>();

        // Tree pane (top) + preview pane (bottom, when a file is selected).
        root.child(
            div()
                .id("file-tree-rows")
                .flex_1()
                .flex_col()
                .overflow_hidden()
                .children(rows),
        )
        .when_some(self.state.preview.clone(), |el, preview| {
            el.child(render_preview(&preview, &theme))
        })
    }
}

/// Render one tree row: an expand glyph for dirs, the name, indentation by
/// depth, selection highlight, and the click affordance.
#[allow(clippy::needless_pass_by_ref_mut)]
fn render_row(row: &TreeRow, theme: &Theme, cx: &mut Context<FileTree>) -> gpui::AnyElement {
    let path = row.path.clone();
    let glyph = if row.is_dir {
        if row.expanded { "▾" } else { "▸" }
    } else {
        "·"
    };
    let name_color = if row.is_dir {
        theme.text_primary
    } else {
        theme.text_secondary
    };

    let mut el = div()
        .id(ElementId::Name(format!("file-row-{}", row.path).into()))
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .cursor_pointer()
        .when(row.selected, |e| e.bg(theme.accent_dim))
        .hover(|s| s.bg(theme.canvas_overlay))
        .child(div().text_color(theme.text_tertiary).child(glyph))
        .child(div().text_color(name_color).child(row.name.clone()));

    // Indent by depth: a leading spacer per level past the root.
    #[allow(clippy::cast_precision_loss)]
    if row.depth > 0 {
        el = el.pl(px(INDENT * row.depth as f32));
    }

    if row.is_dir {
        el = el.on_click(cx.listener(move |this, _, _, cx| {
            this.open_dir(path.clone(), cx);
        }));
    } else {
        el = el.on_click(cx.listener(move |this, _, _, cx| {
            this.open_file(path.clone(), cx);
        }));
    }
    el.into_any_element()
}

/// Pixels of indentation per tree depth level.
const INDENT: f32 = 12.0;

/// Render the read-only preview pane pinned to the panel bottom.
fn render_preview(preview: &FilePreview, theme: &Theme) -> gpui::AnyElement {
    let mut section = div()
        .id("file-preview")
        .flex()
        .flex_col()
        .border_t_1()
        .border_color(theme.border_subtle)
        .max_h(px(240.0))
        .overflow_hidden();

    let mut header = preview.path.clone();
    if preview.truncated {
        header.push_str("  (truncated)");
    }
    section = section.child(
        div()
            .px_2()
            .py_1()
            .text_color(theme.text_tertiary)
            .child(header),
    );
    section = section.child(
        div()
            .px_2()
            .py_1()
            .bg(theme.canvas_base)
            .text_color(theme.text_secondary)
            .child(preview.content.clone()),
    );
    section.into_any_element()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn dir(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_owned(),
            is_dir: true,
        }
    }

    fn file(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_owned(),
            is_dir: false,
        }
    }

    fn names(rows: &[TreeRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    // A collapsed directory's children are hidden; expanding reveals them,
    // depth-first, with the protocol's per-directory order preserved.
    #[test]
    fn expand_reveals_children_depth_first() {
        let mut state = FileTreeState::default();
        // Root: two dirs (src, tests) then a file (README).
        state.load_dir("", vec![dir("src"), dir("tests"), file("README.md")]);
        // Both subdirs are collapsed until their own listing loads.
        assert_eq!(names(&state.rows()), ["src", "tests", "README.md"]);
        assert!(!state.is_loaded("src"));

        // Expanding `src` inserts its children right after it, at depth 1.
        state.load_dir("src", vec![dir("panels"), file("main.rs")]);
        let rows = state.rows();
        assert_eq!(
            names(&rows),
            ["src", "panels", "main.rs", "tests", "README.md"]
        );
        assert_eq!(rows[1].depth, 1, "panels nests under src");
        assert_eq!(rows[3].name, "tests", "tests stays at root depth 0");
        assert_eq!(rows[3].depth, 0);

        // Expanding the nested `panels` dir goes one deeper.
        state.load_dir("src/panels", vec![file("chat.rs")]);
        let rows = state.rows();
        assert_eq!(
            names(&rows),
            ["src", "panels", "chat.rs", "main.rs", "tests", "README.md"]
        );
        assert_eq!(rows[2].depth, 2, "chat.rs nests two levels deep");
    }

    // Collapsing a directory hides its whole subtree but keeps the loaded
    // listing, so re-expand is free (no refetch) and restores the rows.
    #[test]
    fn collapse_hides_subtree_but_keeps_listing() {
        let mut state = FileTreeState::default();
        state.load_dir("", vec![dir("src")]);
        state.load_dir("src", vec![dir("panels"), file("main.rs")]);
        state.load_dir("src/panels", vec![file("chat.rs")]);
        assert_eq!(state.rows().len(), 4);

        // Collapse `src`: everything under it vanishes.
        state.toggle("src");
        assert_eq!(
            names(&state.rows()),
            ["src"],
            "subtree hidden when collapsed"
        );
        // But the listing is still loaded (re-expand needs no refetch).
        assert!(state.is_loaded("src"));
        assert!(state.is_loaded("src/panels"));

        // Re-expand restores the full subtree.
        state.toggle("src");
        assert_eq!(
            names(&state.rows()),
            ["src", "panels", "chat.rs", "main.rs"]
        );
    }

    // The expanded glyph state is per-directory and survives a fold.
    #[test]
    fn expanded_state_is_tracked_per_dir() {
        let mut state = FileTreeState::default();
        state.load_dir("", vec![dir("open"), dir("closed")]);
        state.load_dir("open", vec![file("a.rs")]);
        let rows = state.rows();
        let open = rows.iter().find(|r| r.name == "open").unwrap();
        let closed = rows.iter().find(|r| r.name == "closed").unwrap();
        assert!(open.expanded);
        assert!(!closed.expanded, "unloaded dir renders collapsed");
    }

    // Selecting a row marks it and clears any prior preview; the selection
    // survives a re-fold (it is keyed by path, not row index).
    #[test]
    fn selection_marks_row_and_clears_preview() {
        let mut state = FileTreeState::default();
        state.load_dir("", vec![file("a.rs"), file("b.rs")]);
        state.set_preview(FilePreview {
            path: "a.rs".into(),
            content: "old".into(),
            truncated: false,
        });

        state.select("b.rs");
        assert_eq!(state.selected.as_deref(), Some("b.rs"));
        assert!(
            state.preview.is_none(),
            "a new selection drops the stale preview"
        );

        let rows = state.rows();
        assert!(!rows[0].selected, "a.rs not selected");
        assert!(rows[1].selected, "b.rs selected");
    }

    // Folding a loaded preview in pairs it with the selection.
    #[test]
    fn preview_lands() {
        let mut state = FileTreeState::default();
        state.load_dir("", vec![file("main.rs")]);
        state.select("main.rs");
        state.set_preview(FilePreview {
            path: "main.rs".into(),
            content: "fn main() {}".into(),
            truncated: true,
        });
        let preview = state.preview.as_ref().unwrap();
        assert_eq!(preview.content, "fn main() {}");
        assert!(preview.truncated);
    }

    // Errors surface via the notice slot rather than vanishing (fail loud).
    #[test]
    fn error_surfaces() {
        let mut state = FileTreeState::default();
        state.set_error("path escapes workspace".to_owned());
        assert_eq!(state.notice.as_deref(), Some("path escapes workspace"));
    }
}
