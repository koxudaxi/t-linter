pub(crate) fn double_quoted_string_literal(value: &str) -> String {
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            _ => literal.push(ch),
        }
    }
    literal.push('"');
    literal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_quoted_string_literal_escapes_python_special_characters() {
        assert_eq!(
            double_quoted_string_literal("Literal[\"open\"]\\path\n"),
            "\"Literal[\\\"open\\\"]\\\\path\\n\""
        );
    }
}
