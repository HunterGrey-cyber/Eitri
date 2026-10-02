//! tmux's configuration syntax, read the way tmux 3.7's own lexer and grammar read it (its
//! `cmd-parse.y`), so a file means here what it means to tmux: `#` comments, a backslash before a
//! newline joining two lines, single quotes (nothing expanded), double quotes (escapes, `$VAR` and a
//! leading `~` expanded), backslash escapes outside single quotes (`\\` is one backslash, `\;` is a
//! literal `;` argument, which is how a key binding carries two commands), `;` between commands,
//! `{ ... }` blocks, `NAME=value` assignments and the `%if`/`%else`/`%endif`/`%hidden` lines.
//!
//! It only reads. Nothing here runs a command, evaluates a format or a `%if` condition, or asks a
//! tmux server anything: what a statement means for Eitri is decided by the caller.

use std::collections::BTreeMap;

/// One argument of a command: a word, or a `{ ... }` block holding commands of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Word(String),
    Block(Vec<Command>),
}

impl Arg {
    pub fn word(&self) -> Option<&str> {
        match self {
            Arg::Word(w) => Some(w),
            Arg::Block(_) => None,
        }
    }
}

/// A command and its arguments; the first word is the command's name.
pub type Command = Vec<Arg>;

/// What one statement of a file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Commands separated by `;` (an assignment before a command has already been made).
    Commands(Vec<Command>),
    /// `NAME=value` on its own: tmux sets it in its environment while it reads the file, and a later
    /// `$NAME` in the same file expands to it.
    Assign,
    /// `%hidden NAME=value`.
    Hidden,
    /// `%if <format>` / `%elif <format>`: a condition only tmux can evaluate.
    If,
    Elif,
    Else,
    Endif,
}

/// One statement: the line it starts on (1-based), its text as written (continuations joined,
/// comments dropped, runs of blanks squeezed), and what it is -- or why tmux would refuse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub line: usize,
    pub text: String,
    pub item: Result<Item, String>,
    pub depends_on: Option<String>,
}

/// How deep `{ ... }` blocks may nest. tmux itself has no limit of its own beyond its parser's
/// stack; real configs nest one or two deep, and a bound keeps a hostile file from recursing.
pub const MAX_NESTING: usize = 16;
/// The longest `NAME=value` tmux accepts: its parser refuses a longer one as "environment variable
/// is too long" (`CMD_PARSE_MAX_ENVIRON_LEN` in tmux 3.7's `cmd-parse.y`).
pub const MAX_ASSIGNMENT: usize = 16384;
/// The longest word, after `$NAME` and `~` are expanded.
pub const MAX_WORD: usize = 64 * 1024;
/// Commands one statement may hold.
pub const MAX_COMMANDS: usize = 256;
/// How much of a statement's text is kept for the skip list.
pub const MAX_TEXT: usize = 200;
/// Bytes `$NAME` and `~` may add over a whole import, every file together.
pub const EXPANSION_BUDGET: usize = 1024 * 1024;

/// What `$NAME` and a leading `~` expand to: tmux's environment as it reads a file -- the process's
/// own, plus the `NAME=value` lines read so far -- and the home directory. `uncertain` names the
/// variables a `%if` block or a `%hidden` line may have set: Eitri cannot evaluate those, so it
/// does not set them, and marks whatever reads one. `budget` is what expansion may still add.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    pub home: Option<String>,
    pub env: BTreeMap<String, String>,
    pub uncertain: std::collections::BTreeSet<String>,
    pub budget: std::cell::Cell<usize>,
}

impl Vars {
    pub fn new(home: Option<String>, env: BTreeMap<String, String>) -> Vars {
        Vars {
            home,
            env,
            uncertain: Default::default(),
            budget: std::cell::Cell::new(EXPANSION_BUDGET),
        }
    }

    /// Takes `n` bytes from the expansion budget, or says it is spent.
    fn spend(&self, n: usize) -> Result<(), String> {
        let left = self.budget.get();
        if n > left {
            self.budget.set(0);
            return Err("the expansion budget is spent (variables expanded to too much text)".to_string());
        }
        self.budget.set(left - n);
        Ok(())
    }
}

/// The variables a lexer expands from: its own while reading a file (assignments are made), or a
/// borrowed set while a command string is read again (nothing is assigned).
enum VarsRef<'a> {
    Own(Vars),
    Shared(&'a Vars),
}

impl VarsRef<'_> {
    fn get(&self) -> &Vars {
        match self {
            VarsRef::Own(v) => v,
            VarsRef::Shared(v) => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word { text: String, equals: bool },
    Semi,
    Open,
    Close,
    Newline,
    If,
    Elif,
    Else,
    Endif,
    Hidden,
    Eof,
}

/// tmux removes a backslash and the newline after it before it lexes anything, but only when the
/// backslashes in front of the newline are odd in number (an even run is that many escaped
/// backslashes). Done up front here, so the lexer below works on joined text; `line_of[i]` is the
/// file line character `i` came from.
fn join_continuations(source: &str) -> (Vec<char>, Vec<usize>) {
    let raw: Vec<char> = source.chars().collect();
    let mut chars = Vec::with_capacity(raw.len());
    let mut line_of = Vec::with_capacity(raw.len());
    let mut line = 1;
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == '\\' {
            let start = i;
            while i < raw.len() && raw[i] == '\\' {
                i += 1;
            }
            let run = i - start;
            if i < raw.len() && raw[i] == '\n' && run % 2 == 1 {
                for _ in 0..run - 1 {
                    chars.push('\\');
                    line_of.push(line);
                }
                line += 1;
                i += 1;
            } else {
                for _ in 0..run {
                    chars.push('\\');
                    line_of.push(line);
                }
            }
            continue;
        }
        chars.push(raw[i]);
        line_of.push(line);
        if raw[i] == '\n' {
            line += 1;
        }
        i += 1;
    }
    (chars, line_of)
}

fn is_var(ch: char, first: bool) -> bool {
    if ch == '=' || (first && ch.is_ascii_digit()) {
        return false;
    }
    ch.is_ascii_alphanumeric() || ch == '_'
}

struct Lexer<'a> {
    chars: Vec<char>,
    line_of: Vec<usize>,
    pos: usize,
    vars: VarsRef<'a>,
    /// Where the last token returned ended, for a statement's text.
    end: usize,
    /// The first uncertain variable the current statement expanded.
    tainted: Option<String>,
}

impl Lexer<'_> {
    fn getc(&mut self) -> Option<char> {
        let ch = self.chars.get(self.pos).copied();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    fn ungetc(&mut self, ch: Option<char>) {
        if ch.is_some() {
            self.pos -= 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn line_at(&self, pos: usize) -> usize {
        self.line_of
            .get(pos)
            .or_else(|| self.line_of.last())
            .copied()
            .unwrap_or(1)
    }

    /// The next token, and the position its first character is at.
    fn next(&mut self) -> Result<(Tok, usize), String> {
        loop {
            let start = self.pos;
            let Some(ch) = self.getc() else {
                return Ok((Tok::Eof, start));
            };
            match ch {
                ' ' | '\t' => continue,
                '\r' if self.peek() == Some('\n') => {
                    self.getc();
                    return Ok((Tok::Newline, start));
                }
                '\n' => return Ok((Tok::Newline, start)),
                ';' => {
                    self.end = self.pos;
                    return Ok((Tok::Semi, start));
                }
                '{' => {
                    self.end = self.pos;
                    return Ok((Tok::Open, start));
                }
                '}' => {
                    self.end = self.pos;
                    return Ok((Tok::Close, start));
                }
                // A `#` where a word would start is a comment to the end of the line.
                '#' => {
                    while let Some(c) = self.getc() {
                        if c == '\n' {
                            return Ok((Tok::Newline, start));
                        }
                    }
                    return Ok((Tok::Eof, start));
                }
                '%' => {
                    let mut word = String::from('%');
                    while let Some(c) = self.getc() {
                        if matches!(c, ' ' | '\t' | '\n') {
                            self.ungetc(Some(c));
                            break;
                        }
                        word.push(c);
                    }
                    self.end = self.pos;
                    // All `%` or digits is an ordinary word (`bind % ...`).
                    if word.chars().all(|c| c == '%' || c.is_ascii_digit()) {
                        return Ok((
                            Tok::Word {
                                text: word,
                                equals: false,
                            },
                            start,
                        ));
                    }
                    let tok = match word.as_str() {
                        "%hidden" => Tok::Hidden,
                        "%if" => Tok::If,
                        "%elif" => Tok::Elif,
                        "%else" => Tok::Else,
                        "%endif" => Tok::Endif,
                        _ => return Err(format!("{word} is not a tmux directive")),
                    };
                    if matches!(tok, Tok::If | Tok::Elif) {
                        // The condition is a format to the end of the line; only tmux evaluates it.
                        while let Some(c) = self.getc() {
                            if c == '\n' {
                                self.ungetc(Some(c));
                                break;
                            }
                        }
                        self.end = self.pos;
                    }
                    return Ok((tok, start));
                }
                _ => {
                    let text = self.word(ch)?;
                    self.end = self.pos;
                    let equals = match text.find('=') {
                        Some(at) => {
                            let mut name = text[..at].chars();
                            name.next().is_some_and(|c| is_var(c, true)) && name.all(|c| is_var(c, false))
                        }
                        None => false,
                    };
                    return Ok((Tok::Word { text, equals }, start));
                }
            }
        }
    }

    /// One word, from its first character `ch` (tmux's `yylex_token`).
    fn word(&mut self, first: char) -> Result<String, String> {
        #[derive(PartialEq, Clone, Copy)]
        enum State {
            Start,
            None,
            Double,
            Single,
        }
        let mut buf = String::new();
        let mut state = State::None;
        // `last` is what `state` was after the previous character was taken; a `~` expands only
        // where nothing has been taken yet in the current quoting, as tmux decides it.
        let mut last = State::Start;
        let mut ch = Some(first);
        while let Some(mut c) = ch {
            if buf.len() > MAX_WORD {
                return Err(format!("a word longer than {} KiB", MAX_WORD / 1024));
            }
            if state == State::None && c == '\r' && self.peek() == Some('\n') {
                c = self.getc().expect("peeked");
            }
            if c == '\n' && state == State::None {
                break;
            }
            if state == State::None && matches!(c, ' ' | '\t' | ';' | '}') {
                break;
            }
            if c == '\n' {
                // Inside quotes a newline is kept, but the next line's leading blanks are not, and a
                // comment line there is dropped -- unless the `#` starts a format or a `##`.
                buf.push('\n');
                let mut next = self.getc();
                while matches!(next, Some(' ' | '\t')) {
                    next = self.getc();
                }
                if next == Some('#') {
                    let after = self.getc();
                    if after.is_some_and(|a| ",#{}:".contains(a)) {
                        self.ungetc(after);
                        ch = Some('#');
                        continue;
                    }
                    let mut skip = after;
                    while !matches!(skip, Some('\n') | None) {
                        skip = self.getc();
                    }
                    ch = self.getc();
                    continue;
                }
                ch = next;
                continue;
            }
            if c == '\\' && state != State::Single {
                self.escape(&mut buf)?;
                last = state;
                ch = self.getc();
                continue;
            }
            if c == '~' && last != state && state != State::Single {
                self.tilde(&mut buf)?;
                last = state;
                ch = self.getc();
                continue;
            }
            if c == '$' && state != State::Single {
                self.variable(&mut buf)?;
                last = state;
                ch = self.getc();
                continue;
            }
            if c == '\'' && matches!(state, State::None | State::Single) {
                state = if state == State::None {
                    State::Single
                } else {
                    State::None
                };
                ch = self.getc();
                continue;
            }
            if c == '"' && matches!(state, State::None | State::Double) {
                state = if state == State::None {
                    State::Double
                } else {
                    State::None
                };
                ch = self.getc();
                continue;
            }
            buf.push(c);
            last = state;
            ch = self.getc();
        }
        if matches!(state, State::Single | State::Double) {
            return Err("a quote is not closed".to_string());
        }
        self.ungetc(ch);
        Ok(buf)
    }

    /// After a backslash outside single quotes (tmux's `yylex_token_escape`).
    fn escape(&mut self, buf: &mut String) -> Result<(), String> {
        let Some(ch) = self.getc() else {
            return Err("a backslash at the end of the file".to_string());
        };
        let plain = match ch {
            '4'..='7' => return Err("invalid octal escape".to_string()),
            '0'..='3' => {
                let o2 = self.getc().filter(|c| ('0'..='7').contains(c));
                let o3 = self.getc().filter(|c| ('0'..='7').contains(c));
                match (o2, o3) {
                    (Some(o2), Some(o3)) => {
                        let value =
                            64 * ch.to_digit(8).unwrap() + 8 * o2.to_digit(8).unwrap() + o3.to_digit(8).unwrap();
                        char::from_u32(value).ok_or_else(|| "invalid octal escape".to_string())?
                    }
                    _ => return Err("invalid octal escape".to_string()),
                }
            }
            'a' => '\x07',
            'b' => '\x08',
            'e' => '\x1b',
            'f' => '\x0c',
            's' => ' ',
            'v' => '\x0b',
            'r' => '\r',
            'n' => '\n',
            't' => '\t',
            'u' | 'U' => {
                let size = if ch == 'u' { 4 } else { 8 };
                let mut hex = String::new();
                for _ in 0..size {
                    match self.getc() {
                        Some(h) if h.is_ascii_hexdigit() => hex.push(h),
                        _ => return Err(format!("invalid \\{ch} argument")),
                    }
                }
                let value = u32::from_str_radix(&hex, 16).map_err(|_| format!("invalid \\{ch} argument"))?;
                char::from_u32(value).ok_or_else(|| format!("invalid \\{ch} argument"))?
            }
            other => other,
        };
        buf.push(plain);
        Ok(())
    }

    /// A `~` that starts a word: the home directory. `~user` would need that user's entry in the
    /// password database; it is refused rather than guessed.
    fn tilde(&mut self, buf: &mut String) -> Result<(), String> {
        let mut name = String::new();
        loop {
            let ch = self.getc();
            match ch {
                Some(c) if !"/ \t\n\"'".contains(c) => name.push(c),
                _ => {
                    self.ungetc(ch);
                    break;
                }
            }
        }
        if !name.is_empty() {
            return Err(format!(
                "~{name} names another user's home, which Eitri does not look up"
            ));
        }
        let vars = self.vars.get();
        // `~` is `$HOME`, so it is as uncertain as HOME is.
        let uncertain = vars.uncertain.contains("HOME");
        let home = vars
            .env
            .get("HOME")
            .filter(|h| !h.is_empty())
            .or(vars.home.as_ref())
            .ok_or_else(|| "~ with no home directory".to_string())?;
        vars.spend(home.len())?;
        buf.push_str(home);
        if uncertain && self.tainted.is_none() {
            self.tainted = Some("HOME".to_string());
        }
        Ok(())
    }

    /// `$NAME` or `${NAME}`: its value, or nothing when it is unset (tmux's `yylex_token_variable`).
    fn variable(&mut self, buf: &mut String) -> Result<(), String> {
        let ch = self.getc();
        let mut name = String::new();
        let braces = ch == Some('{');
        if !braces {
            match ch {
                Some(c) if is_var(c, true) => name.push(c),
                _ => {
                    buf.push('$');
                    self.ungetc(ch);
                    return Ok(());
                }
            }
        }
        loop {
            let ch = self.getc();
            if braces && ch == Some('}') {
                break;
            }
            match ch {
                Some(c) if is_var(c, false) => name.push(c),
                _ if !braces => {
                    self.ungetc(ch);
                    break;
                }
                _ => return Err("invalid environment variable".to_string()),
            }
        }
        let vars = self.vars.get();
        let uncertain = vars.uncertain.contains(&name);
        if let Some(value) = vars.env.get(&name) {
            vars.spend(value.len())?;
            if buf.len() + value.len() > MAX_WORD {
                return Err(format!("a word longer than {} KiB", MAX_WORD / 1024));
            }
            buf.push_str(value);
        }
        if uncertain && self.tainted.is_none() {
            self.tainted = Some(name);
        }
        Ok(())
    }
}

struct Parser<'a> {
    lex: Lexer<'a>,
    /// A token read ahead and given back.
    back: Option<(Tok, usize)>,
    /// How many `{` the current statement has open.
    depth: usize,
    /// How many `%if` blocks the current line is inside.
    conditional: usize,
}

impl Parser<'_> {
    fn next(&mut self) -> Result<(Tok, usize), String> {
        match self.back.take() {
            Some(tok) => Ok(tok),
            None => self.lex.next(),
        }
    }

    /// `NAME=value`, made in the environment the rest of the file expands from -- unless it is
    /// inside a `%if` block, whose condition only tmux can evaluate, or its value read a variable
    /// that is already uncertain: then the name is uncertain from here on (and inside a `%if`, the
    /// assignment is not made at all). A command string read again assigns nothing.
    fn assign(&mut self, text: &str) -> Result<(), String> {
        if text.len() > MAX_ASSIGNMENT {
            return Err(format!(
                "environment variable is too long (tmux refuses more than {MAX_ASSIGNMENT} bytes)"
            ));
        }
        // A value that read an uncertain variable is uncertain too, whatever the condition.
        let conditional = self.conditional > 0 || self.lex.tainted.is_some();
        if let (VarsRef::Own(vars), Some((name, value))) = (&mut self.lex.vars, text.split_once('=')) {
            if conditional {
                vars.uncertain.insert(name.to_string());
            } else {
                vars.uncertain.remove(name);
                vars.env.insert(name.to_string(), value.to_string());
            }
        }
        Ok(())
    }

    /// Commands up to the end of the statement -- or, inside a block, up to its `}`, newlines there
    /// separating commands the way `;` does.
    fn commands(&mut self, in_block: bool) -> Result<Vec<Command>, String> {
        let mut commands = Vec::new();
        let mut current: Command = Vec::new();
        loop {
            if commands.len() >= MAX_COMMANDS {
                return Err(format!("more than {MAX_COMMANDS} commands in one statement"));
            }
            let (tok, at) = self.next()?;
            match tok {
                Tok::Word { text, equals } => {
                    if current.is_empty() && equals {
                        self.assign(&text)?;
                    } else {
                        current.push(Arg::Word(text));
                    }
                }
                Tok::Semi => {
                    if !current.is_empty() {
                        commands.push(std::mem::take(&mut current));
                    }
                }
                Tok::Open => {
                    if current.is_empty() {
                        return Err("a { ... } block where a command was expected".to_string());
                    }
                    // Counted before it is judged: a refused `{` is still open as far as finding
                    // the end of the statement goes.
                    self.depth += 1;
                    if self.depth > MAX_NESTING {
                        return Err(format!("{{ ... }} blocks nested more than {MAX_NESTING} deep"));
                    }
                    let block = self.commands(true)?;
                    self.depth -= 1;
                    current.push(Arg::Block(block));
                }
                Tok::Close if in_block => break,
                Tok::Close => return Err("a } with no { before it".to_string()),
                Tok::Newline if in_block => {
                    if !current.is_empty() {
                        commands.push(std::mem::take(&mut current));
                    }
                }
                Tok::Newline => {
                    self.back = Some((Tok::Newline, at));
                    break;
                }
                Tok::Eof if in_block => return Err("a { ... } block is not closed".to_string()),
                Tok::Eof => {
                    self.back = Some((Tok::Eof, at));
                    break;
                }
                Tok::If | Tok::Elif | Tok::Else | Tok::Endif | Tok::Hidden => {
                    return Err("a % directive inside a command, which tmux evaluates itself".to_string())
                }
            }
        }
        if !current.is_empty() {
            commands.push(current);
        }
        Ok(commands)
    }

    /// The rest of the statement must be its end: a newline or the end of the file.
    fn end_of_statement(&mut self) -> Result<(), String> {
        match self.next()? {
            (Tok::Newline | Tok::Eof, at) => {
                self.back = Some((Tok::Newline, at));
                Ok(())
            }
            _ => Err("more after a % directive".to_string()),
        }
    }

    /// After an error: skips the rest of the statement, every block it has open included, so
    /// nothing inside a block that could not be read is ever taken for a statement of its own.
    /// Token by token, with no recursion, whatever the nesting.
    fn recover(&mut self) {
        let mut depth = self.depth;
        self.back = None;
        loop {
            let before = self.lex.pos;
            match self.lex.next() {
                Ok((Tok::Open, _)) => depth += 1,
                Ok((Tok::Close, _)) => depth = depth.saturating_sub(1),
                Ok((Tok::Newline, at)) if depth == 0 => {
                    self.back = Some((Tok::Newline, at));
                    break;
                }
                Ok((Tok::Eof, _)) => break,
                Ok(_) => {}
                Err(_) => {
                    if self.lex.pos == before {
                        self.lex.pos += 1;
                    }
                    if self.lex.pos >= self.lex.chars.len() {
                        break;
                    }
                }
            }
        }
        self.depth = 0;
        self.lex.end = self.lex.end.max(self.lex.pos.min(self.lex.chars.len()));
    }

    fn statement(&mut self) -> Option<Statement> {
        self.lex.tainted = None;
        self.depth = 0;
        let (tok, start) = loop {
            match self.next() {
                Ok((Tok::Newline, _)) => continue,
                Ok((Tok::Eof, _)) => return None,
                Ok(found) => break found,
                Err(why) => {
                    let line = self.lex.line_at(self.lex.pos.saturating_sub(1));
                    let start = self.line_start(self.lex.pos.saturating_sub(1));
                    self.recover();
                    return Some(Statement {
                        line,
                        text: self.text(start, self.lex.end),
                        item: Err(why),
                        depends_on: None,
                    });
                }
            }
        };
        let line = self.lex.line_at(start);
        let item = match tok {
            Tok::If => {
                self.conditional += 1;
                self.end_of_statement().map(|()| Item::If)
            }
            Tok::Elif => self.end_of_statement().map(|()| Item::Elif),
            Tok::Else => self.end_of_statement().map(|()| Item::Else),
            Tok::Endif => {
                self.conditional = self.conditional.saturating_sub(1);
                self.end_of_statement().map(|()| Item::Endif)
            }
            Tok::Hidden => match self.next() {
                Ok((Tok::Word { equals: true, text }, _)) => {
                    // Not made, so every later `$NAME` of it is uncertain.
                    if let (VarsRef::Own(vars), Some((name, _))) = (&mut self.lex.vars, text.split_once('=')) {
                        vars.uncertain.insert(name.to_string());
                    }
                    self.end_of_statement().map(|()| Item::Hidden)
                }
                Ok(_) => Err("%hidden needs NAME=value".to_string()),
                Err(why) => Err(why),
            },
            other => {
                self.back = Some((other, start));
                match self.commands(false) {
                    Ok(commands) if commands.is_empty() => Ok(Item::Assign),
                    Ok(commands) => Ok(Item::Commands(commands)),
                    Err(why) => Err(why),
                }
            }
        };
        if item.is_err() {
            self.recover();
        }
        // The statement's own newline stays for the next call to skip.
        Some(Statement {
            line,
            text: self.text(start, self.lex.end),
            item,
            depends_on: self.lex.tainted.take(),
        })
    }

    fn line_start(&self, pos: usize) -> usize {
        let mut at = pos.min(self.lex.chars.len());
        while at > 0 && self.lex.chars[at - 1] != '\n' {
            at -= 1;
        }
        at
    }

    /// The statement as written: its characters, blanks and kept newlines squeezed to one space,
    /// cut to [`MAX_TEXT`] characters and an ellipsis.
    fn text(&self, start: usize, end: usize) -> String {
        let start = start.min(self.lex.chars.len());
        let end = end.clamp(start, self.lex.chars.len());
        let mut out = String::new();
        let mut count = 0;
        for &c in &self.lex.chars[start..end] {
            if count == MAX_TEXT {
                let trimmed = out.trim_end().len();
                out.truncate(trimmed);
                out.push('\u{2026}');
                return out;
            }
            if c.is_whitespace() {
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                    count += 1;
                }
            } else {
                out.push(c);
                count += 1;
            }
        }
        out.trim_end().to_string()
    }
}

/// Every statement of `source`, in order. `vars` grows by the `NAME=value` lines read, as tmux's
/// environment does while it reads a file.
fn parse_all<'a>(source: &str, vars: VarsRef<'a>) -> (Vec<Statement>, VarsRef<'a>) {
    let (chars, line_of) = join_continuations(source);
    let mut parser = Parser {
        lex: Lexer {
            chars,
            line_of,
            pos: 0,
            vars,
            end: 0,
            tainted: None,
        },
        back: None,
        depth: 0,
        conditional: 0,
    };
    let mut out = Vec::new();
    while let Some(statement) = parser.statement() {
        out.push(statement);
    }
    (out, parser.lex.vars)
}

/// Every statement of `source`, in order. `vars` grows by the `NAME=value` lines read, as tmux's
/// environment does while it reads a file.
pub fn statements(source: &str, vars: &mut Vars) -> Vec<Statement> {
    let (out, back) = parse_all(source, VarsRef::Own(std::mem::take(vars)));
    if let VarsRef::Own(v) = back {
        *vars = v;
    }
    out
}

/// A command given as one string -- `bind x "split-window -h"`, `confirm-before "kill-pane"` --
/// read again as commands, as tmux reads such an argument. It expands from `vars` without copying
/// or changing them, and refuses a string that reads a variable a `%if` block may have set.
pub fn commands_in(text: &str, vars: &Vars) -> Result<Vec<Command>, String> {
    let (statements, _) = parse_all(text, VarsRef::Shared(vars));
    let mut commands = Vec::new();
    for statement in statements {
        if let Some(name) = statement.depends_on {
            return Err(format!(
                "it reads ${name}, which a %if block or %hidden line may have set"
            ));
        }
        match statement.item? {
            Item::Commands(found) => commands.extend(found),
            Item::Assign => {}
            _ => return Err("a % directive inside a command string".to_string()),
        }
    }
    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Arg {
        Arg::Word(s.to_string())
    }

    fn vars() -> Vars {
        let mut env = BTreeMap::new();
        env.insert("HOME".to_string(), "/h".to_string());
        env.insert("EDITOR".to_string(), "nvim".to_string());
        Vars::new(Some("/h".to_string()), env)
    }

    fn parse(source: &str) -> Vec<Statement> {
        statements(source, &mut vars())
    }

    fn only_commands(source: &str) -> Vec<Command> {
        let mut all = Vec::new();
        for s in parse(source) {
            match s.item {
                Ok(Item::Commands(c)) => all.extend(c),
                other => panic!("{source:?}: {other:?}"),
            }
        }
        all
    }

    #[test]
    fn words_comments_and_line_numbers() {
        let got = parse("# a comment\nset -g prefix C-a   # trailing\n\nbind x kill-pane\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].line, 2);
        assert_eq!(got[0].text, "set -g prefix C-a");
        assert_eq!(
            got[0].item,
            Ok(Item::Commands(vec![vec![w("set"), w("-g"), w("prefix"), w("C-a")]]))
        );
        assert_eq!(got[1].line, 4);
    }

    #[test]
    fn quotes_and_escapes() {
        assert_eq!(
            only_commands(r"bind \\ split-window"),
            vec![vec![w("bind"), w("\\"), w("split-window")]]
        );
        assert_eq!(only_commands(r"bind 'C-\' x"), vec![vec![w("bind"), w("C-\\"), w("x")]]);
        assert_eq!(
            only_commands(r#"bind '"' x "a b" 'c d' "e\"f" 'g\h'"#),
            vec![vec![
                w("bind"),
                w("\""),
                w("x"),
                w("a b"),
                w("c d"),
                w("e\"f"),
                w("g\\h")
            ]]
        );
        assert_eq!(only_commands(r"x \e\s\101"), vec![vec![w("x"), w("\x1b A")]]);
        assert_eq!(only_commands(r"x é"), vec![vec![w("x"), w("é")]]);
        // A `#` inside a word, or quoted, is not a comment.
        assert_eq!(
            only_commands(r##"bind - split -c "#{pane_current_path}" a#b"##),
            vec![vec![
                w("bind"),
                w("-"),
                w("split"),
                w("-c"),
                w("#{pane_current_path}"),
                w("a#b")
            ]]
        );
    }

    #[test]
    fn variables_and_the_home_directory() {
        assert_eq!(
            only_commands(r#"source ~/x "~/y" '~/z' a~ $EDITOR "${EDITOR}" '$EDITOR' $NOPE. $1"#),
            vec![vec![
                w("source"),
                w("/h/x"),
                w("/h/y"),
                w("~/z"),
                w("a~"),
                w("nvim"),
                w("nvim"),
                w("$EDITOR"),
                w("."),
                w("$1"),
            ]]
        );
        let got = parse("~bob/x");
        assert!(got[0].item.as_ref().unwrap_err().contains("~bob"), "{got:?}");
    }

    #[test]
    fn assignments_feed_later_expansions() {
        let mut v = vars();
        let got = statements("is_vim=\"ps -o state=\"\nbind -n C-h if-shell \"$is_vim\" x\n", &mut v);
        assert_eq!(got[0].item, Ok(Item::Assign));
        assert_eq!(
            got[1].item,
            Ok(Item::Commands(vec![vec![
                w("bind"),
                w("-n"),
                w("C-h"),
                w("if-shell"),
                w("ps -o state="),
                w("x")
            ]]))
        );
        assert_eq!(v.env.get("is_vim").map(String::as_str), Some("ps -o state="));
    }

    #[test]
    fn continuations_join_lines_and_keep_the_first_line_number() {
        let got = parse("bind b capture-pane -eJ \\; \\\n  display-popup -E 'less'\nbind c x\n");
        assert_eq!(got[0].line, 1);
        assert_eq!(got[0].text, "bind b capture-pane -eJ \\; display-popup -E 'less'");
        assert_eq!(
            got[0].item,
            Ok(Item::Commands(vec![vec![
                w("bind"),
                w("b"),
                w("capture-pane"),
                w("-eJ"),
                w(";"),
                w("display-popup"),
                w("-E"),
                w("less")
            ]]))
        );
        assert_eq!(got[1].line, 3);
        // An even run of backslashes before the newline is that many escaped backslashes.
        let got = parse("x \\\\\ny\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].item, Ok(Item::Commands(vec![vec![w("x"), w("\\")]])));
    }

    #[test]
    fn a_semicolon_separates_commands_and_an_escaped_one_is_an_argument() {
        assert_eq!(
            only_commands("unbind a ; unbind b;unbind c"),
            vec![
                vec![w("unbind"), w("a")],
                vec![w("unbind"), w("b")],
                vec![w("unbind"), w("c")]
            ]
        );
        assert_eq!(
            only_commands(r"bind x a \; b"),
            vec![vec![w("bind"), w("x"), w("a"), w(";"), w("b")]]
        );
    }

    #[test]
    fn blocks_span_lines() {
        let got = parse("bind , command-prompt -I \"#W\" {\n  rename-window -- \"%%\"\n}\nbind z kill-pane\n");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].line, 1);
        assert_eq!(
            got[0].item,
            Ok(Item::Commands(vec![vec![
                w("bind"),
                w(","),
                w("command-prompt"),
                w("-I"),
                w("#W"),
                Arg::Block(vec![vec![w("rename-window"), w("--"), w("%%")]])
            ]]))
        );
        assert_eq!(got[1].line, 4);
        let got = parse("bind x { kill-pane\n");
        assert!(got[0].item.is_err());
    }

    #[test]
    fn directives() {
        let got = parse("%if #{==:#{host},x}\nbind a b\n%elif 1\n%else\n%endif\n%hidden A=b\n% x\n%bogus\n");
        let items: Vec<_> = got.iter().map(|s| s.item.clone()).collect();
        assert_eq!(items[0], Ok(Item::If));
        assert_eq!(items[2], Ok(Item::Elif));
        assert_eq!(items[3], Ok(Item::Else));
        assert_eq!(items[4], Ok(Item::Endif));
        assert_eq!(items[5], Ok(Item::Hidden));
        assert_eq!(items[6], Ok(Item::Commands(vec![vec![w("%"), w("x")]])));
        assert!(items[7].is_err());
    }

    #[test]
    fn an_unreadable_statement_is_reported_and_the_next_line_still_reads() {
        let got = parse("bind x \"open\nbind y z\n");
        // An unclosed double quote runs to the end of the file in tmux too.
        assert!(got[0].item.is_err(), "{got:?}");
        let got = parse("bind x \\5 y\nbind z kill-pane\n");
        assert!(got[0].item.is_err(), "{got:?}");
        assert_eq!(got[1].line, 2);
        assert_eq!(
            got[1].item,
            Ok(Item::Commands(vec![vec![w("bind"), w("z"), w("kill-pane")]]))
        );
    }

    #[test]
    fn blocks_nested_deeper_than_the_bound_are_refused_without_recursing() {
        let nested = |n: usize| {
            format!(
                "{}bind m kill-pane{}\nbind z kill-pane\n",
                "x { ".repeat(n),
                " }".repeat(n)
            )
        };
        let ok = parse(&nested(MAX_NESTING));
        assert!(ok[0].item.is_ok(), "{:?}", ok[0].item);
        let deep = parse(&nested(MAX_NESTING + 1));
        assert!(
            deep[0].item.as_ref().unwrap_err().contains("nested"),
            "{:?}",
            deep[0].item
        );
        // Far past any stack: refused, and the line after it still reads.
        let huge = parse(&nested(200_000));
        assert!(huge[0].item.is_err());
        assert_eq!(huge.len(), 2);
        assert_eq!(
            huge[1].item,
            Ok(Item::Commands(vec![vec![w("bind"), w("z"), w("kill-pane")]]))
        );
    }

    #[test]
    fn an_assignment_longer_than_tmux_allows_is_refused_and_doubling_stops_there() {
        let mut v = vars();
        let mut source = String::from("X=aaaaaaaaaaaaaaaa\n");
        source.push_str(&"X=$X$X\n".repeat(36));
        let got = statements(&source, &mut v);
        assert!(v.env["X"].len() <= MAX_ASSIGNMENT, "{}", v.env["X"].len());
        assert!(
            got.iter()
                .any(|s| s.item.as_ref().is_err_and(|e| e.contains("too long"))),
            "{got:?}"
        );
    }

    #[test]
    fn expansion_stops_at_its_budget() {
        let mut v = vars();
        v.budget.set(100);
        let got = statements("bind m $EDITOR$EDITOR\n".repeat(40).as_str(), &mut v);
        assert!(
            got.iter()
                .any(|s| s.item.as_ref().is_err_and(|e| e.contains("expansion"))),
            "{got:?}"
        );
        assert!(v.budget.get() <= 100);
    }

    #[test]
    fn a_statement_with_too_many_commands_is_refused_and_its_text_is_an_excerpt() {
        let line = "bind m kill-pane;".repeat(MAX_COMMANDS + 1);
        let got = parse(&line);
        assert!(
            got[0].item.as_ref().unwrap_err().contains("commands"),
            "{:?}",
            got[0].item
        );
        assert!(got[0].text.chars().count() <= MAX_TEXT + 1, "{}", got[0].text.len());
        let ok = parse(&"bind m kill-pane;".repeat(3));
        assert_eq!(ok[0].text, "bind m kill-pane;bind m kill-pane;bind m kill-pane;");
    }

    #[test]
    fn an_assignment_inside_a_percent_if_is_not_made_and_what_reads_it_is_marked() {
        let mut v = vars();
        let got = statements(
            "ACTION=new-window\n%if #{==:#{host},x}\nACTION=kill-pane\n%endif\nbind m $ACTION\nbind n x\n",
            &mut v,
        );
        assert_eq!(v.env["ACTION"], "new-window", "the conditional assignment is not made");
        let bind_m = got.iter().find(|s| s.line == 5).unwrap();
        assert_eq!(bind_m.depends_on.as_deref(), Some("ACTION"));
        let bind_n = got.iter().find(|s| s.line == 6).unwrap();
        assert_eq!(bind_n.depends_on, None);
        // A %hidden variable is not set either, so what reads it is marked the same way.
        let mut v = vars();
        let got = statements("%hidden K=m\nbind $K kill-pane\n", &mut v);
        assert_eq!(got[1].depends_on.as_deref(), Some("K"));
        // And a command string read again later, in single quotes, is caught too.
        let mut v = vars();
        statements("%if 1\nA=kill-pane\n%endif\n", &mut v);
        assert!(commands_in("$A", &v).unwrap_err().contains("$A"));
    }

    #[test]
    fn an_error_inside_a_block_skips_the_whole_block_and_promotes_nothing() {
        let got = parse("if-shell false {\n  %if 1\n  bind m kill-pane\n  %endif\n}\nbind z kill-pane\n");
        assert!(got[0].item.is_err(), "{got:?}");
        let commands: Vec<_> = got.iter().filter_map(|s| s.item.clone().ok()).collect();
        assert_eq!(
            commands,
            vec![Item::Commands(vec![vec![w("bind"), w("z"), w("kill-pane")]])],
            "nothing inside the braces came up to the top"
        );
        let got = parse("bind x {\n kill-pane \\5\n bind m kill-pane\n}\nbind z kill-pane\n");
        let commands: Vec<_> = got.iter().filter_map(|s| s.item.clone().ok()).collect();
        assert_eq!(
            commands,
            vec![Item::Commands(vec![vec![w("bind"), w("z"), w("kill-pane")]])]
        );
    }

    #[test]
    fn a_command_string_reads_again_as_commands() {
        assert_eq!(
            commands_in("confirm-before -p 'kill?' kill-pane", &vars()).unwrap(),
            vec![vec![w("confirm-before"), w("-p"), w("kill?"), w("kill-pane")]]
        );
        assert_eq!(commands_in("a ; b", &vars()).unwrap(), vec![vec![w("a")], vec![w("b")]]);
    }
}
