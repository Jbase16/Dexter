#!/usr/bin/env bash
# Shared helpers for stopping subprocess trees started by live smoke scripts.

tree_descendants() {
    local parent="$1"
    local child
    while IFS= read -r child; do
        [[ -n "$child" ]] || continue
        tree_descendants "$child"
        printf '%s\n' "$child"
    done < <(pgrep -P "$parent" 2>/dev/null || true)
}

stop_process_tree() {
    local root="$1"
    local pids alive pid

    [[ -n "$root" ]] || return 0
    kill -0 "$root" >/dev/null 2>&1 || return 0

    pids="$(
        {
            tree_descendants "$root"
            printf '%s\n' "$root"
        } | awk '!seen[$0]++'
    )"
    [[ -n "$pids" ]] || return 0

    while IFS= read -r pid; do
        kill -TERM "$pid" >/dev/null 2>&1 || true
    done <<< "$pids"

    sleep 1

    alive=""
    while IFS= read -r pid; do
        if kill -0 "$pid" >/dev/null 2>&1; then
            alive="${alive}${pid}"$'\n'
        fi
    done <<< "$pids"

    [[ -n "$alive" ]] || return 0
    while IFS= read -r pid; do
        [[ -n "$pid" ]] || continue
        kill -KILL "$pid" >/dev/null 2>&1 || true
    done <<< "$alive"
}
