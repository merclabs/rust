//! Basic syntax highlighting functionality.
//!
//! This module uses libsyntax's lexer to provide token-based highlighting for
//! the HTML documentation generated by rustdoc.
//!
//! Use the `render_with_highlighting` to highlight some rust code.

use crate::html::escape::Escape;

use std::fmt::Display;
use std::io;
use std::io::prelude::*;

use rustc_parse::lexer;
use syntax::sess::ParseSess;
use syntax::source_map::SourceMap;
use syntax::symbol::{kw, sym};
use syntax::token::{self, Token};
use syntax_pos::{FileName, Span};

/// Highlights `src`, returning the HTML output.
pub fn render_with_highlighting(
    src: &str,
    class: Option<&str>,
    extension: Option<&str>,
    tooltip: Option<(&str, &str)>,
) -> String {
    debug!("highlighting: ================\n{}\n==============", src);
    let mut out = Vec::new();
    if let Some((tooltip, class)) = tooltip {
        write!(
            out,
            "<div class='information'><div class='tooltip {}'>ⓘ<span \
                     class='tooltiptext'>{}</span></div></div>",
            class, tooltip
        )
        .unwrap();
    }

    let sess = ParseSess::with_silent_emitter();
    let fm = sess
        .source_map()
        .new_source_file(FileName::Custom(String::from("rustdoc-highlighting")), src.to_owned());
    let highlight_result = {
        let lexer = lexer::StringReader::new(&sess, fm, None);
        let mut classifier = Classifier::new(lexer, sess.source_map());

        let mut highlighted_source = vec![];
        if classifier.write_source(&mut highlighted_source).is_err() {
            Err(())
        } else {
            Ok(String::from_utf8_lossy(&highlighted_source).into_owned())
        }
    };

    match highlight_result {
        Ok(highlighted_source) => {
            write_header(class, &mut out).unwrap();
            write!(out, "{}", highlighted_source).unwrap();
            if let Some(extension) = extension {
                write!(out, "{}", extension).unwrap();
            }
            write_footer(&mut out).unwrap();
        }
        Err(()) => {
            // If errors are encountered while trying to highlight, just emit
            // the unhighlighted source.
            write!(out, "<pre><code>{}</code></pre>", src).unwrap();
        }
    }

    String::from_utf8_lossy(&out[..]).into_owned()
}

/// Processes a program (nested in the internal `lexer`), classifying strings of
/// text by highlighting category (`Class`). Calls out to a `Writer` to write
/// each span of text in sequence.
struct Classifier<'a> {
    lexer: lexer::StringReader<'a>,
    peek_token: Option<Token>,
    source_map: &'a SourceMap,

    // State of the classifier.
    in_attribute: bool,
    in_macro: bool,
    in_macro_nonterminal: bool,
}

/// How a span of text is classified. Mostly corresponds to token kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Class {
    None,
    Comment,
    DocComment,
    Attribute,
    KeyWord,
    // Keywords that do pointer/reference stuff.
    RefKeyWord,
    Self_,
    Op,
    Macro,
    MacroNonTerminal,
    String,
    Number,
    Bool,
    Ident,
    Lifetime,
    PreludeTy,
    PreludeVal,
    QuestionMark,
}

/// Trait that controls writing the output of syntax highlighting. Users should
/// implement this trait to customize writing output.
///
/// The classifier will call into the `Writer` implementation as it finds spans
/// of text to highlight. Exactly how that text should be highlighted is up to
/// the implementation.
trait Writer {
    /// Called when we start processing a span of text that should be highlighted.
    /// The `Class` argument specifies how it should be highlighted.
    fn enter_span(&mut self, _: Class) -> io::Result<()>;

    /// Called at the end of a span of highlighted text.
    fn exit_span(&mut self) -> io::Result<()>;

    /// Called for a span of text. If the text should be highlighted differently from the
    /// surrounding text, then the `Class` argument will be a value other than `None`.
    ///
    /// The following sequences of callbacks are equivalent:
    /// ```plain
    ///     enter_span(Foo), string("text", None), exit_span()
    ///     string("text", Foo)
    /// ```
    /// The latter can be thought of as a shorthand for the former, which is
    /// more flexible.
    fn string<T: Display>(&mut self, text: T, klass: Class) -> io::Result<()>;
}

// Implement `Writer` for anthing that can be written to, this just implements
// the default rustdoc behaviour.
impl<U: Write> Writer for U {
    fn string<T: Display>(&mut self, text: T, klass: Class) -> io::Result<()> {
        match klass {
            Class::None => write!(self, "{}", text),
            klass => write!(self, "<span class=\"{}\">{}</span>", klass.rustdoc_class(), text),
        }
    }

    fn enter_span(&mut self, klass: Class) -> io::Result<()> {
        write!(self, "<span class=\"{}\">", klass.rustdoc_class())
    }

    fn exit_span(&mut self) -> io::Result<()> {
        write!(self, "</span>")
    }
}

enum HighlightError {
    LexError,
    IoError(io::Error),
}

impl From<io::Error> for HighlightError {
    fn from(err: io::Error) -> Self {
        HighlightError::IoError(err)
    }
}

impl<'a> Classifier<'a> {
    fn new(lexer: lexer::StringReader<'a>, source_map: &'a SourceMap) -> Classifier<'a> {
        Classifier {
            lexer,
            peek_token: None,
            source_map,
            in_attribute: false,
            in_macro: false,
            in_macro_nonterminal: false,
        }
    }

    /// Gets the next token out of the lexer.
    fn try_next_token(&mut self) -> Result<Token, HighlightError> {
        if let Some(token) = self.peek_token.take() {
            return Ok(token);
        }
        let token = self.lexer.next_token();
        if let token::Unknown(..) = &token.kind {
            return Err(HighlightError::LexError);
        }
        Ok(token)
    }

    fn peek(&mut self) -> Result<&Token, HighlightError> {
        if self.peek_token.is_none() {
            let token = self.lexer.next_token();
            if let token::Unknown(..) = &token.kind {
                return Err(HighlightError::LexError);
            }
            self.peek_token = Some(token);
        }
        Ok(self.peek_token.as_ref().unwrap())
    }

    /// Exhausts the `lexer` writing the output into `out`.
    ///
    /// The general structure for this method is to iterate over each token,
    /// possibly giving it an HTML span with a class specifying what flavor of token
    /// is used. All source code emission is done as slices from the source map,
    /// not from the tokens themselves, in order to stay true to the original
    /// source.
    fn write_source<W: Writer>(&mut self, out: &mut W) -> Result<(), HighlightError> {
        loop {
            let next = self.try_next_token()?;
            if next == token::Eof {
                break;
            }

            self.write_token(out, next)?;
        }

        Ok(())
    }

    // Handles an individual token from the lexer.
    fn write_token<W: Writer>(&mut self, out: &mut W, token: Token) -> Result<(), HighlightError> {
        let klass = match token.kind {
            token::Shebang(s) => {
                out.string(Escape(&s.as_str()), Class::None)?;
                return Ok(());
            }

            token::Whitespace | token::Unknown(..) => Class::None,
            token::Comment => Class::Comment,
            token::DocComment(..) => Class::DocComment,

            // If this '&' or '*' token is followed by a non-whitespace token, assume that it's the
            // reference or dereference operator or a reference or pointer type, instead of the
            // bit-and or multiplication operator.
            token::BinOp(token::And) | token::BinOp(token::Star)
                if self.peek()? != &token::Whitespace =>
            {
                Class::RefKeyWord
            }

            // Consider this as part of a macro invocation if there was a
            // leading identifier.
            token::Not if self.in_macro => {
                self.in_macro = false;
                Class::Macro
            }

            // Operators.
            token::Eq
            | token::Lt
            | token::Le
            | token::EqEq
            | token::Ne
            | token::Ge
            | token::Gt
            | token::AndAnd
            | token::OrOr
            | token::Not
            | token::BinOp(..)
            | token::RArrow
            | token::BinOpEq(..)
            | token::FatArrow => Class::Op,

            // Miscellaneous, no highlighting.
            token::Dot
            | token::DotDot
            | token::DotDotDot
            | token::DotDotEq
            | token::Comma
            | token::Semi
            | token::Colon
            | token::ModSep
            | token::LArrow
            | token::OpenDelim(_)
            | token::CloseDelim(token::Brace)
            | token::CloseDelim(token::Paren)
            | token::CloseDelim(token::NoDelim) => Class::None,

            token::Question => Class::QuestionMark,

            token::Dollar => {
                if self.peek()?.is_ident() {
                    self.in_macro_nonterminal = true;
                    Class::MacroNonTerminal
                } else {
                    Class::None
                }
            }

            // This might be the start of an attribute. We're going to want to
            // continue highlighting it as an attribute until the ending ']' is
            // seen, so skip out early. Down below we terminate the attribute
            // span when we see the ']'.
            token::Pound => {
                // We can't be sure that our # begins an attribute (it could
                // just be appearing in a macro) until we read either `#![` or
                // `#[` from the input stream.
                //
                // We don't want to start highlighting as an attribute until
                // we're confident there is going to be a ] coming up, as
                // otherwise # tokens in macros highlight the rest of the input
                // as an attribute.

                // Case 1: #![inner_attribute]
                if self.peek()? == &token::Not {
                    self.try_next_token()?; // NOTE: consumes `!` token!
                    if self.peek()? == &token::OpenDelim(token::Bracket) {
                        self.in_attribute = true;
                        out.enter_span(Class::Attribute)?;
                    }
                    out.string("#", Class::None)?;
                    out.string("!", Class::None)?;
                    return Ok(());
                }

                // Case 2: #[outer_attribute]
                if self.peek()? == &token::OpenDelim(token::Bracket) {
                    self.in_attribute = true;
                    out.enter_span(Class::Attribute)?;
                }
                out.string("#", Class::None)?;
                return Ok(());
            }
            token::CloseDelim(token::Bracket) => {
                if self.in_attribute {
                    self.in_attribute = false;
                    out.string("]", Class::None)?;
                    out.exit_span()?;
                    return Ok(());
                } else {
                    Class::None
                }
            }

            token::Literal(lit) => {
                match lit.kind {
                    // Text literals.
                    token::Byte
                    | token::Char
                    | token::Err
                    | token::ByteStr
                    | token::ByteStrRaw(..)
                    | token::Str
                    | token::StrRaw(..) => Class::String,

                    // Number literals.
                    token::Integer | token::Float => Class::Number,

                    token::Bool => panic!("literal token contains `Lit::Bool`"),
                }
            }

            // Keywords are also included in the identifier set.
            token::Ident(name, is_raw) => match name {
                kw::Ref | kw::Mut if !is_raw => Class::RefKeyWord,

                kw::SelfLower | kw::SelfUpper => Class::Self_,
                kw::False | kw::True if !is_raw => Class::Bool,

                sym::Option | sym::Result => Class::PreludeTy,
                sym::Some | sym::None | sym::Ok | sym::Err => Class::PreludeVal,

                _ if token.is_reserved_ident() => Class::KeyWord,

                _ => {
                    if self.in_macro_nonterminal {
                        self.in_macro_nonterminal = false;
                        Class::MacroNonTerminal
                    } else if self.peek()? == &token::Not {
                        self.in_macro = true;
                        Class::Macro
                    } else {
                        Class::Ident
                    }
                }
            },

            token::Lifetime(..) => Class::Lifetime,

            token::Eof
            | token::Interpolated(..)
            | token::Tilde
            | token::At
            | token::SingleQuote => Class::None,
        };

        // Anything that didn't return above is the simple case where we the
        // class just spans a single token, so we can use the `string` method.
        out.string(Escape(&self.snip(token.span)), klass)?;

        Ok(())
    }

    // Helper function to get a snippet from the source_map.
    fn snip(&self, sp: Span) -> String {
        self.source_map.span_to_snippet(sp).unwrap()
    }
}

impl Class {
    /// Returns the css class expected by rustdoc for each `Class`.
    fn rustdoc_class(self) -> &'static str {
        match self {
            Class::None => "",
            Class::Comment => "comment",
            Class::DocComment => "doccomment",
            Class::Attribute => "attribute",
            Class::KeyWord => "kw",
            Class::RefKeyWord => "kw-2",
            Class::Self_ => "self",
            Class::Op => "op",
            Class::Macro => "macro",
            Class::MacroNonTerminal => "macro-nonterminal",
            Class::String => "string",
            Class::Number => "number",
            Class::Bool => "bool-val",
            Class::Ident => "ident",
            Class::Lifetime => "lifetime",
            Class::PreludeTy => "prelude-ty",
            Class::PreludeVal => "prelude-val",
            Class::QuestionMark => "question-mark",
        }
    }
}

fn write_header(class: Option<&str>, out: &mut dyn Write) -> io::Result<()> {
    write!(out, "<div class=\"example-wrap\"><pre class=\"rust {}\">\n", class.unwrap_or(""))
}

fn write_footer(out: &mut dyn Write) -> io::Result<()> {
    write!(out, "</pre></div>\n")
}
