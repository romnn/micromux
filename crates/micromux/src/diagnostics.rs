//! Diagnostics helpers.
//!
//! This module provides a small wrapper around `codespan-reporting` for emitting diagnostics
//! anchored in input sources.

use codespan_reporting::{
    diagnostic::{Diagnostic, Severity},
    files, term,
};
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A diagnostics printer backed by [`codespan_reporting`].
#[derive(Debug, Clone)]
pub struct Printer {
    writer: Arc<term::termcolor::StandardStream>,
    diagnostic_config: term::Config,
    files: Arc<RwLock<files::SimpleFiles<String, String>>>,
}

/// Helper trait for turning various path-like values into a display name.
pub trait ToSourceName {
    /// Convert this value into a source name.
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
    /// Create a new diagnostics printer.
    ///
    /// The `color_choice` controls whether ANSI color codes are emitted.
    #[must_use]
    pub fn new(color_choice: term::termcolor::ColorChoice) -> Self {
        let writer = term::termcolor::StandardStream::stderr(color_choice);
        let diagnostic_config = term::Config::default();
        Self {
            writer: Arc::new(writer),
            diagnostic_config,
            files: Arc::new(RwLock::new(files::SimpleFiles::new())),
        }
    }

    /// Add a new source file to this printer and return its file id.
    pub fn add_source_file(&self, name: impl ToSourceName, source: String) -> usize {
        let mut files = self.files.write();
        files.add(name.to_source_name(), source)
    }

    /// Emit a diagnostic.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying `termcolor` writer fails.
    pub fn emit(&self, diagnostic: &Diagnostic<usize>) -> Result<(), files::Error> {
        if diagnostic.is_error() {
            tracing::error!("{:?}", diagnostic);
        } else if diagnostic.is_warning() {
            tracing::warn!("{:?}", diagnostic);
        } else {
            tracing::warn!("{:?}", diagnostic);
        }
        term::emit_to_write_style(
            &mut self.writer.lock(),
            &self.diagnostic_config,
            &*self.files.read(),
            diagnostic,
        )
    }
}

/// A file identifier returned by [`Printer::add_source_file`].
pub type FileId = usize;
/// A half-open character span.
pub type Span = std::ops::Range<usize>;

/// Convert an error type into codespan diagnostics.
pub trait ToDiagnostics {
    /// Convert this error value into diagnostics.
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>>;
}

/// Additional helper methods for codespan diagnostics.
pub trait DiagnosticExt {
    /// Returns `true` if this diagnostic is an error.
    fn is_error(&self) -> bool;
    /// Returns `true` if this diagnostic is a warning.
    fn is_warning(&self) -> bool;
    /// Create an error diagnostic if `strict` is enabled, otherwise create a warning diagnostic.
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
