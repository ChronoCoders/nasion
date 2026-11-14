#!/usr/bin/env bash
# The project gate. Runs every step, keeps going after a failure, prints one
# PASS or FAIL line per step and exits non-zero if any step failed.
set -u
cd "$(dirname "$0")" || exit 2
# shellcheck source=scripts/checks.sh
source scripts/checks.sh

VENV=.venv
export VIRTUAL_ENV="$PWD/$VENV"
failed=0

# On failure the step output follows the FAIL line, indented.
step() {
    local name=$1 output status note
    shift
    output=$("$@" 2>&1)
    status=$?
    note=$(grep -m1 '^note: ' <<<"$output" | sed 's/^note: //')
    if ((status == 0)); then
        echo "PASS $name${note:+ ($note)}"
        return
    fi
    echo "FAIL $name (exit $status)"
    printf '    %s\n' "${output//$'\n'/$'\n    '}"
    failed=1
}

# shellcheck disable=SC2329 # Called through step, which shellcheck does not follow.
venv_tool() {
    local tool=$1
    shift
    if [[ ! -x $VENV/bin/$tool ]]; then
        echo "$VENV/bin/$tool not found. Create the venv and install the dev group from pyproject.toml."
        return 2
    fi
    "$VENV/bin/$tool" "$@"
}

# shellcheck disable=SC2329 # Called through step, which shellcheck does not follow.
python_tests() {
    venv_tool maturin develop --locked --uv --quiet || return
    venv_tool pytest -q
}

# Audits the frozen venv without the local editable nasion package, which is
# not on PyPI and would otherwise fail the strict audit.
# shellcheck disable=SC2329 # Called through step, which shellcheck does not follow.
python_audit() {
    local requirements status
    requirements=$(mktemp) || return 2
    if ! venv_tool python -m pip freeze --exclude-editable >"$requirements"; then
        rm -f "$requirements"
        return 2
    fi
    venv_tool pip-audit --strict --no-deps --disable-pip -r "$requirements"
    status=$?
    rm -f "$requirements"
    return "$status"
}

# shellcheck disable=SC2329 # Called through step, which shellcheck does not follow.
banned_names() {
    local status=0
    check_banned_content -- || status=$?
    check_banned_paths git ls-files -z || status=$?
    check_banned_messages || status=$?
    return "$status"
}

# Taken before the first step so the last step can show the gate changed
# nothing it checked.
tree_before=$(python3 -I -c "$TREE_STATE_PY" snapshot 2>&1)

step "cargo fmt" cargo fmt --all --check
step "cargo clippy" check_cargo_lints_warning cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
step "cargo test" cargo test --locked --workspace
step "miri" check_unsafe_allow
step "deny unsafe" check_deny_unsafe
step "cargo audit" cargo audit --deny warnings --file Cargo.lock
step "ruff check" venv_tool ruff check python
step "ruff format" venv_tool ruff format --check python
step "mypy" venv_tool mypy --strict python
step "pytest" python_tests
step "pip-audit" python_audit
step "dash sweep" check_dashes
step "instruction files" check_instruction_paths git ls-files -z
step "banned names" banned_names
step "manifest hash" check_manifest
step "tree unchanged" check_tree_unchanged "$tree_before"

exit "$failed"
