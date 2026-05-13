//! The [`Tool`] trait — agent-callable capabilities.
//!
//! v0.1 shipped a single erased [`Tool`] trait using `serde_json::Value` for
//! args and `String` for output. v0.2 adds a typed variant [`TypedTool`] with
//! associated `Args`/`Output`/`Error` types, plus an [`ErasedAdapter`] that
//! implements the erased [`Tool`] trait on top of any `TypedTool`. Both
//! paths coexist — existing `Tool` impls keep working unchanged.
//!
//! See v0.2 plan doc Work Item A1.

use serde_json::Value;
use std::marker::PhantomData;

/// A tool the agent can invoke (erased form).
pub trait Tool: Send + Sync {
    /// The tool's name — used by the model to invoke it and for dispatch.
    fn name(&self) -> &str;

    /// Human-readable description sent to the model as part of the tool list.
    /// This is the primary steering mechanism for tool selection.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's arguments.
    fn schema(&self) -> Value;

    /// Execute the tool synchronously. Return a string result or an error
    /// message. Callers must enforce result-byte caps and envelope framing
    /// before persisting or feeding back to the model.
    fn call(&self, args: Value) -> Result<String, String>;
}

/// A typed tool — associated input/output/error types, schema as const.
///
/// Prefer this trait when writing new tools; wrap with [`ErasedAdapter`] to
/// register into a [`Registry`].
///
/// ```ignore
/// use agnt_core::tool::{TypedTool, ErasedAdapter, Registry};
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Deserialize)] struct Args { a: i64, b: i64 }
/// #[derive(Serialize)] struct Out { sum: i64 }
///
/// struct Add;
/// impl TypedTool for Add {
///     type Args = Args;
///     type Output = Out;
///     type Error = String;
///     const NAME: &'static str = "add";
///     const DESCRIPTION: &'static str = "Add two integers.";
///     fn schema() -> serde_json::Value {
///         serde_json::json!({
///             "type": "object",
///             "properties": {
///                 "a": { "type": "integer" },
///                 "b": { "type": "integer" }
///             },
///             "required": ["a", "b"]
///         })
///     }
///     fn call(&self, args: Args) -> Result<Out, String> {
///         Ok(Out { sum: args.a + args.b })
///     }
///  }
///
/// let mut reg = Registry::new();
/// reg.register(Box::new(ErasedAdapter::new(Add)));
/// ```
pub trait TypedTool: Send + Sync {
    /// Argument type, deserialized from JSON.
    type Args: serde::de::DeserializeOwned + Send;
    /// Return type, serialized to JSON.
    type Output: serde::Serialize + Send;
    /// Error type (displayed as a string when bridged to the erased trait).
    type Error: std::fmt::Display + Send + Sync;

    /// The tool name exposed to the model.
    const NAME: &'static str;
    /// Human-readable description for model steering.
    const DESCRIPTION: &'static str;

    /// JSON Schema for the arguments object.
    fn schema() -> Value;

    /// Execute the tool.
    fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error>;
}

/// Adapter that turns any [`TypedTool`] into an erased [`Tool`].
///
/// Deserializes the incoming `serde_json::Value` into `T::Args`, calls the
/// typed impl, and serializes the output back to a JSON string. Errors at any
/// stage are flattened to `Err(String)`.
pub struct ErasedAdapter<T: TypedTool> {
    inner: T,
    _marker: PhantomData<fn() -> T>,
}

impl<T: TypedTool> ErasedAdapter<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Access the underlying typed tool.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: TypedTool> Tool for ErasedAdapter<T> {
    fn name(&self) -> &str {
        T::NAME
    }

    fn description(&self) -> &str {
        T::DESCRIPTION
    }

    fn schema(&self) -> Value {
        T::schema()
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let typed: T::Args =
            serde_json::from_value(args).map_err(|e| format!("args deserialize: {}", e))?;
        let out = self.inner.call(typed).map_err(|e| e.to_string())?;
        serde_json::to_string(&out).map_err(|e| format!("output serialize: {}", e))
    }
}

/// A collection of tools with name-based dispatch.
///
/// The [`Agent`](crate::Agent) holds a `Registry` and uses it to dispatch
/// tool calls from the model. Tools can be registered at any time before
/// or between calls to [`Agent::step`](crate::Agent::step).
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Register a [`TypedTool`] directly, wrapping it in an [`ErasedAdapter`].
    pub fn register_typed<T: TypedTool + 'static>(&mut self, tool: T) {
        self.tools.push(Box::new(ErasedAdapter::new(tool)));
    }

    pub fn dispatch(&self, name: &str, args: Value) -> Result<String, String> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| format!("unknown tool: {}", name))?
            .call(args)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn as_openai_tools(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name(),
                            "description": t.description(),
                            "parameters": t.schema(),
                        }
                    })
                })
                .collect(),
        )
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// A lightweight proxy that forwards [`Tool`] calls to a shared [`Registry`].
///
/// Created by [`Registry::make_proxies`] so an `Arc<Registry>` can supply
/// tools to an [`Agent`] without requiring `Tool: Clone`.
struct RegistryProxy {
    registry: std::sync::Arc<Registry>,
    name: String,
    description: String,
    schema: Value,
}

impl Tool for RegistryProxy {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    fn call(&self, args: Value) -> Result<String, String> {
        self.registry.dispatch(&self.name, args)
    }
}

impl Registry {
    /// Return a `Vec` of proxy [`Tool`] objects that forward calls to the tools
    /// held in this `Arc<Registry>`.
    ///
    /// Use this to wire a shared registry into an [`Agent`]'s own tool
    /// registry without requiring `Tool: Clone`.
    pub fn make_proxies(self: &std::sync::Arc<Self>) -> Vec<Box<dyn Tool>> {
        self.tools
            .iter()
            .map(|t| {
                Box::new(RegistryProxy {
                    registry: std::sync::Arc::clone(self),
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    schema: t.schema(),
                }) as Box<dyn Tool>
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize)]
    struct AddArgs {
        a: i64,
        b: i64,
    }

    #[derive(Serialize)]
    struct AddOut {
        sum: i64,
    }

    struct Add;
    impl TypedTool for Add {
        type Args = AddArgs;
        type Output = AddOut;
        type Error = String;
        const NAME: &'static str = "add";
        const DESCRIPTION: &'static str = "Add two integers.";
        fn schema() -> Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "a": {"type": "integer"},
                    "b": {"type": "integer"}
                },
                "required": ["a", "b"]
            })
        }
        fn call(&self, args: AddArgs) -> Result<AddOut, String> {
            Ok(AddOut {
                sum: args.a + args.b,
            })
        }
    }

    #[test]
    fn typed_tool_roundtrips_through_erased_adapter() {
        let mut reg = Registry::new();
        reg.register_typed(Add);
        let out = reg
            .dispatch("add", serde_json::json!({"a": 2, "b": 3}))
            .expect("dispatch");
        assert_eq!(out, r#"{"sum":5}"#);
    }

    #[test]
    fn typed_tool_args_deserialize_error_is_string() {
        let mut reg = Registry::new();
        reg.register_typed(Add);
        let err = reg
            .dispatch("add", serde_json::json!({"a": "not-a-number"}))
            .unwrap_err();
        assert!(err.contains("args deserialize"), "got: {}", err);
    }

    #[test]
    fn erased_adapter_name_and_description_are_const() {
        let adapter = ErasedAdapter::new(Add);
        assert_eq!(adapter.name(), "add");
        assert_eq!(adapter.description(), "Add two integers.");
    }
}
