use crate::persistence::DebuggerPaneItem;
use crate::session::DebugSession;
use crate::session::running::RunningState;
use crate::session::running::breakpoint_list::BreakpointList;
use crate::{
    ClearAllBreakpoints, Continue, CopyDebugAdapterArguments, Detach, FocusBreakpointList,
    FocusConsole, FocusFrames, FocusLoadedSources, FocusModules, FocusTerminal, FocusVariables,
    NewProcessModal, NewProcessMode, Pause, Restart, StepInto, StepOut, StepOver, Stop,
    ToggleExpandItem, ToggleSessionPicker, ToggleThreadPicker, persistence, spawn_task_or_modal,
};
use anyhow::{Context as _, Result, anyhow};
use dap::adapters::DebugAdapterName;
use dap::debugger_settings::DebugPanelDockPosition;
use dap::{
    ContinuedEvent, LoadedSourceEvent, ModuleEvent, OutputEvent, StoppedEvent, ThreadEvent,
    client::SessionId, debugger_settings::DebuggerSettings,
};
use dap::{DapRegistry, StartDebuggingRequestArguments};
use gpui::{
    Action, App, AsyncWindowContext, ClipboardItem, Context, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent, Point, Subscription, Task,
    WeakEntity, anchored, deferred,
};

use itertools::Itertools as _;
use language::Buffer;
use project::debugger::session::{Session, SessionStateEvent};
use project::{Fs, ProjectPath, WorktreeId};
use project::{Project, debugger::session::ThreadStatus};
use rpc::proto::{self};
use settings::Settings;
use std::sync::{Arc, LazyLock};
use task::{DebugScenario, TaskContext};
use tree_sitter::{Query, StreamingIterator as _};
use ui::{ContextMenu, Divider, PopoverMenuHandle, Tooltip, prelude::*};
use util::maybe;
use workspace::SplitDirection;
use workspace::{
    Pane, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::ToggleFocus;

pub enum DebugPanelEvent {
    Exited(SessionId),
    Terminated(SessionId),
    Stopped {
        client_id: SessionId,
        event: StoppedEvent,
        go_to_stack_frame: bool,
    },
    Thread((SessionId, ThreadEvent)),
    Continued((SessionId, ContinuedEvent)),
    Output((SessionId, OutputEvent)),
    Module((SessionId, ModuleEvent)),
    LoadedSource((SessionId, LoadedSourceEvent)),
    ClientShutdown(SessionId),
    CapabilitiesChanged(SessionId),
}

pub struct DebugPanel {
    size: Pixels,
    sessions: Vec<Entity<DebugSession>>,
    active_session: Option<Entity<DebugSession>>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    debug_scenario_scheduled_last: bool,
    pub(crate) thread_picker_menu_handle: PopoverMenuHandle<ContextMenu>,
    pub(crate) session_picker_menu_handle: PopoverMenuHandle<ContextMenu>,
    fs: Arc<dyn Fs>,
    is_zoomed: bool,
    _subscriptions: [Subscription; 1],
    breakpoint_list: Entity<BreakpointList>,
}

impl DebugPanel {
    pub fn new(
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let project = workspace.project().clone();
            let focus_handle = cx.focus_handle();
            let thread_picker_menu_handle = PopoverMenuHandle::default();
            let session_picker_menu_handle = PopoverMenuHandle::default();

            let focus_subscription = cx.on_focus(
                &focus_handle,
                window,
                |this: &mut DebugPanel, window, cx| {
                    this.focus_active_item(window, cx);
                },
            );

            Self {
                size: px(300.),
                sessions: vec![],
                active_session: None,
                focus_handle,
                breakpoint_list: BreakpointList::new(None, workspace.weak_handle(), &project, cx),
                project,
                workspace: workspace.weak_handle(),
                context_menu: None,
                fs: workspace.app_state().fs.clone(),
                thread_picker_menu_handle,
                session_picker_menu_handle,
                is_zoomed: false,
                _subscriptions: [focus_subscription],
                debug_scenario_scheduled_last: true,
            }
        })
    }

    pub(crate) fn focus_active_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_session.clone() else {
            return;
        };
        let active_pane = session
            .read(cx)
            .running_state()
            .read(cx)
            .active_pane()
            .clone();
        active_pane.update(cx, |pane, cx| {
            pane.focus_active_item(window, cx);
        });
    }

    pub(crate) fn sessions(&self) -> Vec<Entity<DebugSession>> {
        self.sessions.clone()
    }

    pub fn active_session(&self) -> Option<Entity<DebugSession>> {
        self.active_session.clone()
    }

    pub(crate) fn running_state(&self, cx: &mut App) -> Option<Entity<RunningState>> {
        self.active_session()
            .map(|session| session.read(cx).running_state().clone())
    }

    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let debug_panel = DebugPanel::new(workspace, window, cx);

                workspace.register_action(|workspace, _: &ClearAllBreakpoints, _, cx| {
                    workspace.project().read(cx).breakpoint_store().update(
                        cx,
                        |breakpoint_store, cx| {
                            breakpoint_store.clear_breakpoints(cx);
                        },
                    )
                });

                workspace.set_debugger_provider(DebuggerProvider(debug_panel.clone()));

                debug_panel
            })
        })
    }

    pub fn start_session(
        &mut self,
        scenario: DebugScenario,
        task_context: TaskContext,
        active_buffer: Option<Entity<Buffer>>,
        worktree_id: Option<WorktreeId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dap_store = self.project.read(cx).dap_store();
        let session = dap_store.update(cx, |dap_store, cx| {
            dap_store.new_session(
                scenario.label.clone(),
                DebugAdapterName(scenario.adapter.clone()),
                task_context.clone(),
                None,
                cx,
            )
        });
        let worktree = worktree_id.or_else(|| {
            active_buffer
                .as_ref()
                .and_then(|buffer| buffer.read(cx).file())
                .map(|f| f.worktree_id(cx))
        });
        let Some(worktree) = worktree
            .and_then(|id| self.project.read(cx).worktree_for_id(id, cx))
            .or_else(|| self.project.read(cx).visible_worktrees(cx).next())
        else {
            log::debug!("Could not find a worktree to spawn the debug session in");
            return;
        };
        self.debug_scenario_scheduled_last = true;
        if let Some(inventory) = self
            .project
            .read(cx)
            .task_store()
            .read(cx)
            .task_inventory()
            .cloned()
        {
            inventory.update(cx, |inventory, _| {
                inventory.scenario_scheduled(scenario.clone());
            })
        }
        let task = cx.spawn_in(window, {
            let session = session.clone();
            async move |this, cx| {
                let debug_session =
                    Self::register_session(this.clone(), session.clone(), true, cx).await?;
                let definition = debug_session
                    .update_in(cx, |debug_session, window, cx| {
                        debug_session.running_state().update(cx, |running, cx| {
                            running.resolve_scenario(
                                scenario,
                                task_context,
                                active_buffer,
                                worktree_id,
                                window,
                                cx,
                            )
                        })
                    })?
                    .await?;
                dap_store
                    .update(cx, |dap_store, cx| {
                        dap_store.boot_session(session.clone(), definition, worktree, cx)
                    })?
                    .await
            }
        });

        cx.spawn(async move |_, cx| {
            if let Err(error) = task.await {
                log::error!("{error}");
                session
                    .update(cx, |session, cx| {
                        session
                            .console_output(cx)
                            .unbounded_send(format!("error: {}", error))
                            .ok();
                        session.shutdown(cx)
                    })?
                    .await;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn rerun_last_session(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task_store = workspace.project().read(cx).task_store().clone();
        let Some(task_inventory) = task_store.read(cx).task_inventory() else {
            return;
        };
        let workspace = self.workspace.clone();
        let Some(scenario) = task_inventory.read(cx).last_scheduled_scenario().cloned() else {
            window.defer(cx, move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        NewProcessModal::show(workspace, window, NewProcessMode::Debug, None, cx);
                    })
                    .ok();
            });
            return;
        };

        cx.spawn_in(window, async move |this, cx| {
            let task_contexts = workspace
                .update_in(cx, |workspace, window, cx| {
                    tasks_ui::task_contexts(workspace, window, cx)
                })?
                .await;

            let task_context = task_contexts.active_context().cloned().unwrap_or_default();
            let worktree_id = task_contexts.worktree();

            this.update_in(cx, |this, window, cx| {
                this.start_session(
                    scenario.clone(),
                    task_context,
                    None,
                    worktree_id,
                    window,
                    cx,
                );
            })
        })
        .detach();
    }

    pub(crate) async fn register_session(
        this: WeakEntity<Self>,
        session: Entity<Session>,
        focus: bool,
        cx: &mut AsyncWindowContext,
    ) -> Result<Entity<DebugSession>> {
        let debug_session = register_session_inner(&this, session, cx).await?;

        let workspace = this.update_in(cx, |this, window, cx| {
            if focus {
                this.activate_session(debug_session.clone(), window, cx);
            }

            this.workspace.clone()
        })?;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<Self>(window, cx);
        })?;
        Ok(debug_session)
    }

    pub(crate) fn handle_restart_request(
        &mut self,
        mut curr_session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        while let Some(parent_session) = curr_session.read(cx).parent_session().cloned() {
            curr_session = parent_session;
        }

        let Some(worktree) = curr_session.read(cx).worktree() else {
            log::error!("Attempted to restart a non-running session");
            return;
        };

        let dap_store_handle = self.project.read(cx).dap_store().clone();
        let label = curr_session.read(cx).label().clone();
        let adapter = curr_session.read(cx).adapter().clone();
        let binary = curr_session.read(cx).binary().cloned().unwrap();
        let task = curr_session.update(cx, |session, cx| session.shutdown(cx));
        let task_context = curr_session.read(cx).task_context().clone();

        cx.spawn_in(window, async move |this, cx| {
            task.await;

            let (session, task) = dap_store_handle.update(cx, |dap_store, cx| {
                let session = dap_store.new_session(label, adapter, task_context, None, cx);

                let task = session.update(cx, |session, cx| {
                    session.boot(binary, worktree, dap_store_handle.downgrade(), cx)
                });
                (session, task)
            })?;
            Self::register_session(this.clone(), session.clone(), true, cx).await?;

            if let Err(error) = task.await {
                session
                    .update(cx, |session, cx| {
                        session
                            .console_output(cx)
                            .unbounded_send(format!(
                                "Session failed to restart with error: {}",
                                error
                            ))
                            .ok();
                        session.shutdown(cx)
                    })?
                    .await;

                return Err(error);
            };

            Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn handle_start_debugging_request(
        &mut self,
        request: &StartDebuggingRequestArguments,
        parent_session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = parent_session.read(cx).worktree() else {
            log::error!("Attempted to start a child-session from a non-running session");
            return;
        };

        let dap_store_handle = self.project.read(cx).dap_store().clone();
        let label = self.label_for_child_session(&parent_session, request, cx);
        let adapter = parent_session.read(cx).adapter().clone();
        let Some(mut binary) = parent_session.read(cx).binary().cloned() else {
            log::error!("Attempted to start a child-session without a binary");
            return;
        };
        let task_context = parent_session.read(cx).task_context().clone();
        binary.request_args = request.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (session, task) = dap_store_handle.update(cx, |dap_store, cx| {
                let session = dap_store.new_session(
                    label,
                    adapter,
                    task_context,
                    Some(parent_session.clone()),
                    cx,
                );

                let task = session.update(cx, |session, cx| {
                    session.boot(binary, worktree, dap_store_handle.downgrade(), cx)
                });
                (session, task)
            })?;
            // Focus child sessions if the parent has never emitted a stopped event;
            // this improves our JavaScript experience, as it always spawns a "main" session that then spawns subsessions.
            let parent_ever_stopped =
                parent_session.update(cx, |this, _| this.has_ever_stopped())?;
            Self::register_session(this, session, !parent_ever_stopped, cx).await?;
            task.await
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn close_session(
        &mut self,
        entity_id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .sessions
            .iter()
            .find(|other| entity_id == other.entity_id())
            .cloned()
        else {
            return;
        };
        session.update(cx, |this, cx| {
            this.running_state().update(cx, |this, cx| {
                this.serialize_layout(window, cx);
            });
        });
        let session_id = session.update(cx, |this, cx| this.session_id(cx));
        let should_prompt = self
            .project
            .update(cx, |this, cx| {
                let session = this.dap_store().read(cx).session_by_id(session_id);
                session.map(|session| !session.read(cx).is_terminated())
            })
            .unwrap_or_default();

        cx.spawn_in(window, async move |this, cx| {
            if should_prompt {
                let response = cx.prompt(
                    gpui::PromptLevel::Warning,
                    "This Debug Session is still running. Are you sure you want to terminate it?",
                    None,
                    &["Yes", "No"],
                );
                if response.await == Ok(1) {
                    return;
                }
            }
            session.update(cx, |session, cx| session.shutdown(cx)).ok();
            this.update(cx, |this, cx| {
                this.sessions.retain(|other| entity_id != other.entity_id());

                if let Some(active_session_id) = this
                    .active_session
                    .as_ref()
                    .map(|session| session.entity_id())
                {
                    if active_session_id == entity_id {
                        this.active_session = this.sessions.first().cloned();
                    }
                }
                cx.notify()
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(running_state) = self
            .active_session
            .as_ref()
            .map(|session| session.read(cx).running_state().clone())
        {
            let pane_items_status = running_state.read(cx).pane_items_status(cx);
            let this = cx.weak_entity();

            let context_menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
                for (item_kind, is_visible) in pane_items_status.into_iter() {
                    menu = menu.toggleable_entry(item_kind, is_visible, IconPosition::End, None, {
                        let this = this.clone();
                        move |window, cx| {
                            this.update(cx, |this, cx| {
                                if let Some(running_state) = this
                                    .active_session
                                    .as_ref()
                                    .map(|session| session.read(cx).running_state().clone())
                                {
                                    running_state.update(cx, |state, cx| {
                                        if is_visible {
                                            state.remove_pane_item(item_kind, window, cx);
                                        } else {
                                            state.add_pane_item(item_kind, position, window, cx);
                                        }
                                    })
                                }
                            })
                            .ok();
                        }
                    });
                }

                menu
            });

            window.focus(&context_menu.focus_handle(cx));
            let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
                this.context_menu.take();
                cx.notify();
            });
            self.context_menu = Some((context_menu, position, subscription));
        }
    }

    fn copy_debug_adapter_arguments(
        &mut self,
        _: &CopyDebugAdapterArguments,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let content = maybe!({
            let mut session = self.active_session()?.read(cx).session(cx);
            while let Some(parent) = session.read(cx).parent_session().cloned() {
                session = parent;
            }
            let binary = session.read(cx).binary()?;
            let content = serde_json::to_string_pretty(&binary).ok()?;
            Some(content)
        });
        if let Some(content) = content {
            cx.write_to_clipboard(ClipboardItem::new_string(content));
        }
    }

    pub(crate) fn top_controls_strip(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let active_session = self.active_session.clone();
        let focus_handle = self.focus_handle.clone();
        let is_side = self.position(window, cx).axis() == gpui::Axis::Horizontal;
        let div = if is_side { v_flex() } else { h_flex() };

        let new_session_button = || {
            IconButton::new("debug-new-session", IconName::Plus)
                .icon_size(IconSize::Small)
                .on_click({
                    move |_, window, cx| window.dispatch_action(crate::Start.boxed_clone(), cx)
                })
                .tooltip({
                    let focus_handle = focus_handle.clone();
                    move |window, cx| {
                        Tooltip::for_action_in(
                            "Start Debug Session",
                            &crate::Start,
                            &focus_handle,
                            window,
                            cx,
                        )
                    }
                })
        };
        let documentation_button = || {
            IconButton::new("debug-open-documentation", IconName::CircleHelp)
                .icon_size(IconSize::Small)
                .on_click(move |_, _, cx| cx.open_url("https://zed.dev/docs/debugger"))
                .tooltip(Tooltip::text("Open Documentation"))
        };

        Some(
            div.border_b_1()
                .border_color(cx.theme().colors().border)
                .p_1()
                .justify_between()
                .w_full()
                .when(is_side, |this| this.gap_1())
                .child(
                    h_flex()
                        .child(
                            h_flex().gap_2().w_full().when_some(
                                active_session
                                    .as_ref()
                                    .map(|session| session.read(cx).running_state()),
                                |this, running_state| {
                                    let thread_status =
                                        running_state.read(cx).thread_status(cx).unwrap_or(
                                            project::debugger::session::ThreadStatus::Exited,
                                        );
                                    let capabilities = running_state.read(cx).capabilities(cx);
                                    let supports_detach =
                                        running_state.read(cx).session().read(cx).is_attached();
                                    this.map(|this| {
                                        if thread_status == ThreadStatus::Running {
                                            this.child(
                                                IconButton::new(
                                                    "debug-pause",
                                                    IconName::DebugPause,
                                                )
                                                .icon_size(IconSize::XSmall)
                                                .shape(ui::IconButtonShape::Square)
                                                .on_click(window.listener_for(
                                                    &running_state,
                                                    |this, _, _window, cx| {
                                                        this.pause_thread(cx);
                                                    },
                                                ))
                                                .tooltip({
                                                    let focus_handle = focus_handle.clone();
                                                    move |window, cx| {
                                                        Tooltip::for_action_in(
                                                            "Pause program",
                                                            &Pause,
                                                            &focus_handle,
                                                            window,
                                                            cx,
                                                        )
                                                    }
                                                }),
                                            )
                                        } else {
                                            this.child(
                                                IconButton::new(
                                                    "debug-continue",
                                                    IconName::DebugContinue,
                                                )
                                                .icon_size(IconSize::XSmall)
                                                .shape(ui::IconButtonShape::Square)
                                                .on_click(window.listener_for(
                                                    &running_state,
                                                    |this, _, _window, cx| this.continue_thread(cx),
                                                ))
                                                .disabled(thread_status != ThreadStatus::Stopped)
                                                .tooltip({
                                                    let focus_handle = focus_handle.clone();
                                                    move |window, cx| {
                                                        Tooltip::for_action_in(
                                                            "Continue program",
                                                            &Continue,
                                                            &focus_handle,
                                                            window,
                                                            cx,
                                                        )
                                                    }
                                                }),
                                            )
                                        }
                                    })
                                    .child(
                                        IconButton::new("debug-step-over", IconName::ArrowRight)
                                            .icon_size(IconSize::XSmall)
                                            .shape(ui::IconButtonShape::Square)
                                            .on_click(window.listener_for(
                                                &running_state,
                                                |this, _, _window, cx| {
                                                    this.step_over(cx);
                                                },
                                            ))
                                            .disabled(thread_status != ThreadStatus::Stopped)
                                            .tooltip({
                                                let focus_handle = focus_handle.clone();
                                                move |window, cx| {
                                                    Tooltip::for_action_in(
                                                        "Step over",
                                                        &StepOver,
                                                        &focus_handle,
                                                        window,
                                                        cx,
                                                    )
                                                }
                                            }),
                                    )
                                    .child(
                                        IconButton::new("debug-step-out", IconName::ArrowUpRight)
                                            .icon_size(IconSize::XSmall)
                                            .shape(ui::IconButtonShape::Square)
                                            .on_click(window.listener_for(
                                                &running_state,
                                                |this, _, _window, cx| {
                                                    this.step_out(cx);
                                                },
                                            ))
                                            .disabled(thread_status != ThreadStatus::Stopped)
                                            .tooltip({
                                                let focus_handle = focus_handle.clone();
                                                move |window, cx| {
                                                    Tooltip::for_action_in(
                                                        "Step out",
                                                        &StepOut,
                                                        &focus_handle,
                                                        window,
                                                        cx,
                                                    )
                                                }
                                            }),
                                    )
                                    .child(
                                        IconButton::new(
                                            "debug-step-into",
                                            IconName::ArrowDownRight,
                                        )
                                        .icon_size(IconSize::XSmall)
                                        .shape(ui::IconButtonShape::Square)
                                        .on_click(window.listener_for(
                                            &running_state,
                                            |this, _, _window, cx| {
                                                this.step_in(cx);
                                            },
                                        ))
                                        .disabled(thread_status != ThreadStatus::Stopped)
                                        .tooltip({
                                            let focus_handle = focus_handle.clone();
                                            move |window, cx| {
                                                Tooltip::for_action_in(
                                                    "Step in",
                                                    &StepInto,
                                                    &focus_handle,
                                                    window,
                                                    cx,
                                                )
                                            }
                                        }),
                                    )
                                    .child(Divider::vertical())
                                    .child(
                                        IconButton::new("debug-restart", IconName::DebugRestart)
                                            .icon_size(IconSize::XSmall)
                                            .on_click(window.listener_for(
                                                &running_state,
                                                |this, _, _window, cx| {
                                                    this.restart_session(cx);
                                                },
                                            ))
                                            .tooltip({
                                                let focus_handle = focus_handle.clone();
                                                move |window, cx| {
                                                    Tooltip::for_action_in(
                                                        "Restart",
                                                        &Restart,
                                                        &focus_handle,
                                                        window,
                                                        cx,
                                                    )
                                                }
                                            }),
                                    )
                                    .child(
                                        IconButton::new("debug-stop", IconName::Power)
                                            .icon_size(IconSize::XSmall)
                                            .on_click(window.listener_for(
                                                &running_state,
                                                |this, _, _window, cx| {
                                                    this.stop_thread(cx);
                                                },
                                            ))
                                            .disabled(
                                                thread_status != ThreadStatus::Stopped
                                                    && thread_status != ThreadStatus::Running,
                                            )
                                            .tooltip({
                                                let focus_handle = focus_handle.clone();
                                                let label = if capabilities
                                                    .supports_terminate_threads_request
                                                    .unwrap_or_default()
                                                {
                                                    "Terminate Thread"
                                                } else {
                                                    "Terminate All Threads"
                                                };
                                                move |window, cx| {
                                                    Tooltip::for_action_in(
                                                        label,
                                                        &Stop,
                                                        &focus_handle,
                                                        window,
                                                        cx,
                                                    )
                                                }
                                            }),
                                    )
                                    .when(
                                        supports_detach,
                                        |div| {
                                            div.child(
                                                IconButton::new(
                                                    "debug-disconnect",
                                                    IconName::DebugDetach,
                                                )
                                                .disabled(
                                                    thread_status != ThreadStatus::Stopped
                                                        && thread_status != ThreadStatus::Running,
                                                )
                                                .icon_size(IconSize::XSmall)
                                                .on_click(window.listener_for(
                                                    &running_state,
                                                    |this, _, _, cx| {
                                                        this.detach_client(cx);
                                                    },
                                                ))
                                                .tooltip({
                                                    let focus_handle = focus_handle.clone();
                                                    move |window, cx| {
                                                        Tooltip::for_action_in(
                                                            "Detach",
                                                            &Detach,
                                                            &focus_handle,
                                                            window,
                                                            cx,
                                                        )
                                                    }
                                                }),
                                            )
                                        },
                                    )
                                },
                            ),
                        )
                        .justify_around()
                        .when(is_side, |this| {
                            this.child(new_session_button())
                                .child(documentation_button())
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .when(is_side, |this| this.justify_between())
                        .child(
                            h_flex().when_some(
                                active_session
                                    .as_ref()
                                    .map(|session| session.read(cx).running_state())
                                    .cloned(),
                                |this, running_state| {
                                    this.children({
                                        let running_state = running_state.clone();
                                        let threads =
                                            running_state.update(cx, |running_state, cx| {
                                                let session = running_state.session();
                                                session.read(cx).is_running().then(|| {
                                                    session.update(cx, |session, cx| {
                                                        session.threads(cx)
                                                    })
                                                })
                                            });

                                        threads.and_then(|threads| {
                                            self.render_thread_dropdown(
                                                &running_state,
                                                threads,
                                                window,
                                                cx,
                                            )
                                        })
                                    })
                                    .when(!is_side, |this| this.gap_2().child(Divider::vertical()))
                                },
                            ),
                        )
                        .child(
                            h_flex()
                                .children(self.render_session_menu(
                                    self.active_session(),
                                    self.running_state(cx),
                                    window,
                                    cx,
                                ))
                                .when(!is_side, |this| {
                                    this.child(new_session_button())
                                        .child(documentation_button())
                                }),
                        ),
                ),
        )
    }

    pub(crate) fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.active_session() {
            session.update(cx, |session, cx| {
                session.running_state().update(cx, |running, cx| {
                    running.activate_pane_in_direction(direction, window, cx);
                })
            });
        }
    }

    pub(crate) fn activate_item(
        &mut self,
        item: DebuggerPaneItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.active_session() {
            session.update(cx, |session, cx| {
                session.running_state().update(cx, |running, cx| {
                    running.activate_item(item, window, cx);
                });
            });
        }
    }

    pub(crate) fn activate_session_by_id(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.read(cx).session_id(cx) == session_id)
        {
            self.activate_session(session.clone(), window, cx);
        }
    }

    pub(crate) fn activate_session(
        &mut self,
        session_item: Entity<DebugSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(self.sessions.contains(&session_item));
        session_item.focus_handle(cx).focus(window);
        session_item.update(cx, |this, cx| {
            this.running_state().update(cx, |this, cx| {
                this.go_to_selected_stack_frame(window, cx);
            });
        });
        self.active_session = Some(session_item);
        cx.notify();
    }

    pub(crate) fn save_scenario(
        &self,
        scenario: &DebugScenario,
        worktree_id: WorktreeId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<ProjectPath>> {
        self.workspace
            .update(cx, |workspace, cx| {
                let Some(mut path) = workspace.absolute_path_of_worktree(worktree_id, cx) else {
                    return Task::ready(Err(anyhow!("Couldn't get worktree path")));
                };

                let serialized_scenario = serde_json::to_value(scenario);

                cx.spawn_in(window, async move |workspace, cx| {
                    let serialized_scenario = serialized_scenario?;
                    let fs =
                        workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone())?;

                    path.push(paths::local_settings_folder_relative_path());
                    if !fs.is_dir(path.as_path()).await {
                        fs.create_dir(path.as_path()).await?;
                    }
                    path.pop();

                    path.push(paths::local_debug_file_relative_path());
                    let path = path.as_path();

                    if !fs.is_file(path).await {
                        fs.create_file(path, Default::default()).await?;
                        fs.write(
                            path,
                            settings::initial_local_debug_tasks_content()
                                .to_string()
                                .as_bytes(),
                        )
                        .await?;
                    }

                    let mut content = fs.load(path).await?;
                    let new_scenario = serde_json_lenient::to_string_pretty(&serialized_scenario)?
                        .lines()
                        .map(|l| format!("  {l}"))
                        .join("\n");

                    static ARRAY_QUERY: LazyLock<Query> = LazyLock::new(|| {
                        Query::new(
                            &tree_sitter_json::LANGUAGE.into(),
                            "(document (array (object) @object))", // TODO: use "." anchor to only match last object
                        )
                        .expect("Failed to create ARRAY_QUERY")
                    });

                    let mut parser = tree_sitter::Parser::new();
                    parser
                        .set_language(&tree_sitter_json::LANGUAGE.into())
                        .unwrap();
                    let mut cursor = tree_sitter::QueryCursor::new();
                    let syntax_tree = parser.parse(&content, None).unwrap();
                    let mut matches =
                        cursor.matches(&ARRAY_QUERY, syntax_tree.root_node(), content.as_bytes());

                    // we don't have `.last()` since it's a lending iterator, so loop over
                    // the whole thing to find the last one
                    let mut last_offset = None;
                    while let Some(mat) = matches.next() {
                        if let Some(pos) = mat.captures.first().map(|m| m.node.byte_range().end) {
                            last_offset = Some(pos)
                        }
                    }

                    if let Some(pos) = last_offset {
                        content.insert_str(pos, &new_scenario);
                        content.insert_str(pos, ",\n");
                    }

                    fs.write(path, content.as_bytes()).await?;

                    workspace.update(cx, |workspace, cx| {
                        workspace
                            .project()
                            .read(cx)
                            .project_path_for_absolute_path(&path, cx)
                            .context(
                                "Couldn't get project path for .zed/debug.json in active worktree",
                            )
                    })?
                })
            })
            .unwrap_or_else(|err| Task::ready(Err(err)))
    }

    pub(crate) fn toggle_thread_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.thread_picker_menu_handle.toggle(window, cx);
    }

    pub(crate) fn toggle_session_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session_picker_menu_handle.toggle(window, cx);
    }

    fn toggle_zoom(
        &mut self,
        _: &workspace::ToggleZoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_zoomed {
            cx.emit(PanelEvent::ZoomOut);
        } else {
            if !self.focus_handle(cx).contains_focused(window, cx) {
                cx.focus_self(window);
            }
            cx.emit(PanelEvent::ZoomIn);
        }
    }

    fn label_for_child_session(
        &self,
        parent_session: &Entity<Session>,
        request: &StartDebuggingRequestArguments,
        cx: &mut Context<'_, Self>,
    ) -> SharedString {
        let adapter = parent_session.read(cx).adapter();
        if let Some(adapter) = DapRegistry::global(cx).adapter(&adapter) {
            if let Some(label) = adapter.label_for_child_session(request) {
                return label.into();
            }
        }
        let mut label = parent_session.read(cx).label().clone();
        if !label.ends_with("(child)") {
            label = format!("{label} (child)").into();
        }
        label
    }
}

async fn register_session_inner(
    this: &WeakEntity<DebugPanel>,
    session: Entity<Session>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<DebugSession>> {
    let adapter_name = session.read_with(cx, |session, _| session.adapter())?;
    this.update_in(cx, |_, window, cx| {
        cx.subscribe_in(
            &session,
            window,
            move |this, session, event: &SessionStateEvent, window, cx| match event {
                SessionStateEvent::Restart => {
                    this.handle_restart_request(session.clone(), window, cx);
                }
                SessionStateEvent::SpawnChildSession { request } => {
                    this.handle_start_debugging_request(request, session.clone(), window, cx);
                }
                _ => {}
            },
        )
        .detach();
    })
    .ok();
    let serialized_layout = persistence::get_serialized_layout(adapter_name).await;
    let debug_session = this.update_in(cx, |this, window, cx| {
        let parent_session = this
            .sessions
            .iter()
            .find(|p| Some(p.read(cx).session_id(cx)) == session.read(cx).parent_id(cx))
            .cloned();
        this.sessions.retain(|session| {
            !session
                .read(cx)
                .running_state()
                .read(cx)
                .session()
                .read(cx)
                .is_terminated()
        });

        let debug_session = DebugSession::running(
            this.project.clone(),
            this.workspace.clone(),
            parent_session
                .as_ref()
                .map(|p| p.read(cx).running_state().read(cx).debug_terminal.clone()),
            session,
            serialized_layout,
            this.position(window, cx).axis(),
            window,
            cx,
        );

        // We might want to make this an event subscription and only notify when a new thread is selected
        // This is used to filter the command menu correctly
        cx.observe(
            &debug_session.read(cx).running_state().clone(),
            |_, _, cx| cx.notify(),
        )
        .detach();
        let insert_position = this
            .sessions
            .iter()
            .position(|session| Some(session) == parent_session.as_ref())
            .map(|position| position + 1)
            .unwrap_or(this.sessions.len());
        // Maintain topological sort order of sessions
        this.sessions.insert(insert_position, debug_session.clone());

        debug_session
    })?;
    Ok(debug_session)
}

impl EventEmitter<PanelEvent> for DebugPanel {}
impl EventEmitter<DebugPanelEvent> for DebugPanel {}

impl Focusable for DebugPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for DebugPanel {
    fn persistent_name() -> &'static str {
        "DebugPanel"
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        match DebuggerSettings::get_global(cx).dock {
            DebugPanelDockPosition::Left => DockPosition::Left,
            DebugPanelDockPosition::Bottom => DockPosition::Bottom,
            DebugPanelDockPosition::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if position.axis() != self.position(window, cx).axis() {
            self.sessions.iter().for_each(|session_item| {
                session_item.update(cx, |item, cx| {
                    item.running_state()
                        .update(cx, |state, _| state.invert_axies())
                })
            });
        }

        settings::update_settings_file::<DebuggerSettings>(
            self.fs.clone(),
            cx,
            move |settings, _| {
                let dock = match position {
                    DockPosition::Left => DebugPanelDockPosition::Left,
                    DockPosition::Bottom => DebugPanelDockPosition::Bottom,
                    DockPosition::Right => DebugPanelDockPosition::Right,
                };
                settings.dock = dock;
            },
        );
    }

    fn size(&self, _window: &Window, _: &App) -> Pixels {
        self.size
    }

    fn set_size(&mut self, size: Option<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) {
        self.size = size.unwrap_or(px(300.));
    }

    fn remote_id() -> Option<proto::PanelId> {
        Some(proto::PanelId::DebugPanel)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Debug)
    }

    fn icon_tooltip(&self, _window: &Window, cx: &App) -> Option<&'static str> {
        if DebuggerSettings::get_global(cx).button {
            Some("Debug Panel")
        } else {
            None
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        None
    }

    fn activation_priority(&self) -> u32 {
        9
    }

    fn set_active(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) {}

    fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
        self.is_zoomed
    }

    fn set_zoomed(&mut self, zoomed: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.is_zoomed = zoomed;
        cx.notify();
    }
}

impl Render for DebugPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_sessions = self.sessions.len() > 0;
        let this = cx.weak_entity();
        debug_assert_eq!(has_sessions, self.active_session.is_some());

        if self
            .active_session
            .as_ref()
            .map(|session| session.read(cx).running_state())
            .map(|state| state.read(cx).has_open_context_menu(cx))
            .unwrap_or(false)
        {
            self.context_menu.take();
        }

        v_flex()
            .size_full()
            .key_context("DebugPanel")
            .child(h_flex().children(self.top_controls_strip(window, cx)))
            .track_focus(&self.focus_handle(cx))
            .on_action({
                let this = this.clone();
                move |_: &workspace::ActivatePaneLeft, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_pane_in_direction(SplitDirection::Left, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &workspace::ActivatePaneRight, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_pane_in_direction(SplitDirection::Right, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &workspace::ActivatePaneUp, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_pane_in_direction(SplitDirection::Up, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &workspace::ActivatePaneDown, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_pane_in_direction(SplitDirection::Down, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &FocusConsole, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_item(DebuggerPaneItem::Console, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &FocusVariables, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_item(DebuggerPaneItem::Variables, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &FocusBreakpointList, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_item(DebuggerPaneItem::BreakpointList, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &FocusFrames, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_item(DebuggerPaneItem::Frames, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &FocusModules, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_item(DebuggerPaneItem::Modules, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &FocusLoadedSources, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_item(DebuggerPaneItem::LoadedSources, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &FocusTerminal, window, cx| {
                    this.update(cx, |this, cx| {
                        this.activate_item(DebuggerPaneItem::Terminal, window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &ToggleThreadPicker, window, cx| {
                    this.update(cx, |this, cx| {
                        this.toggle_thread_picker(window, cx);
                    })
                    .ok();
                }
            })
            .on_action({
                let this = this.clone();
                move |_: &ToggleSessionPicker, window, cx| {
                    this.update(cx, |this, cx| {
                        this.toggle_session_picker(window, cx);
                    })
                    .ok();
                }
            })
            .on_action(cx.listener(Self::toggle_zoom))
            .on_action(cx.listener(|panel, _: &ToggleExpandItem, _, cx| {
                let Some(session) = panel.active_session() else {
                    return;
                };
                let active_pane = session
                    .read(cx)
                    .running_state()
                    .read(cx)
                    .active_pane()
                    .clone();
                active_pane.update(cx, |pane, cx| {
                    let is_zoomed = pane.is_zoomed();
                    pane.set_zoomed(!is_zoomed, cx);
                });
                cx.notify();
            }))
            .on_action(cx.listener(Self::copy_debug_adapter_arguments))
            .when(self.active_session.is_some(), |this| {
                this.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        if this
                            .active_session
                            .as_ref()
                            .map(|session| {
                                let state = session.read(cx).running_state();
                                state.read(cx).has_pane_at_position(event.position)
                            })
                            .unwrap_or(false)
                        {
                            this.deploy_context_menu(event.position, window, cx);
                        }
                    }),
                )
                .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                    deferred(
                        anchored()
                            .position(*position)
                            .anchor(gpui::Corner::TopLeft)
                            .child(menu.clone()),
                    )
                    .with_priority(1)
                }))
            })
            .map(|this| {
                if has_sessions {
                    this.children(self.active_session.clone())
                } else {
                    this.child(
                        v_flex()
                            .h_full()
                            .gap_1()
                            .items_center()
                            .justify_center()
                            .child(
                                h_flex().size_full()
                                    .items_start()

                                    .child(v_flex().items_start().min_w_1_3().h_full().p_1()
                                        .child(h_flex().px_1().child(Label::new("Breakpoints").size(LabelSize::Small)))
                                        .child(Divider::horizontal())
                                        .child(self.breakpoint_list.clone()))
                                    .child(Divider::vertical())
                                    .child(
                                        v_flex().w_2_3().h_full().items_center().justify_center()
                                            .gap_2()
                                            .pr_8()
                                            .child(
                                                Button::new("spawn-new-session-empty-state", "New Session")
                                                    .icon(IconName::Plus)
                                                    .icon_size(IconSize::XSmall)
                                                    .icon_color(Color::Muted)
                                                    .icon_position(IconPosition::Start)
                                                    .on_click(|_, window, cx| {
                                                        window.dispatch_action(crate::Start.boxed_clone(), cx);
                                                    })
                                            )
                                            .child(
                                                Button::new("edit-debug-settings", "Edit debug.json")
                                                    .icon(IconName::Code)
                                                    .icon_size(IconSize::XSmall)
                                                    .color(Color::Muted)
                                                    .icon_color(Color::Muted)
                                                    .icon_position(IconPosition::Start)
                                                    .on_click(|_, window, cx| {
                                                        window.dispatch_action(zed_actions::OpenProjectDebugTasks.boxed_clone(), cx);
                                                    })
                                            )
                                            .child(
                                                Button::new("open-debugger-docs", "Debugger Docs")
                                                    .icon(IconName::Book)
                                                    .color(Color::Muted)
                                                    .icon_size(IconSize::XSmall)
                                                    .icon_color(Color::Muted)
                                                    .icon_position(IconPosition::Start)
                                                    .on_click(|_, _, cx| {
                                                        cx.open_url("https://zed.dev/docs/debugger")
                                                    })
                                            )
                                            .child(
                                                Button::new("spawn-new-session-install-extensions", "Debugger Extensions")
                                                    .icon(IconName::Blocks)
                                                    .color(Color::Muted)
                                                    .icon_size(IconSize::XSmall)
                                                    .icon_color(Color::Muted)
                                                    .icon_position(IconPosition::Start)
                                                    .on_click(|_, window, cx| {
                                                        window.dispatch_action(zed_actions::Extensions { category_filter: Some(zed_actions::ExtensionCategoryFilter::DebugAdapters)}.boxed_clone(), cx);
                                                    })
                                            )
                                    )
                            )
                    )
                }
            })
            .into_any()
    }
}

struct DebuggerProvider(Entity<DebugPanel>);

impl workspace::DebuggerProvider for DebuggerProvider {
    fn start_session(
        &self,
        definition: DebugScenario,
        context: TaskContext,
        buffer: Option<Entity<Buffer>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0.update(cx, |_, cx| {
            cx.defer_in(window, |this, window, cx| {
                this.start_session(definition, context, buffer, None, window, cx);
            })
        })
    }

    fn spawn_task_or_modal(
        &self,
        workspace: &mut Workspace,
        action: &tasks_ui::Spawn,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        spawn_task_or_modal(workspace, action, window, cx);
    }

    fn debug_scenario_scheduled(&self, cx: &mut App) {
        self.0.update(cx, |this, _| {
            this.debug_scenario_scheduled_last = true;
        });
    }

    fn task_scheduled(&self, cx: &mut App) {
        self.0.update(cx, |this, _| {
            this.debug_scenario_scheduled_last = false;
        })
    }

    fn debug_scenario_scheduled_last(&self, cx: &App) -> bool {
        self.0.read(cx).debug_scenario_scheduled_last
    }

    fn active_thread_state(&self, cx: &App) -> Option<ThreadStatus> {
        let session = self.0.read(cx).active_session()?;
        let thread = session.read(cx).running_state().read(cx).thread_id()?;
        session.read(cx).session(cx).read(cx).thread_state(thread)
    }
}
