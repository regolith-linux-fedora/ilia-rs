// ilia-calculator: works out one expression for ilia's Calculator page with
// libqalculate.
//
// ilia starts this helper for every expression it works out, and kills it
// when the text changes or the answer takes too long: libqalculate cannot
// interrupt every calculation (a large factorial runs to the end), but a
// process can always be ended. See rust/src/calculator/evaluation.rs.
//
// Usage: ilia-calculator <launcher-pid>
//
// The expression is read from standard input, as ilia writes it: numbers
// with a full stop and an optional E exponent, + - * / ^, brackets, and the
// functions and constants the page accepts. One line is written to
// standard output:
//
//   value <+|-> <digits> <exponent> <whole|part> <decimal-mark>
//
// for a real, finite value: its first 40 significant digits, cut off rather
// than rounded (fewer when the value has fewer), the power of ten of the
// first digit, whether it is an exact whole number, and the decimal mark of
// the locale's numbers, which ilia shows the value with; or
//
//   none
//
// when the expression has no such value. That includes every expression with
// a part that has none, such as a factorial of a negative or fractional
// number, a division by zero, or the square root of a negative number, even
// where the rest of the expression would cancel that part: 0*(1.5!) and
// (-3)!^0 have no value. See settle().

#include <libqalculate/qalculate.h>

#include <clocale>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <iostream>
#include <iterator>
#include <string>

#include <sys/prctl.h>
#include <unistd.h>

namespace {

// Significant digits written; ilia shows whole numbers up to this long in
// full and rounds everything else to fewer.
const long DIGITS = 40;
// Digits libqalculate keeps for values it cannot work out exactly: more than
// are written, so that the written ones are right.
const int WORKING_PRECISION = 60;

// 10^exponent, exactly.
Number power_of_ten(long exponent) {
    return Number(1, 1, exponent);
}

// The decimal mark of the locale's numbers as the environment sets them up
// (LC_ALL, else LC_NUMERIC, else LANG), the way GTK sets up ilia's locale:
// a full stop when that locale is not available.
std::string locale_decimal_mark() {
    std::string mark;
    if (std::setlocale(LC_ALL, "")) {
        const char *point = std::localeconv()->decimal_point;
        if (point)
            mark = point;
    }
    return mark.empty() ? "." : mark;
}

// The reply for a real, finite number, or "none" when it cannot be written.
std::string reply(Number value, const std::string &decimal_mark) {
    if (value.isInterval())
        value.intervalToMidValue();
    if (value.isZero())
        return "value + 0 0 whole " + decimal_mark;

    const bool negative = value.isNegative();
    const bool whole = !value.isApproximate() && value.isInteger();
    value.abs();

    // The power of ten of the first significant digit: the floating-point
    // logarithm, corrected with exact comparisons.
    Number logarithm(value);
    if (!logarithm.log(Number(10, 1)) || !logarithm.floor())
        return "none";
    bool overflow = false;
    long exponent = logarithm.lintValue(&overflow);
    if (overflow)
        return "none";
    while (value.isLessThan(power_of_ten(exponent)))
        exponent--;
    while (value.isGreaterThanOrEqualTo(power_of_ten(exponent + 1)))
        exponent++;

    // The first DIGITS digits as a whole number, cut off.
    Number scaled(value);
    if (!scaled.multiply(power_of_ten(DIGITS - 1 - exponent)) || !scaled.trunc())
        return "none";
    PrintOptions print_options;
    print_options.min_exp = EXP_NONE;
    print_options.number_fraction_format = FRACTION_DECIMAL;
    print_options.digit_grouping = DIGIT_GROUPING_NONE;
    print_options.use_unicode_signs = false;
    print_options.interval_display = INTERVAL_DISPLAY_MIDPOINT;
    std::string digits = scaled.print(print_options);
    if (digits.size() != static_cast<size_t>(DIGITS)
        || digits.find_first_not_of("0123456789") != std::string::npos || digits[0] == '0')
        return "none";
    // Trailing zeros carry no information.
    digits.erase(digits.find_last_not_of('0') + 1);

    return std::string("value ") + (negative ? "-" : "+") + " " + digits + " "
        + std::to_string(exponent) + " " + (whole ? "whole" : "part") + " " + decimal_mark;
}

// Whether libqalculate reported an error since the last call.
bool failed(Calculator *calculator) {
    bool error = false;
    for (CalculatorMessage *message = calculator->message(); message;
         message = calculator->nextMessage())
        error = error || message->type() == MESSAGE_ERROR;
    return error;
}

// Whether an evaluated part is a real, finite value. libqalculate leaves what
// it cannot work out in place, unevaluated: a factorial of 1.5 or of -3, a
// division by zero (0^-1), tan(pi/2). None of those is a value, and neither
// is a complex number (sqrt(-4)) or an infinity (ln(0)).
bool is_value(const MathStructure &value, Calculator *calculator) {
    if (value.isNumber()) {
        const Number &number = value.number();
        return number.isReal() && !number.includesInfinity() && !number.isUndefined();
    }
    return value.representsReal() && !value.containsInfinity(false, true, true)
        && !value.containsFunction(calculator->f_factorial);
}

// Whether a part of this kind can lack a value although its operands have
// one: a factorial or another function, or a power (a division is a power
// of -1).
bool can_fail(const MathStructure &part) {
    return part.isFunction() || part.isPower() || part.isInverse() || part.isDivision();
}

// Whether `part`, given the values of its operands, has a value whenever they
// all have one: sin, cos, exp and abs, and powers to an exact whole exponent
// of at least 0.
bool always_has_value(const MathStructure &part, const MathStructure &operands,
                      Calculator *calculator) {
    if (part.isFunction()) {
        const MathFunction *function = part.function();
        return function == calculator->f_sin || function == calculator->f_cos
            || function == calculator->f_exp || function == calculator->f_abs;
    }
    if (part.isPower() && operands.size() == 2) {
        const MathStructure &exponent = operands[1];
        return exponent.isNumber() && !exponent.isApproximate() && exponent.number().isInteger()
            && !exponent.number().isNegative();
    }
    return false;
}

// Checks every part of the expression, innermost first, before the whole
// expression is worked out: libqalculate's simplification would otherwise
// remove a part that has no value, as in 0*(1.5!), (-3)!^0, (1.5!)-(1.5!) or
// 0*tan(pi/2), and give the expression a value it does not have. Returns
// false when a factorial, function or power in `part` has no real, finite
// value; otherwise sets `value` to the value of `part`.
//
// A part is worked out from the values of its operands where that is
// reliable: when every operand value is exact, or when the part has a value
// whatever its operands are. Otherwise it is worked out again as written,
// exactly: from the approximate value of pi, sin(pi) is not quite 0, and
// 1/sin(pi) would wrongly get a value. An exact number replaces the part it
// is the value of, so that a large factorial is worked out once; other values
// are left out of the expression, which is still worked out exactly where
// possible: sqrt(2)*sqrt(2) is exactly 2.
bool settle(MathStructure &part, MathStructure &value, Calculator *calculator,
            const EvaluationOptions &options) {
    if (part.size() == 0) {
        value = part;
        return true;
    }
    MathStructure quick(part);
    bool exact = true;
    for (size_t index = 0; index < part.size(); index++) {
        if (!settle(part[index], quick[index], calculator, options))
            return false;
        exact = exact && !quick[index].isApproximate();
    }
    part.childrenUpdated();
    quick.childrenUpdated();
    const bool checked = can_fail(part) && !always_has_value(part, quick, calculator);
    if (exact || !checked) {
        quick.eval(options);
        if (!failed(calculator) && (!checked || is_value(quick, calculator))) {
            value = quick;
            if (value.isNumber() && !value.isApproximate())
                part = value;
            return true;
        }
    }
    value = part;
    value.eval(options);
    if (failed(calculator) || (can_fail(part) && !is_value(value, calculator)))
        return false;
    if (value.isNumber() && !value.isApproximate())
        part = value;
    return true;
}

}  // namespace

int main(int argc, char **argv) {
    if (argc != 2) {
        std::fprintf(stderr, "Usage: %s <launcher-pid>\n", argv[0]);
        return 2;
    }
    // Never outlive the launcher, which may be killed while this works.
    // Should it be gone already, the parent is someone else by now.
    prctl(PR_SET_PDEATHSIG, SIGKILL);
    if (getppid() != static_cast<pid_t>(std::strtol(argv[1], nullptr, 10)))
        return 1;

    const std::string expression((std::istreambuf_iterator<char>(std::cin)),
                                 std::istreambuf_iterator<char>());

    const std::string decimal_mark = locale_decimal_mark();
    // Numbers are read and written with a full stop: libqalculate sets
    // itself up from the environment, so set both to the C locale.
    setenv("LC_ALL", "C", 1);
    std::setlocale(LC_ALL, "C");

    // The built-in functions and constants are all that is used, so no
    // definitions are loaded.
    Calculator *calculator = new Calculator();
    calculator->useDecimalPoint(false);
    calculator->setPrecision(WORKING_PRECISION);
    // Trigonometric functions work in radians. Without the definitions the
    // radian unit is created on first use, with an error message that would
    // be taken for a failed calculation; create it beforehand.
    calculator->getRadUnit();
    calculator->clearMessages();

    EvaluationOptions options;
    // Exact where possible: cos(pi/3) is exactly 0.5, 2^100 exactly a
    // 31-digit number.
    options.approximation = APPROXIMATION_TRY_EXACT;
    options.parse_options.angle_unit = ANGLE_UNIT_RADIANS;
    options.parse_options.base = 10;

    std::string line = "none";
    MathStructure parsed;
    calculator->parse(&parsed, expression, options.parse_options);
    MathStructure value;
    if (!failed(calculator) && settle(parsed, value, calculator, options)) {
        const MathStructure result = calculator->calculate(parsed, options);
        // A division by zero stays unevaluated, a square root of a negative
        // number is complex, and ln(0) is infinite: none of them is a value.
        if (!failed(calculator) && result.isNumber() && is_value(result, calculator))
            line = reply(result.number(), decimal_mark);
    }
    std::printf("%s\n", line.c_str());
    // Skip libqalculate's clean-up: the process is done.
    std::fflush(stdout);
    std::_Exit(0);
}
