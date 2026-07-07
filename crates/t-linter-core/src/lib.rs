use anyhow::Result;

pub(crate) mod backend;
pub mod formatting;
pub mod highlighter;
pub mod lint;
pub mod parser;
pub mod project_config;
pub(crate) mod python;
pub mod shadow;
#[cfg(feature = "sql")]
pub(crate) mod sql;
pub(crate) mod tdom;

pub use formatting::{
    FormatError, FormatOptions, TemplateEdit, apply_diagnostic_edits, apply_template_edits,
    format_document, format_document_in_file, format_document_in_file_with_options,
    format_document_range, format_document_range_with_options, format_document_with_options,
};
pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use lint::{
    DiagnosticData, DiagnosticEdit, DiagnosticEditRange, LintDiagnostic, LintFileResult,
    LintRunSummary, LintSeverity, file_read_error, lint_source, lint_source_with_config,
};
pub use parser::{
    Expression, InterpolationInfo, LanguageDetection, Location, StaticTextSegment, TemplatePart,
    TemplateStringInfo, TemplateStringParser,
};
pub use project_config::{
    ProjectConfig, RuleSeverity, SqlConfig, find_config_root, load_project_config,
    load_project_config_for_path,
};
pub use shadow::{
    ShadowCheckSite, ShadowDocument, synthesize_for_type_check,
    synthesize_for_type_check_with_config,
};
#[cfg(feature = "sql")]
pub use sql::catalog::{
    CachedSqlCatalog, DEFAULT_SQL_DESCRIBE_TIMEOUT_SECONDS, DescribeEnvelope, DescribeRequest,
    DescribeResponse, SQL_DESCRIBE_TIMEOUT_ENV, SchemaProvider, SqlCatalogColumn, SqlCatalogError,
    SqlCatalogParam, SqlCatalogQuery, cache_path_for_query, cached_catalog_for_template,
    catalog_entry_from_response, catalog_query_for_template, read_cached_catalog,
    resolve_database_url, response_from_describe_envelope, sql_describe_timeout,
    write_cached_catalog,
};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_succeeds() {
        init().expect("core init");
    }
}
