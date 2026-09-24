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
        nextest_status=0
        cargo "${args[@]}" || nextest_status=$?

        # nextest cannot run doc tests, so they need a separate cargo test
        # invocation or they do not run at all. Only on the host target: a
        # doc test is documentation, which does not vary by target, and
        # running them cross-compiled needs a runner we do not have.
        doc_status=0
        if [[ -z "${TEST_TARGET}" ]]; then
            echo cargo test --doc --locked
            cargo test --doc --locked || doc_status=$?
        else
            echo "skipping doc tests: they run on the host target only"
        fi

        # Report both verdicts rather than stopping at the first failure, so
        # one flaky unit test cannot hide a doc test regression.
        echo "nextest exited ${nextest_status}, doc tests exited ${doc_status}"
        if [[ "${nextest_status}" -ne 0 || "${doc_status}" -ne 0 ]]; then
            exit 1
        fi
    '
