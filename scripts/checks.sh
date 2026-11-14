#!/usr/bin/env bash
# Check library shared by gate.sh and the hooks in .githooks/.
#
# Each check_* function prints what it found and returns 0 when clean, 1 on a
# violation and 2 when the check itself could not run. A check that cannot run
# never reports clean. A line starting with "note: " is a remark the gate shows
# next to PASS.
#
# The dash characters and the banned names are built from hex at runtime, so
# this file holds none of them literally and the checks never match themselves.

EN_DASH=$(printf '\xe2\x80\x93')
EM_DASH=$(printf '\xe2\x80\x94')
BANNED_PRODUCT=$(printf '\x63\x6c\x61\x75\x64\x65')
BANNED_VENDOR=$(printf '\x61\x6e\x74\x68\x72\x6f\x70\x69\x63')

# Agent instruction files and the project documents that live outside this
# repository. Matched case-insensitively against every path component.
INSTRUCTION_PATH_RE="(^|/)(${BANNED_PRODUCT}\.md|agents?\.md|gemini\.md|\.cursorrules|\.windsurfrules|\.clinerules|\.goosehints|\.roomodes|\.aider[^/]*|spec\.md|decisions\.md|status\.md|prior_work\.md|project-kickoff[^/]*)$"
INSTRUCTION_PATH_RE+="|(^|/)\.(${BANNED_PRODUCT}|cursor|windsurf|gemini|codex|clinerules|roo|kiro|junie|amazonq|augment|continue)/"
INSTRUCTION_PATH_RE+="|(^|/)\.github/(copilot-instructions\.md|instructions/|prompts/)"

CONVENTIONAL_PREFIX_RE='^(feat|fix|docs|test|refactor|chore): [^[:space:]]'
CLEANUP_VERB_RE='^[a-z]+: (fix|clean|clean up|cleanup|tidy|remove|replace|strip|normali[sz]e|correct|run|apply|trim|drop|convert|swap)( |$)'
CLEANUP_NOUN_RE='(^|[^a-z])(formatting|reformat|fmt|rustfmt|whitespace|trailing spaces?|indentation|punctuation|dash|dashes|hyphens?|line endings?)([^a-z]|$)'
FORMAT_ONLY_RE='^[a-z]+: (re)?format( |$)'

# Turns a search status into a check status. grep and git grep exit 0 on a
# match, 1 on no match and above 1 on an error, so a bare negation would turn
# an error into a pass.
search_result() {
    local status=$1 what=$2
    case $status in
        0) return 1 ;;
        1) return 0 ;;
        *)
            echo "$what failed to run (exit $status). Check that git and grep work in this directory."
            return 2
            ;;
    esac
}

# Arguments go to git grep. Pass --cached to search staged content.
check_dashes() {
    LC_ALL=C git grep -n -F -e "$EN_DASH" -e "$EM_DASH" "$@"
    search_result $? "dash search"
}

# Arguments go to git grep. Pass --cached to search staged content.
check_banned_content() {
    LC_ALL=C git grep -n -i -F -e "$BANNED_PRODUCT" -e "$BANNED_VENDOR" "$@"
    search_result $? "banned name search in file contents"
}

# Arguments are a command that prints NUL-separated paths.
check_banned_paths() {
    "$@" | LC_ALL=C grep -z -i -F -e "$BANNED_PRODUCT" -e "$BANNED_VENDOR" | tr '\0' '\n'
    local statuses=("${PIPESTATUS[@]}")
    if ((statuses[0] != 0)); then
        echo "Listing paths with \"$*\" failed (exit ${statuses[0]})."
        return 2
    fi
    search_result "${statuses[1]}" "banned name search in paths"
}

check_banned_messages() {
    local log
    if ! log=$(git log --format='commit %H%n%B'); then
        echo "git log failed. Check that this repository has at least one commit."
        return 2
    fi
    LC_ALL=C grep -i -F -e "$BANNED_PRODUCT" -e "$BANNED_VENDOR" <<<"$log"
    search_result $? "banned name search in commit messages"
}

# Arguments are a command that prints NUL-separated paths.
check_instruction_paths() {
    "$@" | LC_ALL=C grep -z -i -E -e "$INSTRUCTION_PATH_RE" | tr '\0' '\n'
    local statuses=("${PIPESTATUS[@]}")
    if ((statuses[0] != 0)); then
        echo "Listing paths with \"$*\" failed (exit ${statuses[0]})."
        return 2
    fi
    search_result "${statuses[1]}" "instruction file search"
}

# Miri is not set up yet, so no unsafe code may compile in the workspace.
# Forbidding unsafe_code on the command line turns every opt-out into a compile
# error wherever it comes from: an untracked file, a #[path] module or a macro.
# Registry dependencies build with --cap-lints, so the forbid reaches only
# workspace members and path dependencies. A separate target directory keeps
# the normal build cache intact. Returns 2 when cargo fails for another reason.
check_unsafe_allow() {
    local output status
    output=$(RUSTFLAGS="-F unsafe_code" CARGO_TARGET_DIR=target/forbid-check \
        cargo check --locked --workspace --all-targets --all-features 2>&1)
    status=$?
    if ((status == 0)); then
        echo "note: no unsafe"
        return 0
    fi
    echo "$output"
    LC_ALL=C grep -q -F 'unsafe_code' <<<"$output"
    case $? in
        0)
            echo "Miri is not set up. Record a decision that pins its nightly toolchain and add it to the gate."
            return 1
            ;;
        1)
            echo "cargo check failed (exit $status) for a reason other than unsafe code. Fix the error above."
            return 2
            ;;
        *)
            echo "Searching the cargo check output for unsafe_code failed. Check that grep works."
            return 2
            ;;
    esac
}

# Prints one line for each workspace member whose Cargo.toml does not set
# [lints] workspace = true, read from cargo metadata on stdin.
LINTS_INHERIT_PY='
import json, sys, tomllib
for package in json.load(sys.stdin)["packages"]:
    with open(package["manifest_path"], "rb") as manifest:
        lints = tomllib.load(manifest).get("lints", {})
    if not (isinstance(lints, dict) and lints.get("workspace") is True):
        print(package["manifest_path"], "does not set [lints] workspace = true")
'

# Prints one line for each Cargo.toml on disk that is neither the workspace
# root manifest nor the manifest of a workspace member, read from cargo metadata
# on stdin. target/ and .venv/ hold build output and .git/ holds no sources, so
# all three are skipped at the root. Exits 0 clean, 1 stray found, 2 failed.
STRAY_MANIFESTS_PY='
import json, os, sys

SKIPPED = {"target", ".venv", ".git"}

# os.walk silently skips a directory it cannot read unless onerror raises.
def raise_error(error):
    raise error

try:
    metadata = json.load(sys.stdin)
    root = metadata["workspace_root"]
    known = {os.path.join(root, "Cargo.toml")}
    known.update(package["manifest_path"] for package in metadata["packages"])
    stray = []
    for directory, subdirectories, files in os.walk(root, onerror=raise_error):
        if directory == root:
            subdirectories[:] = [d for d in subdirectories if d not in SKIPPED]
        manifest = os.path.join(directory, "Cargo.toml")
        if "Cargo.toml" in files and manifest not in known:
            stray.append(manifest)
except (OSError, KeyError, TypeError, ValueError) as error:
    print("Listing Cargo.toml files or reading cargo metadata failed:", error)
    sys.exit(2)
for manifest in sorted(stray):
    print(manifest, "is not the manifest of a workspace member")
sys.exit(1 if stray else 0)
'

# Every target root of every workspace member must hold a crate-level
# #![deny(unsafe_code)], and every member must inherit [workspace.lints].
# The command line forbid in check_unsafe_allow lasts only until Miri is set
# up, so this check keeps the attribute itself in place as SPEC Section 11
# requires. nasion-xtask checks the attribute by parsing each
# target root, so text in a comment or a string never counts. Members and
# targets come from cargo metadata, never from a fixed list, and every
# Cargo.toml on disk must belong to a member so none escapes that list.
check_deny_unsafe() {
    local meta missing stray status
    if ! meta=$(cargo metadata --no-deps --format-version 1); then
        echo "cargo metadata failed. Check that every Cargo.toml in the workspace parses and that every directory under crates/ holds a Cargo.toml."
        return 2
    fi
    if ! missing=$(python3 -I -c "$LINTS_INHERIT_PY" <<<"$meta"); then
        echo "Reading cargo metadata or a member Cargo.toml failed. Check the output above."
        return 2
    fi
    stray=$(python3 -I -c "$STRAY_MANIFESTS_PY" <<<"$meta")
    status=$?
    if ((status > 1)); then
        echo "$stray"
        return 2
    fi
    missing+=${stray:+${missing:+$'\n'}$stray}
    cargo run --locked --quiet -p nasion-xtask
    status=$?
    if ((status > 1)); then
        echo "nasion-xtask could not check the target roots (exit $status). Check the output above."
        return 2
    fi
    if [[ -n $missing ]]; then
        echo "$missing"
        return 1
    fi
    return "$status"
}

# Records the work tree, or compares it with a recorded state on stdin. The
# state is the output of git status --porcelain=v1 -z --untracked-files=all
# plus a SHA-256 of every tracked file and every untracked file that is not
# ignored, as it is on disk. Ignored paths such as target/ and .venv/ stay out
# because build steps write there by design. Exits 0 unchanged, 1 changed
# (naming each path), 2 when git fails or the state cannot be read.
TREE_STATE_PY='
import hashlib, json, os, subprocess, sys

def git(*args):
    result = subprocess.run(["git", *args], capture_output=True, check=False)
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise RuntimeError("git " + " ".join(args) + " failed: " + detail)
    return [entry.decode(errors="surrogateescape") for entry in result.stdout.split(b"\0") if entry]

def content(path):
    if os.path.islink(path):
        return "link:" + os.readlink(path)
    if os.path.isdir(path):
        return "directory"
    if not os.path.exists(path):
        return "missing"
    with open(path, "rb") as tracked_file:
        return "sha256:" + hashlib.sha256(tracked_file.read()).hexdigest()

def snapshot():
    status = git("status", "--porcelain=v1", "-z", "--untracked-files=all")
    paths = git("ls-files", "-z") + git("ls-files", "-z", "--others", "--exclude-standard")
    return {"status": sorted(status), "content": {path: content(path) for path in paths}}

# A status record is "XY path". The source path of a rename comes as its own
# record without the XY prefix.
def status_path(record):
    return record[3:] if record[2:3] == " " else record

try:
    if sys.argv[1] == "snapshot":
        print(json.dumps(snapshot()))
        sys.exit(0)
    before = json.loads(sys.stdin.read())
    after = snapshot()
    changed = {status_path(r) for r in set(before["status"]) ^ set(after["status"])}
    for path in before["content"].keys() | after["content"].keys():
        if before["content"].get(path) != after["content"].get(path):
            changed.add(path)
except (OSError, RuntimeError, KeyError, TypeError, ValueError) as error:
    print("Recording the work tree state failed:", error)
    sys.exit(2)
for path in sorted(changed):
    print(path, "changed, appeared or disappeared while the gate ran")
sys.exit(1 if changed else 0)
'

# The argument is the state recorded before the first gate step.
check_tree_unchanged() {
    local status
    python3 -I -c "$TREE_STATE_PY" compare <<<"$1"
    status=$?
    if ((status > 2)); then
        echo "python3 failed to compare the work tree state (exit $status)."
        return 2
    fi
    return "$status"
}

# Runs a cargo command and also fails when cargo warns that a member does not
# inherit [workspace.lints]. cargo prints that as a plain warning, which
# -D warnings does not turn into an error.
check_cargo_lints_warning() {
    local output status
    output=$("$@" 2>&1)
    status=$?
    echo "$output"
    LC_ALL=C grep -F "missing \`[lints]\` to inherit \`[workspace.lints]\`" <<<"$output" >/dev/null
    search_result $? "search for the cargo [lints] warning" || {
        local search=$?
        ((search == 1)) && echo "A member does not inherit [workspace.lints]. Add [lints] workspace = true to its Cargo.toml."
        return "$search"
    }
    return "$status"
}

# The dataset manifest does not exist yet, so neither does its hash check. A
# manifest that appears before the check is written fails the gate.
check_manifest() {
    local tracked found
    if ! tracked=$(git ls-files); then
        echo "git ls-files failed. Check that this is a git work tree."
        return 2
    fi
    found=$(grep -i 'manifest' <<<"$tracked")
    if [[ -z $found ]]; then
        echo "note: no manifest yet"
        return 0
    fi
    echo "$found"
    echo "A manifest is tracked but the manifest hash check is not written yet."
    return 1
}

check_msg_single_line() {
    if [[ -z $1 ]]; then
        echo "Commit message is empty."
        return 1
    fi
    if [[ $1 == *$'\n'* ]]; then
        echo "Commit message has more than one line. Use one line with no body."
        return 1
    fi
}

check_msg_prefix() {
    local subject=${1%%$'\n'*}
    if [[ ! $subject =~ $CONVENTIONAL_PREFIX_RE ]]; then
        echo "Subject \"$subject\" must start with feat, fix, docs, test, refactor or chore, then \": \" and text."
        return 1
    fi
}

check_msg_trailers() {
    local rest
    [[ $1 == *$'\n'* ]] || return 0
    rest=${1#*$'\n'}
    grep -E '^[A-Za-z][A-Za-z0-9-]*:( |$)' <<<"$rest"
    search_result $? "trailer search in the commit message" || {
        local status=$?
        ((status == 1)) && echo "Commit message has a trailer line. Remove it."
        return "$status"
    }
}

check_msg_banned() {
    LC_ALL=C grep -i -F -e "$BANNED_PRODUCT" -e "$BANNED_VENDOR" <<<"$1"
    search_result $? "banned name search in the commit message" || {
        local status=$?
        ((status == 1)) && echo "Commit message names a banned tool or vendor."
        return "$status"
    }
}

check_msg_dashes() {
    LC_ALL=C grep -F -e "$EN_DASH" -e "$EM_DASH" <<<"$1"
    search_result $? "dash search in the commit message" || {
        local status=$?
        ((status == 1)) && echo "Commit message has an em dash or en dash."
        return "$status"
    }
}

# A subject that only describes a formatting, dash, punctuation or whitespace
# cleanup. Such fixes ride inside a commit that changes the file for a real
# reason.
check_msg_cleanup() {
    local subject
    subject=$(tr '[:upper:]' '[:lower:]' <<<"${1%%$'\n'*}")
    if [[ $subject =~ $FORMAT_ONLY_RE ]] || { [[ $subject =~ $CLEANUP_VERB_RE ]] && [[ $subject =~ $CLEANUP_NOUN_RE ]]; }; then
        echo "Subject \"$subject\" only describes a formatting, dash, punctuation or whitespace cleanup."
        return 1
    fi
}
