#!/usr/bin/env bash
set -euo pipefail

target="${1:-}"

sudo -E \
    PATH="${PATH}:/usr/share/rust/.cargo/bin" \
    TEST_TARGET="${target}" \
    bash -c '
        set -euo pipefail

        ulimit -Sl 512
        ulimit -Hl 512

        echo "PATH=${PATH}"
        rustup show

        args=(
            nextest
            run
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
