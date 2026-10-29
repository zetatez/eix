//! Terminal-native display-math layout.
//!
//! This is the fallback used when the terminal cannot display images. It is
//! not a TeX engine: it parses the subset of LaTeX that agent replies actually
//! use and lays it out on a character grid: fractions get a real horizontal
//! bar, roots get an overline, big operators put limits above and below,
//! matrices keep their columns, and delimiters grow with their body. It also
//! applies TeX-style inter-atom spacing, honours font commands such as
//! `\mathbf`/`\mathit`, aligns `aligned`/`cases`/`array` columns, and draws
//! frames for `\boxed`, `\overbrace` and `\underbrace`.
//! Inline math stays on one line in a compact ASCII form (`x^2`, `1/(1+e^-x)`)
//! so prose keeps its normal line height. Display math is rendered as a LaTeX
//! image by [`super::math_image`] when a graphics protocol is available.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::markdown::{blackboard, symbol};

#[derive(Clone, Copy, PartialEq, Eq)]
enum AtomKind {
    Normal,
    Operator { limits: bool },
    Function { limits: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Font {
    bold: bool,
    italic: bool,
}

impl Font {
    fn merge(self, other: Font) -> Font {
        Font {
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
        }
    }

    fn style(self, base: Style) -> Style {
        let mut modifier = Modifier::empty();
        if self.bold {
            modifier |= Modifier::BOLD;
        }
        if self.italic {
            modifier |= Modifier::ITALIC;
        }
        base.add_modifier(modifier)
    }
}

const BOLD: Font = Font {
    bold: true,
    italic: false,
};
const ITALIC: Font = Font {
    bold: false,
    italic: true,
};

#[derive(Clone)]
enum Node {
    Row(Vec<Node>),
    Atom {
        text: String,
        kind: AtomKind,
        font: Font,
    },
    Text {
        text: String,
        font: Font,
    },
    Space(usize),
    LineBreak,
    Fraction(Box<Node>, Box<Node>),
    Sqrt {
        body: Box<Node>,
        index: Option<Box<Node>>,
    },
    Script {
        base: Box<Node>,
        sup: Option<Box<Node>>,
        sub: Option<Box<Node>>,
    },
    Delimited {
        left: String,
        right: String,
        body: Box<Node>,
    },
    Accent {
        kind: AccentKind,
        body: Box<Node>,
    },
    Matrix {
        env: String,
        spec: String,
        rows: Vec<Vec<Node>>,
    },
    OverUnder {
        over: Option<Box<Node>>,
        under: Option<Box<Node>>,
        body: Box<Node>,
    },
    Binom(Box<Node>, Box<Node>),
    Negated(Box<Node>),
    Boxed(Box<Node>),
}

#[derive(Clone, Copy)]
enum AccentKind {
    Overline,
    Underline,
    Hat,
    Bar,
    Vec,
    Tilde,
    Dot,
    Ddot,
    OverBrace,
    UnderBrace,
}

#[derive(Clone, Copy, Default)]
struct Stop {
    right: bool,
    amp: bool,
    row: bool,
    end: bool,
    bracket: bool,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            pos: 0,
        }
    }

    fn parse(mut self) -> Node {
        self.parse_row(Stop::default())
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    fn at_double_backslash(&self) -> bool {
        self.chars.get(self.pos) == Some(&'\\') && self.chars.get(self.pos + 1) == Some(&'\\')
    }

    fn at_command(&self, name: &str) -> bool {
        if self.peek() != Some('\\') {
            return false;
        }
        let mut index = self.pos + 1;
        for expected in name.chars() {
            if self.chars.get(index) != Some(&expected) {
                return false;
            }
            index += 1;
        }
        !self
            .chars
            .get(index)
            .is_some_and(|ch| ch.is_ascii_alphabetic())
    }

    fn read_command(&mut self) -> String {
        self.pos += 1;
        let Some(&first) = self.chars.get(self.pos) else {
            return "\\".to_string();
        };
        if first.is_ascii_alphabetic() {
            let start = self.pos;
            while self.peek().is_some_and(|ch| ch.is_ascii_alphabetic()) {
                self.pos += 1;
            }
            self.chars[start..self.pos].iter().collect()
        } else {
            self.pos += 1;
            first.to_string()
        }
    }

    fn skip_optional_bracket(&mut self) {
        self.skip_ws();
        if self.peek() != Some('[') {
            return;
        }
        self.pos += 1;
        while let Some(ch) = self.peek() {
            self.pos += 1;
            if ch == ']' {
                break;
            }
        }
    }

    fn parse_optional_bracket(&mut self) -> Option<Node> {
        self.skip_ws();
        if self.peek() != Some('[') {
            return None;
        }
        self.pos += 1;
        let node = self.parse_row(Stop {
            bracket: true,
            ..Stop::default()
        });
        if self.peek() == Some(']') {
            self.pos += 1;
        }
        Some(node)
    }

    fn parse_raw_group(&mut self) -> String {
        self.skip_ws();
        if self.peek() != Some('{') {
            return String::new();
        }
        self.pos += 1;
        let mut depth = 1usize;
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            self.pos += 1;
            match ch {
                '\\' => {
                    out.push('\\');
                    if let Some(next) = self.peek() {
                        out.push(next);
                        self.pos += 1;
                    }
                }
                '{' => {
                    depth += 1;
                    out.push('{');
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push('}');
                }
                _ => out.push(ch),
            }
        }
        out
    }

    fn parse_arg(&mut self) -> Node {
        self.skip_ws();
        match self.peek() {
            Some('{') => {
                self.pos += 1;
                let node = self.parse_row(Stop::default());
                if self.peek() == Some('}') {
                    self.pos += 1;
                }
                node
            }
            Some('\\') => {
                let command = self.read_command();
                self.command(&command)
            }
            Some(ch) => {
                self.pos += 1;
                Node::Atom {
                    text: ch.to_string(),
                    kind: AtomKind::Normal,
                    font: Font::default(),
                }
            }
            None => Node::Row(Vec::new()),
        }
    }

    fn parse_row(&mut self, stop: Stop) -> Node {
        let mut nodes = Vec::new();
        loop {
            let before_ws = self.pos;
            while self.peek().is_some_and(char::is_whitespace) {
                self.pos += 1;
            }
            if self.pos > before_ws && !nodes.is_empty() {
                let next = self.peek();
                let stop_ahead = matches!(next, Some('}' | '&' | '^' | '_'))
                    || self.at_double_backslash()
                    || (stop.right && self.at_command("right"))
                    || (stop.end && self.at_command("end"));
                if !stop_ahead {
                    push_space(&mut nodes, 1);
                }
            }
            let Some(ch) = self.peek() else {
                break;
            };
            if ch == '}' {
                break;
            }
            if stop.amp && ch == '&' {
                break;
            }
            if stop.bracket && ch == ']' {
                break;
            }
            if ch == '\\' {
                if self.at_double_backslash() {
                    if stop.row {
                        break;
                    }
                    self.pos += 2;
                    self.skip_optional_bracket();
                    nodes.push(Node::LineBreak);
                    continue;
                }
                if (stop.right && self.at_command("right")) || (stop.end && self.at_command("end"))
                {
                    break;
                }
                let command = self.read_command();
                let node = self.command(&command);
                push_node(&mut nodes, node);
                continue;
            }
            match ch {
                '^' | '_' => {
                    self.pos += 1;
                    let script = self.parse_arg();
                    let mut sup = None;
                    let mut sub = None;
                    if ch == '^' {
                        sup = Some(Box::new(script));
                    } else {
                        sub = Some(Box::new(script));
                    }
                    attach_script(&mut nodes, sup, sub);
                }
                '&' => {
                    self.pos += 1;
                    push_space(&mut nodes, 2);
                }
                '~' => {
                    self.pos += 1;
                    push_space(&mut nodes, 1);
                }
                '{' => {
                    self.pos += 1;
                    let inner = self.parse_row(Stop::default());
                    if self.peek() == Some('}') {
                        self.pos += 1;
                    }
                    nodes.push(inner);
                }
                '%' => {
                    while self.peek().is_some_and(|ch| ch != '\n') {
                        self.pos += 1;
                    }
                }
                _ => {
                    self.pos += 1;
                    nodes.push(Node::Atom {
                        text: ch.to_string(),
                        kind: AtomKind::Normal,
                        font: Font::default(),
                    });
                }
            }
        }
        Node::Row(nodes)
    }

    fn parse_delimiter(&mut self) -> String {
        self.skip_ws();
        let Some(ch) = self.peek() else {
            return String::new();
        };
        if ch == '\\' {
            let command = self.read_command();
            return match command.as_str() {
                "{" | "lbrace" => "{".to_string(),
                "}" | "rbrace" => "}".to_string(),
                "[" | "lbrack" => "[".to_string(),
                "]" | "rbrack" => "]".to_string(),
                "|" | "vert" | "lvert" | "rvert" => "|".to_string(),
                "\\" | "Vert" | "lVert" | "rVert" => "‖".to_string(),
                "langle" => "⟨".to_string(),
                "rangle" => "⟩".to_string(),
                "lfloor" => "⌊".to_string(),
                "rfloor" => "⌋".to_string(),
                "lceil" => "⌈".to_string(),
                "rceil" => "⌉".to_string(),
                "uparrow" => "↑".to_string(),
                "downarrow" => "↓".to_string(),
                "backslash" => "∖".to_string(),
                _ => symbol(&command).unwrap_or("").to_string(),
            };
        }
        self.pos += 1;
        if ch == '.' {
            String::new()
        } else {
            ch.to_string()
        }
    }

    fn parse_matrix(&mut self, env: &str) -> Node {
        let spec = if matches!(env, "array" | "tabular" | "array*" | "tabular*") {
            self.parse_raw_group()
        } else {
            String::new()
        };
        let mut rows = Vec::new();
        let mut row = Vec::new();
        loop {
            let cell = self.parse_row(Stop {
                amp: true,
                row: true,
                end: true,
                ..Stop::default()
            });
            row.push(cell);
            self.skip_ws();
            if self.peek() == Some('&') {
                self.pos += 1;
                continue;
            }
            if self.at_double_backslash() {
                self.pos += 2;
                self.skip_optional_bracket();
                rows.push(std::mem::take(&mut row));
                continue;
            }
            if self.at_command("end") {
                let _ = self.read_command();
                let _ = self.parse_raw_group();
                rows.push(row);
                break;
            }
            rows.push(row);
            break;
        }
        rows.retain(|row| {
            !(row.len() == 1 && matches!(row.first(), Some(Node::Row(items)) if items.is_empty()))
        });
        Node::Matrix {
            env: env.to_string(),
            spec,
            rows,
        }
    }

    fn command(&mut self, command: &str) -> Node {
        match command {
            "frac" | "dfrac" | "tfrac" | "cfrac" => {
                let numerator = self.parse_arg();
                let denominator = self.parse_arg();
                Node::Fraction(Box::new(numerator), Box::new(denominator))
            }
            "sqrt" => {
                let index = self.parse_optional_bracket();
                Node::Sqrt {
                    body: Box::new(self.parse_arg()),
                    index: index.map(Box::new),
                }
            }
            "binom" | "dbinom" | "tbinom" => {
                let n = self.parse_arg();
                let k = self.parse_arg();
                Node::Binom(Box::new(n), Box::new(k))
            }
            "text" | "textrm" | "textnormal" | "mbox" => Node::Text {
                text: unescape_text(&self.parse_raw_group()),
                font: Font::default(),
            },
            "operatorname" => Node::Atom {
                text: unescape_text(&self.parse_raw_group()),
                kind: AtomKind::Function { limits: false },
                font: Font::default(),
            },
            "mathbb" => Node::Atom {
                text: blackboard(&unescape_text(&self.parse_raw_group())),
                kind: AtomKind::Normal,
                font: Font::default(),
            },
            "mathbf" | "boldsymbol" | "bm" => apply_font(self.parse_arg(), BOLD),
            "mathit" | "mathcal" => apply_font(self.parse_arg(), ITALIC),
            "mathrm" | "mathsf" | "mathtt" | "mathfrak" | "mathop" | "mathrel" | "mathbin"
            | "mathord" | "mathopen" | "mathclose" | "mathpunct" | "mathinner" => self.parse_arg(),
            "overline" => Node::Accent {
                kind: AccentKind::Overline,
                body: Box::new(self.parse_arg()),
            },
            "underline" => Node::Accent {
                kind: AccentKind::Underline,
                body: Box::new(self.parse_arg()),
            },
            "hat" | "widehat" => Node::Accent {
                kind: AccentKind::Hat,
                body: Box::new(self.parse_arg()),
            },
            "bar" => Node::Accent {
                kind: AccentKind::Bar,
                body: Box::new(self.parse_arg()),
            },
            "vec" | "overrightarrow" | "overleftarrow" => Node::Accent {
                kind: AccentKind::Vec,
                body: Box::new(self.parse_arg()),
            },
            "tilde" | "widetilde" => Node::Accent {
                kind: AccentKind::Tilde,
                body: Box::new(self.parse_arg()),
            },
            "dot" => Node::Accent {
                kind: AccentKind::Dot,
                body: Box::new(self.parse_arg()),
            },
            "ddot" => Node::Accent {
                kind: AccentKind::Ddot,
                body: Box::new(self.parse_arg()),
            },
            "overbrace" => Node::Accent {
                kind: AccentKind::OverBrace,
                body: Box::new(self.parse_arg()),
            },
            "underbrace" => Node::Accent {
                kind: AccentKind::UnderBrace,
                body: Box::new(self.parse_arg()),
            },
            "overset" | "stackrel" => {
                let over = self.parse_arg();
                let body = self.parse_arg();
                Node::OverUnder {
                    over: Some(Box::new(over)),
                    under: None,
                    body: Box::new(body),
                }
            }
            "underset" => {
                let under = self.parse_arg();
                let body = self.parse_arg();
                Node::OverUnder {
                    over: None,
                    under: Some(Box::new(under)),
                    body: Box::new(body),
                }
            }
            "left" => {
                let left = self.parse_delimiter();
                let body = self.parse_row(Stop {
                    right: true,
                    ..Stop::default()
                });
                if self.at_command("right") {
                    let _ = self.read_command();
                }
                let right = self.parse_delimiter();
                Node::Delimited {
                    left,
                    right,
                    body: Box::new(body),
                }
            }
            "middle" => Node::Atom {
                text: self.parse_delimiter(),
                kind: AtomKind::Normal,
                font: Font::default(),
            },
            "begin" => {
                let env = unescape_text(&self.parse_raw_group());
                self.parse_matrix(&env)
            }
            "end" => {
                let _ = self.parse_raw_group();
                Node::Row(Vec::new())
            }
            "not" => {
                let next = self.parse_arg();
                Node::Negated(Box::new(next))
            }
            "quad" => Node::Space(2),
            "qquad" => Node::Space(4),
            "hfill" => Node::Space(2),
            "," | ":" | " " => Node::Space(1),
            ";" => Node::Space(2),
            "!" => Node::Space(0),
            "newline" | "cr" => Node::LineBreak,
            "mod" => Node::Text {
                text: " mod ".to_string(),
                font: Font::default(),
            },
            "bmod" => Node::Row(vec![
                Node::Space(1),
                Node::Text {
                    text: "mod".to_string(),
                    font: Font::default(),
                },
                Node::Space(1),
            ]),
            "pmod" => {
                let arg = self.parse_arg();
                Node::Row(vec![
                    Node::Space(1),
                    Node::Text {
                        text: "(mod ".to_string(),
                        font: Font::default(),
                    },
                    arg,
                    Node::Text {
                        text: ")".to_string(),
                        font: Font::default(),
                    },
                ])
            }
            "boxed" | "fbox" => Node::Boxed(Box::new(self.parse_arg())),
            "phantom" | "hphantom" | "vphantom" => {
                let _ = self.parse_arg();
                Node::Space(1)
            }
            "hspace" | "vspace" | "kern" | "mkern" | "mskip" | "hskip" => {
                let _ = self.parse_arg();
                Node::Space(1)
            }
            "color" | "rowcolor" | "cellcolor" => {
                let _ = self.parse_arg();
                Node::Row(Vec::new())
            }
            "textcolor" | "colorbox" => {
                let _ = self.parse_arg();
                self.parse_arg()
            }
            "label" | "tag" | "ref" | "eqref" | "cline" => {
                let _ = self.parse_arg();
                Node::Row(Vec::new())
            }
            "nonumber" | "notag" | "displaystyle" | "textstyle" | "scriptstyle"
            | "scriptscriptstyle" | "limits" | "nolimits" | "hline" | "noalign" | "mathstrut"
            | "strut" | "selectfont" => Node::Row(Vec::new()),
            "mathchoice" => {
                let first = self.parse_arg();
                let _ = self.parse_arg();
                let _ = self.parse_arg();
                let _ = self.parse_arg();
                first
            }
            "genfrac" => {
                let _ = self.parse_arg();
                let _ = self.parse_arg();
                let _ = self.parse_arg();
                let _ = self.parse_arg();
                let numerator = self.parse_arg();
                let denominator = self.parse_arg();
                Node::Fraction(Box::new(numerator), Box::new(denominator))
            }
            "{" | "}" | "$" | "%" | "&" | "#" | "_" | "[" | "]" | "(" | ")" => Node::Atom {
                text: command.to_string(),
                kind: AtomKind::Normal,
                font: Font::default(),
            },
            "|" => Node::Atom {
                text: "‖".to_string(),
                kind: AtomKind::Normal,
                font: Font::default(),
            },
            _ => {
                if let Some(text) = big_operator(command) {
                    return Node::Atom {
                        text: text.to_string(),
                        kind: AtomKind::Operator {
                            limits: !matches!(command, "int" | "iint" | "iiint" | "oint"),
                        },
                        font: Font::default(),
                    };
                }
                if let Some(text) = math_function(command) {
                    return Node::Atom {
                        text: text.to_string(),
                        kind: AtomKind::Function {
                            limits: matches!(
                                command,
                                "lim"
                                    | "liminf"
                                    | "limsup"
                                    | "max"
                                    | "min"
                                    | "sup"
                                    | "inf"
                                    | "det"
                                    | "gcd"
                                    | "Pr"
                                    | "argmax"
                                    | "argmin"
                            ),
                        },
                        font: Font::default(),
                    };
                }
                if let Some(text) = symbol(command) {
                    return Node::Atom {
                        text: text.to_string(),
                        kind: AtomKind::Normal,
                        font: Font::default(),
                    };
                }
                Node::Atom {
                    text: command.to_string(),
                    kind: AtomKind::Normal,
                    font: Font::default(),
                }
            }
        }
    }
}

fn push_space(nodes: &mut Vec<Node>, width: usize) {
    if width == 0 {
        return;
    }
    if let Some(Node::Space(previous)) = nodes.last_mut() {
        *previous = (*previous).max(width);
    } else {
        nodes.push(Node::Space(width));
    }
}

fn push_node(nodes: &mut Vec<Node>, node: Node) {
    match node {
        Node::Space(width) => push_space(nodes, width),
        node => nodes.push(node),
    }
}

fn attach_script(nodes: &mut Vec<Node>, sup: Option<Box<Node>>, sub: Option<Box<Node>>) {
    let base = nodes.pop().unwrap_or(Node::Row(Vec::new()));
    match base {
        Node::Script {
            base,
            sup: mut old_sup,
            sub: mut old_sub,
        } => {
            if sup.is_some() {
                old_sup = sup;
            }
            if sub.is_some() {
                old_sub = sub;
            }
            nodes.push(Node::Script {
                base,
                sup: old_sup,
                sub: old_sub,
            });
        }
        other => nodes.push(Node::Script {
            base: Box::new(other),
            sup,
            sub,
        }),
    }
}

fn apply_font(node: Node, font: Font) -> Node {
    match node {
        Node::Atom {
            text,
            kind,
            font: existing,
        } => Node::Atom {
            text,
            kind,
            font: existing.merge(font),
        },
        Node::Text {
            text,
            font: existing,
        } => Node::Text {
            text,
            font: existing.merge(font),
        },
        Node::Row(nodes) => Node::Row(
            nodes
                .into_iter()
                .map(|node| apply_font(node, font))
                .collect(),
        ),
        Node::Fraction(numerator, denominator) => Node::Fraction(
            Box::new(apply_font(*numerator, font)),
            Box::new(apply_font(*denominator, font)),
        ),
        Node::Sqrt { body, index } => Node::Sqrt {
            body: Box::new(apply_font(*body, font)),
            index: index.map(|node| Box::new(apply_font(*node, font))),
        },
        Node::Script { base, sup, sub } => Node::Script {
            base: Box::new(apply_font(*base, font)),
            sup: sup.map(|node| Box::new(apply_font(*node, font))),
            sub: sub.map(|node| Box::new(apply_font(*node, font))),
        },
        Node::Delimited { left, right, body } => Node::Delimited {
            left,
            right,
            body: Box::new(apply_font(*body, font)),
        },
        Node::Accent { kind, body } => Node::Accent {
            kind,
            body: Box::new(apply_font(*body, font)),
        },
        Node::Matrix { env, spec, rows } => Node::Matrix {
            env,
            spec,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(|node| apply_font(node, font)).collect())
                .collect(),
        },
        Node::OverUnder { over, under, body } => Node::OverUnder {
            over: over.map(|node| Box::new(apply_font(*node, font))),
            under: under.map(|node| Box::new(apply_font(*node, font))),
            body: Box::new(apply_font(*body, font)),
        },
        Node::Binom(numerator, denominator) => Node::Binom(
            Box::new(apply_font(*numerator, font)),
            Box::new(apply_font(*denominator, font)),
        ),
        Node::Negated(body) => Node::Negated(Box::new(apply_font(*body, font))),
        Node::Boxed(body) => Node::Boxed(Box::new(apply_font(*body, font))),
        other @ (Node::Space(_) | Node::LineBreak) => other,
    }
}

fn unescape_text(text: &str) -> String {
    text.replace("\\{", "{")
        .replace("\\}", "}")
        .replace("\\$", "$")
        .replace("\\%", "%")
        .replace("\\&", "&")
        .replace("\\#", "#")
        .replace("\\_", "_")
        .replace("\\\\", "\n")
}

fn big_operator(command: &str) -> Option<&'static str> {
    Some(match command {
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "bigoplus" => "⨁",
        "bigotimes" => "⨂",
        "biguplus" => "⨄",
        "bigsqcup" => "⨆",
        "bigvee" => "⋁",
        "bigwedge" => "⋀",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        _ => return None,
    })
}

fn math_function(command: &str) -> Option<&'static str> {
    Some(match command {
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "cot" => "cot",
        "sec" => "sec",
        "csc" => "csc",
        "sinh" => "sinh",
        "cosh" => "cosh",
        "tanh" => "tanh",
        "coth" => "coth",
        "arcsin" => "arcsin",
        "arccos" => "arccos",
        "arctan" => "arctan",
        "log" => "log",
        "ln" => "ln",
        "lg" => "lg",
        "exp" => "exp",
        "lim" => "lim",
        "liminf" => "lim inf",
        "limsup" => "lim sup",
        "max" => "max",
        "min" => "min",
        "sup" => "sup",
        "inf" => "inf",
        "det" => "det",
        "dim" => "dim",
        "ker" => "ker",
        "hom" => "hom",
        "arg" => "arg",
        "deg" => "deg",
        "gcd" => "gcd",
        "Pr" => "Pr",
        "sgn" => "sgn",
        "tr" => "tr",
        "rank" => "rank",
        "diag" => "diag",
        "erf" => "erf",
        "lcm" => "lcm",
        "argmax" => "arg max",
        "argmin" => "arg min",
        _ => return None,
    })
}

#[derive(Clone)]
struct Cell {
    text: String,
    width: usize,
    style: Style,
}

impl Cell {
    fn new(text: impl Into<String>, style: Style) -> Self {
        let text = text.into();
        let width = UnicodeWidthStr::width(text.as_str());
        Self { text, width, style }
    }

    fn blank(style: Style) -> Self {
        Self {
            text: " ".to_string(),
            width: 1,
            style,
        }
    }
}

#[derive(Clone, Default)]
struct MBox {
    rows: Vec<Vec<Cell>>,
    baseline: usize,
    width: usize,
}

impl MBox {
    fn empty() -> Self {
        Self::default()
    }

    fn from_text(text: &str, style: Style) -> Self {
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for line in text.split('\n') {
            let mut row: Vec<Cell> = Vec::new();
            for ch in line.chars() {
                let width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if width == 0 {
                    if let Some(last) = row.last_mut() {
                        last.text.push(ch);
                    } else {
                        row.push(Cell {
                            text: ch.to_string(),
                            width: 0,
                            style,
                        });
                    }
                } else {
                    row.push(Cell {
                        text: ch.to_string(),
                        width,
                        style,
                    });
                }
            }
            rows.push(row);
        }
        let width = rows.iter().map(|row| row_width(row)).max().unwrap_or(0);
        for row in &mut rows {
            pad_row(row, width, style);
        }
        Self {
            rows,
            baseline: 0,
            width,
        }
    }

    fn space(width: usize, style: Style) -> Self {
        Self {
            rows: vec![blank_row(width, style)],
            baseline: 0,
            width,
        }
    }
}

fn row_width(row: &[Cell]) -> usize {
    row.iter().map(|cell| cell.width).sum()
}

fn blank_row(width: usize, style: Style) -> Vec<Cell> {
    (0..width).map(|_| Cell::blank(style)).collect()
}

fn pad_row(row: &mut Vec<Cell>, width: usize, style: Style) {
    let mut used = row_width(row);
    while used < width {
        row.push(Cell::blank(style));
        used += 1;
    }
}

fn center_rows(rows: &[Vec<Cell>], width: usize, style: Style) -> Vec<Vec<Cell>> {
    rows.iter()
        .map(|row| {
            let pad = width.saturating_sub(row_width(row)) / 2;
            let mut out = Vec::with_capacity(row.len() + pad);
            for _ in 0..pad {
                out.push(Cell::blank(style));
            }
            out.extend(row.iter().cloned());
            pad_row(&mut out, width, style);
            out
        })
        .collect()
}

fn hcat(parts: Vec<MBox>, gap: usize, style: Style) -> MBox {
    let parts: Vec<MBox> = parts
        .into_iter()
        .filter(|part| !(part.rows.is_empty() && part.width == 0))
        .collect();
    if parts.is_empty() {
        return MBox::empty();
    }
    let baseline = parts.iter().map(|part| part.baseline).max().unwrap_or(0);
    let height = parts
        .iter()
        .map(|part| baseline - part.baseline + part.rows.len())
        .max()
        .unwrap_or(0);
    let width =
        parts.iter().map(|part| part.width).sum::<usize>() + gap * parts.len().saturating_sub(1);
    let mut rows = Vec::with_capacity(height);
    for line in 0..height {
        let mut row = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                row.extend(blank_row(gap, style));
            }
            let top = baseline - part.baseline;
            if line >= top && line < top + part.rows.len() {
                row.extend(part.rows[line - top].iter().cloned());
            } else {
                row.extend(blank_row(part.width, style));
            }
        }
        pad_row(&mut row, width, style);
        rows.push(row);
    }
    MBox {
        rows,
        baseline,
        width,
    }
}

fn vcat(parts: Vec<MBox>, gap: usize, style: Style) -> MBox {
    let parts: Vec<MBox> = parts
        .into_iter()
        .filter(|part| !(part.rows.is_empty() && part.width == 0))
        .collect();
    if parts.is_empty() {
        return MBox::empty();
    }
    let width = parts.iter().map(|part| part.width).max().unwrap_or(0);
    let baseline = parts[0].baseline;
    let mut rows = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            rows.extend((0..gap).map(|_| blank_row(width, style)));
        }
        for row in &part.rows {
            let mut row = row.clone();
            pad_row(&mut row, width, style);
            rows.push(row);
        }
    }
    MBox {
        rows,
        baseline,
        width,
    }
}

fn layout(node: &Node, style: Style) -> MBox {
    match node {
        Node::Row(nodes) => {
            let mut groups: Vec<Vec<MBox>> = vec![Vec::new()];
            let mut previous: Option<SpaceClass> = None;
            let mut pending = 0usize;
            for node in nodes {
                match node {
                    Node::LineBreak => {
                        groups.push(Vec::new());
                        previous = None;
                        pending = 0;
                    }
                    Node::Space(width) => pending = pending.max(*width),
                    node => {
                        let gap = pending.max(spacing(previous, node));
                        let group = groups.last_mut().expect("row group");
                        if gap > 0 {
                            group.push(MBox::space(gap, style));
                        }
                        group.push(layout(node, style));
                        pending = 0;
                        previous = Some(effective_class(space_class(node), previous));
                    }
                }
            }
            let boxes: Vec<MBox> = groups
                .into_iter()
                .map(|group| hcat(group, 0, style))
                .collect();
            if boxes.len() == 1 {
                boxes.into_iter().next().unwrap_or_else(MBox::empty)
            } else {
                vcat(boxes, 0, style)
            }
        }
        Node::Atom { text, font, .. } | Node::Text { text, font } => {
            MBox::from_text(text, font.style(style))
        }
        Node::Space(width) => MBox::space(*width, style),
        Node::LineBreak => MBox::empty(),
        Node::Fraction(numerator, denominator) => {
            layout_fraction(layout(numerator, style), layout(denominator, style), style)
        }
        Node::Sqrt { body, index } => layout_sqrt(
            index.as_deref().map(|node| layout(node, style)),
            layout(body, style),
            style,
        ),
        Node::Script { base, sup, sub } => {
            layout_script(base, sup.as_deref(), sub.as_deref(), style)
        }
        Node::Delimited { left, right, body } => {
            let body = layout(body, style);
            wrap_delimited(left, right, body, style)
        }
        Node::Accent { kind, body } => layout_accent(*kind, layout(body, style), style),
        Node::Matrix { env, spec, rows } => layout_matrix(env, spec, rows, style),
        Node::OverUnder { over, under, body } => layout_over_under(
            over.as_deref().map(|node| layout(node, style)),
            under.as_deref().map(|node| layout(node, style)),
            layout(body, style),
            style,
        ),
        Node::Binom(numerator, denominator) => {
            let numerator = layout(numerator, style);
            let denominator = layout(denominator, style);
            layout_binom(numerator, denominator, style)
        }
        Node::Boxed(body) => layout_boxed(layout(body, style), style),
        Node::Negated(body) => {
            if let Some(text) = plain_text(body) {
                if let Some(negated) = negate_text(&text) {
                    return MBox::from_text(&negated, style);
                }
            }
            hcat(
                vec![MBox::from_text("¬", style), layout(body, style)],
                0,
                style,
            )
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpaceClass {
    Ordinary,
    Operator,
    Function,
    Binary,
    Relation,
    Punctuation,
    Open,
    Close,
}

/// TeX-style inter-atom spacing on a character grid. Explicit source spaces
/// are kept by the caller; this only adds the minimum LaTeX would insert.
fn spacing(left: Option<SpaceClass>, next: &Node) -> usize {
    let right = effective_class(space_class(next), left);
    let Some(left) = left else {
        return 0;
    };
    if right == SpaceClass::Punctuation || right == SpaceClass::Close {
        return 0;
    }
    if left == SpaceClass::Open || right == SpaceClass::Open {
        return 0;
    }
    if left == SpaceClass::Function || left == SpaceClass::Operator {
        return 1;
    }
    if left == SpaceClass::Punctuation {
        return 1;
    }
    if matches!(
        (left, right),
        (SpaceClass::Binary | SpaceClass::Relation, _)
            | (_, SpaceClass::Binary | SpaceClass::Relation)
    ) {
        return 1;
    }
    0
}

/// A binary operator with no left operand is unary ("-x", "(-1)"), so it
/// keeps its ordinary spacing instead of opening a gap on both sides.
fn effective_class(class: SpaceClass, left: Option<SpaceClass>) -> SpaceClass {
    if class == SpaceClass::Binary
        && matches!(
            left,
            None | Some(
                SpaceClass::Open
                    | SpaceClass::Binary
                    | SpaceClass::Relation
                    | SpaceClass::Punctuation
                    | SpaceClass::Function
                    | SpaceClass::Operator
            )
        )
    {
        SpaceClass::Ordinary
    } else {
        class
    }
}

fn space_class(node: &Node) -> SpaceClass {
    match node {
        Node::Atom {
            kind: AtomKind::Operator { .. },
            ..
        } => SpaceClass::Operator,
        Node::Atom {
            kind: AtomKind::Function { .. },
            ..
        } => SpaceClass::Function,
        Node::Atom { text, .. } | Node::Text { text, .. } => text_class(text),
        Node::Script { base, .. } => space_class(base),
        Node::Negated(body) => space_class(body),
        _ => SpaceClass::Ordinary,
    }
}

fn text_class(text: &str) -> SpaceClass {
    match text.trim() {
        "," | ";" | ":" => SpaceClass::Punctuation,
        "(" | "[" | "{" | "⟨" | "⌈" | "⌊" => SpaceClass::Open,
        ")" | "]" | "}" | "⟩" | "⌉" | "⌋" => SpaceClass::Close,
        "=" | "<" | ">" | "≤" | "≥" | "≠" | "≈" | "≡" | "∼" | "≅" | "≃" | "≍" | "∝" | "⊂" | "⊃"
        | "⊆" | "⊇" | "∈" | "∉" | "∋" | "→" | "←" | "↔" | "⇒" | "⇐" | "⇔" | "↦" | "≪" | "≫"
        | "≺" | "≻" | "⪯" | "⪰" | "⊥" | "∥" | "∣" => SpaceClass::Relation,
        "+" | "-" | "−" | "±" | "∓" | "×" | "÷" | "·" | "∗" | "⋆" | "∘" | "•" | "∪" | "∩" | "∖"
        | "⊕" | "⊖" | "⊗" | "⊙" | "∧" | "∨" => SpaceClass::Binary,
        _ => SpaceClass::Ordinary,
    }
}

fn layout_fraction(numerator: MBox, denominator: MBox, style: Style) -> MBox {
    let width = numerator.width.max(denominator.width) + 2;
    let mut rows = center_rows(&numerator.rows, width, style);
    rows.push(vec![Cell::new("─".repeat(width), style)]);
    rows.extend(center_rows(&denominator.rows, width, style));
    let baseline = numerator.rows.len();
    MBox {
        rows,
        baseline,
        width,
    }
}

fn layout_sqrt(index: Option<MBox>, body: MBox, style: Style) -> MBox {
    if body.rows.is_empty() {
        return MBox::from_text("√", style);
    }
    let index_height = index.as_ref().map_or(0, |part| part.rows.len());
    let index_width = index.as_ref().map_or(0, |part| part.width);
    let width = index_width + body.width + 1;
    let mut rows = Vec::new();
    if let Some(index) = &index {
        for row in &index.rows {
            let mut line = row.clone();
            pad_row(&mut line, width, style);
            rows.push(line);
        }
    }
    let mut top = blank_row(index_width + 1, style);
    top.push(Cell::new("─".repeat(body.width), style));
    pad_row(&mut top, width, style);
    rows.push(top);
    for (line_index, row) in body.rows.iter().enumerate() {
        let prefix = if line_index + 1 == body.rows.len() {
            "√"
        } else {
            "│"
        };
        let mut line = blank_row(index_width, style);
        line.push(Cell::new(prefix, style));
        line.extend(row.iter().cloned());
        pad_row(&mut line, width, style);
        rows.push(line);
    }
    MBox {
        rows,
        baseline: index_height + body.baseline + 1,
        width,
    }
}

fn layout_script(base: &Node, sup: Option<&Node>, sub: Option<&Node>, style: Style) -> MBox {
    let limits = matches!(
        base,
        Node::Atom {
            kind: AtomKind::Operator { limits: true },
            ..
        } | Node::Atom {
            kind: AtomKind::Function { limits: true },
            ..
        }
    );
    let base = layout(base, style);
    let sup = sup.map(|node| layout(node, style));
    let sub = sub.map(|node| layout(node, style));
    if limits {
        layout_limits(base, sup, sub, style)
    } else {
        layout_side_script(base, sup, sub, style)
    }
}

fn layout_side_script(base: MBox, sup: Option<MBox>, sub: Option<MBox>, style: Style) -> MBox {
    let sup_height = sup.as_ref().map_or(0, |part| part.rows.len());
    let sub_height = sub.as_ref().map_or(0, |part| part.rows.len());
    if sup_height == 0 && sub_height == 0 {
        return base;
    }
    let base_height = base.rows.len();
    let script_width = sup
        .as_ref()
        .map_or(0, |part| part.width)
        .max(sub.as_ref().map_or(0, |part| part.width));
    let mut rows = Vec::new();
    if let Some(sup) = &sup {
        rows.extend(center_rows(&sup.rows, script_width, style));
    }
    rows.extend((0..base_height).map(|_| blank_row(script_width, style)));
    if let Some(sub) = &sub {
        rows.extend(center_rows(&sub.rows, script_width, style));
    }
    let baseline = sup_height + base.baseline;
    let script = MBox {
        rows,
        baseline,
        width: script_width,
    };
    hcat(vec![base, script], usize::from(base_height > 1), style)
}

fn layout_limits(base: MBox, sup: Option<MBox>, sub: Option<MBox>, style: Style) -> MBox {
    let width = base
        .width
        .max(sup.as_ref().map_or(0, |part| part.width))
        .max(sub.as_ref().map_or(0, |part| part.width));
    let mut rows = Vec::new();
    if let Some(sup) = &sup {
        rows.extend(center_rows(&sup.rows, width, style));
    }
    let base_top = rows.len();
    rows.extend(center_rows(&base.rows, width, style));
    if let Some(sub) = &sub {
        rows.extend(center_rows(&sub.rows, width, style));
    }
    MBox {
        rows,
        baseline: base_top + base.baseline,
        width,
    }
}

fn wrap_delimited(left: &str, right: &str, body: MBox, style: Style) -> MBox {
    if body.rows.is_empty() {
        return body;
    }
    let height = body.rows.len();
    let left_parts = delimiter_parts(left, height, true);
    let right_parts = delimiter_parts(right, height, false);
    let left = column_box(&left_parts, style, body.baseline);
    let right = column_box(&right_parts, style, body.baseline);
    hcat(vec![left, body, right], 1, style)
}

fn column_box(parts: &[String], style: Style, baseline: usize) -> MBox {
    if parts.is_empty() || parts.iter().all(String::is_empty) {
        return MBox::empty();
    }
    let width = parts
        .iter()
        .map(|part| UnicodeWidthStr::width(part.as_str()))
        .max()
        .unwrap_or(0);
    let rows = parts
        .iter()
        .map(|part| {
            let mut row = Vec::new();
            if !part.is_empty() {
                row.push(Cell::new(part.clone(), style));
            }
            pad_row(&mut row, width, style);
            row
        })
        .collect();
    MBox {
        rows,
        baseline,
        width,
    }
}

fn delimiter_parts(delimiter: &str, height: usize, left: bool) -> Vec<String> {
    if height <= 1 {
        return vec![delimiter.to_string(); height];
    }
    let (top, middle, bottom) = match (delimiter, left) {
        ("(" | ")", true) => ("⎛", "⎜", "⎝"),
        ("(" | ")", false) => ("⎞", "⎟", "⎠"),
        ("[" | "]", true) => ("⎡", "⎢", "⎣"),
        ("[" | "]", false) => ("⎤", "⎥", "⎦"),
        ("{" | "}", true) => ("⎧", "⎨", "⎩"),
        ("{" | "}", false) => ("⎫", "⎬", "⎭"),
        ("|", _) => ("│", "│", "│"),
        ("‖", _) => ("║", "║", "║"),
        ("⌊", true) => ("⎢", "⎢", "⎣"),
        ("⌋", false) => ("⎥", "⎥", "⎦"),
        ("⌈", true) => ("⎡", "⎢", "⎣"),
        ("⌉", false) => ("⎤", "⎥", "⎦"),
        _ => return (0..height).map(|_| delimiter.to_string()).collect(),
    };
    if height == 2 {
        return vec![top.to_string(), bottom.to_string()];
    }
    let mut parts = Vec::with_capacity(height);
    parts.push(top.to_string());
    for _ in 1..height - 1 {
        parts.push(middle.to_string());
    }
    parts.push(bottom.to_string());
    parts
}

fn layout_accent(kind: AccentKind, body: MBox, style: Style) -> MBox {
    if body.rows.is_empty() {
        return body;
    }
    match kind {
        AccentKind::Overline => accent_row(body, style, "─", true),
        AccentKind::Underline => accent_row(body, style, "─", false),
        AccentKind::OverBrace => accent_row(body, style, "⏞", true),
        AccentKind::UnderBrace => accent_row(body, style, "⏟", false),
        AccentKind::Hat
        | AccentKind::Bar
        | AccentKind::Vec
        | AccentKind::Tilde
        | AccentKind::Dot
        | AccentKind::Ddot => {
            if body.rows.len() == 1 && body.rows[0].len() == 1 {
                let combining = match kind {
                    AccentKind::Hat => '\u{0302}',
                    AccentKind::Bar => '\u{0304}',
                    AccentKind::Vec => '\u{20d7}',
                    AccentKind::Tilde => '\u{0303}',
                    AccentKind::Dot => '\u{0307}',
                    AccentKind::Ddot => '\u{0308}',
                    _ => unreachable!(),
                };
                let mut body = body;
                body.rows[0][0].text.push(combining);
                return body;
            }
            let mark = match kind {
                AccentKind::Hat => "^",
                AccentKind::Bar => "─",
                AccentKind::Vec => "→",
                AccentKind::Tilde => "~",
                AccentKind::Dot => "·",
                AccentKind::Ddot => "··",
                _ => unreachable!(),
            };
            let mut rows = vec![center_text_row(mark, body.width.max(1), style)];
            rows.extend(body.rows);
            MBox {
                rows,
                baseline: body.baseline + 1,
                width: body.width.max(1),
            }
        }
    }
}

fn layout_boxed(body: MBox, style: Style) -> MBox {
    if body.rows.is_empty() {
        return MBox::empty();
    }
    let inner = body.width.max(1);
    let mut rows = vec![vec![Cell::new(format!("┌{}┐", "─".repeat(inner)), style)]];
    for row in &body.rows {
        let mut line = vec![Cell::new("│", style)];
        let mut row = row.clone();
        pad_row(&mut row, inner, style);
        line.extend(row);
        line.push(Cell::new("│", style));
        rows.push(line);
    }
    rows.push(vec![Cell::new(format!("└{}┘", "─".repeat(inner)), style)]);
    MBox {
        rows,
        baseline: body.baseline + 1,
        width: inner + 2,
    }
}

fn accent_row(mut body: MBox, style: Style, mark: &str, over: bool) -> MBox {
    let bar = vec![Cell::new(mark.repeat(body.width.max(1)), style)];
    if over {
        let mut rows = vec![bar];
        rows.extend(body.rows);
        MBox {
            rows,
            baseline: body.baseline + 1,
            width: body.width.max(1),
        }
    } else {
        body.rows.push(bar);
        body
    }
}

fn center_text_row(text: &str, width: usize, style: Style) -> Vec<Cell> {
    let text_width = UnicodeWidthStr::width(text);
    let pad = width.saturating_sub(text_width) / 2;
    let mut row = Vec::new();
    for _ in 0..pad {
        row.push(Cell::blank(style));
    }
    row.push(Cell::new(text, style));
    pad_row(&mut row, width, style);
    row
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

fn column_aligns(env: &str, spec: &str, columns: usize) -> Vec<Align> {
    let env = env.trim_end_matches('*');
    (0..columns)
        .map(|index| match env {
            "cases" => Align::Left,
            "aligned" | "align" | "split" | "alignedat" | "alignat" => {
                if index % 2 == 0 {
                    Align::Right
                } else {
                    Align::Left
                }
            }
            "array" | "tabular" => match spec
                .chars()
                .filter(|ch| matches!(ch, 'l' | 'c' | 'r'))
                .nth(index)
            {
                Some('l') => Align::Left,
                Some('r') => Align::Right,
                _ => Align::Center,
            },
            _ => Align::Center,
        })
        .collect()
}

fn layout_matrix(env: &str, spec: &str, rows: &[Vec<Node>], style: Style) -> MBox {
    if rows.is_empty() {
        return MBox::empty();
    }
    let cells: Vec<Vec<MBox>> = rows
        .iter()
        .map(|row| row.iter().map(|node| layout(node, style)).collect())
        .collect();
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return MBox::empty();
    }
    let mut widths = vec![0usize; columns];
    for row in &cells {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.width);
        }
    }
    let aligns = column_aligns(env, spec, columns);
    let gap = if matches!(
        env.trim_end_matches('*'),
        "matrix" | "pmatrix" | "bmatrix" | "Bmatrix" | "vmatrix" | "Vmatrix" | "smallmatrix"
    ) {
        2
    } else {
        1
    };
    let total_width = widths.iter().sum::<usize>() + gap * columns.saturating_sub(1);
    let mut row_boxes = Vec::new();
    for row in &cells {
        let row_baseline = row.iter().map(|cell| cell.baseline).max().unwrap_or(0);
        let row_height = row
            .iter()
            .map(|cell| row_baseline - cell.baseline + cell.rows.len())
            .max()
            .unwrap_or(1)
            .max(1);
        let mut lines = Vec::new();
        for line in 0..row_height {
            let mut output = Vec::new();
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    output.extend(blank_row(gap, style));
                }
                let Some(cell) = row.get(index) else {
                    output.extend(blank_row(*width, style));
                    continue;
                };
                let top = row_baseline - cell.baseline;
                let mut piece = if line >= top && line < top + cell.rows.len() {
                    cell.rows[line - top].clone()
                } else {
                    Vec::new()
                };
                let free = width.saturating_sub(row_width(&piece));
                let pad = match aligns.get(index).copied().unwrap_or(Align::Center) {
                    Align::Left => 0,
                    Align::Right => free,
                    Align::Center => free / 2,
                };
                let mut aligned = Vec::new();
                for _ in 0..pad {
                    aligned.push(Cell::blank(style));
                }
                aligned.append(&mut piece);
                pad_row(&mut aligned, *width, style);
                output.extend(aligned);
            }
            pad_row(&mut output, total_width, style);
            lines.push(output);
        }
        row_boxes.push(MBox {
            rows: lines,
            baseline: row_baseline,
            width: total_width,
        });
    }
    let mut body = vcat(row_boxes, 0, style);
    body.baseline = body.rows.len() / 2;
    let (left, right) = match env.trim_end_matches('*') {
        "pmatrix" => ("(", ")"),
        "bmatrix" => ("[", "]"),
        "Bmatrix" => ("{", "}"),
        "vmatrix" => ("|", "|"),
        "Vmatrix" => ("‖", "‖"),
        "cases" => ("{", ""),
        _ => ("", ""),
    };
    if left.is_empty() && right.is_empty() {
        body
    } else {
        wrap_delimited(left, right, body, style)
    }
}

fn layout_over_under(over: Option<MBox>, under: Option<MBox>, body: MBox, style: Style) -> MBox {
    let width = body
        .width
        .max(over.as_ref().map_or(0, |part| part.width))
        .max(under.as_ref().map_or(0, |part| part.width));
    let mut rows = Vec::new();
    if let Some(over) = &over {
        rows.extend(center_rows(&over.rows, width, style));
    }
    let body_top = rows.len();
    rows.extend(center_rows(&body.rows, width, style));
    if let Some(under) = &under {
        rows.extend(center_rows(&under.rows, width, style));
    }
    MBox {
        rows,
        baseline: body_top + body.baseline,
        width,
    }
}

fn layout_binom(numerator: MBox, denominator: MBox, style: Style) -> MBox {
    let width = numerator.width.max(denominator.width) + 2;
    let mut rows = center_rows(&numerator.rows, width, style);
    rows.extend(center_rows(&denominator.rows, width, style));
    let body = MBox {
        rows,
        baseline: numerator.rows.len(),
        width,
    };
    wrap_delimited("(", ")", body, style)
}

fn plain_text(node: &Node) -> Option<String> {
    match node {
        Node::Atom { text, .. } | Node::Text { text, .. } => Some(text.clone()),
        Node::Row(nodes) if nodes.len() == 1 => plain_text(&nodes[0]),
        _ => None,
    }
}

fn negate_text(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(match first {
        '=' => "≠".to_string(),
        '<' => "≮".to_string(),
        '>' => "≯".to_string(),
        '∈' => "∉".to_string(),
        '∋' => "∌".to_string(),
        '⊂' => "⊄".to_string(),
        '⊃' => "⊅".to_string(),
        '⊆' => "⊈".to_string(),
        '⊇' => "⊉".to_string(),
        '≈' => "≉".to_string(),
        '≡' => "≢".to_string(),
        '≤' => "≰".to_string(),
        '≥' => "≱".to_string(),
        '∼' => "≁".to_string(),
        other => {
            let mut s = String::new();
            s.push(other);
            s.push('\u{0338}');
            s
        }
    })
}

/// A single-line, script-free rendering used when a real image is not
/// available, such as a terminal without Kitty/Sixel/iTerm2 support. Math
/// stays readable instead of reverting to the literal `$...$` source.
pub(super) fn compact(source: &str) -> String {
    flatten(&Parser::new(source).parse())
}

pub fn display(source: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let ast = Parser::new(source).parse();
    let mut layout = layout(&ast, style);
    if layout.rows.is_empty() {
        return Vec::new();
    }
    if layout.width > width {
        let text = flatten(&ast);
        let mut rows = Vec::new();
        for line in wrap_plain(&text, width) {
            rows.push(center_text_row(&line, width, style));
        }
        return rows_to_lines(rows, style);
    }
    for row in &mut layout.rows {
        let pad = width.saturating_sub(row_width(row)) / 2;
        if pad == 0 {
            continue;
        }
        let mut centered = Vec::with_capacity(row.len() + pad);
        for _ in 0..pad {
            centered.push(Cell::blank(style));
        }
        centered.extend(row.iter().cloned());
        *row = centered;
    }
    rows_to_lines(layout.rows, style)
}

fn rows_to_lines(rows: Vec<Vec<Cell>>, default_style: Style) -> Vec<Line<'static>> {
    rows.into_iter()
        .map(|mut row| {
            while row
                .last()
                .is_some_and(|cell| cell.text.chars().all(char::is_whitespace))
            {
                row.pop();
            }
            let mut spans = Vec::new();
            let mut text = String::new();
            let mut style: Option<Style> = None;
            for cell in row {
                if style != Some(cell.style) {
                    if !text.is_empty() {
                        spans.push(Span::styled(
                            std::mem::take(&mut text),
                            style.unwrap_or(default_style),
                        ));
                    }
                    style = Some(cell.style);
                }
                text.push_str(&cell.text);
            }
            if !text.is_empty() {
                spans.push(Span::styled(text, style.unwrap_or(default_style)));
            }
            Line::from(spans)
        })
        .collect()
}

fn flatten(node: &Node) -> String {
    match node {
        Node::Row(nodes) => nodes.iter().map(flatten).collect(),
        Node::Atom { text, .. } | Node::Text { text, .. } => text.clone(),
        Node::Space(width) => " ".repeat(*width),
        Node::LineBreak => " ".to_string(),
        Node::Fraction(numerator, denominator) => {
            let numerator = flatten(numerator);
            let denominator = flatten(denominator);
            let numerator = if needs_paren(&numerator) {
                format!("({numerator})")
            } else {
                numerator
            };
            let denominator = if needs_paren(&denominator) {
                format!("({denominator})")
            } else {
                denominator
            };
            format!("{numerator}/{denominator}")
        }
        Node::Sqrt { body, index } => {
            let body = flatten(body);
            let body = if needs_paren(&body) {
                format!("({body})")
            } else {
                body
            };
            match index {
                Some(index) => format!("{}√{body}", flatten(index)),
                None => format!("√{body}"),
            }
        }
        Node::Script { base, sup, sub } => {
            let base = flatten(base);
            let simple = |node: &Node| {
                let text = flatten(node);
                if text.chars().count() <= 1 || text.chars().all(|ch| ch.is_alphanumeric()) {
                    text
                } else {
                    format!("({text})")
                }
            };
            let sup = sup.as_deref().map(|node| format!("^{}", simple(node)));
            let sub = sub.as_deref().map(|node| format!("_{}", simple(node)));
            let mut out = base;
            if let Some(sup) = sup {
                out.push_str(&sup);
            }
            if let Some(sub) = sub {
                out.push_str(&sub);
            }
            out
        }
        Node::Delimited { left, right, body } => {
            format!("{left}{}{right}", flatten(body))
        }
        Node::Accent { body, .. } => flatten(body),
        Node::Matrix { env, rows, .. } => {
            let (left, right) = match env.trim_end_matches('*') {
                "pmatrix" => ("(", ")"),
                "bmatrix" => ("[", "]"),
                "Bmatrix" => ("{", "}"),
                "vmatrix" => ("|", "|"),
                "Vmatrix" => ("‖", "‖"),
                "cases" => ("{", ""),
                _ => ("", ""),
            };
            let rows = rows
                .iter()
                .map(|row| row.iter().map(flatten).collect::<Vec<_>>().join(", "))
                .collect::<Vec<_>>()
                .join("; ");
            format!("{left}{rows}{right}")
        }
        Node::OverUnder { over, under, body } => {
            let mut out = flatten(body);
            if let Some(over) = over {
                out.push_str(&format!("^({})", flatten(over)));
            }
            if let Some(under) = under {
                out.push_str(&format!("_({})", flatten(under)));
            }
            out
        }
        Node::Binom(numerator, denominator) => {
            format!("C({},{})", flatten(numerator), flatten(denominator))
        }
        Node::Boxed(body) => format!("[{}]", flatten(body)),
        Node::Negated(body) => {
            if let Some(text) = plain_text(body) {
                if let Some(negated) = negate_text(&text) {
                    negated
                } else {
                    format!("¬{}", flatten(body))
                }
            } else {
                format!("¬{}", flatten(body))
            }
        }
    }
}

fn needs_paren(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch,
            '+' | '-'
                | '−'
                | '='
                | '<'
                | '>'
                | '≤'
                | '≥'
                | '≠'
                | '±'
                | '∓'
                | '*'
                | '·'
                | '×'
                | '÷'
                | '/'
                | '⁄'
                | ','
                | ';'
        )
    })
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut used = 0usize;
        for ch in line.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + ch_width > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(ch);
            used += ch_width;
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str, width: usize) -> String {
        display(source, width, Style::default())
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn trimmed(source: &str, width: usize) -> String {
        render(source, width)
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    #[test]
    fn spacing_follows_tex_atom_classes() {
        assert_eq!(trimmed("a+b=c", 40), "a + b = c");
        assert_eq!(trimmed("x=-1", 40), "x = -1");
        assert_eq!(trimmed("-x", 40), "-x");
        assert_eq!(trimmed("(-1)", 40), "(-1)");
        assert_eq!(trimmed("\\sin x", 40), "sin x");
        assert_eq!(trimmed("\\sin(x)", 40), "sin(x)");
        assert_eq!(trimmed("f(x, y)", 40), "f(x, y)");
    }

    #[test]
    fn font_commands_add_modifiers() {
        let lines = display("\\mathbf{v} + \\mathit{x}", 40, Style::default());
        let mut bold = false;
        let mut italic = false;
        for span in lines.iter().flat_map(|line| line.spans.iter()) {
            match span.content.as_ref() {
                "v" => bold = span.style.add_modifier.contains(Modifier::BOLD),
                "x" => italic = span.style.add_modifier.contains(Modifier::ITALIC),
                _ => {}
            }
        }
        assert!(bold, "\\mathbf did not bold its argument");
        assert!(italic, "\\mathit did not italicise its argument");
    }

    #[test]
    fn indexed_root_puts_the_index_above_the_radical() {
        let output = render("\\sqrt[3]{x}", 20);
        let lines: Vec<&str> = output.lines().map(str::trim).collect();
        assert_eq!(lines.first().copied(), Some("3"), "{output}");
        assert!(output.contains("√x"), "{output}");
    }

    #[test]
    fn array_spec_controls_column_alignment() {
        let output = render("\\begin{array}{lr} a & b \\\\ ccc & d \\end{array}", 40);
        assert!(output.contains("a   b"), "{output}");
        assert!(output.contains("ccc d"), "{output}");
    }

    #[test]
    fn scripts_on_tall_bases_stack_above_and_below() {
        let matrix = render("\\begin{pmatrix}1&2\\\\3&4\\end{pmatrix}^{-1}", 40);
        assert!(!matrix.contains("⁻¹"), "{matrix}");
        assert!(
            matrix
                .lines()
                .next()
                .is_some_and(|line| line.contains("-1")),
            "{matrix}"
        );

        let bracket = render("\\left[\\frac{a}{b}\\right]_k", 40);
        assert!(!bracket.contains("ₖ"), "{bracket}");
        assert!(
            bracket
                .lines()
                .last()
                .is_some_and(|line| line.contains('k')),
            "{bracket}"
        );
    }

    #[test]
    fn display_scripts_do_not_use_unicode_super_or_subscripts() {
        let output = render("x^2 + y_1", 40);
        assert!(!output.contains('²'), "{output}");
        assert!(!output.contains('₁'), "{output}");
        assert!(output.lines().count() > 1, "{output}");
    }

    #[test]
    fn boxed_and_braced_expressions_get_frames() {
        let boxed = render("\\boxed{x+1}", 20);
        assert!(boxed.contains("┌─────┐"), "{boxed}");
        assert!(boxed.contains("│x + 1│"), "{boxed}");
        assert!(boxed.contains("└─────┘"), "{boxed}");
        let brace = render("\\overbrace{a+b}^{n}", 30);
        assert!(brace.contains('⏞'), "{brace}");
    }

    #[test]
    fn extended_symbols_and_functions_render() {
        let output = render(
            "\\forall x \\in \\mathbb{R}: \\nabla f(x) = \\argmax_{y} g(y)",
            80,
        );
        for needle in ["∀", "∈", "ℝ", "∇", "arg max"] {
            assert!(output.contains(needle), "{needle}\n{output}");
        }
    }
}
