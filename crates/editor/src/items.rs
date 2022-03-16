use crate::{Autoscroll, Editor, Event, MultiBuffer, NavigationData, ToOffset, ToPoint as _};
use anyhow::Result;
use gpui::{
    elements::*, AppContext, Entity, ModelContext, ModelHandle, RenderContext, Subscription, Task,
    View, ViewContext, ViewHandle, WeakModelHandle,
};
use language::{Bias, Buffer, Diagnostic, File as _};
use project::{File, Project, ProjectPath};
use std::fmt::Write;
use std::path::PathBuf;
use text::{Point, Selection};
use util::ResultExt;
use workspace::{ItemNavHistory, ItemView, ItemViewHandle, PathOpener, Settings, StatusItemView};

pub struct BufferOpener;

#[derive(Clone)]
pub struct BufferItemHandle(pub ModelHandle<Buffer>);

#[derive(Clone)]
struct WeakBufferItemHandle(WeakModelHandle<Buffer>);

#[derive(Clone)]
pub struct MultiBufferItemHandle(pub ModelHandle<MultiBuffer>);

#[derive(Clone)]
struct WeakMultiBufferItemHandle(WeakModelHandle<MultiBuffer>);

impl PathOpener for BufferOpener {
    fn open(
        &self,
        project: &mut Project,
        project_path: ProjectPath,
        window_id: usize,
        cx: &mut ModelContext<Project>,
    ) -> Option<Task<Result<Box<dyn ItemViewHandle>>>> {
        let buffer = project.open_buffer_for_path(project_path, cx);
        Some(cx.spawn(|project, mut cx| async move {
            let buffer = buffer.await?;
            let multibuffer = cx.add_model(|cx| MultiBuffer::singleton(buffer, cx));
            let editor = cx.add_view(window_id, |cx| {
                Editor::for_buffer(multibuffer, Some(project), cx)
            });
            Ok(Box::new(editor) as Box<dyn ItemViewHandle>)
        }))
    }
}

impl ItemView for Editor {
    fn navigate(&mut self, data: Box<dyn std::any::Any>, cx: &mut ViewContext<Self>) {
        if let Some(data) = data.downcast_ref::<NavigationData>() {
            let buffer = self.buffer.read(cx).read(cx);
            let offset = if buffer.can_resolve(&data.anchor) {
                data.anchor.to_offset(&buffer)
            } else {
                buffer.clip_offset(data.offset, Bias::Left)
            };

            drop(buffer);
            let nav_history = self.nav_history.take();
            self.select_ranges([offset..offset], Some(Autoscroll::Fit), cx);
            self.nav_history = nav_history;
        }
    }

    fn tab_content(&self, style: &theme::Tab, cx: &AppContext) -> ElementBox {
        let title = self.title(cx);
        Label::new(title, style.label.clone()).boxed()
    }

    fn project_path(&self, cx: &AppContext) -> Option<ProjectPath> {
        File::from_dyn(self.buffer().read(cx).file(cx)).map(|file| ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        })
    }

    fn clone_on_split(&self, cx: &mut ViewContext<Self>) -> Option<Self>
    where
        Self: Sized,
    {
        Some(self.clone(cx))
    }

    fn set_nav_history(&mut self, history: ItemNavHistory, _: &mut ViewContext<Self>) {
        self.nav_history = Some(history);
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        let selection = self.newest_anchor_selection();
        self.push_to_nav_history(selection.head(), None, cx);
    }

    fn is_dirty(&self, cx: &AppContext) -> bool {
        self.buffer().read(cx).read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        self.buffer().read(cx).read(cx).has_conflict()
    }

    fn can_save(&self, cx: &AppContext) -> bool {
        !self.buffer().read(cx).is_singleton() || self.project_path(cx).is_some()
    }

    fn save(
        &mut self,
        project: ModelHandle<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer().clone();
        let buffers = buffer.read(cx).all_buffers();
        let transaction = project.update(cx, |project, cx| project.format(buffers, true, cx));
        cx.spawn(|this, mut cx| async move {
            let transaction = transaction.await.log_err();
            this.update(&mut cx, |editor, cx| {
                editor.request_autoscroll(Autoscroll::Fit, cx)
            });
            buffer
                .update(&mut cx, |buffer, cx| {
                    if let Some(transaction) = transaction {
                        if !buffer.is_singleton() {
                            buffer.push_transaction(&transaction.0);
                        }
                    }

                    buffer.save(cx)
                })
                .await?;
            Ok(())
        })
    }

    fn can_save_as(&self, cx: &AppContext) -> bool {
        self.buffer().read(cx).is_singleton()
    }

    fn save_as(
        &mut self,
        project: ModelHandle<Project>,
        abs_path: PathBuf,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<()>> {
        let buffer = self
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("cannot call save_as on an excerpt list")
            .clone();

        project.update(cx, |project, cx| {
            project.save_buffer_as(buffer, abs_path, cx)
        })
    }

    fn should_activate_item_on_event(event: &Event) -> bool {
        matches!(event, Event::Activate)
    }

    fn should_close_item_on_event(event: &Event) -> bool {
        matches!(event, Event::Closed)
    }

    fn should_update_tab_on_event(event: &Event) -> bool {
        matches!(event, Event::Saved | Event::Dirtied | Event::TitleChanged)
    }
}

pub struct CursorPosition {
    position: Option<Point>,
    selected_count: usize,
    _observe_active_editor: Option<Subscription>,
}

impl CursorPosition {
    pub fn new() -> Self {
        Self {
            position: None,
            selected_count: 0,
            _observe_active_editor: None,
        }
    }

    fn update_position(&mut self, editor: ViewHandle<Editor>, cx: &mut ViewContext<Self>) {
        let editor = editor.read(cx);
        let buffer = editor.buffer().read(cx).snapshot(cx);

        self.selected_count = 0;
        let mut last_selection: Option<Selection<usize>> = None;
        for selection in editor.local_selections::<usize>(cx) {
            self.selected_count += selection.end - selection.start;
            if last_selection
                .as_ref()
                .map_or(true, |last_selection| selection.id > last_selection.id)
            {
                last_selection = Some(selection);
            }
        }
        self.position = last_selection.map(|s| s.head().to_point(&buffer));

        cx.notify();
    }
}

impl Entity for CursorPosition {
    type Event = ();
}

impl View for CursorPosition {
    fn ui_name() -> &'static str {
        "CursorPosition"
    }

    fn render(&mut self, cx: &mut RenderContext<Self>) -> ElementBox {
        if let Some(position) = self.position {
            let theme = &cx.app_state::<Settings>().theme.workspace.status_bar;
            let mut text = format!("{},{}", position.row + 1, position.column + 1);
            if self.selected_count > 0 {
                write!(text, " ({} selected)", self.selected_count).unwrap();
            }
            Label::new(text, theme.cursor_position.clone()).boxed()
        } else {
            Empty::new().boxed()
        }
    }
}

impl StatusItemView for CursorPosition {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemViewHandle>,
        cx: &mut ViewContext<Self>,
    ) {
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            self._observe_active_editor = Some(cx.observe(&editor, Self::update_position));
            self.update_position(editor, cx);
        } else {
            self.position = None;
            self._observe_active_editor = None;
        }

        cx.notify();
    }
}

pub struct DiagnosticMessage {
    diagnostic: Option<Diagnostic>,
    _observe_active_editor: Option<Subscription>,
}

impl DiagnosticMessage {
    pub fn new() -> Self {
        Self {
            diagnostic: None,
            _observe_active_editor: None,
        }
    }

    fn update(&mut self, editor: ViewHandle<Editor>, cx: &mut ViewContext<Self>) {
        let editor = editor.read(cx);
        let buffer = editor.buffer().read(cx);
        let cursor_position = editor
            .newest_selection_with_snapshot::<usize>(&buffer.read(cx))
            .head();
        let new_diagnostic = buffer
            .read(cx)
            .diagnostics_in_range::<_, usize>(cursor_position..cursor_position, false)
            .filter(|entry| !entry.range.is_empty())
            .min_by_key(|entry| (entry.diagnostic.severity, entry.range.len()))
            .map(|entry| entry.diagnostic);
        if new_diagnostic != self.diagnostic {
            self.diagnostic = new_diagnostic;
            cx.notify();
        }
    }
}

impl Entity for DiagnosticMessage {
    type Event = ();
}

impl View for DiagnosticMessage {
    fn ui_name() -> &'static str {
        "DiagnosticMessage"
    }

    fn render(&mut self, cx: &mut RenderContext<Self>) -> ElementBox {
        if let Some(diagnostic) = &self.diagnostic {
            let theme = &cx.app_state::<Settings>().theme.workspace.status_bar;
            Label::new(
                diagnostic.message.split('\n').next().unwrap().to_string(),
                theme.diagnostic_message.clone(),
            )
            .boxed()
        } else {
            Empty::new().boxed()
        }
    }
}

impl StatusItemView for DiagnosticMessage {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemViewHandle>,
        cx: &mut ViewContext<Self>,
    ) {
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            self._observe_active_editor = Some(cx.observe(&editor, Self::update));
            self.update(editor, cx);
        } else {
            self.diagnostic = Default::default();
            self._observe_active_editor = None;
        }
        cx.notify();
    }
}
