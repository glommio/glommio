#!/usr/bin/env bash
set -euo pipefail

target="${1:-}"
test_threads="$(nproc)"

sudo -E \
    PATH="${PATH}:/usr/share/rust/.cargo/bin" \
    TEST_TARGET="${target}" \
    TEST_THREADS="${test_threads}" \
    bash -c '
        set -euo pipefail

        ulimit -Sl 512
        ulimit -Hl 512

        echo "${TEST_THREADS} CPU(s) available"
        echo "PATH=${PATH}"
        rustup show

        args=(
            test
            --locked
        )

        if [[ -n "${TEST_TARGET}" ]]; then
            args+=(--target "${TEST_TARGET}")
        fi

        exec cargo "${args[@]}" -- \
            --test-threads="${TEST_THREADS}"
    '
