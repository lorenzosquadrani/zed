mod renderer;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, RenderImage, ScrollHandle, SharedString, Styled,
    Subscription, Task, Window, actions, div, img, point, px,
};
use project::{Project, ProjectEntryId, ProjectPath};
use renderer::PageSize;
use ui::{ScrollAxes, Scrollbars, Tooltip, WithScrollbar, prelude::*};
use util::ResultExt;
use workspace::{
    Pane, WorkspaceId,
    invalid_item_view::InvalidItemView,
    item::{Item, ItemEvent, ProjectItem, TabContentParams},
};

actions!(
    pdf_viewer,
    [
        /// Zoom in the PDF preview.
        ZoomIn,
        /// Zoom out the PDF preview.
        ZoomOut,
        /// Show PDF pages at 100%.
        ResetZoom,
        /// Fit PDF pages to the width of the pane.
        FitToWidth,
        /// Go to the next PDF page.
        NextPage,
        /// Go to the previous PDF page.
        PreviousPage,
        /// Go to the first PDF page.
        FirstPage,
        /// Go to the last PDF page.
        LastPage,
        /// Reload the PDF from disk.
        Reload,
    ]
);

const PAGE_GAP: f32 = 16.0;
const MAX_CACHED_PIXELS: usize = 32 * 1024 * 1024;

pub struct PdfItem {
    project: Entity<Project>,
    path: ProjectPath,
    abs_path: PathBuf,
    entry_id: Option<ProjectEntryId>,
    bytes: Arc<Vec<u8>>,
    pages: Vec<PageSize>,
    error: Option<String>,
    reload_task: Task<()>,
    _subscription: Subscription,
}

impl EventEmitter<()> for PdfItem {}

impl PdfItem {
    fn load(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &App,
    ) -> Task<Result<(Arc<Vec<u8>>, Vec<PageSize>)>> {
        let load = project
            .read(cx)
            .read_binary_file(path, renderer::MAX_FILE_SIZE, cx);
        cx.background_spawn(async move {
            let bytes = Arc::new(load.await?);
            let pages = renderer::metadata(bytes.clone())?;
            Ok((bytes, pages))
        })
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        // Compilers often replace the output in several writes. Wait for the
        // worktree notifications to settle before parsing the replacement.
        self.reload_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let result = this.update(cx, |this, cx| Self::load(&this.project, &this.path, cx));
            let Ok(task) = result else { return };
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((bytes, pages)) => {
                        this.bytes = bytes;
                        this.pages = pages;
                        this.entry_id = this
                            .project
                            .read(cx)
                            .entry_for_path(&this.path, cx)
                            .map(|entry| entry.id);
                        this.error = None;
                    }
                    Err(error) => this.error = Some(format!("Cannot reload PDF: {error:#}")),
                }
                cx.emit(());
                cx.notify();
            })
            .log_err();
        });
    }
}

impl project::ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        let abs_path = project.read(cx).absolute_path(path, cx)?;
        if !abs_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            return None;
        }
        let entry_id = project
            .read(cx)
            .entry_for_path(path, cx)
            .map(|entry| entry.id);
        let task = Self::load(project, path, cx);
        let project = project.clone();
        let path = path.clone();
        Some(cx.spawn(async move |cx| {
            let (bytes, pages) = task.await?;
            Ok(cx.new(|cx| {
                let subscription = cx.subscribe(&project, |this: &mut Self, project, event, cx| {
                    if let project::Event::WorktreeUpdatedEntries(worktree_id, changes) = event {
                        if *worktree_id == this.path.worktree_id
                            && changes.iter().any(|(path, id, _)| {
                                path == &this.path.path || Some(*id) == this.entry_id
                            })
                        {
                            if let Some(worktree) =
                                project.read(cx).worktree_for_id(*worktree_id, cx)
                            {
                                if let Some(entry) = this
                                    .entry_id
                                    .and_then(|id| worktree.read(cx).entry_for_id(id))
                                {
                                    this.path.path = entry.path.clone();
                                    this.abs_path = worktree.read(cx).absolutize(&entry.path);
                                }
                            }
                            this.reload(cx);
                        }
                    }
                });
                Self {
                    project,
                    path,
                    abs_path,
                    entry_id,
                    bytes,
                    pages,
                    error: None,
                    reload_task: Task::ready(()),
                    _subscription: subscription,
                }
            }))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }
    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.path.clone())
    }
    fn is_dirty(&self) -> bool {
        false
    }
}

pub struct PdfViewer {
    item: Entity<PdfItem>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    zoom: f32,
    fit_width: bool,
    render_scale: f32,
    raster_dimension: u16,
    generation: usize,
    pages: BTreeMap<usize, Result<Arc<RenderImage>, String>>,
    render_task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<()> for PdfViewer {}

impl PdfViewer {
    fn new(item: Entity<PdfItem>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&item, |this: &mut Self, _, _, cx| {
            this.invalidate(cx);
            cx.emit(());
        });
        cx.on_release(|this, cx| this.clear_pages(cx)).detach();
        Self {
            item,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            zoom: 1.0,
            fit_width: true,
            render_scale: 0.0,
            raster_dimension: renderer::MAX_DIMENSION,
            generation: 0,
            pages: BTreeMap::new(),
            render_task: None,
            _subscription: subscription,
        }
    }

    fn clear_pages(&mut self, cx: &mut App) {
        for image in std::mem::take(&mut self.pages)
            .into_values()
            .filter_map(Result::ok)
        {
            cx.drop_image(image, None);
        }
    }

    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.clear_pages(cx);
        cx.notify();
    }

    fn scale(&self, cx: &App) -> f32 {
        if self.fit_width {
            let width = f32::from(self.scroll.bounds().size.width);
            let page_width = self
                .item
                .read(cx)
                .pages
                .iter()
                .map(|page| page.width)
                .fold(1.0_f32, f32::max);
            if width > PAGE_GAP * 2.0 {
                ((width - PAGE_GAP * 2.0) / page_width).clamp(0.1, 4.0)
            } else {
                1.0
            }
        } else {
            self.zoom
        }
    }

    fn current_page(&self, cx: &App) -> usize {
        // At the bottom of a document the last page may be shorter than the
        // viewport, so its top cannot always be scrolled to the pane's top.
        let offset = -f32::from(self.scroll.offset().y)
            + (f32::from(self.scroll.bounds().size.height) / 2.0).max(PAGE_GAP);
        let mut bottom = PAGE_GAP;
        for (index, page) in self.item.read(cx).pages.iter().enumerate() {
            bottom += page.height * self.scale(cx) + PAGE_GAP;
            if bottom > offset {
                return index;
            }
        }
        self.item.read(cx).pages.len().saturating_sub(1)
    }

    fn go_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let page = page.min(self.item.read(cx).pages.len().saturating_sub(1));
        let offset: f32 = self
            .item
            .read(cx)
            .pages
            .iter()
            .take(page)
            .map(|page| page.height * self.scale(cx) + PAGE_GAP)
            .sum();
        self.scroll.set_offset(point(px(0.0), px(-offset)));
        cx.notify();
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let old_scale = self.scale(cx);
        self.fit_width = false;
        self.zoom = zoom.clamp(0.1, 4.0);
        self.scroll
            .set_offset(self.scroll.offset() * (self.zoom / old_scale));
        cx.notify();
    }

    fn request_pages(&mut self, window: &Window, cx: &mut Context<Self>) {
        let scale = self.scale(cx);
        let render_scale = scale * window.scale_factor();
        let top = -f32::from(self.scroll.offset().y);
        let height = f32::from(self.scroll.bounds().size.height).max(1.0);
        let needed = visible_pages(&self.item.read(cx).pages, scale, top, height);
        let raster_dimension = raster_dimension(needed.len());
        if (render_scale - self.render_scale).abs() > 0.01
            || raster_dimension != self.raster_dimension
        {
            self.render_scale = render_scale;
            self.raster_dimension = raster_dimension;
            self.invalidate(cx);
        }
        let obsolete: Vec<_> = self
            .pages
            .keys()
            .copied()
            .filter(|index| !needed.contains(index))
            .collect();
        for index in obsolete {
            if let Some(Ok(image)) = self.pages.remove(&index) {
                cx.drop_image(image, None);
            }
        }
        if self.render_task.is_some() || self.item.read(cx).error.is_some() {
            return;
        }
        let Some(index) = needed
            .into_iter()
            .find(|index| !self.pages.contains_key(index))
        else {
            return;
        };
        let bytes = self.item.read(cx).bytes.clone();
        let generation = self.generation;
        // Keep a single raster job in flight even when zoom or scroll changes.
        // Dropping a task cannot interrupt synchronous PDF interpretation.
        self.render_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    renderer::render_page(bytes, index, render_scale, raster_dimension)
                        .map_err(|error| format!("{error:#}"))
                })
                .await;
            this.update(cx, |this, cx| {
                this.render_task = None;
                if this.generation == generation {
                    this.pages.insert(index, result);
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn scroll_wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control || event.modifiers.platform {
            let delta = event.delta.pixel_delta(px(20.0)).y;
            if delta != px(0.0) {
                self.set_zoom(
                    self.scale(cx) * if delta > px(0.0) { 1.1 } else { 1.0 / 1.1 },
                    cx,
                );
            }
            cx.stop_propagation();
        }
    }
}

fn visible_pages(pages: &[PageSize], scale: f32, top: f32, height: f32) -> Vec<usize> {
    let mut offset = PAGE_GAP;
    pages
        .iter()
        .enumerate()
        .filter_map(|(index, page)| {
            let bottom = offset + page.height * scale;
            let visible = bottom >= top && offset <= top + height;
            offset = bottom + PAGE_GAP;
            visible.then_some(index)
        })
        .collect()
}

fn raster_dimension(visible_count: usize) -> u16 {
    ((MAX_CACHED_PIXELS / visible_count.max(1)) as f64)
        .sqrt()
        .clamp(1.0, f64::from(renderer::MAX_DIMENSION)) as u16
}

impl Focusable for PdfViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.request_pages(window, cx);
        let scale = self.scale(cx);
        let current = self.current_page(cx);
        let item = self.item.read(cx);
        let count = item.pages.len();
        let mut content = v_flex()
            .gap(px(PAGE_GAP))
            .p(px(PAGE_GAP))
            .items_center()
            .min_w_full()
            .min_h_full()
            .on_scroll_wheel(cx.listener(Self::scroll_wheel));
        for (index, page) in item.pages.iter().enumerate() {
            let page_content = match self.pages.get(&index) {
                Some(Ok(image)) => img(image.clone()).size_full().into_any_element(),
                Some(Err(error)) => Label::new(error.clone())
                    .color(Color::Error)
                    .into_any_element(),
                None => Label::new("Rendering page…")
                    .color(Color::Muted)
                    .into_any_element(),
            };
            content = content.child(
                div()
                    .flex_none()
                    .w(px(page.width * scale))
                    .h(px(page.height * scale))
                    .bg(gpui::white())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(page_content),
            );
        }
        let error = item.error.clone();
        let previous_bounds = self.scroll.bounds();
        let previous_offset = self.scroll.offset();
        let scroll = self.scroll.clone();
        let view = cx.entity().downgrade();
        v_flex()
            .id("pdf-viewer")
            .key_context("PdfViewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(
                cx.listener(|this, _: &ZoomIn, _, cx| this.set_zoom(this.scale(cx) * 1.2, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ZoomOut, _, cx| this.set_zoom(this.scale(cx) / 1.2, cx)),
            )
            .on_action(cx.listener(|this, _: &ResetZoom, _, cx| this.set_zoom(1.0, cx)))
            .on_action(cx.listener(|this, _: &FitToWidth, _, cx| {
                this.fit_width = true;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextPage, _, cx| {
                this.go_to_page(this.current_page(cx) + 1, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousPage, _, cx| {
                this.go_to_page(this.current_page(cx).saturating_sub(1), cx)
            }))
            .on_action(cx.listener(|this, _: &FirstPage, _, cx| this.go_to_page(0, cx)))
            .on_action(cx.listener(|this, _: &LastPage, _, cx| this.go_to_page(usize::MAX, cx)))
            .on_action(cx.listener(|this, _: &Reload, _, cx| {
                this.item.update(cx, |item, cx| item.reload(cx))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        IconButton::new("previous-page", IconName::ChevronLeft)
                            .disabled(current == 0)
                            .tooltip(Tooltip::for_action_title("Previous Page", &PreviousPage))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.go_to_page(this.current_page(cx).saturating_sub(1), cx)
                            })),
                    )
                    .child(
                        Label::new(format!("Page {} of {count}", current + 1))
                            .size(LabelSize::Small),
                    )
                    .child(
                        IconButton::new("next-page", IconName::ChevronRight)
                            .disabled(current + 1 >= count)
                            .tooltip(Tooltip::for_action_title("Next Page", &NextPage))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.go_to_page(this.current_page(cx) + 1, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("zoom-out", IconName::Dash)
                            .tooltip(Tooltip::for_action_title("Zoom Out", &ZoomOut))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_zoom(this.scale(cx) / 1.2, cx)
                            })),
                    )
                    .child(Label::new(format!("{:.0}%", scale * 100.0)).size(LabelSize::Small))
                    .child(
                        IconButton::new("zoom-in", IconName::Plus)
                            .tooltip(Tooltip::for_action_title("Zoom In", &ZoomIn))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_zoom(this.scale(cx) * 1.2, cx)
                            })),
                    )
                    .child(Button::new("fit-width", "Fit Width").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.fit_width = true;
                            cx.notify();
                        },
                    )))
                    .child(
                        IconButton::new("reload", IconName::RotateCw)
                            .tooltip(Tooltip::for_action_title("Reload PDF", &Reload))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.item.update(cx, |item, cx| item.reload(cx))
                            })),
                    ),
            )
            .when_some(error, |view, error| {
                view.child(Label::new(error).color(Color::Error))
            })
            .child(
                div()
                    .id("pdf-pages")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| window.focus(&this.focus_handle, cx)),
                    )
                    .child(content)
                    .child(
                        gpui::canvas(
                            move |_, _, _| (),
                            move |_, _, _, cx| {
                                if scroll.bounds().size != previous_bounds.size
                                    || scroll.offset() != previous_offset
                                {
                                    view.update(cx, |_, cx| cx.notify()).log_err();
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Both)
                            .tracked_scroll_handle(&self.scroll)
                            .notify_content(),
                        window,
                        cx,
                    ),
            )
    }
}

impl Item for PdfViewer {
    type Event = ();
    fn to_item_events(_: &(), emit: &mut dyn FnMut(ItemEvent)) {
        emit(ItemEvent::UpdateTab);
    }
    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.item
            .read(cx)
            .abs_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PDF".into())
            .into()
    }
    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |label| label.italic())
            .into_any_element()
    }
    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            self.item
                .read(cx)
                .abs_path
                .to_string_lossy()
                .into_owned()
                .into(),
        )
    }
    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        FileIcons::get_icon(&self.item.read(cx).abs_path, cx).map(Icon::from_path)
    }
    fn for_each_project_item(
        &self,
        cx: &App,
        callback: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        callback(self.item.entity_id(), self.item.read(cx));
    }
    fn can_split(&self) -> bool {
        true
    }
    fn clone_on_split(
        &self,
        _: Option<WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        Task::ready(Some(cx.new(|cx| {
            let mut viewer = Self::new(self.item.clone(), cx);
            viewer.zoom = self.zoom;
            viewer.fit_width = self.fit_width;
            viewer.scroll.set_offset(self.scroll.offset());
            viewer
        })))
    }
    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl ProjectItem for PdfViewer {
    type Item = PdfItem;
    fn for_project_item(
        _: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<PdfItem>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, cx)
    }
    fn for_broken_project_item(
        path: &Path,
        local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(path, local, error, window, cx))
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfViewer>(cx);
    workspace::register_serializable_item::<PdfViewer>(cx);
}

impl workspace::item::SerializableItem for PdfViewer {
    fn serialized_item_kind() -> &'static str {
        "PdfViewer"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = persistence::PdfViewerDb::global(cx);
        workspace::delete_unloaded_items(alive_items, workspace_id, "pdf_viewers", &db, cx)
    }

    fn deserialize(
        project: Entity<Project>,
        _: gpui::WeakEntity<workspace::Workspace>,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::PdfViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let path = db
                .get_path(item_id, workspace_id)?
                .context("No saved PDF path")?;
            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(path, false, cx)
                })
                .await?;
            let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
            let task = cx.update(|_, cx| {
                <PdfItem as project::ProjectItem>::try_open(
                    &project,
                    &ProjectPath { worktree_id, path },
                    cx,
                )
                .context("Cannot restore PDF preview")
            })??;
            let item = task.await?;
            cx.update(|_, cx| Ok(cx.new(|cx| Self::new(item, cx))))?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut workspace::Workspace,
        item_id: workspace::ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let path = self.item.read(cx).abs_path.clone();
        let db = persistence::PdfViewerDb::global(cx);
        Some(cx.background_spawn(async move { db.save_path(item_id, workspace_id, path).await }))
    }

    fn should_serialize(&self, _: &()) -> bool {
        true
    }
}

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use std::path::PathBuf;
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct PdfViewerDb(ThreadSafeConnection);
    impl Domain for PdfViewerDb {
        const NAME: &str = "PdfViewerDb";
        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE pdf_viewers (
                workspace_id INTEGER,
                item_id INTEGER,
                path BLOB NOT NULL,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
            ) STRICT;
        )];
    }
    db::static_connection!(PdfViewerDb, [WorkspaceDb]);
    impl PdfViewerDb {
        query! {
            pub async fn save_path(item_id: ItemId, workspace_id: WorkspaceId, path: PathBuf) -> Result<()> {
                INSERT OR REPLACE INTO pdf_viewers(item_id, workspace_id, path) VALUES (?, ?, ?)
            }
        }
        query! {
            pub fn get_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT path FROM pdf_viewers WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn opens_a_pdf_as_a_single_file_workspace(cx: &mut gpui::TestAppContext) {
        use fs::{FakeFs, Fs as _};
        use util::rel_path::rel_path;

        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/project"))
            .await
            .expect("test directory");
        fs.insert_file(
            "/project/preview.pdf",
            include_bytes!("../fixtures/preview.pdf").to_vec(),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project/preview.pdf")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| workspace::Workspace::test_new(project, window, cx));
        let item = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path("")), None, true, window, cx)
            })
            .await
            .expect("single-file PDF opens");
        assert_eq!(
            item.to_any_view().entity_type(),
            std::any::TypeId::of::<PdfViewer>()
        );
    }

    #[gpui::test]
    async fn opens_pdf_and_reloads_after_file_changes(cx: &mut gpui::TestAppContext) {
        use fs::{FakeFs, Fs as _};
        use project::ProjectItem as _;
        use util::rel_path::rel_path;

        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/project"))
            .await
            .expect("test directory");
        let bytes = renderer::tests::sample_pdf();
        fs.insert_file("/project/test.PDF", bytes.as_ref().clone())
            .await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        let path = ProjectPath {
            worktree_id,
            path: rel_path("test.PDF").into(),
        };
        let item = cx
            .update(|cx| PdfItem::try_open(&project, &path, cx))
            .expect("PDF opener")
            .await
            .expect("PDF opens");
        let viewer = cx.new(|cx| PdfViewer::new(item.clone(), cx));
        viewer.update(cx, |viewer, cx| {
            viewer.go_to_page(1, cx);
            assert_eq!(viewer.current_page(cx), 1);
            viewer.go_to_page(usize::MAX, cx);
            assert_eq!(viewer.current_page(cx), 1);
            viewer.go_to_page(0, cx);
            assert_eq!(viewer.current_page(cx), 0);
        });
        assert_eq!(item.read_with(cx, |item, _| item.pages.len()), 2);
        assert!(item.read_with(cx, |item, _| item.entry_id.is_some()));
        let text_path = ProjectPath {
            worktree_id,
            path: rel_path("test.txt").into(),
        };
        assert!(
            cx.update(|cx| PdfItem::try_open(&project, &text_path, cx))
                .is_none()
        );

        fs.insert_file("/project/test.PDF", b"incomplete compiler output".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(item.read_with(cx, |item, _| item.error.is_some()));

        fs.insert_file("/project/test.PDF", bytes.as_ref().clone())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(item.read_with(cx, |item, _| item.error.is_none()));
        assert!(viewer.read_with(cx, |viewer, _| viewer.generation >= 2));

        let (rendered_view, cx) = cx.add_window_view(|_, cx| PdfViewer::new(item, cx));
        for _ in 0..4 {
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            cx.run_until_parked();
        }
        rendered_view.read_with(cx, |viewer, _| {
            assert!(
                !viewer.pages.is_empty(),
                "visible PDF pages must be rendered"
            );
            assert!(viewer.pages.values().all(Result::is_ok));
            let allocated: usize = viewer
                .pages
                .values()
                .filter_map(|result| result.as_ref().ok())
                .filter_map(|image| image.as_bytes(0))
                .map(|bytes| bytes.len())
                .sum();
            assert!(allocated <= MAX_CACHED_PIXELS * 4);
        });
        rendered_view.update(cx, |viewer, cx| viewer.set_zoom(1.0, cx));
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        let (position, old_scale) = rendered_view.read_with(cx, |viewer, cx| {
            (viewer.scroll.bounds().center(), viewer.scale(cx))
        });
        let modifiers = gpui::Modifiers {
            control: true,
            ..Default::default()
        };
        cx.simulate_mouse_move(position, None, modifiers);
        cx.simulate_event(gpui::ScrollWheelEvent {
            position,
            delta: gpui::ScrollDelta::Lines(point(0.0, 1.0)),
            modifiers,
            ..Default::default()
        });
        rendered_view.read_with(cx, |viewer, cx| {
            assert!(
                (viewer.scale(cx) - (old_scale * 1.1).clamp(0.1, 4.0)).abs() < 0.001,
                "Ctrl+wheel zoom: old scale {old_scale}, new scale {}",
                viewer.scale(cx)
            );
            assert_eq!(
                viewer.scroll.offset().y,
                px(0.0),
                "Ctrl+wheel must zoom without also scrolling"
            );
        });
    }

    #[gpui::test]
    async fn bounds_binary_file_reads(cx: &mut gpui::TestAppContext) {
        use fs::{FakeFs, Fs as _};
        use project::binary_file::read_bounded_file;

        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new("/project"))
            .await
            .expect("directory");
        fs.insert_file("/project/binary.pdf", vec![0, 255, 0, 254])
            .await;
        fs.insert_file("/project/empty.pdf", Vec::new()).await;
        let fs: Arc<dyn fs::Fs> = fs;
        assert_eq!(
            read_bounded_file(&fs, Path::new("/project/binary.pdf"), 4)
                .await
                .expect("binary bytes"),
            vec![0, 255, 0, 254]
        );
        assert!(
            read_bounded_file(&fs, Path::new("/project/binary.pdf"), 3)
                .await
                .is_err()
        );
        assert!(
            read_bounded_file(&fs, Path::new("/project/binary.pdf"), u64::MAX)
                .await
                .is_err()
        );
        assert!(
            read_bounded_file(&fs, Path::new("/project/empty.pdf"), 0)
                .await
                .expect("empty file")
                .is_empty()
        );
        assert!(
            read_bounded_file(&fs, Path::new("/project"), 1024)
                .await
                .is_err()
        );
        assert!(
            read_bounded_file(&fs, Path::new("/project/missing.pdf"), 1024)
                .await
                .is_err()
        );
    }

    #[gpui::test]
    async fn opens_remote_pdf_and_reloads(
        cx: &mut gpui::TestAppContext,
        server_cx: &mut gpui::TestAppContext,
    ) {
        use client::{Client, UserStore};
        use fs::{FakeFs, Fs as _};
        use language::LanguageRegistry;
        use node_runtime::NodeRuntime;
        use project::ProjectItem as _;
        use remote::RemoteClient;
        use remote_server::{HeadlessAppState, HeadlessProject};
        use util::rel_path::rel_path;

        for context in [&mut *cx, &mut *server_cx] {
            context.update(|cx| {
                release_channel::init(semver::Version::new(0, 0, 0), cx);
                let settings = settings::SettingsStore::test(cx);
                cx.set_global(settings);
                cx.set_global(db::AppDatabase::test_new());
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            });
        }
        let server_fs = FakeFs::new(server_cx.executor());
        server_fs
            .create_dir(Path::new("/remote"))
            .await
            .expect("remote directory");
        let original = renderer::tests::sample_pdf();
        let replacement = include_bytes!("../fixtures/preview.pdf").to_vec();
        server_fs
            .insert_file("/remote/report.pdf", original.as_ref().clone())
            .await;
        let (options, session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
        server_cx.update(HeadlessProject::init);
        let _headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session,
                    fs: server_fs.clone(),
                    http_client: Arc::new(http_client::BlockedHttpClient),
                    node_runtime: NodeRuntime::unavailable(),
                    languages: Arc::new(LanguageRegistry::new(cx.background_executor().clone())),
                    extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });
        drop(connect_guard);
        let remote = RemoteClient::connect_mock(options, cx).await;
        let client = cx.update(|cx| {
            Client::new(
                Arc::new(clock::FakeSystemClock::new()),
                http_client::FakeHttpClient::with_404_response(),
                cx,
            )
        });
        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let local_fs = FakeFs::new(cx.executor());
        // A local file with the same path must never be used for the remote preview.
        local_fs
            .create_dir(Path::new("/remote"))
            .await
            .expect("local directory");
        local_fs
            .insert_file("/remote/report.pdf", b"not the remote PDF".to_vec())
            .await;
        let languages = Arc::new(LanguageRegistry::test(cx.executor()));
        let project = cx.update(|cx| {
            Project::init(&client, cx);
            Project::remote(
                remote,
                client,
                NodeRuntime::unavailable(),
                user_store,
                languages,
                local_fs,
                false,
                cx,
            )
        });
        let (worktree, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree("/remote", true, cx)
            })
            .await
            .expect("remote worktree");
        cx.run_until_parked();
        let path = ProjectPath {
            worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
            path: rel_path("report.pdf").into(),
        };
        let item = cx
            .update(|cx| PdfItem::try_open(&project, &path, cx))
            .expect("PDF opener")
            .await
            .expect("remote PDF opens");
        item.read_with(cx, |item, _| {
            assert_eq!(item.bytes.as_ref(), original.as_ref());
            assert_eq!(item.pages.len(), 2);
        });
        let viewer = cx.new(|cx| PdfViewer::new(item.clone(), cx));

        server_fs
            .insert_file("/remote/report.pdf", b"incomplete PDF".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(
            item.read_with(cx, |item, _| item.error.is_some()),
            "remote reload errors are visible"
        );

        server_fs
            .insert_file("/remote/report.next.pdf", replacement.clone())
            .await;
        server_fs
            .rename(
                Path::new("/remote/report.next.pdf"),
                Path::new("/remote/report.pdf"),
                fs::RenameOptions {
                    overwrite: true,
                    ..Default::default()
                },
            )
            .await
            .expect("compiler replaces PDF atomically");
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        item.read_with(cx, |item, _| {
            assert!(item.error.is_none());
            assert_eq!(item.bytes.as_ref(), &replacement);
            assert_eq!(item.pages[0].width, 300.0);
        });
        assert!(viewer.read_with(cx, |viewer, _| viewer.generation >= 2));

        let too_small = project.read_with(cx, |project, cx| project.read_binary_file(&path, 8, cx));
        assert!(
            too_small.await.is_err(),
            "server enforces the requested byte limit"
        );
        let directory = ProjectPath {
            path: rel_path("").into(),
            ..path.clone()
        };
        assert!(
            project
                .read_with(cx, |project, cx| project
                    .read_binary_file(&directory, 1024, cx))
                .await
                .is_err()
        );
        let missing = ProjectPath {
            path: rel_path("missing.pdf").into(),
            ..path
        };
        assert!(
            cx.update(|cx| PdfItem::try_open(&project, &missing, cx))
                .expect("PDF opener")
                .await
                .is_err()
        );

        let generation = viewer.read_with(cx, |viewer, _| viewer.generation);
        item.update(cx, |item, cx| item.reload(cx));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(item.read_with(cx, |item, _| item.error.is_none()));
        assert!(viewer.read_with(cx, |viewer, _| viewer.generation > generation));
    }

    #[test]
    fn requests_only_visible_pages_with_mixed_sizes() {
        let pages = [
            PageSize {
                width: 200.0,
                height: 300.0,
            },
            PageSize {
                width: 400.0,
                height: 200.0,
            },
        ];
        assert_eq!(visible_pages(&pages, 1.0, 0.0, 200.0), vec![0]);
        assert_eq!(visible_pages(&pages, 1.0, 290.0, 100.0), vec![0, 1]);
        assert_eq!(visible_pages(&pages, 1.0, 350.0, 100.0), vec![1]);
        assert_eq!(visible_pages(&pages, 2.0, 350.0, 100.0), vec![0]);
    }

    #[test]
    fn bounds_the_page_cache_for_tiny_pages() {
        let pages = vec![
            PageSize {
                width: 1.0,
                height: 1.0
            };
            1000
        ];
        let visible = visible_pages(&pages, 0.1, 0.0, 10000.0);
        assert!(visible.len() > 8);
        let dimension = usize::from(raster_dimension(visible.len()));
        assert!(visible.len() * dimension * dimension <= MAX_CACHED_PIXELS);
    }
}
