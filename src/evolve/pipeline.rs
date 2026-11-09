//! Composable harness middleware: eight hooks, ordered processors, five outcomes
//! (HarnessX, arXiv:2606.14249). The pipeline is empty unless evolution is enabled, so a
//! default session keeps exactly the behaviour it had before this module existed.
//!
//! Processors are declared per hook and run in `Order` sequence; a processor may pass an event
//! through, transform it, split it into several events, intercept the chain, or interrupt the
//! turn. Duplicate singleton groups are rejected at registration time, which is what makes
//! "insert/replace/remove one processor without touching the others" safe.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Harness lifecycle points a processor can attach to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hook {
    TaskStart,
    StepStart,
    BeforeModel,
    AfterModel,
    BeforeTool,
    AfterTool,
    StepEnd,
    TaskEnd,
}

impl Hook {
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::TaskStart => "task_start",
            Hook::StepStart => "step_start",
            Hook::BeforeModel => "before_model",
            Hook::AfterModel => "after_model",
            Hook::BeforeTool => "before_tool",
            Hook::AfterTool => "after_tool",
            Hook::StepEnd => "step_end",
            Hook::TaskEnd => "task_end",
        }
    }

    pub fn all() -> [Hook; 8] {
        [Hook::TaskStart, Hook::StepStart, Hook::BeforeModel, Hook::AfterModel, Hook::BeforeTool, Hook::AfterTool, Hook::StepEnd, Hook::TaskEnd]
    }
}

/// Execution order inside one hook: gatekeepers first, observers last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Order {
    Pre,
    Normal,
    Post,
}

impl Order {
    pub fn as_str(self) -> &'static str {
        match self {
            Order::Pre => "pre",
            Order::Normal => "normal",
            Order::Post => "post",
        }
    }
}

/// What a processor did to the event stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Leave the event untouched.
    Pass,
    /// Replace the event with one or more events.
    Transform(Vec<Value>),
    /// Replace the event and stop the chain.
    Intercept(Vec<Value>),
    /// Stop the chain and refuse the action, emitting the given events first.
    Interrupt { reason: String, events: Vec<Value> },
}

/// Read-only state a processor may consult. Processors never mutate the harness directly.
#[derive(Clone, Debug, Default)]
pub struct PipelineContext {
    pub task: String,
    pub tool: Option<String>,
    pub tool_is_mutation: bool,
    pub verify_failed: bool,
    pub verify_retries: u64,
    pub attribution_present: bool,
    pub jit_enabled: bool,
    pub last_signal: Option<String>,
    pub step: u64,
}

/// A hook processor. Cheap and synchronous: it must never block the harness loop.
pub trait Processor: Send + Sync {
    fn name(&self) -> &'static str;
    fn hook(&self) -> Hook;
    fn order(&self) -> Order {
        Order::Normal
    }
    /// Singleton group: at most one processor of a group may be registered per hook.
    fn group(&self) -> Option<&'static str> {
        None
    }
    fn process(&self, event: Value, cx: &PipelineContext) -> Outcome;
}

/// Result of dispatching one hook: events to emit plus an optional refusal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dispatch {
    pub events: Vec<Value>,
    /// Safety-critical: the harness must not perform the action this event announced.
    pub interrupt: Option<String>,
    pub intercepted: bool,
}

impl Dispatch {
    pub fn allowed(&self) -> bool {
        self.interrupt.is_none()
    }
}

/// Ordered processor registry, one bucket per hook.
#[derive(Default)]
pub struct Pipeline {
    hooks: BTreeMap<Hook, Vec<Arc<dyn Processor>>>,
    enabled: bool,
}

impl Pipeline {
    pub fn disabled() -> Self {
        Self { hooks: BTreeMap::new(), enabled: false }
    }

    pub fn enabled() -> Self {
        Self { hooks: BTreeMap::new(), enabled: true }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.values().all(Vec::is_empty)
    }

    /// Registers a processor, keeping each hook sorted by `Order`.
    pub fn add(&mut self, processor: Arc<dyn Processor>) -> anyhow::Result<()> {
        let hook = processor.hook();
        let bucket = self.hooks.entry(hook).or_default();
        if let Some(group) = processor.group() {
            if bucket.iter().any(|existing| existing.group() == Some(group)) {
                anyhow::bail!("pipeline hook {} already has a processor in singleton group '{group}'", hook.as_str());
            }
        }
        bucket.push(processor);
        bucket.sort_by_key(|item| item.order());
        self.enabled = true;
        Ok(())
    }

    /// Replaces the single processor of a group, keeping the rest of the hook intact.
    pub fn replace(&mut self, processor: Arc<dyn Processor>) -> anyhow::Result<()> {
        let hook = processor.hook();
        let Some(group) = processor.group() else {
            return self.add(processor);
        };
        let bucket = self.hooks.entry(hook).or_default();
        bucket.retain(|existing| existing.group() != Some(group));
        bucket.push(processor);
        bucket.sort_by_key(|item| item.order());
        self.enabled = true;
        Ok(())
    }

    pub fn remove(&mut self, hook: Hook, name: &str) -> bool {
        let Some(bucket) = self.hooks.get_mut(&hook) else { return false };
        let before = bucket.len();
        bucket.retain(|processor| processor.name() != name);
        before != bucket.len()
    }

    pub fn names(&self, hook: Hook) -> Vec<&'static str> {
        self.hooks.get(&hook).map(|bucket| bucket.iter().map(|processor| processor.name()).collect()).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.hooks.values().map(Vec::len).sum()
    }

    /// Runs one hook. Disabled or empty pipelines return the event unchanged, so callers can
    /// dispatch unconditionally at every lifecycle point.
    pub fn dispatch(&self, hook: Hook, event: Value, cx: &PipelineContext) -> Dispatch {
        if !self.enabled {
            return Dispatch { events: vec![event], ..Dispatch::default() };
        }
        let Some(bucket) = self.hooks.get(&hook) else {
            return Dispatch { events: vec![event], ..Dispatch::default() };
        };
        let mut current = vec![event];
        for processor in bucket {
            let mut next: Vec<Value> = Vec::new();
            let mut intercepted = false;
            for item in current.drain(..) {
                match processor.process(item.clone(), cx) {
                    Outcome::Pass => next.push(item),
                    Outcome::Transform(events) => next.extend(events),
                    Outcome::Intercept(events) => {
                        next.extend(events);
                        intercepted = true;
                    }
                    Outcome::Interrupt { reason, events } => {
                        next.extend(events);
                        return Dispatch { events: next, interrupt: Some(reason), intercepted: true };
                    }
                }
            }
            current = next;
            if intercepted {
                return Dispatch { events: current, interrupt: None, intercepted: true };
            }
        }
        Dispatch { events: current, interrupt: None, intercepted: false }
    }
}

// ---------------------------------------------------------------------------------------------
// Built-in processors
// ---------------------------------------------------------------------------------------------

/// Attributes a verification failure before the model edits again (runtime-substrate P4:
/// attribution before recovery). Without the block the mutation is refused, not silently allowed.
pub struct AttributionGate {
    pub enabled: bool,
}

impl Processor for AttributionGate {
    fn name(&self) -> &'static str {
        "attribution_gate"
    }

    fn hook(&self) -> Hook {
        Hook::BeforeTool
    }

    fn order(&self) -> Order {
        Order::Pre
    }

    fn group(&self) -> Option<&'static str> {
        Some("safety")
    }

    fn process(&self, _event: Value, cx: &PipelineContext) -> Outcome {
        if !self.enabled || !cx.verify_failed || !cx.tool_is_mutation || cx.attribution_present {
            return Outcome::Pass;
        }
        // The refusal names the required shape so the model can satisfy it in one step.
        let reason = "Verification is failing: state <attribution>observed|expected|diagnosis|evidence</attribution> before editing again.";
        Outcome::Interrupt {
            reason: reason.to_string(),
            events: vec![json!({
                "type": "attribution_required",
                "tool": cx.tool.clone().unwrap_or_default(),
                "message": reason,
            })],
        }
    }
}

/// Emits a repair directive after an unstable step so the JIT layer can adjust the profile.
pub struct JitRepairDirective {
    pub enabled: bool,
}

impl Processor for JitRepairDirective {
    fn name(&self) -> &'static str {
        "jit_repair"
    }

    fn hook(&self) -> Hook {
        Hook::StepEnd
    }

    fn group(&self) -> Option<&'static str> {
        Some("jit")
    }

    fn process(&self, event: Value, cx: &PipelineContext) -> Outcome {
        if !self.enabled || !cx.jit_enabled {
            return Outcome::Pass;
        }
        let Some(signal) = cx.last_signal.as_deref() else { return Outcome::Pass };
        let unstable = matches!(signal, "verify_fail" | "tool_error" | "context_overflow" | "parse_error");
        if !unstable {
            return Outcome::Pass;
        }
        let mut out = event;
        if let Some(map) = out.as_object_mut() {
            map.insert("jit_repair_signal".into(), json!(signal));
        }
        Outcome::Transform(vec![out, json!({ "type": "jit_repair", "signal": signal })])
    }
}

/// Records every hook into the event stream when tracing is on (component/decision observability).
pub struct HookTracer {
    pub enabled: bool,
    pub hook: Hook,
}

impl Processor for HookTracer {
    fn name(&self) -> &'static str {
        "hook_tracer"
    }

    fn hook(&self) -> Hook {
        self.hook
    }

    fn order(&self) -> Order {
        Order::Post
    }

    fn process(&self, event: Value, cx: &PipelineContext) -> Outcome {
        if !self.enabled {
            return Outcome::Pass;
        }
        Outcome::Transform(vec![
            event,
            json!({
                "type": "harness_hook",
                "hook": self.hook.as_str(),
                "step": cx.step,
                "tool": cx.tool,
                "signal": cx.last_signal,
            }),
        ])
    }
}

/// Detects a missing `<attribution>` block in the most recent assistant text.
pub fn has_attribution(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let Some(start) = lower.find("<attribution>") else { return false };
    let Some(end) = lower[start..].find("</attribution>") else { return false };
    let body = &lower[start + "<attribution>".len()..start + end];
    body.contains("observed") && body.contains("expected") && body.contains("diagnosis")
}

/// The default pipeline for an evolved/bundled session: safety gate, JIT repair, optional trace.
/// Disabled layers are not registered at all, so `len()` equals the number of live processors.
pub fn default_pipeline(attribution: bool, jit: bool, trace: bool) -> anyhow::Result<Pipeline> {
    let mut pipeline = Pipeline::enabled();
    if attribution {
        pipeline.add(Arc::new(AttributionGate { enabled: true }))?;
    }
    if jit {
        pipeline.add(Arc::new(JitRepairDirective { enabled: true }))?;
    }
    if trace {
        for hook in Hook::all() {
            pipeline.add(Arc::new(HookTracer { enabled: true, hook }))?;
        }
    }
    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bumper {
        hook: Hook,
        order: Order,
        name: &'static str,
        group: &'static str,
    }

    impl Processor for Bumper {
        fn name(&self) -> &'static str {
            self.name
        }
        fn hook(&self) -> Hook {
            self.hook
        }
        fn order(&self) -> Order {
            self.order
        }
        fn group(&self) -> Option<&'static str> {
            Some(self.group)
        }
        fn process(&self, event: Value, _cx: &PipelineContext) -> Outcome {
            let mut out = event;
            if let Some(map) = out.as_object_mut() {
                let seen = map.get("seen").and_then(Value::as_u64).unwrap_or(0);
                map.insert("seen".into(), json!(seen + 1));
            }
            Outcome::Transform(vec![out])
        }
    }

    struct Blocker;

    impl Processor for Blocker {
        fn name(&self) -> &'static str {
            "blocker"
        }
        fn hook(&self) -> Hook {
            Hook::BeforeTool
        }
        fn process(&self, _event: Value, _cx: &PipelineContext) -> Outcome {
            Outcome::Interrupt { reason: "blocked".into(), events: vec![json!({"type": "blocked"})] }
        }
    }

    #[test]
    fn disabled_pipeline_passes_events_through() {
        let pipeline = Pipeline::disabled();
        let dispatch = pipeline.dispatch(Hook::BeforeModel, json!({"a": 1}), &PipelineContext::default());
        assert!(dispatch.allowed());
        assert_eq!(dispatch.events, vec![json!({"a": 1})]);
    }

    #[test]
    fn processors_run_in_order_and_noop_when_hook_empty() {
        let mut pipeline = Pipeline::enabled();
        pipeline.add(Arc::new(Bumper { hook: Hook::StepStart, order: Order::Post, name: "late", group: "late" })).unwrap();
        pipeline.add(Arc::new(Bumper { hook: Hook::StepStart, order: Order::Pre, name: "early", group: "early" })).unwrap();
        assert_eq!(pipeline.names(Hook::StepStart), vec!["early", "late"]);
        let dispatch = pipeline.dispatch(Hook::StepStart, json!({"seen": 0}), &PipelineContext::default());
        assert_eq!(dispatch.events, vec![json!({"seen": 2})]);
        let noop = pipeline.dispatch(Hook::TaskEnd, json!({"seen": 0}), &PipelineContext::default());
        assert_eq!(noop.events, vec![json!({"seen": 0})]);
    }

    #[test]
    fn singleton_groups_cannot_be_duplicated_but_can_be_replaced() {
        let mut pipeline = Pipeline::enabled();
        let first = || Arc::new(Bumper { hook: Hook::TaskStart, order: Order::Normal, name: "a", group: "g" }) as Arc<dyn Processor>;
        pipeline.add(first()).unwrap();
        let second = Arc::new(Bumper { hook: Hook::TaskStart, order: Order::Normal, name: "b", group: "g" }) as Arc<dyn Processor>;
        assert!(pipeline.add(second).is_err());
        let replacement = Arc::new(Bumper { hook: Hook::TaskStart, order: Order::Normal, name: "c", group: "g" }) as Arc<dyn Processor>;
        pipeline.replace(replacement).unwrap();
        assert_eq!(pipeline.names(Hook::TaskStart), vec!["c"]);
        assert!(pipeline.remove(Hook::TaskStart, "c"));
        assert!(pipeline.names(Hook::TaskStart).is_empty());
    }

    #[test]
    fn interrupt_stops_the_chain() {
        let mut pipeline = Pipeline::enabled();
        pipeline.add(Arc::new(Blocker)).unwrap();
        let dispatch = pipeline.dispatch(Hook::BeforeTool, json!({}), &PipelineContext::default());
        assert!(!dispatch.allowed());
        assert_eq!(dispatch.interrupt.as_deref(), Some("blocked"));
    }

    #[test]
    fn attribution_gate_requires_the_block_only_after_a_failed_verify() {
        let gate = AttributionGate { enabled: true };
        let failing = PipelineContext { verify_failed: true, tool_is_mutation: true, attribution_present: false, ..PipelineContext::default() };
        assert!(matches!(gate.process(json!({}), &failing), Outcome::Interrupt { .. }));
        let attributed = PipelineContext { attribution_present: true, ..failing.clone() };
        assert_eq!(gate.process(json!({}), &attributed), Outcome::Pass);
        let read_only_tool = PipelineContext { tool_is_mutation: false, ..failing.clone() };
        assert_eq!(gate.process(json!({}), &read_only_tool), Outcome::Pass);
        let healthy = PipelineContext { verify_failed: false, ..failing };
        assert_eq!(gate.process(json!({}), &healthy), Outcome::Pass);
    }

    #[test]
    fn attribution_block_is_parsed_structurally() {
        assert!(has_attribution("<attribution>observed: x expected: y diagnosis: z</attribution>"));
        assert!(!has_attribution("<attribution>observed: x</attribution>"));
        assert!(!has_attribution("no block here"));
        assert!(!has_attribution("<attribution>observed expected diagnosis"));
    }

    #[test]
    fn jit_directive_fires_only_on_unstable_signals() {
        let jit = JitRepairDirective { enabled: true };
        let unstable = PipelineContext { jit_enabled: true, last_signal: Some("verify_fail".into()), ..PipelineContext::default() };
        match jit.process(json!({"type": "step_end"}), &unstable) {
            Outcome::Transform(events) => {
                assert_eq!(events.len(), 2);
                assert_eq!(events[1]["type"], "jit_repair");
            }
            other => panic!("expected transform, got {other:?}"),
        }
        let stable = PipelineContext { jit_enabled: true, last_signal: Some("timeout".into()), ..PipelineContext::default() };
        assert_eq!(jit.process(json!({}), &stable), Outcome::Pass);
        let off = PipelineContext { jit_enabled: false, last_signal: Some("verify_fail".into()), ..PipelineContext::default() };
        assert_eq!(jit.process(json!({}), &off), Outcome::Pass);
    }

    #[test]
    fn default_pipeline_registers_the_documented_hooks() {
        let pipeline = default_pipeline(true, true, true).unwrap();
        // The tracer attaches to every hook, so each bucket is [builtin..., hook_tracer].
        assert_eq!(pipeline.names(Hook::BeforeTool), vec!["attribution_gate", "hook_tracer"]);
        assert_eq!(pipeline.names(Hook::StepEnd), vec!["jit_repair", "hook_tracer"]);
        assert_eq!(pipeline.names(Hook::TaskStart), vec!["hook_tracer"]);
        assert_eq!(pipeline.len(), 10);
        let untraced = default_pipeline(true, false, false).unwrap();
        assert_eq!(untraced.len(), 1);
    }
}
