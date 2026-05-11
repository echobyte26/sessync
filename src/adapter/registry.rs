//! Adapter registry — the single place where all known `ToolAdapter` implementations
//! are registered.  Commands that need to work across tools call `all_adapters()`.
//! Commands that accept a `--tool` flag call `adapter_by_name()`.
//!
//! To add a new tool (e.g. Codex in batch 2):
//!   1. Add the impl under `src/adapter/codex.rs`.
//!   2. Un-comment the `Box::new(codex::CodexAdapter::new())` line below.
//!   3. Export the module in `src/adapter/mod.rs`.

use super::tool::ToolAdapter;

/// All known `ToolAdapter` implementations.
///
/// Allocates on every call — that is intentional and acceptable at our scale
/// (one call per CLI invocation).  Each adapter holds only lightweight config
/// (a single `PathBuf`), so the allocation cost is negligible.
pub fn all_adapters() -> Vec<Box<dyn ToolAdapter>> {
    vec![
        Box::new(super::claude_code::ClaudeCodeAdapter::new()),
        // Box::new(super::codex::CodexAdapter::new()),  // batch 2
    ]
}

/// Look up a single adapter by its `name()` value (e.g. `"claude-code"`).
///
/// Returns `None` when the name is not recognised — the caller should surface
/// a `bail!("unknown tool …")` with `known_tool_names()` in the error message.
pub fn adapter_by_name(name: &str) -> Option<Box<dyn ToolAdapter>> {
    all_adapters().into_iter().find(|a| a.name() == name)
}

/// All registered tool names, in registration order.
///
/// Used in `--help` text and error messages so users know what values are valid.
pub fn known_tool_names() -> Vec<&'static str> {
    // We call all_adapters() and collect names.  The lifetime of the returned
    // `&'static str` slices is guaranteed by the `fn name() -> &'static str`
    // contract on `ToolAdapter`.
    all_adapters().iter().map(|a| a.name()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_by_name_returns_known_tools() {
        let adapter = adapter_by_name("claude-code");
        assert!(adapter.is_some(), "claude-code should be a known adapter");
        assert_eq!(adapter.unwrap().name(), "claude-code");
    }

    #[test]
    fn adapter_by_name_unknown_returns_none() {
        assert!(adapter_by_name("nope").is_none());
        assert!(adapter_by_name("codex").is_none());
        assert!(adapter_by_name("").is_none());
    }

    #[test]
    fn known_tool_names_contains_claude_code() {
        let names = known_tool_names();
        assert!(
            names.contains(&"claude-code"),
            "known_tool_names must include 'claude-code', got: {names:?}"
        );
    }

    #[test]
    fn all_adapters_returns_at_least_one() {
        assert!(!all_adapters().is_empty(), "registry must have at least one adapter");
    }
}
