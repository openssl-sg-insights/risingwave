#!/usr/bin/env bash

# Exits as soon as any line fails.
set -euo pipefail

while getopts 'p:' opt; do
    case ${opt} in
        p )
            profile=$OPTARG
            ;;
        \? )
            echo "Invalid Option: -$OPTARG" 1>&2
            exit 1
            ;;
        : )
            echo "Invalid option: $OPTARG requires an argument" 1>&2
            ;;
    esac
done
shift $((OPTIND -1))

echo "--- Download artifacts"
mkdir -p target/debug
buildkite-agent artifact download risingwave-"$profile" target/debug/
buildkite-agent artifact download risedev-dev-"$profile" target/debug/
buildkite-agent artifact download "e2e_test/generated/*" ./
mv target/debug/risingwave-"$profile" target/debug/risingwave
mv target/debug/risedev-dev-"$profile" target/debug/risedev-dev

echo "--- Adjust permission"
chmod +x ./target/debug/risingwave
chmod +x ./target/debug/risedev-dev

echo "--- Generate RiseDev CI config"
cp ci/risedev-components.ci.env risedev-components.user.env

echo "--- Prepare RiseDev dev cluster"
cargo make pre-start-dev
cargo make link-all-in-one-binaries

echo "--- e2e, ci-3cn-1fe, streaming"
cargo make ci-start ci-3cn-1fe
# Please make sure the regression is expected before increasing the timeout.
sqllogictest -p 4566 -d dev './e2e_test/streaming/**/*.slt' --junit "streaming-${profile}"

echo "--- Kill cluster"
cargo make ci-kill

echo "--- e2e, ci-3cn-1fe, batch"
cargo make ci-start ci-3cn-1fe
sqllogictest -p 4566 -d dev './e2e_test/ddl/**/*.slt' --junit "batch-ddl-${profile}"
sqllogictest -p 4566 -d dev './e2e_test/batch/**/*.slt' --junit "batch-${profile}"
sqllogictest -p 4566 -d dev './e2e_test/database/prepare.slt'
sqllogictest -p 4566 -d test './e2e_test/database/test.slt'

echo "--- Kill cluster"
cargo make ci-kill

echo "--- e2e, ci-3cn-1fe, generated"
cargo make ci-start ci-3cn-1fe
sqllogictest -p 4566 -d dev './e2e_test/generated/**/*.slt' --junit "generated-${profile}"

echo "--- Kill cluster"
cargo make ci-kill

echo "--- e2e, ci-3cn-1fe, extended query"
cargo make ci-start ci-3cn-1fe
sqllogictest -p 4566 -d dev -e postgres-extended './e2e_test/extended_query/**/*.slt'

echo "--- Kill cluster"
cargo make ci-kill

if [[ "$RUN_COMPACTION" -eq "1" ]]; then
    echo "--- e2e, ci-compaction-test, nexmark_q7"
    cargo make clean-data
    cargo make ci-start ci-compaction-test
    # Please make sure the regression is expected before increasing the timeout.
    sqllogictest -p 4566 -d dev './e2e_test/compaction/ingest_rows.slt'

    # We should ingest about 100 version deltas before the test
    echo "--- Wait for data ingestion"

    RC_ENV_FILE="${PREFIX_CONFIG}/risectl-env"
    if [ ! -f "${RC_ENV_FILE}" ]; then
      echo "risectl-env file not found. Did you start cluster using $(tput setaf 4)\`./risedev d\`$(tput sgr0)?"
      exit 1
    fi
    source "${RC_ENV_FILE}"

    # Poll the current version id until we have around 100 version deltas
    delta_log_cnt=0
    while [ $delta_log_cnt -le 95 ]
    do
        delta_log_cnt="$(./target/debug/risingwave risectl hummock list-version | grep -w '^ *id:' | grep -o '[0-9]\+' | head -n 1)"
        echo "Current version $delta_log_cnt"
        sleep 5
    done

    echo "--- Pause source and disable commit new epochs"
    ./target/debug/risingwave risectl meta pause
    ./target/debug/risingwave risectl hummock disable-commit-epoch

    echo "--- Start to run compaction test"
    buildkite-agent artifact download compaction-test-"$profile" target/debug/
    mv target/debug/compaction-test-"$profile" target/debug/compaction-test
    chmod +x ./target/debug/compaction-test
    ./target/debug/compaction-test --ci-mode true --state-store hummock+minio://hummockadmin:hummockadmin@127.0.0.1:9301/hummock001

    echo "--- Kill cluster"
    cargo make ci-kill
fi

if [[ "$RUN_SQLSMITH" -eq "1" ]]; then
    echo "--- e2e, ci-3cn-1fe, fuzzing"
    buildkite-agent artifact download sqlsmith-"$profile" target/debug/
    mv target/debug/sqlsmith-"$profile" target/debug/sqlsmith
    chmod +x ./target/debug/sqlsmith

    cargo make ci-start ci-3cn-1fe
    timeout 20m ./target/debug/sqlsmith test --count "$SQLSMITH_COUNT" --testdata ./src/tests/sqlsmith/tests/testdata

    # Using `kill` instead of `ci-kill` avoids storing excess logs.
    # If there's errors, the failing query will be printed to stderr.
    # Use that to reproduce logs on local machine.
    echo "--- Kill cluster"
    cargo make kill
fi
