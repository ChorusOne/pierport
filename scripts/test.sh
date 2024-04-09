#!/bin/sh

col_reset='\033[0m'
col_green='\033[0;32m'
col_red='\033[0;31m'

dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

tmp_file="test-tmp-out"

# Run a container named as `pierport` to prevent stopping of the container inbetween the test runs.
#cont=pierport

# Mount the files so that we do not unnecessarily spam the github API.
mkdir -p $dir/../pierport_vere_cache
cont=$(docker run --rm -p 4242 -v "$dir/../pierport_vere_db.json:/pierport_vere_db.json:Z" -v "$dir/../pierport_vere_cache:/pierport_vere_cache:Z" -e RUST_LOG=debug --name pierport -d pierport)

port=$(docker port "$cont" | grep 4242 | sed 's/.*://' | head -n 1)

trap "rm -f $tmp_file; if [ $cont != pierport ]; then docker kill $cont > /dev/null; fi; trap - EXIT" EXIT INT HUP TERM
test_num=0

echo_col() {
    col=$1
    shift 1
    printf "${col}$@${col_reset}\n"
}

echo_err() {
    echo_col $col_red "$@" 1>&2
}

assert_import() {
    expected="$1"
    patp="$2"
    content="$3"
    shift 3
    ret=$(curl -s -o "$tmp_file" -w "%{http_code}" -X POST -H "Content-Type: $content" "http://localhost:$port/import/~$patp" "$@")

    if [ "$ret" -ge 400 ]; then
        if [ "$expected" = "fail" ]; then
            return 0
        else
            echo "$(cat $tmp_file)"
            exit 1
        fi
    fi

    if [ "$ret" -eq 202 ]; then
        id=$(cat "$tmp_file")
        echo "Waiting on $id"
        while true; do
            sleep 1
            ret=$(exec curl -s -o "$tmp_file" -w "%{http_code}" "http://localhost:$port/import/~$patp/$id")

            if [ "$ret" -ne 200 ]; then
                if [ "$expected" = "fail" ]; then
                    return 0
                else
                    echo "Unable to query import status ($patp/$id): $(cat $tmp_file)"
                    exit 1
                fi
            fi

            val=$(cat "$tmp_file" | jq -r 'keys | .[]')

            if [ "$val" = "importing" ]; then
                continue
            elif [ "$val" = "done" ] && [ "$expected" = "success" ]; then
                return 0
            elif [ "$val" = "failed" ] && [ "$expected" = "fail" ]; then
                return 0
            else
                echo "Reached unexpected state ($val): $(cat $tmp_file)"
                exit 1
            fi
        done
    elif [ "$expected" = "fail" ]; then
        echo "Expected failure, got status 200 ($(cat $tmp_file))"
        exit 1
    fi
}

run_test() {
    test_num=$((test_num+1))

    ret=$("$@")
    r2="$?"

    if [ "$r2" -eq 0 ]; then
        echo_col $col_green "Test $test_num: pass"
        return 0
    else
        echo_err "Test $test_num: failed ($ret)"
        exit 1
    fi
}

# Send malformed archives
run_test assert_import fail zod "application/zstd"
run_test assert_import fail zod "application/zstd" -d 'sdfsdfsdfsdfsdfsdfsfddfs'

# Send an empty zst archive
run_test assert_import fail zod "application/zstd" --data-binary "@${dir}/empty_zod.tar.zst"

# Send zstd with malformed .urb directory
run_test assert_import fail zod "application/zstd" --data-binary "@${dir}/malformed_zod.tar.zst"

# Send a zip bomb
run_test assert_import fail zod "application/json" -d '{ "url": "https://www.bamsoftware.com/hacks/zipbomb/zbsm.zip", "format": "zip" }'

echo_col ${col_green} "All tests pass"
