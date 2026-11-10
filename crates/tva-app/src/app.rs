//! gpui 界面：左侧实时转写，右侧执行动作。

use std::collections::VecDeque;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    ClickEvent, Context, FontWeight, MouseButton, Render, ScrollHandle, SharedString, Window, div,
    px, rgb,
};
use tva_agent::{Agent, AgentPreset};
use tva_core::{
    ActionId, AgentStage, AppEvent, AsrConfig, AsrPreset, AsrState, Config, EngineState, Route,
};
use tva_macos::AsrManager;

use crate::model::{ActionCard, ActionStatus, Phase, StepState, Turn, format_duration};
use crate::theme;

/// 电平波形保留的采样点数量。
const METER_BARS: usize = 56;
/// 单个动作卡片最多显示的行数。
const OUTPUT_TAIL_LINES: usize = 60;

pub struct TvaApp {
    // 语音状态
    state: EngineState,
    asr_state: AsrState,
    listening: bool,
    level: f32,
    levels: VecDeque<f32>,
    partial: String,

    // 内容
    turns: Vec<Turn>,
    actions: Vec<ActionCard>,
    /// 处理流水线的当前阶段。
    step: StepState,

    // 提示
    notice: Option<(String, bool)>,
    /// 等待用户确认的动作 (id, 命令)。
    pending_confirm: Option<(u64, String, String)>,
    backend: String,
    agent_cfg: tva_core::AgentConfig,
    hotkey_hint: String,
    config_path: String,
    max_actions: usize,

    // 句柄与配置
    asr: Arc<AsrManager>,
    asr_cfg: AsrConfig,
    config: Config,
    agent: Arc<Agent>,

    transcript_scroll: ScrollHandle,
    action_scroll: ScrollHandle,
}

impl TvaApp {
    pub fn new(
        config: Config,
        asr: Arc<AsrManager>,
        agent: Arc<Agent>,
        events: async_channel::Receiver<AppEvent>,
        cx: &mut Context<Self>,
    ) -> Self {
        let backend = agent.describe();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let alive = this.update(cx, |app: &mut Self, cx| {
                    app.on_event(event);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        // 有流程在跑时定期重绘，让步骤上的计时器动起来。
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(250)).await;
                let alive = this.update(cx, |app: &mut Self, cx| {
                    if app.step.active {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            state: EngineState::Idle,
            asr_state: AsrState::Idle,
            listening: false,
            level: 0.0,
            levels: VecDeque::with_capacity(METER_BARS),
            partial: String::new(),
            turns: Vec::new(),
            actions: Vec::new(),
            step: StepState::default(),
            notice: None,
            pending_confirm: None,
            backend,
            hotkey_hint: format!(
                "按住 {} 说话 · {} 开/关",
                config.hotkey.push_to_talk, config.hotkey.toggle
            ),
            config_path: Config::path().display().to_string(),
            max_actions: config.ui.max_actions.max(20),
            asr_cfg: config.asr.clone(),
            agent_cfg: config.agent.clone(),
            config,
            asr,
            agent,
            transcript_scroll: ScrollHandle::new(),
            action_scroll: ScrollHandle::new(),
        }
    }

    // ───────────────────────── 事件处理 ─────────────────────────

    fn on_event(&mut self, event: AppEvent) {
        log::debug!("事件: {event:?}");
        match event {
            AppEvent::Level { rms } => {
                self.level = rms;
                self.levels.push_back(rms);
                while self.levels.len() > METER_BARS {
                    self.levels.pop_front();
                }
            }
            AppEvent::AsrState(state) => {
                self.asr_state = state;
                self.listening = matches!(state, AsrState::Listening | AsrState::Recognizing);
                match state {
                    AsrState::Listening => {
                        // 只在"没有指令在跑"时开启新一轮，否则会把
                        // 上一句的步骤/耗时冲掉（识别器断句后还会再发一次 Listening）。
                        if !self.step.active
                            || matches!(self.step.phase, Phase::Record | Phase::Transcribe)
                        {
                            self.step.reset();
                            self.step.enter(Phase::Record, "等待说话");
                        }
                    }
                    AsrState::Recognizing => self.step.enter(Phase::Transcribe, "正在转写"),
                    AsrState::Idle => self.step.idle(),
                    AsrState::Failed => {
                        self.step.finish(false);
                    }
                    AsrState::Authorizing => {}
                }
                self.state = match state {
                    AsrState::Listening => EngineState::Listening,
                    AsrState::Recognizing => EngineState::Recognizing,
                    AsrState::Failed => EngineState::Error,
                    AsrState::Idle | AsrState::Authorizing => match self.state {
                        EngineState::Listening | EngineState::Recognizing => EngineState::Idle,
                        other => other,
                    },
                };
            }
            AppEvent::Partial { text } => {
                self.partial = text;
                self.step.enter(Phase::Transcribe, "正在转写");
            }
            AppEvent::Final { text } => {
                self.partial.clear();
                let text = text.trim().to_string();
                if text.is_empty() {
                    self.state = EngineState::Idle;
                    self.step.idle();
                } else {
                    self.step.enter(Phase::Route, "解析指令");
                    // 一句说完 -> 进入指令路由（本地动作或 Agent）
                    self.agent.handle_utterance(&text);
                }
            }
            AppEvent::Command { id, text, route } => {
                self.partial.clear();
                self.turns.push(Turn::new(id, text, route));
                self.state = match route {
                    Route::Agent => EngineState::Thinking,
                    Route::Local => EngineState::Acting,
                };
                match route {
                    Route::Agent => self.step.enter(Phase::Think, "已提交给 Agent"),
                    Route::Local => self.step.enter(Phase::Act, "执行本机动作"),
                }
                if self.turns.len() > 200 {
                    self.turns.remove(0);
                }
                self.transcript_scroll.scroll_to_bottom();
            }
            AppEvent::Stage { stage, .. } => {
                match stage {
                    AgentStage::Submitted => self.step.enter(Phase::Think, "已提交，等待模型"),
                    AgentStage::Started => self.step.enter(Phase::Think, "模型开始处理"),
                    AgentStage::Reasoning => self.step.enter(Phase::Think, "推理中"),
                    AgentStage::Responding => self.step.enter(Phase::Think, "生成回复"),
                    AgentStage::Tool { name } => self.step.enter(Phase::Act, name),
                    AgentStage::ToolDone { name } => {
                        self.step.enter(Phase::Think, format!("{name} 完成，继续思考"))
                    }
                    AgentStage::Settled => {}
                }
            }
            AppEvent::AgentDelta { id, text } => {
                if let Some(turn) = self.turn_mut(id) {
                    turn.reply.push_str(&text);
                }
                self.state = EngineState::Thinking;
                self.transcript_scroll.scroll_to_bottom();
            }
            AppEvent::AgentText { id, text } => {
                if let Some(turn) = self.turn_mut(id) {
                    turn.reply = text;
                }
                self.transcript_scroll.scroll_to_bottom();
            }
            AppEvent::ActionStart {
                id,
                action,
                name,
                detail,
            } => {
                if let Some(turn) = self.turn_mut(id) {
                    turn.actions.push(action);
                }
                self.actions.push(ActionCard::new(action, id, name, detail));
                while self.actions.len() > self.max_actions {
                    self.actions.remove(0);
                }
                self.state = EngineState::Acting;
                self.action_scroll.scroll_to_bottom();
            }
            AppEvent::ActionOutput { action, chunk, .. } => {
                if let Some(card) = self.action_mut(action) {
                    card.push_output(&chunk);
                }
                self.action_scroll.scroll_to_bottom();
            }
            AppEvent::ActionEnd {
                action,
                ok,
                summary,
                elapsed,
                ..
            } => {
                if let Some(card) = self.action_mut(action) {
                    card.status = if ok { ActionStatus::Ok } else { ActionStatus::Failed };
                    card.summary = summary;
                    card.elapsed = Some(elapsed);
                }
                self.action_scroll.scroll_to_bottom();
            }
            AppEvent::ConfirmRequest { id, text, detail } => {
                self.pending_confirm = Some((id, text, detail));
                self.state = EngineState::Acting;
            }
            AppEvent::CommandDone { id, ok } => {
                if self.pending_confirm.as_ref().is_some_and(|(pending, ..)| *pending == id) {
                    self.pending_confirm = None;
                }
                if let Some(turn) = self.turn_mut(id) {
                    turn.done = true;
                    turn.ok = ok;
                }
                self.state = if ok { EngineState::Idle } else { EngineState::Error };
                self.step.finish(ok);
            }
            AppEvent::Error { message } => {
                // 非致命提示：保留当前状态，只顶一条横幅。
                self.notice = Some((message, false));
            }
            AppEvent::AsrError { message } => {
                self.notice = Some((message, true));
                self.state = EngineState::Error;
                self.listening = false;
            }
        }
    }

    fn turn_mut(&mut self, id: u64) -> Option<&mut Turn> {
        self.turns.iter_mut().rev().find(|turn| turn.id == id)
    }

    fn action_mut(&mut self, id: ActionId) -> Option<&mut ActionCard> {
        self.actions.iter_mut().rev().find(|card| card.id == id)
    }

    // ───────────────────────── 交互 ─────────────────────────

    fn start_listening(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        self.asr.start();
        self.state = EngineState::Listening;
        cx.notify();
    }

    fn stop_listening(&mut self, cx: &mut Context<Self>) {
        self.asr.stop();
        self.state = EngineState::Idle;
        cx.notify();
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.agent.interrupt();
        self.state = EngineState::Idle;
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.turns.clear();
        self.actions.clear();
        self.partial.clear();
        self.notice = None;
        self.pending_confirm = None;
        cx.notify();
    }

    /// 切换语音识别引擎（界面上的一键切换）。
    fn switch_preset(&mut self, preset: AsrPreset, cx: &mut Context<Self>) {
        if preset.matches(&self.asr_cfg) {
            return;
        }
        if !preset.ready() {
            let missing: Vec<String> = preset
                .required_models()
                .into_iter()
                .filter(|path| !path.exists())
                .map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                .collect();
            self.notice = Some((
                format!(
                    "缺少模型 {}，先运行：./scripts/download-model.sh {}",
                    missing.join("、"),
                    match preset {
                        AsrPreset::WhisperTiny => "base",
                        AsrPreset::WhisperBase => "base",
                        _ => "small",
                    }
                ),
                true,
            ));
            cx.notify();
            return;
        }

        let mut cfg = self.asr_cfg.clone();
        preset.apply(&mut cfg);
        match self.asr.switch(cfg.clone()) {
            Ok(()) => {
                self.asr_cfg = cfg.clone();
                self.config.asr = cfg;
                if let Err(err) = self.config.save() {
                    log::warn!("保存配置失败: {err:#}");
                }
                self.partial.clear();
                self.notice = Some((format!("已切换到「{}」：{}", preset.label(), preset.hint()), false));
                log::info!("切换识别引擎: {:?}", preset);
            }
            Err(err) => {
                self.notice = Some((format!("切换失败: {err:#}"), true));
            }
        }
        cx.notify();
    }

    /// 处理流水线：从录音到完成，当前在哪一步。
    fn render_steps(&self) -> impl IntoElement {
        let step = &self.step;
        let dots = "·".repeat((step.started.elapsed().as_millis() / 400 % 4) as usize);

        let mut row: Vec<gpui::AnyElement> = Vec::new();
        for (index, phase) in Phase::ALL.iter().enumerate() {
            if index > 0 {
                row.push(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::FAINT))
                        .child("›")
                        .into_any_element(),
                );
            }

            let running = step.active && step.phase == *phase;
            let done = step.spent_on(*phase).is_some() && !running;
            let (fg, bg, border) = if running {
                (rgb(theme::TEXT), rgb(theme::ACCENT_SOFT), rgb(theme::ACCENT))
            } else if done {
                (rgb(theme::OK), rgb(theme::PANEL_ALT), rgb(theme::BORDER))
            } else if step.failed && *phase == Phase::Done {
                (rgb(theme::ERR), rgb(theme::PANEL_ALT), rgb(theme::ERR))
            } else {
                (rgb(theme::FAINT), rgb(theme::PANEL), rgb(theme::BORDER))
            };

            let suffix = if running {
                format!(" {dots} {}", format_duration(step.current_elapsed()))
            } else if done {
                format!(
                    " ✓ {}",
                    step.spent_on(*phase).map(format_duration).unwrap_or_default()
                )
            } else if step.failed && *phase == Phase::Done {
                " ✗".to_string()
            } else {
                String::new()
            };

            row.push(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(if running { rgb(theme::ACCENT) } else { fg })
                            .child(SharedString::from(match index {
                                0 => "①",
                                1 => "②",
                                2 => "③",
                                3 => "④",
                                4 => "⑤",
                                _ => "⑥",
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(fg)
                            .child(SharedString::from(phase.label())),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .text_color(if running { rgb(theme::ACCENT) } else { rgb(theme::FAINT) })
                            .child(SharedString::from(suffix)),
                    )
                    .into_any_element(),
            );
        }

        let detail = if step.active {
            format!(
                "当前：{} · 已用 {}",
                if step.detail.is_empty() { step.phase.label().to_string() } else { step.detail.clone() },
                format_duration(step.current_elapsed())
            )
        } else if step.failed {
            format!("当前：{}", step.detail)
        } else if !step.detail.is_empty() {
            format!("当前：{}", step.detail)
        } else {
            String::new()
        };

        div()
            .flex()
            .items_center()
            .gap_2()
            .pl(px(84.0))
            .pr_4()
            .py_2()
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .children(row)
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(if step.active { rgb(theme::ACCENT) } else if step.failed { rgb(theme::ERR) } else { rgb(theme::FAINT) })
                    .child(SharedString::from(detail)),
            )
    }

    /// 切换执行后端（pi / dsh / opencli / shell）。
    fn switch_backend(&mut self, preset: AgentPreset, cx: &mut Context<Self>) {
        if preset.matches(&self.agent_cfg) {
            return;
        }
        let mut cfg = self.agent_cfg.clone();
        preset.apply(&mut cfg);
        match self.agent.switch(cfg.clone()) {
            Ok(()) => {
                self.agent_cfg = cfg.clone();
                self.config.agent = cfg;
                self.backend = self.agent.describe();
                if let Err(err) = self.config.save() {
                    log::warn!("保存配置失败: {err:#}");
                }
                let (message, is_error) = match preset {
                    AgentPreset::Shell => (
                        "已切到 shell 模式：每句语音会被直接当成 shell 命令执行，请谨慎使用。"
                            .to_string(),
                        true,
                    ),
                    _ => (
                        format!("执行后端已切换到「{}」：{}", preset.label(), preset.hint()),
                        false,
                    ),
                };
                self.notice = Some((message, is_error));
                log::info!("切换执行后端: {}", preset.label());
            }
            Err(err) => {
                self.notice = Some((format!("切换后端失败: {err:#}"), true));
            }
        }
        cx.notify();
    }

    /// 工具栏：识别引擎 + 执行后端，两组一键切换。
    fn render_engine_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let engine_chips = AsrPreset::ALL
            .iter()
            .map(|preset| {
                let active = preset.matches(&self.asr_cfg);
                let available = preset.ready();
                chip(
                    format!("engine-{}", preset.label()),
                    preset.label(),
                    active,
                    available,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.switch_preset(*preset, cx);
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();

        let backend_chips = AgentPreset::ALL
            .iter()
            .map(|preset| {
                let active = preset.matches(&self.agent_cfg);
                chip(
                    format!("backend-{}", preset.label()),
                    preset.label(),
                    active,
                    true,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.switch_backend(*preset, cx);
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .items_center()
            .gap_2()
            .pl(px(84.0))
            .pr_4()
            .py_2()
            .bg(rgb(theme::BG))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(group_label("识别引擎"))
            .children(engine_chips)
            .child(div().w(px(12.0)))
            .child(group_label("执行后端"))
            .children(backend_chips)
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(theme::FAINT))
                    .whitespace_nowrap()
                    .child(SharedString::from(
                        if self.agent_cfg.backend == tva_core::Backend::Pi {
                            "pi：可看到工具调用细节".to_string()
                        } else {
                            "该后端一次性执行，只有最终答复".to_string()
                        },
                    )),
            )
    }

    fn confirm_pending(&mut self, ok: bool, cx: &mut Context<Self>) {
        if let Some((id, ..)) = self.pending_confirm.take() {
            if ok {
                self.agent.confirm(id);
            } else {
                self.agent.cancel(id);
            }
        }
        cx.notify();
    }

    /// 危险动作的确认条。
    fn render_confirm(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (id, text, detail) = self.pending_confirm.clone().unwrap_or((0, String::new(), String::new()));
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .bg(rgb(0x3a2f1d))
            .border_b_1()
            .border_color(rgb(theme::WARN))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(theme::WARN))
                            .child(SharedString::from(format!("需要确认后执行 #{id}：{text}"))),
                    )
                    .child(
                        div()
                            .font_family("Menlo")
                            .text_size(px(11.0))
                            .text_color(rgb(theme::TEXT))
                            .child(SharedString::from(detail)),
                    ),
            )
            .child(
                Self::button("执行", true, rgb(theme::OK)).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| this.confirm_pending(true, cx)),
                ),
            )
            .child(
                Self::button("取消", false, rgb(theme::ERR)).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| this.confirm_pending(false, cx)),
                ),
            )
    }

    // ───────────────────────── 渲染 ─────────────────────────

    fn status_color(&self) -> gpui::Rgba {
        match self.state {
            EngineState::Idle => rgb(theme::FAINT),
            EngineState::Listening => rgb(theme::ACCENT),
            EngineState::Recognizing => rgb(theme::OK),
            EngineState::Thinking => rgb(theme::WARN),
            EngineState::Acting => rgb(theme::WARN),
            EngineState::Error => rgb(theme::ERR),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .pl(px(84.0)) // 给 macOS 红绿灯按钮留位置
            .pr_4()
            .py_3()
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(8.0))
                            .rounded_full()
                            .bg(self.status_color()),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::TEXT))
                            .child(SharedString::from(self.state.label())),
                    ),
            )
            .child(self.render_meter())
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(theme::FAINT))
                    .child(SharedString::from(format!("{:.3}", self.level))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(theme::MUTED))
                    .child(SharedString::from(self.backend.clone())),
            )
            .child(self.render_talk_button(cx))
            .child(self.render_stop_button(cx))
            .child(self.render_clear_button(cx))
    }

    /// 电平波形：最近若干帧的 RMS。
    fn render_meter(&self) -> impl IntoElement {
        let bars = self
            .levels
            .iter()
            .map(|value| {
                let height = (3.0 + value * 180.0).clamp(3.0, 26.0);
                let color = if *value > 0.03 {
                    rgb(theme::OK)
                } else if *value > 0.008 {
                    rgb(theme::ACCENT)
                } else {
                    rgb(theme::BORDER)
                };
                div()
                    .w(px(2.0))
                    .h(px(height))
                    .rounded_full()
                    .bg(color)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .h(px(28.0))
            .children(bars)
    }

    fn button(
        label: &str,
        active: bool,
        accent: gpui::Rgba,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(label.to_string()))
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(if active { accent } else { rgb(theme::BORDER) })
            .bg(if active { rgb(theme::ACCENT_SOFT) } else { rgb(theme::PANEL_ALT) })
            .text_size(px(12.0))
            .text_color(if active { rgb(theme::TEXT) } else { rgb(theme::MUTED) })
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::ACCENT_SOFT)))
            .child(SharedString::from(label.to_string()))
    }

    fn render_talk_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::button("按住说话", self.listening, rgb(theme::ACCENT))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _window, cx| {
                    this.start_listening(cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _window, cx| {
                    this.stop_listening(cx);
                }),
            )
    }

    fn render_stop_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::button("打断", false, rgb(theme::ERR)).on_click(
            cx.listener(|this, _: &ClickEvent, _window, cx| this.interrupt(cx)),
        )
    }

    fn render_clear_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Self::button("清空", false, rgb(theme::BORDER)).on_click(
            cx.listener(|this, _: &ClickEvent, _window, cx| this.clear(cx)),
        )
    }

    fn section_title(text: &str, extra: Option<String>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .pt_3()
            .pb_2()
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::MUTED))
                    .child(SharedString::from(text.to_string())),
            )
            .child(div().flex_1())
            .children(extra.map(|extra| {
                div()
                    .text_size(px(10.5))
                    .text_color(rgb(theme::FAINT))
                    .child(SharedString::from(extra))
            }))
    }

    fn render_transcript(&self) -> impl IntoElement {
        let partial = if self.partial.is_empty() {
            match self.asr_state {
                AsrState::Authorizing => "正在申请麦克风与语音识别权限…".to_string(),
                AsrState::Listening => "在听，请说话…".to_string(),
                AsrState::Recognizing => "识别中…".to_string(),
                AsrState::Failed => "语音识别不可用".to_string(),
                AsrState::Idle => "点击「按住说话」或使用全局快捷键开始".to_string(),
            }
        } else {
            self.partial.clone()
        };
        let partial_color = if self.partial.is_empty() {
            rgb(theme::FAINT)
        } else {
            rgb(theme::TEXT)
        };

        let history = self
            .turns
            .iter()
            .rev()
            .take(60)
            .map(|turn| {
                let route_color = match turn.route {
                    Route::Agent => rgb(theme::ACCENT),
                    Route::Local => rgb(theme::OK),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded_sm()
                                    .bg(rgb(theme::PANEL_ALT))
                                    .text_size(px(9.5))
                                    .text_color(route_color)
                                    .child(SharedString::from(turn.route.label())),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(13.0))
                                    .text_color(rgb(theme::TEXT))
                                    .child(SharedString::from(turn.text.clone())),
                            )
                            .children(turn.done.then(|| {
                                div()
                                    .text_size(px(10.0))
                                    .text_color(if turn.ok { rgb(theme::OK) } else { rgb(theme::ERR) })
                                    .child(SharedString::from(if turn.ok { "✓" } else { "✗" }))
                            })),
                    )
                    .children((!turn.reply.is_empty()).then(|| {
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::MUTED))
                            .child(SharedString::from(turn.reply.clone()))
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .child(
                        div()
                            .text_size(px(20.0))
                            .line_height(px(28.0))
                            .text_color(partial_color)
                            .child(SharedString::from(partial)),
                    ),
            )
            .child(
                div()
                    .id("transcript-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.transcript_scroll)
                    .children(history),
            )
    }

    fn render_actions(&self) -> impl IntoElement {
        let cards = self
            .actions
            .iter()
            .rev()
            .take(40)
            .map(|card| {
                let (status_color, status_label) = match card.status {
                    ActionStatus::Running => (rgb(theme::WARN), "运行中"),
                    ActionStatus::Ok => (rgb(theme::OK), "完成"),
                    ActionStatus::Failed => (rgb(theme::ERR), "失败"),
                };
                let tail = card.tail_lines(OUTPUT_TAIL_LINES);
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .mx_3()
                    .mb_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::PANEL_ALT))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::ACCENT))
                                    .child(SharedString::from(card.name.clone())),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.0))
                                    .text_color(rgb(theme::MUTED))
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(SharedString::from(card.detail.clone())),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(theme::FAINT))
                                    .child(SharedString::from(format!("#{}", card.command))),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(status_color)
                                    .child(SharedString::from(match card.elapsed {
                                        Some(elapsed) => format!(
                                            "{status_label} · {}",
                                            format_duration(elapsed)
                                        ),
                                        None => status_label.to_string(),
                                    })),
                            ),
                    )
                    .children((!tail.is_empty()).then(|| {
                        div()
                            .mt_1()
                            .p_2()
                            .rounded_sm()
                            .bg(rgb(theme::BG))
                            .font_family("Menlo")
                            .text_size(px(11.0))
                            .line_height(px(15.0))
                            .text_color(rgb(theme::TEXT))
                            .child(SharedString::from(tail))
                    }))
                    .children((!card.summary.is_empty()).then(|| {
                        div()
                            .mt_1()
                            .text_size(px(10.5))
                            .text_color(if card.status == ActionStatus::Failed {
                                rgb(theme::ERR)
                            } else {
                                rgb(theme::FAINT)
                            })
                            .child(SharedString::from(card.summary.clone()))
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .id("action-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.action_scroll)
                    .children(cards),
            )
    }
}

/// 工具栏分组标题。
fn group_label(text: &str) -> gpui::Div {
    div()
        .text_size(px(10.5))
        .text_color(rgb(theme::FAINT))
        .child(SharedString::from(text.to_string()))
}

/// 可点击的切换芯片。
fn chip(id: String, label: &str, active: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .px_2()
        .py_1()
        .rounded_sm()
        .border_1()
        .border_color(if active {
            rgb(theme::ACCENT)
        } else {
            rgb(theme::BORDER)
        })
        .bg(if active {
            rgb(theme::ACCENT_SOFT)
        } else {
            rgb(theme::PANEL_ALT)
        })
        .text_size(px(10.5))
        .text_color(if active {
            rgb(theme::TEXT)
        } else if enabled {
            rgb(theme::MUTED)
        } else {
            rgb(theme::FAINT)
        })
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ACCENT_SOFT)))
        .child(SharedString::from(label.to_string()))
}

impl Render for TvaApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = self.notice.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT))
            .font_family(".AppleSystemUIFont")
            .child(self.render_header(cx))
            .child(self.render_engine_bar(cx))
            .child(self.render_steps())
            .children(notice.map(|(message, is_error)| {
                div()
                    .px_4()
                    .py_2()
                    .bg(if is_error { rgb(0x3a1d1d) } else { rgb(theme::PANEL) })
                    .text_size(px(11.5))
                    .text_color(if is_error { rgb(theme::ERR) } else { rgb(theme::WARN) })
                    .child(SharedString::from(message))
            }))
            .children(self.pending_confirm.is_some().then(|| self.render_confirm(cx)))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(380.0))
                            .h_full()
                            .border_r_1()
                            .border_color(rgb(theme::BORDER))
                            .child(Self::section_title("实时转写", None))
                            .child(self.render_transcript()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .child(Self::section_title(
                                "执行动作",
                                Some(format!("{} 条", self.actions.len())),
                            ))
                            .child(self.render_actions()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .bg(rgb(theme::PANEL))
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .text_size(px(10.5))
                    .text_color(rgb(theme::FAINT))
                    .child(SharedString::from(self.hotkey_hint.clone()))
                    .child(div().flex_1())
                    .child(SharedString::from(format!("配置 {}", self.config_path))),
            )
    }
}
