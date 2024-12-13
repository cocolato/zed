use std::sync::Arc;

use anyhow::Result;
use assistant_tool::ToolWorkingSet;
use client::zed_urls;
use gpui::{
    prelude::*, px, svg, Action, AnyElement, AppContext, AppContext, EventEmitter, FocusHandle,
    FocusableView, FontWeight, Model, Pixels, Task, View, WeakView,
};
use language::LanguageRegistry;
use language_model::LanguageModelRegistry;
use language_model_selector::LanguageModelSelector;
use time::UtcOffset;
use ui::{prelude::*, ButtonLike, Divider, IconButtonShape, KeyBinding, Tab, Tooltip};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::Workspace;

use crate::active_thread::ActiveThread;
use crate::message_editor::MessageEditor;
use crate::thread::{ThreadError, ThreadId};
use crate::thread_history::{PastThread, ThreadHistory};
use crate::thread_store::ThreadStore;
use crate::{NewThread, OpenHistory, ToggleFocus, ToggleModelSelector};

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(
        |workspace: &mut Workspace, model: &Model<Workspace>, _cx: &mut AppContext| {
            workspace.register_action(model, |workspace, _: &ToggleFocus, cx| {
                workspace.toggle_panel_focus::<AssistantPanel>(cx);
            });
        },
    )
    .detach();
}

enum ActiveView {
    Thread,
    History,
}

pub struct AssistantPanel {
    workspace: WeakModel<Workspace>,
    language_registry: Arc<LanguageRegistry>,
    thread_store: Model<ThreadStore>,
    thread: Model<ActiveThread>,
    message_editor: Model<MessageEditor>,
    tools: Arc<ToolWorkingSet>,
    local_timezone: UtcOffset,
    active_view: ActiveView,
    history: Model<ThreadHistory>,
}

impl AssistantPanel {
    pub fn load(
        workspace: WeakModel<Workspace>,
        window: AnyWindowHandle,
        cx: AsyncAppContext,
    ) -> Task<Result<Model<Self>>> {
        cx.spawn(|mut cx| async move {
            let tools = Arc::new(ToolWorkingSet::default());
            let thread_store = workspace
                .update(&mut cx, |workspace, cx| {
                    let project = workspace.project().clone();
                    ThreadStore::new(project, tools.clone(), cx)
                })?
                .await?;

            workspace.update(&mut cx, |workspace, cx| {
                cx.new_model(|model, cx| Self::new(workspace, thread_store, tools, model, cx))
            })
        })
    }

    fn new(
        workspace: &Workspace,
        thread_store: Model<ThreadStore>,
        tools: Arc<ToolWorkingSet>,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) -> Self {
        let thread = thread_store.update(cx, |this, model, cx| this.create_thread(model, cx));
        let language_registry = workspace.project().read(cx).languages().clone();
        let workspace = workspace.weak_handle();
        let weak_self = model.downgrade();

        Self {
            active_view: ActiveView::Thread,
            workspace: workspace.clone(),
            language_registry: language_registry.clone(),
            thread_store: thread_store.clone(),
            thread: cx.new_model(|model, cx| {
                ActiveThread::new(
                    thread.clone(),
                    workspace,
                    language_registry,
                    tools.clone(),
                    model,
                    cx,
                )
            }),
            message_editor: cx.new_model(|model, cx| MessageEditor::new(thread.clone(), model, cx)),
            tools,
            local_timezone: UtcOffset::from_whole_seconds(
                chrono::Local::now().offset().local_minus_utc(),
            )
            .unwrap(),
            history: cx
                .new_model(|model, cx| ThreadHistory::new(weak_self, thread_store, model, cx)),
        }
    }

    pub(crate) fn local_timezone(&self) -> UtcOffset {
        self.local_timezone
    }

    fn new_thread(&mut self, model: &Model<Self>, cx: &mut AppContext) {
        let thread = self
            .thread_store
            .update(cx, |this, model, cx| this.create_thread(model, cx));

        self.active_view = ActiveView::Thread;
        self.thread = cx.new_model(|model, cx| {
            ActiveThread::new(
                thread.clone(),
                self.workspace.clone(),
                self.language_registry.clone(),
                self.tools.clone(),
                model,
                cx,
            )
        });
        self.message_editor = cx.new_model(|model, cx| MessageEditor::new(thread, model, cx));
        self.message_editor.focus_handle(cx).focus(window);
    }

    pub(crate) fn open_thread(
        &mut self,
        thread_id: &ThreadId,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) {
        let Some(thread) = self
            .thread_store
            .update(cx, |this, model, cx| this.open_thread(thread_id, model, cx))
        else {
            return;
        };

        self.active_view = ActiveView::Thread;
        self.thread = cx.new_model(|model, cx| {
            ActiveThread::new(
                thread.clone(),
                self.workspace.clone(),
                self.language_registry.clone(),
                self.tools.clone(),
                model,
                cx,
            )
        });
        self.message_editor = cx.new_model(|model, cx| MessageEditor::new(thread, model, cx));
        self.message_editor.focus_handle(cx).focus(window);
    }

    pub(crate) fn delete_thread(
        &mut self,
        thread_id: &ThreadId,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) {
        self.thread_store.update(cx, |this, model, cx| {
            this.delete_thread(thread_id, model, cx)
        });
    }
}

impl FocusableView for AssistantPanel {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        match self.active_view {
            ActiveView::Thread => self.message_editor.focus_handle(cx),
            ActiveView::History => self.history.focus_handle(cx),
        }
    }
}

impl EventEmitter<PanelEvent> for AssistantPanel {}

impl Panel for AssistantPanel {
    fn persistent_name() -> &'static str {
        "AssistantPanel2"
    }

    fn position(&self, _window: &Window, cx: &AppContext) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, _position: DockPosition, model: &Model<Self>, _cx: &mut AppContext) {
    }

    fn size(&self, _window: &Window, cx: &AppContext) -> Pixels {
        px(640.)
    }

    fn set_size(&mut self, _size: Option<Pixels>, model: &Model<Self>, _cx: &mut AppContext) {}

    fn set_active(&mut self, _active: bool, model: &Model<Self>, _cx: &mut AppContext) {}

    fn remote_id() -> Option<proto::PanelId> {
        Some(proto::PanelId::AssistantPanel)
    }

    fn icon(&self, _window: &Window, cx: &AppContext) -> Option<IconName> {
        Some(IconName::ZedAssistant)
    }

    fn icon_tooltip(&self, _window: &Window, cx: &AppContext) -> Option<&'static str> {
        Some("Assistant Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
}

impl AssistantPanel {
    fn render_toolbar(&self, model: &Model<Self>, cx: &mut AppContext) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);

        h_flex()
            .id("assistant-toolbar")
            .justify_between()
            .gap(DynamicSpacing::Base08.rems(cx))
            .h(Tab::container_height(cx))
            .px(DynamicSpacing::Base08.rems(cx))
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(h_flex().children(self.thread.read(cx).summary(cx).map(Label::new)))
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .child(self.render_language_model_selector(model, cx))
                    .child(Divider::vertical())
                    .child(
                        IconButton::new("new-thread", IconName::Plus)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |cx| {
                                    Tooltip::for_action_in(
                                        "New Thread",
                                        &NewThread,
                                        &focus_handle,
                                        window,
                                        cx,
                                    )
                                }
                            })
                            .on_click(move |_event, cx| {
                                cx.dispatch_action(NewThread.boxed_clone());
                            }),
                    )
                    .child(
                        IconButton::new("open-history", IconName::HistoryRerun)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip({
                                let focus_handle = focus_handle.clone();
                                move |cx| {
                                    Tooltip::for_action_in(
                                        "Open History",
                                        &OpenHistory,
                                        &focus_handle,
                                        window,
                                        cx,
                                    )
                                }
                            })
                            .on_click(move |_event, cx| {
                                cx.dispatch_action(OpenHistory.boxed_clone());
                            }),
                    )
                    .child(
                        IconButton::new("configure-assistant", IconName::Settings)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(move |window, cx| Tooltip::text("Configure Assistant", cx))
                            .on_click(move |_event, _cx| {
                                println!("Configure Assistant");
                            }),
                    ),
            )
    }

    fn render_language_model_selector(
        &self,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) -> impl IntoElement {
        let active_provider = LanguageModelRegistry::read_global(cx).active_provider();
        let active_model = LanguageModelRegistry::read_global(cx).active_model();

        LanguageModelSelector::new(
            |model, _cx| {
                println!("Selected {:?}", model.name());
            },
            ButtonLike::new("active-model")
                .style(ButtonStyle::Subtle)
                .child(
                    h_flex()
                        .w_full()
                        .gap_0p5()
                        .child(
                            div()
                                .overflow_x_hidden()
                                .flex_grow()
                                .whitespace_nowrap()
                                .child(match (active_provider, active_model) {
                                    (Some(provider), Some(model)) => h_flex()
                                        .gap_1()
                                        .child(
                                            Icon::new(
                                                model.icon().unwrap_or_else(|| provider.icon()),
                                            )
                                            .color(Color::Muted)
                                            .size(IconSize::XSmall),
                                        )
                                        .child(
                                            Label::new(model.name().0)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .into_any_element(),
                                    _ => Label::new("No model selected")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .into_any_element(),
                                }),
                        )
                        .child(
                            Icon::new(IconName::ChevronDown)
                                .color(Color::Muted)
                                .size(IconSize::XSmall),
                        ),
                )
                .tooltip(move |window, cx| {
                    Tooltip::for_action("Change Model", &ToggleModelSelector, model, cx)
                }),
        )
    }

    fn render_active_thread_or_empty_state(
        &self,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) -> AnyElement {
        if self.thread.read(cx).is_empty() {
            return self.render_thread_empty_state(model, cx).into_any_element();
        }

        self.thread.clone().into_any()
    }

    fn render_thread_empty_state(
        &self,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) -> impl IntoElement {
        let recent_threads = self
            .thread_store
            .update(cx, |this, model, cx| this.recent_threads(3, model, cx));

        v_flex()
            .gap_2()
            .mx_auto()
            .child(
                v_flex().w_full().child(
                    svg()
                        .path("icons/logo_96.svg")
                        .text_color(cx.theme().colors().text)
                        .w(px(40.))
                        .h(px(40.))
                        .mx_auto()
                        .mb_4(),
                ),
            )
            .child(v_flex())
            .child(
                h_flex()
                    .w_full()
                    .justify_center()
                    .child(Label::new("Context Examples:").size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_center()
                    .child(
                        h_flex()
                            .gap_1()
                            .p_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Icon::new(IconName::Terminal)
                                    .size(IconSize::Small)
                                    .color(Color::Disabled),
                            )
                            .child(Label::new("Terminal").size(LabelSize::Small)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .p_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Icon::new(IconName::Folder)
                                    .size(IconSize::Small)
                                    .color(Color::Disabled),
                            )
                            .child(Label::new("/src/components").size(LabelSize::Small)),
                    ),
            )
            .when(!recent_threads.is_empty(), |parent| {
                parent
                    .child(
                        h_flex()
                            .w_full()
                            .justify_center()
                            .child(Label::new("Recent Threads:").size(LabelSize::Small)),
                    )
                    .child(
                        v_flex().gap_2().children(
                            recent_threads
                                .into_iter()
                                .map(|thread| PastThread::new(thread, model.downgrade())),
                        ),
                    )
                    .child(
                        h_flex().w_full().justify_center().child(
                            Button::new("view-all-past-threads", "View All Past Threads")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .key_binding(KeyBinding::for_action_in(
                                    &OpenHistory,
                                    &self.focus_handle(cx),
                                    model,
                                    cx,
                                ))
                                .on_click(move |_event, cx| {
                                    cx.dispatch_action(OpenHistory.boxed_clone());
                                }),
                        ),
                    )
            })
    }

    fn render_last_error(&self, model: &Model<Self>, cx: &mut AppContext) -> Option<AnyElement> {
        let last_error = self.thread.read(cx).last_error()?;

        Some(
            div()
                .absolute()
                .right_3()
                .bottom_12()
                .max_w_96()
                .py_2()
                .px_3()
                .elevation_2(cx)
                .occlude()
                .child(match last_error {
                    ThreadError::PaymentRequired => self.render_payment_required_error(model, cx),
                    ThreadError::MaxMonthlySpendReached => {
                        self.render_max_monthly_spend_reached_error(model, cx)
                    }
                    ThreadError::Message(error_message) => {
                        self.render_error_message(&error_message, model, cx)
                    }
                })
                .into_any(),
        )
    }

    fn render_payment_required_error(
        &self,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) -> AnyElement {
        const ERROR_MESSAGE: &str = "Free tier exceeded. Subscribe and add payment to continue using Zed LLMs. You'll be billed at cost for tokens used.";

        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(Icon::new(IconName::XCircle).color(Color::Error))
                    .child(Label::new("Free Usage Exceeded").weight(FontWeight::MEDIUM)),
            )
            .child(
                div()
                    .id("error-message")
                    .max_h_24()
                    .overflow_y_scroll()
                    .child(Label::new(ERROR_MESSAGE)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .mt_1()
                    .child(Button::new("subscribe", "Subscribe").on_click(cx.listener(
                        |this, _, cx| {
                            this.thread.update(cx, |this, model, _cx| {
                                this.clear_last_error();
                            });

                            cx.open_url(&zed_urls::account_url(cx));
                            model.notify(cx);
                        },
                    )))
                    .child(Button::new("dismiss", "Dismiss").on_click(cx.listener(
                        |this, _, cx| {
                            this.thread.update(cx, |this, model, _cx| {
                                this.clear_last_error();
                            });

                            model.notify(cx);
                        },
                    ))),
            )
            .into_any()
    }

    fn render_max_monthly_spend_reached_error(
        &self,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) -> AnyElement {
        const ERROR_MESSAGE: &str = "You have reached your maximum monthly spend. Increase your spend limit to continue using Zed LLMs.";

        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(Icon::new(IconName::XCircle).color(Color::Error))
                    .child(Label::new("Max Monthly Spend Reached").weight(FontWeight::MEDIUM)),
            )
            .child(
                div()
                    .id("error-message")
                    .max_h_24()
                    .overflow_y_scroll()
                    .child(Label::new(ERROR_MESSAGE)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .mt_1()
                    .child(
                        Button::new("subscribe", "Update Monthly Spend Limit").on_click(
                            model.listener(|this, model, _, cx| {
                                this.thread.update(cx, |this, model, _cx| {
                                    this.clear_last_error();
                                });

                                cx.open_url(&zed_urls::account_url(cx));
                                model.notify(cx);
                            }),
                        ),
                    )
                    .child(Button::new("dismiss", "Dismiss").on_click(cx.listener(
                        |this, _, cx| {
                            this.thread.update(cx, |this, model, _cx| {
                                this.clear_last_error();
                            });

                            model.notify(cx);
                        },
                    ))),
            )
            .into_any()
    }

    fn render_error_message(
        &self,
        error_message: &SharedString,
        model: &Model<Self>,
        cx: &mut AppContext,
    ) -> AnyElement {
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(Icon::new(IconName::XCircle).color(Color::Error))
                    .child(
                        Label::new("Error interacting with language model")
                            .weight(FontWeight::MEDIUM),
                    ),
            )
            .child(
                div()
                    .id("error-message")
                    .max_h_32()
                    .overflow_y_scroll()
                    .child(Label::new(error_message.clone())),
            )
            .child(
                h_flex()
                    .justify_end()
                    .mt_1()
                    .child(Button::new("dismiss", "Dismiss").on_click(cx.listener(
                        |this, _, cx| {
                            this.thread.update(cx, |this, model, _cx| {
                                this.clear_last_error();
                            });

                            model.notify(cx);
                        },
                    ))),
            )
            .into_any()
    }
}

impl Render for AssistantPanel {
    fn render(
        &mut self,
        model: &Model<Self>,
        window: &mut gpui::Window,
        cx: &mut AppContext,
    ) -> impl IntoElement {
        v_flex()
            .key_context("AssistantPanel2")
            .justify_between()
            .size_full()
            .on_action(cx.listener(|this, _: &NewThread, cx| {
                this.new_thread(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenHistory, cx| {
                this.active_view = ActiveView::History;
                this.history.focus_handle(cx).focus(window);
                model.notify(cx);
            }))
            .child(self.render_toolbar(model, cx))
            .map(|parent| match self.active_view {
                ActiveView::Thread => parent
                    .child(self.render_active_thread_or_empty_state(model, cx))
                    .child(
                        h_flex()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(self.message_editor.clone()),
                    )
                    .children(self.render_last_error(model, cx)),
                ActiveView::History => parent.child(self.history.clone()),
            })
    }
}
