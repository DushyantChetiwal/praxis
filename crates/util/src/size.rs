pub fn format_file_size(size: u64, use_decimal: bool) -> String {
    if use_decimal {
        if size < 1000 {
            format!("{size}B")
        } else if size < 1000 * 1000 {
            format!("{:.1}KB", size as f64 / 1000.0)
        } else {
            format!("{:.1}MB", size as f64 / (1000.0 * 1000.0))
        }
    } else if size < 1024 {
        format!("{size}B")
    } else if size < 1024 * 1024 {
        format!("{:.1}KiB", size as f64 / 1024.0)
    } else {
        format!("{:.1}MiB", size as f64 / (1024.0 * 1024.0))
    }
}

/// Roughly the heap a parsed JSON value holds, for memory diagnostics. Map
/// bookkeeping is approximated, so treat the result as an estimate.
pub fn estimated_json_heap_bytes(value: &serde_json::Value) -> usize {
    const VALUE_SIZE: usize = std::mem::size_of::<serde_json::Value>();
    const MAP_ENTRY_OVERHEAD: usize = std::mem::size_of::<String>() + VALUE_SIZE + 16;
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(text) => text.capacity(),
        serde_json::Value::Array(items) => {
            items.capacity() * VALUE_SIZE
                + items.iter().map(estimated_json_heap_bytes).sum::<usize>()
        }
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, item)| MAP_ENTRY_OVERHEAD + key.capacity() + estimated_json_heap_bytes(item))
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimated_json_heap_bytes_counts_nested_text() {
        let text = "x".repeat(1000);
        assert_eq!(estimated_json_heap_bytes(&serde_json::json!(42)), 0);
        assert!(estimated_json_heap_bytes(&serde_json::json!(text)) >= 1000);
        let nested = serde_json::json!({ "edits": [{ "new_text": text }] });
        assert!(estimated_json_heap_bytes(&nested) > 1000 + std::mem::size_of::<serde_json::Value>());
    }

    #[test]
    fn test_format_file_size_decimal() {
        assert_eq!(format_file_size(0, true), "0B");
        assert_eq!(format_file_size(999, true), "999B");
        assert_eq!(format_file_size(1000, true), "1.0KB");
        assert_eq!(format_file_size(1500, true), "1.5KB");
        assert_eq!(format_file_size(999999, true), "1000.0KB");
        assert_eq!(format_file_size(1000000, true), "1.0MB");
        assert_eq!(format_file_size(1500000, true), "1.5MB");
        assert_eq!(format_file_size(10000000, true), "10.0MB");
    }

    #[test]
    fn test_format_file_size_binary() {
        assert_eq!(format_file_size(0, false), "0B");
        assert_eq!(format_file_size(1023, false), "1023B");
        assert_eq!(format_file_size(1024, false), "1.0KiB");
        assert_eq!(format_file_size(1536, false), "1.5KiB");
        assert_eq!(format_file_size(1048575, false), "1024.0KiB");
        assert_eq!(format_file_size(1048576, false), "1.0MiB");
        assert_eq!(format_file_size(1572864, false), "1.5MiB");
        assert_eq!(format_file_size(10485760, false), "10.0MiB");
    }
}
