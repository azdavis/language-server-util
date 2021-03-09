//! Event-based parsers. Designed to be paired with libraries like `rowan`.
//!
//! To use this library:
//! 1. Define your own enum, perhaps called `SyntaxKind`, which includes all of
//!    the kinds of tokens and syntactic constructs found in your language,
//!    including 'trivia' like comments and whitespace.
//! 2. Implement [`Eq`], [`Copy`], and [`Triviable`] for this enum.
//! 3. Define a lexer which transforms an input string into a vector of
//!    contiguous [`Token`]s using this `SyntaxKind`.
//! 4. Define your language's grammar with functions operating on a [`Parser`].
//! 5. Call [`Parser::finish`] when done, and feed it a suitable [`Sink`] for
//!    the collected parsing events.
//!
//! A similar approach is used in [rust-analyzer][1].
//!
//! [1]: https://github.com/rust-analyzer/rust-analyzer

#![deny(missing_debug_implementations)]
#![deny(missing_docs)]
#![deny(rust_2018_idioms)]

use drop_bomb::DropBomb;

/// A event-based parser.
#[derive(Debug)]
pub struct Parser<'input, K> {
  tokens: &'input [Token<'input, K>],
  idx: usize,
  expected: Vec<K>,
  events: Vec<Option<Event<K>>>,
}

impl<'input, K> Parser<'input, K> {
  /// Returns a new parser for the given tokens.
  pub fn new(tokens: &'input [Token<'input, K>]) -> Self {
    Self {
      tokens,
      idx: 0,
      expected: Vec::new(),
      events: Vec::new(),
    }
  }

  /// Starts parsing a syntax construct.
  ///
  /// The returned [`Entered`] must eventually be passed to [`Self::exit`] or
  /// [`Self::abandon`]. If it is not, it will panic when dropped.
  ///
  /// `Entered`s returned from `enter` should be consumed with `exit` or
  /// `abandon` in a FIFO manner. That is, the first most recently created
  /// `Entered` should be the first one to be consumed. (Might be more like
  /// first-out first-in in this case actually.)
  ///
  /// If this invariant isn't upheld, as in e.g.
  ///
  /// ```ignore
  /// let e1 = p.enter();
  /// let e2 = p.enter();
  /// p.exit(k, e1);
  /// ```
  ///
  /// then Weird Things might happen.
  pub fn enter(&mut self) -> Entered {
    let idx = self.events.len();
    self.events.push(None);
    Entered {
      bomb: DropBomb::new("Entered markers must be exited"),
      idx,
    }
  }

  /// Abandons parsing a syntax construct.
  ///
  /// The events recorded since this syntax construct began, if any, will belong
  /// to the parent.
  pub fn abandon(&mut self, mut entered: Entered) {
    entered.bomb.defuse();
    assert!(self.events[entered.idx].is_none());
  }

  /// Finishes parsing a syntax construct.
  pub fn exit(&mut self, mut entered: Entered, kind: K) -> Exited {
    entered.bomb.defuse();
    let ev = &mut self.events[entered.idx];
    assert!(ev.is_none());
    *ev = Some(Event::Enter(kind, None));
    self.events.push(Some(Event::Exit));
    Exited { idx: entered.idx }
  }

  /// Starts parsing a syntax construct and makes it the parent of the given
  /// completed node.
  ///
  /// Consider an expression grammar `<expr> ::= <int> | <expr> + <expr>`. When
  /// we see an `<int>`, we enter and exit an `<expr>` node for it. But then
  /// we see the `+` and realize the completed `<expr>` node for the int should
  /// be the child of a node for the `+`. That's when this function comes in.
  pub fn precede(&mut self, exited: Exited) -> Entered {
    let ret = self.enter();
    match self.events[exited.idx] {
      Some(Event::Enter(_, ref mut parent)) => {
        assert!(parent.is_none());
        *parent = Some(ret.idx);
      }
      _ => unreachable!("{:?} did not precede an Enter", exited),
    }
    ret
  }

  /// Saves the state of the parser.
  ///
  /// This clears the set of expected tokens.
  ///
  /// Between when this `Save` is created and when it consumed (with either
  /// [`Self::restore`] or just by dropping it), there should be no calls to
  /// [`Self::exit`] or [`Self::precede`] with any [`Entered`] that were created
  /// before this `Save`, nor should there be any calls to [`Self::restore`]
  /// with any `Save` created before this `Save`.
  ///
  /// If there are, as in e.g.
  ///
  /// ```ignore
  /// let ent = p.enter();
  /// let s = p.save();
  /// p.exit(k, ent);
  /// p.restore(s);
  /// ```
  ///
  /// then Weird Things will happen, and the call to `restore` may not actually
  /// fully restore the state of the parser to whatever it was when saved.
  pub fn save(&mut self) -> Save<K> {
    Save {
      idx: self.idx,
      events_len: self.events.len(),
      expected: std::mem::take(&mut self.expected),
    }
  }

  /// Returns whether there has been an error since the save.
  pub fn error_since(&self, save: &Save<K>) -> bool {
    self
      .events
      .iter()
      .skip(save.events_len)
      .any(|ev| matches!(*ev, Some(Event::Error(..))))
  }

  /// Restores the saved state.
  pub fn restore(&mut self, save: Save<K>) {
    self.idx = save.idx;
    self.events.truncate(save.events_len);
    self.expected = save.expected;
  }
}

impl<'input, K> Parser<'input, K>
where
  K: Copy + Triviable,
{
  /// Returns the token after the "current" token, or `None` if the parser is
  /// out of tokens.
  ///
  /// Equivalent to `self.peek_n(0)`. See [`Self::peek_n`].
  pub fn peek(&mut self) -> Option<Token<'input, K>> {
    while let Some(&tok) = self.tokens.get(self.idx) {
      if tok.kind.is_trivia() {
        self.idx += 1;
      } else {
        return Some(tok);
      }
    }
    None
  }

  /// Returns the token `n` tokens in front of the current token, or `None` if
  /// there is no such token.
  ///
  /// The current token is the first token not yet consumed for which
  /// [`Triviable::is_trivia`] returns `true`; thus, if this returns
  /// `Some(tok)`, then `tok.kind.is_trivia()` is `false`.
  ///
  /// Note that it is not recommended to match on the `K` inside to e.g.
  /// determine what syntax construct to parse next. Using [`Self::at`] is
  /// better for this task since it keeps track of the `K`s that have been tried
  /// and will report them from [`Self::error`].
  pub fn peek_n(&mut self, n: usize) -> Option<Token<'input, K>> {
    let mut ret = self.peek();
    let idx = self.idx;
    for _ in 0..n {
      self.idx += 1;
      ret = self.peek();
    }
    self.idx = idx;
    ret
  }

  /// Consumes and returns the current token, and clears the set of expected
  /// tokens.
  ///
  /// Panics if there are no more tokens, i.e. if [`Self::peek`] would return
  /// `None` just prior to calling this.
  ///
  /// This is often used after calling [`Self::at`] to verify some expected
  /// token was present.
  pub fn bump(&mut self) -> Token<'input, K> {
    let ret = self.peek().expect("bump with no tokens");
    self.events.push(Some(Event::Token));
    self.idx += 1;
    self.expected.clear();
    ret
  }

  /// Records an error at the current token.
  pub fn error(&mut self) {
    self._error(None)
  }

  /// Records an error with a custom message at the current token.
  pub fn error_with(&mut self, message: String) {
    self._error(Some(message))
  }

  fn _error(&mut self, message: Option<String>) {
    let expected = std::mem::take(&mut self.expected);
    if self.peek().is_some() {
      self.bump();
    }
    self.events.push(Some(Event::Error(expected, message)));
  }

  fn eat_trivia(&mut self, sink: &mut dyn Sink<K>) {
    while let Some(&tok) = self.tokens.get(self.idx) {
      if !tok.kind.is_trivia() {
        break;
      }
      sink.token(tok);
      self.idx += 1;
    }
  }

  /// Finishes parsing, and writes the parsed tree into the `sink`.
  pub fn finish(mut self, sink: &mut dyn Sink<K>) {
    self.idx = 0;
    let mut kinds = Vec::new();
    let mut levels: usize = 0;
    for idx in 0..self.events.len() {
      let ev = match self.events[idx].take() {
        Some(ev) => ev,
        None => continue,
      };
      match ev {
        Event::Enter(kind, mut parent) => {
          assert!(kinds.is_empty());
          kinds.push(kind);
          while let Some(p) = parent {
            match self.events[p].take() {
              Some(Event::Enter(kind, new_parent)) => {
                kinds.push(kind);
                parent = new_parent;
              }
              _ => unreachable!("{:?} was not an Enter", parent),
            }
          }
          for kind in kinds.drain(..).rev() {
            // keep as much trivia as possible outside of what we're entering.
            if levels != 0 {
              self.eat_trivia(sink);
            }
            sink.enter(kind);
            levels += 1;
          }
        }
        Event::Exit => {
          sink.exit();
          levels -= 1;
          // keep as much trivia as possible outside of top-level items.
          if levels == 1 {
            self.eat_trivia(sink);
          }
        }
        Event::Token => {
          self.eat_trivia(sink);
          sink.token(self.tokens[self.idx]);
          self.idx += 1;
        }
        Event::Error(expected, message) => sink.error(expected, message),
      }
    }
    assert_eq!(levels, 0);
  }
}

impl<'input, K> Parser<'input, K>
where
  K: Copy + Triviable + Eq,
{
  /// Returns whether the current token has the given `kind`.
  ///
  /// Also records that `kind` was one of the expected kinds, to be used if
  /// [`Self::error`] is called later.
  pub fn at(&mut self, kind: K) -> bool {
    self.expected.push(kind);
    self.peek().map_or(false, |tok| tok.kind == kind)
  }

  /// If the current token's kind is `kind`, then this consumes it, else this
  /// errors. Returns the token if it was eaten.
  pub fn eat(&mut self, kind: K) -> Option<Token<'input, K>> {
    if self.at(kind) {
      Some(self.bump())
    } else {
      self.error();
      None
    }
  }
}

/// A marker for a syntax construct that is mid-parse. If this is not consumed
/// by a [`Parser`], it will panic when dropped.
#[derive(Debug)]
pub struct Entered {
  bomb: DropBomb,
  idx: usize,
}

/// A marker for a syntax construct that has been fully parsed.
#[derive(Debug)]
pub struct Exited {
  idx: usize,
}

/// The saved state of the parser.
#[derive(Debug)]
pub struct Save<K> {
  idx: usize,
  events_len: usize,
  expected: Vec<K>,
}

/// A token, a pair of kind and text.
#[derive(Debug, Clone, Copy)]
pub struct Token<'a, K> {
  /// The kind of token.
  pub kind: K,
  /// The text of the token.
  pub text: &'a str,
}

/// Types whose values can report whether they are trivia or not.
pub trait Triviable {
  /// Returns whether this is trivia.
  fn is_trivia(&self) -> bool;
}

/// Types which can construct a syntax tree.
pub trait Sink<K> {
  /// Enters a syntax construct with the given kind.
  fn enter(&mut self, kind: K);
  /// Adds a token to the given syntax construct.
  fn token(&mut self, token: Token<'_, K>);
  /// Exits a syntax construct.
  fn exit(&mut self);
  /// Reports an error.
  fn error(&mut self, expected: Vec<K>, message: Option<String>);
}

#[derive(Debug)]
enum Event<K> {
  Enter(K, Option<usize>),
  Token,
  Exit,
  Error(Vec<K>, Option<String>),
}

#[test]
fn event_size() {
  let ev = std::mem::size_of::<Event<()>>();
  let op_ev = std::mem::size_of::<Option<Event<()>>>();
  assert_eq!(ev, op_ev)
}
