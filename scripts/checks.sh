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

# Miri is not set up yet. Any opt-out from deny(unsafe_code) means unsafe code
# exists, and that code must not pass the gate untested. Whitespace is removed
# before matching so an attribute split over lines is still found.
check_unsafe_allow() {
    local files file content found=0
    if ! files=$(git -c core.quotePath=false ls-files -- '*.rs'); then
        echo "git ls-files failed. Check that this is a git work tree."
        return 2
    fi
    while IFS= read -r file; do
        [[ -n $file ]] || continue
        if ! content=$(tr -d '[:space:]' <"$file"); then
            echo "$file is tracked but cannot be read. Restore it or remove it from the index."
            return 2
        fi
        if grep -q -E '(allow|expect)\([^)]*unsafe_code' <<<"$content"; then
            echo "$file opts out of deny(unsafe_code)"
            found=1
        fi
    done <<<"$files"
    if ((found)); then
        echo "Miri is not set up. Record a decision that pins its nightly toolchain and add it to the gate."
        return 1
    fi
    echo "note: no unsafe"
}

# Prints one line per workspace member and one per target, read from cargo
# metadata on stdin: "member<TAB>yes|no<TAB>manifest" where yes means the
# manifest sets [lints] workspace = true, and "target<TAB>package<TAB>src_path".
DENY_UNSAFE_PY='
import json, sys, tomllib
for package in json.load(sys.stdin)["packages"]:
    with open(package["manifest_path"], "rb") as manifest:
        lints = tomllib.load(manifest).get("lints", {})
    inherits = isinstance(lints, dict) and lints.get("workspace") is True
    print("member", "yes" if inherits else "no", package["manifest_path"], sep="\t")
    for target in package["targets"]:
        print("target", package["name"], target["src_path"], sep="\t")
'

# Every target root of every workspace member must hold #![deny(unsafe_code)],
# and every member must inherit [workspace.lints]. check_unsafe_allow only sees
# opt-outs, so a crate that never denied unsafe code is invisible to it. Targets
# come from cargo metadata, never from a fixed list. Line comments are removed
# and whitespace is dropped before matching, so a commented-out attribute does
# not count and one split over lines does.
check_deny_unsafe() {
    local meta records kind field path src targets=0 found=0
    if ! meta=$(cargo metadata --no-deps --format-version 1); then
        echo "cargo metadata failed. Check that every Cargo.toml in the workspace parses."
        return 2
    fi
    if ! records=$(python3 -I -c "$DENY_UNSAFE_PY" <<<"$meta"); then
        echo "Reading cargo metadata or a member Cargo.toml failed. Check the output above."
        return 2
    fi
    while IFS=$'\t' read -r kind field path; do
        case $kind in
            member)
                if [[ $field != yes ]]; then
                    echo "$path does not set [lints] workspace = true"
                    found=1
                fi
                ;;
            target)
                targets=$((targets + 1))
                if ! src=$(<"$path"); then
                    echo "$path is a target of $field but cannot be read."
                    return 2
                fi
                src=$(awk '{ sub(/\/\/.*/, ""); print }' <<<"$src")
                src=${src//[[:space:]]/}
                if [[ $src != *'#![deny(unsafe_code)]'* ]]; then
                    echo "$path (target of $field) lacks #![deny(unsafe_code)]"
                    found=1
                fi
                ;;
        esac
    done <<<"$records"
    if ((targets == 0)); then
        echo "cargo metadata listed no targets. Check the workspace members in Cargo.toml."
        return 2
    fi
    if ((found)); then
        return 1
    fi
    echo "note: $targets targets"
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
