//! Stock Polaris startup with optional guard experiments.

fn main() {
    // Translate the original experiment's provider settings before starting any
    // worker threads. The stock host accepts their general equivalents.
    let legacy = "POLARIS_CODE_EXP_REASONING_EFFORT";
    let current = "POLARIS_CODE_REASONING_EFFORT";
    if let Some(value) = std::env::var_os(legacy) {
        let effort = value.to_str().and_then(legacy_effort);
        // SAFETY: this entry point has not created a runtime or any threads.
        unsafe {
            if let Some(effort) = effort
                && std::env::var_os(current).is_none()
            {
                std::env::set_var(current, effort);
            }
            std::env::remove_var(legacy);
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let code = runtime.block_on(polaris_guards::host::host().run(std::env::args_os()));
    std::process::exit(code);
}

fn legacy_effort(raw: &str) -> Option<&str> {
    match raw.trim() {
        value @ ("low" | "medium" | "high") => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_effort_preserves_the_original_whitespace_and_case_rules() {
        for (raw, expected) in [("low", "low"), (" medium ", "medium"), (" high ", "high")] {
            assert_eq!(super::legacy_effort(raw), Some(expected));
        }
        for raw in ["", "max", "HIGH"] {
            assert_eq!(super::legacy_effort(raw), None);
        }
    }
}
