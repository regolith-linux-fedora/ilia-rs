#!/bin/sh
# Usage: calculator-helper.sh <ilia-calculator>
#
# Works out expressions with the built calculator helper, written as the
# Calculator page writes them (rust/src/calculator/expression.rs), and
# compares its replies (rust/src/calculator/answer.rs reads them).
set -u
helper=$1
failures=0

# The helper wants the pid of its parent: `exec` keeps the pid of the
# `sh -c` whose $PPID is passed.
work_out() {
    printf '%s' "$1" | sh -c 'exec "$0" "$PPID"' "$helper"
}

check() {
    expected=$1
    expression=$2
    reply=$(work_out "$expression")
    if [ "$reply" = "$expected" ]; then
        echo "ok: $expression => $reply"
    else
        echo "FAIL: $expression => '$reply', expected '$expected'"
        failures=$((failures + 1))
    fi
}

LC_ALL=C.UTF-8
export LC_ALL

# Exact where possible, with the first 40 digits cut off, not rounded.
check 'value + 1428571428571428571428571428571428571428 -1 part .' '1/7'
check 'value + 1267650600228229401496703205376 30 whole .' '2^(100)'
check 'value + 3041409320171337804361260816606476884437 64 whole .' 'factorial(50)'
check 'value + 9999999999999999999999999999999999999999 39 whole .' '10^(40)-1'
check 'value + 1 40 whole .' '10^(40)'
check 'value + 3333333333333333333333333333333333333333 -8 part .' '1/(3E7)'
check 'value + 2000000000000005 0 part .' '2.000000000000005'
check 'value + 3 0 whole .' '(0.1+0.2)*10'
check 'value + 5 -1 part .' 'cos(pi/3)'
check 'value + 2 0 whole .' 'sqrt(2)*sqrt(2)'
check 'value + 3141592653589793238462643383279502884197 0 part .' 'pi'
check 'value + 2302585092994045684017991454684364207601 0 part .' 'ln(10)'
# The grouping the page spells out is the grouping worked out.
check 'value - 9 0 whole .' '-(3^(2))'
check 'value + 256 2 whole .' '2^(2^(3))'
check 'value + 12 1 whole .' '20-5-3'
check 'value + 0 0 whole .' '(-0)*3'
check 'value + 72 2 whole .' 'factorial(factorial(3))'
check 'value + 1 0 whole .' 'factorial(0)'

# No real, finite value.
check none '5/0'
check none '0^(-1)'
check none 'sqrt(-4)'
check none 'ln(0)'
check none 'tan(pi/2)'
check none '1/sin(pi)'
check none 'factorial(-3)'
check none 'factorial(1.5)'
# A part with no value leaves the whole expression without one, even where
# the rest of it would cancel that part before simplification.
check none '0*factorial(1.5)'
check none 'factorial(-3)^(0)'
check none 'factorial(1.5)-factorial(1.5)'
check none 'factorial(-1)*0'
check none '0*tan(pi/2)'
check none '0*sqrt(-4)'
check none '5/0*0'
check none 'ln(0)-ln(0)'
# The same shapes with parts that have a value keep it.
check 'value + 1 0 whole .' 'factorial(3)^(0)'
check 'value + 0 0 whole .' '0*factorial(5)'
check 'value + 0 0 whole .' 'factorial(2.0)-factorial(2)'
check 'value + 0 0 whole .' '0*tan(pi/4)'

# The decimal mark comes from LC_ALL, else LC_NUMERIC; the arithmetic is
# the same in every locale.
if locale -a 2>/dev/null | grep -Eqi '^de_DE\.utf-?8$'; then
    LC_ALL=de_DE.UTF-8
    check 'value + 125 -1 part ,' '1/8'
    check 'value + 5 0 whole ,' '2.5*2'
    unset LC_ALL
    LC_NUMERIC=de_DE.UTF-8 LANG=C.UTF-8
    export LC_NUMERIC LANG
    check 'value + 125 -1 part ,' '1/8'
    LC_ALL=C.UTF-8
    export LC_ALL
    check 'value + 125 -1 part .' '1/8'
    unset LC_NUMERIC
    LC_ALL=C.UTF-8 LC_MESSAGES=de_DE.UTF-8
    export LC_MESSAGES
    check 'value + 125 -1 part .' '1/8'
    unset LC_MESSAGES
else
    echo "skip: no de_DE.UTF-8 locale for the decimal mark cases"
fi
LC_ALL=C.UTF-8

# A helper whose parent is not the launcher it names exits at once.
reply=$(printf '1' | "$helper" 1)
status=$?
if [ "$status" -eq 1 ] && [ -z "$reply" ]; then
    echo "ok: wrong parent exits 1 with no reply"
else
    echo "FAIL: wrong parent: exit $status, reply '$reply'"
    failures=$((failures + 1))
fi

[ "$failures" -eq 0 ]
