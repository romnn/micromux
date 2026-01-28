use codespan_reporting::{
    diagnostic::{Diagnostic, Severity},
    files, term,
};
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Printer {
    writer: Arc<term::termcolor::StandardStream>,
    diagnostic_config: term::Config,
    files: Arc<RwLock<files::SimpleFiles<String, String>>>,
}

pub trait ToSourceName {
    fn to_source_name(self) -> String;
}

impl ToSourceName for String {
    fn to_source_name(self) -> String {
        self
    }
}

impl ToSourceName for &Path {
    fn to_source_name(self) -> String {
        self.to_string_lossy().to_string()
    }
}

impl ToSourceName for &PathBuf {
    fn to_source_name(self) -> String {
        self.as_path().to_source_name()
    }
}

impl Default for Printer {
    fn default() -> Self {
        Self::new(term::termcolor::ColorChoice::Auto)
    }
}

impl Printer {
    pub fn new(color_choice: term::termcolor::ColorChoice) -> Self {
        let writer = term::termcolor::StandardStream::stderr(color_choice);
        let diagnostic_config = term::Config::default();
        Self {
            writer: Arc::new(writer),
            diagnostic_config,
            files: Arc::new(RwLock::new(files::SimpleFiles::new())),
        }
    }

    pub async fn add_source_file(&self, name: impl ToSourceName, source: String) -> usize {
        let mut files = self.files.write();
        files.add(name.to_source_name(), source)
    }

    pub async fn emit(&self, diagnostic: &Diagnostic<usize>) -> Result<(), files::Error> {
        if diagnostic.severity == codespan_reporting::diagnostic::Severity::Error {
            tracing::error!("{:?}", diagnostic);
        } else {
            tracing::warn!("{:?}", diagnostic);
        };
        term::emit(
            &mut self.writer.lock(),
            &self.diagnostic_config,
            &*self.files.read(),
            diagnostic,
        )
    }
}

pub type FileId = usize;
pub type Span = std::ops::Range<usize>;

pub trait ToDiagnostics {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>>;
}

pub trait DiagnosticExt {
    fn is_error(&self) -> bool;
    fn is_warning(&self) -> bool;
    fn warning_or_error(strict: bool) -> Self;
}

impl<F> DiagnosticExt for Diagnostic<F> {
    fn is_error(&self) -> bool {
        match self.severity {
            Severity::Bug | Severity::Error => true,
            Severity::Warning | Severity::Note | Severity::Help => false,
        }
    }

    fn is_warning(&self) -> bool {
        match self.severity {
            Severity::Warning => true,
            Severity::Bug | Severity::Error | Severity::Note | Severity::Help => false,
        }
    }

    fn warning_or_error(strict: bool) -> Self {
        if strict {
            Self::error()
        } else {
            Self::warning()
        }
    }
}

pub struct DisplayRepr<'a, T>(pub &'a T);

impl<'a, T> std::fmt::Debug for DisplayRepr<'a, T>
where
    T: std::fmt::Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0, f)
    }
}

impl<'a, T> std::fmt::Display for DisplayRepr<'a, T>
where
    T: std::fmt::Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.0, f)
    }
}
