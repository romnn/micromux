use codespan_reporting::{
    diagnostic::{Diagnostic, Severity},
    files, term,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::diagnostics;

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

impl<'a> ToSourceName for &'a Path {
    fn to_source_name(self) -> String {
        self.to_string_lossy().to_string()
    }
}

impl<'a> ToSourceName for &'a PathBuf {
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
        let mut files = self.files.write().await;
        files.add(name.to_source_name(), source)
    }

    pub async fn emit(&self, diagnostic: &Diagnostic<usize>) -> Result<(), files::Error> {
        if diagnostic.severity == codespan_reporting::diagnostic::Severity::Error {
            tracing::error!("{:?}", diagnostic);
        } else {
            tracing::warn!("{:?}", diagnostic);
        };
        Ok(())
        // use term::views::{RichDiagnostic, ShortDiagnostic};
        //
        // let mut renderer = Renderer::new(writer, config);
        // match config.display_style {
        //     DisplayStyle::Rich => {
        //         RichDiagnostic::new(diagnostic, config).render(files, &mut renderer)
        //     }
        //     DisplayStyle::Medium => {
        //         ShortDiagnostic::new(diagnostic, true).render(files, &mut renderer)
        //     }
        //     DisplayStyle::Short => {
        //         ShortDiagnostic::new(diagnostic, false).render(files, &mut renderer)
        //     }
        // }

        // term::emit(
        //     &mut self.writer.lock(),
        //     &self.diagnostic_config,
        //     &*self.files.read().await,
        //     diagnostic,
        // )
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

// impl<'a, T> tracing::Value for DisplayRepr<'a, T> {
//     fn record(&self, key: &tracing::field::Field, visitor: &mut dyn tracing::field::Visit) {
//
//     }
// }

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

// #[derive(Debug, Clone)]
// pub struct Spanned<T> {
//     pub inner: T,
//     pub span: Span,
// }
//
// impl<T> serde::Serialize for Spanned<T>
// where
//     T: serde::Serialize,
// {
//     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
//     where
//         S: serde::Serializer,
//     {
//         self.inner.serialize(serializer)
//     }
// }
//
// impl<T> std::ops::Deref for Spanned<T> {
//     type Target = T;
//     fn deref(&self) -> &Self::Target {
//         &self.inner
//     }
// }
//
// impl<T> AsRef<T> for Spanned<T> {
//     fn as_ref(&self) -> &T {
//         &self.inner
//     }
// }
//
// impl<T> Spanned<T> {
//     pub fn new(span: impl Into<Span>, value: T) -> Self {
//         Self {
//             span: span.into(),
//             inner: value,
//         }
//     }
//
//     pub fn dummy(value: T) -> Self {
//         Self {
//             span: Span::default(),
//             inner: value,
//         }
//     }
//
//     pub fn into_inner(self) -> T {
//         self.inner
//     }
//
//     pub fn display(&self) -> DisplayRepr<'_, Self> {
//         DisplayRepr(self)
//     }
// }
//
// impl<T> std::fmt::Display for Spanned<T>
// where
//     T: std::fmt::Display,
// {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         std::fmt::Display::fmt(&self.inner, f)
//     }
// }
//
// impl<T> PartialEq for Spanned<T>
// where
//     T: PartialEq,
// {
//     fn eq(&self, other: &Self) -> bool {
//         self.inner == other.inner
//     }
// }
//
// impl<T> PartialEq<T> for Spanned<T>
// where
//     T: PartialEq,
// {
//     fn eq(&self, other: &T) -> bool {
//         (&self.inner as &dyn PartialEq<T>).eq(other)
//     }
// }
//
// impl<T> PartialEq<&T> for Spanned<T>
// where
//     T: PartialEq,
// {
//     fn eq(&self, other: &&T) -> bool {
//         (&self.inner as &dyn PartialEq<T>).eq(*other)
//     }
// }
//
// impl<T> Ord for Spanned<T>
// where
//     T: Ord,
// {
//     fn cmp(&self, other: &Self) -> std::cmp::Ordering {
//         Ord::cmp(&self.inner, &other.inner)
//     }
// }
//
// impl<T> PartialOrd for Spanned<T>
// where
//     T: PartialOrd,
// {
//     fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
//         PartialOrd::partial_cmp(&self.inner, &other.inner)
//     }
// }
//
// impl<T> PartialOrd<T> for Spanned<T>
// where
//     T: PartialOrd,
// {
//     fn partial_cmp(&self, other: &T) -> Option<std::cmp::Ordering> {
//         PartialOrd::partial_cmp(&self.inner, &other)
//     }
// }
//
// impl<T> Eq for Spanned<T> where T: Eq {}
//
// impl<T> std::hash::Hash for Spanned<T>
// where
//     T: std::hash::Hash,
// {
//     fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
//         self.inner.hash(state);
//     }
// }
