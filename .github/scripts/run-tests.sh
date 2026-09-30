#!/usr/bin/env bash
set -euo pipefail

target="${1:-}"
features="${2-stats}"

sudo -E \
    PATH="${PATH}:/usr/share/rust/.cargo/bin" \
    TEST_TARGET="${target}" \
    TEST_FEATURES="${features}" \
    bash -c '
        set -euo pipefail

        ulimit -Sl 512
        ulimit -Hl 512

        echo "PATH=${PATH}"
        rustup show

        args=(
            nextest
            run
            --no-default-features
            --features "${TEST_FEATURES}"
            --locked
            --profile
            ci
        )

        if [[ -n "${TEST_TARGET}" ]]; then
            args+=(--target "${TEST_TARGET}")
        fi

        echo cargo "${args[@]}"
        exec cargo "${args[@]}"
    '
