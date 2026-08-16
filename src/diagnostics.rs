use crate::lexer::Span;
use miette::{Diagnostic, SourceSpan};
use thiserror::Error;

#[derive(Error, Debug, Diagnostic)]
pub enum CompileError {
    #[error("unexpected token: expected {expected}, found {found}")]
    UnexpectedToken {
        expected: String,
        found: String,
        #[label("here")]
        span: SourceSpan,
    },

    #[error("unexpected end of file")]
    UnexpectedEof,

    #[error("mismatched closing name: expected `{expected}`, found `{found}`")]
    MismatchedClosingName {
        expected: String,
        found: String,
        #[label("closing name here")]
        span: SourceSpan,
    },

    #[error("mismatched closing kind: opened with `{opened}`, closed with `{closed}`")]
    MismatchedClosingKind {
        opened: String,
        closed: String,
        #[label("closing here")]
        span: SourceSpan,
    },

    #[error("lexer error: invalid token")]
    LexerError {
        #[label("here")]
        span: SourceSpan,
    },

    #[error("{message}")]
    General {
        message: String,
        #[label("here")]
        span: SourceSpan,
    },

    /// Construct that has a known-not-yet-supported analogue in HARC.
    /// Use this instead of a generic `unexpected token` so the user
    /// gets a one-line hint about the right shape.
    #[error("{message}")]
    #[diagnostic(help("{help}"))]
    UnsupportedSyntax {
        message: String,
        help: String,
        #[label("here")]
        span: SourceSpan,
    },
}

pub fn span_to_source_span(span: Span) -> SourceSpan {
    SourceSpan::new(
        span.start_usize().into(),
        (span.end_usize() - span.start_usize()).into(),
    )
}

impl CompileError {
    pub fn unexpected_token(expected: &str, found: &str, span: Span) -> Self {
        CompileError::UnexpectedToken {
            expected: expected.to_string(),
            found: found.to_string(),
            span: span_to_source_span(span),
        }
    }

    pub fn mismatched_closing(expected: &str, found: &str, span: Span) -> Self {
        CompileError::MismatchedClosingName {
            expected: expected.to_string(),
            found: found.to_string(),
            span: span_to_source_span(span),
        }
    }

    pub fn mismatched_kind(opened: &str, closed: &str, span: Span) -> Self {
        CompileError::MismatchedClosingKind {
            opened: opened.to_string(),
            closed: closed.to_string(),
            span: span_to_source_span(span),
        }
    }

    pub fn general(message: &str, span: Span) -> Self {
        CompileError::General {
            message: message.to_string(),
            span: span_to_source_span(span),
        }
    }

    pub fn unsupported_syntax(message: &str, help: &str, span: Span) -> Self {
        CompileError::UnsupportedSyntax {
            message: message.to_string(),
            help: help.to_string(),
            span: span_to_source_span(span),
        }
    }
}
