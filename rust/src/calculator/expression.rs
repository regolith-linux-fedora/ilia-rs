//! The expressions the calculator page accepts, and their translation into
//! the input of the libqalculate helper.
//!
//! The language is small and fixed: numbers (whole, fractional, or with an
//! `e` exponent, with `.` or `,` as the decimal mark), the operators `+`,
//! `-`, `*`, `/` and `^`, `!` after an operand, brackets, the functions
//! `sqrt`, `sin`, `cos`, `tan`, `ln`, `exp` and `abs`, and the constants
//! `pi` and `e`. White space is ignored.
//!
//! Text outside the language has no answer, so it is never handed to
//! libqalculate, which would read much more (units, other functions,
//! implicit multiplication). The translation brackets every operand whose
//! grouping could be read another way, so libqalculate works out exactly
//! the grouping described here:
//!
//! - `^` binds tightest and groups to the right: `2^2^3` is `2^(2^3)`;
//! - a minus sign in front applies to the whole power: `-3^2` is `-(3^2)`;
//! - `*` and `/` bind tighter than `+` and `-`;
//! - all four group to the left: `20-5-3` is `(20-5)-3`.

/// How deep brackets, function arguments and exponents may nest together.
/// Deeper text has no answer. The text is read without recursion, so any
/// depth is refused safely.
pub const MAX_DEPTH: usize = 1000;

const FUNCTIONS: &[&str] = &["sqrt", "sin", "cos", "tan", "ln", "exp", "abs"];
const CONSTANTS: &[&str] = &["pi", "e"];

/// How tightly a translated part holds together, loosest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Binding {
    Sum,
    Product,
    Negation,
    Power,
    /// A number, a constant, a function call or anything in brackets.
    Atom,
}

struct Part {
    text: String,
    binding: Binding,
}

impl Part {
    fn atom(text: String) -> Self {
        Self {
            text,
            binding: Binding::Atom,
        }
    }

    /// The text, bracketed unless it is an atom.
    fn into_operand(self) -> String {
        if self.binding == Binding::Atom {
            self.text
        } else {
            format!("({})", self.text)
        }
    }
}

/// An operator waiting for its operands, or an open bracket.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    Add,
    Subtract,
    Multiply,
    Divide,
    Power,
    Negate,
    Bracket,
    Function(&'static str),
}

impl Pending {
    /// The binding of the part the operator makes.
    fn binding(self) -> Binding {
        match self {
            Pending::Add | Pending::Subtract => Binding::Sum,
            Pending::Multiply | Pending::Divide => Binding::Product,
            Pending::Negate => Binding::Negation,
            Pending::Power => Binding::Power,
            Pending::Bracket | Pending::Function(_) => Binding::Atom,
        }
    }

    /// Whether the operator, pending before `next`, takes its operands
    /// before `next` does.
    fn applies_before(self, next: Pending) -> bool {
        match self {
            Pending::Bracket | Pending::Function(_) => false,
            // `^` groups to the right, the others to the left.
            _ if next == Pending::Power => self.binding() > Binding::Power,
            _ => self.binding() >= next.binding(),
        }
    }

    /// Whether the operator opens a level of nesting.
    fn nests(self) -> bool {
        matches!(
            self,
            Pending::Power | Pending::Bracket | Pending::Function(_)
        )
    }
}

/// The libqalculate input for `text`, or `None` when the text is not a
/// complete expression of the language, or nests more than [`MAX_DEPTH`]
/// levels deep.
pub fn translate(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    let mut reader = Reader {
        chars: &chars,
        position: 0,
    };
    let mut parts: Vec<Part> = Vec::new();
    let mut pending: Vec<Pending> = Vec::new();
    let mut depth = 0;
    // Operator-precedence parsing: operands are expected at the start and
    // after an operator or an open bracket, operators everywhere else.
    let mut expect_operand = true;
    while let Some(c) = reader.peek() {
        if expect_operand {
            match c {
                '-' => {
                    reader.position += 1;
                    // Two minus signs in a row cancel out.
                    if reader.chars.get(reader.position.wrapping_sub(2)) == Some(&'-')
                        && pending.last() == Some(&Pending::Negate)
                    {
                        pending.pop();
                    } else {
                        pending.push(Pending::Negate);
                    }
                    continue;
                }
                '(' => {
                    reader.position += 1;
                    open(&mut pending, &mut depth, Pending::Bracket)?;
                    continue;
                }
                c if c.is_ascii_digit() || is_decimal_mark(c) => parts.push(reader.number()?),
                c if c.is_ascii_alphabetic() => {
                    let name = reader.name();
                    if let Some(constant) = CONSTANTS.iter().find(|known| **known == name) {
                        parts.push(Part::atom((*constant).to_owned()));
                    } else {
                        let function = FUNCTIONS.iter().find(|known| **known == name)?;
                        if reader.peek() != Some('(') {
                            return None;
                        }
                        reader.position += 1;
                        open(&mut pending, &mut depth, Pending::Function(function))?;
                        continue;
                    }
                }
                _ => return None,
            }
            expect_operand = false;
            continue;
        }
        reader.position += 1;
        let operator = match c {
            '+' => Pending::Add,
            '-' => Pending::Subtract,
            '*' => Pending::Multiply,
            '/' => Pending::Divide,
            '^' => Pending::Power,
            '!' => {
                // Factorials apply to the operand just read.
                let mut count = 1;
                while reader.peek() == Some('!') {
                    reader.position += 1;
                    count += 1;
                }
                let operand = parts.pop()?;
                parts.push(Part::atom(format!(
                    "{}{}{}",
                    "factorial(".repeat(count),
                    operand.text,
                    ")".repeat(count)
                )));
                continue;
            }
            ')' => {
                let opened = close(&mut parts, &mut pending, &mut depth)?;
                if let Pending::Function(name) = opened {
                    let argument = parts.pop()?;
                    parts.push(Part::atom(format!("{name}({})", argument.text)));
                }
                // A bracketed part keeps its binding: whoever takes it as an
                // operand brackets it again where needed.
                continue;
            }
            _ => return None,
        };
        while pending
            .last()
            .is_some_and(|last| last.applies_before(operator))
        {
            let last = pending.pop()?;
            apply(&mut parts, &mut depth, last)?;
        }
        open(&mut pending, &mut depth, operator)?;
        expect_operand = true;
    }
    if expect_operand {
        return None;
    }
    while let Some(last) = pending.pop() {
        if matches!(last, Pending::Bracket | Pending::Function(_)) {
            return None;
        }
        apply(&mut parts, &mut depth, last)?;
    }
    let part = parts.pop()?;
    parts.is_empty().then_some(part.text)
}

/// Pushes an operator or an open bracket, counting the nesting it opens.
fn open(pending: &mut Vec<Pending>, depth: &mut usize, operator: Pending) -> Option<()> {
    if operator.nests() {
        *depth += 1;
        if *depth > MAX_DEPTH {
            return None;
        }
    }
    pending.push(operator);
    Some(())
}

/// Applies the operators back to the innermost open bracket, which it
/// removes and returns.
fn close(parts: &mut Vec<Part>, pending: &mut Vec<Pending>, depth: &mut usize) -> Option<Pending> {
    loop {
        let last = pending.pop()?;
        if matches!(last, Pending::Bracket | Pending::Function(_)) {
            *depth -= 1;
            return Some(last);
        }
        apply(parts, depth, last)?;
    }
}

/// Replaces the operands of `operator` with the part it makes.
fn apply(parts: &mut Vec<Part>, depth: &mut usize, operator: Pending) -> Option<()> {
    let right = parts.pop()?;
    let part = match operator {
        Pending::Negate => Part {
            text: format!("-{}", right.into_operand()),
            binding: Binding::Negation,
        },
        Pending::Power => {
            *depth -= 1;
            let base = parts.pop()?;
            Part {
                text: format!("{}^({})", base.into_operand(), right.text),
                binding: Binding::Power,
            }
        }
        Pending::Add => binary(parts.pop()?, '+', right, Binding::Sum),
        Pending::Subtract => binary(parts.pop()?, '-', right, Binding::Sum),
        Pending::Multiply => binary(parts.pop()?, '*', right, Binding::Product),
        Pending::Divide => binary(parts.pop()?, '/', right, Binding::Product),
        Pending::Bracket | Pending::Function(_) => return None,
    };
    parts.push(part);
    Some(())
}

struct Reader<'a> {
    chars: &'a [char],
    position: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn name(&mut self) -> String {
        let start = self.position;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.position += 1;
        }
        self.chars[start..self.position].iter().collect()
    }

    /// Digits with an optional decimal mark and an optional exponent, as in
    /// `12`, `0,5`, `.5` or `3e-7`, written for libqalculate with a full stop
    /// and an `E` exponent, which it reads exactly.
    fn number(&mut self) -> Option<Part> {
        let whole = self.digits();
        let mut fraction = String::new();
        if self.peek().is_some_and(is_decimal_mark) {
            self.position += 1;
            fraction = self.digits();
        }
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        let mut text = if whole.is_empty() {
            "0".to_owned()
        } else {
            whole
        };
        if !fraction.is_empty() {
            text.push('.');
            text.push_str(&fraction);
        }
        // An `e` not followed by an exponent is the constant, which cannot
        // follow a number directly: that is left for the caller to refuse.
        let after_e = self.position + 1;
        let signed = matches!(self.chars.get(after_e), Some('+' | '-'));
        let exponent_digits = after_e + usize::from(signed);
        let has_exponent = matches!(self.peek(), Some('e' | 'E'))
            && self
                .chars
                .get(exponent_digits)
                .is_some_and(|c| c.is_ascii_digit());
        if !has_exponent {
            return Some(Part::atom(text));
        }
        let negative = self.chars[after_e] == '-';
        self.position = exponent_digits;
        let exponent = self.digits();
        let exponent = exponent.trim_start_matches('0');
        let exponent = if exponent.is_empty() { "0" } else { exponent };
        let sign = if negative { "-" } else { "" };
        Some(Part::atom(format!("({text}E{sign}{exponent})")))
    }

    fn digits(&mut self) -> String {
        let start = self.position;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.position += 1;
        }
        self.chars[start..self.position].iter().collect()
    }
}

fn is_decimal_mark(c: char) -> bool {
    c == '.' || c == ','
}

/// `left operator right` for an operator of the given binding, which groups
/// to the left. A negation is bracketed wherever it is an operand.
fn binary(left: Part, operator: char, right: Part, binding: Binding) -> Part {
    let mut text = if left.binding < binding || left.binding == Binding::Negation {
        left.into_operand()
    } else {
        // A chain like `1+2+3` extends its text in place.
        left.text
    };
    text.push(operator);
    if right.binding <= binding || right.binding == Binding::Negation {
        text.push_str(&right.into_operand());
    } else {
        text.push_str(&right.text);
    }
    Part { text, binding }
}

#[cfg(test)]
mod tests {
    use super::{translate, MAX_DEPTH};

    fn nested(open: &str, inner: &str, close: &str, levels: usize) -> String {
        format!("{}{inner}{}", open.repeat(levels), close.repeat(levels))
    }

    #[test]
    fn grouping_is_spelled_out() {
        assert_eq!(translate("2+3*4").as_deref(), Some("2+3*4"));
        assert_eq!(translate("(2+3)*4").as_deref(), Some("(2+3)*4"));
        assert_eq!(translate("2^2^3").as_deref(), Some("2^(2^(3))"));
        assert_eq!(translate("-3^2").as_deref(), Some("-(3^(2))"));
        assert_eq!(translate("(-3)^2").as_deref(), Some("(-3)^(2)"));
        assert_eq!(translate("20-5-3").as_deref(), Some("20-5-3"));
        assert_eq!(translate("20-(5-3)").as_deref(), Some("20-(5-3)"));
        assert_eq!(translate("100/10/5").as_deref(), Some("100/10/5"));
        assert_eq!(translate("100/(10*5)").as_deref(), Some("100/(10*5)"));
        assert_eq!(translate("2^-3").as_deref(), Some("2^(-3)"));
        assert_eq!(translate("2*-3").as_deref(), Some("2*(-3)"));
        assert_eq!(translate("2--3").as_deref(), Some("2-(-3)"));
        assert_eq!(translate("--3").as_deref(), Some("3"));
        assert_eq!(translate("-0*3").as_deref(), Some("(-0)*3"));
    }

    #[test]
    fn factorials_functions_and_constants() {
        assert_eq!(translate("30!").as_deref(), Some("factorial(30)"));
        assert_eq!(translate("4!-2").as_deref(), Some("factorial(4)-2"));
        assert_eq!(translate("-3!").as_deref(), Some("-factorial(3)"));
        assert_eq!(translate("(2+1)!").as_deref(), Some("factorial(2+1)"));
        assert_eq!(translate("2^3!").as_deref(), Some("2^(factorial(3))"));
        assert_eq!(translate("cos(pi/3)").as_deref(), Some("cos(pi/3)"));
        assert_eq!(translate("2*pi").as_deref(), Some("2*pi"));
        assert_eq!(translate("e").as_deref(), Some("e"));
        assert_eq!(translate("-e^2").as_deref(), Some("-(e^(2))"));
        for function in ["sqrt", "sin", "cos", "tan", "ln", "exp", "abs"] {
            assert_eq!(
                translate(&format!("{function}(1)")),
                Some(format!("{function}(1)"))
            );
        }
    }

    #[test]
    fn parts_without_a_value_reach_the_helper_as_typed() {
        // The helper refuses these before libqalculate's simplification can
        // cancel the part with no value (test/calculator-helper.sh), so the
        // translation must keep that part, and its operand, as typed.
        assert_eq!(translate("0*(1.5!)").as_deref(), Some("0*factorial(1.5)"));
        assert_eq!(translate("0*1,5!").as_deref(), Some("0*factorial(1.5)"));
        assert_eq!(translate("(-3)!^0").as_deref(), Some("factorial(-3)^(0)"));
        assert_eq!(
            translate("(1.5!)-(1.5!)").as_deref(),
            Some("factorial(1.5)-factorial(1.5)")
        );
        assert_eq!(translate("(-1)!*0").as_deref(), Some("factorial(-1)*0"));
        assert_eq!(translate("0*tan(pi/2)").as_deref(), Some("0*tan(pi/2)"));
        assert_eq!(translate("0*sqrt(-4)").as_deref(), Some("0*sqrt(-4)"));
        assert_eq!(translate("(5/0)*0").as_deref(), Some("5/0*0"));
        // A minus sign in front negates the factorial, not its operand.
        assert_eq!(translate("-3!").as_deref(), Some("-factorial(3)"));
        assert_eq!(translate("(-3)!").as_deref(), Some("factorial(-3)"));
    }

    #[test]
    fn numbers_take_either_decimal_mark_and_an_exponent() {
        assert_eq!(translate("0.7+0.1").as_deref(), Some("0.7+0.1"));
        assert_eq!(translate("2,5*2").as_deref(), Some("2.5*2"));
        assert_eq!(translate(".5").as_deref(), Some("0.5"));
        assert_eq!(translate("5.").as_deref(), Some("5"));
        assert_eq!(translate("3e7").as_deref(), Some("(3E7)"));
        assert_eq!(translate("3e+7").as_deref(), Some("(3E7)"));
        assert_eq!(translate("2,5e-03").as_deref(), Some("(2.5E-3)"));
        assert_eq!(translate("1/3e7").as_deref(), Some("1/(3E7)"));
        assert_eq!(translate("2^1e1").as_deref(), Some("2^((1E1))"));
        assert_eq!(translate("1E0").as_deref(), Some("(1E0)"));
    }

    #[test]
    fn white_space_is_ignored() {
        assert_eq!(translate("  ( 2 + 3 ) * 4  ").as_deref(), Some("(2+3)*4"));
        assert_eq!(
            translate(" sqrt ( 9 ) + abs ( -2 ) ").as_deref(),
            Some("sqrt(9)+abs(-2)")
        );
        assert_eq!(translate("1 000").as_deref(), Some("1000"));
    }

    #[test]
    fn text_outside_the_language_has_no_translation() {
        for text in [
            "",
            "   ",
            "3*",
            "/2",
            "2+",
            "*2",
            "+2",
            "2^",
            "((2)",
            "2))",
            "()",
            "(2",
            "2)",
            "sqrt()",
            "sqrt 9",
            "y*2",
            "bar(1)",
            "3 kg",
            "factor(12)",
            "2pi",
            "pi(2)",
            "2e",
            "2e+",
            "e3",
            "Sin(1)",
            "1..2",
            ".",
            ",",
            "2(3)",
            "(2)(3)",
            "!3",
            "2**3",
            "2^^3",
            "1,2,3",
            "x",
            "2 = 2",
            "½",
            "−3",
        ] {
            assert_eq!(translate(text), None, "{text:?}");
        }
    }

    #[test]
    fn nesting_is_allowed_up_to_the_limit() {
        let fifty = nested("(", "6*7", ")", 50);
        assert_eq!(translate(&fifty).as_deref(), Some("6*7"));
        let functions = nested("abs(", "-7", ")", 50);
        assert!(translate(&functions).is_some());
        assert_eq!(
            translate(&format!("{}2", "1^".repeat(50))).map(|text| text.matches('^').count()),
            Some(50)
        );
        let mixed = format!(
            "{}{}{}2{}",
            "abs(".repeat(20),
            "(".repeat(10),
            "1^".repeat(20),
            ")".repeat(30)
        );
        assert!(translate(&mixed).is_some());
        assert!(translate(&nested("(", "1", ")", MAX_DEPTH)).is_some());
    }

    #[test]
    fn deeper_nesting_has_no_translation() {
        assert_eq!(translate(&nested("(", "1", ")", MAX_DEPTH + 1)), None);
        assert_eq!(translate(&nested("abs(", "1", ")", MAX_DEPTH + 1)), None);
        assert_eq!(translate(&nested("1^(", "2", ")", 501)), None);
        assert_eq!(translate(&format!("{}2", "1^".repeat(MAX_DEPTH + 1))), None);
        // Refused at the limit, without recursing through the rest.
        assert_eq!(translate(&nested("(", "1", ")", 200_000)), None);
    }

    #[test]
    fn long_flat_text_does_not_recurse() {
        let sum = vec!["1"; 100_000].join("+");
        assert_eq!(translate(&sum).map(|text| text.len()), Some(sum.len()));
        let minus = format!("{}3", "-".repeat(100_001));
        assert_eq!(translate(&minus).as_deref(), Some("-3"));
        let factorials = format!("3{}", "!".repeat(3));
        assert_eq!(
            translate(&factorials).as_deref(),
            Some("factorial(factorial(factorial(3)))")
        );
    }
}
