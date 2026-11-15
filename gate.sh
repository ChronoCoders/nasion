#!/usr/bin/env bash
# The project gate. Runs every step, keeps going after a failure, prints one
# PASS or FAIL line per step and exits non-zero if any step failed.
set -u
cd "$(dirname "$0")" || exit 2
# shellcheck source=scripts/checks.sh
source scripts/checks.sh

VENV=.venv
export VIRTUAL_ENV="$PWD/$VENV"
# uv syncs this same directory, whatever the environment says.
export UV_PROJECT_ENVIRONMENT="$VIRTUAL_ENV"
failed=0
last_status=0
python_env_status=2

# On failure the step output follows the FAIL line, indented.
step() {
    local name=$1 output status note
    shift
    output=$("$@" 2>&1)
    status=$?
    last_status=$status
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
    if ((python_env_status != 0)); then
        echo "The python env step failed, so $VENV is not used. Fix that step first."
        return 2
    fi
    if [[ ! -x $VENV/bin/$tool ]]; then
        echo "$VENV/bin/$tool not found, although the python env step passed. Check that the dev group in pyproject.toml lists it."
        return 2
    fi
    "$VENV/bin/$tool" "$@"
}

# shellcheck disable=SC2329 # Called through step, which shellcheck does not follow.
python_tests() {
    # Without --quiet, because --quiet also hides cargo's warnings.
    fail_on_warnings venv_tool maturin develop --locked --uv || return
    venv_tool pytest -q
}

# Audits uv.lock, exported with hashes and without the local package, which
# is not on PyPI and would otherwise fail the strict audit (D28).
# shellcheck disable=SC2329 # Called through step, which shellcheck does not follow.
python_audit() {
    local requirements status
    if ! requirements=$(mktemp); then
        echo "mktemp failed, so the lock cannot be exported. Check that the temporary directory is writable."
        return 2
    fi
    if ! export_lock >"$requirements"; then
        echo "uv export --locked failed, so the lock cannot be audited."
        rm -f "$requirements"
        return 2
    fi
    venv_tool pip-audit --strict --disable-pip --require-hashes -r "$requirements"
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

# First, because the cargo steps build nasion-py, whose PyO3 build script runs
# the interpreter in .venv.
step "python env" check_python_env
python_env_status=$last_status
step "cargo fmt" fail_on_warnings cargo fmt --all --check
step "cargo clippy" check_clippy
step "cargo test" fail_on_warnings cargo test --locked --workspace
step "miri" check_unsafe_allow
step "deny unsafe" check_deny_unsafe
step "cargo audit" fail_on_warnings cargo audit --deny warnings --file Cargo.lock
step "ruff check" venv_tool ruff check python
step "ruff format" venv_tool ruff format --check python
step "mypy" venv_tool mypy --strict python
step "pytest" python_tests
step "pip-audit" python_audit
step "venv matches lock" check_venv_matches_lock
step "dash sweep" check_dashes
step "instruction files" check_instruction_paths git ls-files -z
step "banned names" banned_names
step "manifest hash" check_manifest
step "tree unchanged" check_tree_unchanged "$tree_before"

exit "$failed"
