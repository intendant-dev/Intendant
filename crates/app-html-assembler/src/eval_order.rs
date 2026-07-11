//! Cross-fragment eval-order lint: the assembled dashboard is ONE
//! `<script type="module">` (manifest order is program order), so a
//! top-level reference in fragment M to a `let`/`const`/`class` binding
//! declared in a LATER fragment N throws a temporal-dead-zone
//! `ReferenceError` at module evaluation — and an uncaught top-level throw
//! kills every fragment after the throwing line while the page still
//! half-renders (the 2026-07-09 incident: fragment 49 reached a `let`
//! declared in fragment 58; listeners and `window.*` exposures from
//! fragments 50–58 silently never initialized). This lint fails assembly —
//! and therefore the build and the CI regen gate — when it can prove that
//! shape statically.
//!
//! ## What it catches, and the documented limitation
//!
//! The incident's *exact* shape — top-level code calling a function whose
//! body reads a later fragment's `let` — is not statically catchable in
//! general: deciding whether a function body runs during evaluation needs
//! whole-program call-graph and reachability analysis. Per the design
//! decision, the lint catches the **direct** form: an identifier referenced
//! in eval-time position (module top level, plus top-level control-flow
//! blocks, computed initializers, template interpolations, `extends`
//! clauses) whose `let`/`const`/`class` declaration lives in a later
//! fragment. References inside function / method / arrow / class bodies are
//! deferred and deliberately not flagged — calling such a function at top
//! level (including IIFEs, which this heuristic also treats as deferred) is
//! the residual hazard the runtime module-death canary covers
//! (`static/app/30-module-canary.js` + `static/app/59-module-alive.js`:
//! any module death paints a fatal banner within ~3s).
//!
//! ## Heuristic, not a JS engine
//!
//! The scanner is a pragmatic tokenizer: comments / strings / template
//! literals / regex literals are skipped, scopes are tracked by brace kind
//! (function-ish bodies defer, control-flow blocks and object literals do
//! not), and a handful of shapes are handled specially (arrow parameters,
//! object keys and method shorthand, ternary colons, catch parameters,
//! declarator lists, destructuring patterns). Deliberate softenings keep it
//! zero-false-positive on real fragment code at a small false-negative
//! cost:
//!
//! - any name *declared anywhere* in fragment M (any depth: `var`/`let`/
//!   `const`/`function`/`class`/params) suppresses flagging references to
//!   that name inside M — block-scoped shadowing without scope-exact
//!   tracking;
//! - identifiers directly before a `:` outside a ternary are treated as
//!   object keys / labels and skipped (loses `case X:` references);
//! - default-parameter expressions are treated as deferred (they evaluate
//!   at call time), and unbraced arrow bodies stay deferred until a `,`/`;`
//!   at their own depth (a missing semicolon extends that window).
//!
//! Fail-closed on its own blind spots: if the tokenizer finishes a fragment
//! with unbalanced scopes it errors out rather than silently under-linting.

use std::collections::{HashMap, HashSet};

/// Everything the lint needs to know about one JS fragment.
#[derive(Debug, Default)]
struct FragmentFacts {
    /// Names declared `let`/`const`/`class` at module top level (TDZ-prone
    /// in the shared module scope), with the 1-based declaration line.
    tdz_decls: Vec<(String, usize)>,
    /// Every name declared anywhere in the fragment, any depth. Used to
    /// suppress cross-fragment flagging of same-named references here.
    local_names: HashSet<String>,
    /// Identifier references in eval-time position: (name, 1-based line).
    eval_refs: Vec<(String, usize)>,
}

/// Lint the module's JS fragments, in assembly order, as the single shared
/// scope they become. `fragments` is `(display name, source)`.
pub(crate) fn check_eval_order(fragments: &[(String, String)]) -> Result<(), String> {
    let mut facts = Vec::with_capacity(fragments.len());
    for (name, source) in fragments {
        facts.push(scan_fragment(name, source)?);
    }

    // Map each TDZ-prone top-level name to its declaring fragment. A name
    // declared twice is itself fatal (redeclaration in one module scope is a
    // SyntaxError that kills the whole script), so report that too.
    let mut decl_map: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut violations: Vec<String> = Vec::new();
    for (idx, fact) in facts.iter().enumerate() {
        for (name, line) in &fact.tdz_decls {
            if let Some((prev_idx, prev_line)) = decl_map.get(name.as_str()) {
                violations.push(format!(
                    "duplicate top-level declaration `{name}`: {}:{prev_line} and {}:{line} both \
                     declare it — in the single assembled <script type=\"module\"> scope this is \
                     a SyntaxError that kills the entire dashboard script",
                    fragments[*prev_idx].0, fragments[idx].0,
                ));
            } else {
                decl_map.insert(name, (idx, *line));
            }
        }
    }

    for (idx, fact) in facts.iter().enumerate() {
        // One report per (fragment, identifier): the first reference line.
        let mut seen: HashSet<&str> = HashSet::new();
        for (name, line) in &fact.eval_refs {
            let Some(&(decl_idx, decl_line)) = decl_map.get(name.as_str()) else {
                continue;
            };
            if decl_idx <= idx || fact.local_names.contains(name) || !seen.insert(name) {
                continue;
            }
            violations.push(format!(
                "cross-fragment eval-order hazard (TDZ): {}:{line} references `{name}` at module \
                 eval time, but its declaration lives later in assembly order at {}:{decl_line} — \
                 at evaluation the binding is still in its temporal dead zone, the reference \
                 throws, and every fragment after the throw silently never initializes \
                 (manifest order is program order). Fix: declare the binding in or before the \
                 referencing fragment, or defer the reference into code that runs after module \
                 evaluation",
                fragments[idx].0, fragments[decl_idx].0,
            ));
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "eval-order lint failed ({} finding{}):\n  - {}",
            violations.len(),
            if violations.len() == 1 { "" } else { "s" },
            violations.join("\n  - "),
        ))
    }
}

// ── Tokenizer ────────────────────────────────────────────────────────────

/// What kind of construct a `(` belongs to; decides parameter binding and
/// how a `{` directly after the matching `)` is classified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParenKind {
    /// `if`/`for`/`while`/`switch`/`with` head — `) {` opens an eval block.
    ControlHead,
    /// `catch (e)` — binds parameters; `) {` opens an eval block.
    CatchHead,
    /// `function f(...)` — binds parameters (identifiers at its own level),
    /// defers default-value expressions, and `) {` opens a deferred body.
    FunctionParams,
    /// Grouping / call / (possibly) arrow params / method shorthand —
    /// `) {` opens a deferred body (`foo() {` is only valid as a body).
    Other,
}

#[derive(Clone, Copy, Debug)]
enum Scope {
    /// `deferred`: function/method/arrow/class body (does not run at eval).
    /// `literal`: object literal (tracks key positions via `expect_key`).
    Brace {
        deferred: bool,
        literal: bool,
        expect_key: bool,
    },
    Paren {
        kind: ParenKind,
        refs_start: usize,
    },
    Bracket,
    /// `${ … }` inside a template literal.
    Interp,
}

/// Previous significant token, tracked for regex-vs-division, `{`
/// classification, and property-access skipping.
#[derive(Clone, Debug, PartialEq)]
enum Prev {
    Start,
    Ident,
    Keyword(&'static str),
    /// Operators and separators; exact text kept for the few that matter,
    /// everything else is `"op"`.
    Punct(&'static str),
    ParenClose(ParenKind),
    BracketClose,
    BraceClose,
    Value, // number / string / template / regex literal
}

/// Hard keywords: never identifier references. Contextual words (`async`,
/// `of`, `get`, `set`, `static`, `undefined`, …) stay identifiers — a
/// "reference" to them is harmless because no fragment `let`-declares them,
/// and treating them as keywords would break their identifier uses.
const KEYWORDS: &[&str] = &[
    "var",
    "let",
    "const",
    "function",
    "class",
    "return",
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "new",
    "delete",
    "typeof",
    "instanceof",
    "void",
    "in",
    "this",
    "super",
    "null",
    "true",
    "false",
    "try",
    "catch",
    "finally",
    "throw",
    "await",
    "yield",
    "extends",
    "import",
    "export",
    "debugger",
    "with",
];

/// Keywords after which a `/` starts a regex literal (not division).
const REGEX_AFTER_KEYWORD: &[&str] = &[
    "return",
    "typeof",
    "case",
    "in",
    "instanceof",
    "new",
    "delete",
    "void",
    "do",
    "else",
    "throw",
    "await",
    "yield",
];

/// Declarator-list state for `var`/`let`/`const`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DeclMode {
    /// Right after the keyword or a top-depth `,`: expecting a binding name
    /// or a destructuring pattern.
    AwaitBinding,
    /// After a binding name: `=` starts the initializer, `,` the next
    /// declarator, `;`/`in`/`of` (or ASI) ends the declaration.
    AfterBinding,
    /// Inside an initializer expression (normal reference scanning).
    InInit,
    /// Inside a destructuring pattern: identifiers bind (keys over-collect
    /// harmlessly into the suppressor set) except in `=` default
    /// expressions, which scan as ordinary references.
    Pattern,
}

#[derive(Clone, Copy, Debug)]
struct DeclState {
    kind: &'static str, // "var" | "let" | "const"
    /// Scope-stack depth at the keyword; `,`/`;` only affect the declarator
    /// list at this depth.
    depth: usize,
    /// Declared at module top level (empty scope stack)?
    top_level: bool,
    mode: DeclMode,
    /// While in `Pattern`: stack depth where a `=` default expression
    /// started, if one is active (identifiers in it are refs, not bindings).
    pattern_default_depth: Option<usize>,
}

struct Scanner<'a> {
    fragment: &'a str,
    chars: Vec<char>,
    i: usize,
    line: usize,
    stack: Vec<Scope>,
    /// Saved ternary depth per scope entry (parallel to `stack`).
    ternary_stack: Vec<usize>,
    ternary: usize,
    prev: Prev,
    /// Identifier waiting for its following token to decide key/label/param
    /// vs. reference: (name, line, eval_position, at_object_key_position).
    pending: Option<(String, usize, bool, bool)>,
    /// Arrow-expression bodies (`=> expr` without braces): stack depths at
    /// which each began; refs while any is active are deferred.
    arrow_exprs: Vec<usize>,
    /// After `)` of a `ParenKind::Other` group: refs_start to truncate to if
    /// the very next token turns out to be `=>` (the group was arrow
    /// parameters, and its identifiers were bindings, not references).
    paren_close_refs: Option<usize>,
    /// `function` keyword seen: optional name slot, then `(` = params.
    fn_name_slot: bool,
    fn_params_next: bool,
    /// `class` keyword seen at this stack depth: name slot, and the next
    /// `{` at that depth is the (deferred) class body.
    class_pending: Option<usize>,
    class_name_slot: bool,
    decl: Option<DeclState>,
    facts: FragmentFacts,
}

fn scan_fragment(fragment: &str, source: &str) -> Result<FragmentFacts, String> {
    let mut s = Scanner {
        fragment,
        chars: source.chars().collect(),
        i: 0,
        line: 1,
        stack: Vec::new(),
        ternary_stack: Vec::new(),
        ternary: 0,
        prev: Prev::Start,
        pending: None,
        arrow_exprs: Vec::new(),
        paren_close_refs: None,
        fn_name_slot: false,
        fn_params_next: false,
        class_pending: None,
        class_name_slot: false,
        decl: None,
        facts: FragmentFacts::default(),
    };
    s.run()?;
    Ok(s.facts)
}

impl Scanner<'_> {
    fn run(&mut self) -> Result<(), String> {
        while self.i < self.chars.len() {
            let c = self.chars[self.i];
            match c {
                '\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                c if c.is_whitespace() => self.i += 1,
                '/' => {
                    if self.peek(1) == Some('/') {
                        self.skip_line_comment();
                    } else if self.peek(1) == Some('*') {
                        self.skip_block_comment()?;
                    } else if self.regex_allowed() {
                        self.skip_regex()?;
                        self.token_boundary(Prev::Value);
                    } else {
                        // Division (or /=): plain operator.
                        self.i += 1;
                        if self.peek(0) == Some('=') {
                            self.i += 1;
                        }
                        self.token_boundary(Prev::Punct("op"));
                    }
                }
                '\'' | '"' => {
                    self.skip_string(c)?;
                    self.token_boundary(Prev::Value);
                }
                '`' => {
                    // A pending identifier here is a tagged-template tag —
                    // a real read of the binding.
                    self.token_boundary(Prev::Punct("op"));
                    self.i += 1;
                    self.scan_template_text()?;
                }
                c if c == '$' || c == '_' || c.is_ascii_alphabetic() => self.scan_word(),
                c if c.is_ascii_digit() => {
                    while self
                        .peek(0)
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
                    {
                        self.i += 1;
                    }
                    self.token_boundary(Prev::Value);
                }
                '{' => self.open_brace(),
                '}' => self.close_brace()?,
                '(' => self.open_paren(),
                ')' => self.close_paren()?,
                '[' => {
                    self.token_boundary(Prev::Punct("["));
                    self.enter_pattern_if_awaiting_binding();
                    self.push_scope(Scope::Bracket);
                    self.i += 1;
                }
                ']' => {
                    self.token_boundary(Prev::BracketClose);
                    if !matches!(self.stack.last(), Some(Scope::Bracket)) {
                        return Err(self.unbalanced("']'"));
                    }
                    self.pop_scope();
                    self.i += 1;
                }
                ';' => {
                    self.token_boundary(Prev::Punct(";"));
                    let depth = self.stack.len();
                    self.arrow_exprs.retain(|d| *d != depth);
                    if let Some(decl) = self.decl {
                        if depth <= decl.depth {
                            self.decl = None;
                        }
                    }
                    self.i += 1;
                }
                ',' => {
                    self.token_boundary(Prev::Punct(","));
                    self.on_comma();
                    self.i += 1;
                }
                ':' => {
                    // token_boundary consults ternary depth for the pending
                    // identifier (object key / label vs ternary alternative).
                    self.token_boundary(Prev::Punct(":"));
                    self.ternary = self.ternary.saturating_sub(1);
                    self.i += 1;
                }
                '?' => match self.peek(1) {
                    Some('.') => {
                        self.token_boundary(Prev::Punct("?."));
                        self.i += 2;
                    }
                    Some('?') => {
                        self.token_boundary(Prev::Punct("op"));
                        self.i += 2;
                        if self.peek(0) == Some('=') {
                            self.i += 1;
                        }
                    }
                    _ => {
                        self.token_boundary(Prev::Punct("?"));
                        self.ternary += 1;
                        self.i += 1;
                    }
                },
                '.' => {
                    // `...` spread first: the following identifier is a ref.
                    if self.peek(1) == Some('.') && self.peek(2) == Some('.') {
                        self.token_boundary(Prev::Punct("op"));
                        self.i += 3;
                    } else {
                        self.token_boundary(Prev::Punct("."));
                        self.i += 1;
                    }
                }
                '=' => {
                    if self.peek(1) == Some('>') {
                        self.token_boundary(Prev::Punct("=>"));
                        self.i += 2;
                        self.on_arrow();
                    } else if self.peek(1) == Some('=') {
                        self.token_boundary(Prev::Punct("op"));
                        self.i += 2;
                        if self.peek(0) == Some('=') {
                            self.i += 1;
                        }
                    } else {
                        self.token_boundary(Prev::Punct("="));
                        self.on_assign();
                        self.i += 1;
                    }
                }
                _ => {
                    // Every other operator char (+ - * % ! < > & | ^ ~ #).
                    self.token_boundary(Prev::Punct("op"));
                    self.i += 1;
                }
            }
        }
        self.token_boundary(Prev::Start); // flush a trailing pending ident
        if !self.stack.is_empty() {
            return Err(self.unbalanced("end of fragment"));
        }
        Ok(())
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.i + ahead).copied()
    }

    fn unbalanced(&self, at: &str) -> String {
        format!(
            "eval-order lint could not lex {}: unbalanced scopes at {} (line {}) — either the \
             fragment has a syntax error or the lint's tokenizer needs a fix \
             (crates/app-html-assembler/src/eval_order.rs); refusing to under-lint",
            self.fragment, at, self.line
        )
    }

    // ── Significant-token boundary: resolve the pending identifier ──

    /// Called with the token that FOLLOWS the pending identifier (if any);
    /// decides whether that identifier was a reference, then records `next`
    /// as the new previous-token. Also expires the one-token window in
    /// which a closed `(...)` group may become arrow parameters.
    fn token_boundary(&mut self, next: Prev) {
        if !matches!(next, Prev::Punct("=>")) {
            self.paren_close_refs = None;
        }
        if let Some((name, line, eval, at_key)) = self.pending.take() {
            let is_ref = match &next {
                // Object key (`{ a: … }`) or label — not a reference. A
                // ternary alternative (`c ? x : y`) keeps the reference.
                Prev::Punct(":") if self.ternary == 0 => false,
                // Single arrow parameter: `x => …`.
                Prev::Punct("=>") => false,
                // Method shorthand name in an object literal: `foo() { … }`.
                Prev::Punct("(") if at_key => false,
                _ => true,
            };
            if is_ref && eval {
                self.facts.eval_refs.push((name, line));
            }
        }
        self.prev = next;
    }

    // ── Words: keywords, declarations, identifiers ──

    fn scan_word(&mut self) {
        let start = self.i;
        while self
            .peek(0)
            .is_some_and(|c| c == '$' || c == '_' || c.is_ascii_alphanumeric())
        {
            self.i += 1;
        }
        let word: String = self.chars[start..self.i].iter().collect();

        // Property access: `obj.name` / `obj?.name` — never a reference
        // (checked before keyword handling: `o.class` is a property).
        if matches!(self.prev, Prev::Punct(".") | Prev::Punct("?.")) {
            self.token_boundary(Prev::Ident);
            return;
        }

        // At an object-literal key position, even keyword-spelled words are
        // keys (`{ class: 'x' }`), and get/set/async/static prefixes keep
        // the slot armed for the real name (`get foo() {}`).
        if self.at_key_position() {
            if matches!(word.as_str(), "get" | "set" | "async" | "static") {
                // Prefix only when another word follows; `get: 1` and
                // `get() {}` treat `get` itself as the key/method name.
                let mut j = self.i;
                while self.chars.get(j).is_some_and(|c| c.is_whitespace()) {
                    j += 1;
                }
                if self
                    .chars
                    .get(j)
                    .is_some_and(|c| *c == '$' || *c == '_' || c.is_ascii_alphanumeric())
                {
                    self.token_boundary(Prev::Ident);
                    return; // key slot stays armed for the following name
                }
            }
            self.leave_key_position();
            self.token_boundary(Prev::Ident); // flush any stale pending
            self.pending = Some((word, self.line, self.eval_position(), true));
            return;
        }

        // Declarator machinery (any depth: bindings become suppressors;
        // top-level let/const bindings become TDZ declarations).
        if let Some(mut decl) = self.decl {
            match decl.mode {
                DeclMode::AwaitBinding => {
                    decl.mode = DeclMode::AfterBinding;
                    self.decl = Some(decl);
                    self.facts.local_names.insert(word.clone());
                    if decl.top_level && decl.kind != "var" {
                        self.facts.tdz_decls.push((word, self.line));
                    }
                    self.token_boundary(Prev::Ident);
                    return;
                }
                DeclMode::AfterBinding => {
                    if word == "of" || word == "in" {
                        // for-head: the declarator list ends; the `of`/`in`
                        // operand scans as ordinary references.
                        self.decl = None;
                        self.token_boundary(Prev::Keyword("in"));
                        return;
                    }
                    // ASI: `let x <newline> ident…` — declaration over;
                    // fall through and treat the word normally.
                    self.decl = None;
                }
                DeclMode::Pattern => {
                    if decl.pattern_default_depth.is_none() {
                        // Binding name (or key — over-collecting keys into
                        // the suppressor set is harmless) inside a pattern.
                        self.facts.local_names.insert(word);
                        self.token_boundary(Prev::Ident);
                        return;
                    }
                    // Inside a `= default` expression: ordinary references.
                }
                DeclMode::InInit => {} // ordinary reference scanning
            }
        }

        if let Some(kw) = KEYWORDS.iter().find(|k| **k == word.as_str()) {
            let kw: &'static str = kw;
            match kw {
                "var" | "let" | "const" => {
                    self.decl = Some(DeclState {
                        kind: kw,
                        depth: self.stack.len(),
                        top_level: self.stack.is_empty(),
                        mode: DeclMode::AwaitBinding,
                        pattern_default_depth: None,
                    });
                }
                "function" => {
                    self.fn_name_slot = true;
                    self.fn_params_next = true;
                }
                "class" => {
                    self.class_pending = Some(self.stack.len());
                    self.class_name_slot = true;
                }
                _ => {}
            }
            self.token_boundary(Prev::Keyword(kw));
            return;
        }

        // `function foo` — declaration name, not a reference.
        if self.fn_name_slot {
            self.fn_name_slot = false;
            self.facts.local_names.insert(word);
            self.token_boundary(Prev::Ident);
            return;
        }
        // `class Foo` — TDZ-prone at top level, suppressor otherwise.
        if self.class_name_slot {
            self.class_name_slot = false;
            self.facts.local_names.insert(word.clone());
            if self.stack.is_empty() {
                self.facts.tdz_decls.push((word, self.line));
            }
            self.token_boundary(Prev::Ident);
            return;
        }

        // Parameters bind, they don't reference (identifiers directly in a
        // function-params or catch-head group).
        if matches!(
            self.stack.last(),
            Some(Scope::Paren {
                kind: ParenKind::FunctionParams | ParenKind::CatchHead,
                ..
            })
        ) {
            self.facts.local_names.insert(word);
            self.token_boundary(Prev::Ident);
            return;
        }

        // An ordinary identifier: hold as pending until the next token
        // decides key/label/arrow-param vs reference.
        self.token_boundary(Prev::Ident); // flush any stale pending
        self.pending = Some((word, self.line, self.eval_position(), false));
    }

    /// True when no enclosing construct defers execution: an identifier (or
    /// expression) here runs at module evaluation. Function bodies, class
    /// bodies, arrow bodies (braced or expression) and default-parameter
    /// lists defer; control-flow blocks, object/array literals, call
    /// arguments and template interpolations do not.
    fn eval_position(&self) -> bool {
        self.arrow_exprs.is_empty()
            && !self.stack.iter().any(|s| {
                matches!(
                    s,
                    Scope::Brace { deferred: true, .. }
                        | Scope::Paren {
                            kind: ParenKind::FunctionParams,
                            ..
                        }
                )
            })
    }

    fn at_key_position(&self) -> bool {
        matches!(
            self.stack.last(),
            Some(Scope::Brace {
                literal: true,
                expect_key: true,
                ..
            })
        )
    }

    fn leave_key_position(&mut self) {
        if let Some(Scope::Brace {
            literal: true,
            expect_key,
            ..
        }) = self.stack.last_mut()
        {
            *expect_key = false;
        }
    }

    fn enter_pattern_if_awaiting_binding(&mut self) {
        if let Some(decl) = &mut self.decl {
            if decl.mode == DeclMode::AwaitBinding && self.stack.len() == decl.depth {
                decl.mode = DeclMode::Pattern;
            }
        }
    }

    // ── Scopes ──

    fn push_scope(&mut self, scope: Scope) {
        self.stack.push(scope);
        self.ternary_stack.push(self.ternary);
        self.ternary = 0;
    }

    fn pop_scope(&mut self) {
        self.stack.pop();
        self.ternary = self.ternary_stack.pop().unwrap_or(0);
        let depth = self.stack.len();
        // Arrow-expression bodies cannot outlive the scope they started in.
        self.arrow_exprs.retain(|d| *d <= depth);
        if let Some(decl) = &mut self.decl {
            if depth < decl.depth {
                // The scope holding the declaration closed (for-heads).
                self.decl = None;
            } else if decl.mode == DeclMode::Pattern && depth == decl.depth {
                // The destructuring pattern just closed.
                decl.mode = DeclMode::AfterBinding;
                decl.pattern_default_depth = None;
            }
        }
    }

    fn open_brace(&mut self) {
        let deferred_body = matches!(self.prev, Prev::Punct("=>"))
            || matches!(
                self.prev,
                Prev::ParenClose(ParenKind::FunctionParams) | Prev::ParenClose(ParenKind::Other)
            )
            || self.class_pending == Some(self.stack.len());
        if self.class_pending == Some(self.stack.len()) {
            self.class_pending = None;
        }
        let literal = !deferred_body
            && matches!(
                self.prev,
                Prev::Punct("=")
                    | Prev::Punct("(")
                    | Prev::Punct("[")
                    | Prev::Punct(",")
                    | Prev::Punct(":")
                    | Prev::Punct("?")
                    | Prev::Punct("op")
                    | Prev::Keyword("return")
                    | Prev::Keyword("typeof")
                    | Prev::Keyword("in")
                    | Prev::Keyword("case")
                    | Prev::Keyword("throw")
                    | Prev::Keyword("await")
                    | Prev::Keyword("yield")
            );
        self.token_boundary(Prev::Punct("{"));
        self.enter_pattern_if_awaiting_binding();
        self.push_scope(Scope::Brace {
            deferred: deferred_body,
            literal,
            expect_key: literal,
        });
        self.i += 1;
    }

    fn close_brace(&mut self) -> Result<(), String> {
        self.token_boundary(Prev::BraceClose);
        match self.stack.last() {
            Some(Scope::Brace { .. }) => {
                self.pop_scope();
                self.i += 1;
                Ok(())
            }
            Some(Scope::Interp) => {
                self.pop_scope();
                self.i += 1;
                self.scan_template_text()
            }
            _ => Err(self.unbalanced("'}'")),
        }
    }

    fn open_paren(&mut self) {
        let kind = if self.fn_params_next
            && matches!(
                self.prev,
                Prev::Keyword("function") | Prev::Ident | Prev::Punct("op")
            ) {
            // `function (`, `function foo (`, `function* (` — the "op"
            // branch is the generator star.
            self.fn_params_next = false;
            self.fn_name_slot = false;
            ParenKind::FunctionParams
        } else {
            match self.prev {
                Prev::Keyword("if")
                | Prev::Keyword("for")
                | Prev::Keyword("while")
                | Prev::Keyword("switch")
                | Prev::Keyword("with") => ParenKind::ControlHead,
                Prev::Keyword("catch") => ParenKind::CatchHead,
                _ => ParenKind::Other,
            }
        };
        self.token_boundary(Prev::Punct("("));
        let refs_start = self.facts.eval_refs.len();
        self.push_scope(Scope::Paren { kind, refs_start });
        self.i += 1;
    }

    fn close_paren(&mut self) -> Result<(), String> {
        // Resolve the pending identifier before popping so ternary
        // bookkeeping stays scoped to the group.
        self.token_boundary(Prev::Start);
        let Some(Scope::Paren { kind, refs_start }) = self.stack.last().copied() else {
            return Err(self.unbalanced("')'"));
        };
        self.pop_scope();
        self.prev = Prev::ParenClose(kind);
        // If `=>` follows immediately, this group was arrow parameters: the
        // identifiers recorded inside were bindings, not references.
        if kind == ParenKind::Other {
            self.paren_close_refs = Some(refs_start);
        }
        self.i += 1;
        Ok(())
    }

    fn on_arrow(&mut self) {
        if let Some(refs_start) = self.paren_close_refs.take() {
            self.facts.eval_refs.truncate(refs_start);
        }
        // `=> expr` without braces: defer references until the expression
        // ends (`,`/`;` at this depth, or the enclosing scope closes).
        let mut j = self.i;
        while self.chars.get(j).is_some_and(|c| c.is_whitespace()) {
            j += 1;
        }
        if self.chars.get(j) != Some(&'{') {
            self.arrow_exprs.push(self.stack.len());
        }
    }

    fn on_assign(&mut self) {
        if let Some(decl) = &mut self.decl {
            match decl.mode {
                DeclMode::AfterBinding if self.stack.len() == decl.depth => {
                    decl.mode = DeclMode::InInit;
                }
                DeclMode::Pattern if decl.pattern_default_depth.is_none() => {
                    decl.pattern_default_depth = Some(self.stack.len());
                }
                _ => {}
            }
        }
    }

    fn on_comma(&mut self) {
        let depth = self.stack.len();
        self.arrow_exprs.retain(|d| *d != depth);
        if let Some(decl) = &mut self.decl {
            if depth == decl.depth && matches!(decl.mode, DeclMode::AfterBinding | DeclMode::InInit)
            {
                decl.mode = DeclMode::AwaitBinding;
            }
            if decl.mode == DeclMode::Pattern && decl.pattern_default_depth == Some(depth) {
                decl.pattern_default_depth = None;
            }
        }
        // Object literal: the next slot is a key again.
        if let Some(Scope::Brace {
            literal: true,
            expect_key,
            ..
        }) = self.stack.last_mut()
        {
            *expect_key = true;
        }
    }

    // ── Literals and comments ──

    fn skip_line_comment(&mut self) {
        while self.peek(0).is_some_and(|c| c != '\n') {
            self.i += 1;
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), String> {
        self.i += 2;
        loop {
            match self.peek(0) {
                None => return Err(self.unbalanced("unterminated block comment")),
                Some('*') if self.peek(1) == Some('/') => {
                    self.i += 2;
                    return Ok(());
                }
                Some('\n') => {
                    self.line += 1;
                    self.i += 1;
                }
                Some(_) => self.i += 1,
            }
        }
    }

    fn skip_string(&mut self, quote: char) -> Result<(), String> {
        self.i += 1;
        loop {
            match self.peek(0) {
                None => return Err(self.unbalanced("unterminated string")),
                Some('\\') => {
                    if self.peek(1) == Some('\n') {
                        self.line += 1;
                    }
                    self.i += 2;
                }
                Some(c) if c == quote => {
                    self.i += 1;
                    return Ok(());
                }
                Some('\n') => {
                    // Tolerate an (invalid) raw newline rather than derail.
                    self.line += 1;
                    self.i += 1;
                }
                Some(_) => self.i += 1,
            }
        }
    }

    /// Scan template-literal text until the closing backtick or a `${`,
    /// which pushes a [`Scope::Interp`] and returns to code scanning.
    fn scan_template_text(&mut self) -> Result<(), String> {
        loop {
            match self.peek(0) {
                None => return Err(self.unbalanced("unterminated template literal")),
                Some('\\') => {
                    if self.peek(1) == Some('\n') {
                        self.line += 1;
                    }
                    self.i += 2;
                }
                Some('`') => {
                    self.i += 1;
                    self.prev = Prev::Value;
                    return Ok(());
                }
                Some('$') if self.peek(1) == Some('{') => {
                    self.i += 2;
                    self.token_boundary(Prev::Punct("("));
                    self.push_scope(Scope::Interp);
                    return Ok(());
                }
                Some('\n') => {
                    self.line += 1;
                    self.i += 1;
                }
                Some(_) => self.i += 1,
            }
        }
    }

    fn regex_allowed(&self) -> bool {
        // A pending identifier means the previous token was an identifier —
        // division position.
        if self.pending.is_some() {
            return false;
        }
        match &self.prev {
            Prev::Start => true,
            Prev::Punct(_) => true, // ops, separators, `(`, `[`, `{`, `:`, …
            Prev::Keyword(kw) => REGEX_AFTER_KEYWORD.contains(kw),
            // `}` ends a statement/block far more often than an expression.
            Prev::BraceClose => true,
            Prev::Ident | Prev::Value | Prev::ParenClose(_) | Prev::BracketClose => false,
        }
    }

    fn skip_regex(&mut self) -> Result<(), String> {
        self.i += 1;
        let mut in_class = false;
        loop {
            match self.peek(0) {
                None | Some('\n') => {
                    // A regex can't span lines; treat as a mis-lex and fail
                    // closed rather than silently swallowing code.
                    return Err(self.unbalanced("unterminated regex literal"));
                }
                Some('\\') => self.i += 2,
                Some('[') => {
                    in_class = true;
                    self.i += 1;
                }
                Some(']') => {
                    in_class = false;
                    self.i += 1;
                }
                Some('/') if !in_class => {
                    self.i += 1;
                    while self.peek(0).is_some_and(|c| c.is_ascii_alphabetic()) {
                        self.i += 1;
                    }
                    return Ok(());
                }
                Some(_) => self.i += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frags(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, s)| (n.to_string(), s.to_string()))
            .collect()
    }

    fn check(list: &[(&str, &str)]) -> Result<(), String> {
        check_eval_order(&frags(list))
    }

    // ── The incident, direct form (must be caught) ──

    #[test]
    fn direct_top_level_reference_to_later_let_fails() {
        // Fragment 49 reads (at eval time) a `let` that fragment 58
        // declares: the direct form of the 2026-07-09 module death.
        let err = check(&[
            (
                "49-daemons.js",
                "function ui() { return 1; }\nif (daemonVirtualDisplaysAvailable) { ui(); }\n",
            ),
            (
                "58-boot.js",
                "let daemonVirtualDisplaysAvailable = null;\nui();\n",
            ),
        ])
        .unwrap_err();
        assert!(err.contains("49-daemons.js:2"), "{err}");
        assert!(err.contains("58-boot.js:1"), "{err}");
        assert!(err.contains("daemonVirtualDisplaysAvailable"), "{err}");
        assert!(err.contains("temporal dead zone"), "{err}");
    }

    #[test]
    fn top_level_initializer_and_call_argument_references_fail() {
        let err = check(&[
            ("40-a.js", "const derived = LATER_CONST + 1;\n"),
            ("50-b.js", "const LATER_CONST = 2;\n"),
        ])
        .unwrap_err();
        assert!(err.contains("LATER_CONST"), "{err}");

        let err = check(&[
            ("40-a.js", "function go(x) {}\ngo(laterFlag);\n"),
            ("50-b.js", "let laterFlag = false;\n"),
        ])
        .unwrap_err();
        assert!(
            err.contains("laterFlag") && err.contains("40-a.js:2"),
            "{err}"
        );
    }

    #[test]
    fn assignment_template_and_extends_references_fail() {
        let err = check(&[
            ("40-a.js", "laterState = 5;\n"),
            ("50-b.js", "let laterState;\n"),
        ])
        .unwrap_err();
        assert!(err.contains("laterState"), "{err}");

        let err = check(&[
            ("40-a.js", "const s = `v=${laterVal}`;\n"),
            ("50-b.js", "const laterVal = 1;\n"),
        ])
        .unwrap_err();
        assert!(err.contains("laterVal"), "{err}");

        let err = check(&[
            ("40-a.js", "class Sub extends LaterBase {}\n"),
            ("50-b.js", "class LaterBase {}\n"),
        ])
        .unwrap_err();
        assert!(err.contains("LaterBase"), "{err}");
    }

    #[test]
    fn duplicate_top_level_declarations_fail() {
        let err = check(&[
            ("40-a.js", "let shared = 1;\n"),
            ("50-b.js", "const shared = 2;\n"),
        ])
        .unwrap_err();
        assert!(err.contains("duplicate top-level declaration"), "{err}");
        assert!(err.contains("shared"), "{err}");
    }

    // ── Legal cross-fragment patterns (must NOT be flagged) ──

    #[test]
    fn deferred_references_and_hoisted_functions_pass() {
        check(&[
            (
                "40-a.js",
                concat!(
                    // Function bodies are deferred: reading a later let from
                    // inside one is legal when it runs post-eval.
                    "function readLater() { return laterFlag; }\n",
                    // Hoisted function declared later, called at eval: legal.
                    "laterFn();\n",
                    // Arrow bodies (block and expression) are deferred.
                    "const a = () => laterFlag;\n",
                    "const b = (x) => { return laterFlag + x; };\n",
                    // Handlers registered at eval, invoked later.
                    "document.addEventListener('x', () => laterFlag);\n",
                    // Method and getter bodies in object literals.
                    "const obj = { go() { return laterFlag; }, get v() { return laterFlag; } };\n",
                    // Class bodies (methods) are deferred.
                    "class C { peek() { return laterFlag; } }\n",
                    // Default-parameter expressions evaluate at call time.
                    "function withDefault(x = laterFlag, y = { k: laterFlag }) { return x + y.k; }\n",
                ),
            ),
            ("50-b.js", "let laterFlag = true;\nfunction laterFn() {}\n"),
        ])
        .unwrap();
    }

    #[test]
    fn references_to_earlier_and_same_fragment_declarations_pass() {
        check(&[
            ("40-a.js", "let flag = 1;\nconst n = flag + 1;\n"),
            (
                "50-b.js",
                "if (flag) { console.log(flag, n); }\nlet local = flag;\n",
            ),
        ])
        .unwrap();
    }

    #[test]
    fn browser_globals_and_undeclared_names_pass() {
        check(&[
            (
                "40-a.js",
                "window.addEventListener('load', main);\nconst qs = new URLSearchParams(location.search);\nfunction main() {}\n",
            ),
            ("50-b.js", "let unrelated = 1;\n"),
        ])
        .unwrap();
    }

    #[test]
    fn local_shadowing_suppresses_cross_fragment_flagging() {
        // `item` is block-scoped in fragment 40 (for-of head); the later
        // top-level `let item` must not turn those uses into findings.
        check(&[
            (
                "40-a.js",
                "for (const item of list()) { render(item); }\nfunction list() { return []; }\nfunction render(_) {}\n",
            ),
            ("50-b.js", "let item = null;\n"),
        ])
        .unwrap();
    }

    #[test]
    fn destructuring_declarations_bind_not_reference() {
        check(&[
            (
                "40-a.js",
                concat!(
                    "for (const [k, v] of Object.entries({})) { use(k, v); }\n",
                    "function use(a, b) {}\n",
                    "const { width, height: h } = measure();\n",
                    "function measure() { return { width: 1, height: 2 }; }\n",
                ),
            ),
            (
                "50-b.js",
                "let k = 1;\nlet v = 2;\nlet width = 3;\nlet h = 4;\n",
            ),
        ])
        .unwrap();
    }

    #[test]
    fn object_keys_labels_and_properties_are_not_references() {
        check(&[
            (
                "40-a.js",
                concat!(
                    // `status` here is a key and a property, never a read of
                    // the binding.
                    "const o = { status: 1, nested: { status: 2 } };\n",
                    "console.log(o.status, o?.status);\n",
                    // Ternary alternates after a key colon.
                    "const p = { pick: o.status ? 'a' : 'b' };\n",
                ),
            ),
            ("50-b.js", "let status = 'later';\n"),
        ])
        .unwrap();

        // But shorthand `{ status }` IS a read of the binding.
        let err = check(&[
            ("40-a.js", "const o = { status };\n"),
            ("50-b.js", "let status = 'later';\n"),
        ])
        .unwrap_err();
        assert!(err.contains("status"), "{err}");
    }

    #[test]
    fn strings_comments_templates_and_regex_are_skipped() {
        check(&[
            (
                "40-a.js",
                concat!(
                    "// laterFlag in a comment\n",
                    "/* laterFlag\n   in a block comment */\n",
                    "const s = 'laterFlag';\n",
                    "const t = \"laterFlag\";\n",
                    "const u = `laterFlag ${'x'}`;\n",
                    "const re = /laterFlag/g;\n",
                    "const div = 4 / 2 / 1;\n",
                ),
            ),
            ("50-b.js", "let laterFlag = true;\n"),
        ])
        .unwrap();
    }

    #[test]
    fn arrow_and_function_parameters_are_bindings_not_references() {
        check(&[
            (
                "40-a.js",
                concat!(
                    "const f = (laterFlag, other) => laterFlag + other;\n",
                    "const g = laterFlag => laterFlag * 2;\n",
                    "function h(laterFlag) { return laterFlag; }\n",
                    "try { h(1); } catch (laterFlag) { console.log(laterFlag); }\n",
                ),
            ),
            ("50-b.js", "let laterFlag = true;\nlet other = 1;\n"),
        ])
        .unwrap();
    }

    #[test]
    fn control_flow_blocks_are_eval_time() {
        // Top-level if/for/try blocks run at eval: references inside them
        // to later declarations are real TDZ throws.
        let err = check(&[
            (
                "40-a.js",
                "if (true) { touch(laterFlag); }\nfunction touch(_) {}\n",
            ),
            ("50-b.js", "let laterFlag = 1;\n"),
        ])
        .unwrap_err();
        assert!(err.contains("laterFlag"), "{err}");

        let err = check(&[
            ("40-a.js", "try { void laterFlag; } catch (e) {}\n"),
            ("50-b.js", "let laterFlag = 1;\n"),
        ])
        .unwrap_err();
        assert!(err.contains("laterFlag"), "{err}");
    }

    // ── The documented limitation: the indirect (call-chain) form ──

    #[test]
    fn indirect_call_chain_is_not_catchable_and_passes() {
        // The incident's literal shape: an eval-time CALL into a function
        // whose body reads the later `let`. Statically deciding this needs
        // call-graph reachability — out of scope by design; the runtime
        // module-death canary covers it. This test pins the limitation.
        check(&[
            (
                "49-daemons.js",
                concat!(
                    "function updateUi() { return virtualDisplaysAvailableNow(); }\n",
                    "function virtualDisplaysAvailableNow() { return laterLet === true; }\n",
                    "updateUi();\n", // throws at runtime, invisible statically
                ),
            ),
            ("58-boot.js", "let laterLet = null;\n"),
        ])
        .unwrap();
    }

    #[test]
    fn iife_bodies_are_treated_as_deferred_known_false_negative() {
        // Documented limitation: IIFE bodies DO run at eval time, but the
        // heuristic classifies function bodies as deferred without checking
        // for immediate invocation. Pinned so a future improvement flips
        // this test deliberately.
        check(&[
            (
                "40-a.js",
                "(function () { void laterFlag; })();\n(() => { void laterFlag; })();\n",
            ),
            ("50-b.js", "let laterFlag = 1;\n"),
        ])
        .unwrap();
    }

    // ── Scanner robustness ──

    #[test]
    fn unbalanced_fragment_fails_closed() {
        let err = check(&[("40-a.js", "function broken() {\n")]).unwrap_err();
        assert!(err.contains("could not lex"), "{err}");
        assert!(err.contains("40-a.js"), "{err}");
    }

    #[test]
    fn keyword_spelled_object_keys_do_not_confuse_the_scanner() {
        check(&[
            (
                "40-a.js",
                "const o = { class: 'chip', for: 'id', if: 1, get: 2 };\nsetCls(o.class);\nfunction setCls(_) {}\n",
            ),
            ("50-b.js", "let unrelated = 0;\n"),
        ])
        .unwrap();
    }

    #[test]
    fn tagged_template_tag_is_a_reference() {
        let err = check(&[
            ("40-a.js", "const html = laterTag`<b>x</b>`;\n"),
            ("50-b.js", "const laterTag = (s) => s;\n"),
        ])
        .unwrap_err();
        assert!(err.contains("laterTag"), "{err}");
    }
}
