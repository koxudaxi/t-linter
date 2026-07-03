use serde_json::Value;

pub(crate) fn response_id(message: &Value) -> Option<u64> {
    if message.get("method").is_some() {
        return None;
    }
    message.get("id").and_then(Value::as_u64)
}

pub(crate) fn server_request_id(message: &Value) -> Option<Value> {
    message.get("method")?;
    message.get("id").cloned()
}

pub(crate) fn is_uv_pyproject_table(line: &str) -> bool {
    let Some(table) = toml_table_name(line) else {
        return false;
    };
    table == "dependency-groups" || table == "tool.uv" || table.starts_with("tool.uv.")
}

fn toml_table_name(line: &str) -> Option<&str> {
    let table = line.split('#').next()?.trim();
    let table = table.strip_prefix('[')?.strip_suffix(']')?.trim();
    let table = table.strip_prefix('[').unwrap_or(table);
    Some(table.strip_suffix(']').unwrap_or(table).trim())
}
